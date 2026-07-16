# M3 manual gate — the real Explorer paste

M3's automated tests drive `IDataObject`/`IStream` **in-process**. Explorer drives the same
interfaces **from another process, across a COM apartment boundary**, with its own copy
engine and its own read sizes. That difference is the entire reason this document exists:
the tests cannot tell us whether the lazy paste works, only that it *should*.

Run this before calling M3 done. Record the outcome at the bottom.

## Why two machines

**Not two processes on one Windows box.** The clipboard is per-session, so two nodes in one
session feed each other: B's `set_promise` fires A's `WM_CLIPBOARDUPDATE`, A captures it as
a *local* copy (its owner check only skips its own window), re-offers it, and round it goes.
Echo suppression is by `origin_id` and does not help — A genuinely believes a new local copy
happened.

Use two physical machines, two VMs, or two separate login sessions (each has its own
clipboard). Same LAN, and the listening port open between them.

## Setup

```
cargo build --release          # on each box, or build once and copy target\release\clipline.exe
```

Box **A**: `clipline.exe --port 9860`
Box **B**: `clipline.exe --port 9860 --peer <A-ip>:9860`

Only one side needs `--peer`: inbound from unlisted peers is accepted (`SPEC.md` §10).

**Set the log level first** — several checks below are unobservable without it:

```
set RUST_LOG=clipline=debug,clipline_core=debug
```

Expect on both: `peer connected`.

Contents are **never** logged (`CONVENTIONS.md`) — sizes, seqs and origins only. Everything
below is checked by observing behaviour, not by reading data out of logs.

## The checks

Each numbered item states what it proves. If one fails, that is an M3 bug, not a
test-procedure problem.

### 1. Text round-trip

Copy text in Notepad on A → paste in Notepad on B.

* **Expect:** the text appears.
* **Proves:** the whole chain — capture, offer, promise, fetch, render.

### 2. Laziness — with a documented Windows exception for text/images

**Requires `RUST_LOG=...=debug` on A** (`serving fetch` is a debug log).

* **Files stay lazy:** A logs `serving fetch` for a *file* only when B **pastes** (verified
  in check 4). Clipboard managers cannot cheaply force-render a virtual file, so files keep
  the "bytes move on paste" guarantee.
* **Text and images do NOT, on Windows.** `PLATFORM-NOTES.md` Finding B: any clipboard
  listener on B (Windows Clipboard History, or a manager like FDM/CopyQ) force-renders a
  delayed text/image promise within ~2 ms of it landing — **before any paste**. So seeing
  `serving fetch` on A right after you copy text is **expected**, not a bug. It is the OS,
  not us.
* **Why it is acceptable:** text and small images are exactly the payloads the M5
  eager-threshold makes eager anyway. The laziness that matters — large files — holds.
  Bounding this for *large* text/images (coalesce the forced render, cache it) is M5.

To see the pure-lazy path, watch a **file** copy (check 4), not text.

### 3. Image round-trip

Copy an image on A → paste on B. **Try both** Paint *and* **Win+Shift+S**: they produce
different DIB layouts, and only Paint's worked on the first run.

* **Expect:** both appear, alpha and all. And exactly **one** `remote offer` per copy on the
  receiver — not two.
* **Proves:** the PNG-on-wire normalisation (`SPEC.md` §9); for the screenshot, the
  `BI_BITFIELDS`/V5 decode path; and same-origin duplicate suppression (a source app that
  writes the clipboard twice for one image now yields one offer, not two). A screenshot that
  syncs nothing (A logs `local copy has no transferable format; ignoring` at debug) means the
  DIB was rejected again; two `remote offer`s means dedup regressed.

### 4. The big one — a large file

Copy a **≥1 GB** file in Explorer on A → paste in Explorer on B. (Start smaller — 5 MB — if
you want a quick signal.)

* **Expect:** the copy is instant on A. On B, **Explorer stays responsive**, a copy dialog
  appears, and progress climbs.
* **Proves:** two things at once. The pulling `IStream` (M3.5) — bytes arrive as read, not
  buffered whole. And `IDataObjectAsyncCapability`, which is what lets the shell do the
  extraction on a background thread instead of its UI thread.
* **First run failed here:** Explorer froze with no dialog, because the data object did not
  advertise async capability, so the shell extracted synchronously on the UI thread. If it
  freezes again, that fix did not take — check for `shell started an async paste` at debug
  on B. Its absence means the shell ignored the interface and we need a different approach.
* **Also watch:** A's memory. It must not grow by the file size — nothing is staged and
  nothing is buffered whole (locked decision #8).

### 4b. A folder

Copy a **folder** with a few nested files in Explorer on A → paste on B.

* **Expect:** the whole tree appears on B, subdirectories and all. A logs `captured local
  copy` with `files=N` (N = the file count under the folder, not 1).
* **Proves:** the recursive `CF_HDROP` walk (M3 follow-up). Empty subfolders won't appear
  (see Known limits); symlinks inside are skipped.

### 5. Cancel mid-transfer

Start check 4 again and hit **Cancel** in Explorer's copy dialog.

* **Expect:** the transfer stops promptly. A logs `peer ended job` then
  `releasing capture (unpinned)`.
* **Proves:** M3.4's `EndJob` through a real consumer — Explorer releasing the stream is
  what ends the job. This is the one that depends on Explorer's `ReleaseStgMedium`
  behaviour, which no in-process test can confirm.

### 6. A new copy does not break an in-flight paste (`SPEC.md` §6 row 2)

Start a large-file paste on B. **While it runs**, copy something else on A.

* **Expect:** the transfer completes, with the **original** file's contents. B's head then
  points at the newer copy for the *next* paste.
* **Proves:** locked decision #6 + the origin's pin. The file must not be corrupted or
  swapped — never a paste-time substitution (`SPEC.md` §5).

### 7. Two pastes at once (`SPEC.md` §6 row 3)

Copy file 1 on A, paste on B. While it transfers, copy file 2 on A and paste that on B too.

* **Expect:** **both** complete. They may serialise (one waits, then proceeds) — that is
  decision #7's serial bulk plane, and `SPEC.md` §4 explicitly allows it.
* **Proves:** the case that was **impossible before M3.5** — with whole-file buffering the
  pump was wedged for the duration and the second paste could not even be dispatched.

### 8. Graceful failure

Start a large-file paste on B, then kill A's process outright.

* **Expect:** the paste fails and Explorer reports an error. **Nothing hangs** — not
  Explorer, not the shell.
* **Proves:** `SPEC.md` §5's unavoidable race, failing the way `CONVENTIONS.md` requires.

### 9. Files stay by-reference

After check 4 completes, look at A: no staging copy of the file anywhere, and no temp
directory growth.

* **Proves:** locked decision #8 as amended in M1 — there is no `materialize_files` and no
  staging dir, on either side.

## Known limits (not bugs — do not file these)

* **Virtual-file sources are not captured.** Copying *out of* a zip, or from any app that
  offers `FILEDESCRIPTOR` rather than `CF_HDROP`, will not be offered.
* **Empty folders inside a copied tree are not recreated.** The shell rebuilds directories
  from file paths, so a folder containing no files leaves no trace. Symlinks inside a
  copied folder are skipped (loop safety).
* **Large non-file copies are snapshotted at copy time**, which briefly blocks A's pump.
  Forced by decision #6: nothing but a snapshot survives the next copy.
* **Sensitivity hints are ignored** — the safety layer is M5. Do not test with real
  passwords.
* **No auth.** TLS gives confidentiality only; anyone who can reach the port is a peer
  (decision #10; pairing is Phase 2). Trusted LAN only.

## Result

| Check | Pass? | Notes |
|---|---|---|
| 1 text | | |
| 2 laziness | | |
| 3 image (+ dedup) | | |
| 4 large file | | |
| 4b folder | | |
| 5 cancel | | |
| 6 copy during transfer | | |
| 7 two pastes | | |
| 8 graceful failure | | |
| 9 by-reference | | |

Date / boxes / build:
