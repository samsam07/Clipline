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

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc as sync_mpsc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, oneshot};

use clipline_core::{
    AdapterError, ClipboardAdapter, FileEntry, FormatReq, LocalCopy, Mime, Offer, OriginId,
    Payload, RenderRequest, SensitivityHint, Seq,
};

use windows::core::{implement, PCWSTR};
use windows::Win32::Foundation::{
    GlobalFree, SetLastError, DV_E_FORMATETC, DV_E_LINDEX, E_FAIL, E_NOTIMPL, E_OUTOFMEMORY,
    HANDLE, HWND, LPARAM, LRESULT, OLE_E_ADVISENOTSUPPORTED, S_FALSE, S_OK, WIN32_ERROR, WPARAM,
};
use windows::Win32::System::Com::{
    IDataObject, IDataObject_Impl, IEnumFORMATETC, DATADIR_GET, DVASPECT_CONTENT, FORMATETC,
    STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL, TYMED_ISTREAM,
};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardOwner, OpenClipboard,
    RegisterClipboardFormatW, RemoveClipboardFormatListener, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::{
    OleInitialize, OleSetClipboard, OleUninitialize, CF_DIB, CF_UNICODETEXT,
};
use windows::Win32::UI::Shell::{
    SHCreateMemStream, SHCreateStdEnumFmtEtc, FD_FILESIZE, FILEDESCRIPTORW,
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

/// State shared with the pump thread (reached from the wndproc via the window's
/// user-data pointer). Every field is `Send + Sync`, so the adapter is `Sync` as the
/// trait requires.
struct PumpShared {
    /// The current promised head — its `origin_id`/`seq` key each forced render.
    current: Mutex<Option<Offer>>,
    render_tx: mpsc::UnboundedSender<RenderRequest>,
    watch_tx: mpsc::UnboundedSender<LocalCopy>,
    /// Adapter-owned render deadline (D2). Generous on Windows (Finding A tolerates ~30 s).
    render_timeout: Duration,
    /// Commands awaiting execution on the pump thread. Drained on `WM_APP_CMD`.
    cmd_rx: Mutex<mpsc::UnboundedReceiver<Cmd>>,
}

/// The injected Windows clipboard adapter. See module docs.
pub struct WinClipboardAdapter {
    hwnd: isize,
    cmd_tx: mpsc::UnboundedSender<Cmd>,
    render_rx: Mutex<Option<mpsc::UnboundedReceiver<RenderRequest>>>,
    watch_rx: Mutex<Option<mpsc::UnboundedReceiver<LocalCopy>>>,
    pump: Option<JoinHandle<()>>,
}

impl WinClipboardAdapter {
    /// Start the pump thread and claim a message-only window. `render_timeout` is the
    /// per-render deadline the pump enforces before releasing the OS call empty.
    pub fn new(render_timeout: Duration) -> Result<Self, AdapterError> {
        let (render_tx, render_rx) = mpsc::unbounded_channel();
        let (watch_tx, watch_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        let shared = Arc::new(PumpShared {
            current: Mutex::new(None),
            render_tx,
            watch_tx,
            render_timeout,
            cmd_rx: Mutex::new(cmd_rx),
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

        Ok(WinClipboardAdapter {
            hwnd,
            cmd_tx,
            render_rx: Mutex::new(Some(render_rx)),
            watch_rx: Mutex::new(Some(watch_rx)),
            pump: Some(pump),
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
    let req = FormatReq {
        origin_id,
        seq,
        format: mime,
        file_idx: None,
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
    let payload = match wait_reply(reply_rx, shared.render_timeout) {
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

/// Best-effort local-copy notification (ARCHITECTURE.md `watch`). M1 wires the channel
/// and suppresses our own copies; full format enumeration is M2 (when core consumes it).
fn handle_clipboard_update(shared: &PumpShared, hwnd: HWND) {
    unsafe {
        let owner = GetClipboardOwner().unwrap_or(HWND(std::ptr::null_mut()));
        if owner.0 == hwnd.0 {
            return; // our own set_promise/set_eager
        }
    }
    let _ = shared.watch_tx.send(LocalCopy {
        formats: Vec::new(),
        sensitivity_hint: SensitivityHint::None,
    });
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
        for (j, u) in entry
            .rel_path
            .to_string_lossy()
            .encode_utf16()
            .take(259)
            .enumerate()
        {
            name[j] = u;
        }
        let fd = FILEDESCRIPTORW {
            dwFlags: FD_FILESIZE.0 as u32,
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
#[implement(IDataObject)]
struct FileDataObject {
    files: Vec<FileEntry>,
    origin_id: OriginId,
    seq: Seq,
    render_tx: mpsc::UnboundedSender<RenderRequest>,
    render_timeout: Duration,
    cf_descriptor: u16,
    cf_contents: u16,
}

impl FileDataObject {
    #[allow(clippy::too_many_arguments)]
    fn new(
        files: Vec<FileEntry>,
        origin_id: OriginId,
        seq: Seq,
        render_tx: mpsc::UnboundedSender<RenderRequest>,
        render_timeout: Duration,
        cf_descriptor: u16,
        cf_contents: u16,
    ) -> Self {
        FileDataObject {
            files,
            origin_id,
            seq,
            render_tx,
            render_timeout,
            cf_descriptor,
            cf_contents,
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

    /// Serve one file's contents as an `IStream`, pulling the bytes through the render
    /// bridge (blocks up to the deadline — Windows tolerates it; Finding A/D). No staging.
    fn serve_contents(&self, lindex: i32) -> windows::core::Result<STGMEDIUM> {
        if lindex < 0 || lindex as usize >= self.files.len() {
            return Err(DV_E_LINDEX.into());
        }
        let (reply, rx) = oneshot::channel();
        let req = FormatReq {
            origin_id: self.origin_id,
            seq: self.seq,
            format: Mime::uri_list(),
            file_idx: Some(lindex as u32),
        };
        if self.render_tx.send(RenderRequest { req, reply }).is_err() {
            return Err(E_FAIL.into());
        }
        let payload = match wait_reply(rx, self.render_timeout) {
            Some(Ok(p)) => p,
            // Timeout / origin gone → graceful paste-fail (the consumer gets an error).
            _ => return Err(E_FAIL.into()),
        };
        let stream = unsafe { SHCreateMemStream(Some(&payload.bytes)) }.ok_or(E_OUTOFMEMORY)?;
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

/// Advertise a virtual file group on the OLE clipboard (STA pump thread). Each file's
/// contents stream through the render bridge on read — no staging (decision #8).
fn do_set_promise_files(shared: &PumpShared, offer: &Offer) -> Result<(), AdapterError> {
    let (cf_descriptor, cf_contents) = register_file_formats();
    let obj: IDataObject = FileDataObject::new(
        offer.files.clone(),
        offer.origin_id,
        offer.seq,
        shared.render_tx.clone(),
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
            files: vec![FileEntry {
                rel_path: "note.txt".into(),
                size: 10,
            }],
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
    struct CountingFileSource {
        count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        contents: Vec<Vec<u8>>,
    }
    impl RenderSource for CountingFileSource {
        async fn render(&self, req: FormatReq) -> RenderResult {
            self.count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let idx = req.file_idx.expect("file render needs an index") as usize;
            Ok(Payload::new(Mime::uri_list(), self.contents[idx].clone()))
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

        let files = vec![
            FileEntry {
                rel_path: "a.txt".into(),
                size: 21,
            },
            FileEntry {
                rel_path: "b.txt".into(),
                size: 9,
            },
        ];

        let count_probe = count.clone();
        let result = tokio::task::spawn_blocking(move || unsafe {
            let obj: IDataObject = FileDataObject::new(
                files,
                OriginId(1),
                Seq(1),
                render_tx,
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
            let medium = obj.GetData(&contents_fe).expect("file contents");
            let bytes = read_stream(&medium);
            (count_probe.load(Ordering::SeqCst), bytes)
        })
        .await
        .unwrap();

        assert_eq!(result.0, 1, "exactly one render for the one file read");
        assert_eq!(result.1, b"file-one!");
        loop_handle.abort();
    }
}
