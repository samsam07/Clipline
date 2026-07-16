//! The Transfer Engine (M3.3): the **destination** side of a lazy paste, and the thing
//! that finally closes the loop M0 opened.
//!
//! A paste forces a render; [`crate::driver::run_render_loop`] spawns it as a detached job
//! and asks a [`RenderSource`] for the bytes. This *is* that `RenderSource` — no longer a
//! mock: it turns the request into a bulk-plane `FetchReq` to the origin and streams the
//! answer back into the blocked OS call (ARCHITECTURE.md paste flow; SPEC.md §1 "Fetch").
//!
//! # Jobs (locked decision #5; SPEC.md §4)
//!
//! A paste detaches an independent job **bound to the seq that was head at paste time**.
//! That binding needs no work here: the adapter promised a specific `Offer`, so the
//! `FormatReq` it raises already names `{origin_id, seq}`. The head moving on afterwards
//! cannot affect a job in flight — which, with the origin's pin (see [`crate::serve`]), is
//! how "multiple pastes all complete" (§6 row 3) holds even across a new copy (§6 row 2).
//!
//! Jobs are registered here for status and for abort (**M3.4**). `job_id` comes from the
//! adapter, not from us: one `IStream` is one job however many reads it makes (ruling Q12).
//!
//! # Ending a job (M3.4)
//!
//! When a job is over the origin is told, and drops its pin. "Over" means the adapter said
//! so ([`ClipboardAdapter::job_ends`]) or the user aborted ([`TransferEngine::abort`]) —
//! never that a single request finished, since a job may issue several (ruling Q12).
//!
//! **No automatic cancellation** (locked decision #6): nothing here ends a job because the
//! head moved, because a newer copy arrived, or because a transfer is slow. Only the two
//! above do.
//!
//! # What is deliberately not here
//!
//! * **Throttling** — M5. Bulk is the throttleable plane; nothing rate-limits it yet.
//! * **Substituting a stale seq** — never. SPEC.md §5 is explicit that reconciliation is a
//!   proactive re-point of the *head* (M4), never a paste-time swap of the *payload*: the
//!   user must not press Ctrl+V expecting file X and receive item Y. An unfetchable seq
//!   fails gracefully instead.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tokio::sync::mpsc;

use crate::adapter::FormatReq;
use crate::driver::RenderSource;
use crate::error::{FetchError, RenderError};
use crate::mesh::MeshHandle;
use crate::protocol::{Mime, OriginId, Payload, Seq};
use crate::wire::{FetchReq, JobId};

/// A snapshot of one in-flight transfer (ARCHITECTURE.md "State — Active jobs").
#[derive(Debug, Clone)]
pub struct JobInfo {
    pub job: JobId,
    /// Who we are fetching from, and which copy of theirs (the paste-time head).
    pub origin_id: OriginId,
    pub seq: Seq,
    pub format: Mime,
    pub file_idx: Option<u32>,
    /// Bytes received so far — progress.
    pub bytes: u64,
    pub started_at: Instant,
}

/// See module docs.
pub struct TransferEngine {
    mesh: MeshHandle,
    jobs: Mutex<HashMap<JobId, JobInfo>>,
}

impl TransferEngine {
    pub fn new(mesh: MeshHandle) -> TransferEngine {
        TransferEngine {
            mesh,
            jobs: Mutex::new(HashMap::new()),
        }
    }

    /// Open transfers, for status/UI (M6) and tests. A job stays listed until it *ends* —
    /// not merely until a request of it completes.
    pub fn jobs(&self) -> Vec<JobInfo> {
        self.jobs
            .lock()
            .expect("jobs lock")
            .values()
            .cloned()
            .collect()
    }

    /// Register a job, or refresh one already open (a second read of the same `IStream`).
    fn start(&self, req: &FormatReq) {
        self.jobs
            .lock()
            .expect("jobs lock")
            .entry(req.job)
            .or_insert_with(|| JobInfo {
                job: req.job,
                origin_id: req.origin_id,
                seq: req.seq,
                format: req.format.clone(),
                file_idx: req.file_idx,
                bytes: 0,
                started_at: Instant::now(),
            });
    }

    fn advance(&self, job: JobId, n: u64) {
        if let Some(info) = self.jobs.lock().expect("jobs lock").get_mut(&job) {
            info.bytes += n;
        }
    }

    /// End a job: forget it, and tell the origin so it drops the pin.
    ///
    /// Idempotent — an unknown or already-ended job is a no-op, which matters because the
    /// adapter may announce a job whose fetch already failed.
    pub async fn end_job(&self, job: JobId) {
        let origin = self
            .jobs
            .lock()
            .expect("jobs lock")
            .remove(&job)
            .map(|i| i.origin_id);
        if let Some(origin) = origin {
            tracing::debug!(job = job.0, origin = %origin, "job ended; releasing origin pin");
            self.mesh.end_job(origin, job).await;
        }
    }

    /// **Explicit user abort** — the only thing that cancels a transfer (locked decision #6;
    /// SPEC.md §6 "B explicitly aborts a transfer → that job is cancelled; origin releases
    /// its pin").
    ///
    /// Identical to [`Self::end_job`] on the wire: the origin stops serving and unpins
    /// either way. The difference is only here — an in-flight read is answered with
    /// `Aborted`, so the paste fails gracefully instead of receiving bytes.
    ///
    /// The user-facing trigger for this is M6.
    pub async fn abort(&self, job: JobId) {
        self.end_job(job).await;
    }
}

/// Drive an adapter's `job_ends` stream: each announced job is released on its origin.
/// Runs until the adapter closes the stream.
pub async fn run_job_end_loop(
    mut ends: mpsc::UnboundedReceiver<JobId>,
    engine: Arc<TransferEngine>,
) {
    while let Some(job) = ends.recv().await {
        engine.end_job(job).await;
    }
}

impl RenderSource for TransferEngine {
    /// Resolve one forced render by fetching from the origin.
    ///
    /// Every failure becomes [`RenderError::Unavailable`]: the pasting app is blocked on
    /// this call, so whatever went wrong, the only acceptable outcome is to release it
    /// cleanly (CONVENTIONS.md; SPEC.md §5) — never to hang, and never to hand back a
    /// truncated payload as if it were whole.
    async fn render(&self, req: FormatReq) -> Result<Payload, RenderError> {
        self.start(&req);
        // The job stays open past this request: only the adapter or an abort ends it
        // (ruling Q12), and the origin's pin must outlive a seek back into the same file.
        let result = self.fetch_all(&req).await;

        match result {
            Ok(bytes) => Ok(Payload::new(req.format, bytes)),
            Err(e) => {
                // Metadata only — never contents (CONVENTIONS.md logging).
                tracing::debug!(
                    origin_id = %req.origin_id,
                    seq = req.seq.0,
                    job = req.job.0,
                    error = %e,
                    "fetch failed; paste will fail gracefully",
                );
                Err(RenderError::Unavailable)
            }
        }
    }
}

impl TransferEngine {
    /// Drive one fetch to completion, accumulating exactly what was asked for.
    ///
    /// Buffered rather than streamed because [`crate::adapter::RenderResult`] is one
    /// `Payload` — the OS call wants a blob. That is bounded by `req.range`: for text and
    /// images the whole format is the point, and for a file the adapter asks in slices, so
    /// this never holds more than one slice of a large file (locked decision #8).
    async fn fetch_all(&self, req: &FormatReq) -> Result<Vec<u8>, FetchError> {
        let fetch = FetchReq {
            job_id: req.job,
            origin_id: req.origin_id,
            seq: req.seq,
            format: req.format.clone(),
            file_idx: req.file_idx,
            range: req.range,
        };
        let mut chunks = self.mesh.fetch(fetch).await?;

        let mut out = Vec::with_capacity(req.range.map_or(0, |r| r.len as usize));
        while let Some(chunk) = chunks.recv().await {
            let chunk = chunk?;
            self.advance(req.job, chunk.len() as u64);
            out.extend_from_slice(&chunk);
        }
        Ok(out)
    }
}
