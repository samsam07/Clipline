//! Per-OS clipboard adapters, injected into `clipline-core` by this binary
//! (CONVENTIONS.md — "the `clipline` binary injects the Win32 / Wayland / X11
//! adapters, cfg-gated, one per OS"). Linux is M-Linux: Wayland (ML.1) with the FUSE
//! inbound-file mount (ML.2, `fuse`); X11 (ML.3) follows.

#[cfg(windows)]
pub mod codec;
#[cfg(any(windows, target_os = "linux"))]
pub mod files;
#[cfg(target_os = "linux")]
pub mod fuse;
#[cfg(target_os = "linux")]
pub mod wayland;
#[cfg(windows)]
pub mod windows;

#[cfg(target_os = "linux")]
pub use wayland::WaylandClipboardAdapter;
#[cfg(windows)]
pub use windows::WinClipboardAdapter;

#[cfg(any(windows, target_os = "linux"))]
use std::{sync::Arc, time::Duration};

#[cfg(any(windows, target_os = "linux"))]
use clipline_core::{AdapterError, ClipboardAdapter};

/// One fetch window: how much a destination file reader pulls from the origin per network
/// round trip, independent of the pasting app's (usually smaller) read size. 4 MiB amortizes
/// the round trip over ~16 wire chunks instead of paying it per read — the M3 manual gate
/// found paste throughput was round-trip-bound at the read size (~256 KiB), ~2.7 MB/s.
/// Shared by the Windows `LazyFileStream` and the Linux FUSE reader (ML.2), so both ends
/// fetch the same way.
#[cfg(any(windows, target_os = "linux"))]
pub(crate) const READAHEAD_WINDOW: u64 = 4 * 1024 * 1024;

/// Build this OS's adapter, ready to inject. Must run inside the tokio runtime (both
/// adapters spawn their `local_reads` server on it).
#[cfg(windows)]
pub fn new_adapter(render_timeout: Duration) -> Result<Arc<dyn ClipboardAdapter>, AdapterError> {
    Ok(Arc::new(WinClipboardAdapter::new(render_timeout)?))
}

/// Linux: Wayland data-control. No session, or a compositor without data-control (e.g.
/// GNOME/Mutter), is an error until the X11 owner lands (M-Linux ML.3) as the fallback.
#[cfg(target_os = "linux")]
pub fn new_adapter(render_timeout: Duration) -> Result<Arc<dyn ClipboardAdapter>, AdapterError> {
    WaylandClipboardAdapter::new(render_timeout)
        .map(|a| Arc::new(a) as Arc<dyn ClipboardAdapter>)
        .map_err(|e| match e {
            AdapterError::Os(m) => AdapterError::Os(format!(
                "{m} (Wayland data-control required; the X11 fallback is M-Linux ML.3)"
            )),
        })
}
