# Clipline

**Full cross-machine clipboard for a small LAN — text, images, files, and rich text —
as a symmetric peer-to-peer mesh, with lazy (on-demand) transfer.**

> **Status: Phase 1 complete (Windows).** The end-to-end lazy paste works today between two
> Windows machines — copy on one, paste on another; text, images, and files (including large
> files and whole folders) are pulled on demand. Later phases add the full N-node mesh,
> capture modes / safety / bandwidth throttling, Linux, and a security layer. See
> [`docs/PLAN.md`](docs/PLAN.md) for the roadmap and the Phase 1/2/3 structure.

## What it is

Clipline runs on every machine in a small trusted LAN and makes them share **one
clipboard**. Copy on any machine, paste on any other, and it feels like a local paste —
including for **files** and **images**, not just text.

It exists because the clipboard sharing built into remote-desktop and game-streaming
tools (RDP, Apollo/Sunshine, Moonlight) is typically **text-only**. Clipline aims for the
full clipboard, cross-platform (Windows ↔ Linux), with no central server.

Clipline is an **independent tool**. It happens to pair well with an Apollo/Moonlight
streaming setup, but it doesn't depend on them and isn't limited to that use.

## How it works

Clipline uses an **offer / promise / lazy-fetch** model — the same idea RDP uses for
clipboard redirection, generalized from a single host to an N-node mesh:

1. **Copy** broadcasts a tiny *offer* (which formats are available, their sizes, a hash).
   No content moves.
2. Every other machine sets its clipboard to a *promise* advertising those formats.
3. **Paste** pulls only the format you actually asked for, on demand, directly from the
   machine you copied on.

So copying a 4 GB file is instant; the bytes only travel if and when someone pastes it.
Small items (text, small images) are replicated eagerly so they survive the source going
offline and show up in your OS clipboard history (Win+V / Klipper).

The mesh is **symmetric** — every node is equal, there is no hub or server, and any node
can be the one you're sitting at.

## Features

Planned for v1 (see [`docs/PLAN.md`](docs/PLAN.md) for status):

- Full clipboard: **text, images, files, HTML** (RTF later).
- **Lazy transfer** — bytes move only on paste; big files don't block copying.
- **Symmetric P2P mesh** — no central server, no single point of failure.
- **Two capture modes** — mstsc-style head-only, or continuous (every machine's copies
  appear in your OS clipboard history).
- **Safety levels** — honor OS "confidential content" hints (password managers) and keep
  tagged content off the wire.
- **Bandwidth throttling** — keep clipboard transfers polite when the network is busy
  (e.g. during a live stream).
- **Headless-capable** — runs without a GUI; usable as a background daemon.
- Encrypted transport (TLS over TCP).

## Platform support

| Platform | Status |
|---|---|
| Windows | **Working** (Phase 1) |
| Linux — KDE Plasma / Wayland | Planned (Phase 2) |
| Linux — X11 | Planned (Phase 2) |
| macOS | Not planned for v1 |

Wayland clipboard access uses the `ext-data-control` / `wlr-data-control` protocol
(supported by KWin), so Clipline can serve the clipboard headlessly.

## Architecture & design

The design is documented in full:

- [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md) — components, the platform adapter contract,
  the wire protocol shape, concurrency model.
- [`docs/SPEC.md`](docs/SPEC.md) — behavioral specification (the offer/promise/fetch
  semantics, capture modes, edge cases, knobs, lifecycle).
- [`docs/CONVENTIONS.md`](docs/CONVENTIONS.md) — code conventions and the `clipline-core` /
  `clipline` split.
- [`docs/PLAN.md`](docs/PLAN.md) — the delivery roadmap (Phase 1/2/3).

Clipline is split into a reusable, consumer-agnostic core crate (`clipline-core`) and a
desktop binary (`clipline`), so the core can back other clients later (e.g. a mobile app
over FFI).

## Building & running

Clipline is written in **Rust**. On Windows you need the Rust toolchain (MSVC) plus the
Visual Studio C++ build tools.

```sh
# development build + tests
cargo build
cargo test --all-features        # some tests need the `mock` feature

# release build — a self-contained, statically-linked clipline.exe
cargo build --release
```

The release binary at `target\release\clipline.exe` links the MSVC C runtime **statically**
(see [`.cargo/config.toml`](.cargo/config.toml)), so it runs on any Windows machine with **no
VC++ redistributable installed** — copy the single `.exe` across and launch it.

### Running

Clipline is a **long-lived** process — it must keep running to own the clipboard and serve
bytes on demand. Start one on each machine:

```sh
# machine A
clipline --port 9860

# machine B — only one side needs to list the other; inbound is accepted either way
clipline --port 9860 --peer <A-ip>
```

| Flag | Meaning |
|---|---|
| `--port N` | Listening port. Optional; defaults to `9860`. |
| `--peer IP[:PORT]` | A peer to dial (repeatable). A bare IP uses `--port`. Optional — a node also accepts inbound connections. |
| `--log-file PATH` | Write logs to a file instead of stdout (useful when launched detached/unsupervised). |

Ctrl-C stops it cleanly. Set `RUST_LOG=clipline=debug,clipline_core=debug` for verbose logs.
See [`docs/TEST.md`](docs/TEST.md) for the end-to-end manual test flow.

## License

MIT.
