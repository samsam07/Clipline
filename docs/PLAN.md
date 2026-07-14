# PLAN.md — Clipline

Risk-first milestone ordering. The **most uncertain, most blocking** work goes first.
For Clipline the uncertainty is **not** the mesh (well-trodden) — it is the platform
adapter and the lazy-render sync↔async bridge. Prove that before building anything on
top of it.

Linux was validated **from the start** in M0 (dev on a Windows box, remoting into a
Linux box) so the adapter trait is proven expressible against *both* OS models before
the protocol hardens — which is precisely what now lets implementation proceed
**Windows-first** (see "Post-M0 sequencing") without degrading into a Linux redesign
later.

**Post-M1 granularity:** the platform-adapter risk that justified fine early milestones
is now **retired** (M0 + M1 done). Everything after M1 is **mesh** work (well-trodden),
so those milestones are grouped more coarsely — M2 and M3 below each fold two of the
original slices. Ordering is unchanged; only the gate boundaries moved.

## M0 — Prove `on_render` on both OSes, no networking  ✅ DONE — GO

**Outcome: GO on both Windows and KDE-Wayland.** The lazy-render bridge holds and
fails gracefully; five cross-validated platform findings (A–E) are recorded in
`PLATFORM-NOTES.md` and feed M1. See "Post-M0 sequencing" below.

The make-or-break. With **zero mesh code**, prove the `ClipboardAdapter::on_render`
inversion works on:
- **Windows** — own the clipboard, advertise a delayed-render format, serve bytes from
  a `WM_RENDERFORMAT` callback that blocks the OS while an async task produces them
  (simulate the network with a delay), with a timeout → graceful paste-fail.
- **Linux / KDE Plasma (Wayland)** — same, via `wl-clipboard-rs` data-control: own the
  selection, advertise formats, serve bytes on the `send` fd on demand.

Exit criterion: copy a placeholder in-process on each OS, paste in a real app
(Explorer/Notepad; Dolphin/Kate), confirm bytes arrive lazily and a forced timeout
fails cleanly without hanging the app. **If this bridge doesn't hold, the lazy design
must be reworked — stop and redesign here, not later.**

## Post-M0 sequencing (decided after M0)

M0 validated the `on_render` bridge and the adapter's expressibility on **both** OS
models, so implementation now proceeds **Windows-first**; the **Linux adapter is
deferred to a dedicated later milestone (M-Linux)**. That milestone owns:
- the Wayland **`ext-data-control`** source adapter (KWin advertises
  `ext_data_control_manager_v1`; the older wlr protocol is absent here) and the X11
  owner impl;
- **FUSE-backed lazy files** (`PLATFORM-NOTES.md` Finding E) — the FreeRDP-proven
  decoupling so file bytes stream as ordinary file I/O, not inside the ~1 s clipboard
  read budget;
- verifying whether **`wl-clipboard-rs`** (named in CONVENTIONS) can serve *lazily*
  per-request, or whether the raw `wayland-client` + `ext-data-control` approach the
  M0b spike used is required (a CONVENTIONS decision);
- **re-running the M0b latency tests**, including the pending empirical confirm of the
  ~1 s Qt boundary (Finding E, source-pinned but not yet bracketed on the metal): in
  Kate, `--delay-ms 900` should paste and `--delay-ms 1100` should not.

## M1 — Adapter trait in core (injection seam) + Windows adapter  ✅ DONE

**Outcome: DONE.** The `ClipboardAdapter` contract is locked (deferred render inversion,
Finding D); the Windows adapter serves text, image (PNG-on-wire), and virtual files
(`IDataObject` `FILEDESCRIPTORW`/`FILECONTENTS`, streaming — decision #8 amended). Core
builds with **no** platform clipboard crate, driven by a mock. 10 tests green.

Lock the `ClipboardAdapter` contract against what M0 learned. It must be
**deferred/async — not a synchronous callback** — so it is expressible for *both*
Windows (block the pump thread) and Wayland (non-blocking fd-serve): see
`PLATFORM-NOTES.md` Finding D. **Define the trait in `clipline-core`; implement and
inject the Windows adapter** (Linux deferred — see Post-M0 sequencing) — validating the
reuse seam (CONVENTIONS.md) from the first milestone that has it. Core must compile and
be drivable with a mock/test adapter, no platform clipboard crate in its dependency
tree — and the mock stands in for the not-yet-written Linux adapter, keeping the trait
honest for both.

Serve files lazily on **Windows** via an `IDataObject` advertising
`CFSTR_FILEDESCRIPTORW` + `CFSTR_FILECONTENTS` (virtual files — **not** `CF_HDROP`,
which clipboard monitors force-materialize at copy; see `PLATFORM-NOTES.md` Finding C).
File contents **stream** through the render bridge (`FormatReq.file_idx`); **no staging
dir** and **no `materialize_files`** (M1 decision — mstsc-style). A file group is carried
in `Offer.files`. Settle locked decision #8's Windows mechanism here. Text + image format
round-trip (PNG-on-wire).

## M2 — Mesh control plane + offer/promise end-to-end  (was M2 + M3)

Merged: the control plane can't be demonstrated with "no bytes," and its
`HeadQuery`/`HeadReply` need the very message codec the offer path pins — so they are
one milestone.

- **Transport:** TLS-over-TCP, explicit endpoints, one listening port, per-peer
  **control** connection; `Presence`/heartbeat; peer table.
- **Message codec:** length-prefixed frames + a compact serde codec for control
  messages. Wire field layout / framing / encoding / error codes `[CRYSTALLIZE]` here.
- **Offer/promise:** copy → broadcast `Offer` → peers set their head (`set_promise`, or
  `set_eager` for small payloads riding the control plane). Echo suppression
  (`origin_id`), ordering (highest `seq`, `origin_id` tiebreak). Connect-time
  `HeadQuery`/`HeadReply` for late joiners.

Demonstrable end: copy on A → B reflects a promise on its head; a late joiner syncs.

## M3 — Bulk plane + lazy fetch (the end-to-end lazy paste)  (was M4 + M5 job semantics)

Second per-peer **bulk** connection. `FetchReq` keyed `{origin_id, seq, format,
file_idx?}` → byte stream. Paste → **detached job** → fetch → adapter supplies the bytes,
wired into the M0/M1 `on_render` bridge for the real cross-mesh lazy paste (text, image,
and Windows `FILECONTENTS` files). **Origin pins** the requested seq's bytes; serial bulk
transfers. Includes the job-model semantics that are just this system exercised: multiple
pastes → multiple completing jobs, pin-survives-new-copy, explicit-abort-only
(`SPEC.md` §4). The biggest integration — kept as its own gate.

## M4 — Reconciliation + edge-case coverage  (was M5 remainder)

Background head **reconciliation** on peer drop: re-point affected heads to the latest
still-reachable offer proactively — never a paste-time substitution (`SPEC.md` §5).
Cover the `SPEC.md` §6 edge-case table with tests.

## M5 — Capture modes + Policy knobs  (was M6)

`HeadCapture` / `ContinuousCapture` (eager threshold `[CRYSTALLIZE]`). Safety level
(`Off`/`RespectHints`/`Strict`) reading OS sensitivity hints. Throttling level
(`Unlimited`/`Throttled`/`SignalDriven`, bulk-only, token bucket; rate `[CRYSTALLIZE]`).
Three lifecycle toggles. Gate as an abstract signal input (no source binding yet).

## M6 — Control/UI + packaging  (was M7)

CLI (`clipline up`, status, toggles) and system tray. Native packaging per OS. Headless
operation confirmed (no GUI required for daemon use).

## M7 — HTML format  (was M8)

`text/html` ↔ `CF_HTML` codec (byte-offset preamble). `[CRYSTALLIZE: html milestone]`.

---

## Phase 2 (post-v1)
- mDNS auto-discovery; hybrid (mDNS discover + identity-key trust gate).
- Pairing / per-peer identity keys / optional PSK.
- Gate **signal-source bindings** (Apollo command hooks, scripts, manual triggers).
- RTF format support.
- Directed one-shot transfer: `clip send <file> --to <peer>` (reuses the lazy file path,
  bypasses the shared clipboard).
- Headless CLI clipboard primitives (`clip copy` / `clip paste` over the mesh).
- Clipboard-history UI / ring buffer beyond what the OS historian provides.

## Phase 3
- Parallel transfers (multiple bulk jobs concurrently) — only if a real need appears;
  it fights the bandwidth-politeness goal, so it stays opt-in and last.

## Deferred detail (`[CRYSTALLIZE]`, by owning milestone)
- Wire field layouts / framing / encoding / error codes → M2 (protocol).
- Eager-size threshold value → M5.
- Throttle rate(s) → M5.
- Per-OS render mechanics specifics → M0/M1 (platform) — **done** for Windows. Empirical
  spike findings live in `PLATFORM-NOTES.md` (incl. the `CF_HDROP` vs.
  `CFSTR_FILEDESCRIPTOR` question that resolved locked decision #8).
- Staging-dir layout + cleanup → **dropped** (streaming, no staging — M1 decision).
  Linux FUSE mount lifecycle → M-Linux.
- `Strict` safety policy specifics → M5 (policy).
- X11 crate choice → M-Linux (platform).
