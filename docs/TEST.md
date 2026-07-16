# TEST.md — the end-to-end manual test flow

The automated tests drive core and the platform interfaces **in-process**. The real OS —
Explorer, the clipboard historian, another machine's COM apartment — drives them from
*outside*, with its own copy engine, read sizes, and threading. That gap is why this file
exists: the unit tests prove the pieces *should* work; this proves the whole flow *does*, on
real machines.

**This is a living document.** It covers the whole cross-machine flow and **grows as
features land** — each milestone adds its checks here and updates the ones its work changed.
Run it before calling a milestone done, and whenever a change could plausibly affect the
real-OS behaviour. Record the outcome in the table at the bottom.

Sections are tagged with the milestone that introduced or last changed them, so it is clear
what a given check is exercising.

## Why two machines

**Not two processes on one Windows box.** The clipboard is per-session, so two nodes in one
session feed each other: B's `set_promise` fires A's `WM_CLIPBOARDUPDATE`, A captures it as a
*local* copy (its owner check only skips its own window), re-offers it, and round it goes.
Echo suppression is by `origin_id` and does not help — A genuinely believes a new local copy
happened.

Use two physical machines, two VMs, or two separate login sessions (each has its own
clipboard). Same LAN, listening port open between them. **Test both Wi-Fi and wired** where
throughput matters (M3 found Wi-Fi is often the real ceiling, not the software).

## Setup

```
cargo build --release          # on each box, or build once and copy target\release\clipline.exe
set RUST_LOG=clipline=debug,clipline_core=debug   # several checks are unobservable without it
```

Box **A**: `clipline.exe --port 9860`
Box **B**: `clipline.exe --port 9860 --peer <A-ip>:9860`

Only one side needs `--peer`: inbound from unlisted peers is accepted (`SPEC.md` §10). Expect
`peer connected` on both.

Contents are **never** logged (`CONVENTIONS.md`) — sizes, seqs, origins only. Every check
below is verified by observing behaviour, not by reading data out of logs.

> The runner (`clipline up`-style `--port`/`--peer`) is a **dev harness**, not the product
> CLI. `clipline up` + status + tray + packaging are **M6**; update this setup section when
> they land.

---

## Clipboard sync — the lazy paste (M3)

The end-to-end lazy paste: copy on one machine, paste on another, bytes pulled on demand.

### 1. Text round-trip
Copy text in Notepad on A → paste in Notepad on B.
* **Expect:** the text appears.
* **Proves:** the whole chain — capture, offer, promise, fetch, render.

### 2. Laziness — with a documented Windows exception for text/images
**Needs `RUST_LOG=…=debug` on A** (`serving fetch` is a debug log).
* **Files stay lazy:** A logs `serving fetch` for a *file* only when B **pastes** (check 4).
* **Text/images do NOT, on Windows** — expected, not a bug. `PLATFORM-NOTES.md` Finding B:
  any clipboard listener on B (Clipboard History, a manager like FDM/CopyQ) force-renders a
  delayed text/image promise within ~2 ms, before any paste. Bounding this for *large*
  text/images is M5; the laziness that matters — large files — holds.

### 3. Image round-trip (+ duplicate suppression)
Copy an image on A → paste on B. **Try both** Paint *and* **Win+Shift+S** (different DIB
layouts).
* **Expect:** both appear, alpha and all; exactly **one** `remote offer` per copy on B.
* **Proves:** PNG-on-wire (`SPEC.md` §9); the `BI_BITFIELDS`/V5 decode path (screenshots);
  content-hash dedup (a source that writes the clipboard twice yields one offer). A
  screenshot that syncs nothing → `local copy has no transferable format; ignoring` on A =
  DIB rejected; two `remote offer`s = dedup regressed.

### 4. A large file
Copy a **≥1 GB** file in Explorer on A → paste in Explorer on B. Test **wired and Wi-Fi**.
* **Expect:** instant copy on A; on B Explorer stays responsive, a copy dialog shows, and
  progress climbs. Throughput reaches the **link** speed (wired: fast and smooth; Wi-Fi:
  link-limited, ~3 MB/s, chunky progress — that stutter is the link, not a bug).
* **Proves:** the pulling `IStream` (bytes arrive as read, not buffered whole);
  `IDataObjectAsyncCapability` (extraction on a background thread, or Explorer freezes with no
  dialog — look for `shell started an async paste` on B); window read-ahead (throughput off
  the round-trip floor).
* **Also watch:** A's memory must not grow by the file size — nothing staged, nothing
  buffered whole (decision #8).

### 4b. A folder (incl. empty directories)
Copy a **folder** with nested files *and at least one empty subfolder* on A → paste on B.
* **Expect:** the whole tree appears, subdirectories and empty folders included. A logs
  `captured local copy` with `files=N` (files + empty dirs, not 1).
* **Proves:** the recursive `CF_HDROP` walk; empty-directory entries
  (`FILE_ATTRIBUTE_DIRECTORY`). Symlinks inside are skipped by design.

### 5. Cancel mid-transfer
Start check 4, hit **Cancel** in Explorer's dialog.
* **Expect:** stops promptly; A logs `peer ended job` then `releasing capture (unpinned)`.
* **Proves:** `EndJob` through a real consumer (Explorer releasing the stream ends the job) —
  depends on Explorer's `ReleaseStgMedium`, unreachable by any in-process test.

### 6. A new copy does not break an in-flight paste (`SPEC.md` §6 row 2)
Start a large-file paste on B; **while it runs**, copy something else on A.
* **Expect:** the transfer completes with the **original** file; B's head moves to the newer
  copy for the *next* paste.
* **Proves:** decision #6 + the origin pin — never a paste-time substitution (`SPEC.md` §5).

### 7. Two pastes at once (`SPEC.md` §6 row 3)
Copy file 1 on A, paste on B; while it transfers, copy file 2 on A and paste that too.
* **Expect:** **both** complete (may serialise — decision #7's serial bulk plane).
* **Proves:** the case impossible before the pulling `IStream` (whole-file buffering wedged
  the pump).

### 8. Graceful failure
Start a large-file paste on B, then kill A's process.
* **Expect:** the paste fails, Explorer reports an error, **nothing hangs**.
* **Proves:** `SPEC.md` §5's unavoidable race, failing the way `CONVENTIONS.md` requires.

### 9. Files stay by-reference
After check 4, look at A: no staging copy anywhere, no temp-dir growth.
* **Proves:** decision #8 — no `materialize_files`, no staging, either side.

---

## Not yet covered (add checks here as these land)

- **Reconciliation on peer drop** — M4.
- **Capture modes / eager threshold, safety levels, throttling** — M5.
- **`clipline up` / status / toggles / tray** — M6.
- **HTML / rich text** — M7.
- **Linux (Wayland/X11) as either end** — M-Linux.

## Known limits (by design — not bugs)

- **Text/large images are eager on Windows** (Finding B, above) — link-force-rendered at copy
  by clipboard managers. Bounded in M5.
- **Virtual-file sources** (copying *out of* a zip; apps offering `FILEDESCRIPTOR` not
  `CF_HDROP`), **file metadata** (timestamps, attributes, permissions, ADS), **symlinks in a
  tree**, and **trees deeper than 64** are not transferred — Phase 2 (`PLAN.md`).
- **No auth.** TLS is confidentiality only; anyone who reaches the port is a peer (decision
  #10; pairing is Phase 2). Trusted LAN only — do not test with real secrets.

## Result

| Milestone | Check | Pass? | Notes |
|---|---|---|---|
| M3 | 1 text | | |
| M3 | 2 laziness | | |
| M3 | 3 image (+ dedup) | | |
| M3 | 4 large file (wired + Wi-Fi) | | |
| M3 | 4b folder (+ empty dirs) | | |
| M3 | 5 cancel | | |
| M3 | 6 copy during transfer | | |
| M3 | 7 two pastes | | |
| M3 | 8 graceful failure | | |
| M3 | 9 by-reference | | |

Date / boxes / build:
