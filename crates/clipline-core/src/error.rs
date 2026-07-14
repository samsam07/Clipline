//! Typed library errors (CONVENTIONS.md — thiserror-style; the binary maps these to
//! user-facing messages). No `unwrap`/`expect` in library code.

use thiserror::Error;

/// A render (the `on_render` inversion) could not be satisfied. A paste that hits one
/// of these must fail **gracefully** — the adapter releases the OS render call cleanly
/// and never hangs the pasting app (CONVENTIONS.md; SPEC.md §5).
///
/// This is the *source-side* failure (the bytes couldn't be produced). The
/// adapter-owned render **timeout** is a separate concern (D2): when the adapter's
/// per-platform deadline elapses it drops the reply and performs the graceful
/// paste-fail itself — see `mock::RenderOutcome::TimedOut`.
#[derive(Debug, Error)]
pub enum RenderError {
    /// The origin of the requested `{origin_id, seq, format}` is gone / unreachable
    /// (SPEC.md §5 — the unavoidable origin-vanishes race). Real fetch lands in M4.
    #[error("origin unavailable for requested format")]
    Unavailable,

    /// Core's responder dropped the reply channel without answering (e.g. the render
    /// loop shut down). The adapter treats this as a graceful paste-fail.
    #[error("render responder dropped without replying")]
    ResponderDropped,
}

/// An adapter command (`set_promise` / `set_eager`) failed at the platform boundary.
#[derive(Debug, Error)]
pub enum AdapterError {
    /// A platform clipboard/OS call failed. The string is a metadata-only description
    /// (never clipboard contents — CONVENTIONS.md logging).
    #[error("platform clipboard error: {0}")]
    Os(String),
}
