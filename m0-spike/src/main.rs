//! Clipline M0a spike — Windows lazy delayed-render sync↔async bridge.
//!
//! THROWAWAY / not committed. This proves ONE thing: the `on_render` inversion
//! from ARCHITECTURE.md — the OS asks (synchronously, on the clipboard-owner's
//! message-pump thread) for the bytes of a format we advertised but never
//! populated; we block that thread while an async (tokio) task "fetches" the
//! bytes (a `sleep` stands in for the future network), then hand them back —
//! with a timeout that yields a graceful paste-fail instead of a hung app.
//!
//! Covers BOTH formats the docs call out as risky (per M0 decision "both"):
//!   * CF_UNICODETEXT — text
//!   * CF_HDROP        — files by reference; the fetch materializes a local
//!                       staging file and advertises a *local* path, mirroring
//!                       the real materialize-then-advertise flow (SPEC §9).
//!
//! Modes:
//!   (default)     serve: claim the clipboard and pump forever; you paste in a
//!                 real app (Notepad / Explorer). Ctrl+C to quit.
//!   --selftest    drive the bridge automatically: a second thread performs a
//!                 real GetClipboardData (which forces WM_RENDERFORMAT), for
//!                 text then files, verifies the bytes, and exits.
//!
//! Flags: --delay-ms N (simulated fetch latency, default 1500)
//!        --timeout-ms N (render timeout, default 3000)
//! Force the timeout path with e.g. `--selftest --delay-ms 4000 --timeout-ms 800`.

use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tokio::runtime::Handle;
use tokio::time::{sleep, timeout};

use windows::Win32::Foundation::{
    CloseHandle, HANDLE, HGLOBAL, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardOwner, GetOpenClipboardWindow,
    OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{CF_HDROP, CF_UNICODETEXT};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Shell::DROPFILES;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, GetWindowThreadProcessId,
    HWND_MESSAGE, MSG, PostQuitMessage, RegisterClassW, TranslateMessage, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_DESTROY, WM_RENDERALLFORMATS, WM_RENDERFORMAT, WNDCLASSW,
};
use windows::core::{PWSTR, w};

// Clipboard formats are u32 in the Win32 clipboard API.
const CF_TEXT: u32 = CF_UNICODETEXT.0 as u32;
const CF_FILES: u32 = CF_HDROP.0 as u32;

/// Shared, WndProc-reachable state. Single window, single process → a global is fine.
struct Spike {
    handle: Handle,
    fetch_delay: Duration,
    render_timeout: Duration,
    started: Instant,
    claim_text: bool,
    claim_files: bool,
    exclude_monitors: bool,
    text_bytes: usize,
}
static SPIKE: OnceLock<Spike> = OnceLock::new();

fn spike() -> &'static Spike {
    SPIKE.get().expect("SPIKE initialized in main before any render")
}

/// Timestamped (ms since start) + thread-tagged stderr log. Never logs clipboard
/// *contents* beyond a short placeholder preview — mirrors CONVENTIONS.md rule.
fn log(msg: &str) {
    let t = SPIKE.get().map(|s| s.started.elapsed().as_millis()).unwrap_or(0);
    let tid = format!("{:?}", std::thread::current().id());
    eprintln!("[{t:>6} ms {tid:>9}] {msg}");
}

fn fmt_name(format: u32) -> &'static str {
    match format {
        CF_TEXT => "CF_UNICODETEXT",
        CF_FILES => "CF_HDROP",
        _ => "OTHER",
    }
}

/// The "network fetch", async. In the real system this rides the bulk plane to
/// the origin; here it's a delay + a placeholder payload.
enum Rendered {
    Text(String),
    Files(Vec<PathBuf>),
}

async fn fetch(format: u32, delay: Duration) -> Rendered {
    sleep(delay).await;
    match format {
        CF_TEXT => {
            let want = spike().text_bytes;
            let base = format!(
                "Hello from Clipline M0 spike — rendered LAZILY at paste time (delay {} ms). ",
                delay.as_millis()
            );
            let mut text = base.clone();
            while text.len() < want {
                text.push_str(&base);
            }
            text.truncate(want.max(base.len()));
            Rendered::Text(text)
        }
        _ => {
            // Files are by-reference: materialize a destination-LOCAL copy in a
            // staging dir, then advertise that local path (SPEC §9 / M1 preview).
            let dir = std::env::temp_dir().join("clipline-m0-spike");
            let _ = std::fs::create_dir_all(&dir);
            let path = dir.join(format!("lazy-payload-{}.txt", std::process::id()));
            let _ = std::fs::write(
                &path,
                b"Clipline M0 spike: this file's bytes were materialized lazily on paste.\n",
            );
            Rendered::Files(vec![path])
        }
    }
}

// ── The bridge ───────────────────────────────────────────────────────────────

/// Called from WM_RENDERFORMAT on the clipboard-owner (pump) thread. This is the
/// single sync↔async crossing: we BLOCK this platform-affine thread on the tokio
/// runtime while the async fetch runs, bounded by a timeout. On timeout we return
/// without SetClipboardData → the paste yields nothing, but the app is released
/// promptly (graceful paste-fail, CONVENTIONS.md error-handling rule).
/// During WM_RENDERFORMAT the *requester* holds the clipboard open. Name it, so
/// we can see who forces our delayed render (a real paste vs. cbdhsvc/cloud/etc.).
unsafe fn clipboard_requester() -> String {
    let hwnd = match unsafe { GetOpenClipboardWindow() } {
        Ok(h) if !h.0.is_null() => h,
        _ => return "<none/self>".to_string(),
    };
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return "<unknown pid>".to_string();
    }
    let hproc = match unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) } {
        Ok(h) => h,
        Err(_) => return format!("pid {pid}"),
    };
    let mut buf = [0u16; 260];
    let mut size = buf.len() as u32;
    let name = if unsafe {
        QueryFullProcessImageNameW(hproc, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut size)
    }
    .is_ok()
    {
        String::from_utf16_lossy(&buf[..size as usize])
    } else {
        format!("pid {pid}")
    };
    let _ = unsafe { CloseHandle(hproc) };
    name
}

unsafe fn handle_render(format: u32) {
    let s = spike();
    let name = fmt_name(format);
    let requester = unsafe { clipboard_requester() };
    log(&format!(
        "WM_RENDERFORMAT({name}) requested by [{requester}]: blocking pump thread, awaiting async fetch (timeout {} ms)…",
        s.render_timeout.as_millis()
    ));

    let outcome = s
        .handle
        .block_on(async { timeout(s.render_timeout, fetch(format, s.fetch_delay)).await });

    match outcome {
        Ok(Rendered::Text(text)) => match unsafe { make_text_handle(&text) } {
            Ok(h) => match unsafe { SetClipboardData(CF_TEXT, Some(h)) } {
                Ok(_) => log(&format!("served {name} lazily → SetClipboardData OK")),
                Err(e) => log(&format!("SetClipboardData({name}) failed: {e}")),
            },
            Err(e) => log(&format!("make_text_handle failed: {e}")),
        },
        Ok(Rendered::Files(paths)) => match unsafe { make_hdrop_handle(&paths) } {
            Ok(h) => match unsafe { SetClipboardData(CF_FILES, Some(h)) } {
                Ok(_) => log(&format!(
                    "served {name} lazily ({} file[s]) → SetClipboardData OK",
                    paths.len()
                )),
                Err(e) => log(&format!("SetClipboardData({name}) failed: {e}")),
            },
            Err(e) => log(&format!("make_hdrop_handle failed: {e}")),
        },
        Err(_elapsed) => log(&format!(
            "TIMEOUT rendering {name} after {} ms → returning WITHOUT SetClipboardData (graceful paste-fail)",
            s.render_timeout.as_millis()
        )),
    }
}

/// Build a GMEM_MOVEABLE handle holding a NUL-terminated UTF-16 string (CF_UNICODETEXT).
unsafe fn make_text_handle(text: &str) -> windows::core::Result<HANDLE> {
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0);
    let bytes = wide.len() * 2;
    let hglobal = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes)? };
    let ptr = unsafe { GlobalLock(hglobal) } as *mut u8;
    unsafe { std::ptr::copy_nonoverlapping(wide.as_ptr() as *const u8, ptr, bytes) };
    let _ = unsafe { GlobalUnlock(hglobal) };
    Ok(HANDLE(hglobal.0))
}

/// Build a CF_HDROP handle: a DROPFILES header followed by a double-NUL-terminated
/// list of wide file paths.
unsafe fn make_hdrop_handle(paths: &[PathBuf]) -> windows::core::Result<HANDLE> {
    let mut list: Vec<u16> = Vec::new();
    for p in paths {
        list.extend(p.as_os_str().encode_wide());
        list.push(0);
    }
    list.push(0); // extra NUL: terminates the list

    let header = std::mem::size_of::<DROPFILES>();
    let list_bytes = list.len() * 2;
    let total = header + list_bytes;

    let hglobal = unsafe { GlobalAlloc(GMEM_MOVEABLE, total)? };
    let base = unsafe { GlobalLock(hglobal) } as *mut u8;

    let df = base as *mut DROPFILES;
    unsafe {
        (*df).pFiles = header as u32; // offset to the path list
        (*df).pt = POINT { x: 0, y: 0 };
        (*df).fNC = false.into();
        (*df).fWide = true.into(); // wide (UTF-16) paths
        std::ptr::copy_nonoverlapping(list.as_ptr() as *const u8, base.add(header), list_bytes);
    }
    let _ = unsafe { GlobalUnlock(hglobal) };
    Ok(HANDLE(hglobal.0))
}

// ── Window / clipboard ownership ─────────────────────────────────────────────

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_RENDERFORMAT => {
            unsafe { handle_render(wparam.0 as u32) };
            LRESULT(0)
        }
        WM_RENDERALLFORMATS => {
            // Sent when we're losing ownership / shutting down. We don't need to
            // eagerly render everything for the spike; just acknowledge.
            log("WM_RENDERALLFORMATS (ignored for spike)");
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// Claim the clipboard advertising delayed-render CF_UNICODETEXT + CF_HDROP with
/// NO bytes. NOTE: SetClipboardData(fmt, None) returns the NULL handle, which
/// windows-rs surfaces as Err even though delayed-render registration succeeded —
/// so we deliberately ignore that Err here.
unsafe fn claim_clipboard(hwnd: HWND) -> windows::core::Result<()> {
    let s = spike();
    unsafe { OpenClipboard(Some(hwnd))? };
    unsafe { EmptyClipboard()? };
    if s.exclude_monitors {
        unsafe { set_monitor_exclusion() };
    }
    if s.claim_text {
        let _ = unsafe { SetClipboardData(CF_TEXT, None) };
    }
    if s.claim_files {
        let _ = unsafe { SetClipboardData(CF_FILES, None) };
    }
    unsafe { CloseClipboard()? };
    Ok(())
}

/// Finding B mitigation under test: mark the clipboard so OS monitors — the
/// clipboard-history/cloud service (cbdhsvc) and clipboard managers — skip it and
/// do NOT force an eager GetClipboardData on our delayed-render promise. These are
/// the same well-known formats password managers use to stay out of history; they
/// also dovetail with the Strict/RespectHints safety layer (SPEC §7). We set REAL
/// (tiny, non-NULL) data so the marker itself isn't a delayed-render format.
unsafe fn set_monitor_exclusion() {
    // A GMEM_MOVEABLE global holding a 4-byte 0 (DWORD 0). Serves both the
    // presence-only Exclude marker and the DWORD-valued history/cloud markers.
    unsafe fn zero_dword() -> windows::core::Result<HANDLE> {
        let h = unsafe { GlobalAlloc(GMEM_MOVEABLE, 4)? };
        let p = unsafe { GlobalLock(h) } as *mut u32;
        unsafe { *p = 0 };
        let _ = unsafe { GlobalUnlock(h) };
        Ok(HANDLE(h.0))
    }
    let markers = [
        w!("ExcludeClipboardContentFromMonitorProcessing"), // broadest: all monitors skip it
        w!("CanIncludeInClipboardHistory"),                 // DWORD 0 = keep out of history
        w!("CanUploadToCloudClipboard"),                    // DWORD 0 = keep out of cloud sync
    ];
    for name in markers {
        let fmt = unsafe { RegisterClipboardFormatW(name) };
        if fmt == 0 {
            continue;
        }
        if let Ok(h) = unsafe { zero_dword() } {
            let _ = unsafe { SetClipboardData(fmt, Some(h)) };
        }
    }
    log("set monitor-exclusion markers (Exclude…Processing + history/cloud=0)");
}

/// Runs on the platform-affine clipboard thread: create a message-only window,
/// claim the clipboard, signal ready, then pump messages (serving renders).
fn clipboard_thread(ready: mpsc::Sender<()>) -> windows::core::Result<()> {
    unsafe {
        let hmodule = GetModuleHandleW(None)?;
        let hinstance = HINSTANCE(hmodule.0);
        let class = w!("CliplineM0SpikeWindow");

        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinstance,
            lpszClassName: class,
            ..Default::default()
        };
        if RegisterClassW(&wc) == 0 {
            return Err(windows::core::Error::from_thread());
        }

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("clipline-m0-spike"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE), // message-only window
            None,
            Some(hinstance),
            None,
        )?;

        claim_clipboard(hwnd)?;
        let s = spike();
        let owner = GetClipboardOwner().unwrap_or(HWND(std::ptr::null_mut()));
        log(&format!(
            "clipboard claimed: delayed-render [{}{}], no bytes set (owner==us: {})",
            if s.claim_text { "CF_UNICODETEXT " } else { "" },
            if s.claim_files { "CF_HDROP" } else { "" },
            owner == hwnd
        ));
        let _ = ready.send(());

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        Ok(())
    }
}

// ── Self-test: simulate a paste from a second thread ─────────────────────────

/// Read back CF_UNICODETEXT and log a short preview + length (never the full body).
unsafe fn verify_text(h: HANDLE) {
    let ptr = unsafe { GlobalLock(HGLOBAL(h.0)) } as *const u16;
    if ptr.is_null() {
        log("[selftest] text handle locked to null");
        return;
    }
    let mut len = 0usize;
    while unsafe { *ptr.add(len) } != 0 {
        len += 1;
    }
    let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
    let text = String::from_utf16_lossy(slice);
    let preview: String = text.chars().take(32).collect();
    let _ = unsafe { GlobalUnlock(HGLOBAL(h.0)) };
    log(&format!(
        "[selftest] got CF_UNICODETEXT: {len} chars, preview=\"{preview}…\" ✓"
    ));
}

/// Read back CF_HDROP, extract the advertised path(s), confirm the file exists.
unsafe fn verify_files(h: HANDLE) {
    let base = unsafe { GlobalLock(HGLOBAL(h.0)) } as *const u8;
    if base.is_null() {
        log("[selftest] hdrop handle locked to null");
        return;
    }
    let df = base as *const DROPFILES;
    let offset = unsafe { (*df).pFiles } as usize;
    let mut p = unsafe { base.add(offset) } as *const u16;
    let mut paths = Vec::new();
    loop {
        let mut len = 0usize;
        while unsafe { *p.add(len) } != 0 {
            len += 1;
        }
        if len == 0 {
            break; // double-NUL terminator
        }
        let slice = unsafe { std::slice::from_raw_parts(p, len) };
        paths.push(String::from_utf16_lossy(slice));
        p = unsafe { p.add(len + 1) };
    }
    let _ = unsafe { GlobalUnlock(HGLOBAL(h.0)) };
    for path in &paths {
        let exists = std::path::Path::new(path).exists();
        log(&format!("[selftest] got CF_HDROP path: {path} (exists: {exists}) ✓"));
    }
}

/// OpenClipboard races other clipboard users and can transiently fail with
/// ERROR_ACCESS_DENIED; real code retries. (Not the bridge under test — just
/// making the self-test's stand-in "paste" robust.)
unsafe fn open_clipboard_retry(tries: u32) -> windows::core::Result<()> {
    let mut last = Ok(());
    for _ in 0..tries {
        match unsafe { OpenClipboard(None) } {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Err(e);
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }
    last
}

/// A real GetClipboardData call from a NON-owner thread. For a delayed-render
/// format this forces the system to send WM_RENDERFORMAT to our pump thread and
/// blocks here until it renders — exactly what a real paste does.
unsafe fn simulate_paste(format: u32) {
    let name = fmt_name(format);
    log(&format!("[selftest] simulating paste: GetClipboardData({name})…"));
    if let Err(e) = unsafe { open_clipboard_retry(240) } {
        log(&format!("[selftest] OpenClipboard failed after retries (~6s): {e}"));
        return;
    }
    match unsafe { GetClipboardData(format) } {
        Ok(h) if !h.0.is_null() => match format {
            CF_TEXT => unsafe { verify_text(h) },
            CF_FILES => unsafe { verify_files(h) },
            _ => {}
        },
        Ok(_) => log(&format!(
            "[selftest] GetClipboardData({name}) returned NULL → nothing pasted (expected on timeout)"
        )),
        Err(e) => log(&format!(
            "[selftest] GetClipboardData({name}) → {e} → nothing pasted (graceful)"
        )),
    }
    let _ = unsafe { CloseClipboard() };
}

// ── Entry ────────────────────────────────────────────────────────────────────

fn arg_ms(args: &[String], flag: &str, default: u64) -> u64 {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let selftest = args.iter().any(|a| a == "--selftest");
    let only_text = args.iter().any(|a| a == "--only-text");
    let only_files = args.iter().any(|a| a == "--only-files");
    let exclude_monitors = args.iter().any(|a| a == "--exclude-monitors");
    let claim_text = !only_files;
    let claim_files = !only_text;
    let fetch_delay = Duration::from_millis(arg_ms(&args, "--delay-ms", 1500));
    let render_timeout = Duration::from_millis(arg_ms(&args, "--timeout-ms", 3000));
    let text_bytes = arg_ms(&args, "--text-bytes", 80) as usize;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    SPIKE
        .set(Spike {
            handle: rt.handle().clone(),
            fetch_delay,
            render_timeout,
            started: Instant::now(),
            claim_text,
            claim_files,
            exclude_monitors,
            text_bytes,
        })
        .ok();

    log(&format!(
        "M0a spike start — mode={}, claim=[{}{}], exclude_monitors={}, text_bytes={}, fetch_delay={} ms, render_timeout={} ms, will_timeout={}",
        if selftest { "selftest" } else { "serve" },
        if claim_text { "text " } else { "" },
        if claim_files { "files" } else { "" },
        exclude_monitors,
        text_bytes,
        fetch_delay.as_millis(),
        render_timeout.as_millis(),
        fetch_delay >= render_timeout,
    ));

    let (tx, rx) = mpsc::channel::<()>();
    let pump = std::thread::spawn(move || {
        if let Err(e) = clipboard_thread(tx) {
            log(&format!("clipboard thread error: {e}"));
        }
    });

    rx.recv().expect("clipboard thread signals ready");

    if selftest {
        // Let any OS clipboard monitor (cbdhsvc history/cloud) take its initial
        // snapshot first, so its render is visible and doesn't collide with ours.
        std::thread::sleep(Duration::from_millis(400));
        // Drive the bridge automatically from this (non-pump) thread. Retry budget
        // must outlast an OS-monitor-held clipboard AND a full render.
        if claim_text {
            unsafe { simulate_paste(CF_TEXT) };
        }
        if claim_files {
            unsafe { simulate_paste(CF_FILES) };
        }
        // Let any in-flight render finish logging before we tear the process down.
        std::thread::sleep(Duration::from_millis(400));
        log("[selftest] done — exiting.");
        std::process::exit(0);
    } else {
        println!();
        println!("clipboard is CLAIMED with lazy promises for text + files.");
        println!("  • paste into Notepad/Kate  → pulls CF_UNICODETEXT lazily");
        println!("  • paste into Explorer      → pulls CF_HDROP (a staged file) lazily");
        println!("watch the timestamped log to see bytes appear only on paste.");
        println!("Ctrl+C to quit.");
        let _ = pump.join();
    }
}
