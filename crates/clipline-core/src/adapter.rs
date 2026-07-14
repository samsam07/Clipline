//! The `ClipboardAdapter` contract — the platform-adapter injection seam
//! (ARCHITECTURE.md "Platform boundary"; CONVENTIONS.md "Module / workspace layout").
//!
//! This trait is defined in `clipline-core`; **implementations live outside it and are
//! injected by the consumer** (the `clipline` binary injects Win32/Wayland/X11; a
//! future Android client injects a JNI-backed adapter). Core depends only on this
//! trait, never on a platform clipboard crate — that seam is what makes core reusable.
//!
//! # The render inversion is DEFERRED, not a sync callback (M0 Finding D)
//!
//! ARCHITECTURE.md sketched `on_render(&self, cb: impl Fn(FormatReq) -> RenderResult)`
//! and explicitly deferred the final shape to M1. It **cannot** be a synchronous
//! callback, because the two OSes impose *opposite* threading rules:
//!
//! * **Windows** (`WM_RENDERFORMAT`) *must block* the clipboard-owner thread until
//!   `SetClipboardData` (tolerated up to ~30 s — Finding A).
//! * **Wayland** (data-control `send`) *must not block* its dispatch thread — a
//!   blocking write broke every transfer with `Broken pipe` (Finding D).
//!
//! So the inversion is modelled as a **request stream** (D1): the adapter emits a
//! [`RenderRequest`] carrying a `oneshot` reply channel; core answers asynchronously
//! (in M4, via the network fetch). Each adapter then bridges to its OS *internally* —
//! Windows blocks its pump thread on the reply, Wayland writes the fd from a task —
//! while core's contract stays identical across both. This is exactly the
//! expressibility M0 was run to prove.

use tokio::sync::{mpsc, oneshot};

use crate::error::{AdapterError, RenderError};
use crate::protocol::{LocalCopy, Mime, Offer, OriginId, Payload, Seq};

/// What the OS asks for when it forces a render of the current head: one format of a
/// specific `{origin_id, seq}`. Keyed identically to the bulk-plane `FetchReq`
/// (ARCHITECTURE.md — fetches are keyed `{origin_id, seq, format}`, with an optional
/// per-file index).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatReq {
    pub origin_id: OriginId,
    pub seq: Seq,
    pub format: Mime,
    /// For per-file `CFSTR_FILECONTENTS` renders (Windows `FORMATETC.lindex`): which
    /// file within the offer's file group is being read. `None` for non-file formats.
    pub file_idx: Option<u32>,
}

/// The result core supplies for a [`RenderRequest`]: the produced bytes, or a
/// source-side failure. The adapter's own render **timeout** is separate — if the
/// adapter's per-platform deadline fires first it simply drops the reply receiver,
/// which the responder observes as a closed channel (D2).
pub type RenderResult = Result<Payload, RenderError>;

/// One forced render, emitted by the adapter to core. Core resolves `req` (the M4
/// fetch) and sends the outcome back through `reply`.
///
/// Dropping `reply` without sending == graceful paste-fail: the responder never
/// hangs, and the adapter, having timed out, has already released the OS call.
#[derive(Debug)]
pub struct RenderRequest {
    pub req: FormatReq,
    pub reply: oneshot::Sender<RenderResult>,
}

/// One implementation per platform (Windows / Linux-X11 / Linux-Wayland / Android-JNI),
/// injected into core by the consumer. `Send + Sync + 'static` so core can hold it as
/// `Arc<dyn ClipboardAdapter>` across its tasks.
///
/// **Object-safe on purpose:** every method is dyn-compatible (no generics, no
/// `async fn`) so the injection seam works through a trait object — no `async-trait`.
/// The command methods (`set_promise`/`set_eager`) are quick platform-thread marshals;
/// the async work (the render fetch) rides the `render_requests` stream instead (D3).
pub trait ClipboardAdapter: Send + Sync + 'static {
    /// Stream of locally-detected copies (ARCHITECTURE.md copy flow). Yields the
    /// receiver once; core takes it at startup. Core-side consumption (→ build an
    /// `Offer`, broadcast it) is M3 — M1 only locks the shape.
    fn watch(&self) -> mpsc::UnboundedReceiver<LocalCopy>;

    /// **THE inversion.** Stream of forced renders the OS raises against the current
    /// head. Yields the receiver once; core drives it (see `driver::run_render_loop`).
    /// Each [`RenderRequest`] carries its own reply channel — see the module docs for
    /// why this is deferred rather than a synchronous callback (Finding D).
    fn render_requests(&self) -> mpsc::UnboundedReceiver<RenderRequest>;

    /// Set our local head as a lazy promise advertising `offer`'s formats, holding no
    /// bytes (locked decision #2; SPEC.md §1 "Promise"). Quick platform-thread marshal,
    /// no network I/O.
    fn set_promise(&self, offer: &Offer) -> Result<(), AdapterError>;

    /// Continuous mode, payload under the eager threshold: set the head with real bytes
    /// now, so the OS historian can record it (SPEC.md §3). Eager-threshold value is
    /// `[CRYSTALLIZE: head/eager milestone]` (M6); this method only carries the bytes.
    fn set_eager(&self, offer: &Offer, payload: Payload) -> Result<(), AdapterError>;

    // NOTE (M1 decision — streaming, mstsc-style): there is no `materialize_files`. Files
    // are advertised by `set_promise` (carried in `Offer.files`) and their contents are
    // served on demand through `render_requests` above — keyed by `FormatReq.file_idx`
    // (and, in M4, a byte range) — streaming origin→destination with no local staging.
    // See locked decision #8 (amended M1) and `protocol::FileEntry`.
}
