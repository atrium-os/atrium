//! The content index: the list of every entry in a signed tree (kind, mode,
//! size, SHA-256, path, link target), named by hash in the tree's signed
//! `atrium.toml`, so one detached signature covers every byte (opifex.md §4.2).
//! Shared by app bundles and toolchain layers. Indexing refuses what a
//! signed tree may not hold (setuid/setgid/sticky, devices, sockets, pipes,
//! hard links, links leaving the tree); placing copies entry by entry into
//! a staging folder, hashing what it copies.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

pub use portcullis_toml::layer::CONTENT_INDEX;
use sha2::{Digest, Sha256};

pub const MANIFEST: &str = "atrium.toml";
pub const SIG: &str = "atrium.toml.sig";
/// The files at a tree's top that the index doesn't list (it can't list itself).
pub const TOP: [&str; 3] = [MANIFEST, SIG, CONTENT_INDEX];
const MAX_ENTRIES: usize = 500_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind { File, Dir, Link }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub kind:   Kind,
    pub mode:   u32,
    pub size:   u64,
    pub sha:    [u8; 32],
    pub path:   String,
    pub target: String,
}

pub fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }

fn unhex(s: &str) -> Option<[u8; 32]> { portcullis_toml::layer::content_digest(&format!("sha256:{s}")) }

impl Entry {
    pub fn line(&self) -> String {
        let k = match self.kind { Kind::File => "f", Kind::Dir => "d", Kind::Link => "l" };
        let sha = if self.kind == Kind::File { hex(&self.sha) } else { "-".into() };
        format!("{k}\t{:04o}\t{}\t{sha}\t{}\t{}\n", self.mode, self.size, self.path, self.target)
    }
}

/// A path in the index: relative, inside, printable, no tabs.
fn ok_path(p: &str) -> bool {
    !p.is_empty() && p.len() <= 1024 && !p.starts_with('/')
        && p.split('/').all(|c| !c.is_empty() && c != "." && c != "..")
        && !p.bytes().any(|c| c < 0x20 || c == 0x7f)
        && !(TOP.contains(&p))
}

/// Does a link at `path` pointing to `target` stay inside the tree?
/// Lexically: relative, and its `..`s never climb above the top.
pub fn link_stays_inside(path: &str, target: &str) -> bool {
    if target.is_empty() || target.starts_with('/') || target.bytes().any(|c| c < 0x20 || c == 0x7f) {
        return false;
    }
    let mut depth: i64 = path.split('/').count() as i64 - 1; // the link's folder
    for c in target.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => depth += 1,
        }
    }
    true
}

pub fn parse_index(text: &str) -> Result<Vec<Entry>, String> {
    let mut v = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let bad = |m: &str| format!("{CONTENT_INDEX} line {}: {m}", n + 1);
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 6 {
            return Err(bad("six tab-separated fields"));
        }
        let kind = match f[0] { "f" => Kind::File, "d" => Kind::Dir, "l" => Kind::Link, _ => return Err(bad("kind f, d or l")) };
        let mode = u32::from_str_radix(f[1], 8).map_err(|_| bad("mode"))?;
        if mode & !0o777 != 0 {
            return Err(bad("setuid, setgid or sticky"));
        }
        let size: u64 = f[2].parse().map_err(|_| bad("size"))?;
        let sha = if kind == Kind::File { unhex(f[3]).ok_or_else(|| bad("sha256"))? } else { [0; 32] };
        if !ok_path(f[4]) {
            return Err(bad("path"));
        }
        if kind == Kind::Link && !link_stays_inside(f[4], f[5]) {
            return Err(bad("a link that leaves the tree"));
        }
        if kind != Kind::Link && !f[5].is_empty() {
            return Err(bad("a target on something not a link"));
        }
        v.push(Entry { kind, mode, size, sha, path: f[4].into(), target: f[5].into() });
        if v.len() > MAX_ENTRIES {
            return Err(format!("{CONTENT_INDEX}: more than {MAX_ENTRIES} entries"));
        }
    }
    // Each entry's folder comes before it; no path twice.
    let mut dirs = std::collections::HashSet::new();
    let mut seen = std::collections::HashSet::new();
    for e in &v {
        if !seen.insert(e.path.as_str()) {
            return Err(format!("{CONTENT_INDEX}: {} twice", e.path));
        }
        if let Some((parent, _)) = e.path.rsplit_once('/') {
            if !dirs.contains(parent) {
                return Err(format!("{CONTENT_INDEX}: {} before its folder", e.path));
            }
        }
        if e.kind == Kind::Dir {
            dirs.insert(e.path.as_str());
        }
    }
    Ok(v)
}

pub fn hash_file(p: &Path) -> Result<([u8; 32], u64), String> {
    let mut f = fs::File::open(p).map_err(|e| format!("{}: {e}", p.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut n = 0u64;
    loop {
        let k = f.read(&mut buf).map_err(|e| format!("{}: {e}", p.display()))?;
        if k == 0 {
            break;
        }
        h.update(&buf[..k]);
        n += k as u64;
    }
    Ok((h.finalize().into(), n))
}

/// Index a tree, refusing what a signed tree may not hold.
pub fn index_tree(top: &Path) -> Result<Vec<Entry>, String> {
    let mut out = Vec::new();
    walk(top, "", &mut out)?;
    Ok(out)
}

fn walk(top: &Path, rel: &str, out: &mut Vec<Entry>) -> Result<(), String> {
    let dir = if rel.is_empty() { top.to_path_buf() } else { top.join(rel) };
    let mut names: Vec<String> = Vec::new();
    for e in fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let e = e.map_err(|e| e.to_string())?;
        let n = e.file_name().into_string().map_err(|n| format!("{}: a name that isn't UTF-8 ({n:?})", dir.display()))?;
        names.push(n);
    }
    names.sort();
    for n in names {
        let p = if rel.is_empty() { n.clone() } else { format!("{rel}/{n}") };
        if rel.is_empty() && TOP.contains(&n.as_str()) {
            continue;
        }
        if !ok_path(&p) {
            return Err(format!("{p:?}: not a name a signed tree can hold"));
        }
        let full = top.join(&p);
        let m = fs::symlink_metadata(&full).map_err(|e| format!("{}: {e}", full.display()))?;
        let mode = m.mode() & 0o7777;
        if mode & 0o7000 != 0 && !m.file_type().is_symlink() {
            return Err(format!("{p}: setuid, setgid or sticky"));
        }
        let ft = m.file_type();
        if ft.is_dir() {
            out.push(Entry { kind: Kind::Dir, mode, size: 0, sha: [0; 32], path: p.clone(), target: String::new() });
            walk(top, &p, out)?;
        } else if ft.is_symlink() {
            let t = fs::read_link(&full).map_err(|e| format!("{p}: {e}"))?;
            let t = t.to_str().ok_or_else(|| format!("{p}: a link target that isn't UTF-8"))?.to_string();
            if !link_stays_inside(&p, &t) {
                return Err(format!("{p} -> {t}: a link that leaves the tree"));
            }
            out.push(Entry { kind: Kind::Link, mode: 0o777, size: 0, sha: [0; 32], path: p, target: t });
        } else if ft.is_file() {
            if m.nlink() > 1 {
                return Err(format!("{p}: a hard link (copy it instead)"));
            }
            let (sha, size) = hash_file(&full)?;
            out.push(Entry { kind: Kind::File, mode, size, sha, path: p, target: String::new() });
        } else {
            return Err(format!("{p}: a device, socket or pipe"));
        }
    }
    Ok(())
}

pub fn index_text(entries: &[Entry]) -> String { entries.iter().map(Entry::line).collect() }

pub fn own(p: &Path) -> Result<(), String> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc_geteuid() } == 0 {
        std::os::unix::fs::lchown(p, Some(0), Some(0)).map_err(|e| format!("chown {}: {e}", p.display()))?;
    }
    Ok(())
}

extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
}

pub fn place(src: &Path, stage: &Path, entries: &[Entry]) -> Result<(), String> {
    for e in entries {
        let from = src.join(&e.path);
        let to = stage.join(&e.path);
        let m = fs::symlink_metadata(&from).map_err(|err| format!("{}: {err} (the index lists it)", e.path))?;
        match e.kind {
            Kind::Dir => {
                if !m.file_type().is_dir() {
                    return Err(format!("{}: the index says a folder", e.path));
                }
                fs::create_dir(&to).map_err(|err| format!("{}: {err}", to.display()))?;
                fs::set_permissions(&to, fs::Permissions::from_mode((e.mode & 0o755) | 0o700)).map_err(|err| err.to_string())?;
            }
            Kind::Link => {
                if !m.file_type().is_symlink() {
                    return Err(format!("{}: the index says a link", e.path));
                }
                let t = fs::read_link(&from).map_err(|err| err.to_string())?;
                if t.to_str() != Some(e.target.as_str()) {
                    return Err(format!("{}: points somewhere else than the index says", e.path));
                }
                std::os::unix::fs::symlink(&e.target, &to).map_err(|err| format!("{}: {err}", to.display()))?;
            }
            Kind::File => {
                if !m.file_type().is_file() {
                    return Err(format!("{}: the index says a file", e.path));
                }
                if m.nlink() > 1 {
                    return Err(format!("{}: a hard link", e.path));
                }
                if m.mode() & 0o7000 != 0 {
                    return Err(format!("{}: setuid, setgid or sticky", e.path));
                }
                // Hash what is copied, not what was looked at.
                let mut r = open_nofollow(&from)?;
                let mut w = fs::OpenOptions::new().write(true).create_new(true).open(&to)
                    .map_err(|err| format!("{}: {err}", to.display()))?;
                let mut h = Sha256::new();
                let mut buf = vec![0u8; 1 << 16];
                let mut n = 0u64;
                loop {
                    let k = r.read(&mut buf).map_err(|err| format!("{}: {err}", e.path))?;
                    if k == 0 {
                        break;
                    }
                    n += k as u64;
                    if n > e.size {
                        return Err(format!("{}: bigger than the index says", e.path));
                    }
                    h.update(&buf[..k]);
                    w.write_all(&buf[..k]).map_err(|err| format!("{}: {err}", to.display()))?;
                }
                let got: [u8; 32] = h.finalize().into();
                if n != e.size || got != e.sha {
                    return Err(format!("{}: not the file the index lists (changed after signing?)", e.path));
                }
                fs::set_permissions(&to, fs::Permissions::from_mode(e.mode & 0o755)).map_err(|err| err.to_string())?;
            }
        }
        own(&to)?;
    }
    Ok(())
}

pub fn open_nofollow(p: &Path) -> Result<fs::File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    #[cfg(target_os = "freebsd")]
    const O_NOFOLLOW: i32 = 0x0100;
    #[cfg(not(target_os = "freebsd"))]
    const O_NOFOLLOW: i32 = 0o400000;
    fs::OpenOptions::new().read(true).custom_flags(O_NOFOLLOW).open(p).map_err(|e| format!("{}: {e}", p.display()))
}


/// SHA-256 of an index's text, as `sha256:<hex>` (what a manifest names).
pub fn digest_text(idx: &str) -> String { format!("sha256:{}", hex(&Sha256::digest(idx.as_bytes()))) }

/// Read `<src>/content.index`, check it is the one `want` names, parse it.
pub fn read_index(src: &Path, want: [u8; 32]) -> Result<Vec<Entry>, String> {
    let idx = fs::read(src.join(CONTENT_INDEX)).map_err(|e| format!("{CONTENT_INDEX}: {e}"))?;
    let got: [u8; 32] = Sha256::digest(&idx).into();
    if got != want {
        return Err(format!("{CONTENT_INDEX} is not the one the manifest names"));
    }
    parse_index(std::str::from_utf8(&idx).map_err(|_| format!("{CONTENT_INDEX}: not UTF-8"))?)
}
