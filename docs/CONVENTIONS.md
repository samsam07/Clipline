# CONVENTIONS.md — Clipline

Shared conventions. Most of this **amortizes from Vox** — reuse the same `.editorconfig`,
`rustfmt.toml`, `clippy` posture, and module-layout instincts rather than re-deciding.
This file records only what's worth stating explicitly or what differs for Clipline.

## Language / toolchain
- Rust (edition pinned in `Cargo.toml`). Single static binary per platform.
- `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings`
  must pass before any slice is considered done.
- Cross-compilation: defer `cargo-zigbuild` to a later phase, as with Vox. Develop
  natively on each target first.

## Crates (leaning; final set pinned per milestone)
- `tokio` — async runtime for mesh I/O and transfer jobs.
- `rustls` — TLS over TCP. (No QUIC.)
- `wl-clipboard-rs` — Wayland clipboard via `ext-data-control` / `wlr-data-control`
  (MIME-typed, fd-serve = lazy-capable). **Not `arboard`** (text+image only).
- `windows` — Win32 clipboard + delayed render (`WM_RENDERFORMAT`).
- X11 selection-owner: raw impl (crate choice `[CRYSTALLIZE: platform milestone]`).
- serde + a compact codec for control messages (`[CRYSTALLIZE: protocol milestone]`).

## Module / workspace layout
- Workspace split, mirroring Vox: **`clipline-core`** (reusable crate) and **`clipline`**
  (CLI + tray; includes core). Same naming pattern as `vox-core` / `vox`.
- `clipline-core` is **consumer-agnostic and genuinely reusable** — not merely "a library
  the CLI imports." The reuse test is the same as Vox's Android-client goal: a consumer
  that is *not* a desktop CLI (e.g. an Android app binding core over FFI/JNI) must be able
  to drive core. Everything reusable lives in core; the binary is a thin shell.
- Concretely, `clipline-core` owns: head manager, transfer engine, mesh, policy, protocol
  types, **and the `ClipboardAdapter` trait itself**. It must **not** depend on any
  platform clipboard crate (`wl-clipboard-rs`, `windows`, X11) — those are *injected* by
  the consumer:
  - the `clipline` binary injects the Win32 / Wayland / X11 adapters (cfg-gated, one per OS);
  - a future Android client would inject a JNI-backed `ClipboardManager` adapter.
- Core does **no I/O it doesn't own**: it takes config/state *in* and emits events *out*.
  No logging to stdout from core, no config-file reading in core, no assumption that a CLI
  or tray exists. Core's public API must be drivable programmatically (start/stop mesh, set
  toggles, query head, subscribe to offers/events) by both the CLI and FFI.
- This is the deeper *why* behind "core must not depend on platform clipboard crates" and
  behind Windows-first not leaking assumptions into core (see CLAUDE.md): the seam exists
  for reuse, not just for tidy layering.

## Naming
- Use the names already fixed in `SPEC.md`/`ARCHITECTURE.md` **verbatim**: `Offer`,
  `seq`, `origin_id`, `HeadQuery`/`HeadReply`, `FetchReq`, `Presence`, `Abort`,
  `HeadCapture`/`ContinuousCapture`, safety levels `Off`/`RespectHints`/`Strict`,
  throttle levels `Unlimited`/`Throttled`/`SignalDriven`, toggles `Presence`/`Send`/
  `Receive`, `set_promise`/`set_eager`/`on_render`/`materialize_files`.
- Apply the **anti-drift grep rule** (CLAUDE.md) before introducing or reusing any name.

## Error handling
- Library code returns `Result` with typed errors (`thiserror`-style); the binary maps
  to user-facing messages. No `unwrap()`/`expect()` outside tests and provably-infallible
  startup.
- A paste that cannot be satisfied (timeout, origin gone) must fail **gracefully** —
  release the OS render call cleanly, never hang the pasting application.

## Logging
- Structured logging (tracing-style), levels used consistently. The hot path
  (`on_render`, fetch) should be traceable end-to-end for the sync↔async bridge.
- Never log clipboard *contents*; log metadata (formats, sizes, seq, origin) only.
  This matters doubly given the safety layer and the corporate-LAN context.

## Concurrency conventions
- Head mutations go through the single Head Manager task — never lock and mutate the
  head from elsewhere.
- Clipboard-thread ↔ core is channels only. The `on_render` block→oneshot→fetch→reply
  bridge is the *only* place a platform thread blocks on core; keep it isolated and
  well-commented, with an explicit timeout.

## Testing
- Tests are the contract (Vox posture). Add one for any non-trivial behavior.
- Prioritize tests that pin the locked semantics: ordering (seq + origin tiebreak),
  echo suppression, multi-paste → multiple completing jobs, pin-survives-new-copy,
  no-auto-cancel, control-plane-never-throttled.
- Platform `on_render` behavior needs real-OS smoke tests on Windows and KDE-Wayland
  (M0). Mesh logic should be testable without real clipboards via the trait.

## Comments / docs
- Comment the *why* for anything touching a locked decision or the sync↔async bridge,
  so a future session doesn't "helpfully" undo it.
- Reference `SPEC.md` section numbers in code near the behavior they implement.
