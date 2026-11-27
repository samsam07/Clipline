//! Linux **inbound files** (M-Linux ML.2): a read-only FUSE mount whose files stream from the
//! origin as the pasting app reads them (locked decision #8 — by-reference, no staging).
//!
//! # Why a filesystem (PLATFORM-NOTES Finding E)
//!
//! A Linux file copy is a `text/uri-list` of paths, and the pasting app opens those paths
//! itself with ordinary file I/O — Clipline is not in that read path the way it is on Windows
//! (`CFSTR_FILECONTENTS`). So the paths must exist and be readable, with bytes that come from
//! the network at read time. A FUSE mount is the unprivileged way to back a path that way, and
//! it also takes the transfer out of Qt's ~1 s clipboard-read budget: the uri-list is served
//! instantly from the manifest, and the bytes move later as plain file reads.
//!
//! # Mount lifecycle (`[CRYSTALLIZE: M-Linux]`, pinned 2026-11-26 — option A)
//!
//! **One mount per process**, at `$XDG_RUNTIME_DIR/clipline/files`, mounted at startup and
//! unmounted on exit. Each remote file offer is its own subtree, `<mount>/<seq>/<rel_path>`.
//! The view is **head-only for new pastes**: the uri-list we serve always names the current
//! head's subtree. Older subtrees stay only while a paste of them is in progress (below).
//!
//! Auto-unmount on SIGKILL is not used: fuser's `AutoUnmount` needs `allow_other`, which a
//! stock `/etc/fuse.conf` refuses. Instead a lock file marks the live owner; a mount whose lock
//! is free at startup is stale (the owner died) and is lazily unmounted then.
//!
//! # Jobs: one per paste of a subtree, not per file (pinned 2026-11-26)
//!
//! Locked decisions #5/#6: a paste must complete even if the head moves on. A file manager
//! copies a folder one file at a time, and between two files nothing is open — so a
//! per-open-file job would let the origin unpin (and our view drop the subtree) mid-folder.
//! So a subtree's first `open` starts **one job** that every read of every file in it shares;
//! the job ends [`PASTE_GRACE`] after the subtree's last file closes with no new open. That
//! is the Linux stand-in for the paste-scoped lock mstsc takes (`Lock Clipboard Data`), which
//! we cannot observe directly: Linux has no "paste finished" signal.
//!
//! # Threading
//!
//! The FUSE session runs one thread, and **no callback blocks**: metadata comes from the
//! manifest (`getattr`/`lookup`/`readdir` never touch the origin), and a `read` is handed to
//! its open file's tokio task, which replies when the bytes arrive. One task per open file
//! keeps that file's reads in kernel order, so the read-ahead window logic stays sequential.

use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant, SystemTime};

use fuser::{
    BackgroundSession, Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags,
    Generation, INodeNo, LockOwner, MountOption, OpenFlags, ReplyAttr, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, Request,
};
use rustix::fs::FlockOperation;
use tokio::sync::{mpsc, oneshot};

use clipline_core::{
    ByteRange, FileEntry, FormatReq, JobId, Mime, Offer, OriginId, RenderRequest, RenderResult, Seq,
};

use super::READAHEAD_WINDOW;

/// How long a pasted subtree's job outlives the last file the pasting app closed in it
/// (pinned with the human, 2026-11-26). It has to bridge every pause *inside* one paste — the
/// gap between two files is milliseconds, but a file manager's "file exists — overwrite?"
/// dialog can sit for minutes. Holding it is cheap: the origin keeps a path list, not bytes.
/// Equal to the origin's job idle backstop (`serve.rs` `JOB_IDLE_TIMEOUT`); longer would buy
/// nothing, since the origin drops a job idle past that anyway.
pub(crate) const PASTE_GRACE: Duration = Duration::from_secs(300);

/// How often expired jobs and subtrees are collected. Precision is irrelevant next to
/// [`PASTE_GRACE`]; this only bounds the lag.
const SWEEP_EVERY: Duration = Duration::from_secs(5);

/// Kernel attribute/entry cache lifetime. Short: the root's listing changes as heads move.
const ATTR_TTL: Duration = Duration::from_secs(1);

/// The mount root's inode (`FUSE_ROOT_ID`).
const ROOT_INO: u64 = 1;

/// What the mount needs from the adapter to fetch bytes: the same render bridge every other
/// destination read uses (core turns each `FormatReq` into a bulk-plane fetch).
#[derive(Clone)]
pub(crate) struct Bridge {
    pub render_tx: mpsc::UnboundedSender<RenderRequest>,
    pub job_end_tx: mpsc::UnboundedSender<JobId>,
    /// Bounds one window fetch, as it bounds one render (`RENDER_TIMEOUT`).
    pub render_timeout: Duration,
    pub rt: tokio::runtime::Handle,
}

type TreeKey = (OriginId, Seq);

/// One remote file offer's subtree.
struct Tree {
    /// The `<seq>` directory under the mount root.
    root_ino: u64,
    name: OsString,
    /// Every inode of this subtree (including `root_ino`), for removal.
    inos: Vec<u64>,
    /// When the offer arrived: the mtime every entry reports (the manifest has none).
    created: SystemTime,
    /// Open file handles into this subtree.
    open: usize,
    /// The paste job, from the first `open` until [`PASTE_GRACE`] after the last `release`.
    job: Option<JobId>,
    /// Last open/release/uri-list serve — what the grace is measured from.
    last_active: Instant,
}

enum NodeKind {
    Dir { children: BTreeMap<OsString, u64> },
    File { file_idx: u32, size: u64 },
}

struct Node {
    /// `None` for the mount root.
    tree: Option<TreeKey>,
    parent: u64,
    kind: NodeKind,
}

#[derive(Default)]
struct Inner {
    /// The offer our clipboard currently promises, when it carries files.
    head: Option<TreeKey>,
    trees: HashMap<TreeKey, Tree>,
    /// Every inode but the root's (the root is synthesized from `root_children`).
    nodes: HashMap<u64, Node>,
    /// `<seq>` name → subtree root inode.
    root_children: BTreeMap<OsString, u64>,
    next_ino: u64,
    handles: HashMap<u64, OpenFile>,
    next_fh: u64,
}

/// One open file: its identity on the wire, and the task serving its reads in order.
struct OpenFile {
    key: TreeKey,
    reads: mpsc::UnboundedSender<ReadOp>,
}

struct ReadOp {
    offset: u64,
    size: u32,
    reply: ReplyData,
}

struct MountState {
    dir: PathBuf,
    bridge: Bridge,
    grace: Duration,
    uid: u32,
    gid: u32,
    started: SystemTime,
    inner: Mutex<Inner>,
}

/// A cheap handle the adapter uses to drive the mount: set the head, serve the uri-list,
/// recognise our own paths.
#[derive(Clone)]
pub(crate) struct FileView(Arc<MountState>);

/// The live mount. Dropping it ends every open job and unmounts.
pub(crate) struct FileMount {
    session: Option<BackgroundSession>,
    view: FileView,
    /// Held for the process lifetime: a free lock means the mount's owner is dead (stale).
    _lock: File,
    /// A per-process fallback mount (`files-<pid>`) removes its directory and lock on exit.
    remove_on_exit: bool,
}

// ---------------------------------------------------------------------------
// Mount lifecycle
// ---------------------------------------------------------------------------

impl FileMount {
    /// Mount at `$XDG_RUNTIME_DIR/clipline/files`. Fails (the caller then refuses inbound
    /// files, as before ML.2) when there is no runtime dir, no `/dev/fuse`, or no
    /// `fusermount3`.
    pub(crate) fn start(bridge: Bridge) -> Result<FileMount, String> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .ok_or_else(|| "XDG_RUNTIME_DIR is not set".to_string())?;
        Self::start_in(
            &PathBuf::from(runtime).join("clipline"),
            bridge,
            PASTE_GRACE,
        )
    }

    /// [`Self::start`] under an explicit base directory and grace (tests use a short one).
    pub(crate) fn start_in(
        base: &Path,
        bridge: Bridge,
        grace: Duration,
    ) -> Result<FileMount, String> {
        std::fs::create_dir_all(base).map_err(|e| format!("create {}: {e}", base.display()))?;
        let _ = std::fs::set_permissions(base, std::fs::Permissions::from_mode(0o700));
        sweep_stale_fallbacks(base);

        let (dir, lock, remove_on_exit) = claim_mount_dir(base)?;
        let state = Arc::new(MountState {
            dir: dir.clone(),
            bridge: bridge.clone(),
            grace,
            uid: rustix::process::getuid().as_raw(),
            gid: rustix::process::getgid().as_raw(),
            started: SystemTime::now(),
            inner: Mutex::new(Inner {
                next_ino: ROOT_INO + 1,
                next_fh: 1,
                ..Inner::default()
            }),
        });

        let mut config = Config::default();
        config.mount_options = vec![
            MountOption::FSName("clipline".into()),
            MountOption::Subtype("clipline".into()),
            // Pasted files are copies out of the mount; nothing may write into it.
            MountOption::RO,
            MountOption::NoDev,
            MountOption::NoSuid,
            // The kernel enforces the modes we report — we never check access ourselves.
            MountOption::DefaultPermissions,
        ];
        // One thread is enough: no callback blocks (see module docs).
        config.n_threads = Some(1);
        let session = fuser::spawn_mount(
            ClipFs {
                st: Arc::clone(&state),
            },
            &dir,
            &config,
        )
        .map_err(|e| format!("mount {}: {e}", dir.display()))?;

        bridge.rt.spawn(sweeper(Arc::downgrade(&state)));
        tracing::info!(dir = %dir.display(), "inbound file mount up");
        Ok(FileMount {
            session: Some(session),
            view: FileView(state),
            _lock: lock,
            remove_on_exit,
        })
    }

    pub(crate) fn view(&self) -> FileView {
        self.view.clone()
    }
}

impl Drop for FileMount {
    fn drop(&mut self) {
        // Leaving: tell every origin we are done, so they unpin now rather than at their
        // idle backstop.
        {
            let mut inner = self.view.0.inner.lock().expect("mount lock");
            for tree in inner.trees.values_mut() {
                if let Some(job) = tree.job.take() {
                    let _ = self.view.0.bridge.job_end_tx.send(job);
                }
            }
        }
        let dir = self.view.0.dir.clone();
        if let Some(session) = self.session.take() {
            if let Err(e) = session.umount_and_join() {
                tracing::warn!(dir = %dir.display(), error = %e, "unmounting the inbound file mount");
            }
        }
        if self.remove_on_exit {
            let _ = std::fs::remove_dir(&dir);
            let _ = std::fs::remove_file(lock_path(&dir));
        }
    }
}

fn lock_path(dir: &Path) -> PathBuf {
    let mut name = dir.file_name().unwrap_or_default().to_os_string();
    name.push(".lock");
    dir.with_file_name(name)
}

/// Take `files` if no live process holds it, else a per-process `files-<pid>` (a second
/// node in the same session — the TEST.md one-session setup). Clears a stale mount first.
fn claim_mount_dir(base: &Path) -> Result<(PathBuf, File, bool), String> {
    let candidates = [
        ("files".to_string(), false),
        (format!("files-{}", std::process::id()), true),
    ];
    for (name, per_process) in candidates {
        let dir = base.join(&name);
        let Some(lock) = try_lock(&lock_path(&dir))? else {
            tracing::debug!(dir = %dir.display(), "mount dir held by a live clipline; trying the next");
            continue;
        };
        unmount_if_mounted(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        return Ok((dir, lock, per_process));
    }
    Err("no free mount directory".into())
}

/// `Some(lock)` if we now hold `path`'s exclusive lock; `None` if a live process does.
fn try_lock(path: &Path) -> Result<Option<File>, String> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|e| format!("open {}: {e}", path.display()))?;
    match rustix::fs::flock(&file, FlockOperation::NonBlockingLockExclusive) {
        Ok(()) => Ok(Some(file)),
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(None),
        Err(e) => Err(format!("lock {}: {e}", path.display())),
    }
}

/// Clear `files-<pid>` fallbacks left by dead processes (their lock is free).
fn sweep_stale_fallbacks(base: &Path) {
    let Ok(rd) = std::fs::read_dir(base) else {
        return;
    };
    for entry in rd.flatten() {
        let name = entry.file_name();
        let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".lock")) else {
            continue;
        };
        if !stem.starts_with("files-") {
            continue;
        }
        let dir = base.join(stem);
        if let Ok(Some(_lock)) = try_lock(&entry.path()) {
            unmount_if_mounted(&dir);
            let _ = std::fs::remove_dir(&dir);
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// A mount left by a process that died (SIGKILL, crash) answers every access with ENOTCONN
/// until unmounted. Lazy, so a straggling reader cannot make it fail.
fn unmount_if_mounted(dir: &Path) {
    let Ok(info) = std::fs::read("/proc/self/mountinfo") else {
        return;
    };
    if !mount_points(&info).iter().any(|p| p == dir) {
        return;
    }
    tracing::info!(dir = %dir.display(), "clearing a stale inbound file mount");
    match std::process::Command::new("fusermount3")
        .arg("-u")
        .arg("-z")
        .arg(dir)
        .status()
    {
        Ok(s) if s.success() => {}
        Ok(s) => tracing::warn!(dir = %dir.display(), status = %s, "fusermount3 -u failed"),
        Err(e) => tracing::warn!(dir = %dir.display(), error = %e, "could not run fusermount3"),
    }
}

/// The mount points in a `/proc/self/mountinfo` (field 5, octal escapes like `\040` decoded).
fn mount_points(info: &[u8]) -> Vec<PathBuf> {
    info.split(|&b| b == b'\n')
        .filter_map(|line| line.split(|&b| b == b' ').nth(4))
        .map(|raw| PathBuf::from(OsStr::from_bytes(&unescape_octal(raw))))
        .collect()
}

fn unescape_octal(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'\\'
            && i + 3 < raw.len()
            && raw[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let v = (raw[i + 1] - b'0') * 64 + (raw[i + 2] - b'0') * 8 + (raw[i + 3] - b'0');
            out.push(v);
            i += 4;
        } else {
            out.push(raw[i]);
            i += 1;
        }
    }
    out
}

/// Collect expired jobs and subtrees every [`SWEEP_EVERY`]; ends with the mount.
async fn sweeper(state: Weak<MountState>) {
    let mut tick = tokio::time::interval(SWEEP_EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let Some(st) = state.upgrade() else {
            return;
        };
        st.sweep(Instant::now());
    }
}

// ---------------------------------------------------------------------------
// The view the adapter drives
// ---------------------------------------------------------------------------

impl FileView {
    /// The clipboard head changed. `Some(offer)` with files makes its subtree the one new
    /// pastes see; anything else (a text head, a local copy) leaves no file head. Either way
    /// the previous head's subtree only *stops being the head* — it lingers while pasted.
    pub(crate) fn set_head(&self, offer: Option<&Offer>) {
        let mut inner = self.0.inner.lock().expect("mount lock");
        let offer = offer.filter(|o| !o.files.is_empty());
        inner.head = offer.map(|o| (o.origin_id, o.seq));
        if let Some(o) = offer {
            let key = (o.origin_id, o.seq);
            if let Some(tree) = inner.trees.get_mut(&key) {
                tree.last_active = Instant::now();
            } else {
                add_tree(&mut inner, key, &o.files);
            }
        }
    }

    /// The `text/uri-list` for the subtree of `key`: one `file://` URI per top-level entry,
    /// CRLF-terminated (RFC 2483). Synthesized here, never fetched — the origin holds no
    /// uri-list bytes. Counts as activity: a paste is about to open these paths.
    pub(crate) fn uri_list(&self, key: (OriginId, Seq)) -> Option<Vec<u8>> {
        let mut inner = self.0.inner.lock().expect("mount lock");
        let root_ino = {
            let tree = inner.trees.get_mut(&key)?;
            tree.last_active = Instant::now();
            tree.root_ino
        };
        let root_dir = self.0.dir.join(&inner.trees[&key].name);
        let Some(Node {
            kind: NodeKind::Dir { children },
            ..
        }) = inner.nodes.get(&root_ino)
        else {
            return None;
        };
        let mut out = Vec::new();
        for name in children.keys() {
            out.extend_from_slice(b"file://");
            out.extend_from_slice(&percent_encode_path(
                root_dir.join(name).as_os_str().as_bytes(),
            ));
            out.extend_from_slice(b"\r\n");
        }
        Some(out)
    }

    /// Whether `path` lies in our mount — a captured uri-list naming one is our own promise
    /// re-published (a clipboard manager), never a user copy to offer back to the mesh.
    pub(crate) fn is_ours(&self, path: &Path) -> bool {
        path.starts_with(&self.0.dir)
    }
}

/// Build a subtree from a manifest. Names come off the wire, so each path component is
/// checked here: empty, `.` and `..` components are dropped (a hostile manifest must not be
/// able to name anything outside its subtree — though in a virtual namespace there is
/// nothing to escape *to*, a `..` entry would still confuse a pasting app).
fn add_tree(inner: &mut Inner, key: TreeKey, files: &[FileEntry]) {
    let name = OsString::from(key.1 .0.to_string());
    let root_ino = alloc_ino(inner);
    let mut inos = vec![root_ino];
    inner.nodes.insert(
        root_ino,
        Node {
            tree: Some(key),
            parent: ROOT_INO,
            kind: NodeKind::Dir {
                children: BTreeMap::new(),
            },
        },
    );

    for (idx, entry) in files.iter().enumerate() {
        let parts: Vec<&str> = entry
            .rel_path
            .split('/')
            .filter(|p| !p.is_empty() && *p != "." && *p != "..")
            .collect();
        let Some((leaf, dirs)) = parts.split_last() else {
            continue;
        };
        let mut parent = root_ino;
        let mut ok = true;
        for d in dirs {
            match child_of(inner, parent, d) {
                Some(ino) if matches!(inner.nodes[&ino].kind, NodeKind::Dir { .. }) => parent = ino,
                Some(_) => {
                    ok = false; // a file already holds this name; the manifest is inconsistent
                    break;
                }
                None => {
                    let ino = alloc_ino(inner);
                    inos.push(ino);
                    insert_child(
                        inner,
                        parent,
                        d,
                        ino,
                        key,
                        NodeKind::Dir {
                            children: BTreeMap::new(),
                        },
                    );
                    parent = ino;
                }
            }
        }
        if !ok || child_of(inner, parent, leaf).is_some() {
            continue; // duplicate path: the first entry wins
        }
        let kind = if entry.is_dir {
            NodeKind::Dir {
                children: BTreeMap::new(),
            }
        } else {
            NodeKind::File {
                file_idx: idx as u32,
                size: entry.size,
            }
        };
        let ino = alloc_ino(inner);
        inos.push(ino);
        insert_child(inner, parent, leaf, ino, key, kind);
    }

    // Two origins can share a seq (a true tie, broken by origin_id — decision #3). The newer
    // subtree takes the name; the other keeps serving handles already open on it.
    inner.root_children.insert(name.clone(), root_ino);
    inner.trees.insert(
        key,
        Tree {
            root_ino,
            name,
            inos,
            created: SystemTime::now(),
            open: 0,
            job: None,
            last_active: Instant::now(),
        },
    );
}

fn alloc_ino(inner: &mut Inner) -> u64 {
    let ino = inner.next_ino;
    inner.next_ino += 1;
    ino
}

fn child_of(inner: &Inner, parent: u64, name: &str) -> Option<u64> {
    match &inner.nodes.get(&parent)?.kind {
        NodeKind::Dir { children } => children.get(OsStr::new(name)).copied(),
        NodeKind::File { .. } => None,
    }
}

fn insert_child(
    inner: &mut Inner,
    parent: u64,
    name: &str,
    ino: u64,
    key: TreeKey,
    kind: NodeKind,
) {
    inner.nodes.insert(
        ino,
        Node {
            tree: Some(key),
            parent,
            kind,
        },
    );
    if let Some(Node {
        kind: NodeKind::Dir { children },
        ..
    }) = inner.nodes.get_mut(&parent)
    {
        children.insert(OsString::from(name), ino);
    }
}

/// Percent-encode an absolute path for a `file://` URI: everything but RFC 3986 unreserved
/// characters and `/`.
fn percent_encode_path(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for &b in bytes {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/') {
            out.push(b);
        } else {
            out.extend_from_slice(format!("%{b:02X}").as_bytes());
        }
    }
    out
}

impl MountState {
    /// End jobs whose grace has run out, and drop subtrees that are neither the head nor
    /// in use.
    fn sweep(&self, now: Instant) {
        let mut inner = self.inner.lock().expect("mount lock");
        let head = inner.head;
        let mut gone = Vec::new();
        for (key, tree) in inner.trees.iter_mut() {
            let idle =
                tree.open == 0 && now.saturating_duration_since(tree.last_active) >= self.grace;
            if !idle {
                continue;
            }
            if let Some(job) = tree.job.take() {
                tracing::debug!(
                    job = job.0,
                    seq = key.1 .0,
                    "paste idle past the grace; ending its job"
                );
                let _ = self.bridge.job_end_tx.send(job);
            }
            if head != Some(*key) {
                gone.push(*key);
            }
        }
        for key in gone {
            if let Some(tree) = inner.trees.remove(&key) {
                for ino in &tree.inos {
                    inner.nodes.remove(ino);
                }
                if inner.root_children.get(&tree.name) == Some(&tree.root_ino) {
                    inner.root_children.remove(&tree.name);
                }
            }
        }
    }

    fn attr(&self, ino: u64, inner: &Inner) -> Option<FileAttr> {
        let (kind, size, created) = if ino == ROOT_INO {
            (FileType::Directory, 0, self.started)
        } else {
            let node = inner.nodes.get(&ino)?;
            let created = node
                .tree
                .and_then(|k| inner.trees.get(&k))
                .map_or(self.started, |t| t.created);
            match node.kind {
                NodeKind::Dir { .. } => (FileType::Directory, 0, created),
                NodeKind::File { size, .. } => (FileType::RegularFile, size, created),
            }
        };
        let dir = kind == FileType::Directory;
        Some(FileAttr {
            ino: INodeNo(ino),
            size,
            blocks: size.div_ceil(512),
            atime: created,
            mtime: created,
            ctime: created,
            crtime: created,
            kind,
            // Writable modes although the mount is read-only: a file manager copies the
            // source's mode, and a pasted copy must not come out read-only.
            perm: if dir { 0o755 } else { 0o644 },
            nlink: if dir { 2 } else { 1 },
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        })
    }
}

// ---------------------------------------------------------------------------
// The filesystem
// ---------------------------------------------------------------------------

struct ClipFs {
    st: Arc<MountState>,
}

impl Filesystem for ClipFs {
    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let inner = self.st.inner.lock().expect("mount lock");
        let child = if parent.0 == ROOT_INO {
            inner.root_children.get(name).copied()
        } else {
            match inner.nodes.get(&parent.0).map(|n| &n.kind) {
                Some(NodeKind::Dir { children }) => children.get(name).copied(),
                _ => None,
            }
        };
        match child.and_then(|ino| self.st.attr(ino, &inner)) {
            Some(attr) => reply.entry(&ATTR_TTL, &attr, Generation(0)),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        let inner = self.st.inner.lock().expect("mount lock");
        match self.st.attr(ino.0, &inner) {
            Some(attr) => reply.attr(&ATTR_TTL, &attr),
            None => reply.error(Errno::ENOENT),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let inner = self.st.inner.lock().expect("mount lock");
        let (parent, children): (u64, Vec<(OsString, u64)>) = if ino.0 == ROOT_INO {
            (
                ROOT_INO,
                inner
                    .root_children
                    .iter()
                    .map(|(n, i)| (n.clone(), *i))
                    .collect(),
            )
        } else {
            match inner.nodes.get(&ino.0) {
                Some(Node {
                    kind: NodeKind::Dir { children },
                    parent,
                    ..
                }) => (
                    *parent,
                    children.iter().map(|(n, i)| (n.clone(), *i)).collect(),
                ),
                Some(_) => return reply.error(Errno::ENOTDIR),
                None => return reply.error(Errno::ENOENT),
            }
        };
        let mut entries = vec![
            (ino.0, FileType::Directory, OsString::from(".")),
            (parent, FileType::Directory, OsString::from("..")),
        ];
        for (name, child) in children {
            let kind = match inner.nodes.get(&child).map(|n| &n.kind) {
                Some(NodeKind::File { .. }) => FileType::RegularFile,
                _ => FileType::Directory,
            };
            entries.push((child, kind, name));
        }
        for (i, (child, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(INodeNo(child), (i + 1) as u64, kind, name) {
                break; // buffer full; the kernel asks again from this offset
            }
        }
        reply.ok();
    }

    fn open(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, _kill: bool, reply: ReplyOpen) {
        let mut guard = self.st.inner.lock().expect("mount lock");
        let inner = &mut *guard;
        let (key, file_idx, size) = match inner.nodes.get(&ino.0) {
            Some(Node {
                tree: Some(key),
                kind: NodeKind::File { file_idx, size },
                ..
            }) => (*key, *file_idx, *size),
            Some(_) => return reply.error(Errno::EISDIR),
            None => return reply.error(Errno::ENOENT),
        };
        let Some(tree) = inner.trees.get_mut(&key) else {
            return reply.error(Errno::ENOENT);
        };
        tree.open += 1;
        tree.last_active = Instant::now();
        // The paste's job: started by its first open, shared by every file it reads.
        let job = *tree.job.get_or_insert_with(JobId::next);

        let (tx, rx) = mpsc::unbounded_channel();
        self.st.bridge.rt.spawn(serve_file(
            FileStream {
                origin_id: key.0,
                seq: key.1,
                file_idx,
                size,
                job,
                bridge: self.st.bridge.clone(),
                cache_start: 0,
                cache: Vec::new(),
                pending: None,
            },
            rx,
        ));
        let fh = inner.next_fh;
        inner.next_fh += 1;
        inner.handles.insert(fh, OpenFile { key, reads: tx });
        reply.opened(FileHandle(fh), FopenFlags::empty());
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        let inner = self.st.inner.lock().expect("mount lock");
        let Some(file) = inner.handles.get(&fh.0) else {
            return reply.error(Errno::EBADF);
        };
        if let Err(mpsc::error::SendError(op)) = file.reads.send(ReadOp {
            offset,
            size,
            reply,
        }) {
            op.reply.error(Errno::EIO);
        }
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let mut inner = self.st.inner.lock().expect("mount lock");
        // Dropping the handle closes its read queue; the serving task ends after the
        // reads already queued.
        if let Some(file) = inner.handles.remove(&fh.0) {
            if let Some(tree) = inner.trees.get_mut(&file.key) {
                tree.open = tree.open.saturating_sub(1);
                tree.last_active = Instant::now();
            }
        }
        reply.ok();
    }
}

// ---------------------------------------------------------------------------
// Reading: the destination end of a lazy file paste, windowed like `LazyFileStream`
// ---------------------------------------------------------------------------

/// The read state of one open file. Same read-ahead as the Windows `LazyFileStream`: fetch
/// a [`READAHEAD_WINDOW`] per round trip and prefetch the next one as soon as the current
/// one is being consumed. At most one window beyond what the app read crosses the wire.
struct FileStream {
    origin_id: OriginId,
    seq: Seq,
    file_idx: u32,
    /// From the manifest; the origin never re-states it.
    size: u64,
    job: JobId,
    bridge: Bridge,
    /// The buffered window covers `[cache_start, cache_start + cache.len())`.
    cache_start: u64,
    cache: Vec<u8>,
    /// An issued, not yet consumed prefetch of the window starting at this offset.
    pending: Option<(u64, oneshot::Receiver<RenderResult>)>,
}

/// Serve one open file's reads, in the order the kernel sent them, until it is released.
async fn serve_file(mut stream: FileStream, mut reads: mpsc::UnboundedReceiver<ReadOp>) {
    while let Some(op) = reads.recv().await {
        match stream.read(op.offset, op.size).await {
            Ok(bytes) => op.reply.data(&bytes),
            Err(e) => op.reply.error(e),
        }
    }
}

impl FileStream {
    fn request_window(&self, offset: u64) -> Option<oneshot::Receiver<RenderResult>> {
        if offset >= self.size {
            return None;
        }
        let (reply, rx) = oneshot::channel();
        let req = FormatReq {
            origin_id: self.origin_id,
            seq: self.seq,
            format: Mime::uri_list(),
            file_idx: Some(self.file_idx),
            range: Some(ByteRange {
                offset,
                len: READAHEAD_WINDOW.min(self.size - offset),
            }),
            job: self.job,
        };
        self.bridge
            .render_tx
            .send(RenderRequest { req, reply })
            .ok()?;
        Some(rx)
    }

    /// Read `[offset, offset + size)`, clamped to the file. A FUSE read must return every
    /// byte asked for short of EOF (the kernel zero-fills a short reply), so a read that
    /// straddles two windows is assembled from both.
    async fn read(&mut self, offset: u64, size: u32) -> Result<Vec<u8>, Errno> {
        let end = offset.saturating_add(u64::from(size)).min(self.size);
        let mut out = Vec::with_capacity(end.saturating_sub(offset) as usize);
        let mut pos = offset;
        while pos < end {
            let cached =
                pos >= self.cache_start && pos < self.cache_start + self.cache.len() as u64;
            if !cached {
                let rx = match self.pending.take() {
                    Some((start, rx)) if start == pos => rx,
                    _ => self.request_window(pos).ok_or(Errno::EIO)?,
                };
                match tokio::time::timeout(self.bridge.render_timeout, rx).await {
                    Ok(Ok(Ok(payload))) => {
                        self.cache_start = pos;
                        self.cache = payload.bytes;
                    }
                    // Fetch failed, timed out, or the bridge is gone: an error, never a hang
                    // and never a silently zero-filled file (CONVENTIONS.md).
                    _ => return Err(Errno::EIO),
                }
                if self.cache.is_empty() {
                    break; // the origin's file shrank since the copy: end it here
                }
            }
            let off = (pos - self.cache_start) as usize;
            let n = ((end - pos) as usize).min(self.cache.len() - off);
            out.extend_from_slice(&self.cache[off..off + n]);
            pos += n as u64;
        }
        if self.pending.is_none() && !self.cache.is_empty() {
            let next = self.cache_start + self.cache.len() as u64;
            self.pending = self.request_window(next).map(|rx| (next, rx));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clipline_core::{ContentHash, FormatDesc, Payload};

    fn bridge() -> (
        Bridge,
        mpsc::UnboundedReceiver<RenderRequest>,
        mpsc::UnboundedReceiver<JobId>,
    ) {
        let (render_tx, render_rx) = mpsc::unbounded_channel();
        let (job_end_tx, job_end_rx) = mpsc::unbounded_channel();
        (
            Bridge {
                render_tx,
                job_end_tx,
                render_timeout: Duration::from_secs(5),
                rt: tokio::runtime::Handle::current(),
            },
            render_rx,
            job_end_rx,
        )
    }

    fn offer(seq: u64, files: Vec<FileEntry>) -> Offer {
        Offer {
            origin_id: OriginId(9),
            seq: Seq(seq),
            formats: vec![FormatDesc {
                mime: Mime::uri_list(),
                size: files.iter().map(|f| f.size).sum(),
            }],
            files,
            hash: ContentHash([0; 32]),
        }
    }

    /// A state without a kernel mount: the namespace and job logic are plain data.
    fn state(bridge: Bridge, grace: Duration) -> Arc<MountState> {
        Arc::new(MountState {
            dir: PathBuf::from("/run/user/1000/clipline/files"),
            bridge,
            grace,
            uid: 1000,
            gid: 1000,
            started: SystemTime::now(),
            inner: Mutex::new(Inner {
                next_ino: ROOT_INO + 1,
                next_fh: 1,
                ..Inner::default()
            }),
        })
    }

    fn lookup_path(st: &MountState, path: &str) -> Option<u64> {
        let inner = st.inner.lock().unwrap();
        let mut parts = path.split('/');
        let mut ino = *inner.root_children.get(OsStr::new(parts.next()?))?;
        for p in parts {
            ino = child_of(&inner, ino, p)?;
        }
        Some(ino)
    }

    /// The manifest becomes a tree: folders from paths, empty dirs kept, `..` dropped.
    #[tokio::test]
    async fn manifest_builds_a_subtree() {
        let (b, _r, _j) = bridge();
        let st = state(b, PASTE_GRACE);
        let view = FileView(Arc::clone(&st));
        view.set_head(Some(&offer(
            12,
            vec![
                FileEntry::new("mydir/a.txt", 3),
                FileEntry::new("mydir/sub/b.txt", 5),
                FileEntry::new_dir("mydir/empty"),
                FileEntry {
                    rel_path: "../evil".into(),
                    size: 1,
                    is_dir: false,
                },
            ],
        )));

        let a = lookup_path(&st, "12/mydir/a.txt").expect("a.txt");
        let inner = st.inner.lock().unwrap();
        assert!(matches!(
            inner.nodes[&a].kind,
            NodeKind::File {
                file_idx: 0,
                size: 3
            }
        ));
        drop(inner);
        let b = lookup_path(&st, "12/mydir/sub/b.txt").expect("b.txt");
        assert!(matches!(
            st.inner.lock().unwrap().nodes[&b].kind,
            NodeKind::File {
                file_idx: 1,
                size: 5
            }
        ));
        assert!(
            lookup_path(&st, "12/mydir/empty").is_some(),
            "empty dir kept"
        );
        assert!(
            lookup_path(&st, "12/evil").is_some(),
            "`..` dropped, not followed"
        );
        let attr = st.attr(a, &st.inner.lock().unwrap()).unwrap();
        assert_eq!(
            (attr.size, attr.perm, attr.kind),
            (3, 0o644, FileType::RegularFile)
        );
    }

    /// The uri-list names each top-level entry under the mount, encoded, CRLF-terminated.
    #[tokio::test]
    async fn uri_list_names_top_level_entries() {
        let (b, _r, _j) = bridge();
        let view = FileView(state(b, PASTE_GRACE));
        let o = offer(
            7,
            vec![
                FileEntry::new("my dir/a.txt", 1),
                FileEntry::new("my dir/b.txt", 1),
                FileEntry::new("é.pdf", 1),
            ],
        );
        view.set_head(Some(&o));
        let list = String::from_utf8(view.uri_list((o.origin_id, o.seq)).unwrap()).unwrap();
        assert_eq!(
            list,
            "file:///run/user/1000/clipline/files/7/my%20dir\r\n\
             file:///run/user/1000/clipline/files/7/%C3%A9.pdf\r\n"
        );
        assert!(view.is_ours(Path::new("/run/user/1000/clipline/files/7/my dir")));
        assert!(!view.is_ours(Path::new("/home/u/my dir")));
    }

    /// One job per pasted subtree: shared by every file opened in it, ended only after the
    /// grace with nothing open; a non-head subtree is then dropped, the head kept.
    #[tokio::test]
    async fn one_job_per_paste_ends_after_grace() {
        let (b, _r, mut ends) = bridge();
        let grace = Duration::from_secs(300);
        let st = state(b, grace);
        let view = FileView(Arc::clone(&st));
        let o = offer(3, vec![FileEntry::new("d/x", 1), FileEntry::new("d/y", 1)]);
        let key = (o.origin_id, o.seq);
        view.set_head(Some(&o));

        // Simulate a folder copy: open x, close x, open y, close y.
        let job = |st: &MountState| st.inner.lock().unwrap().trees[&key].job;
        let open = |st: &MountState| {
            let mut i = st.inner.lock().unwrap();
            let t = i.trees.get_mut(&key).unwrap();
            t.open += 1;
            *t.job.get_or_insert_with(JobId::next)
        };
        let close = |st: &MountState, at: Instant| {
            let mut i = st.inner.lock().unwrap();
            let t = i.trees.get_mut(&key).unwrap();
            t.open -= 1;
            t.last_active = at;
        };
        let j1 = open(&st);
        let t0 = Instant::now();
        close(&st, t0);
        // A new head arrives between the two files: the paste must carry on.
        view.set_head(Some(&offer(4, vec![FileEntry::new("z", 1)])));
        st.sweep(t0 + Duration::from_secs(1));
        assert_eq!(job(&st), Some(j1), "the job survives the gap between files");
        let j2 = open(&st);
        assert_eq!(j1, j2, "both files share one job");
        close(&st, t0 + Duration::from_secs(2));

        st.sweep(t0 + Duration::from_secs(2) + grace - Duration::from_secs(1));
        assert!(ends.try_recv().is_err(), "not ended inside the grace");
        st.sweep(t0 + Duration::from_secs(2) + grace);
        assert_eq!(ends.try_recv().ok(), Some(j1), "ended after the grace");
        assert!(
            !st.inner.lock().unwrap().trees.contains_key(&key),
            "old subtree dropped"
        );
        assert!(lookup_path(&st, "4/z").is_some(), "the head stays");
    }

    /// Reads assemble across window boundaries, prefetch the next window, and stop at EOF.
    #[tokio::test]
    async fn reads_are_windowed_and_assembled() {
        let (b, mut renders, _j) = bridge();
        let size = READAHEAD_WINDOW + 100;
        let content: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let served = content.clone();
        let origin = tokio::spawn(async move {
            let mut ranges = Vec::new();
            while let Some(RenderRequest { req, reply }) = renders.recv().await {
                let r = req.range.unwrap();
                ranges.push((r.offset, r.len));
                let end = (r.offset + r.len).min(served.len() as u64) as usize;
                let _ = reply.send(Ok(Payload::new(
                    req.format,
                    served[r.offset as usize..end].to_vec(),
                )));
            }
            ranges
        });
        let mut s = FileStream {
            origin_id: OriginId(1),
            seq: Seq(1),
            file_idx: 0,
            size,
            job: JobId(77),
            bridge: b,
            cache_start: 0,
            cache: Vec::new(),
            pending: None,
        };
        let first = s.read(0, 128 * 1024).await.unwrap();
        assert_eq!(first, content[..128 * 1024]);
        // Straddles the first window's end.
        let off = READAHEAD_WINDOW - 50;
        let mid = s.read(off, 128 * 1024).await.unwrap();
        assert_eq!(
            mid,
            content[off as usize..],
            "assembled to EOF from two windows"
        );
        assert!(s.read(size, 4096).await.unwrap().is_empty(), "EOF");
        drop(s);
        let ranges = origin.await.unwrap();
        assert_eq!(
            ranges,
            vec![(0, READAHEAD_WINDOW), (READAHEAD_WINDOW, 100)],
            "one window, then the prefetched tail — no re-fetch"
        );
    }

    /// **Live** (needs `/dev/fuse` + `fusermount3`; `make test-live`): a real kernel mount,
    /// read through plain `std::fs` like a file manager would — a folder's two files stream
    /// from a fake origin under **one** job, the uri-list resolves, and drop unmounts. A
    /// second mount while the first lives takes the per-process fallback dir.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "mounts a real FUSE filesystem"]
    async fn live_mount_streams_a_folder_under_one_job() {
        let base = std::env::temp_dir().join(format!("clipline-fuse-live-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (b, mut renders, mut ends) = bridge();

        let big: Vec<u8> = (0..(READAHEAD_WINDOW * 2 + 12345))
            .map(|i| (i % 253) as u8)
            .collect();
        let small = b"hello from the origin".to_vec();
        let contents = [big.clone(), small.clone()];
        let origin = tokio::spawn(async move {
            let mut jobs = Vec::new();
            while let Some(RenderRequest { req, reply }) = renders.recv().await {
                jobs.push(req.job);
                let data = &contents[req.file_idx.unwrap() as usize];
                let r = req.range.unwrap();
                let end = (r.offset + r.len).min(data.len() as u64) as usize;
                let _ = reply.send(Ok(Payload::new(
                    req.format,
                    data[r.offset as usize..end].to_vec(),
                )));
            }
            jobs
        });

        let mount =
            FileMount::start_in(&base, b.clone(), Duration::from_millis(200)).expect("mount");
        let o = offer(
            42,
            vec![
                FileEntry::new("folder/big.bin", big.len() as u64),
                FileEntry::new("folder/sub/small.txt", small.len() as u64),
                FileEntry::new_dir("folder/empty"),
            ],
        );
        let view = mount.view();
        view.set_head(Some(&o));
        let list = String::from_utf8(view.uri_list((o.origin_id, o.seq)).unwrap()).unwrap();
        let dir = base.join("files/42/folder");
        assert_eq!(list, format!("file://{}\r\n", dir.display()));

        let d = dir.clone();
        let (got_big, got_small, empty_is_dir, write_err) =
            tokio::task::spawn_blocking(move || {
                let meta = std::fs::metadata(d.join("big.bin")).unwrap();
                assert_eq!(
                    meta.len(),
                    (READAHEAD_WINDOW * 2 + 12345),
                    "size from the manifest"
                );
                (
                    std::fs::read(d.join("big.bin")).unwrap(),
                    std::fs::read(d.join("sub/small.txt")).unwrap(),
                    std::fs::metadata(d.join("empty")).unwrap().is_dir(),
                    std::fs::write(d.join("new"), b"x")
                        .unwrap_err()
                        .raw_os_error(),
                )
            })
            .await
            .unwrap();
        assert!(got_big == big, "big file streamed intact across windows");
        assert_eq!(got_small, small);
        assert!(empty_is_dir, "empty dir recreated");
        assert_eq!(write_err, Some(libc_erofs()), "read-only");

        // A second node in the same session gets its own dir.
        let second =
            FileMount::start_in(&base, b, Duration::from_millis(200)).expect("second mount");
        assert!(second
            .view()
            .0
            .dir
            .ends_with(format!("files-{}", std::process::id())));
        drop(second);

        // The grace runs out → the one job ends.
        let ended = tokio::time::timeout(Duration::from_secs(10), ends.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(ends.try_recv().is_err(), "exactly one job ended");
        drop(view); // the last bridge handles, so the fake origin's stream ends
        drop(mount);
        let jobs = tokio::time::timeout(Duration::from_secs(10), origin)
            .await
            .expect("origin ended")
            .unwrap();
        assert!(
            !jobs.is_empty() && jobs.iter().all(|j| *j == ended),
            "every read rode the one job"
        );

        let mounted = mount_points(&std::fs::read("/proc/self/mountinfo").unwrap());
        assert!(
            !mounted.iter().any(|p| p.starts_with(&base)),
            "unmounted on drop"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    fn libc_erofs() -> i32 {
        rustix::io::Errno::ROFS.raw_os_error()
    }

    #[test]
    fn mountinfo_points_are_unescaped() {
        let info =
            b"36 25 0:32 / /run/user/1000/clipline/files rw,nosuid - fuse.clipline clipline ro\n\
                     40 25 0:33 / /mnt/with\\040space rw - ext4 /dev/sda1 rw\n";
        let pts = mount_points(info);
        assert!(pts.contains(&PathBuf::from("/run/user/1000/clipline/files")));
        assert!(pts.contains(&PathBuf::from("/mnt/with space")));
    }
}
