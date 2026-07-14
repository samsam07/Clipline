# ARCHITECTURE.md — Clipline

Coarse architecture. Component boundaries, the platform-adapter contract, the wire
shape, concurrency model, and the core data flows. Atomic detail is deferred via
`[CRYSTALLIZE]` tags to its owning milestone.

## Component map

```
                         ┌─────────────────────────────────────┐
                         │             CORE (tokio)             │
   clipboard thread      │  ┌────────────┐    ┌──────────────┐  │      mesh peers
  (platform-affine) ───▶ │  │ Head Mgr   │◀──▶│ Transfer Eng │  │ ◀──▶ (TCP+TLS,
        ▲   │            │  │ (1 slot,   │    │ (jobs, pins, │  │       1 listen
        │   │ render     │  │  mode)     │    │  throttle)   │  │        port)
        │   │ callback   │  └─────┬──────┘    └──────┬───────┘  │
   ┌────┴───▼────┐       │        │                  │          │
   │  Clipboard  │       │   ┌────▼─────┐     ┌──────▼───────┐  │
   │  Adapter    │       │   │ Policy   │     │ Mesh/Peer Mgr│  │
   │ (per-OS)    │       │   │ (safety, │     │ (offers,     │  │
   └─────────────┘       │   │ toggles, │     │  routing,    │  │
                         │   │ gate,    │     │  late-join,  │  │
   ┌─────────────┐       │   │ throttle)│     │  reconcile)  │  │
   │ Control/UI  │◀──────┤   └──────────┘     └──────────────┘  │
   │ (CLI, tray) │       └──────────────────────────────────────┘
   └─────────────┘
```

### Clipboard Adapter (the hard 40% — lock its contract first)
Per-OS. Watches the local clipboard, sets the local head (as a promise or eagerly),
and serves byte requests from the OS on demand (the `on_render` inversion) — including
file contents, which **stream** through on demand with no local staging. Three
implementations behind one trait:
`Windows`, `Linux-X11`, `Linux-Wayland`. **The trait must remain expressible for
Wayland's fd-serve model even while only Windows is implemented first** (see
`PLAN.md` M0) — otherwise Linux becomes a redesign, not an implementation.

**The trait is defined in `clipline-core`; the implementations live outside it and are
injected by the consumer.** Core depends only on the trait, never on a platform clipboard
crate. The `clipline` binary injects the Win32/Wayland/X11 adapters; a future Android
client (the Vox-style reuse goal) injects a JNI-backed adapter. This injection seam is
what makes core consumer-agnostic — see CONVENTIONS.md.

### Head Manager
Owns the **single overwritable head slot** and the **capture mode**
(`HeadCapture` | `ContinuousCapture`). All head mutations are serialized through this
one owning task — no shared-mutex contention on the head.

### Transfer Engine
Spawns a **detached job per paste**, manages **origin-side pins** (bytes held alive
for an accepted fetch until it completes), drains bulk transfers **serially**, and
applies the **token-bucket throttle** to the bulk writer only.

### Mesh / Peer Manager
Holds connections to explicitly-configured peers, broadcasts offers on copy, routes
fetch requests, answers late-joiner head queries, and runs **background head
reconciliation** when a peer drops.

### Policy
Holds the orthogonal knobs: **safety level**, the three **lifecycle toggles**
(Presence / Send / Receive), the **gate** (abstract signal input), and the
**throttling level**. Pure decision logic; emits no I/O itself.

### Control / UI
CLI front-end (`clipline up`, status, etc., Vox-style) and a system-tray surface for
toggles. Thin; drives Policy and reports state.

## Platform boundary — the adapter contract

This signature is what *travels*; lock its shape. Per-OS mechanics are
`[CRYSTALLIZE: platform milestone]`; empirical findings from the platform spikes
(what each OS actually does — forced eager renders, retry counts, format choices)
are recorded in `PLATFORM-NOTES.md` and feed these decisions. Illustrative (final
types pinned at milestone):

```rust
/// One implementation per platform. Must be expressible for Windows
/// (WM_RENDERFORMAT), X11 (selection-request), and Wayland (data-control send-fd).
trait ClipboardAdapter {
    /// Local copy detected. Carries available formats, sizes, and any
    /// OS sensitivity hint (for the safety layer). No bytes.
    fn watch(&self) -> Receiver<LocalCopy>;

    /// Set our local head as a lazy promise advertising `offer`'s formats.
    fn set_promise(&self, offer: &Offer) -> Result<()>;

    /// Continuous mode, payload under the eager threshold: set the head with
    /// real bytes now (so the OS historian can record it).
    fn set_eager(&self, offer: &Offer, payload: Payload) -> Result<()>;

    /// THE inversion. The OS asks for one format of the current promise; core
    /// fetches the bytes over the network (async) and supplies them, or times out
    /// into a graceful paste-fail.
    /// NOTE (M0 Finding D — see PLATFORM-NOTES.md): this must be **deferred/async**,
    /// NOT the synchronous `Fn(FormatReq) -> RenderResult` sketched here. The two OSes
    /// impose opposite threading rules — Windows *must* block its platform thread
    /// awaiting the bytes; Wayland *must not* block its dispatch thread (it hands off
    /// the fd and writes it from a task). Final shape pinned in M1.
    fn on_render(&self, cb: impl Fn(FormatReq) -> RenderResult);

    // NOTE (M1 decision — streaming, mstsc-style): files are NOT materialized to a
    // local staging copy, so there is no `materialize_files`. A file group is advertised
    // via `set_promise` (carried in `Offer.files` as `{ rel_path, size }` entries); each
    // file's contents are served on demand through the render inversion above, keyed by
    // `FormatReq.file_idx` (and, in M3, a byte range) — bytes stream origin→destination.
}
```

`on_render` is the single biggest risk in the whole design. Windows serves it via
`WM_RENDERFORMAT` on the clipboard-owner thread; Wayland via the data-control `send`
event (write to the provided fd); X11 via `SelectionRequest`. **All three block the
OS synchronously while we do async network I/O** — bridging that sync↔async gap with
a timeout is make-or-break and is proven in M0 before any networking exists.

`arboard` is **insufficient** (text+image only; no file/arbitrary-MIME model). Use
`wl-clipboard-rs` for Wayland (MIME-typed, fd-serve = lazy-capable) and the `windows`
crate for Win32 clipboard + delayed render. X11 via a raw selection-owner impl.

## Wire shape

Two connections per peer, **one listening port** (many accepted sockets behind one
listen socket — connections are cheap, listening ports are the scarce/firewalled
resource).

- **Control plane** (small, always live, **never throttled**):
  `Offer`, `HeadQuery` / `HeadReply` (late-join), `Presence` / heartbeat, `Abort`.
  Eager small payloads ride here *with* the offer.
- **Bulk plane** (serial, throttleable): `FetchReq { origin_id, seq, format, file_idx? }`
  → byte stream.

Message *kinds* are locked above. Field layout, framing, encoding, and error codes are
`[CRYSTALLIZE: protocol milestone]` (leaning: length-prefixed frames; a compact serde
codec for control messages; raw byte stream for bulk). Fetches are keyed
`{ origin_id, seq, format }`.

## State

- **Head slot:** `Option<Offer>` (single, overwritable) + `mode`.
- **Peer table:** per peer — endpoint, connection state, last-seen, their advertised head.
- **Active jobs:** detached transfers, each `{ origin_id, seq, format, target, progress }`.
- **Pins (origin side):** seqs whose bytes are held alive until their fetch(es) complete.
- **Ordering:** highest `seq` wins; tie → `origin_id`.

## Core data flows

**Copy (on node A):**
1. Adapter `watch` fires → `LocalCopy { formats, sizes, sensitivity_hint }`.
2. Policy checks Send + safety level (tagged-sensitive → drop, per level).
3. Head Mgr assigns `seq`, A becomes origin. Build `Offer`.
4. Mesh broadcasts the offer on the control plane. If `ContinuousCapture` and under
   threshold, attach eager bytes.
5. Each peer (Receive on): Head Mgr sets local head — `set_promise` (lazy) or
   `set_eager` (small/continuous). Echo suppressed via `origin_id`.

**Paste (on node B):**
1. OS asks B's adapter (`on_render`) for format X of the current head.
2. Adapter blocks the OS; Head Mgr resolves head → `{ origin_id, seq }`.
3. Transfer Eng spawns a **detached job**, opens a bulk `FetchReq` to the origin.
4. Origin pins `seq`, streams the format's bytes (serially w.r.t. other bulk jobs;
   throttled if the throttle level says so).
5. For files: the adapter serves each file's `FILECONTENTS` (per-file, and per-range in
   M3) through the same render inversion — bytes stream origin→destination with **no
   local staging**. For text/image: adapter supplies bytes directly. Timeout →
   graceful paste-fail.
6. A second paste during step 3–5 spawns a *second independent job*; both complete.

**Peer drop:**
- Mesh notices (heartbeat). Background reconciliation re-points any head that pointed
  at the now-unreachable origin to the latest *still-reachable* offer. Not done at
  paste-time. A fetch already in flight to the dropped peer fails (the RDP-equivalent
  race).

## Concurrency model

- **One clipboard thread per process, platform-affine** (Win32 needs a message pump;
  Wayland needs its own event loop). Communicates with core only via channels.
- **tokio runtime** for all mesh I/O and transfer jobs.
- **Head Manager = single owning task.** Serializes head mutations; no locks on the head.
- **The one hard bridge:** `on_render` blocks the clipboard thread → `oneshot` to core
  → async fetch → reply, with timeout. Everything else is clean message-passing.
- Discipline mirrors Vox: clear thread ownership, channels over shared state.
