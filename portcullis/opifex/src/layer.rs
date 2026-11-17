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
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::content::{hex, index_text, index_tree, own, place, read_index, CONTENT_INDEX, MANIFEST, SIG, TOP};
#[cfg(test)]
use crate::content::{link_stays_inside, parse_index};
use portcullis_toml::layer::{self as tl, LayerManifest};
use sha2::{Digest, Sha256};

/// Keys a layer must be signed by (the platform's own, not publishers').
pub const PLATFORM_KEYS: &str = "/etc/atrium/platform-keys";
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
        let raw = fs::read(src.join(SIG)).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => format!("{} is not signed (no {SIG})", m.reference()),
            _ => format!("{SIG}: {e}"),
        })?;
        let sig = std::str::from_utf8(&raw).ok().and_then(|s| portcullis_sig::sig_from_base64(s).ok())
            .ok_or_else(|| format!("{SIG}: not base64"))?;
        portcullis_sig::verify_trusted(text.as_bytes(), &sig, o.keys)
            .map_err(|e| format!("{} is not signed by a platform key ({e:?})", m.reference()))?;
    }
    if m.layer.arch != o.arch {
        return Err(format!("{} is for {}, this is {}", m.reference(), m.layer.arch, o.arch));
    }
    // 2. The index, as signed.
    let entries = read_index(src, want)?;
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
