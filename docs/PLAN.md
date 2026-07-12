# PLAN.md — Clipline

Risk-first milestone ordering. The **most uncertain, most blocking** work goes first.
For Clipline the uncertainty is **not** the mesh (well-trodden) — it is the platform
adapter and the lazy-render sync↔async bridge. Prove that before building anything on
top of it.

Linux is in scope **from the start** (dev on a Windows box, remoting into a Linux box
to test) so the adapter trait is validated against *both* OS models before the protocol
hardens — this is what keeps "Windows-first" from degrading into a Linux redesign later.

## M0 — Prove `on_render` on both OSes, no networking

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

## M1 — Adapter trait in core (injection seam) + file materialization

Lock the `ClipboardAdapter` contract against what M0 learned (it must stay expressible
for Windows, X11, and Wayland). **Define the trait in `clipline-core`; implement the
desktop adapters in the `clipline` binary and inject them** — validating the reuse seam
(CONVENTIONS.md) from the first milestone that has it, rather than refactoring it in
later. Core must compile and be drivable with a mock/test adapter, no platform clipboard
crate in its dependency tree.

Implement `materialize_files` + staging on both OSes: read source file bytes, write to
staging, advertise destination-local refs (`CF_HDROP` / `text/uri-list`). Text + image
format round-trip (PNG-on-wire). X11 owner impl can land here or M-late if time-boxed.

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
- Per-OS render mechanics specifics → M0/M1 (platform).
- Staging-dir layout + cleanup → M1 (file).
- `Strict` safety policy specifics → M6 (policy).
- X11 crate choice → M1 (platform).
