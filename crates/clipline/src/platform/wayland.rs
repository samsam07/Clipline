//! Linux **Wayland** `ClipboardAdapter` (M-Linux ML.1) over the data-control protocols:
//! `ext-data-control-v1` (KWin — PLATFORM-NOTES M0b) with a `zwlr-data-control-v1` fallback
//! (wlroots compositors). Raw `wayland-client`, not `wl-clipboard-rs` — that crate needs the
//! bytes up front and blocks in its `send` handler, so it cannot serve lazily (M-Linux finding).
//!
//! # The bridge, Wayland-style (Finding D)
//!
//! The mirror image of the Windows adapter. Windows *must* block its pump thread inside
//! `WM_RENDERFORMAT`; Wayland *must not* block its dispatch thread inside `send(mime, fd)` —
//! a blocking handler broke every transfer with `Broken pipe` in M0b. So a `send` emits a
//! [`RenderRequest`] and hands the fd to a tokio task, which writes it when core's reply
//! arrives, or drops it empty at the adapter deadline (D2: graceful paste-fail, never a hang).
//! The dispatch loop keeps running throughout.
//!
//! # Threading map
//!
//! * **dispatch thread** (`clipline-clipboard`) — the only thread that touches the Wayland
//!   connection. Polls the connection fd plus a wake pipe; executes `set_promise`/`set_eager`
//!   commands marshalled to it (same shape as the Windows pump's command channel).
//! * **capture thread** (`clipline-capture`) — reads a foreign selection's bytes off the pipes
//!   the dispatch thread opened, stats files, and emits the [`LocalCopy`]. Separate so a slow
//!   source app never stalls the dispatch loop; one thread so copies stay in order.
//! * **tokio tasks** — fd writes (render replies) and the `local_reads` server.
//!
//! # Scope (ML.1)
//!
//! Text (UTF-8 on the wire) and images (`image/png`, already the Wayland-native type — no
//! codec) both ways; **outbound** files (a local `text/uri-list` is captured by-reference:
//! paths stat'ed, never read — locked decision #8). **Inbound** files need the FUSE mount
//! (ML.2): until then a files-only remote offer is refused and the head left unchanged.

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc as sync_mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::fs::OFlags;
use rustix::pipe::PipeFlags;
use tokio::sync::{mpsc, oneshot};

use clipline_core::{
    AdapterError, CaptureId, ClipboardAdapter, FileEntry, FormatDesc, FormatReq, JobId, LocalCopy,
    LocalRead, LocalReadError, Mime, Offer, OriginId, Payload, RenderRequest, SensitivityHint, Seq,
};

use wayland_client::backend::ObjectId;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{event_created_child, Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1::{self, ExtDataControlDeviceV1},
    ext_data_control_manager_v1::{self, ExtDataControlManagerV1},
    ext_data_control_offer_v1::{self, ExtDataControlOfferV1},
    ext_data_control_source_v1::{self, ExtDataControlSourceV1},
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::{self, ZwlrDataControlManagerV1},
    zwlr_data_control_offer_v1::{self, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self, ZwlrDataControlSourceV1},
};

use super::files::{collect_path, read_file_range, slice_of};

/// Advertised on every selection **we** own, so our own `selection` event (the compositor
/// echoes every selection change to every data-control device, ours included) is recognized
/// and not captured as a local copy. The Wayland stand-in for the Windows owner-window check.
/// A clipboard manager that re-publishes *all* types also carries it — correctly: that is a
/// re-assertion of our head, not a new copy.
const OWNER_MARKER: &str = "application/x-clipline-owner";

/// Text MIME types, in order of preference for reading a foreign selection. We advertise all
/// of them on a promise and serve UTF-8 (SPEC.md §9) for each — the X11-era atoms included,
/// since XWayland and older toolkits still ask for them.
const TEXT_MIMES: &[&str] = &[
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "STRING",
    "TEXT",
];
const PNG_MIME: &str = "image/png";
const URI_LIST_MIME: &str = "text/uri-list";

/// How long the capture thread waits for a foreign source app to write one local copy's
/// bytes. Bounds a hung or malicious source; a real copy is milliseconds.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(5);

/// A clipboard op marshalled to the dispatch thread, acked back to the caller.
enum Cmd {
    SetPromise {
        offer: Offer,
        ack: sync_mpsc::Sender<Result<(), AdapterError>>,
    },
    SetEager {
        offer: Offer,
        payload: Payload,
        ack: sync_mpsc::Sender<Result<(), AdapterError>>,
    },
    Quit,
}

/// One captured local copy — the Linux twin of `WinCapture` (M3.2; locked decisions #6/#8):
/// bytes for non-file formats (the next copy destroys the selection), **paths** for files.
/// Paths never reach core or the wire.
#[derive(Debug, Default)]
struct Capture {
    formats: HashMap<Mime, Vec<u8>>,
    files: Vec<PathBuf>,
}

/// Bytes already produced for the head we currently promise, by wire MIME.
///
/// Two jobs. **Coalescing** (PLATFORM-NOTES Finding B direction 1): Klipper force-reads a
/// fresh promise within ~5 ms, then the real paste reads it again — Windows caches a
/// successful render in the OS, Wayland does not, so without this every reader would be a
/// fresh network fetch. **Loop guard**: a clipboard manager that re-publishes our promise as
/// its own selection would otherwise look like a new local copy, and two nodes would bounce
/// one copy back and forth forever (echo suppression is by `origin_id`, which a re-publish
/// launders). Only the current head is held — this is not a history (locked decision #4).
///
/// `inflight` closes the gap the cache leaves: Klipper's force-read and the plasma applet
/// read a fresh promise *concurrently* (observed live: two reads ~simultaneously), so both
/// would miss the cache and fetch. A miss for a format already being fetched parks its fd
/// here instead, and the one fetch answers every parked reader.
#[derive(Default)]
struct HeadCache {
    key: Option<(OriginId, Seq)>,
    bytes: HashMap<Mime, Arc<Vec<u8>>>,
    inflight: HashMap<Mime, Vec<OwnedFd>>,
}

/// State shared between the dispatch thread, the capture thread, and the adapter handle.
struct Shared {
    render_tx: mpsc::UnboundedSender<RenderRequest>,
    watch_tx: mpsc::UnboundedSender<LocalCopy>,
    job_end_tx: mpsc::UnboundedSender<JobId>,
    /// Adapter-owned render deadline (D2). 30 s, as on Windows ([CRYSTALLIZE: M-Linux],
    /// pinned): Qt gives up on its own at ~1 s (Finding E), data-control/GTK readers wait.
    render_timeout: Duration,
    captures: Mutex<HashMap<CaptureId, Arc<Capture>>>,
    next_capture: AtomicU64,
    cache: Mutex<HeadCache>,
    rt: tokio::runtime::Handle,
}

/// The injected Wayland clipboard adapter. See module docs.
pub struct WaylandClipboardAdapter {
    cmd_tx: sync_mpsc::Sender<Cmd>,
    wake_tx: OwnedFd,
    render_rx: Mutex<Option<mpsc::UnboundedReceiver<RenderRequest>>>,
    watch_rx: Mutex<Option<mpsc::UnboundedReceiver<LocalCopy>>>,
    job_end_rx: Mutex<Option<mpsc::UnboundedReceiver<JobId>>>,
    reads_tx: mpsc::Sender<LocalRead>,
    shared: Arc<Shared>,
    dispatch: Option<JoinHandle<()>>,
    reads_task: Option<tokio::task::JoinHandle<()>>,
}

impl WaylandClipboardAdapter {
    /// Connect to the compositor (`$WAYLAND_DISPLAY`), bind a data-control manager, and start
    /// the dispatch thread. Fails — so the caller can fall back (X11, ML.3) — when there is no
    /// Wayland session or the compositor offers neither data-control protocol (e.g. Mutter).
    ///
    /// Must be called inside a tokio runtime: fd writes and `local_reads` run as tasks on it.
    pub fn new(render_timeout: Duration) -> Result<Self, AdapterError> {
        let rt = tokio::runtime::Handle::try_current()
            .map_err(|_| AdapterError::Os("wayland adapter needs a tokio runtime".into()))?;

        let (render_tx, render_rx) = mpsc::unbounded_channel();
        let (watch_tx, watch_rx) = mpsc::unbounded_channel();
        let (job_end_tx, job_end_rx) = mpsc::unbounded_channel();
        let (reads_tx, reads_rx) = mpsc::channel(8);
        let (cmd_tx, cmd_rx) = sync_mpsc::channel();
        let (wake_rx, wake_tx) = rustix::pipe::pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK)
            .map_err(|e| AdapterError::Os(format!("wake pipe: {e}")))?;

        let shared = Arc::new(Shared {
            render_tx,
            watch_tx,
            job_end_tx,
            render_timeout,
            captures: Mutex::new(HashMap::new()),
            next_capture: AtomicU64::new(1),
            cache: Mutex::new(HeadCache::default()),
            rt: rt.clone(),
        });

        let (ready_tx, ready_rx) = sync_mpsc::channel::<Result<(), AdapterError>>();
        let shared_for_thread = Arc::clone(&shared);
        let dispatch = std::thread::Builder::new()
            .name("clipline-clipboard".into())
            .spawn(move || dispatch_main(shared_for_thread, cmd_rx, wake_rx, ready_tx))
            .map_err(|e| AdapterError::Os(format!("spawn dispatch thread: {e}")))?;
        ready_rx
            .recv()
            .map_err(|_| AdapterError::Os("dispatch thread exited before ready".into()))??;

        // Serving reads is not on the dispatch thread: everything it needs was captured at
        // copy time, and a large file read must never stall clipboard serving.
        let reads_task = Some(rt.spawn(serve_local_reads(reads_rx, Arc::clone(&shared))));

        Ok(WaylandClipboardAdapter {
            cmd_tx,
            wake_tx,
            render_rx: Mutex::new(Some(render_rx)),
            watch_rx: Mutex::new(Some(watch_rx)),
            job_end_rx: Mutex::new(Some(job_end_rx)),
            reads_tx,
            shared,
            dispatch: Some(dispatch),
            reads_task,
        })
    }

    fn wake(&self) {
        // EAGAIN = the pipe is already full of wake-ups; the thread will drain it anyway.
        let _ = rustix::io::write(&self.wake_tx, &[1]);
    }

    /// Marshal a command to the dispatch thread and wait for its ack.
    fn run_on_dispatch(
        &self,
        make: impl FnOnce(sync_mpsc::Sender<Result<(), AdapterError>>) -> Cmd,
    ) -> Result<(), AdapterError> {
        let (ack_tx, ack_rx) = sync_mpsc::channel();
        self.cmd_tx
            .send(make(ack_tx))
            .map_err(|_| AdapterError::Os("dispatch thread gone".into()))?;
        self.wake();
        ack_rx
            .recv()
            .map_err(|_| AdapterError::Os("dispatch dropped command ack".into()))?
    }
}

impl Drop for WaylandClipboardAdapter {
    fn drop(&mut self) {
        if let Some(t) = self.reads_task.take() {
            t.abort();
        }
        let _ = self.cmd_tx.send(Cmd::Quit);
        self.wake();
        if let Some(h) = self.dispatch.take() {
            let _ = h.join();
        }
    }
}

impl ClipboardAdapter for WaylandClipboardAdapter {
    fn watch(&self) -> mpsc::UnboundedReceiver<LocalCopy> {
        take_or_empty(&self.watch_rx)
    }

    fn render_requests(&self) -> mpsc::UnboundedReceiver<RenderRequest> {
        take_or_empty(&self.render_rx)
    }

    fn job_ends(&self) -> mpsc::UnboundedReceiver<JobId> {
        take_or_empty(&self.job_end_rx)
    }

    fn set_promise(&self, offer: &Offer) -> Result<(), AdapterError> {
        let offer = offer.clone();
        self.run_on_dispatch(move |ack| Cmd::SetPromise { offer, ack })
    }

    fn set_eager(&self, offer: &Offer, payload: Payload) -> Result<(), AdapterError> {
        let offer = offer.clone();
        self.run_on_dispatch(move |ack| Cmd::SetEager {
            offer,
            payload,
            ack,
        })
    }

    fn local_reads(&self) -> mpsc::Sender<LocalRead> {
        self.reads_tx.clone()
    }

    fn release_capture(&self, capture: CaptureId) {
        if self
            .shared
            .captures
            .lock()
            .expect("captures lock")
            .remove(&capture)
            .is_some()
        {
            tracing::debug!(%capture, "released capture");
        }
    }
}

fn take_or_empty<T>(
    slot: &Mutex<Option<mpsc::UnboundedReceiver<T>>>,
) -> mpsc::UnboundedReceiver<T> {
    slot.lock()
        .expect("receiver slot lock")
        .take()
        .unwrap_or_else(|| mpsc::unbounded_channel().1)
}

// ---------------------------------------------------------------------------
// ext / wlr abstraction — the two protocols are message-for-message identical.
// ---------------------------------------------------------------------------

enum DcManager {
    Ext(ExtDataControlManagerV1),
    Wlr(ZwlrDataControlManagerV1),
}

#[derive(Clone)]
enum DcSource {
    Ext(ExtDataControlSourceV1),
    Wlr(ZwlrDataControlSourceV1),
}

enum DcDevice {
    Ext(ExtDataControlDeviceV1),
    Wlr(ZwlrDataControlDeviceV1),
}

enum DcOffer {
    Ext(ExtDataControlOfferV1),
    Wlr(ZwlrDataControlOfferV1),
}

impl DcManager {
    fn create_source(&self, qh: &QueueHandle<State>) -> DcSource {
        match self {
            DcManager::Ext(m) => DcSource::Ext(m.create_data_source(qh, ())),
            DcManager::Wlr(m) => DcSource::Wlr(m.create_data_source(qh, ())),
        }
    }
    fn get_device(&self, seat: &wl_seat::WlSeat, qh: &QueueHandle<State>) -> DcDevice {
        match self {
            DcManager::Ext(m) => DcDevice::Ext(m.get_data_device(seat, qh, ())),
            DcManager::Wlr(m) => DcDevice::Wlr(m.get_data_device(seat, qh, ())),
        }
    }
    fn protocol(&self) -> &'static str {
        match self {
            DcManager::Ext(_) => "ext-data-control-v1",
            DcManager::Wlr(_) => "zwlr-data-control-v1",
        }
    }
}

impl DcSource {
    fn offer(&self, mime: &str) {
        match self {
            DcSource::Ext(s) => s.offer(mime.to_owned()),
            DcSource::Wlr(s) => s.offer(mime.to_owned()),
        }
    }
    fn destroy(&self) {
        match self {
            DcSource::Ext(s) => s.destroy(),
            DcSource::Wlr(s) => s.destroy(),
        }
    }
    fn id(&self) -> ObjectId {
        match self {
            DcSource::Ext(s) => s.id(),
            DcSource::Wlr(s) => s.id(),
        }
    }
}

impl DcDevice {
    fn set_selection(&self, source: &DcSource) {
        match (self, source) {
            (DcDevice::Ext(d), DcSource::Ext(s)) => d.set_selection(Some(s)),
            (DcDevice::Wlr(d), DcSource::Wlr(s)) => d.set_selection(Some(s)),
            // Both are created from the one bound manager, so this cannot happen.
            _ => tracing::error!("data-control source/device protocol mismatch"),
        }
    }
}

impl DcOffer {
    fn receive(&self, mime: &str, fd: std::os::fd::BorrowedFd<'_>) {
        match self {
            DcOffer::Ext(o) => o.receive(mime.to_owned(), fd),
            DcOffer::Wlr(o) => o.receive(mime.to_owned(), fd),
        }
    }
    fn destroy(&self) {
        match self {
            DcOffer::Ext(o) => o.destroy(),
            DcOffer::Wlr(o) => o.destroy(),
        }
    }
    fn id(&self) -> ObjectId {
        match self {
            DcOffer::Ext(o) => o.id(),
            DcOffer::Wlr(o) => o.id(),
        }
    }
}

// ---------------------------------------------------------------------------
// Dispatch-thread state
// ---------------------------------------------------------------------------

/// What our currently-owned source serves.
enum Owned {
    /// A lazy promise for a remote offer (locked decision #2).
    Promise(Offer),
    /// Real bytes, set now (Continuous mode, SPEC.md §3 — M5 drives it). The bytes live in
    /// the [`HeadCache`], which `set_eager` fills, so every `send` is a cache hit.
    Eager(Offer),
}

/// A foreign selection's pipes, handed to the capture thread to read.
struct CaptureJob {
    reads: Vec<(&'static str, OwnedFd)>,
}

struct State {
    shared: Arc<Shared>,
    manager: DcManager,
    device: Option<DcDevice>,
    /// Offers announced by `data_offer`, accumulating their MIME types until the
    /// `selection`/`primary_selection` event that consumes them.
    pending_offers: HashMap<ObjectId, (DcOffer, Vec<String>)>,
    /// The live foreign selection offer, kept until replaced (its source may still be
    /// writing our capture pipes).
    selection_offer: Option<DcOffer>,
    /// The source we own, and what it serves.
    owned: Option<(DcSource, Owned)>,
    /// The compositor sends the *current* selection as soon as the device exists. That is
    /// not a copy the user just made — Windows' listener does not report the pre-existing
    /// clipboard either — so the first `selection` is skipped.
    primed: bool,
    capture_tx: sync_mpsc::Sender<CaptureJob>,
    quit: bool,
}

fn dispatch_main(
    shared: Arc<Shared>,
    cmd_rx: sync_mpsc::Receiver<Cmd>,
    wake_rx: OwnedFd,
    ready: sync_mpsc::Sender<Result<(), AdapterError>>,
) {
    let (mut queue, mut state) = match setup(&shared) {
        Ok(v) => v,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    tracing::info!(
        protocol = state.manager.protocol(),
        "wayland clipboard adapter up"
    );
    let _ = ready.send(Ok(()));

    if let Err(e) = run_loop(&mut queue, &mut state, &cmd_rx, &wake_rx) {
        tracing::error!(error = %e, "wayland dispatch loop ended");
    }
    // Dropping the connection releases our source: the compositor clears a selection whose
    // owner is gone, so no dead promise outlives the process (as on Windows, P1-A).
}

fn setup(shared: &Arc<Shared>) -> Result<(EventQueue<State>, State), AdapterError> {
    let conn = Connection::connect_to_env()
        .map_err(|e| AdapterError::Os(format!("connect to Wayland ($WAYLAND_DISPLAY): {e}")))?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)
        .map_err(|e| AdapterError::Os(format!("wayland registry: {e}")))?;
    let qh = queue.handle();

    // Prefer ext (the standardized one KWin ships); fall back to wlr (wlroots).
    let manager = match globals.bind::<ExtDataControlManagerV1, _, _>(&qh, 1..=1, ()) {
        Ok(m) => DcManager::Ext(m),
        Err(_) => match globals.bind::<ZwlrDataControlManagerV1, _, _>(&qh, 1..=1, ()) {
            Ok(m) => DcManager::Wlr(m),
            Err(_) => {
                return Err(AdapterError::Os(
                    "compositor offers neither ext-data-control nor wlr-data-control".into(),
                ))
            }
        },
    };
    let seat = globals
        .bind::<wl_seat::WlSeat, _, _>(&qh, 1..=1, ())
        .map_err(|e| AdapterError::Os(format!("no wl_seat: {e}")))?;
    let device = manager.get_device(&seat, &qh);

    let (capture_tx, capture_rx) = sync_mpsc::channel();
    let shared_for_capture = Arc::clone(shared);
    std::thread::Builder::new()
        .name("clipline-capture".into())
        .spawn(move || capture_main(shared_for_capture, capture_rx))
        .map_err(|e| AdapterError::Os(format!("spawn capture thread: {e}")))?;

    let mut state = State {
        shared: Arc::clone(shared),
        manager,
        device: Some(device),
        pending_offers: HashMap::new(),
        selection_offer: None,
        owned: None,
        primed: false,
        capture_tx,
        quit: false,
    };
    // Drain the initial selection (skipped via `primed`) before reporting ready.
    queue
        .roundtrip(&mut state)
        .map_err(|e| AdapterError::Os(format!("wayland roundtrip: {e}")))?;
    state.primed = true;
    Ok((queue, state))
}

/// The dispatch loop: poll the Wayland fd and the wake pipe; never block on anything else.
fn run_loop(
    queue: &mut EventQueue<State>,
    state: &mut State,
    cmd_rx: &sync_mpsc::Receiver<Cmd>,
    wake_rx: &OwnedFd,
) -> Result<(), String> {
    let qh = queue.handle();
    loop {
        queue
            .dispatch_pending(state)
            .map_err(|e| format!("dispatch: {e}"))?;
        while let Ok(cmd) = cmd_rx.try_recv() {
            state.handle_cmd(cmd, &qh);
        }
        if state.quit {
            let _ = queue.flush();
            return Ok(());
        }
        queue.flush().map_err(|e| format!("flush: {e}"))?;

        // `None` = events are already queued; go dispatch them.
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let (conn_ready, wake_ready) = {
            let mut fds = [
                PollFd::from_borrowed_fd(guard.connection_fd(), PollFlags::IN),
                PollFd::new(wake_rx, PollFlags::IN),
            ];
            match rustix::event::poll(&mut fds, None) {
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => {}
                Err(e) => return Err(format!("poll: {e}")),
            }
            (!fds[0].revents().is_empty(), !fds[1].revents().is_empty())
        };
        if conn_ready {
            match guard.read() {
                Ok(_) => {}
                Err(wayland_client::backend::WaylandError::Io(e))
                    if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(format!("read: {e}")),
            }
        } else {
            drop(guard);
        }
        if wake_ready {
            let mut buf = [0u8; 64];
            while matches!(rustix::io::read(wake_rx, &mut buf), Ok(n) if n > 0) {}
        }
    }
}

impl State {
    fn handle_cmd(&mut self, cmd: Cmd, qh: &QueueHandle<State>) {
        match cmd {
            Cmd::SetPromise { offer, ack } => {
                let _ = ack.send(self.set_promise(offer, qh));
            }
            Cmd::SetEager {
                offer,
                payload,
                ack,
            } => {
                let _ = ack.send(self.set_eager(offer, payload, qh));
            }
            Cmd::Quit => self.quit = true,
        }
    }

    /// Own the selection with a source advertising `offer`'s formats, holding no bytes
    /// (SPEC.md §1; locked decision #2). Bytes are produced on `send`.
    fn set_promise(&mut self, offer: Offer, qh: &QueueHandle<State>) -> Result<(), AdapterError> {
        let mimes: Vec<&str> = offer
            .formats
            .iter()
            .flat_map(|f| os_mimes_for(&f.mime))
            .copied()
            .collect();
        if mimes.is_empty() {
            // Files-only (or nothing servable): advertising a uri-list we cannot back would
            // promise a paste that cannot succeed. Inbound files arrive with ML.2 (FUSE).
            return Err(AdapterError::Os(if offer.files.is_empty() {
                "remote offer has no format this adapter serves".into()
            } else {
                "inbound files need the FUSE mount (M-Linux ML.2)".into()
            }));
        }
        // A new head: dropping the old cache also drops any readers parked on the old
        // head's fetch — they get EOF (an empty paste of a head that is no longer current).
        *self.shared.cache.lock().expect("cache lock") = HeadCache {
            key: Some((offer.origin_id, offer.seq)),
            ..HeadCache::default()
        };
        self.own(&mimes, Owned::Promise(offer), qh)
    }

    fn set_eager(
        &mut self,
        offer: Offer,
        payload: Payload,
        qh: &QueueHandle<State>,
    ) -> Result<(), AdapterError> {
        let mimes: Vec<&str> = os_mimes_for(&payload.format).to_vec();
        if mimes.is_empty() {
            return Err(AdapterError::Os("eager payload format not servable".into()));
        }
        {
            let mut cache = self.shared.cache.lock().expect("cache lock");
            *cache = HeadCache {
                key: Some((offer.origin_id, offer.seq)),
                ..HeadCache::default()
            };
            cache.bytes.insert(payload.format, Arc::new(payload.bytes));
        }
        self.own(&mimes, Owned::Eager(offer), qh)
    }

    fn own(
        &mut self,
        mimes: &[&str],
        what: Owned,
        qh: &QueueHandle<State>,
    ) -> Result<(), AdapterError> {
        let device = self
            .device
            .as_ref()
            .ok_or_else(|| AdapterError::Os("data-control device finished".into()))?;
        let source = self.manager.create_source(qh);
        for m in mimes {
            source.offer(m);
        }
        source.offer(OWNER_MARKER);
        device.set_selection(&source);
        if let Some((old, _)) = self.owned.replace((source, what)) {
            old.destroy();
        }
        Ok(())
    }

    fn on_data_offer(&mut self, offer: DcOffer) {
        self.pending_offers.insert(offer.id(), (offer, Vec::new()));
    }

    fn on_offer_mime(&mut self, id: ObjectId, mime: String) {
        if let Some((_, mimes)) = self.pending_offers.get_mut(&id) {
            mimes.push(mime);
        }
    }

    fn on_primary_selection(&mut self, offer: Option<DcOffer>) {
        // Only the regular clipboard is shared; the middle-click selection is not.
        if let Some(o) = offer {
            self.pending_offers.remove(&o.id());
            o.destroy();
        }
    }

    /// The selection changed (ARCHITECTURE.md `watch`). Ours → ignore; foreign → open a pipe
    /// per wanted format and hand them to the capture thread.
    fn on_selection(&mut self, offer: Option<DcOffer>, conn: &Connection) {
        if let Some(old) = self.selection_offer.take() {
            old.destroy();
        }
        let Some(offer) = offer else {
            return; // cleared (owner exited); nothing was copied
        };
        let mimes = self
            .pending_offers
            .remove(&offer.id())
            .map(|(_, m)| m)
            .unwrap_or_default();

        if !self.primed || mimes.iter().any(|m| m == OWNER_MARKER) {
            self.selection_offer = Some(offer);
            return; // the pre-existing clipboard, or our own promise echoed back
        }

        let mut wanted: Vec<&'static str> = Vec::new();
        if let Some(t) = TEXT_MIMES.iter().find(|t| mimes.iter().any(|m| m == *t)) {
            wanted.push(t);
        }
        for m in [PNG_MIME, URI_LIST_MIME] {
            if mimes.iter().any(|x| x == m) {
                wanted.push(m);
            }
        }
        if wanted.is_empty() {
            tracing::debug!("local copy has no transferable format; ignoring");
            self.selection_offer = Some(offer);
            return;
        }

        let mut reads = Vec::new();
        for m in wanted {
            match rustix::pipe::pipe_with(PipeFlags::CLOEXEC) {
                Ok((r, w)) => {
                    offer.receive(m, w.as_fd());
                    reads.push((m, r, w));
                }
                Err(e) => tracing::debug!(error = %e, "capture pipe failed"),
            }
        }
        // The write ends travel with the `receive` requests on flush; only then may we close
        // ours, or the reader would never see EOF.
        let _ = conn.flush();
        let reads = reads.into_iter().map(|(m, r, _w)| (m, r)).collect();
        let _ = self.capture_tx.send(CaptureJob { reads });
        self.selection_offer = Some(offer);
    }

    fn on_finished(&mut self) {
        tracing::error!("data-control device finished (seat gone); clipboard sync stopped");
        self.device = None;
    }

    fn on_cancelled(&mut self, source: &DcSource) {
        // Someone else took the selection. Their copy arrives as a `selection` event.
        source.destroy();
        if self
            .owned
            .as_ref()
            .is_some_and(|(s, _)| s.id() == source.id())
        {
            self.owned = None;
        }
    }

    /// THE bridge, Wayland side (Finding D): never block here.
    fn on_send(&mut self, source: &DcSource, mime: String, fd: OwnedFd) {
        let Some((owned_src, what)) = &self.owned else {
            return; // stale source; dropping the fd = empty read
        };
        if owned_src.id() != source.id() {
            return;
        }
        let offer = match what {
            Owned::Promise(o) | Owned::Eager(o) => o,
        };
        let Some(wire) = wire_for_os(&mime, offer) else {
            return; // a type we did not advertise (or the marker)
        };

        // Coalesce (see `HeadCache`): produced already → serve from memory; being fetched
        // already → park this reader on that fetch. Either way no second fetch, no job.
        let key = (offer.origin_id, offer.seq);
        let fd = {
            let mut cache = self.shared.cache.lock().expect("cache lock");
            if cache.key != Some(key) {
                return; // cannot happen: the cache key moves with `owned`
            }
            if let Some(bytes) = cache.bytes.get(&wire).cloned() {
                self.shared.rt.spawn_blocking(move || write_fd(fd, &bytes));
                return;
            }
            if let Some(waiters) = cache.inflight.get_mut(&wire) {
                waiters.push(fd);
                return;
            }
            cache.inflight.insert(wire.clone(), Vec::new());
            fd
        };

        let (reply, reply_rx) = oneshot::channel();
        let job = JobId::next();
        let req = FormatReq {
            origin_id: offer.origin_id,
            seq: offer.seq,
            format: wire.clone(),
            file_idx: None,
            // Text and images are one blob: the reader wants all of it.
            range: None,
            // One `send` is the whole transfer for a non-file format (SPEC.md §4), as one
            // `WM_RENDERFORMAT` is on Windows.
            job,
        };
        if self
            .shared
            .render_tx
            .send(RenderRequest { req, reply })
            .is_err()
        {
            return; // core render loop gone
        }
        let shared = Arc::clone(&self.shared);
        self.shared.rt.spawn(async move {
            let outcome = tokio::time::timeout(shared.render_timeout, reply_rx).await;
            // The job is over either way (as on Windows): bytes or no bytes, this reader
            // will not ask again. Releases the origin's pin promptly (M3.4).
            let _ = shared.job_end_tx.send(job);
            let bytes = match outcome {
                Ok(Ok(Ok(p))) => Some(Arc::new(p.bytes)),
                _ => None,
            };
            // Collect every reader parked on this fetch; cache success for later readers. A
            // failure is not cached — the next paste tries afresh (the origin may be back).
            let mut readers = {
                let mut cache = shared.cache.lock().expect("cache lock");
                if cache.key == Some(key) {
                    if let Some(b) = &bytes {
                        cache.bytes.insert(wire.clone(), Arc::clone(b));
                    }
                    cache.inflight.remove(&wire).unwrap_or_default()
                } else {
                    Vec::new() // head moved on; `set_promise` already dropped them
                }
            };
            readers.push(fd);
            let Some(bytes) = bytes else {
                // Timeout / source failure / responder gone → close every fd empty:
                // graceful paste-fail, readers see EOF and never hang.
                tracing::debug!(
                    readers = readers.len(),
                    "render not satisfied; closing fds empty"
                );
                return;
            };
            let _ = tokio::task::spawn_blocking(move || {
                for fd in readers {
                    write_fd(fd, &bytes);
                }
            })
            .await;
        });
    }
}

/// Write all of `bytes` to a reader's fd. Blocking — run on the blocking pool only.
/// A reader that gave up (Qt's ~1 s, Finding E) shows up as `EPIPE`; Rust masks `SIGPIPE`,
/// so that is a logged non-event, never a crash (QTBUG-57202).
fn write_fd(fd: OwnedFd, bytes: &[u8]) {
    // The fd may arrive non-blocking; a blocking write is what we want on this pool thread.
    let _ = rustix::fs::fcntl_setfl(&fd, OFlags::empty());
    if let Err(e) = std::fs::File::from(fd).write_all(bytes) {
        tracing::debug!(error = %e, "reader closed before the render finished");
    }
}

/// The OS MIME types we advertise for one wire format.
fn os_mimes_for(wire: &Mime) -> &'static [&'static str] {
    let s = wire.as_str();
    if s.starts_with("text/plain") {
        TEXT_MIMES
    } else if s == PNG_MIME {
        &[PNG_MIME]
    } else {
        &[]
    }
}

/// The wire format of `offer` that a `send(os_mime)` is asking for.
fn wire_for_os(os_mime: &str, offer: &Offer) -> Option<Mime> {
    let want_text = TEXT_MIMES.contains(&os_mime);
    let want_png = os_mime == PNG_MIME;
    offer
        .formats
        .iter()
        .find(|f| {
            (want_text && f.mime.as_str().starts_with("text/plain"))
                || (want_png && f.mime.as_str() == PNG_MIME)
        })
        .map(|f| f.mime.clone())
}

// ---------------------------------------------------------------------------
// Protocol dispatch (generated for both protocol families)
// ---------------------------------------------------------------------------

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_seat::WlSeat,
        _: wl_seat::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

macro_rules! impl_data_control {
    ($variant:ident, $mgr:ty, $mgr_mod:ident, $dev:ty, $dev_mod:ident, $src:ty, $src_mod:ident, $off:ty, $off_mod:ident) => {
        impl Dispatch<$mgr, ()> for State {
            fn event(
                _: &mut Self,
                _: &$mgr,
                _: $mgr_mod::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }

        impl Dispatch<$dev, ()> for State {
            fn event(
                state: &mut Self,
                _: &$dev,
                event: $dev_mod::Event,
                _: &(),
                conn: &Connection,
                _: &QueueHandle<Self>,
            ) {
                match event {
                    $dev_mod::Event::DataOffer { id } => state.on_data_offer(DcOffer::$variant(id)),
                    $dev_mod::Event::Selection { id } => {
                        state.on_selection(id.map(DcOffer::$variant), conn)
                    }
                    $dev_mod::Event::PrimarySelection { id } => {
                        state.on_primary_selection(id.map(DcOffer::$variant))
                    }
                    $dev_mod::Event::Finished => state.on_finished(),
                    _ => {}
                }
            }

            event_created_child!(State, $dev, [
                $dev_mod::EVT_DATA_OFFER_OPCODE => ($off, ()),
            ]);
        }

        impl Dispatch<$src, ()> for State {
            fn event(
                state: &mut Self,
                source: &$src,
                event: $src_mod::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                let source = DcSource::$variant(source.clone());
                match event {
                    $src_mod::Event::Send { mime_type, fd } => state.on_send(&source, mime_type, fd),
                    $src_mod::Event::Cancelled => state.on_cancelled(&source),
                    _ => {}
                }
            }
        }

        impl Dispatch<$off, ()> for State {
            fn event(
                state: &mut Self,
                offer: &$off,
                event: $off_mod::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
                if let $off_mod::Event::Offer { mime_type } = event {
                    state.on_offer_mime(offer.id(), mime_type);
                }
            }
        }
    };
}

impl_data_control!(
    Ext,
    ExtDataControlManagerV1,
    ext_data_control_manager_v1,
    ExtDataControlDeviceV1,
    ext_data_control_device_v1,
    ExtDataControlSourceV1,
    ext_data_control_source_v1,
    ExtDataControlOfferV1,
    ext_data_control_offer_v1
);
impl_data_control!(
    Wlr,
    ZwlrDataControlManagerV1,
    zwlr_data_control_manager_v1,
    ZwlrDataControlDeviceV1,
    zwlr_data_control_device_v1,
    ZwlrDataControlSourceV1,
    zwlr_data_control_source_v1,
    ZwlrDataControlOfferV1,
    zwlr_data_control_offer_v1
);

// ---------------------------------------------------------------------------
// Capture thread — the origin side of a local copy
// ---------------------------------------------------------------------------

fn capture_main(shared: Arc<Shared>, rx: sync_mpsc::Receiver<CaptureJob>) {
    while let Ok(job) = rx.recv() {
        capture_one(&shared, job);
    }
}

/// Read a foreign selection's formats, **capture** them, and tell core (M3.2) — the Linux
/// twin of the Windows `handle_clipboard_update`.
fn capture_one(shared: &Shared, job: CaptureJob) {
    let deadline = Instant::now() + CAPTURE_TIMEOUT;
    let mut capture = Capture::default();
    let mut formats = Vec::new();
    let mut files = Vec::new();

    for (mime, fd) in job.reads {
        let Some(bytes) = read_fd_until(fd, deadline) else {
            tracing::debug!(mime, "source did not deliver a format in time; skipping it");
            continue;
        };
        if bytes.is_empty() {
            continue;
        }
        if TEXT_MIMES.contains(&mime) {
            // UTF-8 on the wire (SPEC.md §9). Legacy `STRING` may be Latin-1; lossy is the
            // honest best for a type that carries no charset.
            let utf8 = String::from_utf8_lossy(&bytes).into_owned().into_bytes();
            formats.push(FormatDesc {
                mime: Mime::text_utf8(),
                size: utf8.len() as u64,
            });
            capture.formats.insert(Mime::text_utf8(), utf8);
        } else if mime == PNG_MIME {
            formats.push(FormatDesc {
                mime: Mime::png(),
                size: bytes.len() as u64,
            });
            capture.formats.insert(Mime::png(), bytes);
        } else if mime == URI_LIST_MIME {
            // By-reference (locked decision #8): stat the paths, never read them.
            for path in parse_uri_list(&bytes) {
                let root = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                collect_path(&path, &root, &mut capture.files, &mut files, 0);
            }
            if !files.is_empty() {
                formats.push(FormatDesc {
                    mime: Mime::uri_list(),
                    size: files.iter().map(|e: &FileEntry| e.size).sum(),
                });
            }
        }
    }

    if formats.is_empty() && files.is_empty() {
        tracing::debug!("local copy has no transferable format; ignoring");
        return;
    }
    if is_reassertion(shared, &capture) {
        tracing::debug!(
            "selection re-published by another client (same content as our head); ignoring"
        );
        return;
    }

    let content_hash = fingerprint_capture(&capture, &files);
    let id = CaptureId(shared.next_capture.fetch_add(1, Ordering::Relaxed));
    shared
        .captures
        .lock()
        .expect("captures lock")
        .insert(id, Arc::new(capture));
    tracing::debug!(capture = %id, formats = formats.len(), files = files.len(), "captured local copy");
    let _ = shared.watch_tx.send(LocalCopy {
        formats,
        files,
        capture: id,
        content_hash,
        sensitivity_hint: SensitivityHint::None, // consumed in M5 (SPEC.md §7)
    });
}

/// The loop guard (see [`HeadCache`]): a non-file capture whose every format is byte-equal to
/// what we served for the head we promise is a clipboard manager re-publishing our head,
/// not a user copy. Cost: a user who re-copies exactly the pasted content does not become
/// origin — harmless, the head already holds that content.
fn is_reassertion(shared: &Shared, capture: &Capture) -> bool {
    if !capture.files.is_empty() || capture.formats.is_empty() {
        return false;
    }
    let cache = shared.cache.lock().expect("cache lock");
    cache.key.is_some()
        && capture
            .formats
            .iter()
            .all(|(mime, bytes)| cache.bytes.get(mime).is_some_and(|c| c.as_slice() == bytes))
}

/// Read a pipe to EOF, or give up at `deadline` (a hung source must not wedge capture).
fn read_fd_until(fd: OwnedFd, deadline: Instant) -> Option<Vec<u8>> {
    let flags = rustix::fs::fcntl_getfl(&fd).ok()?;
    rustix::fs::fcntl_setfl(&fd, flags | OFlags::NONBLOCK).ok()?;
    let mut file = std::fs::File::from(fd);
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => return Some(out),
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                let left = deadline.checked_duration_since(Instant::now())?;
                let ts = Timespec::try_from(left).ok()?;
                let mut fds = [PollFd::new(&file, PollFlags::IN)];
                match rustix::event::poll(&mut fds, Some(&ts)) {
                    Ok(0) => return None, // timed out
                    Ok(_) | Err(rustix::io::Errno::INTR) => {}
                    Err(_) => return None,
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
}

/// `text/uri-list` (RFC 2483) → local paths. Comments and non-`file:` URIs are dropped;
/// `file://host/path` keeps only the path (a copy is always of this machine's files).
fn parse_uri_list(bytes: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.strip_prefix("file://"))
        .filter_map(|rest| rest.find('/').map(|i| &rest[i..]))
        .map(|p| PathBuf::from(OsString::from_vec(percent_decode(p))))
        .collect()
}

fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

/// Same formula as the Windows adapter's: non-file formats hash their bytes (sorted by MIME),
/// files hash path + size — never contents (locked decision #8).
fn fingerprint_capture(capture: &Capture, files: &[FileEntry]) -> [u8; 32] {
    let mut h = blake3::Hasher::new();
    let mut fmts: Vec<(&Mime, &Vec<u8>)> = capture.formats.iter().collect();
    fmts.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
    for (mime, bytes) in fmts {
        h.update(mime.as_str().as_bytes());
        h.update(bytes);
    }
    for (path, entry) in capture.files.iter().zip(files) {
        h.update(path.as_os_str().to_string_lossy().as_bytes());
        h.update(&entry.size.to_le_bytes());
    }
    *h.finalize().as_bytes()
}

/// Serve `local_reads` (M3.2) — the origin side of a peer's fetch. Same contract as the
/// Windows server: files are read now, only the requested range; non-file formats are
/// sliced from the copy-time snapshot.
async fn serve_local_reads(mut rx: mpsc::Receiver<LocalRead>, shared: Arc<Shared>) {
    while let Some(read) = rx.recv().await {
        let capture = shared
            .captures
            .lock()
            .expect("captures lock")
            .get(&read.capture)
            .cloned();
        let Some(capture) = capture else {
            let _ = read.reply.send(Err(LocalReadError::NoSuchCapture));
            continue;
        };
        let result = match read.file_idx {
            Some(idx) => match capture.files.get(idx as usize).cloned() {
                Some(path) => {
                    let range = read.range;
                    let format = read.format.clone();
                    tokio::task::spawn_blocking(move || read_file_range(&path, range, format))
                        .await
                        .unwrap_or_else(|e| {
                            Err(LocalReadError::SourceFailed(format!("read task: {e}")))
                        })
                }
                None => Err(LocalReadError::NoSuchFormat),
            },
            None => match capture.formats.get(&read.format) {
                Some(bytes) => Ok(Payload::new(
                    read.format.clone(),
                    slice_of(bytes, read.range),
                )),
                None => Err(LocalReadError::NoSuchFormat),
            },
        };
        let _ = read.reply.send(result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_core::ContentHash;

    fn offer_with(mimes: &[Mime]) -> Offer {
        let formats: Vec<FormatDesc> = mimes
            .iter()
            .map(|m| FormatDesc {
                mime: m.clone(),
                size: 3,
            })
            .collect();
        Offer {
            origin_id: OriginId(7),
            seq: Seq(1),
            hash: ContentHash::of_manifest(OriginId(7), Seq(1), &formats, &[]),
            formats,
            files: Vec::new(),
        }
    }

    #[test]
    fn uri_list_parses_file_uris_only() {
        let list = b"# comment\r\nfile:///home/u/a%20b.txt\r\nhttps://example.com/x\r\nfile://localhost/tmp/d\r\n\r\n";
        assert_eq!(
            parse_uri_list(list),
            vec![PathBuf::from("/home/u/a b.txt"), PathBuf::from("/tmp/d")]
        );
    }

    #[test]
    fn percent_decode_handles_bytes_and_bad_escapes() {
        assert_eq!(percent_decode("/x%C3%A9"), "/xé".as_bytes());
        assert_eq!(percent_decode("/50%"), b"/50%");
        assert_eq!(percent_decode("/%zz"), b"/%zz");
        assert_eq!(percent_decode("/%4"), b"/%4");
    }

    #[test]
    fn send_mime_maps_to_the_offers_wire_format() {
        let offer = offer_with(&[Mime::text_utf8(), Mime::png()]);
        for t in TEXT_MIMES {
            assert_eq!(wire_for_os(t, &offer), Some(Mime::text_utf8()));
        }
        assert_eq!(wire_for_os(PNG_MIME, &offer), Some(Mime::png()));
        assert_eq!(wire_for_os(OWNER_MARKER, &offer), None);
        assert_eq!(
            wire_for_os(PNG_MIME, &offer_with(&[Mime::text_utf8()])),
            None
        );
    }

    #[test]
    fn files_are_not_advertised_until_the_fuse_mount() {
        assert!(os_mimes_for(&Mime::uri_list()).is_empty());
        assert_eq!(os_mimes_for(&Mime::png()), &[PNG_MIME]);
        assert_eq!(os_mimes_for(&Mime::text_utf8()), TEXT_MIMES);
    }

    #[test]
    fn read_fd_until_reads_to_eof_and_times_out_on_a_silent_writer() {
        let (r, w) = rustix::pipe::pipe().expect("pipe");
        std::fs::File::from(w).write_all(b"hello").expect("write");
        assert_eq!(
            read_fd_until(r, Instant::now() + Duration::from_secs(1)).as_deref(),
            Some(&b"hello"[..])
        );

        let (r, _w_kept_open) = rustix::pipe::pipe().expect("pipe");
        let t = Instant::now();
        assert_eq!(
            read_fd_until(r, Instant::now() + Duration::from_millis(100)),
            None
        );
        assert!(t.elapsed() < Duration::from_secs(1));
    }
}

/// Real-compositor tests: drive the adapter against the **live** Wayland session and read
/// it back with `wl-paste`, as the Windows tests drive the real clipboard. `#[ignore]`d
/// because they need a data-control compositor and **overwrite the session clipboard**; run
/// with `cargo test -p clipline -- --ignored` inside a Wayland session (TEST.md).
#[cfg(test)]
mod live_tests {
    use super::*;
    use clipline_core::{ContentHash, RenderError};
    use std::process::Command;

    fn text_offer(seq: u64) -> Offer {
        let formats = vec![FormatDesc {
            mime: Mime::text_utf8(),
            size: 5,
        }];
        Offer {
            origin_id: OriginId(0xC11),
            seq: Seq(seq),
            hash: ContentHash::of_manifest(OriginId(0xC11), Seq(seq), &formats, &[]),
            formats,
            files: Vec::new(),
        }
    }

    /// Answer every render with `answer`, counting them.
    fn respond(
        adapter: &WaylandClipboardAdapter,
        answer: fn() -> Result<Payload, RenderError>,
    ) -> Arc<AtomicU64> {
        let count = Arc::new(AtomicU64::new(0));
        let mut rx = adapter.render_requests();
        let c = Arc::clone(&count);
        tokio::spawn(async move {
            while let Some(r) = rx.recv().await {
                c.fetch_add(1, Ordering::SeqCst);
                // A little latency, so concurrent readers really overlap the fetch.
                tokio::time::sleep(Duration::from_millis(50)).await;
                let _ = r.reply.send(answer());
            }
        });
        count
    }

    async fn wl_paste() -> (Vec<u8>, Duration) {
        tokio::task::spawn_blocking(|| {
            let t = Instant::now();
            let out = Command::new("timeout")
                .args(["20", "wl-paste", "-n", "--type", "text/plain;charset=utf-8"])
                .output()
                .expect("run wl-paste");
            (out.stdout, t.elapsed())
        })
        .await
        .expect("join")
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a live Wayland data-control session; overwrites the clipboard"]
    async fn promise_serves_lazily_and_coalesces_readers() {
        let adapter = WaylandClipboardAdapter::new(Duration::from_secs(30)).expect("adapter");
        let renders = respond(&adapter, || {
            Ok(Payload::new(Mime::text_utf8(), b"lazy!".to_vec()))
        });
        let mut ends = adapter.job_ends();
        tokio::task::block_in_place(|| adapter.set_promise(&text_offer(1))).expect("promise");

        // Two concurrent pastes (plus whatever Klipper does) → still one fetch.
        let (a, b) = tokio::join!(wl_paste(), wl_paste());
        assert_eq!(a.0, b"lazy!");
        assert_eq!(b.0, b"lazy!");
        assert_eq!(
            renders.load(Ordering::SeqCst),
            1,
            "readers coalesced onto one fetch"
        );
        assert!(ends.recv().await.is_some(), "the fetch's job was ended");
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a live Wayland data-control session; overwrites the clipboard"]
    async fn failed_render_is_an_empty_paste_not_a_hang() {
        let adapter = WaylandClipboardAdapter::new(Duration::from_secs(30)).expect("adapter");
        let _renders = respond(&adapter, || Err(RenderError::Unavailable));
        tokio::task::block_in_place(|| adapter.set_promise(&text_offer(2))).expect("promise");

        let (out, took) = wl_paste().await;
        assert!(out.is_empty(), "origin unavailable → fd closed empty");
        assert!(
            took < Duration::from_secs(5),
            "graceful, not a timeout ({took:?})"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a live Wayland data-control session; overwrites the clipboard"]
    async fn adapter_deadline_closes_the_fd() {
        // Responder never answers: the adapter's own deadline (D2) must release the reader.
        let adapter = WaylandClipboardAdapter::new(Duration::from_millis(300)).expect("adapter");
        let mut rx = adapter.render_requests();
        let parked = Arc::new(Mutex::new(Vec::new()));
        let p = Arc::clone(&parked);
        tokio::spawn(async move {
            while let Some(r) = rx.recv().await {
                p.lock().unwrap().push(r.reply); // hold the reply open, never send
            }
        });
        tokio::task::block_in_place(|| adapter.set_promise(&text_offer(3))).expect("promise");

        let (out, took) = wl_paste().await;
        assert!(out.is_empty());
        assert!(
            took < Duration::from_secs(3),
            "deadline released the paste ({took:?})"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs a live Wayland data-control session; overwrites the clipboard"]
    async fn foreign_copy_is_captured_and_served_by_range() {
        let adapter = WaylandClipboardAdapter::new(Duration::from_secs(30)).expect("adapter");
        let mut watch = adapter.watch();
        tokio::task::spawn_blocking(|| {
            let mut c = Command::new("wl-copy")
                .stdin(std::process::Stdio::piped())
                .spawn()
                .expect("wl-copy");
            c.stdin.take().unwrap().write_all(b"captured text").unwrap();
            c.wait().unwrap();
        })
        .await
        .unwrap();
        let copy = tokio::time::timeout(Duration::from_secs(5), watch.recv())
            .await
            .expect("a local copy within 5 s")
            .expect("watch open");
        assert_eq!(copy.formats[0].mime, Mime::text_utf8());
        assert_eq!(copy.formats[0].size, 13);

        let (reply, rx) = oneshot::channel();
        adapter
            .local_reads()
            .send(LocalRead {
                capture: copy.capture,
                format: Mime::text_utf8(),
                file_idx: None,
                range: Some(clipline_core::ByteRange {
                    offset: 9,
                    len: 100,
                }),
                reply,
            })
            .await
            .unwrap();
        assert_eq!(rx.await.unwrap().unwrap().bytes, b"text");
    }
}
