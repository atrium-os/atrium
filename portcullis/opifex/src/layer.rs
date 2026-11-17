//! `opifex layer` — toolchain layers (dev-tools-plan.md M3).
//!
//! ```text
//! opifex layer pack <tree> --manifest <atrium.toml> [--sign <key.pem>]
//! opifex layer install <dir> [--root <dir>] [--keys <dir>] [--arch <a>] [--allow-unsigned]
//! opifex layer list [--root <dir>]
//! opifex layer remove <name@version> [--root <dir>]
//! ```
//!
//! A packed layer is its tree with three files at the top: `atrium.toml`
//! (portcullis_toml::layer), `content.index` and `atrium.toml.sig`. The
//! index lists every entry (kind, mode, size, SHA-256, path, link target);
//! the manifest names the index's SHA-256 (`[layer] content`), so the one
//! detached signature (portcullis-sig: P-256 ECDSA, base64) covers every
//! byte. Install checks the signature against the **platform** keys only
//! (not third-party publishers), then copies entry by entry into a staging
//! folder, hashing what it copies, refusing anything the index doesn't say
//! or anything unsafe (setuid/setgid, devices, hard links, links leaving
//! the tree), and renames the staging folder into
//! `<root>/layers/<name>@<version>` (the install contract portcullisd reads).
//! The package catalog builds fetch and updates on top of this.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use portcullis_toml::layer::{self as tl, LayerManifest, CONTENT_INDEX};
use sha2::{Digest, Sha256};

/// Keys a layer must be signed by (the platform's own, not publishers').
pub const PLATFORM_KEYS: &str = "/etc/atrium/platform-keys";
const MANIFEST: &str = "atrium.toml";
const SIG: &str = "atrium.toml.sig";
const TOP: [&str; 3] = [MANIFEST, SIG, CONTENT_INDEX];
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

fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }

fn unhex(s: &str) -> Option<[u8; 32]> { tl::content_digest(&format!("sha256:{s}")) }

impl Entry {
    fn line(&self) -> String {
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
            return Err(bad("a link that leaves the layer"));
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

fn hash_file(p: &Path) -> Result<([u8; 32], u64), String> {
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

/// Index a tree, refusing what a layer may not hold.
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
            return Err(format!("{p:?}: not a name a layer can hold"));
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
                return Err(format!("{p} -> {t}: a link that leaves the layer"));
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

/// `opifex layer pack`: index `tree`, write `content.index`, the manifest
/// with `content` filled in, and (with a key) its signature.
pub fn pack(tree: &Path, template: &str, key_pem: Option<&str>) -> Result<LayerManifest, String> {
    let mut m = LayerManifest::from_str(template)?;
    let entries = index_tree(tree)?;
    let idx = index_text(&entries);
    m.layer.content = format!("sha256:{}", hex(&Sha256::digest(idx.as_bytes())));
    m.validate()?;
    let text = toml::to_string(&m).map_err(|e| e.to_string())?;
    fs::write(tree.join(CONTENT_INDEX), &idx).map_err(|e| format!("write {CONTENT_INDEX}: {e}"))?;
    fs::write(tree.join(MANIFEST), &text).map_err(|e| format!("write {MANIFEST}: {e}"))?;
    let _ = fs::remove_file(tree.join(SIG));
    if let Some(k) = key_pem {
        use base64::Engine;
        let sig = portcullis_sig::sign(text.as_bytes(), k).map_err(|e| format!("sign: {e:?}"))?;
        fs::write(tree.join(SIG), base64::engine::general_purpose::STANDARD.encode(sig) + "\n")
            .map_err(|e| format!("write {SIG}: {e}"))?;
    }
    Ok(m)
}

pub struct InstallOpts<'a> {
    pub root:           &'a Path,
    pub keys:           &'a [String],
    pub arch:           &'a str,
    pub allow_unsigned: bool,
}

/// `opifex layer install`: check everything, then place it.
pub fn install(src: &Path, o: &InstallOpts) -> Result<(LayerManifest, PathBuf), String> {
    let text = fs::read_to_string(src.join(MANIFEST)).map_err(|e| format!("{MANIFEST}: {e}"))?;
    let m = LayerManifest::from_str(&text)?;
    let want = m.require_content()?;
    // 1. The signature: platform keys only.
    if o.allow_unsigned {
        eprintln!("opifex: --allow-unsigned: {} not checked against the platform key", m.reference());
    } else {
        if o.keys.is_empty() {
            return Err(format!("no platform keys in {PLATFORM_KEYS}: a layer can't be checked"));
        }
        let raw = fs::read(src.join(SIG)).map_err(|e| format!("{SIG}: {e}"))?;
        let sig = std::str::from_utf8(&raw).ok().and_then(|s| portcullis_sig::sig_from_base64(s).ok())
            .ok_or_else(|| format!("{SIG}: not base64"))?;
        portcullis_sig::verify_trusted(text.as_bytes(), &sig, o.keys)
            .map_err(|e| format!("{} is not signed by a platform key ({e:?})", m.reference()))?;
    }
    if m.layer.arch != o.arch {
        return Err(format!("{} is for {}, this is {}", m.reference(), m.layer.arch, o.arch));
    }
    // 2. The index, as signed.
    let idx = fs::read(src.join(CONTENT_INDEX)).map_err(|e| format!("{CONTENT_INDEX}: {e}"))?;
    let got: [u8; 32] = Sha256::digest(&idx).into();
    if got != want {
        return Err(format!("{CONTENT_INDEX} is not the one the manifest names"));
    }
    let entries = parse_index(std::str::from_utf8(&idx).map_err(|_| format!("{CONTENT_INDEX}: not UTF-8"))?)?;
    // 3. Copy into staging, checking each entry as it is copied.
    let layers = o.root.join(tl::INSTALLED);
    fs::create_dir_all(&layers).map_err(|e| format!("{}: {e}", layers.display()))?;
    let dest = layers.join(m.reference());
    if fs::symlink_metadata(&dest).is_ok() {
        return Err(format!("{} is installed already (remove it first)", m.reference()));
    }
    let stage = layers.join(format!(".staging-{}-{}", m.reference(), std::process::id()));
    let _ = fs::remove_dir_all(&stage);
    fs::create_dir(&stage).map_err(|e| format!("{}: {e}", stage.display()))?;
    let res = place(src, &stage, &entries).and_then(|()| {
        for f in TOP {
            if src.join(f).is_file() {
                fs::copy(src.join(f), stage.join(f)).map_err(|e| format!("{f}: {e}"))?;
                let _ = fs::set_permissions(stage.join(f), fs::Permissions::from_mode(0o444));
            }
        }
        own(&stage)?;
        fs::set_permissions(&stage, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
        fs::rename(&stage, &dest).map_err(|e| format!("{}: {e}", dest.display()))
    });
    if let Err(e) = res {
        let _ = fs::remove_dir_all(&stage);
        return Err(e);
    }
    Ok((m, dest))
}

fn own(p: &Path) -> Result<(), String> {
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

fn place(src: &Path, stage: &Path, entries: &[Entry]) -> Result<(), String> {
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

fn open_nofollow(p: &Path) -> Result<fs::File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    #[cfg(target_os = "freebsd")]
    const O_NOFOLLOW: i32 = 0x0100;
    #[cfg(not(target_os = "freebsd"))]
    const O_NOFOLLOW: i32 = 0o400000;
    fs::OpenOptions::new().read(true).custom_flags(O_NOFOLLOW).open(p).map_err(|e| format!("{}: {e}", p.display()))
}

/// Installed layers: (reference, manifest).
pub fn list(root: &Path) -> Vec<(String, LayerManifest)> {
    let mut v = Vec::new();
    if let Ok(rd) = fs::read_dir(root.join(tl::INSTALLED)) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if tl::parse_ref(&n).is_none() {
                continue;
            }
            if let Some(m) = fs::read_to_string(e.path().join(MANIFEST)).ok().and_then(|t| LayerManifest::from_str(&t).ok()) {
                if m.reference() == n {
                    v.push((n, m));
                }
            }
        }
    }
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

/// Remove an installed layer, unless something has it mounted.
pub fn remove(root: &Path, reference: &str) -> Result<PathBuf, String> {
    let d = tl::installed_dir(root, reference).ok_or_else(|| format!("{reference:?}: name@version"))?;
    let m = fs::symlink_metadata(&d).map_err(|_| format!("{reference} is not installed"))?;
    if !m.is_dir() {
        return Err(format!("{} is not a folder", d.display()));
    }
    if mounted_from(&d)? {
        return Err(format!("{reference} is mounted in a running workspace: stop it first"));
    }
    fs::remove_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    Ok(d)
}

fn mounted_from(d: &Path) -> Result<bool, String> {
    if !cfg!(target_os = "freebsd") {
        return Ok(false);
    }
    let o = std::process::Command::new("mount").arg("-p").output().map_err(|e| format!("mount -p: {e}"))?;
    Ok(String::from_utf8_lossy(&o.stdout).lines()
        .filter_map(|l| l.split_whitespace().next())
        .any(|src| Path::new(src).starts_with(d)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("opifex-layer-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    const TEMPLATE: &str = "kind = \"layer\"\n[layer]\nname = \"rust\"\nversion = \"1.100.0\"\narch = \"amd64\"\nmount = \"/atrium/toolchains/rust\"\npath = [\"bin\"]\n";

    fn tree(tag: &str) -> PathBuf {
        let t = tmp(tag);
        fs::create_dir_all(t.join("bin")).unwrap();
        fs::create_dir_all(t.join("lib/rustlib")).unwrap();
        fs::write(t.join("bin/rustc"), b"\x7fELF rustc").unwrap();
        fs::set_permissions(t.join("bin/rustc"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(t.join("lib/librustc_driver.so"), vec![7u8; 200_000]).unwrap();
        std::os::unix::fs::symlink("../lib/librustc_driver.so", t.join("bin/driver")).unwrap();
        t
    }

    fn keys() -> (String, String) { portcullis_sig::new_key([5; 32]).unwrap() }

    #[test]
    fn pack_then_install() {
        let t = tree("ok");
        let (k, p) = keys();
        let m = pack(&t, TEMPLATE, Some(&k)).unwrap();
        assert!(m.layer.content.starts_with("sha256:"));
        let root = tmp("ok-root");
        let (m2, dest) = install(&t, &InstallOpts { root: &root, keys: &[p.clone()], arch: "amd64", allow_unsigned: false }).unwrap();
        assert_eq!(m2.reference(), "rust@1.100.0");
        assert_eq!(dest, root.join("layers/rust@1.100.0"));
        assert_eq!(fs::read(dest.join("bin/rustc")).unwrap(), b"\x7fELF rustc");
        assert_eq!(fs::read_link(dest.join("bin/driver")).unwrap(), Path::new("../lib/librustc_driver.so"));
        assert_eq!(list(&root).len(), 1);
        // A version is installed once.
        assert!(install(&t, &InstallOpts { root: &root, keys: &[p], arch: "amd64", allow_unsigned: false }).is_err());
        remove(&root, "rust@1.100.0").unwrap();
        assert!(list(&root).is_empty());
        // No staging folder left behind.
        assert_eq!(fs::read_dir(root.join("layers")).unwrap().count(), 0);
    }

    fn refused(tag: &str, spoil: impl Fn(&Path), opts_key: Option<String>, arch: &str) -> String {
        let t = tree(tag);
        let (k, p) = keys();
        pack(&t, TEMPLATE, Some(&k)).unwrap();
        spoil(&t);
        let root = tmp(&format!("{tag}-root"));
        let keyset = vec![opts_key.unwrap_or(p)];
        let e = install(&t, &InstallOpts { root: &root, keys: &keyset, arch, allow_unsigned: false }).unwrap_err();
        assert!(!root.join("layers/rust@1.100.0").exists(), "{tag}");
        assert_eq!(fs::read_dir(root.join("layers")).map(|r| r.count()).unwrap_or(0), 0, "{tag}: staging left");
        e
    }

    #[test]
    fn what_install_refuses() {
        // A file changed after signing.
        let e = refused("tamper", |t| fs::write(t.join("bin/rustc"), b"\x7fELF evil!").unwrap(), None, "amd64");
        assert!(e.contains("not the file"), "{e}");
        // Same size, different bytes, in the big file.
        let e = refused("tamper2", |t| { let mut b = fs::read(t.join("lib/librustc_driver.so")).unwrap(); b[150_000] ^= 1; fs::write(t.join("lib/librustc_driver.so"), b).unwrap(); }, None, "amd64");
        assert!(e.contains("not the file"), "{e}");
        // The index edited.
        let e = refused("index", |t| { let s = fs::read_to_string(t.join(CONTENT_INDEX)).unwrap(); fs::write(t.join(CONTENT_INDEX), s.replace("0755", "0777")).unwrap(); }, None, "amd64");
        assert!(e.contains("not the one"), "{e}");
        // The manifest edited (an env added) — the signature no longer holds.
        let e = refused("manifest", |t| { let s = fs::read_to_string(t.join(MANIFEST)).unwrap(); assert!(s.contains("summary = \"\"")); fs::write(t.join(MANIFEST), s.replace("summary = \"\"", "summary = \"x\"")).unwrap(); }, None, "amd64");
        assert!(e.contains("not signed") || e.contains("platform"), "{e}");
        // Another key.
        let other = portcullis_sig::new_key([6; 32]).unwrap().1;
        let e = refused("key", |_| {}, Some(other), "amd64");
        assert!(e.contains("not signed"), "{e}");
        // Another arch.
        let e = refused("arch", |_| {}, None, "aarch64");
        assert!(e.contains("is for amd64"), "{e}");
        // A link swapped for one that leaves the tree.
        let e = refused("link", |t| { fs::remove_file(t.join("bin/driver")).unwrap(); std::os::unix::fs::symlink("/etc/passwd", t.join("bin/driver")).unwrap(); }, None, "amd64");
        assert!(e.contains("somewhere else"), "{e}");
        // A file swapped for a link.
        let e = refused("swap", |t| { fs::remove_file(t.join("bin/rustc")).unwrap(); std::os::unix::fs::symlink("../lib/librustc_driver.so", t.join("bin/rustc")).unwrap(); }, None, "amd64");
        assert!(e.contains("says a file"), "{e}");
        // A setuid bit added after signing.
        let e = refused("suid", |t| fs::set_permissions(t.join("bin/rustc"), fs::Permissions::from_mode(0o4755)).unwrap(), None, "amd64");
        assert!(e.contains("setuid"), "{e}");
        // A hard link made after signing.
        let e = refused("hard", |t| fs::hard_link(t.join("bin/rustc"), t.join("lib/x")).unwrap(), None, "amd64");
        assert!(e.contains("hard link"), "{e}");
    }

    #[test]
    fn what_pack_refuses() {
        let t = tree("pack-escape");
        std::os::unix::fs::symlink("../../etc/passwd", t.join("bin/pw")).unwrap();
        assert!(pack(&t, TEMPLATE, None).unwrap_err().contains("leaves"));
        let t = tree("pack-abs");
        std::os::unix::fs::symlink("/usr/local/lib/libz.so", t.join("lib/libz.so")).unwrap();
        assert!(pack(&t, TEMPLATE, None).unwrap_err().contains("leaves"));
        let t = tree("pack-suid");
        fs::set_permissions(t.join("bin/rustc"), fs::Permissions::from_mode(0o4755)).unwrap();
        assert!(pack(&t, TEMPLATE, None).unwrap_err().contains("setuid"));
        let t = tree("pack-hard");
        fs::hard_link(t.join("bin/rustc"), t.join("bin/rustc2")).unwrap();
        assert!(pack(&t, TEMPLATE, None).unwrap_err().contains("hard link"));
    }

    #[test]
    fn unsigned_needs_the_flag() {
        let t = tree("unsigned");
        pack(&t, TEMPLATE, None).unwrap();
        let root = tmp("unsigned-root");
        let (_, p) = keys();
        assert!(install(&t, &InstallOpts { root: &root, keys: &[p], arch: "amd64", allow_unsigned: false }).is_err());
        assert!(install(&t, &InstallOpts { root: &root, keys: &[], arch: "amd64", allow_unsigned: false }).unwrap_err().contains("no platform keys"));
        install(&t, &InstallOpts { root: &root, keys: &[], arch: "amd64", allow_unsigned: true }).unwrap();
    }

    #[test]
    fn links_inside() {
        assert!(link_stays_inside("bin/x", "../lib/y"));
        assert!(link_stays_inside("x", "y"));
        assert!(link_stays_inside("a/b/c", "../../d"));
        assert!(!link_stays_inside("a/b/c", "../../../d"));
        assert!(!link_stays_inside("x", "../y"));
        assert!(!link_stays_inside("a/x", "/etc"));
        assert!(!link_stays_inside("a/x", ""));
    }

    #[test]
    fn index_rules() {
        let sha = "ab".repeat(32);
        assert!(parse_index(&format!("d\t0755\t0\t-\tbin\t\nf\t0755\t3\t{sha}\tbin/x\t\n")).is_ok());
        assert!(parse_index(&format!("f\t0755\t3\t{sha}\tbin/x\t\n")).is_err(), "folder first");
        assert!(parse_index(&format!("f\t4755\t3\t{sha}\tx\t\n")).is_err());
        assert!(parse_index(&format!("f\t0755\t3\t{sha}\t../x\t\n")).is_err());
        assert!(parse_index(&format!("f\t0755\t3\t{sha}\tatrium.toml\t\n")).is_err());
        assert!(parse_index("l\t0777\t0\t-\tx\t../y\n").is_err());
        assert!(parse_index(&format!("f\t0755\t3\t{sha}\tx\t\nf\t0755\t3\t{sha}\tx\t\n")).is_err());
    }
}
