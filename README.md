# Clipline

**Full cross-machine clipboard for a small LAN — text, images, files, and rich text —
as a symmetric peer-to-peer mesh, with lazy (on-demand) transfer.**

> **Status: early development.** The design is complete and locked; implementation is
> just beginning. Expect things to be incomplete or absent until the first milestones
> land. See [`PLAN.md`](PLAN.md) for the roadmap.

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

Planned for v1 (see [`PLAN.md`](PLAN.md) for status):

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
| Windows | Primary target (first) |
| Linux — KDE Plasma / Wayland | Primary target (first) |
| Linux — X11 | Planned |
| macOS | Not planned for v1 |

Wayland clipboard access uses the `ext-data-control` / `wlr-data-control` protocol
(supported by KWin), so Clipline can serve the clipboard headlessly.

## Architecture & design

The design is documented in full:

- [`ARCHITECTURE.md`](ARCHITECTURE.md) — components, the platform adapter contract, the
  wire protocol shape, concurrency model.
- [`SPEC.md`](SPEC.md) — behavioral specification (the offer/promise/fetch semantics,
  capture modes, edge cases, knobs, lifecycle).
- [`CONVENTIONS.md`](CONVENTIONS.md) — code conventions and the `clipline-core` / `clipline`
  split.
- [`PLAN.md`](PLAN.md) — milestone roadmap.

Clipline is split into a reusable, consumer-agnostic core crate (`clipline-core`) and a
desktop binary (`clipline`), so the core can back other clients later (e.g. a mobile app
over FFI).

## Building

Not yet — implementation is at its first milestone. Build instructions will appear here
once there's something to build. Clipline is written in **Rust**.

## License

MIT.
