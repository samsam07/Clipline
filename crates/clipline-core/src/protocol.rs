//! In-core protocol/data types the `ClipboardAdapter` contract is expressed in
//! terms of (SPEC.md §1, §9; ARCHITECTURE.md "State" / "Wire shape").
//!
//! M1 pins only the **Rust shape** of these types — enough to lock the adapter
//! trait. Their **wire layout / framing / encoding is deferred to M3**
//! (`[CRYSTALLIZE: protocol milestone]`), as are the concrete identity of
//! `OriginId` and the hash algorithm of `ContentHash`. Field *names* here are the
//! ones fixed in SPEC.md / ARCHITECTURE.md and are reused verbatim (anti-drift rule,
//! CLAUDE.md).

use std::path::PathBuf;

/// Monotonic per-origin sequence number. Newest offer wins = highest `seq`; ties
/// broken by `origin_id` (SPEC.md §1 ordering; locked decision #3).
///
/// Placeholder width — final encoding is `[CRYSTALLIZE: protocol milestone]` (M3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Seq(pub u64);

/// Identity of the node that originated an offer. Used for echo suppression (a node
/// never re-applies an offer it originated — SPEC.md §1) and as the ordering
/// tiebreak.
///
/// Placeholder representation — the real identity type (and whether it is derived
/// from an address, a name, or a key) is `[CRYSTALLIZE: protocol milestone]` (M3;
/// pairing/identity keys are Phase 2 per CLAUDE.md scope).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OriginId(pub u64);

/// Content hash carried by an offer (SPEC.md §1). Algorithm/width is
/// `[CRYSTALLIZE: protocol milestone]` (M3); modelled as opaque bytes for now.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ContentHash(pub Vec<u8>);

/// A clipboard format identifier as it travels on the wire: a MIME type (SPEC.md
/// §9 — "UTF-8 on the wire", "normalize to PNG", `text/uri-list`). The *adapter*
/// maps this to/from OS-native formats (e.g. Windows `CF_UNICODETEXT` / `CF_DIB` /
/// `CFSTR_FILEDESCRIPTORW`; Wayland MIME strings). Core never speaks OS formats.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormatDesc {
    pub mime: Mime,
    pub size: u64,
}

/// One file in an offer's file group (SPEC.md §9 — files are by-reference; the offer
/// carries a manifest of names+sizes, no bytes). `rel_path` preserves folder structure
/// within the transfer; contents are fetched by this entry's index (`FormatReq.file_idx`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub rel_path: PathBuf,
    pub size: u64,
}

/// The offer broadcast on copy: metadata only, no bytes (SPEC.md §1; locked
/// decision #2). Also what a receiving node stores as its head slot
/// (ARCHITECTURE.md "State" — `Option<Offer>`).
#[derive(Debug, Clone, PartialEq, Eq)]
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
/// result in M4). Text = UTF-8 bytes; image = PNG bytes; a file's contents = raw
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
/// (SPEC.md §7 `RespectHints`). Consuming this is M6; M1 only carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SensitivityHint {
    /// No confidential-content tag observed.
    None,
    /// The OS tagged this content confidential (e.g. a password-manager hint).
    Sensitive,
}

/// A local copy detected by the adapter's `watch` (ARCHITECTURE.md copy flow —
/// `LocalCopy { formats, sizes, sensitivity_hint }`; sizes live inside `FormatDesc`).
/// No bytes. Core-side consumption (→ build `Offer`, broadcast) is M3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalCopy {
    pub formats: Vec<FormatDesc>,
    pub sensitivity_hint: SensitivityHint,
}

// NOTE (M1 decision — streaming, mstsc-style): there are no `FileBytes` / `LocalRef`
// types. Files are never materialized to a local staging copy; each file's contents are
// served on demand through the render inversion (a `Payload` keyed by `FormatReq.file_idx`,
// and a byte range in M4), streaming origin→destination. See locked decision #8.
