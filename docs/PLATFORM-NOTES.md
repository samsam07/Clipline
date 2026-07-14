# PLATFORM-NOTES.md — per-OS clipboard mechanics

Empirical findings from the milestone spikes about how each OS actually behaves,
feeding the `[CRYSTALLIZE: platform milestone]` details in `ARCHITECTURE.md` /
`SPEC.md`. These are **observations + proposed directions**, not locked decisions —
anything that touches a locked decision is flagged for the human to rule on at its
owning milestone. Each finding is **cross-validated against authoritative sources**
(vendor docs / project source) — see the Sources list at the end.

---

## Windows (from the M0a `on_render` spike)

**Environment when measured:** Windows 10 Pro 19045. A third-party clipboard
manager — **Free Download Manager (`fdm.exe`)** — was running and monitoring the
clipboard, plus the OS **Clipboard User Service (`cbdhsvc`)**. Findings below
depend on *some* clipboard listener being present; that is the realistic case, not
the exception.

### Finding A — a failed render is re-triggered once per consumer (not an OS retry)
When the clipboard owner returns from `WM_RENDERFORMAT` **without** calling
`SetClipboardData` (our timeout path), we observed `WM_RENDERFORMAT` for the same
format fire **three** times (806 → 1612 → 2416 ms at an 800 ms render timeout) before
`GetClipboardData` returned NULL.

*Cross-validated interpretation:* Windows itself does **not** retry — the clipboard
manager waits on a **single, non-configurable ~30 s** timeout per render before
abandoning it and returning NULL (see Sources: Old New Thing / `WM_RENDERFORMAT`
docs). The "3×" we saw is therefore **three independent consumers** (`fdm.exe`,
`cbdhsvc`, our own paste) each re-triggering the render, because a **failed render is
never cached** — so every listener that asks re-fires it. A *successful* render
populates real bytes once and all later readers hit the cache (confirmed: happy-path
runs showed exactly one render per format).

*Consequence / direction (M1/M3):* cache the *failure* too — track already-failed
`{origin_id, seq, format}` and **fast-fail** subsequent forced renders instead of
re-running the fetch. Our own on-render timeout can be generous (Windows tolerates up
to ~30 s), unlike Wayland (Finding E).

### Finding B — delayed **text** is force-rendered immediately, before any paste
The instant we claim the clipboard with a delayed-render `CF_UNICODETEXT` promise
(no bytes), a clipboard listener calls `GetClipboardData(CF_UNICODETEXT)` within
~2 ms — **before any human paste** — forcing our render. Proven up to **1 MB** of
text. The requester was identified (via `GetOpenClipboardWindow` →
`GetWindowThreadProcessId` → `QueryFullProcessImageNameW`) as `fdm.exe`.

*Consequence:* in `HeadCapture` (the "lazy" default), a remote copy of text
triggers an **immediate full network fetch** on every peer that has any clipboard
listener — even if nobody ever pastes. "Copy is instant, bytes move only on paste"
does **not** hold for text on Windows.

*Mitigations tested:*
- `ExcludeClipboardContentFromMonitorProcessing` + `CanIncludeInClipboardHistory=0`
  + `CanUploadToCloudClipboard=0` — the well-known "keep out of history/cloud"
  markers (also relevant to the SPEC §7 safety layer). **Did not stop the forced
  render** by `fdm.exe`. *Cross-validated:* these are documented Microsoft formats
  used by password managers, but honoring them is **voluntary** — the OS history/cloud
  service and well-behaved managers (e.g. CopyQ) respect them; arbitrary third-party
  managers (FDM here) do not. So the markers reduce *which* listeners grab us, but
  don't eliminate forced reads. (There is also a legacy `"Clipboard Viewer Ignore"`
  format with the same voluntary status.)

*Direction (owning milestone M5 eager-threshold / capture-mode):* accept that
text/images (small historizable formats) are effectively **eager on copy** on
Windows; that already matches the eager-threshold design for *small* payloads. For
text **above** the eager threshold (the case flagged by the human), we cannot make
it truly lazy while a clipboard manager is present. Bound the damage:
1. **Coalesce to one fetch** — serve the forced render once, cache locally, serve
   subsequent readers from cache (no repeated network pulls).
2. Route the forced fetch through the **bulk-plane throttle** so it stays polite.
3. *Optional, fragile:* a **requester heuristic** — we can name the requesting
   process; serve background monitors a short placeholder and the real bytes only
   to a genuine foreground paste. Heuristic, opt-in, not v1-critical.
4. Still set the exclusion markers — they remove the OS history/cloud as a grabber
   and serve the safety layer, even though they don't bind third-party managers.

### Finding C — `CF_HDROP` promise is force-**materialized** at copy time ⚠️ (touches locked decision #8)
The same listener also force-reads a delayed **`CF_HDROP`** promise at ~2 ms. Unlike
text, satisfying a `CF_HDROP` render means producing **real local file paths** — so
in the real design it forces `materialize_files`, i.e. **fetching the file bytes
from the origin at copy time**. That breaks "copying a 4 GB file is instant" on
Windows whenever a clipboard manager is present.

This is why RDP/`mstsc` does **not** use `CF_HDROP` for redirected files. It uses
the shell virtual-file model: **`CFSTR_FILEDESCRIPTORW`** (a `FILEGROUPDESCRIPTOR`
of names/sizes — cheap metadata, safe to force-render) + **`CFSTR_FILECONTENTS`**
(per-file bytes, pulled via `IStream`/`HGLOBAL` **only when the destination
actually reads a file**). A monitor reading the descriptor gets metadata only;
contents stay lazy until a real paste. *Cross-validated:* Microsoft's Shell
Clipboard Formats docs call this pair "the preferred way to transfer Shell objects
that are not stored as file-system files," and state that `IDataObject::GetData`
supports **delayed rendering** of both formats (per-file via `FORMATETC.lindex`).

*Prototype result (validated in the spike):* an `IDataObject` advertising
`FileGroupDescriptorW` + `FileContents`, `OleSetClipboard`'d, was **not touched at
all** by the clipboard monitor over a multi-second observation — `fdm.exe` issued
**zero** `GetData` calls (vs. grabbing `CF_HDROP`/`CF_UNICODETEXT` at ~2 ms). An
in-process pull confirmed the two-tier laziness: `FileGroupDescriptorW` served
instantly with **no fetch**; `FileContents` served only on request, through the
same block→async-fetch→timeout bridge. Conclusion: **outbound Windows file
promises should use `CFSTR_FILEDESCRIPTORW` + `CFSTR_FILECONTENTS`, not
`CF_HDROP`.** Incoming file bytes are **streamed** through the `FileContents` fetch
straight to the pasting app — **no local staging copy** (M1 decision; mstsc-style, which
serves `FILECONTENTS` ranges on demand rather than pre-copying the file).

*Tension with a locked decision:* CLAUDE.md locked decision **#8** names
`CF_HDROP` / `text/uri-list` as the by-reference file mechanism, with the
destination "materializing local copies and advertising local refs **at paste**."
Finding C shows `CF_HDROP` forces materialization **at copy** (under monitors),
contradicting the "at paste" goal. Honoring #8's *intent* (lazy, materialize at
paste) on Windows requires `CFSTR_FILEDESCRIPTORW` + `CFSTR_FILECONTENTS` instead of
`CF_HDROP` for outbound promises. **Resolved (amended twice):** decision #8 now (a)
adopts the virtual-file mechanism for Windows outbound (keeping `text/uri-list` for
Linux), and (b) drops the "materialize local copies / advertise local refs" step in
favor of **streaming** — `FILECONTENTS` bytes flow origin→pasting-app on demand with
**no staging dir** (mstsc-style; M1 decision). This also removed `materialize_files`
from the adapter trait: file contents ride the render bridge keyed by `FormatReq.file_idx`
(a file group is carried in `Offer.files`). Implementation is owned by **M1**.

*Note:* the Linux analog (`text/uri-list` referencing real paths) has the same
shape, and Linux lacks a standard virtual-file-contents clipboard mechanism —
**M0b must check whether a Wayland clipboard manager (Klipper/GPaste/wl-clip-persist)
force-reads the selection the same way.**

### What the M0a bridge itself proved (unaffected by A/B/C)
The `on_render` sync↔async inversion **works**: own the clipboard with delayed
promises (no bytes), block the platform-affine pump thread while a tokio task
produces bytes, supply them via `SetClipboardData`, and time out into a clean
paste-fail. Verified for both `CF_UNICODETEXT` and `CF_HDROP`, happy-path and
timeout, in-process and via real-app paste (Notepad/Explorer). Findings A–C are
about *which formats stay lazy* and *when the bridge is triggered*, not about
whether the bridge holds.

---

## Linux — KDE Plasma / Wayland (from the M0b `on_render` spike)

**Environment when measured:** Fedora 43, KWin 6.6.4, live Plasma Wayland session
driven headlessly over ssh (`WAYLAND_DISPLAY=wayland-0`, `XDG_RUNTIME_DIR=/run/user/1000`).
The compositor advertises **`ext_data_control_manager_v1` (v1)** — the newer *ext*
protocol; the older `zwlr_data_control_manager_v1` is **not** advertised, so bind
`ext-data-control` (crate: `wayland-protocols` `staging` feature). Plasma's clipboard
manager (Klipper) is integrated into `plasmashell`.

### What the M0b bridge proved
Own the selection via an `ext_data_control_source_v1`, advertise MIME types
(`text/plain(;charset=utf-8)`, `text/uri-list`) with **no bytes**. On paste the
compositor sends `send(mime, fd)`; we produce bytes lazily and write the fd. Verified:
lazy render reaches a **real app** (Kate showed the text at a 200 ms produce delay);
a produce that exceeds our own timeout closes the fd empty → **empty paste, no hang**
(graceful fail). Both `text/plain` and `text/uri-list` served. **Caveat:** the "3 s
delay still works" result was a *data-control consumer* (`wl-paste`); regular apps go
through a different path with a latency ceiling — see Finding E.

### Finding E — regular Qt apps abort the paste read after ~1 s ⚠️ (pinned)
Two consumer paths behave differently when reading a slow data-control *source*:
- **data-control consumers** (`wl-paste`, clipboard managers): **no** timeout — a 3 s
  produce delay still delivered.
- **regular apps** (`wl_data_device` — Kate, Dolphin, etc.): a hard read timeout.
  At a 200 ms produce delay Kate pasted the text; at **1500 ms it pasted nothing**
  (cleanly — no hang).

*Pinned via source:* the ceiling is **~1 second, and it lives in the toolkit (Qt), not
KWin.** `QWaylandMimeData::readData()` retries reading the pipe **1000× with
`usleep(1000)` ≈ 1 s**, then gives up ("QWaylandDataOffer: timeout reading from pipe").
That exactly matches 200 ms OK / 1500 ms fail. (GTK's timeout may differ; data-control
consumers have none.) Also note the `wl_data_device` selection only reaches a client
with **keyboard focus** (emersion; a security measure) — so this path can only be
tested by a focused GUI app, not headlessly. *Pending empirical confirm* (deferred to
the M-Linux milestone, per Post-M0 sequencing): bracket the boundary in Kate —
`--delay-ms 900` should paste, `--delay-ms 1100` should not.

*Why it matters:* the fetch must beat ~1 s or a paste into a normal Qt app yields
nothing. For Clipline's **LAN + small formats** (text, small images) this is fine —
sub-second. It bites two cases, both **M-Linux/design** problems (this is the Wayland
paste path; the Windows adapter is M1):
1. **Large inline payloads** (big text/image served on the fd): can't finish within
   ~1 s → serve them **eagerly** (pre-fetched, matching the SPEC §3 eager path), or
   accept graceful-empty on very large items.
2. **Files** (`text/uri-list`): the URI list serves instantly, but the referenced files
   must *exist* when read — materialize-then-serve reintroduces the fetch into the ~1 s
   budget. **Decoupled fix (industry-validated):** serve URIs immediately pointing at a
   **FUSE-backed staging path**, so the byte transfer happens as ordinary file I/O (no
   clipboard timeout) when the destination reads the files. *Cross-validated:* this is
   exactly what **FreeRDP** does — its clipboard file redirection presents remote files
   as **FUSE** virtual files on Linux/Wayland (`CliprdrFuse…`), for the same
   lazy-file-over-a-network problem. Building the FUSE layer is **M-Linux** (FUSE is
   Linux-only; the Windows outbound file path uses `CFSTR_FILEDESCRIPTORW` +
   `CFSTR_FILECONTENTS` and is M1).

*Not a go/no-go failure:* the bridge holds — lazy render reaches real apps, and
exceeding the budget fails gracefully (empty paste, no hang). E is a constraint to
design around (eager small formats; FUSE-backed files), not a broken mechanism.

### Finding D — Wayland fd-serve must be NON-BLOCKING ⚠️ (adapter-trait implication)
Unlike Windows `WM_RENDERFORMAT` (which *requires* blocking the clipboard-owner
thread until `SetClipboardData`, tolerated up to ~30 s), the data-control `send` must
**not** block the wayland dispatch thread. A blocking handler (our first cut) made
**every** write fail with `Broken pipe`: concurrent reads serialize behind the block,
and readers time out. The fix: hand the fd to an async task and write it whenever the
fetch completes, while the dispatch loop keeps running. *Cross-validated:* the
canonical Wayland clipboard reference (emersion) explicitly warns that blocking writes
in the `send` handler "could stall the Wayland event loop… A real client would perform
non-blocking writes instead." (Also: writing to a closed pipe raises `SIGPIPE`, which
terminates naive Qt sources — QTBUG-57202; Rust masks `SIGPIPE` by default and surfaces
`EPIPE`/`Broken pipe`, which we handle. The real adapter must keep that guarantee.)

**Implication for the `ClipboardAdapter::on_render` contract (owned by M1):** the two
OSes impose *opposite* threading requirements — Windows must block its platform
thread; Wayland must not block its dispatch thread. So `on_render` cannot be a
synchronous `Fn(FormatReq) -> RenderResult` callback run on the platform thread (the
ARCHITECTURE.md sketch). It must be **deferred/async**: the adapter emits a request
and receives the bytes via a channel/future. The Windows adapter then *internally*
blocks its pump thread awaiting that future; the Wayland adapter *internally* writes
the fd from a task. This keeps core's contract identical across both. **M0 validated
that the trait must be expressible this way — exactly its stated purpose.**

### Finding B/C analog on Wayland — Plasma force-reads at copy time (gentler)
The instant we `set_selection`, `plasmashell` (Klipper) issues `send` for our text
types at ~5 ms — **before any paste** — the Wayland twin of the Windows Finding B. But
it self-limits: the read is abandoned once our write is slower than the toolkit's
timeout (our forced-read writes broke at ≤1.5 s, consistent with the ~1 s Qt
`readData` timeout of Finding E, since Klipper lives in Qt-based `plasmashell`). So a
slow/large lazy payload's forced read simply **breaks the pipe and is dropped** (it
just isn't cached in history), whereas a *real* paste of a *fast* source succeeds.
Below ~200–500 ms the forced reads also succeed (small/fast content caches into
history, matching the eager path). This is **gentler than Windows**, where the
monitor's read blocks up to ~30 s and completes, forcing a full fetch.

### Linux file-by-reference — resolved direction (implementation is M-Linux)
`text/uri-list` carries **file paths/URIs**; contents live **on disk**, not in the
clipboard — there is **no `CFSTR_FILECONTENTS` analog** on Wayland. So the referenced
files must exist when the uri-list is read. The resolved strategy (see Finding E):
**serve the uri-list immediately with URIs under a FUSE mount** (a virtual filesystem,
**not** a pre-copied staging dir), and let the actual bytes stream in as ordinary file
I/O when the destination reads them — no clipboard-timeout involvement. This is the
FreeRDP-proven approach. Building the FUSE layer + its mount lifecycle is **M-Linux**
(deferred with the rest of the Linux adapter; see PLAN.md "Post-M0 sequencing"). Windows
needs **no staging dir at all** — `FILECONTENTS` streams per-file/per-range straight to
the pasting app via the `IDataObject` (M1 decision; mstsc-style).

---

## Sources (cross-validation)

- Microsoft — *Shell Clipboard Formats* (`CFSTR_FILEDESCRIPTOR`/`CFSTR_FILECONTENTS`,
  delayed rendering via `IDataObject::GetData`, per-file `lindex`):
  https://learn.microsoft.com/en-us/windows/win32/shell/clipboard
- Microsoft — *`WM_RENDERFORMAT`* / *Clipboard Operations* / *`SetClipboardData`*
  (synchronous, owner must not re-open the clipboard):
  https://learn.microsoft.com/en-us/windows/win32/dataxchg/wm-renderformat
- The Old New Thing — the delayed-render wait is a single **~30 s** timeout:
  https://devblogs.microsoft.com/oldnewthing/20220609-00/?p=106731
- Microsoft — *Clipboard Formats* (`CanIncludeInClipboardHistory` DWORD, exclusion
  markers): https://learn.microsoft.com/en-us/windows/win32/dataxchg/clipboard-formats
- CopyQ — exclusion markers are honored *voluntarily* by managers (Security docs /
  issue #2282, "Clipboard Viewer Ignore"): https://copyq.readthedocs.io/en/latest/security.html
- emersion — *Wayland clipboard and drag & drop* (non-blocking `send` writes; selection
  requires keyboard focus): https://emersion.fr/blog/2020/wayland-clipboard-drag-and-drop/
- Qt — `QWaylandMimeData::readData()` ~1 s read timeout (1000×`usleep(1000)`):
  https://github.com/qt/qtwayland/blob/HEAD/src/client/qwaylanddataoffer.cpp ·
  SIGPIPE on source write: https://bugreports.qt.io/browse/QTBUG-57202
- FreeRDP — clipboard file redirection via **FUSE** virtual files on Linux/Wayland:
  https://github.com/FreeRDP/FreeRDP/issues/6727
- KWin — ports clipboard/data-control to `ext-data-control-v1`:
  https://invent.kde.org/plasma/kwin/-/merge_requests/6606
