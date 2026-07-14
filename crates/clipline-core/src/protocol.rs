//! In-core protocol/data types the `ClipboardAdapter` contract is expressed in
//! terms of (SPEC.md §1, §9; ARCHITECTURE.md "State" / "Wire shape").
//!
//! M1 pinned the **Rust shape** of these types; **M2 pins their wire form** — every
//! type that rides a control frame derives `serde` and is encoded with `postcard`
//! (see [`crate::wire`]). This slice also resolves the three protocol
//! `[CRYSTALLIZE]`s: `Seq` (u64, postcard varint), `OriginId` (random `u128`, D4), and
//! `ContentHash` (BLAKE3 manifest digest, D5). Field *names* are the ones fixed in
//! SPEC.md / ARCHITECTURE.md and are reused verbatim (anti-drift rule, CLAUDE.md).

use std::path::PathBuf;

/// Per-origin sequence number driving the head ordering (SPEC.md §1; locked decision #3:
/// newest offer wins = highest `seq`, ties broken by `origin_id`).
///
/// A `u64` **Lamport timestamp** (M2 ruling): a node bumps its clock by 1 on each local
/// copy and to `max(clock, offer.seq)` on receiving an offer, so a strictly later copy
/// always carries a higher `seq` — which is what makes "highest `seq` wins" actually mean
/// "latest copy wins" across machines. Strictly increasing per origin; the tiebreak on an
/// equal `seq` (truly concurrent copies) is **higher `origin_id` wins**. Encoded as a
/// `postcard` varint on the wire.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct Seq(pub u64);

/// Identity of the node that originated an offer. Used for echo suppression (a node
/// never re-applies an offer it originated — SPEC.md §1) and as the ordering tiebreak.
///
/// A **random `u128` generated once at process startup** (D4), *not* persisted and
/// *not* key-derived: uniqueness + a total order for the tiebreak is all the semantics
/// need, and pairing/identity keys are Phase 2 (locked decision #10). A restarted node
/// simply gets a new identity; its old offers become unreachable and are re-pointed by
/// background reconciliation (M4).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct OriginId(pub u128);

impl OriginId {
    /// A fresh random node identity (D4). Call once at startup and reuse for the
    /// process lifetime.
    pub fn new_random() -> Self {
        OriginId(rand::random())
    }
}

// Lowercase-hex so u128 ids are loggable (tracing has no `u128` value type) without
// ever printing clipboard contents (CONVENTIONS.md).
impl std::fmt::Display for OriginId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

/// Content hash carried by an offer (SPEC.md §1). A fixed **32-byte BLAKE3** digest over
/// the offer **manifest**, not over content bytes (D5): files are by-reference and never
/// read at copy time (locked decisions #2/#8), so hashing content would break laziness.
/// The manifest hash still gives a stable per-offer identity for dedup/change-detection;
/// byte-level integrity of a *fetched* format, if wanted, is a separate per-format hash
/// computed lazily in M3.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ContentHash(pub [u8; 32]);

impl ContentHash {
    /// BLAKE3 over the offer manifest — `origin_id`, `seq`, and each advertised format
    /// (MIME + size) and file (`rel_path` + size), in order. No content bytes are read
    /// (D5). Deterministic for a given manifest.
    pub fn of_manifest(
        origin_id: OriginId,
        seq: Seq,
        formats: &[FormatDesc],
        files: &[FileEntry],
    ) -> Self {
        let mut h = blake3::Hasher::new();
        h.update(&origin_id.0.to_le_bytes());
        h.update(&seq.0.to_le_bytes());
        for fmt in formats {
            h.update(fmt.mime.as_str().as_bytes());
            h.update(&fmt.size.to_le_bytes());
        }
        for file in files {
            h.update(file.rel_path.to_string_lossy().as_bytes());
            h.update(&file.size.to_le_bytes());
        }
        ContentHash(*h.finalize().as_bytes())
    }
}

/// A clipboard format identifier as it travels on the wire: a MIME type (SPEC.md
/// §9 — "UTF-8 on the wire", "normalize to PNG", `text/uri-list`). The *adapter*
/// maps this to/from OS-native formats (e.g. Windows `CF_UNICODETEXT` / `CF_DIB` /
/// `CFSTR_FILEDESCRIPTORW`; Wayland MIME strings). Core never speaks OS formats.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct Mime(pub String);

impl Mime {
    pub fn new(s: impl Into<String>) -> Self {
        Mime(s.into())
    }

    /// `text/plain;charset=utf-8` — text is UTF-8 on the wire (SPEC.md §9).
    pub fn text_utf8() -> Self {
        Mime("text/plain;charset=utf-8".to_owned())
    }

    /// `image/png` — images are normalized to PNG on the wire (SPEC.md §9).
    pub fn png() -> Self {
        Mime("image/png".to_owned())
    }

    /// `text/uri-list` — the by-reference file format (SPEC.md §9; Linux mechanism).
    pub fn uri_list() -> Self {
        Mime("text/uri-list".to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One advertised format in an offer: its MIME type and byte size (SPEC.md §1 —
/// "available formats (each with MIME + size)").
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FormatDesc {
    pub mime: Mime,
    pub size: u64,
}

/// One file in an offer's file group (SPEC.md §9 — files are by-reference; the offer
/// carries a manifest of names+sizes, no bytes). `rel_path` preserves folder structure
/// within the transfer; contents are fetched by this entry's index (`FormatReq.file_idx`).
///
/// NOTE (M2): `rel_path` is a `PathBuf` and `serde`-serializes as the platform `OsStr`.
/// That is fine for the M2 Windows↔Windows mesh, but a cross-OS wire needs a normalized
/// UTF-8, forward-slash relative string. Finalizing that representation is deferred to
/// **M3** (when file bytes actually ride the wire) / **M-Linux**; M2 tests use non-file
/// offers, so it is not yet exercised.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileEntry {
    pub rel_path: PathBuf,
    pub size: u64,
}

/// The offer broadcast on copy: metadata only, no bytes (SPEC.md §1; locked
/// decision #2). Also what a receiving node stores as its head slot
/// (ARCHITECTURE.md "State" — `Option<Offer>`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Offer {
    pub origin_id: OriginId,
    pub seq: Seq,
    pub formats: Vec<FormatDesc>,
    /// The file group, if this offer carries files (SPEC.md §9). Empty = no files. A
    /// non-empty manifest is served as virtual files (Windows `CFSTR_FILEDESCRIPTORW` +
    /// `CFSTR_FILECONTENTS`; Linux `text/uri-list`); contents stream on demand, no
    /// staging (locked decision #8, amended M1).
    pub files: Vec<FileEntry>,
    pub hash: ContentHash,
}

/// The actual bytes of one format, produced on demand for a render (the fetch
/// result in M3). Text = UTF-8 bytes; image = PNG bytes; a file's contents = raw
/// bytes (SPEC.md §9). Never logged (CONVENTIONS.md).
#[derive(Clone, PartialEq, Eq)]
pub struct Payload {
    pub format: Mime,
    pub bytes: Vec<u8>,
}

impl Payload {
    pub fn new(format: Mime, bytes: Vec<u8>) -> Self {
        Payload { format, bytes }
    }
}

// Redacted debug: metadata only, never contents (CONVENTIONS.md logging rule).
impl std::fmt::Debug for Payload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Payload")
            .field("format", &self.format)
            .field("len", &self.bytes.len())
            .finish()
    }
}

/// OS sensitivity hint attached to a locally-detected copy, for the safety layer
/// (SPEC.md §7 `RespectHints`). Consuming this is M5; M1 only carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensitivityHint {
    /// No confidential-content tag observed.
    None,
    /// The OS tagged this content confidential (e.g. a password-manager hint).
    Sensitive,
}

/// A local copy detected by the adapter's `watch` (ARCHITECTURE.md copy flow —
/// `LocalCopy { formats, sizes, sensitivity_hint }`; sizes live inside `FormatDesc`).
/// No bytes. Core-side consumption (→ build `Offer`, broadcast) is M2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalCopy {
    pub formats: Vec<FormatDesc>,
    pub sensitivity_hint: SensitivityHint,
}

// NOTE (M1 decision — streaming, mstsc-style): there are no `FileBytes` / `LocalRef`
// types. Files are never materialized to a local staging copy; each file's contents are
// served on demand through the render inversion (a `Payload` keyed by `FormatReq.file_idx`,
// and a byte range in M3), streaming origin→destination. See locked decision #8.
