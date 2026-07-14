//! clipline — the thin consumer shell (CLI + tray) around `clipline-core`. Its job is
//! to construct the per-OS clipboard adapter and inject it into core (ARCHITECTURE.md
//! "Platform boundary"; CONVENTIONS.md — "the binary is a thin shell").
//!
//! M1 Slice 1 stands up the workspace and the injection seam in core. Slice 2 adds the
//! Windows adapter ([`platform::WinClipboardAdapter`]); this shell constructs it and
//! runs the render-answering loop. The network fetch behind the loop lands in M3 — until
//! then there is no `RenderSource` to serve real bytes, so the shell just logs.

mod platform;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    #[cfg(windows)]
    demo_injection();

    tracing::info!("clipline shell up (M1)");
}

/// Prove the injection seam from the real binary: construct the Win32 adapter and hold
/// it as a `dyn ClipboardAdapter` (core only ever sees the trait object). Wiring it to
/// the render loop needs M3's fetch source, so we don't run the loop here yet.
#[cfg(windows)]
fn demo_injection() {
    use std::time::Duration;
    match platform::WinClipboardAdapter::new(Duration::from_secs(10)) {
        Ok(adapter) => {
            let _injected: Box<dyn clipline_core::ClipboardAdapter> = Box::new(adapter);
            tracing::info!("Windows clipboard adapter constructed and injectable as dyn");
        }
        Err(e) => tracing::error!(error = %e, "failed to construct Windows clipboard adapter"),
    }
}
