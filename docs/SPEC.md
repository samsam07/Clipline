# SPEC.md — Clipline

Behavioral specification. *What* the system does, independent of *how*. Atomic values
are deferred via `[CRYSTALLIZE: <milestone>]`.

## 1. The offer/promise/fetch model

- **Offer** — broadcast on copy. Metadata only: `origin_id`, `seq`, available
  `formats` (each with MIME + size), content `hash`. No bytes (except eager small
  payloads in Continuous mode, which ride with the offer). Field layout
  `[CRYSTALLIZE: protocol milestone]`.
- **Promise** — what a receiving node sets as its local clipboard head: a delayed-render
  placeholder advertising the offer's formats. Holds no bytes.
- **Fetch** — issued when a local paste asks for a specific format. Keyed
  `{ origin_id, seq, format }`. Pulls only that format's bytes, point-to-point from
  the origin, over the bulk plane.

**Ordering:** the newest offer wins, defined as highest `seq`; ties broken by
`origin_id`. Every node acts only on its **own local head** and its **own in-flight
jobs** — there is no distributed agreement in the hot path.

**Echo suppression:** every offer carries `origin_id`; a node never re-broadcasts or
re-applies an offer it originated.

## 2. The head slot

- The head is a **single overwritable slot**. Every copy (local or remote, subject to
  mode) overwrites it. Clipline maintains **no history of its own** — the OS historian
  (Win+V / Klipper), if the user has it enabled, records whatever lands on the head.
- "Main machine" is not a role; it is simply whichever node a human is sitting at.
  The model is fully symmetric.

## 3. Capture modes (affects how often we touch the local head)

- **HeadCapture** (default; `mstsc`-like): we set the local head only when needed —
  the freshest offer is reflected as a promise; bytes are lazy. The OS historian only
  ever sees items actually pasted/rendered.
- **ContinuousCapture:** the instant *any* peer copies, we set the local head with the
  offer. For payloads **under the eager threshold** we set real bytes (`set_eager`) so
  the OS historian records them — the user sees every machine's copies in Win+V/Klipper
  as a side effect of the OS feature, not of us. Payloads **over the threshold** remain
  lazy promises (cannot enter history; a promise can't be historized).

Eager threshold value: `[CRYSTALLIZE: head/eager milestone]` (working guess ~256 KB).

## 4. Paste and transfer jobs

- A **paste detaches an independent transfer job** bound to the seq that was head at
  paste-time. This mirrors multi copy-paste from different folders on a single machine:
  the clipboard is one slot, but each paste spawns its own copy operation.
- **Multiple pastes → multiple jobs → all complete.** No clipboard queue; jobs are the
  unit that runs and finishes.
- **Serial on the wire:** bulk transfers run one at a time (politeness/bandwidth; see
  §7). Both jobs are visible as in-progress to the user; one may wait then proceed.
  Parallel transfer is **Phase 3**.
- **Origin pins** each requested seq's bytes until that job completes. A new copy on the
  origin advances the head but does **not** disturb an accepted fetch.
- **No automatic cancellation.** Only an explicit user abort cancels a transfer.
  (Re-pasting the same head simply spawns another job of the same content, exactly as
  on one machine.)

## 5. Origin availability

- Small eager items survive the origin going offline (their bytes are already local /
  in OS history). Large lazy items cannot be fetched once the origin is gone.
- On peer drop, **background reconciliation** re-points affected heads to the latest
  still-reachable offer — proactively, *not* as a paste-time substitution (the user must
  never press Ctrl+V expecting file X and silently receive item Y).
- The single unavoidable failure: origin vanishes between keystroke and fetch
  completion → that paste fails (identical to RDP's race).

## 6. Edge-case table (authoritative)

| Case | Behavior |
|---|---|
| Copy file 1 on A, paste on B; later copy file 2 on A, paste on B again | Two independent serial jobs. No conflict (the pastes don't overlap). |
| Copy file 2 on A **while** B is fetching file 1 | Fetch 1 completes (origin pins seq-1 bytes); head advances to seq 2; next paste gets file 2. Better than RDP. |
| B pastes file 1, then **also** pastes file 2 while file 1 still transferring | Two independent jobs, both complete (multi-paste = multiple detached copy ops, like one machine). |
| B explicitly aborts a transfer | That job is cancelled; origin releases its pin. |
| B and C both fetch from A at once | Served serially on A's bulk plane. Polite, throttleable. No parallelism (until Phase 3). |
| A goes offline mid-fetch | That fetch fails. Background reconciliation re-points heads to latest reachable offer for future pastes. |
| Gate signal asserts mid-fetch | Throttle posture applies to bulk transfers; control plane unaffected, offers still instant. |
| Tiny text copied during a big fetch | Rides the control plane — instant, unaffected by the bulk transfer. |
| Late joiner connects | Issues `HeadQuery`; receives current head via `HeadReply`. |

## 7. Knobs (orthogonal — do not bundle)

**Safety level** (independent):
- `Off` — replicate everything, including content the OS tags sensitive.
- `RespectHints` (default) — honor OS confidential-content tags (password-manager
  hints on Windows/KDE); keep tagged content **off the wire**; sync everything else.
- `Strict` — tighter posture, e.g. text+images only / files never, or opt-in per copy.
  Exact strict policy `[CRYSTALLIZE: policy milestone]`.

**Throttling level** (independent; applies to **bulk plane only** — control is never
throttled):
- `Unlimited` — no cap.
- `Throttled` — rate ceiling on the bulk writer (token bucket). Rate value
  `[CRYSTALLIZE: throttle milestone]`.
- `SignalDriven` — posture flips on an abstract external signal. The **signal-source
  binding is Phase 2**; the core only consumes an abstract "signal asserted" input.

## 8. Lifecycle (three toggles + a gate)

The process is **long-lived** (clipboard ownership + on-demand serving require it).
*How* it launches — manual `clipline up`, login item, or service — is per-machine and
not prescribed.

- **Presence** — am I a discoverable/connectable mesh member at all?
- **Send** — do I broadcast my local copies outward?
- **Receive** — do I reflect others' offers onto my local head?
- **Gate** — an abstract external signal can drive Send/Receive on/off **without**
  touching Presence.

These compose to cover every requested behavior:
- Always-on daemon = Presence+Send+Receive on, no gate.
- Session-scoped = gate binds Send/Receive to an external signal.
- "Disable remote clipboard" (tray) = flip Send and/or Receive off (per-direction
  control is intentional — e.g. Receive-only on a corporate box: take others' clipboards
  in, never leak yours out).

Gate signal-source bindings (Apollo command hooks, scripts, manual): **Phase 2**.

## 9. Format handling

- **Preserve the whole format set across the wire; the destination picks.** Never
  pre-flatten at the source.
- **Text** — UTF-8 on the wire; rebuild platform variants at paste.
- **Images** — normalize to PNG on the wire; rebuild `CF_DIB` / `image/png` per-OS at
  paste.
- **Files** — by-reference everywhere; bytes move only on a real paste, and only the
  bytes actually read. Read source bytes on demand and **stream** them through to the
  pasting app — **no destination-side staging copy** (M1 decision; mstsc-style). Per-OS
  mechanism (M0 Finding C, see `PLATFORM-NOTES.md`): **Windows outbound uses
  `CFSTR_FILEDESCRIPTORW` + `CFSTR_FILECONTENTS`** via an `IDataObject` (virtual files —
  a `CF_HDROP` promise is force-materialized at *copy* time by clipboard monitors),
  serving `FILECONTENTS` per-file (and, in M4, per-range) through the lazy-render
  bridge; **Linux uses `text/uri-list`** pointing at a **FUSE mount** that streams on
  read (M-Linux). No staging dir on either OS. A file group is carried in the offer as
  a manifest of `{ rel_path, size }` entries; contents are fetched by file index.
- **Rich text (HTML)** — `text/html` ↔ `CF_HTML` codec (the byte-offset preamble).
  Owning milestone `[CRYSTALLIZE: html milestone]`.
- **RTF** — **Phase 2**.

## 10. Discovery

- **v1:** explicit endpoints — peers listed by IP in config (Vox-style). Trusted LAN,
  no auth (pairing/identity/PSK is Phase 2).
- **Phase 2:** mDNS auto-discovery, or hybrid (mDNS discover + identity-key trust gate).
