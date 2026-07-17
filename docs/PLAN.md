# PLAN.md — Clipline

**Delivery-first phases.** The original plan was *risk-first* (M0–M7): prove the platform
adapter and the lazy-render sync↔async bridge before building anything on top. That worked
— M0–M3 are done and the make-or-break risk (the `on_render` bridge) is retired. The plan
is now **re-grouped by deliverable** into three phases, ordered by *who each phase serves*,
so there is always a working thing to ship.

The **milestone tags (M0–M7, M-Linux) are retained** — they are the unit-of-work anchors
the code comments reference (`M3.1`, `M2.3`, ruling `Q7`, …), and they did not move; they
are simply grouped into phases. Where one milestone spans phases (M6), it is split.

## Milestone → phase crosswalk

| Milestone / work | Phase | Status |
|---|---|---|
| M0 bridge · M1 adapter+Windows · M2 control plane · M3 bulk plane + lazy paste | **1** | ✅ done |
| P1-A launch/config interface · P1-B packaging | **1** | ✅ done |
| M4 reconciliation + §6 edge-case coverage | **2** | |
| M5 capture modes + policy (safety, throttling, gate) | **2** | |
| M6 full CLI + tray + per-OS packaging | **2** | headless slice pulled into P1 |
| M7 HTML format | **2** | |
| M-Linux Linux adapter (Wayland/X11 + FUSE) | **2** | |
| file/folder fidelity + deeper pipelining | **2** | |
| security/trust (pairing, PSK, admission, cert validation) | **3** | |
| mDNS/hybrid discovery | **3** | |
| gate signal-source bindings (Apollo hooks) | **3** | |
| RTF · directed transfer · history UI · headless primitives | **3** | |
| parallel transfers | **3** | |
| general-public distribution | **3** | |

> **Phase-numbering note.** These Phase 1/2/3 are the delivery grouping and **supersede**
> the old "Phase 2 (post-v1)" / "Phase 3" labels earlier docs and code comments used. Their
> contents were folded in: the old post-v1 **security/discovery/RTF/history** work is now
> **Phase 3**; the old **file-fidelity/pipelining** backlog is **Phase 2**; the old "Phase
> 3" **parallel transfers** stays **Phase 3**. All `Phase 2`/`Phase 3` comments across docs
> and code were swept to match. Milestone tags (`M4`, `M5`, `M-Linux`, `M3.1`, …) are
> unchanged.

---

# Phase 1 — the Lance deliverable  (2-node point-to-point; full mesh not required)  ✅ COMPLETE

**Status: complete on Windows** — the end-to-end lazy paste (M0–M3) plus the launch surface
(P1-A) and self-contained release binary (P1-B) are all done and building. Remaining before
declaring it battle-tested with Lance: an end-to-end run against an actual Lance session
(hook launch + kill-by-name; confirm no visible console on Lance's detached launch) and a
recorded two-machine TEST.md pass. Known Phase-1 limitations, all deferred by decision to
Phase 2: no bandwidth throttling (clipboard vs. the GPU stream), text/images eager on
Windows (Finding B), no HTML, no safety-hint filtering (a copied password syncs), no
reconciliation (moot at 2 nodes).

**Goal:** a working clipboard for a **Lance** streaming session. Lance
(github.com/samsam07/Lance) orchestrates Apollo (a Sunshine fork) + Moonlight for
multi-monitor GPU streaming — the "Apollo/Moonlight use case Clipline happens to fit"
(CLAUDE.md) — and has no clipboard of its own. Lance launches `clipline` per machine via a
**session hook** (its `async`, unsupervised, variable-substituting external-command
mechanism); the two instances bridge the clipboards over Clipline's own **2-node** link.
The general N-node mesh (reconciliation, discovery, multi-peer robustness) is **not
required** here — a single explicit peer pair is the operative topology, which M2/M3 already
support (the M3 manual gate is literally box-A ↔ box-B).

Security is **trusted-LAN only** in this phase (confidentiality-only TLS, accept-any-cert);
real auth/pairing/cert-validation is Phase 3.

## M0 — Prove `on_render` on both OSes, no networking  ✅ DONE — GO

**Outcome: GO on both Windows and KDE-Wayland.** The lazy-render bridge holds and fails
gracefully; five cross-validated platform findings (A–E) are recorded in
`PLATFORM-NOTES.md` and feed M1. See "Post-M0 sequencing" below.

The make-or-break. With **zero mesh code**, prove the `ClipboardAdapter::on_render`
inversion works on:
- **Windows** — own the clipboard, advertise a delayed-render format, serve bytes from a
  `WM_RENDERFORMAT` callback that blocks the OS while an async task produces them (simulate
  the network with a delay), with a timeout → graceful paste-fail.
- **Linux / KDE Plasma (Wayland)** — same, via `wl-clipboard-rs` data-control: own the
  selection, advertise formats, serve bytes on the `send` fd on demand.

Exit criterion: copy a placeholder in-process on each OS, paste in a real app
(Explorer/Notepad; Dolphin/Kate), confirm bytes arrive lazily and a forced timeout fails
cleanly without hanging the app. **If this bridge doesn't hold, the lazy design must be
reworked — stop and redesign here, not later.**

### Post-M0 sequencing (decided after M0)

M0 validated the `on_render` bridge and the adapter's expressibility on **both** OS models,
so implementation proceeds **Windows-first**; the **Linux adapter is deferred to a dedicated
milestone (M-Linux, now Phase 2)** — see its entry there.

## M1 — Adapter trait in core (injection seam) + Windows adapter  ✅ DONE

**Outcome: DONE.** The `ClipboardAdapter` contract is locked (deferred render inversion,
Finding D); the Windows adapter serves text, image (PNG-on-wire), and virtual files
(`IDataObject` `FILEDESCRIPTORW`/`FILECONTENTS`, streaming — decision #8 amended). Core
builds with **no** platform clipboard crate, driven by a mock. 10 tests green.

Lock the `ClipboardAdapter` contract against what M0 learned. It must be **deferred/async —
not a synchronous callback** — so it is expressible for *both* Windows (block the pump
thread) and Wayland (non-blocking fd-serve): see `PLATFORM-NOTES.md` Finding D. **Define the
trait in `clipline-core`; implement and inject the Windows adapter** (Linux deferred — see
Post-M0 sequencing), validating the reuse seam (CONVENTIONS.md) from the first milestone
that has it. Core must compile and be drivable with a mock/test adapter, no platform
clipboard crate in its dependency tree — and the mock stands in for the not-yet-written
Linux adapter, keeping the trait honest for both.

Serve files lazily on **Windows** via an `IDataObject` advertising `CFSTR_FILEDESCRIPTORW` +
`CFSTR_FILECONTENTS` (virtual files — **not** `CF_HDROP`, which clipboard monitors
force-materialize at copy; see `PLATFORM-NOTES.md` Finding C). File contents **stream**
through the render bridge (`FormatReq.file_idx`); **no staging dir** and **no
`materialize_files`** (M1 decision — mstsc-style). A file group is carried in `Offer.files`.
Settle locked decision #8's Windows mechanism here. Text + image format round-trip
(PNG-on-wire).

## M2 — Mesh control plane + offer/promise end-to-end  ✅ DONE

**Outcome: DONE.** TLS-over-TCP (rustls on ring; ephemeral self-signed cert + accept-any
verifier — confidentiality only, auth is Phase 3), one listening port, per-peer control
connection, `Presence` handshake with version check, peer table + connection dedup, 2 s
heartbeat / 6 s liveness drop. The protocol `[CRYSTALLIZE]` is **pinned** (`wire.rs`):
length-prefixed `postcard` frames (`ControlCodec`), 1 MiB cap, `ErrorCode`; `OriginId` =
random `u128`, `Seq` = `u64` Lamport, `ContentHash` = BLAKE3 over the offer *manifest* (not
content — keeps files lazy). Head Manager (single owning task, decision #4) with Lamport
ordering, echo suppression, offer/promise through the injected adapter, and connect-time
`HeadQuery`/`HeadReply` late-join. `HeadCapture` only (eager/Continuous is M5). 15 core
tests green — **needs `--features mock`** (or `--all-features`); a plain `cargo test`
silently skips the three offer/promise gate tests.

- **Transport:** TLS-over-TCP, explicit endpoints, one listening port, per-peer **control**
  connection; `Presence`/heartbeat; peer table.
- **Message codec:** length-prefixed frames + a compact serde codec. Wire field layout /
  framing / encoding / error codes `[CRYSTALLIZE]` here — **pinned**; see `wire.rs` /
  `protocol.rs`.
- **Offer/promise:** copy → broadcast `Offer` → peers set their head (`set_promise`, or
  `set_eager` for small payloads riding the control plane). Echo suppression (`origin_id`),
  ordering (highest `seq`, `origin_id` tiebreak). Connect-time `HeadQuery`/`HeadReply` for
  late joiners.

## M3 — Bulk plane + lazy fetch (the end-to-end lazy paste)  ✅ DONE  *(was M4 + M5 job semantics)*

**Outcome: DONE.** The end-to-end lazy paste, proven on real hardware (TEST.md checks 1–9):
text, images (incl. `BI_BITFIELDS`/V5 screenshots), ≥1 GB files, folders with empty dirs,
cancel mid-transfer, concurrent pastes, graceful failure on origin loss — all by-reference,
no staging either side. Directional per-peer **bulk** connection split from control by a
`ConnRole` byte; `TCP_NODELAY` + windowed read-ahead (M3.5 perf). Origin `PinStore` keyed
`(peer, job_id)`; `EndJob`/abort.

Second per-peer **bulk** connection. `FetchReq` keyed `{origin_id, seq, format, file_idx?}`
→ byte stream. Paste → **detached job** → fetch → adapter supplies the bytes, wired into the
M0/M1 `on_render` bridge for the real cross-mesh lazy paste (text, image, and Windows
`FILECONTENTS` files). **Origin pins** the requested seq's bytes; serial bulk transfers.
Includes the job-model semantics that are just this system exercised: multiple pastes →
multiple completing jobs, pin-survives-new-copy, explicit-abort-only (`SPEC.md` §4).

## P1-A — Stable launch/config interface for Lance hooks  ✅ DONE

**Outcome: DONE.** The runner is now the product launch surface a Lance session hook depends
on: `--port` (optional, default 9860), `--peer IP[:PORT]` (repeatable; a bare IP inherits
`--port`, resolved order-independently; inbound from unlisted peers also accepted),
`--log-file PATH` (a detached, unsupervised process's stdout may go nowhere; falls back to
stdout on open failure), headless, and a clean Ctrl-C shutdown for the manual/dev case.
Lance launches it **detached** and stops it at session end with `taskkill /F /IM
clipline.exe`; on process exit — graceful *or* forceful — the OS destroys our message window
and releases clipboard ownership, so no dead delayed-render promise is left behind (no
explicit release path needed). TEST.md gained the Launch & lifecycle checks (L1/L2).

Not the product CLI (`clipline up`, status, toggles, tray) — that is M6 (Phase 2). P1-A is
the minimum stable surface the hook needs.

## P1-B — Packaged launchable Windows binary  ✅ DONE

**Outcome: DONE.** `cargo build --release` produces a **self-contained** `clipline.exe`
(~4 MB): the MSVC C runtime is linked **statically** (`.cargo/config.toml`,
`+crt-static`), so it launches on a target machine with **no VC++ redistributable
installed** — verified with `dumpbin /dependents` (zero `vcruntime`/`msvcp`/`ucrtbase`
imports; only always-present OS DLLs, incl. one `api-ms-*` API set). Release symbols
stripped (`strip = true`). Console subsystem kept (Ctrl-C for dev; Lance launches detached,
so no visible window). Version metadata / icon and cross-OS packaging are deferred to M6
(Phase 2).

---

# Phase 2 — the tool itself  (real mesh + strong clipboard, stable & usable)

Lance-agnostic maturity: make Clipline good at what it was built for — the N-node mesh, a
complete clipboard, and the policy knobs that make it usable.

## M4 — Reconciliation + edge-case coverage

Background head **reconciliation** on peer drop: re-point affected heads to the latest
still-reachable offer proactively — never a paste-time substitution (`SPEC.md` §5). Cover
the `SPEC.md` §6 edge-case table with tests.

## M5 — Capture modes + Policy knobs

`HeadCapture` / `ContinuousCapture` (eager threshold `[CRYSTALLIZE]`). Safety level
(`Off`/`RespectHints`/`Strict`) reading OS sensitivity hints. Throttling level
(`Unlimited`/`Throttled`/`SignalDriven`, bulk-only, token bucket; rate `[CRYSTALLIZE]`).
Three lifecycle toggles. Gate as an abstract signal input (no source binding yet — that is
Phase 3). The `SignalDriven` throttle is the "don't saturate Wi-Fi while a GPU stream runs"
lever most relevant to Lance, deferred here deliberately because the useful variant needs
the Phase-3 signal-source binding and its rate is an unpinned `[CRYSTALLIZE]`.

## M6 — Control/UI + packaging  (full)

CLI (`clipline up`, status, toggles) and system tray. Native packaging per OS. Headless
operation confirmed (no GUI required for daemon use). *(The headless-launch slice — a stable
`--port`/`--peer` interface + a Windows binary — was pulled forward into Phase 1 as P1-A/B
for Lance; this is the remaining product CLI, tray, and cross-OS packaging.)*

## M7 — HTML format

`text/html` ↔ `CF_HTML` codec (byte-offset preamble). `[CRYSTALLIZE: html milestone]`.

## M-Linux — Linux adapter

Deferred with the rest of the Linux work after M0 proved the trait is expressible against
both OS models (see Post-M0 sequencing). Owns:
- the Wayland **`ext-data-control`** source adapter (KWin advertises
  `ext_data_control_manager_v1`; the older wlr protocol is absent here) and the X11 owner
  impl;
- **FUSE-backed lazy files** (`PLATFORM-NOTES.md` Finding E) — the FreeRDP-proven decoupling
  so file bytes stream as ordinary file I/O, not inside the ~1 s clipboard read budget;
- verifying whether **`wl-clipboard-rs`** (named in CONVENTIONS) can serve *lazily*
  per-request, or whether the raw `wayland-client` + `ext-data-control` approach the M0b
  spike used is required (a CONVENTIONS decision);
- **re-running the M0b latency tests**, including the pending empirical confirm of the ~1 s
  Qt boundary (Finding E, source-pinned but not yet bracketed on the metal): in Kate,
  `--delay-ms 900` should paste and `--delay-ms 1100` should not.

## Strong-clipboard fidelity + deeper pipelining

The clipboard-completeness and throughput work that makes the tool "strong" (moved here from
the old post-v1 backlog).

- **File/folder transfer fidelity (M3 omissions).** M3 moves a file's *bytes* and rebuilds
  the folder tree (incl. empty dirs); these are the gaps:
  - **Virtual-file sources** — copying *out of* a zip, or from any app offering an
    `IDataObject` / `FILEDESCRIPTOR` source rather than `CF_HDROP` real paths (e.g. Outlook
    attachments). Only real filesystem paths are captured today.
  - **File metadata** — only size + contents cross. Timestamps, attributes (read-only,
    hidden), permissions/ACLs, and NTFS alternate data streams are dropped; pasted files get
    fresh timestamps.
  - **Symlinks inside a copied tree** — skipped for loop safety; not recreated.
  - **Very deep trees** — capped at depth 64 at capture.
- **Deeper transfer pipelining.** M3 shipped `TCP_NODELAY`, a bigger wire chunk, and
  **window read-ahead** (fetch a 4 MiB window per round trip + prefetch the next while the
  app consumes the current), lifting paste throughput off the round-trip floor. The
  remaining lever is *multiple windows in flight at once* (true request pipelining on the
  bulk connection), for links where a single window still leaves the pipe idle. Only if a
  real need appears.
- **Alternate bulk transport for large files `[CRYSTALLIZE]`.** A config that switches the
  large-file path onto a faster technology (an FTP/SFTP-class bulk mover, or similar) for
  maximum throughput, while the clipboard model still drives *what* moves and *when*. Needs
  a spec — the offer/pin/abort semantics and the by-reference contract (decision #8) must
  still hold; only the byte pipe changes. Idea captured; design deferred.

---

# Phase 3 — public polish + security layer

What "true public usage" needs beyond a trusted LAN.

## Security / trust layer

- Pairing / per-peer identity keys / optional PSK.
- Inbound **admission gating** — the commented seam in the mesh accept path (`SPEC.md` §10
  ⚠️ note): on a no-auth LAN any host that reaches the port joins the mesh; this gate closes
  that.
- Replace the accept-any-cert verifier (`mesh/tls.rs` `AcceptAnyServerCert`) with real
  certificate validation against the pairing/identity trust anchor.

## Discovery

- mDNS auto-discovery; hybrid (mDNS discover + identity-key trust gate). Supersedes v1's
  explicit-endpoints-only model (locked decision #10).

## Gate signal-source bindings

- Bind the abstract gate signal (which core already consumes from M5) to real sources:
  Apollo command hooks, scripts, manual triggers.

## Other reach features

- RTF format support.
- Directed one-shot transfer: `clip send <file> --to <peer>` (reuses the lazy file path,
  bypasses the shared clipboard).
- Headless CLI clipboard primitives (`clip copy` / `clip paste` over the mesh).
- Clipboard-history UI / ring buffer beyond what the OS historian provides.

## Parallel transfers

Multiple bulk jobs concurrently — only if a real need appears; it fights the
bandwidth-politeness goal, so it stays opt-in and last.

## General-public distribution

Signed installers, auto-update, per-OS packaging for non-technical users.

---

## Deferred detail (`[CRYSTALLIZE]`, by owning milestone)

- Wire field layouts / framing / encoding / error codes → **done**: control plane in M2,
  bulk plane in M3.1 (both `wire.rs` — `ControlCodec` / `BulkCodec`, `MAX_FRAME_LEN` /
  `MAX_BULK_FRAME_LEN`, `BULK_CHUNK`, `ConnRole`, `PROTOCOL_VERSION`, `ErrorCode`).
- Eager-size threshold value → M5.
- Throttle rate(s) → M5.
- Per-OS render mechanics specifics → M0/M1 (platform) — **done** for Windows. Empirical
  spike findings live in `PLATFORM-NOTES.md` (incl. the `CF_HDROP` vs.
  `CFSTR_FILEDESCRIPTOR` question that resolved locked decision #8).
- Staging-dir layout + cleanup → **dropped** (streaming, no staging — M1 decision). Linux
  FUSE mount lifecycle → M-Linux.
- `Strict` safety policy specifics → M5 (policy).
- X11 crate choice → M-Linux (platform).
