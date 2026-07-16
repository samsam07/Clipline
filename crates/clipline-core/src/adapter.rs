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
//! (in M3, via the network fetch). Each adapter then bridges to its OS *internally* —
//! Windows blocks its pump thread on the reply, Wayland writes the fd from a task —
//! while core's contract stays identical across both. This is exactly the
//! expressibility M0 was run to prove.

use tokio::sync::{mpsc, oneshot};

use crate::error::{AdapterError, LocalReadError, RenderError};
use crate::protocol::{CaptureId, LocalCopy, Mime, Offer, OriginId, Payload, Seq};
use crate::wire::{ByteRange, JobId};

/// What the OS asks for when it forces a render of the current head: one slice of one
/// format of a specific `{origin_id, seq}`. Keyed identically to the bulk-plane
/// [`crate::wire::FetchReq`], which core builds from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatReq {
    pub origin_id: OriginId,
    pub seq: Seq,
    pub format: Mime,
    /// For per-file `CFSTR_FILECONTENTS` renders (Windows `FORMATETC.lindex`): which
    /// file within the offer's file group is being read. `None` for non-file formats.
    pub file_idx: Option<u32>,
    /// Which slice the OS is reading; `None` means the whole format (M3).
    ///
    /// Whole-format is right for text and images — they are one blob and the OS wants all
    /// of it. For files it is the adapter's ranged reads that make locked decision #8's
    /// "only the bytes actually read" true, and that bound how long one OS call blocks
    /// (M0 Finding A budgets a *call*, not a transfer).
    pub range: Option<ByteRange>,
    /// Which **transfer job** this read belongs to (SPEC.md §4; locked decision #5).
    ///
    /// The adapter allocates it, because only the adapter knows where a job begins and
    /// ends: one `IStream` handed to a pasting app is one job, however many reads it makes
    /// of it. Core cannot infer that from the reads themselves — and it must not, or the
    /// origin's pin would be released between two reads of the same file (M3 ruling Q12).
    pub job: JobId,
}

/// The result core supplies for a [`RenderRequest`]: the produced bytes, or a
/// source-side failure. The adapter's own render **timeout** is separate — if the
/// adapter's per-platform deadline fires first it simply drops the reply receiver,
/// which the responder observes as a closed channel (D2).
pub type RenderResult = Result<Payload, RenderError>;

/// One forced render, emitted by the adapter to core. Core resolves `req` (the M3
/// fetch) and sends the outcome back through `reply`.
///
/// Dropping `reply` without sending == graceful paste-fail: the responder never
/// hangs, and the adapter, having timed out, has already released the OS call.
#[derive(Debug)]
pub struct RenderRequest {
    pub req: FormatReq,
    pub reply: oneshot::Sender<RenderResult>,
}

/// One read of a **local** capture — the origin side of a fetch (M3.2).
///
/// The mirror image of [`RenderRequest`]: that one is the OS asking *us* for bytes we
/// promised; this is core asking the *adapter* for bytes of a copy we originated, because
/// a peer is pasting it. The direction of the channel is flipped accordingly — core holds
/// the `Sender` (see [`ClipboardAdapter::local_reads`]) and the adapter serves.
///
/// Async through a `oneshot` for the same reason as `RenderRequest`: reading a file range
/// is real I/O and must not block a core task, and the trait stays object-safe.
#[derive(Debug)]
pub struct LocalRead {
    /// Which snapshot to read (core resolved `seq → CaptureId` via its pin store).
    pub capture: CaptureId,
    pub format: Mime,
    /// Which file of the capture's group; `None` for non-file formats.
    pub file_idx: Option<u32>,
    /// The slice being read; `None` means all of it. For files this is what makes locked
    /// decision #8's "only the bytes actually read" true.
    pub range: Option<ByteRange>,
    pub reply: oneshot::Sender<Result<Payload, LocalReadError>>,
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
    /// `Offer`, broadcast it) is M2 — M1 only locks the shape.
    fn watch(&self) -> mpsc::UnboundedReceiver<LocalCopy>;

    /// **THE inversion.** Stream of forced renders the OS raises against the current
    /// head. Yields the receiver once; core drives it (see `driver::run_render_loop`).
    /// Each [`RenderRequest`] carries its own reply channel — see the module docs for
    /// why this is deferred rather than a synchronous callback (Finding D).
    fn render_requests(&self) -> mpsc::UnboundedReceiver<RenderRequest>;

    /// Stream of finished transfer **jobs** — the adapter announcing that a `JobId` it put
    /// on a [`FormatReq`] will issue no further reads. Yields the receiver once; core
    /// drives it and tells the origin, which then drops the job's pin (SPEC.md §4).
    ///
    /// Core cannot infer this. A job ends when the pasting app lets go of what it was
    /// given — an `IStream` being released, say — which only the adapter observes, and
    /// which may be many reads after the first (M3 ruling Q12). Emitting a job id here is
    /// what turns the origin's pin from "released eventually, by an idle sweep" into
    /// "released now".
    ///
    /// A job that is never announced is not a correctness problem, only a wasteful one:
    /// the origin's sweep collects it. Announcing a job id twice is harmless.
    fn job_ends(&self) -> mpsc::UnboundedReceiver<JobId>;

    /// Set our local head as a lazy promise advertising `offer`'s formats, holding no
    /// bytes (locked decision #2; SPEC.md §1 "Promise"). Quick platform-thread marshal,
    /// no network I/O.
    fn set_promise(&self, offer: &Offer) -> Result<(), AdapterError>;

    /// Continuous mode, payload under the eager threshold: set the head with real bytes
    /// now, so the OS historian can record it (SPEC.md §3). Eager-threshold value is
    /// `[CRYSTALLIZE: head/eager milestone]` (M5); this method only carries the bytes.
    fn set_eager(&self, offer: &Offer, payload: Payload) -> Result<(), AdapterError>;

    /// **The origin-side inversion** (M3.2). A sender core keeps to ask the adapter for
    /// bytes of a copy *we* originated, when a peer fetches it. Cloneable; the adapter owns
    /// the receiver and serves reads on whatever thread it must (the Windows pump, a
    /// blocking pool, …).
    ///
    /// Note the shape is the reverse of [`Self::render_requests`]: there the adapter pushes
    /// and core answers; here core pushes and the adapter answers. The two directions are
    /// genuinely different flows — a paste *we* perform vs. a paste performed *against us*
    /// — and collapsing them into one channel would conflate them.
    fn local_reads(&self) -> mpsc::Sender<LocalRead>;

    /// Release a capture: no head and no in-flight job reference it any more, so its
    /// snapshot (and any file paths it holds) can be dropped.
    ///
    /// Core owns this decision — the pin lifecycle is core's (locked decision #6: a
    /// capture lives while it is the head **or** any accepted fetch still needs it, and a
    /// new copy never releases a pinned one). The adapter just forgets it. Releasing an
    /// unknown id is a no-op, not an error.
    fn release_capture(&self, capture: CaptureId);

    // NOTE (M1 decision — streaming, mstsc-style): there is no `materialize_files`. Files
    // are advertised by `set_promise` (carried in `Offer.files`) and their contents are
    // served on demand through `render_requests` above — keyed by `FormatReq.file_idx` and
    // a byte range — streaming origin→destination with no local staging. See locked
    // decision #8 (amended M1) and `protocol::FileEntry`.
}
