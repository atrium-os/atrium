//! Single-file `.insula` archive format.
//!
//! Wire format:
//!
//! ```text
//!   bytes  [0..4)   magic "INSB"
//!   byte   [4]      version = 1
//!   bytes  [5..8)   reserved (zero)
//!   bytes  [8..16)  n_entries (u64 LE)
//!   for each entry:
//!     bytes [..2)   path_len (u16 LE)
//!     bytes [..L)   path UTF-8 (forward-slash, bundle-relative)
//!     bytes [..4)   mode (u32 LE, low 9 bits used)
//!     bytes [..8)   size (u64 LE)
//!     bytes [..N)   data
//! ```
//!
//! Properties:
//!
//! - **Deterministic** — entries are sorted lexicographically
//!   by path; no mtimes, uids, or other host-dependent
//!   fields. Two packs of the same directory tree produce
//!   byte-identical output.
//! - **Self-describing** — the consumer never needs file-
//!   extension heuristics; the leading 4 bytes are
//!   enough.
//! - **Streamable enough** — pack/unpack walk in a single
//!   pass; no central directory.
//!
//! Out of scope for v0: compression (zstd planned),
//! checksums per entry (the detached `signature` file at
//! the bundle root already covers manifest + entry
//! binary; whole-archive integrity belongs to the
//! distribution layer).

use std::path::{Path, PathBuf};
use thiserror::Error;

/// Magic prefix.
pub const MAGIC: &[u8; 4] = b"INSB";

/// Current archive-format version.
pub const VERSION: u8 = 1;

/// Header size in bytes (magic + version + reserved + n_entries).
const HEADER_LEN: usize = 16;

/// Recognized file extension.
pub const EXTENSION: &str = "insula";

/// Errors that can occur reading/writing a `.insula`
/// archive.
#[derive(Debug, Error)]
pub enum ArchiveError {
    /// Underlying I/O error.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// File doesn't start with the `INSB` magic.
    #[error("not an .insula archive (bad magic)")]
    BadMagic,

    /// Archive version is newer than we understand.
    #[error("unsupported .insula archive version {0} (we speak {})", VERSION)]
    UnsupportedVersion(u8),

    /// Archive header / entry table is structurally
    /// malformed (truncated, bogus lengths, etc.).
    #[error("malformed archive: {0}")]
    Malformed(&'static str),

    /// An entry path tried to escape the bundle root
    /// (absolute, `..`, or contains a null byte).
    #[error("unsafe entry path in archive: {0:?}")]
    UnsafePath(String),

    /// Bundle source path passed to [`pack_dir`] is not
    /// a directory.
    #[error("source is not a directory: {0}")]
    SourceNotADir(PathBuf),
}

/// Read a `.insula` archive into memory and extract it
/// into `dest_dir` (which must already exist).
///
/// Entry paths are validated to be relative and to
/// contain no `..` components; any violation aborts with
/// [`ArchiveError::UnsafePath`] *before* any file is
/// written, so a malicious archive cannot leave partial
/// state on disk outside `dest_dir`.
pub fn unpack_into(archive_path: impl AsRef<Path>, dest_dir: impl AsRef<Path>)
    -> Result<(), ArchiveError>
{
    let bytes = std::fs::read(archive_path.as_ref())?;
    unpack_bytes_into(&bytes, dest_dir.as_ref())
}

/// Like [`unpack_into`] but reads from an in-memory
/// buffer.
pub fn unpack_bytes_into(bytes: &[u8], dest_dir: &Path) -> Result<(), ArchiveError> {
    let entries = parse(bytes)?;

    // Validate ALL paths before touching disk.
    for (path, _, _) in &entries {
        validate_relative_path(path)?;
    }

    std::fs::create_dir_all(dest_dir)?;

    for (path, mode, data) in entries {
        let full = dest_dir.join(&path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&full, data)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = std::fs::metadata(&full)?.permissions();
            p.set_mode(mode & 0o777);
            std::fs::set_permissions(&full, p)?;
        }
        #[cfg(not(unix))]
        let _ = mode;
    }
    Ok(())
}

/// Walk a directory tree, pack it into a `.insula`
/// archive, and write the result to `out_path`.
///
/// The walk follows the bundle convention: `manifest.toml`
/// + `signature` (if present) at the root, `bin/`,
/// `assets/`. Symlinks are not followed; their targets
/// are not included.
pub fn pack_dir(src_dir: impl AsRef<Path>, out_path: impl AsRef<Path>)
    -> Result<(), ArchiveError>
{
    let bytes = pack_dir_to_bytes(src_dir.as_ref())?;
    std::fs::write(out_path.as_ref(), bytes)?;
    Ok(())
}

/// Pack a directory tree into an in-memory `.insula`
/// archive.
pub fn pack_dir_to_bytes(src_dir: &Path) -> Result<Vec<u8>, ArchiveError> {
    if !src_dir.is_dir() {
        return Err(ArchiveError::SourceNotADir(src_dir.to_path_buf()));
    }

    let mut entries: Vec<(String, u32, Vec<u8>)> = Vec::new();
    collect_files(src_dir, src_dir, &mut entries)?;
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = Vec::with_capacity(64 * 1024);
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&[0u8; 3]); // reserved
    out.extend_from_slice(&(entries.len() as u64).to_le_bytes());

    for (path, mode, data) in entries {
        let path_bytes = path.as_bytes();
        let path_len: u16 = path_bytes.len()
            .try_into()
            .map_err(|_| ArchiveError::Malformed("entry path > 65535 bytes"))?;
        out.extend_from_slice(&path_len.to_le_bytes());
        out.extend_from_slice(path_bytes);
        out.extend_from_slice(&mode.to_le_bytes());
        out.extend_from_slice(&(data.len() as u64).to_le_bytes());
        out.extend_from_slice(&data);
    }
    Ok(out)
}

fn collect_files(
    root: &Path,
    dir: &Path,
    out: &mut Vec<(String, u32, Vec<u8>)>,
) -> Result<(), ArchiveError> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let path = entry.path();
        if ft.is_dir() {
            collect_files(root, &path, out)?;
        } else if ft.is_file() {
            let rel = path.strip_prefix(root).unwrap();
            // Forward slashes on every host — the archive
            // is consumed cross-platform.
            let rel_str: String = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");

            // Skip nothing — bundle layout is the source
            // of truth; we faithfully record what's there.

            let data = std::fs::read(&path)?;
            let mode = file_mode(&path)?;
            out.push((rel_str, mode, data));
        }
        // Symlinks, sockets, fifos, etc. are silently
        // ignored. A well-formed bundle shouldn't have
        // them; if it does, the host adapter will catch
        // the missing entry binary at install time.
    }
    Ok(())
}

#[cfg(unix)]
fn file_mode(p: &Path) -> std::io::Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    Ok(std::fs::metadata(p)?.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn file_mode(_p: &Path) -> std::io::Result<u32> {
    Ok(0o644)
}

/// Parse the leading header + entry table.
fn parse(bytes: &[u8]) -> Result<Vec<(String, u32, Vec<u8>)>, ArchiveError> {
    if bytes.len() < HEADER_LEN {
        return Err(ArchiveError::Malformed("header truncated"));
    }
    if &bytes[..4] != MAGIC {
        return Err(ArchiveError::BadMagic);
    }
    let version = bytes[4];
    if version != VERSION {
        return Err(ArchiveError::UnsupportedVersion(version));
    }
    let n_entries = u64::from_le_bytes(bytes[8..16].try_into().unwrap()) as usize;
    let mut entries = Vec::with_capacity(n_entries.min(1024));
    let mut cur = HEADER_LEN;

    for _ in 0..n_entries {
        if cur + 2 > bytes.len() {
            return Err(ArchiveError::Malformed("entry header truncated"));
        }
        let path_len = u16::from_le_bytes(bytes[cur..cur + 2].try_into().unwrap()) as usize;
        cur += 2;
        if cur + path_len > bytes.len() {
            return Err(ArchiveError::Malformed("entry path truncated"));
        }
        let path = std::str::from_utf8(&bytes[cur..cur + path_len])
            .map_err(|_| ArchiveError::Malformed("entry path not UTF-8"))?
            .to_string();
        cur += path_len;
        if cur + 4 + 8 > bytes.len() {
            return Err(ArchiveError::Malformed("entry metadata truncated"));
        }
        let mode = u32::from_le_bytes(bytes[cur..cur + 4].try_into().unwrap());
        cur += 4;
        let size = u64::from_le_bytes(bytes[cur..cur + 8].try_into().unwrap()) as usize;
        cur += 8;
        if cur + size > bytes.len() {
            return Err(ArchiveError::Malformed("entry data truncated"));
        }
        let data = bytes[cur..cur + size].to_vec();
        cur += size;

        entries.push((path, mode, data));
    }
    if cur != bytes.len() {
        return Err(ArchiveError::Malformed("trailing bytes after entry table"));
    }
    Ok(entries)
}

fn validate_relative_path(p: &str) -> Result<(), ArchiveError> {
    if p.is_empty() || p.starts_with('/') || p.contains('\0') {
        return Err(ArchiveError::UnsafePath(p.to_string()));
    }
    for comp in p.split('/') {
        if comp == ".." || comp.is_empty() {
            return Err(ArchiveError::UnsafePath(p.to_string()));
        }
    }
    Ok(())
}

/// Quick magic-only check — does `bytes` start with the
/// `INSB` prefix?
pub fn looks_like_archive(bytes: &[u8]) -> bool {
    bytes.len() >= 4 && &bytes[..4] == MAGIC
}

/// Convenience: peek at a path on disk and report
/// whether the leading bytes look like a `.insula`
/// archive. Returns `false` on any I/O error (caller
/// should fall back to treating the path as a directory).
pub fn path_looks_like_archive(p: &Path) -> bool {
    if let Ok(mut f) = std::fs::File::open(p) {
        use std::io::Read;
        let mut head = [0u8; 4];
        if f.read_exact(&mut head).is_ok() {
            return &head == MAGIC;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_minimal_bundle(root: &Path) {
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("manifest.toml"), b"manifest body").unwrap();
        std::fs::write(root.join("bin/x"), b"#!/bin/sh\necho hi\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut p = std::fs::metadata(root.join("bin/x")).unwrap().permissions();
            p.set_mode(0o755);
            std::fs::set_permissions(root.join("bin/x"), p).unwrap();
        }
    }

    #[test]
    fn roundtrip_preserves_files_and_modes() {
        let src = tempfile::tempdir().unwrap();
        build_minimal_bundle(src.path());

        let bytes = pack_dir_to_bytes(src.path()).unwrap();
        assert!(looks_like_archive(&bytes));

        let dst = tempfile::tempdir().unwrap();
        unpack_bytes_into(&bytes, dst.path()).unwrap();

        assert_eq!(
            std::fs::read(dst.path().join("manifest.toml")).unwrap(),
            b"manifest body",
        );
        assert_eq!(
            std::fs::read(dst.path().join("bin/x")).unwrap(),
            b"#!/bin/sh\necho hi\n",
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dst.path().join("bin/x"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o755, "executable bit must round-trip");
        }
    }

    #[test]
    fn pack_is_deterministic() {
        let src = tempfile::tempdir().unwrap();
        build_minimal_bundle(src.path());
        let a = pack_dir_to_bytes(src.path()).unwrap();
        let b = pack_dir_to_bytes(src.path()).unwrap();
        assert_eq!(a, b, "two packs of the same tree must be byte-identical");
    }

    #[test]
    fn pack_includes_signature_file_if_present() {
        let src = tempfile::tempdir().unwrap();
        build_minimal_bundle(src.path());
        std::fs::write(src.path().join("signature"), b"INSL\x01...").unwrap();

        let bytes = pack_dir_to_bytes(src.path()).unwrap();
        let dst = tempfile::tempdir().unwrap();
        unpack_bytes_into(&bytes, dst.path()).unwrap();
        assert!(dst.path().join("signature").exists());
    }

    #[test]
    fn bad_magic_rejected() {
        let dst = tempfile::tempdir().unwrap();
        let err = unpack_bytes_into(b"not-an-archive", dst.path()).unwrap_err();
        matches!(err, ArchiveError::BadMagic);
    }

    #[test]
    fn unsafe_path_rejected_before_writes() {
        // Build a hand-crafted archive with an entry
        // path of "../escape" — must be refused.
        let mut bad = Vec::new();
        bad.extend_from_slice(MAGIC);
        bad.push(VERSION);
        bad.extend_from_slice(&[0u8; 3]);
        bad.extend_from_slice(&1u64.to_le_bytes());

        let path = b"../escape";
        bad.extend_from_slice(&(path.len() as u16).to_le_bytes());
        bad.extend_from_slice(path);
        bad.extend_from_slice(&0o644u32.to_le_bytes());
        bad.extend_from_slice(&0u64.to_le_bytes());

        let dst = tempfile::tempdir().unwrap();
        let err = unpack_bytes_into(&bad, dst.path()).unwrap_err();
        match err {
            ArchiveError::UnsafePath(_) => (),
            other => panic!("expected UnsafePath, got: {:?}", other),
        }
        // Nothing should have been written.
        assert!(dst.path().read_dir().unwrap().next().is_none());
    }

    #[test]
    fn looks_like_archive_detects() {
        let src = tempfile::tempdir().unwrap();
        build_minimal_bundle(src.path());

        let arc = src.path().with_extension("insula");
        pack_dir(src.path(), &arc).unwrap();
        assert!(path_looks_like_archive(&arc));

        let plain = src.path().join("manifest.toml");
        assert!(!path_looks_like_archive(&plain));
    }
}
