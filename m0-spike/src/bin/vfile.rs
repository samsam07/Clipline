//! Clipline M0a spike — Finding C prototype: lazy VIRTUAL files on Windows via
//! the shell IDataObject model (CFSTR_FILEDESCRIPTORW + CFSTR_FILECONTENTS),
//! instead of CF_HDROP.
//!
//! THROWAWAY / not committed. Question under test: does a clipboard monitor
//! (fdm.exe et al.) force us to produce file *contents* at copy time — or only
//! the cheap *descriptor* (names/sizes), leaving contents lazy until a real
//! paste? CF_HDROP forces materialization (Finding C); this checks whether the
//! RDP-style virtual-file model keeps contents lazy.
//!
//! We advertise an IDataObject with two formats:
//!   * FileGroupDescriptorW — metadata only, cheap, safe to force-render.
//!   * FileContents (per lindex) — the bytes; served LAZILY through the same
//!     sync↔async bridge (block the STA thread on an async fetch + timeout).
//! Every IDataObject::GetData call is logged (format + lindex + requester), so
//! we can SEE who asks for what and when.
//!
//! Modes: (default) serve — OleSetClipboard + pump; you paste into an Explorer
//!        folder and we observe FileContents pulled lazily.
//!        --selftest — a second thread OleGetClipboards and pulls descriptor then
//!        contents, proving the lazy serve end-to-end in-process.
//! Flags: --delay-ms N (fetch latency, default 1500), --timeout-ms N (default 5000).

use std::mem::ManuallyDrop;
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use tokio::runtime::Handle;
use tokio::time::{sleep, timeout};

use windows::Win32::Foundation::{CloseHandle, HGLOBAL};
use windows::Win32::System::Com::{
    DATADIR_GET, DVASPECT_CONTENT, FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl,
    IEnumFORMATETC, IEnumSTATDATA, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
};
use windows::Win32::System::DataExchange::{GetOpenClipboardWindow, RegisterClipboardFormatW};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::{
    OleFlushClipboard, OleInitialize, OleSetClipboard, OleUninitialize, ReleaseStgMedium,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::Shell::{
    CFSTR_FILECONTENTS, CFSTR_FILEDESCRIPTORW, FILEDESCRIPTORW, FILEGROUPDESCRIPTORW,
    SHCreateStdEnumFmtEtc,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, GetWindowThreadProcessId, MSG, TranslateMessage,
};
use windows::core::{Error, HRESULT, PWSTR, Ref, Result, implement};

const FD_FILESIZE: u32 = 0x40; // FILEDESCRIPTOR.dwFlags: nFileSize* are valid
const S_OK: HRESULT = HRESULT(0);
const E_NOTIMPL: HRESULT = HRESULT(0x80004001u32 as i32);
const E_UNEXPECTED: HRESULT = HRESULT(0x8000FFFFu32 as i32);
const DV_E_FORMATETC: HRESULT = HRESULT(0x80040064u32 as i32);

static STARTED: OnceLock<Instant> = OnceLock::new();

fn log(msg: &str) {
    let t = STARTED.get().map(|s| s.elapsed().as_millis()).unwrap_or(0);
    let tid = format!("{:?}", std::thread::current().id());
    eprintln!("[{t:>6} ms {tid:>9}] {msg}");
}

/// Name the process holding the clipboard open during a GetData (best-effort;
/// the OLE clipboard may not always expose an opener window).
unsafe fn clipboard_requester() -> String {
    let hwnd = match unsafe { GetOpenClipboardWindow() } {
        Ok(h) if !h.0.is_null() => h,
        _ => return "<none/self>".to_string(),
    };
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return "<unknown>".to_string();
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

// ── The virtual-file data object ─────────────────────────────────────────────

#[implement(IDataObject)]
struct VFile {
    handle: Handle,
    delay: Duration,
    timeout: Duration,
    file_name: Vec<u16>, // NUL-terminated wide file name for the descriptor
    display: String,
    bytes: Vec<u8>, // the "origin" bytes; served only when FileContents is asked
    fd_fmt: u16,     // registered CFSTR_FILEDESCRIPTORW
    fc_fmt: u16,     // registered CFSTR_FILECONTENTS
}

impl VFile {
    fn fmt_name(&self, cf: u16) -> String {
        if cf == self.fd_fmt {
            "FileGroupDescriptorW".to_string()
        } else if cf == self.fc_fmt {
            "FileContents".to_string()
        } else {
            format!("cf#{cf}")
        }
    }

    /// Cheap: build the FILEGROUPDESCRIPTORW (metadata only, NO fetch).
    unsafe fn descriptor_medium(&self) -> Result<STGMEDIUM> {
        // FILEDESCRIPTORW is packed(1) — build the name array as a local, then
        // construct the struct by value (no references into packed fields).
        let mut cfn = [0u16; 260];
        let n = self.file_name.len().min(cfn.len());
        cfn[..n].copy_from_slice(&self.file_name[..n]);
        let fd = FILEDESCRIPTORW {
            dwFlags: FD_FILESIZE,
            nFileSizeLow: self.bytes.len() as u32,
            nFileSizeHigh: 0,
            cFileName: cfn,
            ..Default::default()
        };
        let group = FILEGROUPDESCRIPTORW { cItems: 1, fgd: [fd] };
        let size = std::mem::size_of::<FILEGROUPDESCRIPTORW>();
        unsafe { hglobal_medium_from(&group as *const _ as *const u8, size) }
    }

    /// The lazy part: fetch (delay) then serve the bytes — bounded by timeout.
    unsafe fn contents_medium(&self, lindex: i32) -> Result<STGMEDIUM> {
        log(&format!(
            "  → FileContents(lindex={lindex}) requested → LAZY fetch (delay {} ms, timeout {} ms)…",
            self.delay.as_millis(),
            self.timeout.as_millis()
        ));
        let fetched = self.handle.block_on(async {
            timeout(self.timeout, async {
                sleep(self.delay).await;
                self.bytes.clone()
            })
            .await
        });
        match fetched {
            Ok(b) => {
                log(&format!("  → served FileContents ({} bytes) lazily", b.len()));
                unsafe { hglobal_medium_from(b.as_ptr(), b.len()) }
            }
            Err(_) => {
                log("  → FileContents fetch TIMEOUT → graceful fail");
                Err(Error::from_hresult(E_UNEXPECTED))
            }
        }
    }
}

/// Copy `len` bytes into a fresh GMEM_MOVEABLE HGLOBAL and wrap it in a
/// TYMED_HGLOBAL STGMEDIUM (caller owns it via ReleaseStgMedium).
unsafe fn hglobal_medium_from(src: *const u8, len: usize) -> Result<STGMEDIUM> {
    let h = unsafe { GlobalAlloc(GMEM_MOVEABLE, len)? };
    let p = unsafe { GlobalLock(h) } as *mut u8;
    unsafe { std::ptr::copy_nonoverlapping(src, p, len) };
    let _ = unsafe { GlobalUnlock(h) };
    Ok(STGMEDIUM {
        tymed: TYMED_HGLOBAL.0 as u32,
        u: STGMEDIUM_0 { hGlobal: h },
        pUnkForRelease: ManuallyDrop::new(None),
    })
}

impl IDataObject_Impl for VFile_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> Result<STGMEDIUM> {
        let fe = unsafe { &*pformatetcin };
        let name = self.fmt_name(fe.cfFormat);
        let who = unsafe { clipboard_requester() };
        log(&format!(
            "IDataObject::GetData({name}, lindex={}) requested by [{who}]",
            fe.lindex
        ));
        if fe.cfFormat == self.fd_fmt {
            log("  → serving FileGroupDescriptorW (metadata only, NO fetch)");
            unsafe { self.descriptor_medium() }
        } else if fe.cfFormat == self.fc_fmt {
            unsafe { self.contents_medium(fe.lindex) }
        } else {
            Err(Error::from_hresult(DV_E_FORMATETC))
        }
    }

    fn GetDataHere(&self, _f: *const FORMATETC, _m: *mut STGMEDIUM) -> Result<()> {
        Err(Error::from_hresult(E_NOTIMPL))
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        let fe = unsafe { &*pformatetc };
        if fe.cfFormat == self.fd_fmt || fe.cfFormat == self.fc_fmt {
            S_OK
        } else {
            DV_E_FORMATETC
        }
    }

    fn GetCanonicalFormatEtc(&self, _i: *const FORMATETC, _o: *mut FORMATETC) -> HRESULT {
        E_NOTIMPL
    }

    fn SetData(&self, _f: *const FORMATETC, _m: *const STGMEDIUM, _r: windows::core::BOOL) -> Result<()> {
        Err(Error::from_hresult(E_NOTIMPL))
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> Result<IEnumFORMATETC> {
        if dwdirection != DATADIR_GET.0 as u32 {
            return Err(Error::from_hresult(E_NOTIMPL));
        }
        let mk = |cf: u16, lindex: i32| FORMATETC {
            cfFormat: cf,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0,
            lindex,
            tymed: TYMED_HGLOBAL.0 as u32,
        };
        let fmts = [mk(self.fd_fmt, -1), mk(self.fc_fmt, -1)];
        unsafe { SHCreateStdEnumFmtEtc(&fmts) }
    }

    fn DAdvise(&self, _f: *const FORMATETC, _a: u32, _s: Ref<IAdviseSink>) -> Result<u32> {
        Err(Error::from_hresult(E_NOTIMPL))
    }
    fn DUnadvise(&self, _c: u32) -> Result<()> {
        Err(Error::from_hresult(E_NOTIMPL))
    }
    fn EnumDAdvise(&self) -> Result<IEnumSTATDATA> {
        Err(Error::from_hresult(E_NOTIMPL))
    }
}

// ── Pump thread: owns the OLE clipboard on an STA ────────────────────────────

fn ole_thread(
    handle: Handle,
    delay: Duration,
    timeout: Duration,
    ready: mpsc::Sender<()>,
) -> Result<()> {
    unsafe {
        OleInitialize(None)?; // STA
        let fd_fmt = RegisterClipboardFormatW(CFSTR_FILEDESCRIPTORW) as u16;
        let fc_fmt = RegisterClipboardFormatW(CFSTR_FILECONTENTS) as u16;

        let name: Vec<u16> = "clipline-m0-lazy.bin\0".encode_utf16().collect();
        let vfile = VFile {
            handle,
            delay,
            timeout,
            file_name: name,
            display: "clipline-m0-lazy.bin".to_string(),
            bytes: b"Clipline M0: these FileContents bytes were fetched LAZILY at paste time.\n"
                .to_vec(),
            fd_fmt,
            fc_fmt,
        };
        let obj: IDataObject = vfile.into();
        OleSetClipboard(&obj)?;
        log(&format!(
            "OleSetClipboard: virtual file advertised (FileGroupDescriptorW #{fd_fmt} + FileContents #{fc_fmt}), NO bytes rendered yet"
        ));
        let _ = ready.send(());

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = OleFlushClipboard();
        OleUninitialize();
        Ok(())
    }
}

// ── Self-test: pull descriptor then contents from another thread ─────────────

unsafe fn selftest(fd_fmt: u16, fc_fmt: u16) {
    use windows::Win32::System::Ole::OleGetClipboard;
    unsafe { OleInitialize(None).ok() };
    let obj = match unsafe { OleGetClipboard() } {
        Ok(o) => o,
        Err(e) => {
            log(&format!("[selftest] OleGetClipboard failed: {e}"));
            return;
        }
    };
    let fe = |cf: u16, lindex: i32| FORMATETC {
        cfFormat: cf,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex,
        tymed: TYMED_HGLOBAL.0 as u32,
    };

    log("[selftest] pulling FileGroupDescriptorW (metadata)…");
    match unsafe { obj.GetData(&fe(fd_fmt, -1)) } {
        Ok(mut m) => {
            log("[selftest] got descriptor ✓ (no fetch expected above)");
            unsafe { ReleaseStgMedium(&mut m) };
        }
        Err(e) => log(&format!("[selftest] descriptor failed: {e}")),
    }

    log("[selftest] pulling FileContents(lindex=0) — should trigger the lazy fetch…");
    match unsafe { obj.GetData(&fe(fc_fmt, 0)) } {
        Ok(mut m) => {
            let h = HGLOBAL(unsafe { m.u.hGlobal.0 });
            let len = unsafe { GlobalSize(h) };
            log(&format!("[selftest] got FileContents ✓ ({len} bytes)"));
            unsafe { ReleaseStgMedium(&mut m) };
        }
        Err(e) => log(&format!("[selftest] FileContents failed: {e}")),
    }
}

fn arg_ms(args: &[String], flag: &str, default: u64) -> u64 {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn main() {
    STARTED.set(Instant::now()).ok();
    let args: Vec<String> = std::env::args().collect();
    let is_selftest = args.iter().any(|a| a == "--selftest");
    let delay = Duration::from_millis(arg_ms(&args, "--delay-ms", 1500));
    let render_timeout = Duration::from_millis(arg_ms(&args, "--timeout-ms", 5000));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");
    let handle = rt.handle().clone();

    log(&format!(
        "vfile prototype — mode={}, delay={} ms, timeout={} ms",
        if is_selftest { "selftest" } else { "serve" },
        delay.as_millis(),
        render_timeout.as_millis()
    ));

    let (tx, rx) = mpsc::channel::<()>();
    let h2 = handle.clone();
    let pump = std::thread::spawn(move || {
        if let Err(e) = ole_thread(h2, delay, render_timeout, tx) {
            log(&format!("ole thread error: {e}"));
        }
    });
    rx.recv().expect("ole thread ready");

    if is_selftest {
        std::thread::sleep(Duration::from_millis(400)); // let any monitor act first
        let fd_fmt = unsafe { RegisterClipboardFormatW(CFSTR_FILEDESCRIPTORW) as u16 };
        let fc_fmt = unsafe { RegisterClipboardFormatW(CFSTR_FILECONTENTS) as u16 };
        unsafe { selftest(fd_fmt, fc_fmt) };
        std::thread::sleep(Duration::from_millis(300));
        log("[selftest] done — exiting.");
        std::process::exit(0);
    } else {
        println!();
        println!("virtual file on the clipboard as an IDataObject (FileDescriptor + FileContents).");
        println!("  • paste into an Explorer folder → should pull FileContents LAZILY (watch the log)");
        println!("  • note whether any clipboard monitor pulls FileContents at copy time (it should NOT)");
        println!("Ctrl+C to quit.");
        let _ = pump.join();
    }
}
