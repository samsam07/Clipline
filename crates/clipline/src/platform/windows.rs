//! Windows `ClipboardAdapter` (M1). A dedicated **platform-affine pump thread** owns a
//! message-only window and the clipboard; `WM_RENDERFORMAT` on that thread is the
//! synchronous side of the lazy-render bridge (ARCHITECTURE.md; M0a). Per Finding D,
//! Windows *must* block that thread while core produces the bytes — so the handler
//! emits a [`RenderRequest`] and blocks on the reply up to the adapter-owned deadline
//! (D2), then `SetClipboardData`s the result or returns empty (graceful paste-fail).
//!
//! Core never sees any of this: it just drives `render_requests()` and answers.
//!
//! Threading map:
//! * **pump thread** — the only thread that touches the clipboard (Win32 affinity).
//!   Runs the message loop; serves renders; executes `set_promise`/`set_eager`
//!   commands marshalled to it via a channel + a wake message.
//! * **caller threads** (core/tests) — call the trait methods, which marshal to the
//!   pump thread and wait on a short ack.
//!
//! M1 covers text (`CF_UNICODETEXT`), image (`CF_DIB`, PNG-on-wire), and virtual files
//! (`CFSTR_FILEDESCRIPTORW`/`CFSTR_FILECONTENTS` via an `IDataObject`) whose contents
//! stream through the render bridge — no staging (locked decision #8, amended M1).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc as sync_mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};

use clipline_core::{
    AdapterError, ByteRange, CaptureId, ClipboardAdapter, FileEntry, FormatDesc, FormatReq, JobId,
    LocalCopy, LocalRead, LocalReadError, Mime, Offer, OriginId, Payload, RenderRequest,
    SensitivityHint, Seq,
};

use windows::core::HRESULT;
use windows::core::{implement, PCWSTR};
use windows::Win32::Foundation::{
    GlobalFree, SetLastError, DV_E_FORMATETC, DV_E_LINDEX, E_FAIL, E_INVALIDARG, E_NOTIMPL,
    E_OUTOFMEMORY, E_POINTER, HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, OLE_E_ADVISENOTSUPPORTED,
    STG_E_INVALIDFUNCTION, S_FALSE, S_OK, WIN32_ERROR, WPARAM,
};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_DIRECTORY;
use windows::Win32::System::Com::{
    IDataObject, IDataObject_Impl, IEnumFORMATETC, ISequentialStream_Impl, IStream, IStream_Impl,
    DATADIR_GET, DVASPECT_CONTENT, FORMATETC, LOCKTYPE, STATFLAG, STATSTG, STGC, STGMEDIUM,
    STGMEDIUM_0, STGTY_STREAM, STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET,
    TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
    GetClipboardOwner, IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
    RemoveClipboardFormatListener, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::System::Ole::{
    OleInitialize, OleSetClipboard, OleUninitialize, CF_DIB, CF_HDROP, CF_UNICODETEXT,
};
use windows::Win32::UI::Shell::{
    DragQueryFileW, IDataObjectAsyncCapability, IDataObjectAsyncCapability_Impl,
    SHCreateStdEnumFmtEtc, FD_ATTRIBUTES, FD_FILESIZE, FD_PROGRESSUI, FILEDESCRIPTORW, HDROP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetWindowLongPtrW, PostMessageW, PostQuitMessage, RegisterClassW, SetWindowLongPtrW,
    TranslateMessage, GWLP_USERDATA, HWND_MESSAGE, MSG, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
    WM_DESTROY, WM_RENDERFORMAT, WNDCLASSW,
};

use super::codec;

/// `WM_CLIPBOARDUPDATE` (declared locally to avoid a feature dependency).
const WM_CLIPBOARDUPDATE: u32 = 0x031D;
/// Drain the command channel (marshalled clipboard ops).
const WM_APP_CMD: u32 = WM_APP + 1;
/// Tear the window down on the pump thread (adapter drop).
const WM_APP_QUIT: u32 = WM_APP + 2;

/// Distinguishes window-class names when multiple adapters exist (tests).
static CLASS_SEQ: AtomicU32 = AtomicU32::new(0);

/// A clipboard op marshalled to the pump thread. `ack` reports completion back to the
/// caller (a plain sync channel — cheap, and safe to wait on from any thread).
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
}

/// One captured local copy (M3.2): what this node can still serve for a `seq` after the
/// OS clipboard has moved on (locked decision #6; SPEC.md §6 row 2).
///
/// The split is forced by the locked decisions, not chosen:
/// * `formats` holds **bytes**, snapshotted at copy. The clipboard holds one thing, and the
///   next copy destroys it — nothing else survives.
/// * `files` holds **absolute paths**, never bytes (locked decision #8: file bytes move on
///   a real paste, and only the bytes actually read). So a file pin is a pin on a path: it
///   does not stop the user editing or deleting the file, and a *copy* does not disturb the
///   old files — which is exactly what §6 row 2 asks for. `mstsc` behaves the same way.
///
/// Paths stay here and never reach core or the wire: they would leak this machine's
/// filesystem layout and mean nothing remotely (the wire carries `FileEntry.rel_path`).
#[derive(Debug, Default)]
struct WinCapture {
    formats: HashMap<Mime, Vec<u8>>,
    files: Vec<PathBuf>,
}

/// State shared with the pump thread (reached from the wndproc via the window's
/// user-data pointer). Every field is `Send + Sync`, so the adapter is `Sync` as the
/// trait requires.
struct PumpShared {
    /// The current promised head — its `origin_id`/`seq` key each forced render.
    current: Mutex<Option<Offer>>,
    render_tx: mpsc::UnboundedSender<RenderRequest>,
    watch_tx: mpsc::UnboundedSender<LocalCopy>,
    /// Announces a finished transfer job to core, which releases the origin's pin (M3.4).
    job_end_tx: mpsc::UnboundedSender<JobId>,
    /// Adapter-owned render deadline (D2). Generous on Windows (Finding A tolerates ~30 s).
    render_timeout: Duration,
    /// Commands awaiting execution on the pump thread. Drained on `WM_APP_CMD`.
    cmd_rx: Mutex<mpsc::UnboundedReceiver<Cmd>>,
    /// Snapshots of our local copies, keyed by the id handed to core in `LocalCopy`
    /// (M3.2). Written on the pump thread at copy; read by the `local_reads` server on a
    /// normal task; entries removed by `release_capture` when core unpins them.
    captures: Mutex<HashMap<CaptureId, Arc<WinCapture>>>,
    next_capture: AtomicU64,
}

/// The injected Windows clipboard adapter. See module docs.
pub struct WinClipboardAdapter {
    hwnd: isize,
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    render_rx: Mutex<Option<mpsc::UnboundedReceiver<RenderRequest>>>,
    watch_rx: Mutex<Option<mpsc::UnboundedReceiver<LocalCopy>>>,
    /// Core's handle for asking us to serve bytes of a copy we originated (M3.2).
    reads_tx: mpsc::Sender<LocalRead>,
    job_end_rx: Mutex<Option<mpsc::UnboundedReceiver<JobId>>>,
    shared: Arc<PumpShared>,
    pump: Option<JoinHandle<()>>,
    /// The `local_reads` server task; aborted on drop with the adapter.
    reads_task: Option<tokio::task::JoinHandle<()>>,
}

impl WinClipboardAdapter {
    /// Start the pump thread and claim a message-only window. `render_timeout` is the
    /// per-render deadline the pump enforces before releasing the OS call empty.
    pub fn new(render_timeout: Duration) -> Result<Self, AdapterError> {
        let (render_tx, render_rx) = mpsc::unbounded_channel();
        let (watch_tx, watch_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        let (reads_tx, reads_rx) = mpsc::channel(8);
        let (job_end_tx, job_end_rx) = mpsc::unbounded_channel();

        let shared = Arc::new(PumpShared {
            current: Mutex::new(None),
            render_tx,
            watch_tx,
            job_end_tx,
            render_timeout,
            cmd_rx: Mutex::new(cmd_rx),
            captures: Mutex::new(HashMap::new()),
            next_capture: AtomicU64::new(1),
        });

        let (ready_tx, ready_rx) = sync_mpsc::channel::<Result<isize, AdapterError>>();
        let shared_for_pump = Arc::clone(&shared);
        let pump = std::thread::Builder::new()
            .name("clipline-clipboard".into())
            .spawn(move || pump_main(shared_for_pump, ready_tx))
            .map_err(|e| AdapterError::Os(format!("spawn pump thread: {e}")))?;

        let hwnd = ready_rx
            .recv()
            .map_err(|_| AdapterError::Os("pump thread exited before ready".into()))??;

        // Serving reads is deliberately *not* on the pump thread — see `serve_local_reads`.
        let reads_task = tokio::runtime::Handle::try_current()
            .ok()
            .map(|rt| rt.spawn(serve_local_reads(reads_rx, Arc::clone(&shared))));

        Ok(WinClipboardAdapter {
            hwnd,
            cmd_tx,
            render_rx: Mutex::new(Some(render_rx)),
            watch_rx: Mutex::new(Some(watch_rx)),
            reads_tx,
            job_end_rx: Mutex::new(Some(job_end_rx)),
            shared,
            pump: Some(pump),
            reads_task,
        })
    }

    /// Marshal a command to the pump thread and wait for its ack.
    fn run_on_pump(
        &self,
        make: impl FnOnce(sync_mpsc::Sender<Result<(), AdapterError>>) -> Cmd,
    ) -> Result<(), AdapterError> {
        let (ack_tx, ack_rx) = sync_mpsc::channel();
        self.cmd_tx
            .send(make(ack_tx))
            .map_err(|_| AdapterError::Os("pump thread gone".into()))?;
        // Wake the message loop so it drains the command.
        unsafe {
            let _ = PostMessageW(Some(hwnd(self.hwnd)), WM_APP_CMD, WPARAM(0), LPARAM(0));
        }
        ack_rx
            .recv()
            .map_err(|_| AdapterError::Os("pump dropped command ack".into()))?
    }
}

impl Drop for WinClipboardAdapter {
    fn drop(&mut self) {
        if let Some(t) = self.reads_task.take() {
            t.abort();
        }
        unsafe {
            let _ = PostMessageW(Some(hwnd(self.hwnd)), WM_APP_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(h) = self.pump.take() {
            let _ = h.join();
        }
    }
}

impl ClipboardAdapter for WinClipboardAdapter {
    fn watch(&self) -> mpsc::UnboundedReceiver<LocalCopy> {
        take_or_empty(&self.watch_rx)
    }

    fn render_requests(&self) -> mpsc::UnboundedReceiver<RenderRequest> {
        take_or_empty(&self.render_rx)
    }

    fn set_promise(&self, offer: &Offer) -> Result<(), AdapterError> {
        let offer = offer.clone();
        self.run_on_pump(move |ack| Cmd::SetPromise { offer, ack })
    }

    fn set_eager(&self, offer: &Offer, payload: Payload) -> Result<(), AdapterError> {
        let offer = offer.clone();
        self.run_on_pump(move |ack| Cmd::SetEager {
            offer,
            payload,
            ack,
        })
    }

    fn job_ends(&self) -> mpsc::UnboundedReceiver<JobId> {
        take_or_empty(&self.job_end_rx)
    }

    fn local_reads(&self) -> mpsc::Sender<LocalRead> {
        self.reads_tx.clone()
    }

    fn release_capture(&self, capture: CaptureId) {
        // No pump marshal needed: a capture is our own memory (and, for files, just paths),
        // not clipboard state. Dropping it here is what actually frees a pinned copy.
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

    // No `materialize_files` (M1 decision — streaming, mstsc-style). The virtual-file
    // promise (`CFSTR_FILEDESCRIPTORW` + `CFSTR_FILECONTENTS` via `IDataObject`) is
    // advertised from `set_promise` when `offer.files` is non-empty, and each file's
    // contents are served on demand through the render bridge (`FormatReq.file_idx`).
}

fn take_or_empty<T>(
    slot: &Mutex<Option<mpsc::UnboundedReceiver<T>>>,
) -> mpsc::UnboundedReceiver<T> {
    slot.lock()
        .expect("receiver slot lock")
        .take()
        .unwrap_or_else(|| mpsc::unbounded_channel().1)
}

/// Rebuild an `HWND` from the adapter's stored pointer value.
fn hwnd(raw: isize) -> HWND {
    HWND(raw as *mut core::ffi::c_void)
}

// ---------------------------------------------------------------------------
// Pump thread
// ---------------------------------------------------------------------------

fn pump_main(shared: Arc<PumpShared>, ready: sync_mpsc::Sender<Result<isize, AdapterError>>) {
    unsafe {
        // OLE clipboard (virtual files) needs this thread to be an STA. Harmless for the
        // text/image `SetClipboardData` path. Paired with `OleUninitialize` below.
        if let Err(e) = OleInitialize(None) {
            let _ = ready.send(Err(AdapterError::Os(format!("OleInitialize: {e}"))));
            return;
        }

        let hinstance = match GetModuleHandleW(None) {
            Ok(h) => h,
            Err(e) => {
                let _ = ready.send(Err(AdapterError::Os(format!("GetModuleHandleW: {e}"))));
                return;
            }
        };

        let class_id = CLASS_SEQ.fetch_add(1, Ordering::Relaxed);
        let class_name: Vec<u16> = format!("CliplineClipboard-{class_id}")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance.into(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            let _ = ready.send(Err(AdapterError::Os("RegisterClassW failed".into())));
            return;
        }

        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class_name.as_ptr()),
            PCWSTR::null(),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinstance.into()),
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                let _ = ready.send(Err(AdapterError::Os(format!("CreateWindowExW: {e}"))));
                return;
            }
        };

        // Hand the shared state to the wndproc via user-data (one extra Arc ref, freed
        // on WM_DESTROY).
        let raw = Arc::into_raw(Arc::clone(&shared));
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, raw as isize);

        let _ = AddClipboardFormatListener(hwnd);

        let _ = ready.send(Ok(hwnd.0 as isize));

        // Message loop.
        let mut msg = MSG::default();
        loop {
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 <= 0 {
                break; // 0 = WM_QUIT, -1 = error
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        OleUninitialize();
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const PumpShared;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let shared = &*ptr;

    match msg {
        WM_RENDERFORMAT => {
            handle_render(shared, wparam.0 as u32);
            LRESULT(0)
        }
        WM_APP_CMD => {
            drain_commands(shared, hwnd);
            LRESULT(0)
        }
        WM_CLIPBOARDUPDATE => {
            handle_clipboard_update(shared, hwnd);
            LRESULT(0)
        }
        WM_APP_QUIT => {
            let _ = RemoveClipboardFormatListener(hwnd);
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            let raw = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const PumpShared;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            if !raw.is_null() {
                drop(Arc::from_raw(raw));
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// THE bridge (Finding D, Windows side). The OS is forcing a render of `cf`; block the
/// pump thread while core produces the bytes, up to the adapter deadline.
fn handle_render(shared: &PumpShared, cf: u32) {
    let Some(mime) = mime_for_cf(cf) else {
        return; // format we don't serve
    };
    let (origin_id, seq) = match &*shared.current.lock().expect("current head lock") {
        Some(o) => (o.origin_id, o.seq),
        None => return, // no promise set
    };

    let (reply_tx, reply_rx) = oneshot::channel();
    let job = JobId::next();
    let req = FormatReq {
        origin_id,
        seq,
        format: mime,
        file_idx: None,
        // Text and images are one blob: the OS wants all of it, in one call.
        range: None,
        // One `WM_RENDERFORMAT` is the whole transfer for a non-file format, so the render
        // *is* the job (SPEC.md §4). Files differ — see `serve_contents`.
        job,
    };
    if shared
        .render_tx
        .send(RenderRequest {
            req,
            reply: reply_tx,
        })
        .is_err()
    {
        return; // core render loop gone
    }

    // Block the pump thread on the reply (Windows must; Finding D), with the deadline.
    let outcome = wait_reply(reply_rx, shared.render_timeout);
    // The job is over either way: the OS asked once, and got bytes or did not. Telling core
    // now is what releases the origin's pin promptly rather than leaving it to the sweep
    // (M3.4). Failure ends the job just as completion does — the OS will not ask again.
    let _ = shared.job_end_tx.send(job);
    let payload = match outcome {
        Some(Ok(p)) => p,
        // Timeout or source failure -> return WITHOUT SetClipboardData = graceful
        // paste-fail (GetClipboardData yields NULL; the app is never hung).
        _ => return,
    };

    let bytes = match cf_bytes_for(cf, &payload) {
        Ok(b) => b,
        Err(e) => {
            tracing::debug!(error = %e, "render byte conversion failed");
            return;
        }
    };
    // In a render handler the clipboard is already open by the requester — set the data
    // directly, do NOT Open/Close it here.
    if let Err(e) = unsafe { set_current_data(cf, &bytes) } {
        tracing::debug!(error = %e, "SetClipboardData in render failed");
    }
}

/// Poll the oneshot reply on this (runtime-less) thread until it resolves or the
/// deadline passes. Adds ≤1 ms of latency; negligible against a network fetch, and the
/// pump thread is supposed to block here anyway (Finding D).
fn wait_reply(
    mut rx: oneshot::Receiver<Result<Payload, clipline_core::RenderError>>,
    timeout: Duration,
) -> Option<Result<Payload, clipline_core::RenderError>> {
    // Blocks this (non-async) OS thread on the reply. It polls with a short sleep because
    // the thread has no tokio runtime of its own, and the deadline (the adapter-owned
    // graceful-paste-fail budget, D2) must still be honoured — a blocking recv would need a
    // `runtime::Handle` plumbed in to get a timer. The ≤1 ms per-chunk cost is noise next to
    // the network round trip; replacing the poll is a deferred micro-optimization.
    let deadline = Instant::now() + timeout;
    loop {
        match rx.try_recv() {
            Ok(res) => return Some(res),
            Err(oneshot::error::TryRecvError::Closed) => return None,
            Err(oneshot::error::TryRecvError::Empty) => {
                if Instant::now() >= deadline {
                    return None;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

fn drain_commands(shared: &PumpShared, hwnd: HWND) {
    let cmds: Vec<Cmd> = {
        let mut rx = shared.cmd_rx.lock().expect("cmd_rx lock");
        let mut v = Vec::new();
        while let Ok(cmd) = rx.try_recv() {
            v.push(cmd);
        }
        v
    };
    for cmd in cmds {
        match cmd {
            Cmd::SetPromise { offer, ack } => {
                let r = do_set_promise(shared, hwnd, &offer);
                let _ = ack.send(r);
            }
            Cmd::SetEager {
                offer,
                payload,
                ack,
            } => {
                let r = do_set_eager(shared, hwnd, &offer, &payload);
                let _ = ack.send(r);
            }
        }
    }
}

/// Own the clipboard with delayed-render promises for `offer`'s supported formats
/// (SPEC.md §1; locked decision #2). NULL handle = delayed rendering.
fn do_set_promise(shared: &PumpShared, hwnd: HWND, offer: &Offer) -> Result<(), AdapterError> {
    // A file group is a virtual-file `IDataObject` (OLE), not a flat clipboard format —
    // see `do_set_promise_files`. (M1 serves files-only offers this way; mixing files with
    // text/image in one data object is a later refinement.)
    if !offer.files.is_empty() {
        return do_set_promise_files(shared, offer);
    }
    unsafe {
        open_clipboard(hwnd)?;
        let result = (|| {
            EmptyClipboard().map_err(|e| AdapterError::Os(format!("EmptyClipboard: {e}")))?;
            for fmt in &offer.formats {
                if let Some(cf) = cf_for_mime(&fmt.mime) {
                    // Delayed render hands the OS a NULL handle. The wrapper flags NULL
                    // returns as an "error", so pre-clear the last-error and accept a
                    // zero code as success (a genuine failure sets a non-zero GetLastError).
                    SetLastError(WIN32_ERROR(0));
                    if let Err(e) = SetClipboardData(cf, Some(HANDLE(std::ptr::null_mut()))) {
                        if e.code().0 != 0 {
                            return Err(AdapterError::Os(format!(
                                "SetClipboardData(delayed): {e}"
                            )));
                        }
                    }
                }
            }
            Ok(())
        })();
        let _ = CloseClipboard();
        if result.is_ok() {
            *shared.current.lock().expect("current head lock") = Some(offer.clone());
        }
        result
    }
}

/// Set the head with real bytes now (Continuous mode, small payload — SPEC.md §3), so
/// the OS historian can record it.
fn do_set_eager(
    shared: &PumpShared,
    hwnd: HWND,
    offer: &Offer,
    payload: &Payload,
) -> Result<(), AdapterError> {
    unsafe {
        open_clipboard(hwnd)?;
        let result = (|| {
            EmptyClipboard().map_err(|e| AdapterError::Os(format!("EmptyClipboard: {e}")))?;
            if let Some(cf) = cf_for_mime(&payload.format) {
                let bytes = cf_bytes_for(cf, payload)?;
                set_current_data(cf, &bytes)?;
            }
            Ok(())
        })();
        let _ = CloseClipboard();
        if result.is_ok() {
            *shared.current.lock().expect("current head lock") = Some(offer.clone());
        }
        result
    }
}

/// A local copy happened (ARCHITECTURE.md `watch`): enumerate what it is available in,
/// **capture** it, and tell core (M3.2).
///
/// Capture is what makes the origin able to serve a fetch later, after the clipboard has
/// moved on — see [`WinCapture`] for why non-file formats are snapshotted here while files
/// are only pathed. Runs on the pump thread: it is the only thread that may touch the
/// clipboard.
fn handle_clipboard_update(shared: &PumpShared, hwnd: HWND) {
    unsafe {
        let owner = GetClipboardOwner().unwrap_or(HWND(std::ptr::null_mut()));
        if owner.0 == hwnd.0 {
            return; // our own set_promise/set_eager
        }
    }

    let (capture, formats, files) = match capture_clipboard(hwnd) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!(error = %e, "could not capture local copy; ignoring");
            return;
        }
    };
    if formats.is_empty() && files.is_empty() {
        // Nothing we can carry (e.g. an app-private format only). Offering it would
        // promise a paste we cannot satisfy.
        tracing::debug!("local copy has no transferable format; ignoring");
        return;
    }

    // Content fingerprint for same-origin dup suppression (see `LocalCopy::content_hash`).
    // Non-file formats hash their bytes; files hash path + size (contents are never read at
    // copy — decision #8), which still distinguishes different file sets and matches an
    // identical re-copy of the same paths.
    let content_hash = fingerprint_capture(&capture, &files);

    let id = CaptureId(shared.next_capture.fetch_add(1, Ordering::Relaxed));
    shared
        .captures
        .lock()
        .expect("captures lock")
        .insert(id, Arc::new(capture));

    tracing::debug!(
        capture = %id,
        formats = formats.len(),
        files = files.len(),
        "captured local copy",
    );
    let _ = shared.watch_tx.send(LocalCopy {
        formats,
        files,
        capture: id,
        content_hash,
        sensitivity_hint: SensitivityHint::None, // consumed in M5 (SPEC.md §7)
    });
}

/// Fingerprint a capture's content for duplicate suppression. Stable regardless of format
/// map ordering; files contribute path + size, never contents (decision #8).
fn fingerprint_capture(capture: &WinCapture, files: &[FileEntry]) -> [u8; 32] {
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

/// Read everything transferable off the clipboard, once, on the pump thread.
///
/// Returns the capture plus the two manifests core needs (`FormatDesc`s and the file
/// group). Text and image are normalized to their wire forms here (SPEC.md §9: UTF-8 and
/// PNG) so the snapshot is already what a fetch will serve.
fn capture_clipboard(
    hwnd: HWND,
) -> Result<(WinCapture, Vec<FormatDesc>, Vec<FileEntry>), AdapterError> {
    unsafe {
        open_clipboard(hwnd)?;
        let result = (|| {
            let mut capture = WinCapture::default();
            let mut formats = Vec::new();

            // Text: CF_UNICODETEXT -> UTF-8 on the wire.
            if IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).is_ok() {
                let bytes = read_clipboard_global(CF_UNICODETEXT.0 as u32);
                if !bytes.is_empty() {
                    let units: Vec<u16> = bytes
                        .chunks_exact(2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                        .collect();
                    let text = codec::utf16_to_string(&units);
                    let utf8 = text.into_bytes();
                    formats.push(FormatDesc {
                        mime: Mime::text_utf8(),
                        size: utf8.len() as u64,
                    });
                    capture.formats.insert(Mime::text_utf8(), utf8);
                }
            }

            // Image: CF_DIB -> PNG on the wire.
            if IsClipboardFormatAvailable(CF_DIB.0 as u32).is_ok() {
                let dib = read_clipboard_global(CF_DIB.0 as u32);
                match codec::dib_to_png(&dib) {
                    Ok(png) => {
                        formats.push(FormatDesc {
                            mime: Mime::png(),
                            size: png.len() as u64,
                        });
                        capture.formats.insert(Mime::png(), png);
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "clipboard DIB did not convert; skipping")
                    }
                }
            }

            // Files: CF_HDROP gives real paths. Record the paths and stat them — **never**
            // read their bytes (locked decision #8). This is the by-reference capture.
            let files = if IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_ok() {
                let (paths, entries) = capture_hdrop()?;
                capture.files = paths;
                if !entries.is_empty() {
                    formats.push(FormatDesc {
                        mime: Mime::uri_list(),
                        size: entries.iter().map(|e| e.size).sum(),
                    });
                }
                entries
            } else {
                Vec::new()
            };

            Ok((capture, formats, files))
        })();
        let _ = CloseClipboard();
        result
    }
}

/// Copy an `HGLOBAL`-backed clipboard format out to a `Vec`. Empty on any failure — the
/// caller treats "no bytes" as "format not capturable".
///
/// Requires the clipboard to be open. Note this may block: for a *delayed-render* format
/// the OS calls back into the source app to produce the bytes, on this thread. That is the
/// price of snapshotting at copy time, which locked decision #6 leaves no way around for
/// non-file formats (see [`WinCapture`]). Files never come through here.
unsafe fn read_clipboard_global(cf: u32) -> Vec<u8> {
    let Ok(handle) = GetClipboardData(cf) else {
        return Vec::new();
    };
    let hglobal = HGLOBAL(handle.0);
    let ptr = GlobalLock(hglobal) as *const u8;
    if ptr.is_null() {
        return Vec::new();
    }
    let len = GlobalSize(hglobal);
    let mut out = vec![0u8; len];
    std::ptr::copy_nonoverlapping(ptr, out.as_mut_ptr(), len);
    let _ = GlobalUnlock(hglobal);
    out
}

/// Read `CF_HDROP` into (absolute paths, wire manifest). Paths stay local; the manifest is
/// names + sizes only.
///
/// A **directory** in the drop is walked recursively: every file under it becomes a
/// manifest entry whose `rel_path` keeps its position relative to the dropped item (the
/// folder name is the root, so `mydir` containing `sub/a.txt` yields `mydir/sub/a.txt`).
/// Only the *paths* are recorded — contents are still read on demand (locked decision #8);
/// the walk stats, it does not read.
///
/// The walk runs on the pump thread at copy time (it must, to build the offer's manifest),
/// so a copy of a huge tree briefly costs one stat per file. That mirrors what the shell
/// itself does for a virtual-file copy, and it is metadata only.
unsafe fn capture_hdrop() -> Result<(Vec<PathBuf>, Vec<FileEntry>), AdapterError> {
    let handle = GetClipboardData(CF_HDROP.0 as u32)
        .map_err(|e| AdapterError::Os(format!("GetClipboardData(CF_HDROP): {e}")))?;
    let hdrop = HDROP(handle.0);

    let count = DragQueryFileW(hdrop, u32::MAX, None);
    let mut paths = Vec::new();
    let mut entries = Vec::new();
    for i in 0..count {
        let len = DragQueryFileW(hdrop, i, None) as usize;
        if len == 0 {
            continue;
        }
        let mut buf = vec![0u16; len + 1];
        let written = DragQueryFileW(hdrop, i, Some(&mut buf)) as usize;
        let path = PathBuf::from(codec::utf16_to_string(&buf[..written]));

        // Root name = the dropped item's own name; the walk keys everything off it.
        let root = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        collect_path(&path, &root, &mut paths, &mut entries, 0);
    }
    Ok((paths, entries))
}

/// Recursion bound: a symlink loop or a pathological tree must not run away on the pump
/// thread. Deep real trees past this are truncated (rare; logged).
const MAX_DIR_DEPTH: usize = 64;

/// Add `abs` (a file → one entry; a directory → its files, recursively) to the manifest,
/// keyed by `rel` (a forward-slash path relative to the dropped root).
fn collect_path(
    abs: &Path,
    rel: &str,
    paths: &mut Vec<PathBuf>,
    entries: &mut Vec<FileEntry>,
    depth: usize,
) {
    // `symlink_metadata` does not follow links — a symlinked directory is not descended,
    // which is what stops loops and stops a folder copy from silently escaping its tree.
    let meta = match std::fs::symlink_metadata(abs) {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!(error = %e, "skipping unreadable clipboard path");
            return;
        }
    };

    if meta.file_type().is_symlink() {
        tracing::debug!("skipping symlink in a copied folder");
        return;
    }

    if meta.is_file() {
        entries.push(FileEntry::new(rel, meta.len()));
        paths.push(abs.to_path_buf());
        return;
    }

    if meta.is_dir() {
        if depth >= MAX_DIR_DEPTH {
            tracing::debug!(%rel, "folder deeper than the recursion bound; truncating");
            return;
        }
        let rd = match std::fs::read_dir(abs) {
            Ok(rd) => rd,
            Err(e) => {
                tracing::debug!(error = %e, "skipping unreadable directory");
                return;
            }
        };
        let mut empty = true;
        for child in rd.flatten() {
            empty = false;
            let child_rel = format!("{}/{}", rel, child.file_name().to_string_lossy());
            collect_path(&child.path(), &child_rel, paths, entries, depth + 1);
        }
        // A directory with children is implied by their paths; only a directory with *no*
        // entries needs its own manifest record, or it would vanish (the shell rebuilds
        // folders from file paths). The path is kept aligned with the entry but never read.
        if empty {
            entries.push(FileEntry::new_dir(rel));
            paths.push(abs.to_path_buf());
        }
    }
}

/// Serve `local_reads` (M3.2): the origin side of a peer's fetch.
///
/// Runs as an ordinary task, **not** on the pump thread: everything it needs was captured
/// at copy time, so nothing here touches the clipboard. That matters — a fetch of a large
/// file must never block the thread that owns the clipboard and serves pastes.
async fn serve_local_reads(mut rx: mpsc::Receiver<LocalRead>, shared: Arc<PumpShared>) {
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
            // A file: read the requested slice off disk, now. This is the whole point of
            // by-reference files (locked decision #8) — the bytes were never read at copy,
            // and only the asked-for range is read now.
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
            // A non-file format: already snapshotted, just slice it.
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

fn slice_of(bytes: &[u8], range: Option<ByteRange>) -> Vec<u8> {
    match range {
        None => bytes.to_vec(),
        // Past EOF is a short read, not an error — the caller treats it as the end.
        Some(ByteRange { offset, len }) => {
            let start = (offset as usize).min(bytes.len());
            let end = (start + len as usize).min(bytes.len());
            bytes[start..end].to_vec()
        }
    }
}

/// Read one range of a file. A failure here is the expected consequence of pinning a
/// *path* rather than bytes: the user may have edited or deleted it since the copy.
fn read_file_range(
    path: &Path,
    range: Option<ByteRange>,
    format: Mime,
) -> Result<Payload, LocalReadError> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)
        .map_err(|e| LocalReadError::SourceFailed(format!("open: {e}")))?;
    let bytes = match range {
        None => {
            let mut buf = Vec::new();
            file.read_to_end(&mut buf)
                .map_err(|e| LocalReadError::SourceFailed(format!("read: {e}")))?;
            buf
        }
        Some(ByteRange { offset, len }) => {
            file.seek(SeekFrom::Start(offset))
                .map_err(|e| LocalReadError::SourceFailed(format!("seek: {e}")))?;
            let mut buf = vec![0u8; len as usize];
            let mut got = 0usize;
            // Read to fill, tolerating short reads; a genuine EOF just ends it.
            while got < buf.len() {
                match file.read(&mut buf[got..]) {
                    Ok(0) => break,
                    Ok(n) => got += n,
                    Err(e) => return Err(LocalReadError::SourceFailed(format!("read: {e}"))),
                }
            }
            buf.truncate(got);
            buf
        }
    };
    Ok(Payload::new(format, bytes))
}

// ---------------------------------------------------------------------------
// Clipboard helpers (all called on the pump thread)
// ---------------------------------------------------------------------------

/// Open the clipboard for our window, retrying briefly if another process holds it.
unsafe fn open_clipboard(hwnd: HWND) -> Result<(), AdapterError> {
    let deadline = Instant::now() + Duration::from_millis(500);
    loop {
        if OpenClipboard(Some(hwnd)).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(AdapterError::Os("OpenClipboard timed out".into()));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Allocate a moveable global, copy `bytes` in, and hand it to `SetClipboardData` for
/// `cf` (the system takes ownership on success).
unsafe fn set_current_data(cf: u32, bytes: &[u8]) -> Result<(), AdapterError> {
    let hglobal = GlobalAlloc(GMEM_MOVEABLE, bytes.len())
        .map_err(|e| AdapterError::Os(format!("GlobalAlloc: {e}")))?;
    let dst = GlobalLock(hglobal);
    if dst.is_null() {
        let _ = GlobalFree(Some(hglobal));
        return Err(AdapterError::Os("GlobalLock returned null".into()));
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst as *mut u8, bytes.len());
    let _ = GlobalUnlock(hglobal);

    match SetClipboardData(cf, Some(HANDLE(hglobal.0))) {
        Ok(_) => Ok(()),
        Err(e) => {
            let _ = GlobalFree(Some(hglobal));
            Err(AdapterError::Os(format!("SetClipboardData: {e}")))
        }
    }
}

/// Wire MIME -> Windows clipboard format id, for formats M1 serves.
fn cf_for_mime(mime: &Mime) -> Option<u32> {
    let s = mime.as_str();
    if s.starts_with("text/plain") {
        Some(CF_UNICODETEXT.0 as u32)
    } else if s == "image/png" {
        Some(CF_DIB.0 as u32)
    } else {
        None
    }
}

/// The reverse mapping, for a forced render / update enumeration.
fn mime_for_cf(cf: u32) -> Option<Mime> {
    if cf == CF_UNICODETEXT.0 as u32 {
        Some(Mime::text_utf8())
    } else if cf == CF_DIB.0 as u32 {
        Some(Mime::png())
    } else {
        None
    }
}

/// Convert a wire `Payload` into the bytes for clipboard format `cf`.
fn cf_bytes_for(cf: u32, payload: &Payload) -> Result<Vec<u8>, AdapterError> {
    if cf == CF_UNICODETEXT.0 as u32 {
        let s = std::str::from_utf8(&payload.bytes)
            .map_err(|e| AdapterError::Os(format!("text payload not UTF-8: {e}")))?;
        let units = codec::text_to_utf16(s);
        let mut bytes = Vec::with_capacity(units.len() * 2);
        for u in units {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        Ok(bytes)
    } else if cf == CF_DIB.0 as u32 {
        codec::png_to_dib(&payload.bytes).map_err(|e| AdapterError::Os(format!("png->dib: {e}")))
    } else {
        Err(AdapterError::Os("unsupported clipboard format".into()))
    }
}

// ---------------------------------------------------------------------------
// Virtual files: an `IDataObject` advertising `CFSTR_FILEDESCRIPTORW` +
// `CFSTR_FILECONTENTS` (locked decision #8, amended M1 — streaming, mstsc-style;
// PLATFORM-NOTES Finding C). The descriptor (names/sizes) is served for free; each
// file's contents are pulled through the render bridge on demand and returned as an
// `IStream` — no staging dir.
// ---------------------------------------------------------------------------

/// Register (idempotently, process-wide) the two shell file formats; return their ids.
fn register_file_formats() -> (u16, u16) {
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }
    unsafe {
        let d = RegisterClipboardFormatW(PCWSTR(wide("FileGroupDescriptorW").as_ptr()));
        let c = RegisterClipboardFormatW(PCWSTR(wide("FileContents").as_ptr()));
        (d as u16, c as u16)
    }
}

/// Pack a `FILEGROUPDESCRIPTORW` (`cItems` + one `FILEDESCRIPTORW` per file) into a byte
/// buffer for an `HGLOBAL`. Names/sizes only — cheap and monitor-safe (Finding C).
fn build_file_group_descriptor(files: &[FileEntry]) -> Vec<u8> {
    let fd_size = std::mem::size_of::<FILEDESCRIPTORW>();
    let mut buf = vec![0u8; 4 + files.len() * fd_size];
    buf[0..4].copy_from_slice(&(files.len() as u32).to_le_bytes());
    for (i, entry) in files.iter().enumerate() {
        let mut name = [0u16; 260];
        // `rel_path` is the normalized forward-slash wire form (M3.2); the shell wants
        // backslashes for any nested path.
        let shell_name = entry.rel_path.replace('/', "\\");
        for (j, u) in shell_name.encode_utf16().take(259).enumerate() {
            name[j] = u;
        }
        // A directory entry (an empty folder to recreate) carries the directory attribute
        // and no size/progress — the shell makes the folder and never asks for contents.
        let (flags, attrs) = if entry.is_dir {
            (FD_ATTRIBUTES.0 as u32, FILE_ATTRIBUTE_DIRECTORY.0)
        } else {
            // `FD_FILESIZE`: size is known from the manifest without reading a byte — which
            // lets the shell show a real progress bar for a streamed file. `FD_PROGRESSUI`:
            // ask for that UI; a lazy paste is slow by construction and the user must see it
            // and be able to cancel (SPEC.md §6). M3 manual gate.
            ((FD_FILESIZE.0 | FD_PROGRESSUI.0) as u32, 0)
        };
        let fd = FILEDESCRIPTORW {
            dwFlags: flags,
            dwFileAttributes: attrs,
            nFileSizeHigh: (entry.size >> 32) as u32,
            nFileSizeLow: (entry.size & 0xFFFF_FFFF) as u32,
            cFileName: name,
            ..Default::default()
        };
        // Safe: read the whole local value's bytes through a u8 pointer (no field ref).
        let fd_bytes = unsafe {
            std::slice::from_raw_parts(&fd as *const FILEDESCRIPTORW as *const u8, fd_size)
        };
        buf[4 + i * fd_size..4 + (i + 1) * fd_size].copy_from_slice(fd_bytes);
    }
    buf
}

/// The virtual-file data object placed on the OLE clipboard for a files offer. Holds the
/// file manifest and a handle to the render bridge; serves `FILECONTENTS` lazily.
///
/// # Why it implements `IDataObjectAsyncCapability`
///
/// Without it the shell extracts the data **synchronously on its own UI thread**: Explorer
/// freezes for the whole transfer and never shows a copy dialog. Advertising async capability
/// is what licenses the shell to do the extraction on a background thread, which is the only
/// reason a progress dialog (and its Cancel button — SPEC.md §6) exists at all.
///
/// This is not an optimization. A lazy paste is by definition slow enough to need it: the
/// bytes are coming off a network on demand. `mstsc` does the same. Found by the M3 manual
/// gate — no in-process test can see it, because the freeze is *the shell's* threading
/// choice, not ours.
#[implement(IDataObject, IDataObjectAsyncCapability)]
struct FileDataObject {
    files: Vec<FileEntry>,
    origin_id: OriginId,
    seq: Seq,
    render_tx: mpsc::UnboundedSender<RenderRequest>,
    /// Announces each finished per-file job so the origin unpins (M3.4).
    job_end_tx: mpsc::UnboundedSender<JobId>,
    render_timeout: Duration,
    cf_descriptor: u16,
    cf_contents: u16,
    /// Async-extraction state, driven by the shell (`SetAsyncMode`/`StartOperation`/
    /// `EndOperation`). We *want* async, so it starts enabled.
    async_mode: Mutex<bool>,
    in_operation: Mutex<bool>,
}

impl FileDataObject {
    #[allow(clippy::too_many_arguments)]
    fn new(
        files: Vec<FileEntry>,
        origin_id: OriginId,
        seq: Seq,
        render_tx: mpsc::UnboundedSender<RenderRequest>,
        job_end_tx: mpsc::UnboundedSender<JobId>,
        render_timeout: Duration,
        cf_descriptor: u16,
        cf_contents: u16,
    ) -> Self {
        FileDataObject {
            files,
            origin_id,
            seq,
            render_tx,
            job_end_tx,
            render_timeout,
            cf_descriptor,
            cf_contents,
            // Default on: a consumer that does not understand async ignores the interface
            // and gets today's synchronous behaviour; one that does gets a background
            // extraction and a progress dialog.
            async_mode: Mutex::new(true),
            in_operation: Mutex::new(false),
        }
    }

    /// Serve the file-group descriptor (names/sizes) as an `HGLOBAL` — no bridge call.
    fn serve_descriptor(&self) -> windows::core::Result<STGMEDIUM> {
        let buf = build_file_group_descriptor(&self.files);
        let hglobal = unsafe {
            let h = GlobalAlloc(GMEM_MOVEABLE, buf.len()).map_err(|_| E_OUTOFMEMORY)?;
            let dst = GlobalLock(h);
            if dst.is_null() {
                let _ = GlobalFree(Some(h));
                return Err(E_OUTOFMEMORY.into());
            }
            std::ptr::copy_nonoverlapping(buf.as_ptr(), dst as *mut u8, buf.len());
            let _ = GlobalUnlock(h);
            h
        };
        Ok(STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: hglobal },
            pUnkForRelease: std::mem::ManuallyDrop::new(None),
        })
    }

    /// Serve one file's contents as a **lazily-pulling** `IStream` (M3.5).
    ///
    /// Returns immediately. No bytes have moved yet — they are fetched range by range as
    /// the pasting app reads the stream ([`LazyFileStream::Read`]). That is what locked
    /// decision #8 means by "only the bytes actually read", and it is also what keeps the
    /// render deadline honest: M0 Finding A budgets one *blocking OS call*, and until now
    /// this call was an entire multi-gigabyte transfer.
    fn serve_contents(&self, lindex: i32) -> windows::core::Result<STGMEDIUM> {
        if lindex < 0 || lindex as usize >= self.files.len() {
            return Err(DV_E_LINDEX.into());
        }
        let entry = &self.files[lindex as usize];
        // A directory entry (empty folder) has no contents to stream; the shell creates it
        // from the descriptor and should never ask. Reject defensively.
        if entry.is_dir {
            return Err(DV_E_FORMATETC.into());
        }
        let stream: IStream = LazyFileStream {
            origin_id: self.origin_id,
            seq: self.seq,
            file_idx: lindex as u32,
            size: entry.size,
            // One stream is one job, across every read it makes (M3 ruling Q12). Allocated
            // here, at the start of the transfer — not per read, which would let the
            // origin's pin lapse between two reads of the same file.
            job: JobId::next(),
            state: Mutex::new(StreamState::default()),
            render_tx: self.render_tx.clone(),
            job_end_tx: self.job_end_tx.clone(),
            render_timeout: self.render_timeout,
        }
        .into();

        Ok(STGMEDIUM {
            tymed: TYMED_ISTREAM.0 as u32,
            u: STGMEDIUM_0 {
                pstm: std::mem::ManuallyDrop::new(Some(stream)),
            },
            pUnkForRelease: std::mem::ManuallyDrop::new(None),
        })
    }
}

impl IDataObject_Impl for FileDataObject_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        let fe = unsafe { &*pformatetcin };
        if fe.cfFormat == self.cf_descriptor && fe.tymed & TYMED_HGLOBAL.0 as u32 != 0 {
            return self.serve_descriptor();
        }
        if fe.cfFormat == self.cf_contents && fe.tymed & TYMED_ISTREAM.0 as u32 != 0 {
            return self.serve_contents(fe.lindex);
        }
        Err(DV_E_FORMATETC.into())
    }

    fn GetDataHere(&self, _f: *const FORMATETC, _m: *mut STGMEDIUM) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> windows::core::HRESULT {
        let fe = unsafe { &*pformatetc };
        let ok = (fe.cfFormat == self.cf_descriptor && fe.tymed & TYMED_HGLOBAL.0 as u32 != 0)
            || (fe.cfFormat == self.cf_contents && fe.tymed & TYMED_ISTREAM.0 as u32 != 0);
        if ok {
            S_OK
        } else {
            S_FALSE
        }
    }

    fn GetCanonicalFormatEtc(
        &self,
        _in: *const FORMATETC,
        _out: *mut FORMATETC,
    ) -> windows::core::HRESULT {
        E_NOTIMPL
    }

    fn SetData(
        &self,
        _f: *const FORMATETC,
        _m: *const STGMEDIUM,
        _r: windows::core::BOOL,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
        if dwdirection != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        let fmts = [
            FORMATETC {
                cfFormat: self.cf_descriptor,
                ptd: std::ptr::null_mut(),
                dwAspect: DVASPECT_CONTENT.0,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
            },
            FORMATETC {
                cfFormat: self.cf_contents,
                ptd: std::ptr::null_mut(),
                dwAspect: DVASPECT_CONTENT.0,
                lindex: -1,
                tymed: TYMED_ISTREAM.0 as u32,
            },
        ];
        unsafe { SHCreateStdEnumFmtEtc(&fmts) }
    }

    fn DAdvise(
        &self,
        _f: *const FORMATETC,
        _advf: u32,
        _sink: windows::core::Ref<'_, windows::Win32::System::Com::IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _c: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows::core::Result<windows::Win32::System::Com::IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

/// The shell's async-extraction handshake (M3.5, from the manual gate).
///
/// The consumer asks `GetAsyncMode`; if it says yes, it calls `StartOperation`, does the
/// `GetData` reads on a background thread, and calls `EndOperation` when done. We hold no
/// resources across it — the pin lifecycle belongs to the streams themselves (M3.4), each of
/// which ends its own job on release — so these are almost pure state.
impl IDataObjectAsyncCapability_Impl for FileDataObject_Impl {
    fn SetAsyncMode(&self, fdoopasync: windows_core::BOOL) -> windows::core::Result<()> {
        *self.async_mode.lock().expect("async_mode lock") = fdoopasync.as_bool();
        Ok(())
    }

    fn GetAsyncMode(&self) -> windows::core::Result<windows_core::BOOL> {
        Ok((*self.async_mode.lock().expect("async_mode lock")).into())
    }

    fn StartOperation(
        &self,
        _pbcreserved: windows_core::Ref<'_, windows::Win32::System::Com::IBindCtx>,
    ) -> windows::core::Result<()> {
        *self.in_operation.lock().expect("in_operation lock") = true;
        tracing::debug!("shell started an async paste");
        Ok(())
    }

    fn InOperation(&self) -> windows::core::Result<windows_core::BOOL> {
        Ok((*self.in_operation.lock().expect("in_operation lock")).into())
    }

    fn EndOperation(
        &self,
        hresult: windows::core::HRESULT,
        _pbcreserved: windows_core::Ref<'_, windows::Win32::System::Com::IBindCtx>,
        _dweffects: u32,
    ) -> windows::core::Result<()> {
        *self.in_operation.lock().expect("in_operation lock") = false;
        // Cancel and failure both land here; the streams have already ended their own jobs
        // on release, so there is nothing to unwind — just say how it went.
        tracing::debug!(ok = hresult.is_ok(), "shell finished an async paste");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// The pulling file stream (M3.5) — the destination end of a lazy file paste.
// ---------------------------------------------------------------------------

/// One fetch window: how much the stream pulls from the origin per network round trip,
/// independent of the pasting app's (usually smaller) read size. 4 MiB amortizes the round
/// trip over ~16 wire chunks instead of paying it per read — the M3 manual gate found paste
/// throughput was round-trip-bound at the read size (~256 KiB), ~2.7 MB/s.
const READAHEAD_WINDOW: u64 = 4 * 1024 * 1024;

/// An `IStream` over a remote file, fetched **ahead of the reader** in windows.
///
/// Handed to the pasting app by `GetData(CFSTR_FILECONTENTS)`, which returns instantly. The
/// app then reads it like any file; each read is served from a buffered window, and the
/// *next* window is prefetched in the background so the network stays busy while the app
/// consumes the current one. Nothing is staged (locked decision #8) — abandon the paste and
/// at most one window beyond what was consumed has crossed the wire.
///
/// # Read-ahead (M3, from the manual gate)
///
/// Without windowing, every app read was one network round trip with nothing overlapping
/// it — throughput was `read_size / RTT`, ~2.7 MB/s on WiFi. Now:
/// * a **window** (`READAHEAD_WINDOW`) is fetched per round trip and buffered, so the RTT is
///   amortized over the whole window (the origin streams it back-to-back, no per-chunk RTT);
/// * the **next window** is requested as soon as the current one starts being consumed, so
///   the connection does not go idle between windows (which would also trip TCP slow-start).
///
/// Deeper pipelining (many windows in flight) is Phase 2; this alone lifts the ceiling by
/// ~10× while keeping the render-bridge model and a bounded footprint (~2 windows/stream).
///
/// # Which thread blocks
///
/// A read that misses the buffer blocks until its window arrives, on whatever thread COM
/// hands us (our `IDataObject` lives in the pump STA, so a cross-apartment caller's `Read`
/// is marshalled to the pump). A window is bounded, so the block is bounded — never the
/// whole transfer. Aggregating the free-threaded marshaler would take the pump out of the
/// path entirely; worth doing if it ever proves the bottleneck, not needed for correctness.
#[implement(IStream)]
struct LazyFileStream {
    origin_id: OriginId,
    seq: Seq,
    file_idx: u32,
    /// From the offer's manifest — the origin never re-states it, and `Stat` needs it.
    size: u64,
    /// This transfer, for the whole life of the stream (M3 ruling Q12).
    job: JobId,
    state: Mutex<StreamState>,
    render_tx: mpsc::UnboundedSender<RenderRequest>,
    job_end_tx: mpsc::UnboundedSender<JobId>,
    render_timeout: Duration,
}

/// The read cursor plus the buffered window and any in-flight prefetch. All of `IStream`'s
/// mutable state, behind one lock (the methods take `&self`).
#[derive(Default)]
struct StreamState {
    /// The reader's position in the file.
    pos: u64,
    /// The buffered window covers `[cache_start, cache_start + cache.len())`.
    cache_start: u64,
    cache: Vec<u8>,
    /// An issued-but-not-yet-consumed prefetch of the window starting at this offset.
    pending: Option<(
        u64,
        oneshot::Receiver<Result<Payload, clipline_core::RenderError>>,
    )>,
}

impl LazyFileStream {
    /// Issue a fetch for the window at `offset` (clamped to the file end) and return its
    /// reply channel *without blocking* — the caller either waits on it now (demand) or
    /// stashes it as a prefetch. `None` if the window is past EOF or the bridge is gone.
    fn request_window(
        &self,
        offset: u64,
    ) -> Option<oneshot::Receiver<Result<Payload, clipline_core::RenderError>>> {
        if offset >= self.size {
            return None;
        }
        let len = READAHEAD_WINDOW.min(self.size - offset);
        let (reply, rx) = oneshot::channel();
        let req = FormatReq {
            origin_id: self.origin_id,
            seq: self.seq,
            format: Mime::uri_list(),
            file_idx: Some(self.file_idx),
            range: Some(ByteRange { offset, len }),
            job: self.job,
        };
        self.render_tx.send(RenderRequest { req, reply }).ok()?;
        Some(rx)
    }

    /// Ensure `state.cache` covers `pos`, refilling (blocking) on a miss. Uses the pending
    /// prefetch when it matches. Returns `false` on a failed/timed-out fetch (paste-fail).
    fn ensure_cache(&self, state: &mut StreamState) -> bool {
        let pos = state.pos;
        let hit = pos >= state.cache_start && pos < state.cache_start + state.cache.len() as u64;
        if hit {
            return true;
        }
        // Refill the window at `pos`. A matching prefetch is likely already in flight (or
        // done); otherwise a stale prefetch is dropped and we request fresh.
        let rx = match state.pending.take() {
            Some((start, rx)) if start == pos => rx,
            _ => match self.request_window(pos) {
                Some(rx) => rx,
                None => return false,
            },
        };
        match wait_reply(rx, self.render_timeout).and_then(|r| r.ok()) {
            Some(payload) => {
                state.cache_start = pos;
                state.cache = payload.bytes;
                true
            }
            None => false,
        }
    }
}

impl Drop for LazyFileStream {
    /// The stream is released: the pasting app is finished with this file, so the job is
    /// over and the origin can unpin (M3.4). This is the honest end of a file transfer —
    /// the last `Read` is not, because a seek could always bring another.
    fn drop(&mut self) {
        let _ = self.job_end_tx.send(self.job);
    }
}

impl ISequentialStream_Impl for LazyFileStream_Impl {
    fn Read(&self, pv: *mut core::ffi::c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
        let mut wrote = 0u32;
        let result = (|| {
            if pv.is_null() {
                return E_POINTER;
            }
            let mut state = self.state.lock().expect("stream state lock");
            if state.pos >= self.size {
                return S_FALSE; // clean EOF
            }
            if !self.ensure_cache(&mut state) {
                // Graceful paste-fail: the app gets an error, never a hang and never a
                // silently short file (CONVENTIONS.md).
                return E_FAIL;
            }

            let off = (state.pos - state.cache_start) as usize;
            let avail = state.cache.len().saturating_sub(off);
            if avail == 0 {
                return S_FALSE; // origin returned a short window (file shrank under us)
            }
            // Bound by the caller's buffer (`cb` — never write past what we were handed,
            // whatever the origin sent), by what the window holds, and by the file end.
            let remaining = (self.size - state.pos) as usize;
            let n = (cb as usize).min(avail).min(remaining);
            unsafe {
                std::ptr::copy_nonoverlapping(state.cache[off..].as_ptr(), pv as *mut u8, n);
            }
            state.pos += n as u64;

            // Prefetch the next window as soon as we begin consuming this one, so the
            // connection does not idle. At most one window is read beyond what the app has
            // consumed (bounded footprint; decision #8's "only bytes read", within a window).
            let next = state.cache_start + state.cache.len() as u64;
            if next < self.size && state.pending.is_none() {
                if let Some(rx) = self.request_window(next) {
                    state.pending = Some((next, rx));
                }
            }
            wrote = n as u32;
            S_OK
        })();

        if !pcbread.is_null() {
            unsafe { *pcbread = wrote };
        }
        result
    }

    /// Read-only: the pasting app never writes back into a clipboard file.
    fn Write(&self, _pv: *const core::ffi::c_void, _cb: u32, _pcbwritten: *mut u32) -> HRESULT {
        E_NOTIMPL
    }
}

impl IStream_Impl for LazyFileStream_Impl {
    fn Seek(
        &self,
        dlibmove: i64,
        dworigin: STREAM_SEEK,
        plibnewposition: *mut u64,
    ) -> windows::core::Result<()> {
        let mut state = self.state.lock().expect("stream state lock");
        let base = match dworigin {
            STREAM_SEEK_SET => 0i64,
            STREAM_SEEK_CUR => state.pos as i64,
            STREAM_SEEK_END => self.size as i64,
            _ => return Err(E_INVALIDARG.into()),
        };
        let target = base.checked_add(dlibmove).ok_or(E_INVALIDARG)?;
        if target < 0 {
            return Err(E_INVALIDARG.into());
        }
        let target = target as u64;
        // A seek outside the buffered window abandons the read-ahead: the prefetched next
        // window is no longer what comes next, and the cache no longer covers `pos`. Drop
        // both so the next read refills at the seek target. (A seek *within* the window is
        // free — the cache still covers it.) Seeking past the end is legal; the next read
        // just reports EOF.
        let within =
            target >= state.cache_start && target < state.cache_start + state.cache.len() as u64;
        if !within {
            state.cache.clear();
            state.cache_start = 0;
            state.pending = None;
        }
        state.pos = target;
        if !plibnewposition.is_null() {
            unsafe { *plibnewposition = target };
        }
        Ok(())
    }

    fn Stat(&self, pstatstg: *mut STATSTG, _grfstatflag: &STATFLAG) -> windows::core::Result<()> {
        if pstatstg.is_null() {
            return Err(E_POINTER.into());
        }
        // Size comes from the offer's manifest — known without touching the origin, which
        // is the whole point of a by-reference file (locked decision #8).
        unsafe {
            *pstatstg = STATSTG {
                cbSize: self.size,
                r#type: STGTY_STREAM.0 as u32,
                ..Default::default()
            };
        }
        Ok(())
    }

    /// A second reader of the same file, at the same position — same job, since it is the
    /// same transfer. Ending it twice is harmless (the release is idempotent). The clone
    /// starts with an empty cache (its own read-ahead), only the position carries over.
    fn Clone(&self) -> windows::core::Result<IStream> {
        let pos = self.state.lock().expect("stream state lock").pos;
        Ok(LazyFileStream {
            origin_id: self.origin_id,
            seq: self.seq,
            file_idx: self.file_idx,
            size: self.size,
            job: self.job,
            state: Mutex::new(StreamState {
                pos,
                ..StreamState::default()
            }),
            render_tx: self.render_tx.clone(),
            job_end_tx: self.job_end_tx.clone(),
            render_timeout: self.render_timeout,
        }
        .into())
    }

    // Read-only stream: the rest of `IStream` is for writable/transacted storage.
    fn SetSize(&self, _libnewsize: u64) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn CopyTo(
        &self,
        _pstm: windows_core::Ref<'_, IStream>,
        _cb: u64,
        _pcbread: *mut u64,
        _pcbwritten: *mut u64,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn Commit(&self, _grfcommitflags: &STGC) -> windows::core::Result<()> {
        Ok(()) // nothing buffered to flush
    }
    fn Revert(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn LockRegion(
        &self,
        _liboffset: u64,
        _cb: u64,
        _dwlocktype: &LOCKTYPE,
    ) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }
    fn UnlockRegion(
        &self,
        _liboffset: u64,
        _cb: u64,
        _dwlocktype: u32,
    ) -> windows::core::Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }
}

/// Advertise a virtual file group on the OLE clipboard (STA pump thread). Each file's
/// contents stream through the render bridge on read — no staging (decision #8).
fn do_set_promise_files(shared: &PumpShared, offer: &Offer) -> Result<(), AdapterError> {
    let (cf_descriptor, cf_contents) = register_file_formats();
    let obj: IDataObject = FileDataObject::new(
        offer.files.clone(),
        offer.origin_id,
        offer.seq,
        shared.render_tx.clone(),
        shared.job_end_tx.clone(),
        shared.render_timeout,
        cf_descriptor,
        cf_contents,
    )
    .into();
    unsafe {
        OleSetClipboard(&obj).map_err(|e| AdapterError::Os(format!("OleSetClipboard: {e}")))?;
    }
    *shared.current.lock().expect("current head lock") = Some(offer.clone());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_core::{run_render_loop, ContentHash, FormatDesc, RenderResult, RenderSource};
    use windows::Win32::Foundation::HGLOBAL;
    use windows::Win32::System::DataExchange::GetClipboardData;
    use windows::Win32::System::Ole::ReleaseStgMedium;

    /// The folder walk (the testable half of `capture_hdrop`): a directory expands to its
    /// files, with `rel_path`s rooted at the dropped folder's name and using forward
    /// slashes — and it records paths, never reading contents (locked decision #8).
    #[test]
    fn collect_path_walks_a_folder_tree() {
        let base = std::env::temp_dir().join(format!("clipline-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("mydir");
        std::fs::create_dir_all(root.join("sub")).expect("mkdirs");
        std::fs::write(root.join("a.txt"), b"aaa").expect("a");
        std::fs::write(root.join("sub").join("b.txt"), b"bbbbb").expect("b");
        // (Empty-directory handling has its own test.)

        let mut paths = Vec::new();
        let mut entries = Vec::new();
        collect_path(&root, "mydir", &mut paths, &mut entries, 0);

        // Two files; non-empty dirs (mydir, sub) are implied by the file paths, no entries.
        assert_eq!(entries.len(), 2, "both files, no dir entries");
        assert_eq!(
            paths.len(),
            entries.len(),
            "paths and manifest stay aligned"
        );

        let mut by_rel: Vec<(String, u64)> = entries
            .iter()
            .map(|e| (e.rel_path.clone(), e.size))
            .collect();
        by_rel.sort();
        assert_eq!(
            by_rel,
            vec![
                ("mydir/a.txt".to_string(), 3),
                ("mydir/sub/b.txt".to_string(), 5),
            ],
            "rel paths rooted at the folder, forward slashes, sizes from stat",
        );

        // The recorded paths point at the real files (contents readable on demand, not now).
        for p in &paths {
            assert!(p.is_absolute() || p.exists(), "recorded a real path");
        }

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Empty directories are preserved as directory entries (an empty folder must survive
    /// the paste); non-empty ones are implied by their files and get no entry.
    #[test]
    fn collect_path_preserves_empty_directories() {
        let base = std::env::temp_dir().join(format!("clipline-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("mydir");
        std::fs::create_dir_all(root.join("hasfile")).expect("mkdirs");
        std::fs::create_dir_all(root.join("empty")).expect("empty");
        std::fs::create_dir_all(root.join("nested").join("deep_empty")).expect("nested");
        std::fs::write(root.join("hasfile").join("f.txt"), b"x").expect("f");

        let mut paths = Vec::new();
        let mut entries = Vec::new();
        collect_path(&root, "mydir", &mut paths, &mut entries, 0);
        assert_eq!(
            paths.len(),
            entries.len(),
            "paths and manifest stay aligned"
        );

        let files: Vec<&str> = entries
            .iter()
            .filter(|e| !e.is_dir)
            .map(|e| e.rel_path.as_str())
            .collect();
        let mut dirs: Vec<&str> = entries
            .iter()
            .filter(|e| e.is_dir)
            .map(|e| e.rel_path.as_str())
            .collect();
        dirs.sort();

        assert_eq!(files, vec!["mydir/hasfile/f.txt"], "the one file");
        assert_eq!(
            dirs,
            vec!["mydir/empty", "mydir/nested/deep_empty"],
            "each leaf-empty dir kept; hasfile/ and nested/ implied by descendants",
        );
        assert!(
            entries.iter().filter(|e| e.is_dir).all(|e| e.size == 0),
            "directory entries have size 0",
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A lone file collects as one entry named by its filename.
    #[test]
    fn collect_path_takes_a_single_file() {
        let f = std::env::temp_dir().join(format!("clipline-one-{}.bin", std::process::id()));
        std::fs::write(&f, vec![0u8; 10]).expect("write");

        let mut paths = Vec::new();
        let mut entries = Vec::new();
        let name = f.file_name().unwrap().to_string_lossy().into_owned();
        collect_path(&f, &name, &mut paths, &mut entries, 0);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].size, 10);
        assert_eq!(paths[0], f);

        let _ = std::fs::remove_file(&f);
    }

    struct ConstSource(Payload);
    impl RenderSource for ConstSource {
        async fn render(&self, _req: FormatReq) -> RenderResult {
            Ok(self.0.clone())
        }
    }

    fn offer_with(mime: Mime, size: u64) -> Offer {
        Offer {
            origin_id: OriginId(1),
            seq: Seq(1),
            formats: vec![FormatDesc { mime, size }],
            files: vec![],
            hash: ContentHash([0; 32]),
        }
    }

    unsafe fn read_clipboard_bytes(cf: u32) -> Vec<u8> {
        // Force a render by reading; the pump serves it on its own thread.
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if OpenClipboard(None).is_ok() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "OpenClipboard for read timed out"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        let handle = GetClipboardData(cf);
        let out = match handle {
            Ok(h) if !h.0.is_null() => {
                let hglobal = HGLOBAL(h.0);
                let size = windows::Win32::System::Memory::GlobalSize(hglobal);
                let ptr = GlobalLock(hglobal) as *const u8;
                let bytes = std::slice::from_raw_parts(ptr, size).to_vec();
                let _ = GlobalUnlock(hglobal);
                bytes
            }
            _ => Vec::new(),
        };
        let _ = CloseClipboard();
        out
    }

    // The clipboard is a single global OS resource, so both round-trips run in ONE test
    // (sequentially) rather than as separate tests that could race each other. Each
    // phase promises a format, then reads it back through the real `WM_RENDERFORMAT`
    // bridge (Finding D) and checks the wire<->OS conversion.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn render_round_trips_through_real_clipboard() {
        text_round_trip().await;
        image_round_trip().await;
        files_promise_via_ole().await;
    }

    /// The real `set_promise(files)` integration path: it builds a `FileDataObject` and
    /// `OleSetClipboard`s it on the STA pump thread. Success proves the OLE wiring; the
    /// `GetData` two-tier laziness itself is proven by `file_contents_serve_lazily_*`.
    async fn files_promise_via_ole() {
        let adapter = WinClipboardAdapter::new(Duration::from_secs(2)).expect("adapter");
        let requests = adapter.render_requests();
        let source = ConstSource(Payload::new(Mime::uri_list(), b"file-bytes".to_vec()));
        let loop_handle = tokio::spawn(run_render_loop(requests, source));

        let offer = Offer {
            origin_id: OriginId(1),
            seq: Seq(1),
            formats: vec![],
            files: vec![FileEntry::new("note.txt", 10)],
            hash: ContentHash([0; 32]),
        };
        adapter
            .set_promise(&offer)
            .expect("set_promise(files) via OleSetClipboard");

        drop(adapter);
        loop_handle.abort();
    }

    async fn text_round_trip() {
        let adapter = WinClipboardAdapter::new(Duration::from_secs(2)).expect("adapter");
        let requests = adapter.render_requests();

        let text = "hello from clipline — 🌐";
        let source = ConstSource(Payload::new(Mime::text_utf8(), text.as_bytes().to_vec()));
        let loop_handle = tokio::spawn(run_render_loop(requests, source));

        adapter
            .set_promise(&offer_with(Mime::text_utf8(), text.len() as u64))
            .expect("set_promise");

        let bytes = tokio::task::spawn_blocking(|| unsafe {
            read_clipboard_bytes(CF_UNICODETEXT.0 as u32)
        })
        .await
        .unwrap();

        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(codec::utf16_to_string(&units), text);

        drop(adapter);
        loop_handle.abort();
    }

    async fn image_round_trip() {
        let adapter = WinClipboardAdapter::new(Duration::from_secs(2)).expect("adapter");
        let requests = adapter.render_requests();

        // A 2x2 RGBA PNG with alpha.
        let mut src = image::RgbaImage::new(2, 2);
        src.put_pixel(0, 0, image::Rgba([255, 0, 0, 255]));
        src.put_pixel(1, 0, image::Rgba([0, 255, 0, 200]));
        src.put_pixel(0, 1, image::Rgba([0, 0, 255, 128]));
        src.put_pixel(1, 1, image::Rgba([9, 8, 7, 6]));
        let mut png = Vec::new();
        src.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();

        let source = ConstSource(Payload::new(Mime::png(), png.clone()));
        let loop_handle = tokio::spawn(run_render_loop(requests, source));

        adapter
            .set_promise(&offer_with(Mime::png(), png.len() as u64))
            .expect("set_promise");

        let dib = tokio::task::spawn_blocking(|| unsafe { read_clipboard_bytes(CF_DIB.0 as u32) })
            .await
            .unwrap();
        assert!(!dib.is_empty(), "clipboard returned no DIB");

        let png_back = codec::dib_to_png(&dib).expect("dib->png");
        let back = image::load_from_memory(&png_back).unwrap().to_rgba8();
        assert_eq!(back, src, "image survives PNG->clipboard->DIB->PNG");

        drop(adapter);
        loop_handle.abort();
    }

    // A render source that counts calls and returns per-file bytes by index.
    /// Stands in for the mesh fetch: serves per-file bytes, honouring `range` exactly as a
    /// real origin does — a source that ignored it would let this test pass while the
    /// stream read the wrong bytes.
    struct CountingFileSource {
        count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        contents: Vec<Vec<u8>>,
    }
    impl RenderSource for CountingFileSource {
        async fn render(&self, req: FormatReq) -> RenderResult {
            self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let idx = req.file_idx.expect("file render needs an index") as usize;
            let whole = &self.contents[idx];
            let bytes = match req.range {
                None => whole.clone(),
                Some(r) => {
                    let start = (r.offset as usize).min(whole.len());
                    let end = (start + r.len as usize).min(whole.len());
                    whole[start..end].to_vec()
                }
            };
            Ok(Payload::new(Mime::uri_list(), bytes))
        }
    }

    unsafe fn read_descriptor(medium: &STGMEDIUM) -> (u32, String) {
        let hglobal = medium.u.hGlobal;
        let ptr = GlobalLock(hglobal) as *const u8;
        let count = std::ptr::read_unaligned(ptr as *const u32);
        let fd: FILEDESCRIPTORW = std::ptr::read_unaligned(ptr.add(4) as *const FILEDESCRIPTORW);
        let name_units = fd.cFileName;
        let end = name_units
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(name_units.len());
        let name = String::from_utf16_lossy(&name_units[..end]);
        let _ = GlobalUnlock(hglobal);
        (count, name)
    }

    unsafe fn read_stream(medium: &STGMEDIUM) -> Vec<u8> {
        let stream = medium.u.pstm.as_ref().expect("pstm").clone();
        let mut out = Vec::new();
        let mut buf = [0u8; 256];
        loop {
            let mut read = 0u32;
            let hr = stream.Read(
                buf.as_mut_ptr() as *mut _,
                buf.len() as u32,
                Some(&mut read as *mut u32),
            );
            if read > 0 {
                out.extend_from_slice(&buf[..read as usize]);
            }
            if read == 0 || hr.is_err() {
                break;
            }
        }
        out
    }

    /// Two-tier laziness (the M0a spike's in-process pull): the descriptor (names/sizes)
    /// is served with **zero** bridge calls; a file's contents are served only on request,
    /// pulled through the render bridge — proving files stream lazily with no staging.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn file_contents_serve_lazily_through_bridge() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let (cf_desc, cf_contents) = register_file_formats();
        let count = Arc::new(AtomicUsize::new(0));
        let (render_tx, render_rx) = mpsc::unbounded_channel();
        let source = CountingFileSource {
            count: count.clone(),
            contents: vec![b"contents-of-file-zero".to_vec(), b"file-one!".to_vec()],
        };
        let loop_handle = tokio::spawn(run_render_loop(render_rx, source));

        let files = vec![FileEntry::new("a.txt", 21), FileEntry::new("b.txt", 9)];

        let count_probe = count.clone();
        let (job_end_tx, _job_end_rx) = mpsc::unbounded_channel();
        let result = tokio::task::spawn_blocking(move || unsafe {
            let obj: IDataObject = FileDataObject::new(
                files,
                OriginId(1),
                Seq(1),
                render_tx,
                job_end_tx,
                Duration::from_secs(2),
                cf_desc,
                cf_contents,
            )
            .into();

            // 1) Descriptor: names/sizes for free — must NOT touch the bridge.
            let desc_fe = FORMATETC {
                cfFormat: cf_desc,
                ptd: std::ptr::null_mut(),
                dwAspect: DVASPECT_CONTENT.0,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
            };
            let desc = obj.GetData(&desc_fe).expect("descriptor");
            let (n_items, first_name) = read_descriptor(&desc);
            assert_eq!(n_items, 2);
            assert_eq!(first_name, "a.txt");
            assert_eq!(
                count_probe.load(Ordering::SeqCst),
                0,
                "descriptor must be free"
            );

            // 2) File contents (index 1): pulled through the render bridge on demand.
            let contents_fe = FORMATETC {
                cfFormat: cf_contents,
                ptd: std::ptr::null_mut(),
                dwAspect: DVASPECT_CONTENT.0,
                lindex: 1,
                tymed: TYMED_ISTREAM.0 as u32,
            };
            let mut medium = obj.GetData(&contents_fe).expect("file contents");
            let bytes = read_stream(&medium);
            ReleaseStgMedium(&mut medium); // the consumer owns the reference GetData gave
            (count_probe.load(Ordering::SeqCst), bytes)
        })
        .await
        .unwrap();

        assert_eq!(result.0, 1, "exactly one render for the one file read");
        assert_eq!(result.1, b"file-one!");
        loop_handle.abort();
    }

    /// **M3.5, the point of it.** `GetData` hands back the stream having fetched *nothing*,
    /// and each read pulls only its own range. Before this, `GetData` blocked until the
    /// whole file had crossed the wire — which buffered a multi-gigabyte file in RAM and
    /// made the render deadline cover an entire transfer instead of one call (Finding A).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn file_stream_fetches_nothing_until_read_and_then_only_ranges() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let (cf_desc, cf_contents) = register_file_formats();
        let count = Arc::new(AtomicUsize::new(0));
        let (render_tx, render_rx) = mpsc::unbounded_channel();
        // 1 KiB of file, read 64 bytes at a time. Read-ahead fetches one window covering the
        // whole file, so the 16 small reads become a single network pull (M3 perf).
        let body: Vec<u8> = (0..1024u32).map(|i| i as u8).collect();
        let source = CountingFileSource {
            count: count.clone(),
            contents: vec![body.clone()],
        };
        let loop_handle = tokio::spawn(run_render_loop(render_rx, source));

        let files = vec![FileEntry::new("big.bin", body.len() as u64)];
        let (job_end_tx, mut job_end_rx) = mpsc::unbounded_channel();
        let count_probe = count.clone();
        let expected = body.clone();

        let (after_getdata, pulls, bytes) = tokio::task::spawn_blocking(move || unsafe {
            let obj: IDataObject = FileDataObject::new(
                files,
                OriginId(1),
                Seq(1),
                render_tx,
                job_end_tx,
                Duration::from_secs(5),
                cf_desc,
                cf_contents,
            )
            .into();

            let fe = FORMATETC {
                cfFormat: cf_contents,
                ptd: std::ptr::null_mut(),
                dwAspect: DVASPECT_CONTENT.0,
                lindex: 0,
                tymed: TYMED_ISTREAM.0 as u32,
            };
            let mut medium = obj.GetData(&fe).expect("file contents");
            // The stream exists; not one byte has been fetched.
            let after_getdata = count_probe.load(Ordering::SeqCst);

            let stream = medium.u.pstm.as_ref().expect("pstm").clone();
            // `Stat` answers from the offer manifest — still no fetch.
            let mut stat = STATSTG::default();
            stream.Stat(&mut stat, STATFLAG(0)).expect("stat");
            assert_eq!(stat.cbSize, 1024, "size comes from the manifest");

            let mut out = Vec::new();
            let mut buf = [0u8; 64];
            loop {
                let mut read = 0u32;
                let hr = stream.Read(buf.as_mut_ptr() as *mut _, 64, Some(&mut read as *mut u32));
                if read > 0 {
                    out.extend_from_slice(&buf[..read as usize]);
                }
                if read == 0 || hr.is_err() {
                    break;
                }
            }
            let pulls = count_probe.load(Ordering::SeqCst);

            // Release it exactly as a consumer does. `STGMEDIUM.pstm` is `ManuallyDrop`, so
            // the reference `GetData` handed out is the caller's to free — the job does not
            // end until they do.
            drop(stream);
            ReleaseStgMedium(&mut medium);
            (after_getdata, pulls, out)
        })
        .await
        .unwrap();

        assert_eq!(after_getdata, 0, "GetData must not fetch a single byte");
        assert_eq!(bytes, expected, "the file reassembles exactly");
        assert_eq!(
            pulls, 1,
            "one read-ahead window covers the whole 1 KiB file"
        );

        // Releasing the stream ends the job, which is what unpins the origin (M3.4).
        let ended = job_end_rx.recv().await;
        assert!(ended.is_some(), "dropping the stream announces the job end");

        loop_handle.abort();
    }

    /// A source that answers a range with *more* bytes than were asked for — a buggy or
    /// hostile origin.
    struct OversizedSource;
    impl RenderSource for OversizedSource {
        async fn render(&self, _req: FormatReq) -> RenderResult {
            Ok(Payload::new(Mime::uri_list(), vec![0xAA; 4096]))
        }
    }

    /// `Read` must never write past the buffer it was handed, whatever the origin says.
    /// A trusted LAN is not a reason to trust a length off the wire with someone else's
    /// memory.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn read_clamps_an_oversized_response_to_the_callers_buffer() {
        let (cf_desc, cf_contents) = register_file_formats();
        let (render_tx, render_rx) = mpsc::unbounded_channel();
        let loop_handle = tokio::spawn(run_render_loop(render_rx, OversizedSource));

        // The manifest says 16 bytes, so the stream requests a 16-byte window — but the
        // source answers with 4096. Both the caller's `cb` and the manifest size must bound
        // what we write.
        let files = vec![FileEntry::new("evil.bin", 16)];
        let (job_end_tx, _job_end_rx) = mpsc::unbounded_channel();

        let (read, canary_intact) = tokio::task::spawn_blocking(move || unsafe {
            let obj: IDataObject = FileDataObject::new(
                files,
                OriginId(1),
                Seq(1),
                render_tx,
                job_end_tx,
                Duration::from_secs(5),
                cf_desc,
                cf_contents,
            )
            .into();
            let fe = FORMATETC {
                cfFormat: cf_contents,
                ptd: std::ptr::null_mut(),
                dwAspect: DVASPECT_CONTENT.0,
                lindex: 0,
                tymed: TYMED_ISTREAM.0 as u32,
            };
            let mut medium = obj.GetData(&fe).expect("file contents");
            let stream = medium.u.pstm.as_ref().expect("pstm").clone();

            // 16 bytes of "buffer" followed by a canary the stream must not touch.
            let mut region = [0u8; 64];
            region[16..].fill(0x5A);
            let mut read = 0u32;
            let _ = stream.Read(
                region.as_mut_ptr() as *mut _,
                16,
                Some(&mut read as *mut u32),
            );
            let out = (read, region[16..].iter().all(|b| *b == 0x5A));
            drop(stream);
            ReleaseStgMedium(&mut medium);
            out
        })
        .await
        .unwrap();

        assert!(canary_intact, "Read wrote past the caller's buffer");
        assert!(read <= 16, "Read reported more than the buffer size");
        loop_handle.abort();
    }

    /// A file larger than one read-ahead window reassembles exactly across window
    /// boundaries, and costs one network fetch **per window** (not per read) — the
    /// read-ahead win. A short read at each boundary is normal for `IStream`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn multi_window_file_reassembles_and_fetches_per_window() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let (cf_desc, cf_contents) = register_file_formats();
        let count = Arc::new(AtomicUsize::new(0));
        let (render_tx, render_rx) = mpsc::unbounded_channel();

        // 5 MiB spans two 4 MiB windows: [0,4 MiB) and [4 MiB,5 MiB).
        let size = 5 * 1024 * 1024usize;
        let body: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let source = CountingFileSource {
            count: count.clone(),
            contents: vec![body.clone()],
        };
        let loop_handle = tokio::spawn(run_render_loop(render_rx, source));

        let files = vec![FileEntry::new("big.bin", size as u64)];
        let (job_end_tx, _job_end_rx) = mpsc::unbounded_channel();
        let count_probe = count.clone();
        let expected = body.clone();

        let (pulls, bytes) = tokio::task::spawn_blocking(move || unsafe {
            let obj: IDataObject = FileDataObject::new(
                files,
                OriginId(1),
                Seq(1),
                render_tx,
                job_end_tx,
                Duration::from_secs(10),
                cf_desc,
                cf_contents,
            )
            .into();
            let fe = FORMATETC {
                cfFormat: cf_contents,
                ptd: std::ptr::null_mut(),
                dwAspect: DVASPECT_CONTENT.0,
                lindex: 0,
                tymed: TYMED_ISTREAM.0 as u32,
            };
            let mut medium = obj.GetData(&fe).expect("file contents");
            let stream = medium.u.pstm.as_ref().expect("pstm").clone();

            // Read in 256 KiB chunks, as Explorer does.
            let mut out = Vec::new();
            let mut buf = vec![0u8; 256 * 1024];
            loop {
                let mut read = 0u32;
                let hr = stream.Read(
                    buf.as_mut_ptr() as *mut _,
                    buf.len() as u32,
                    Some(&mut read as *mut u32),
                );
                if read > 0 {
                    out.extend_from_slice(&buf[..read as usize]);
                }
                if read == 0 || hr.is_err() {
                    break;
                }
            }
            let pulls = count_probe.load(Ordering::SeqCst);
            drop(stream);
            ReleaseStgMedium(&mut medium);
            (pulls, out)
        })
        .await
        .unwrap();

        assert_eq!(bytes.len(), expected.len(), "every byte arrived");
        assert_eq!(
            bytes, expected,
            "5 MiB reassembles across the window boundary"
        );
        assert_eq!(pulls, 2, "two windows fetched, not one-per-256-KiB-read");

        loop_handle.abort();
    }
}
