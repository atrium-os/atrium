//! The `.insula` archive (atrium-bundle-format.md §8), version 2: the
//! single-file form a package travels in.
//!
//! ```text
//!   [0..4)   "INSB"
//!   [4]      version = 2
//!   [5..8)   reserved, zero
//!   [8..16)  n_entries, u64 LE
//!   per entry, in content.index order, then the top files:
//!     type u8 (0 file, 1 folder, 2 link) · path_len u16 LE · path ·
//!     mode u32 LE (low 9 bits) · size u64 LE · data (file) or target (link)
//! ```
//!
//! Version 2 adds folders and in-tree links, which layers need. Unpacking
//! trusts nothing: it runs in `opifex-check` (chrooted into an empty folder,
//! as `_opifex`), refuses odd paths, links leaving the tree, existing names
//! and anything over the size it was told to expect. The real check (the
//! signature and every file against the signed index) happens afterwards in
//! `opifexd`, on what was unpacked.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

use crate::content::{self, Entry, Kind, TOP};

pub const MAGIC: &[u8; 4] = b"INSB";
pub const VERSION: u8 = 2;
const MAX_ENTRIES: u64 = 500_000;

fn put(out: &mut Vec<u8>, kind: u8, path: &str, mode: u32, data: &[u8]) {
    out.push(kind);
    out.extend_from_slice(&(path.len() as u16).to_le_bytes());
    out.extend_from_slice(path.as_bytes());
    out.extend_from_slice(&mode.to_le_bytes());
    out.extend_from_slice(&(data.len() as u64).to_le_bytes());
    out.extend_from_slice(data);
}

/// Pack a packed bundle or layer folder: everything its content.index lists,
/// then atrium.toml, its signature and the index itself.
pub fn pack(tree: &Path) -> Result<Vec<u8>, String> {
    let idx = fs::read_to_string(tree.join(content::CONTENT_INDEX))
        .map_err(|e| format!("{}: {e} (run opifex pack first)", content::CONTENT_INDEX))?;
    let entries = content::parse_index(&idx)?;
    let mut body = Vec::new();
    let mut n = 0u64;
    for e in &entries {
        match e.kind {
            Kind::Dir => put(&mut body, 1, &e.path, e.mode, &[]),
            Kind::Link => put(&mut body, 2, &e.path, 0o777, e.target.as_bytes()),
            Kind::File => {
                let data = fs::read(tree.join(&e.path)).map_err(|err| format!("{}: {err}", e.path))?;
                put(&mut body, 0, &e.path, e.mode, &data);
            }
        }
        n += 1;
    }
    for f in TOP {
        if let Ok(data) = fs::read(tree.join(f)) {
            put(&mut body, 0, f, 0o444, &data);
            n += 1;
        }
    }
    let mut out = Vec::with_capacity(body.len() + 16);
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.extend_from_slice(&[0, 0, 0]);
    out.extend_from_slice(&n.to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

struct Reader<R: Read> { r: R, left: u64 }

impl<R: Read> Reader<R> {
    fn take(&mut self, n: u64) -> Result<Vec<u8>, String> {
        if n > self.left {
            return Err("archive: longer than expected".into());
        }
        let mut b = vec![0u8; n as usize];
        self.r.read_exact(&mut b).map_err(|_| "archive: cut short".to_string())?;
        self.left -= n;
        Ok(b)
    }
    fn u16(&mut self) -> Result<u16, String> { Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap())) }
    fn u32(&mut self) -> Result<u32, String> { Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap())) }
    fn u64(&mut self) -> Result<u64, String> { Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap())) }
}

fn ok_name(p: &str) -> bool {
    !p.is_empty() && p.len() <= 1024 && !p.starts_with('/')
        && p.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
        && !p.bytes().any(|c| c < 0x20 || c == 0x7f)
}

/// Unpack `size` bytes of archive from `r` into `dest` (an empty folder).
/// Files are created new (never through an existing name or a link), modes
/// keep only the low 9 bits, links must stay inside, folders come first.
pub fn unpack(r: impl Read, size: u64, dest: &Path) -> Result<u64, String> {
    let mut a = Reader { r, left: size };
    let head = a.take(8)?;
    if &head[..4] != MAGIC {
        return Err("archive: not an .insula file".into());
    }
    if head[4] != VERSION {
        return Err(format!("archive: version {} (want {VERSION})", head[4]));
    }
    let n = a.u64()?;
    if n > MAX_ENTRIES {
        return Err("archive: too many entries".into());
    }
    for _ in 0..n {
        let kind = a.take(1)?[0];
        let len = a.u16()?;
        let path = String::from_utf8(a.take(len as u64)?).map_err(|_| "archive: a name that isn't UTF-8")?;
        let mode = a.u32()? & 0o777;
        let sz = a.u64()?;
        if !ok_name(&path) {
            return Err(format!("archive: bad name {path:?}"));
        }
        let to = dest.join(&path);
        if let Some(parent) = Path::new(&path).parent().filter(|p| !p.as_os_str().is_empty()) {
            let m = fs::symlink_metadata(dest.join(parent)).map_err(|_| format!("archive: {path} before its folder"))?;
            if !m.file_type().is_dir() {
                return Err(format!("archive: {path} under something not a folder"));
            }
        }
        match kind {
            0 => {
                let mut w = fs::OpenOptions::new().write(true).create_new(true).custom_flags(nofollow())
                    .mode(0o600).open(&to).map_err(|e| format!("archive: {path}: {e}"))?;
                let mut left = sz;
                while left > 0 {
                    let chunk = a.take(left.min(1 << 16))?;
                    w.write_all(&chunk).map_err(|e| format!("archive: {path}: {e}"))?;
                    left -= chunk.len() as u64;
                }
                fs::set_permissions(&to, fs::Permissions::from_mode(mode)).map_err(|e| e.to_string())?;
            }
            1 => {
                if sz != 0 {
                    return Err(format!("archive: folder {path} with data"));
                }
                fs::create_dir(&to).map_err(|e| format!("archive: {path}: {e}"))?;
                fs::set_permissions(&to, fs::Permissions::from_mode(mode | 0o700)).map_err(|e| e.to_string())?;
            }
            2 => {
                if sz > 1024 {
                    return Err(format!("archive: link {path} too long"));
                }
                let t = String::from_utf8(a.take(sz)?).map_err(|_| "archive: a link target that isn't UTF-8")?;
                if !content::link_stays_inside(&path, &t) {
                    return Err(format!("archive: {path} -> {t} leaves the tree"));
                }
                std::os::unix::fs::symlink(&t, &to).map_err(|e| format!("archive: {path}: {e}"))?;
            }
            _ => return Err(format!("archive: {path}: unknown type {kind}")),
        }
    }
    if a.left != 0 {
        return Err("archive: bytes after the last entry".into());
    }
    Ok(n)
}

fn nofollow() -> i32 {
    #[cfg(target_os = "freebsd")]
    { 0x0100 }
    #[cfg(not(target_os = "freebsd"))]
    { 0o400000 }
}

/// The entries of a tree, for tests and listings.
pub fn entries_of(tree: &Path) -> Result<Vec<Entry>, String> { content::index_tree(tree) }

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("opifex-archive-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn round_trip_then_install_checks_pass() {
        let d = tmp("rt");
        let b = d.join("b");
        fs::create_dir_all(b.join("bin")).unwrap();
        fs::create_dir_all(b.join("lib")).unwrap();
        fs::write(b.join("bin/x"), b"\x7fELF x").unwrap();
        fs::set_permissions(b.join("bin/x"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(b.join("lib/libx.so.1"), b"lib").unwrap();
        std::os::unix::fs::symlink("libx.so.1", b.join("lib/libx.so")).unwrap();
        fs::write(b.join("atrium.toml"), "[app]\nid = \"org.example.x\"\nname = \"X\"\nversion = \"1\"\nentry = \"bin/x\"\n").unwrap();
        crate::app::pack(&b, None).unwrap();
        let bytes = pack(&b).unwrap();
        assert_eq!(bytes, pack(&b).unwrap(), "deterministic");
        let out = d.join("out");
        fs::create_dir(&out).unwrap();
        unpack(&bytes[..], bytes.len() as u64, &out).unwrap();
        assert_eq!(fs::read(out.join("bin/x")).unwrap(), b"\x7fELF x");
        assert_eq!(fs::read_link(out.join("lib/libx.so")).unwrap(), Path::new("libx.so.1"));
        assert_eq!(fs::read(out.join("atrium.toml")).unwrap(), fs::read(b.join("atrium.toml")).unwrap());
        // What was unpacked still matches the signed index.
        let m = fs::read_to_string(out.join("atrium.toml")).unwrap();
        let c = portcullis_toml::Manifest::from_str(&m).unwrap().bundle.unwrap().content.unwrap();
        let entries = content::read_index(&out, portcullis_toml::layer::content_digest(&c).unwrap()).unwrap();
        let stage = d.join("stage");
        fs::create_dir(&stage).unwrap();
        content::place(&out, &stage, &entries).unwrap();
    }

    fn arc(entries: &[(u8, &str, u32, &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        for (k, p, m, data) in entries {
            put(&mut body, *k, p, *m, data);
        }
        let mut out = MAGIC.to_vec();
        out.push(VERSION);
        out.extend_from_slice(&[0, 0, 0]);
        out.extend_from_slice(&(entries.len() as u64).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    #[test]
    fn unpack_refuses() {
        let d = tmp("refuse");
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("bad name", arc(&[(0, "../x", 0o644, b"x")])),
            ("bad name", arc(&[(0, "/etc/passwd", 0o644, b"x")])),
            ("before its folder", arc(&[(0, "a/x", 0o644, b"x")])),
            ("leaves the tree", arc(&[(2, "x", 0o777, b"../../etc")])),
            ("File exists", arc(&[(0, "x", 0o644, b"1"), (0, "x", 0o644, b"2")])),
            ("unknown type", arc(&[(9, "x", 0o644, b"")])),
            ("with data", arc(&[(1, "a", 0o755, b"zz")])),
        ];
        for (i, (want, bytes)) in cases.iter().enumerate() {
            let out = d.join(format!("o{i}"));
            fs::create_dir(&out).unwrap();
            let e = unpack(&bytes[..], bytes.len() as u64, &out).unwrap_err();
            assert!(e.contains(want), "{want}: {e}");
        }
        // A link, then a file written through it: the second create refuses.
        let out = d.join("through");
        fs::create_dir(&out).unwrap();
        let bytes = arc(&[(1, "a", 0o755, b""), (2, "a/l", 0o777, b"../b"), (0, "a/l", 0o644, b"x")]);
        assert!(unpack(&bytes[..], bytes.len() as u64, &out).is_err());
        assert!(!out.join("b").exists());
        // Longer than announced.
        let bytes = arc(&[(0, "x", 0o644, b"hello")]);
        let out = d.join("long");
        fs::create_dir(&out).unwrap();
        assert!(unpack(&bytes[..], bytes.len() as u64 - 2, &out).unwrap_err().contains("expected") );
        // setuid bits are dropped.
        let bytes = arc(&[(0, "s", 0o4755, b"x")]);
        let out = d.join("suid");
        fs::create_dir(&out).unwrap();
        unpack(&bytes[..], bytes.len() as u64, &out).unwrap();
        assert_eq!(fs::metadata(out.join("s")).unwrap().permissions().mode() & 0o7777, 0o755);
    }
}
