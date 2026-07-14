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

## M1 — Adapter trait in core (injection seam) + Windows file materialization

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

## M2 — Mesh transport (control plane)

TLS-over-TCP, explicit endpoints, one listening port, per-peer **control** connection.
`Presence`/heartbeat, peer table, connect-time `HeadQuery`/`HeadReply`. No bytes yet.

## M3 — Offer/promise end-to-end (small/eager path)

Copy → broadcast `Offer` → peers set head. Echo suppression (origin_id), ordering
(seq + origin tiebreak). Eager small payloads ride the control plane. Wire
field layout / framing / encoding `[CRYSTALLIZE]` here.

## M4 — Bulk plane + lazy fetch

Second per-peer **bulk** connection. `FetchReq` keyed `{origin_id, seq, format}` →
byte stream. Paste → detached job → fetch → adapter supplies bytes. Origin pins.
Serial bulk transfers. Wire this into the M0 `on_render` bridge for the real
end-to-end lazy paste.

## M5 — Multi-paste, pins, no-auto-cancel, reconciliation

The distributed-behavior semantics from `SPEC.md` §4–§6: multiple pastes → multiple
completing jobs; pin-survives-new-copy; explicit-abort-only; background head
reconciliation on peer drop. Cover the edge-case table with tests.

## M6 — Capture modes + Policy knobs

`HeadCapture` / `ContinuousCapture` (eager threshold `[CRYSTALLIZE]`). Safety level
(`Off`/`RespectHints`/`Strict`) reading OS sensitivity hints. Throttling level
(`Unlimited`/`Throttled`/`SignalDriven`, bulk-only, token bucket; rate `[CRYSTALLIZE]`).
Three lifecycle toggles. Gate as an abstract signal input (no source binding yet).

## M7 — Control/UI + packaging

CLI (`clipline up`, status, toggles) and system tray. Native packaging per OS. Headless
operation confirmed (no GUI required for daemon use).

## M8 — HTML format

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
- Wire field layouts / framing / encoding / error codes → M3 (protocol).
- Eager-size threshold value → M6.
- Throttle rate(s) → M6.
- Per-OS render mechanics specifics → M0/M1 (platform). Empirical spike findings
  live in `PLATFORM-NOTES.md` (incl. the `CF_HDROP` vs. `CFSTR_FILEDESCRIPTOR`
  question that touches locked decision #8).
- Staging-dir layout + cleanup → **dropped** (streaming, no staging — M1 decision).
  Linux FUSE mount lifecycle → M-Linux.
- `Strict` safety policy specifics → M6 (policy).
- X11 crate choice → M1 (platform).
