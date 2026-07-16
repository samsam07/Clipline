//! An in-memory `ClipboardAdapter` with no OS behind it. It **stands in for the
//! not-yet-written Linux adapter**, keeping the trait honest for both OS models
//! (PLAN.md M1), and lets mesh/core logic be tested without a real clipboard
//! (CONVENTIONS.md testing).
//!
//! It also models the **adapter-owned render timeout** (D2): `simulate_render`
//! emits a [`RenderRequest`] to core and awaits the reply against a per-adapter
//! deadline; if the deadline fires first it drops the reply (the graceful paste-fail
//! path that, on Windows, returns from `WM_RENDERFORMAT` without `SetClipboardData`,
//! and on Wayland closes the fd empty). No real OS is blocked, but the seam is
//! exercised exactly as a platform adapter would drive it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};

use crate::adapter::{ClipboardAdapter, FormatReq, LocalRead, RenderRequest};
use crate::error::{AdapterError, LocalReadError, RenderError};
use crate::protocol::{CaptureId, FileEntry, LocalCopy, Mime, Offer, Payload, SensitivityHint};
use crate::wire::{ByteRange, JobId};

/// Default render deadline for the mock. Generous like Windows (Finding A tolerates
/// ~30 s); tests override it to something small under paused time.
const DEFAULT_RENDER_TIMEOUT: Duration = Duration::from_secs(10);

/// What a simulated paste observes — the outcome the OS would see from a render.
#[derive(Debug)]
pub enum RenderOutcome {
    /// Bytes arrived within the deadline (happy path — the OS gets the data).
    Rendered(Payload),
    /// The adapter's deadline elapsed; it released the OS call with no data. Graceful
    /// paste-fail, no hang (the make-or-break M0 behavior).
    TimedOut,
    /// Core reported a source-side failure (origin gone, responder dropped).
    Failed(RenderError),
}

/// A mock capture: what the adapter snapshotted for one local copy (M3.2). Mirrors the
/// real split — non-file formats hold bytes, files hold only what a path would give you.
#[derive(Debug, Clone, Default)]
pub struct MockCapture {
    /// Whole-format bytes, keyed by MIME (text, image, …).
    pub formats: HashMap<Mime, Vec<u8>>,
    /// Per-file bytes, in manifest order. A real adapter would hold *paths* here and read
    /// on demand; the mock holds bytes because it has no filesystem — the laziness that
    /// matters (never read at copy) is the platform adapter's to honour.
    pub files: Vec<Vec<u8>>,
}

/// See module docs.
pub struct MockAdapter {
    render_timeout: Duration,
    render_tx: mpsc::UnboundedSender<RenderRequest>,
    render_rx: Mutex<Option<mpsc::UnboundedReceiver<RenderRequest>>>,
    watch_tx: mpsc::UnboundedSender<LocalCopy>,
    watch_rx: Mutex<Option<mpsc::UnboundedReceiver<LocalCopy>>>,
    job_end_tx: mpsc::UnboundedSender<JobId>,
    job_end_rx: Mutex<Option<mpsc::UnboundedReceiver<JobId>>>,
    reads_tx: mpsc::Sender<LocalRead>,
    // Recorded commands, for test assertions.
    promises: Mutex<Vec<Offer>>,
    eagers: Mutex<Vec<(Offer, Payload)>>,
    captures: Arc<Mutex<HashMap<CaptureId, MockCapture>>>,
    released: Arc<Mutex<Vec<CaptureId>>>,
    next_capture: AtomicU64,
    _serve: tokio::task::JoinHandle<()>,
}

impl MockAdapter {
    pub fn new() -> Self {
        Self::with_render_timeout(DEFAULT_RENDER_TIMEOUT)
    }

    pub fn with_render_timeout(render_timeout: Duration) -> Self {
        let (render_tx, render_rx) = mpsc::unbounded_channel();
        let (watch_tx, watch_rx) = mpsc::unbounded_channel();
        let (job_end_tx, job_end_rx) = mpsc::unbounded_channel();
        let (reads_tx, reads_rx) = mpsc::channel(8);
        let captures: Arc<Mutex<HashMap<CaptureId, MockCapture>>> =
            Arc::new(Mutex::new(HashMap::new()));
        // The adapter side of `local_reads`: a real adapter serves these on its platform
        // thread; the mock just answers from its snapshot map.
        let serve = tokio::spawn(serve_local_reads(reads_rx, captures.clone()));
        MockAdapter {
            render_timeout,
            render_tx,
            render_rx: Mutex::new(Some(render_rx)),
            watch_tx,
            watch_rx: Mutex::new(Some(watch_rx)),
            job_end_tx,
            job_end_rx: Mutex::new(Some(job_end_rx)),
            reads_tx,
            promises: Mutex::new(Vec::new()),
            eagers: Mutex::new(Vec::new()),
            captures,
            released: Arc::new(Mutex::new(Vec::new())),
            next_capture: AtomicU64::new(1),
            _serve: serve,
        }
    }

    /// Simulate a local copy of `capture`'s content, deriving the `LocalCopy` manifest from
    /// it. Returns the allocated [`CaptureId`] so tests can assert its lifecycle.
    pub fn push_capture(&self, capture: MockCapture, files: Vec<FileEntry>) -> CaptureId {
        let id = CaptureId(self.next_capture.fetch_add(1, Ordering::Relaxed));
        let formats: Vec<crate::protocol::FormatDesc> = capture
            .formats
            .iter()
            .map(|(mime, bytes)| crate::protocol::FormatDesc {
                mime: mime.clone(),
                size: bytes.len() as u64,
            })
            .collect();
        // Content fingerprint over the actual bytes (see `LocalCopy::content_hash`): a
        // real adapter hashes what it captured. Sorted so map ordering does not perturb it.
        let mut h = blake3::Hasher::new();
        let mut fmts: Vec<(&Mime, &Vec<u8>)> = capture.formats.iter().collect();
        fmts.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        for (mime, bytes) in fmts {
            h.update(mime.as_str().as_bytes());
            h.update(bytes);
        }
        for f in &capture.files {
            h.update(f);
        }
        let content_hash = *h.finalize().as_bytes();

        self.captures.lock().expect("captures").insert(id, capture);
        let _ = self.watch_tx.send(LocalCopy {
            formats,
            files,
            capture: id,
            content_hash,
            sensitivity_hint: SensitivityHint::None,
        });
        id
    }

    /// Captures still held (not yet released) — the pin lifecycle under test.
    pub fn live_captures(&self) -> Vec<CaptureId> {
        let mut ids: Vec<CaptureId> = self
            .captures
            .lock()
            .expect("captures")
            .keys()
            .copied()
            .collect();
        ids.sort();
        ids
    }

    /// Captures core has released, in order.
    pub fn released(&self) -> Vec<CaptureId> {
        self.released.lock().expect("released").clone()
    }

    /// Simulate the OS forcing a render of one format (a paste). Emits the request to
    /// core and awaits the reply against the adapter-owned deadline (D2).
    ///
    /// Announces the job as ended once the render resolves, modelling the one-read job a
    /// real adapter has for text and images (`WM_RENDERFORMAT` → `SetClipboardData` → done).
    /// For a multi-read job — a file `IStream` — drive the reads yourself and call
    /// [`Self::announce_job_end`] when the stream would be released.
    pub async fn simulate_render(&self, req: FormatReq) -> RenderOutcome {
        let job = req.job;
        let (reply, rx) = oneshot::channel();
        if self.render_tx.send(RenderRequest { req, reply }).is_err() {
            // No render loop is consuming — nothing can answer; a real paste fails.
            return RenderOutcome::Failed(RenderError::ResponderDropped);
        }
        let outcome = match tokio::time::timeout(self.render_timeout, rx).await {
            Ok(Ok(Ok(payload))) => RenderOutcome::Rendered(payload),
            Ok(Ok(Err(err))) => RenderOutcome::Failed(err),
            Ok(Err(_recv)) => RenderOutcome::Failed(RenderError::ResponderDropped),
            // Deadline elapsed: dropping `rx` here releases the OS call empty.
            Err(_elapsed) => RenderOutcome::TimedOut,
        };
        self.announce_job_end(job);
        outcome
    }

    /// Announce that a transfer job will issue no more reads (the `job_ends` stream).
    pub fn announce_job_end(&self, job: JobId) {
        let _ = self.job_end_tx.send(job);
    }

    /// Offers passed to `set_promise`, in order (test assertion helper).
    pub fn promises(&self) -> Vec<Offer> {
        self.promises.lock().expect("mock promises lock").clone()
    }

    /// `(offer, payload)` pairs passed to `set_eager`, in order (test assertion helper).
    pub fn eagers(&self) -> Vec<(Offer, Payload)> {
        self.eagers.lock().expect("mock eagers lock").clone()
    }
}

impl Default for MockAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl ClipboardAdapter for MockAdapter {
    fn watch(&self) -> mpsc::UnboundedReceiver<LocalCopy> {
        self.watch_rx
            .lock()
            .expect("mock watch_rx lock")
            .take()
            .unwrap_or_else(|| mpsc::unbounded_channel().1)
    }

    fn render_requests(&self) -> mpsc::UnboundedReceiver<RenderRequest> {
        self.render_rx
            .lock()
            .expect("mock render_rx lock")
            .take()
            .unwrap_or_else(|| mpsc::unbounded_channel().1)
    }

    fn job_ends(&self) -> mpsc::UnboundedReceiver<JobId> {
        self.job_end_rx
            .lock()
            .expect("mock job_end_rx lock")
            .take()
            .unwrap_or_else(|| mpsc::unbounded_channel().1)
    }

    fn set_promise(&self, offer: &Offer) -> Result<(), AdapterError> {
        self.promises
            .lock()
            .expect("mock promises lock")
            .push(offer.clone());
        Ok(())
    }

    fn set_eager(&self, offer: &Offer, payload: Payload) -> Result<(), AdapterError> {
        self.eagers
            .lock()
            .expect("mock eagers lock")
            .push((offer.clone(), payload));
        Ok(())
    }

    fn local_reads(&self) -> mpsc::Sender<LocalRead> {
        self.reads_tx.clone()
    }

    fn release_capture(&self, capture: CaptureId) {
        // Dropping the snapshot is the whole point: this is where a pinned copy's memory
        // (or its hold on a path) actually goes away.
        self.captures.lock().expect("captures").remove(&capture);
        self.released.lock().expect("released").push(capture);
    }
}

/// The mock's `local_reads` server: resolve `{capture, format, file_idx, range}` out of the
/// snapshot map, applying `range` exactly as a real adapter's ranged file read would.
async fn serve_local_reads(
    mut rx: mpsc::Receiver<LocalRead>,
    captures: Arc<Mutex<HashMap<CaptureId, MockCapture>>>,
) {
    while let Some(read) = rx.recv().await {
        let result = (|| {
            let map = captures.lock().expect("captures");
            let capture = map
                .get(&read.capture)
                .ok_or(LocalReadError::NoSuchCapture)?;
            let whole: &[u8] = match read.file_idx {
                Some(idx) => capture
                    .files
                    .get(idx as usize)
                    .ok_or(LocalReadError::NoSuchFormat)?,
                None => capture
                    .formats
                    .get(&read.format)
                    .ok_or(LocalReadError::NoSuchFormat)?,
            };
            let bytes = match read.range {
                None => whole.to_vec(),
                // A read past EOF is a short read, not an error — the caller stops there.
                Some(ByteRange { offset, len }) => {
                    let start = (offset as usize).min(whole.len());
                    let end = (start + len as usize).min(whole.len());
                    whole[start..end].to_vec()
                }
            };
            Ok(Payload::new(read.format.clone(), bytes))
        })();
        let _ = read.reply.send(result);
    }
}
