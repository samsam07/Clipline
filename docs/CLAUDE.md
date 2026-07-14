# CLAUDE.md — Clipline

> Read this file first, every session. It is the contract. `ARCHITECTURE.md`,
> `SPEC.md`, `CONVENTIONS.md`, and `PLAN.md` are the detail; this file governs
> how you work and what you may and may not decide.

## What Clipline is

Clipline is a **symmetric, headless-capable, long-lived process** running on every
machine in a small trusted LAN **mesh**. It shares the **full OS clipboard**
cross-machine — text, images, files, and rich text — so that pasting on any node
feels like a local paste. It is an **independent tool**. Apollo/Moonlight GPU
streaming is *one use case it happens to fit*, not its reason for existing. Never
couple the core to Apollo.

The defining mechanism is **lazy / delayed rendering** (the model `mstsc`/RDP uses
for clipboard redirection, generalized from a single host to an N-node mesh):
copy broadcasts a tiny *offer*; bytes move only when a paste actually asks for them.

Language: **Rust**. Targets: **Windows and Linux** (Fedora/KDE Plasma, Wayland-first;
most distros ideally). Transport: **TLS-over-TCP** (not QUIC).

## Locked decisions — DO NOT relitigate

These were settled deliberately during design. Treat them as fixed. If you believe
one is wrong, *stop and ask the human* — do not silently re-decide.

1. **Mesh, symmetric, P2P.** No hub, no relay, no designated aggregator. Every node
   is equal; "main" just means "the node a human is currently sitting at."
2. **Offer/promise + lazy fetch.** Copy broadcasts an offer (metadata only). Peers
   set their local clipboard head to a delayed-render *promise*. Paste pulls only
   the requested format's bytes, on demand, point-to-point from the origin.
3. **Latest wins** = highest `seq`; ties broken by `origin_id`.
4. **Single overwritable head slot.** We never maintain our own clipboard history.
   The OS historian (Win+V / Klipper) records whatever lands on the head. We only
   ever own one slot.
5. **Paste = a detached transfer job** bound to the seq that was head at paste-time.
   Multiple pastes spawn multiple independent jobs that *all complete* (mirrors
   multi copy-paste from different folders on one machine). No clipboard queue.
6. **No automatic cancellation.** A transfer is cancelled only by explicit user
   abort. A new copy on the origin never kills an already-accepted fetch — the
   origin pins the requested seq's bytes until that fetch finishes.
7. **Transport: TLS-over-TCP, two connections per peer (control + bulk), one
   listening port.** Control plane is never throttled. Bulk transfers are serial
   (no parallelism until Phase 3) and throttleable.
8. **Files are by-reference everywhere** — bytes move only on a real paste, and only
   the bytes actually read. The destination **streams** file contents from the origin
   on demand straight to the pasting app; Clipline **never stages a local copy**
   (M1 decision — mstsc-style; there is no `materialize_files`).
   *Per-OS mechanism (refined by M0 Finding C — see `PLATFORM-NOTES.md`):* Windows
   outbound promises use the shell virtual-file model **`CFSTR_FILEDESCRIPTORW` +
   `CFSTR_FILECONTENTS`** via an `IDataObject`, **not `CF_HDROP`** — a `CF_HDROP`
   promise is force-materialized at *copy* time by clipboard monitors, which breaks
   laziness. `FILECONTENTS` is served per-file (and, in M4, per-range) through the same
   lazy-render bridge — **no staging dir**. Linux uses `text/uri-list` pointing at a
   **FUSE mount** that streams on read (no copy either; deferred to M-Linux).
9. **Preserve the whole format set across the wire**; the destination picks. Never
   pre-flatten formats at the source.
10. **Discovery v1 = explicit endpoints** (config by IP, Vox-style). mDNS/hybrid is
    Phase 2.
11. **Long-lived process is mandatory** (clipboard selection ownership + on-demand
    byte serving die if the process exits). *How* it launches (manual / login /
    service) is free per machine.

## Anti-drift rule (mechanical, non-negotiable)

Your memory of named values across a long session is **not trustworthy**. Before you
write or substitute any previously-specified named value — a struct field, a message
kind, a config key, a constant, a port, a state name, a trait method — **grep the
codebase and these docs for it first** and match the existing spelling/shape exactly.
Do not invent a second name for an existing thing. Do not "improve" a name in passing.
If grep shows a value already exists, reuse it verbatim; if it shows two spellings,
stop and flag the conflict.

## Judgment vs. work boundary

- **Locked decisions** (above, and anything tagged in `SPEC.md`/`ARCHITECTURE.md`):
  not yours to change. Implement them.
- **Implementation detail** (data-structure choice, internal function names, how you
  structure a module, which crate helper to call): yours to decide. Don't ask.
- **Anything in neither category** — a behavior the docs don't specify, an ambiguity,
  a decision with cross-cutting consequences: **ask the human.** Do not guess and
  proceed.

## `[CRYSTALLIZE]` tags

Atomic detail is deliberately deferred to the milestone that owns it, marked
`[CRYSTALLIZE: <milestone>]` in the docs. When you reach that milestone, you (with the
human) pin the detail *then* — not earlier. Examples currently deferred: exact wire
field layouts, framing/encoding, error codes, the eager-size threshold value, throttle
rates, per-OS render mechanics, the Linux FUSE mount lifecycle (M-Linux). Do not fill
these in speculatively ahead of their milestone.

## Per-slice verification ritual

For every unit of work:

1. **Restate** — in your own words, what this slice must do and which locked
   decisions/spec sections constrain it.
2. **Implement** — the slice, nothing beyond it.
3. **Self-check** — grep for reused names (anti-drift rule); re-read the relevant
   spec section; confirm you didn't relitigate a locked decision or silently fill a
   `[CRYSTALLIZE]`.
4. **Human review** — surface what you did, what you decided, and any neither-category
   question you hit.

## Scope discipline

v1 is intentionally small. Out of scope for v1 (do not build unprompted):
mDNS/discovery beyond explicit endpoints, pairing/identity keys/PSK, gate
signal-source bindings (Apollo hooks etc.), RTF format support, directed
`clip send <file> --to <peer>`, parallel transfers, clipboard-history UI. See
`PLAN.md` for the phase map.
