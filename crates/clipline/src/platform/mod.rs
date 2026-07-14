//! Per-OS clipboard adapters, injected into `clipline-core` by this binary
//! (CONVENTIONS.md — "the `clipline` binary injects the Win32 / Wayland / X11
//! adapters, cfg-gated, one per OS"). Linux (Wayland/X11) is deferred to M-Linux.

#[cfg(windows)]
pub mod codec;
#[cfg(windows)]
pub mod windows;

#[cfg(windows)]
pub use windows::WinClipboardAdapter;
