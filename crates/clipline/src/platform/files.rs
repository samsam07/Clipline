//! Platform-neutral helpers for **by-reference files** on the origin side, shared by every
//! adapter that captures real filesystem paths (Windows `CF_HDROP`, Linux `text/uri-list`).
//!
//! Moved verbatim out of `windows.rs` when the Linux adapter arrived (M-Linux): the folder
//! walk and the ranged read are pure `std::fs`, and both adapters must behave identically
//! here — a manifest built on one OS is consumed on the other.
//!
//! Locked decision #8 governs all of it: at copy time we **stat, never read**; bytes are
//! read only when a peer fetches, and only the requested range.

use std::path::{Path, PathBuf};

use clipline_core::{ByteRange, FileEntry, LocalReadError, Mime, Payload};

/// Recursion bound: a symlink loop or a pathological tree must not run away on the pump
/// thread. Deep real trees past this are truncated (rare; logged).
pub(crate) const MAX_DIR_DEPTH: usize = 64;

/// Add `abs` (a file → one entry; a directory → its files, recursively) to the manifest,
/// keyed by `rel` (a forward-slash path relative to the dropped root).
pub(crate) fn collect_path(
    abs: &Path,
    rel: &str,
    paths: &mut Vec<PathBuf>,
    entries: &mut Vec<FileEntry>,
    depth: usize,
) {
    // `symlink_metadata` does not follow links — a symlinked directory is not descended,
    // which is what stops loops and stops a folder copy from silently escaping its tree.
    let meta = match std::fs::symlink_metadata(abs) {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!(error = %e, "skipping unreadable clipboard path");
            return;
        }
    };

    if meta.file_type().is_symlink() {
        tracing::debug!("skipping symlink in a copied folder");
        return;
    }

    if meta.is_file() {
        entries.push(FileEntry::new(rel, meta.len()));
        paths.push(abs.to_path_buf());
        return;
    }

    if meta.is_dir() {
        if depth >= MAX_DIR_DEPTH {
            tracing::debug!(%rel, "folder deeper than the recursion bound; truncating");
            return;
        }
        let rd = match std::fs::read_dir(abs) {
            Ok(rd) => rd,
            Err(e) => {
                tracing::debug!(error = %e, "skipping unreadable directory");
                return;
            }
        };
        let mut empty = true;
        for child in rd.flatten() {
            empty = false;
            let child_rel = format!("{}/{}", rel, child.file_name().to_string_lossy());
            collect_path(&child.path(), &child_rel, paths, entries, depth + 1);
        }
        // A directory with children is implied by their paths; only a directory with *no*
        // entries needs its own manifest record, or it would vanish (the shell rebuilds
        // folders from file paths). The path is kept aligned with the entry but never read.
        if empty {
            entries.push(FileEntry::new_dir(rel));
            paths.push(abs.to_path_buf());
        }
    }
}

pub(crate) fn slice_of(bytes: &[u8], range: Option<ByteRange>) -> Vec<u8> {
    match range {
        None => bytes.to_vec(),
        // Past EOF is a short read, not an error — the caller treats it as the end.
        Some(ByteRange { offset, len }) => {
            let start = (offset as usize).min(bytes.len());
            let end = (start + len as usize).min(bytes.len());
            bytes[start..end].to_vec()
        }
    }
}

/// Read one range of a file. A failure here is the expected consequence of pinning a
/// *path* rather than bytes: the user may have edited or deleted it since the copy.
pub(crate) fn read_file_range(
    path: &Path,
    range: Option<ByteRange>,
    format: Mime,
) -> Result<Payload, LocalReadError> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = std::fs::File::open(path)
        .map_err(|e| LocalReadError::SourceFailed(format!("open: {e}")))?;
    let bytes = match range {
        None => {
            let mut buf = Vec::new();
            file.read_to_end(&mut buf)
                .map_err(|e| LocalReadError::SourceFailed(format!("read: {e}")))?;
            buf
        }
        Some(ByteRange { offset, len }) => {
            file.seek(SeekFrom::Start(offset))
                .map_err(|e| LocalReadError::SourceFailed(format!("seek: {e}")))?;
            let mut buf = vec![0u8; len as usize];
            let mut got = 0usize;
            // Read to fill, tolerating short reads; a genuine EOF just ends it.
            while got < buf.len() {
                match file.read(&mut buf[got..]) {
                    Ok(0) => break,
                    Ok(n) => got += n,
                    Err(e) => return Err(LocalReadError::SourceFailed(format!("read: {e}"))),
                }
            }
            buf.truncate(got);
            buf
        }
    };
    Ok(Payload::new(format, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The folder walk (the testable half of `capture_hdrop`): a directory expands to its
    /// files, with `rel_path`s rooted at the dropped folder's name and using forward
    /// slashes — and it records paths, never reading contents (locked decision #8).
    #[test]
    fn collect_path_walks_a_folder_tree() {
        let base = std::env::temp_dir().join(format!("clipline-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("mydir");
        std::fs::create_dir_all(root.join("sub")).expect("mkdirs");
        std::fs::write(root.join("a.txt"), b"aaa").expect("a");
        std::fs::write(root.join("sub").join("b.txt"), b"bbbbb").expect("b");
        // (Empty-directory handling has its own test.)

        let mut paths = Vec::new();
        let mut entries = Vec::new();
        collect_path(&root, "mydir", &mut paths, &mut entries, 0);

        // Two files; non-empty dirs (mydir, sub) are implied by the file paths, no entries.
        assert_eq!(entries.len(), 2, "both files, no dir entries");
        assert_eq!(
            paths.len(),
            entries.len(),
            "paths and manifest stay aligned"
        );

        let mut by_rel: Vec<(String, u64)> = entries
            .iter()
            .map(|e| (e.rel_path.clone(), e.size))
            .collect();
        by_rel.sort();
        assert_eq!(
            by_rel,
            vec![
                ("mydir/a.txt".to_string(), 3),
                ("mydir/sub/b.txt".to_string(), 5),
            ],
            "rel paths rooted at the folder, forward slashes, sizes from stat",
        );

        // The recorded paths point at the real files (contents readable on demand, not now).
        for p in &paths {
            assert!(p.is_absolute() || p.exists(), "recorded a real path");
        }

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Empty directories are preserved as directory entries (an empty folder must survive
    /// the paste); non-empty ones are implied by their files and get no entry.
    #[test]
    fn collect_path_preserves_empty_directories() {
        let base = std::env::temp_dir().join(format!("clipline-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let root = base.join("mydir");
        std::fs::create_dir_all(root.join("hasfile")).expect("mkdirs");
        std::fs::create_dir_all(root.join("empty")).expect("empty");
        std::fs::create_dir_all(root.join("nested").join("deep_empty")).expect("nested");
        std::fs::write(root.join("hasfile").join("f.txt"), b"x").expect("f");

        let mut paths = Vec::new();
        let mut entries = Vec::new();
        collect_path(&root, "mydir", &mut paths, &mut entries, 0);
        assert_eq!(
            paths.len(),
            entries.len(),
            "paths and manifest stay aligned"
        );

        let files: Vec<&str> = entries
            .iter()
            .filter(|e| !e.is_dir)
            .map(|e| e.rel_path.as_str())
            .collect();
        let mut dirs: Vec<&str> = entries
            .iter()
            .filter(|e| e.is_dir)
            .map(|e| e.rel_path.as_str())
            .collect();
        dirs.sort();

        assert_eq!(files, vec!["mydir/hasfile/f.txt"], "the one file");
        assert_eq!(
            dirs,
            vec!["mydir/empty", "mydir/nested/deep_empty"],
            "each leaf-empty dir kept; hasfile/ and nested/ implied by descendants",
        );
        assert!(
            entries.iter().filter(|e| e.is_dir).all(|e| e.size == 0),
            "directory entries have size 0",
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// A lone file collects as one entry named by its filename.
    #[test]
    fn collect_path_takes_a_single_file() {
        let f = std::env::temp_dir().join(format!("clipline-one-{}.bin", std::process::id()));
        std::fs::write(&f, vec![0u8; 10]).expect("write");

        let mut paths = Vec::new();
        let mut entries = Vec::new();
        let name = f.file_name().unwrap().to_string_lossy().into_owned();
        collect_path(&f, &name, &mut paths, &mut entries, 0);

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].size, 10);
        assert_eq!(paths[0], f);

        let _ = std::fs::remove_file(&f);
    }
}
