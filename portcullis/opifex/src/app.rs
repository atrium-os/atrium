//! App bundles: `opifex pack` and the checked part of `opifex install`
//! (opifex.md §4.2, §7.4, §7.5; build step C0).
//!
//! A packed bundle carries `content.index` and names its hash in
//! `[bundle] content`, so the manifest's signature covers every file. Install
//! checks the signature through `portcullis-trust` (the same gate portcullisd
//! applies at launch), then copies the tree into a staging folder entry by
//! entry, hashing what it copies against the index. Bundles without an index
//! are still accepted for bring-up (copied as before, with a warning), unless
//! `/etc/atrium/trust.toml` says `require_content = true`.
//!
//! Placing is a rename: the previous tree moves to `<root>/opifex/prev/<id>`
//! (one version kept, for rollback) and the staged tree to `<root>/apps/<id>`.
//! Staging and the previous version live outside `apps/`, which portcullisd
//! lists, so neither ever shows up as an app.

use std::fs;
use std::path::{Path, PathBuf};

use crate::content::{self, CONTENT_INDEX, MANIFEST, SIG};
use portcullis_toml::Manifest;

pub struct Opts<'a> {
    pub root:            &'a Path,
    pub trust:           &'a portcullis_trust::Trust,
    pub allow_unsigned:  bool,
    pub require_content: bool,
    /// A catalog install: the signer's key from the checked catalog. The
    /// manifest must verify against it (instead of the side-load list).
    pub catalog_key:     Option<&'a str>,
}

/// Where Opifex keeps its own state under the Atrium root.
pub fn state(root: &Path) -> PathBuf { root.join("opifex") }

/// `require_content` from the trust config. Absent file or key: false. A file
/// that doesn't parse: true (a security setting fails closed, like
/// `require_signatures`).
pub fn require_content(config: &Path) -> bool {
    match fs::read_to_string(config) {
        Err(_) => false,
        Ok(t) => match t.parse::<toml::Value>() {
            Ok(v) => v.get("require_content").and_then(|b| b.as_bool()).unwrap_or(false),
            Err(_) => true,
        },
    }
}

/// Set `[bundle] content` in manifest text, keeping everything else as written.
pub fn with_content(text: &str, digest: &str) -> String {
    let mut out = Vec::new();
    let mut in_bundle = false;
    let mut placed = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_bundle = t == "[bundle]";
            out.push(line.to_string());
            if in_bundle {
                out.push(format!("content = \"{digest}\""));
                placed = true;
            }
            continue;
        }
        if in_bundle && t.split('=').next().map(str::trim) == Some("content") {
            continue;
        }
        out.push(line.to_string());
    }
    let mut s = out.join("\n");
    s.push('\n');
    if !placed {
        s.push_str(&format!("\n[bundle]\ncontent = \"{digest}\"\n"));
    }
    s
}

/// `opifex pack <tree> [--sign key.pem]`: index the tree, name the index in
/// its manifest, and sign the manifest.
pub fn pack(tree: &Path, key_pem: Option<&str>) -> Result<Manifest, String> {
    let text = fs::read_to_string(tree.join(MANIFEST)).map_err(|e| format!("{MANIFEST}: {e}"))?;
    let entries = content::index_tree(tree)?;
    let idx = content::index_text(&entries);
    let digest = content::digest_text(&idx);
    let text = with_content(&text, &digest);
    let m = Manifest::from_str(&text).map_err(|e| format!("{MANIFEST}: {e:?}"))?;
    if m.bundle.as_ref().and_then(|b| b.content.as_deref()) != Some(digest.as_str()) {
        return Err(format!("{MANIFEST}: [bundle] content didn't take"));
    }
    let report = portcullis_toml::validate(&m);
    if !report.is_ok() {
        return Err(format!("invalid manifest: {}", report.errors.join("; ")));
    }
    if portcullis_trust::is_dev(&m.app.id) {
        return Err(format!("{}: dev. ids are signed by the machine's dev publisher, not packed", m.app.id));
    }
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

/// Check a bundle and stage it: returns (manifest, staged tree). The caller
/// finishes it (closure) and calls [`commit`].
pub fn stage(bundle: &Path, o: &Opts) -> Result<(Manifest, PathBuf), String> {
    let text = fs::read_to_string(bundle.join(MANIFEST)).map_err(|e| format!("{MANIFEST}: {e}"))?;
    let m = Manifest::from_str(&text).map_err(|e| format!("parse {MANIFEST}: {e:?}"))?;
    let report = portcullis_toml::validate(&m);
    if !report.is_ok() {
        return Err(format!("invalid manifest: {}", report.errors.join("; ")));
    }
    let id = m.app.id.clone();
    if portcullis_trust::is_dev(&id) {
        return Err(format!("{id}: dev. ids are Run from their workspace, not installed"));
    }
    if id.is_empty() || id.contains('/') || id.starts_with('.') {
        return Err(format!("{id:?}: not an app id"));
    }
    // 1. The signature, through the launch gate.
    if let Some(pem) = o.catalog_key {
        let raw = fs::read(bundle.join(SIG)).map_err(|e| format!("{SIG}: {e}"))?;
        let der = std::str::from_utf8(&raw).ok().and_then(|t| portcullis_sig::sig_from_base64(t.trim()).ok())
            .unwrap_or(raw);
        portcullis_sig::verify_one(text.as_bytes(), &der, pem)
            .map_err(|e| format!("{id}: not signed by the catalog's signer ({e:?})"))?;
        if m.bundle.as_ref().and_then(|b| b.content.as_deref()).is_none() {
            return Err(format!("{id}: a catalog package without a content index"));
        }
    } else if o.allow_unsigned {
        eprintln!("opifex: --allow-unsigned: skipping signature verification for {}", bundle.display());
    } else {
        o.trust.verify(bundle, &text, portcullis_trust::Demand::PolicyDefault)?;
    }
    // 2. Stage, checking every file against the signed index when there is one.
    let staging = state(o.root).join("staging");
    fs::create_dir_all(&staging).map_err(|e| format!("{}: {e}", staging.display()))?;
    let stage = staging.join(format!("{id}.{}", std::process::id()));
    let _ = fs::remove_dir_all(&stage);
    fs::create_dir(&stage).map_err(|e| format!("{}: {e}", stage.display()))?;
    let res = match m.bundle.as_ref().and_then(|b| b.content.as_deref()) {
        Some(c) => {
            let want = portcullis_toml::layer::content_digest(c)
                .ok_or_else(|| format!("[bundle] content {c:?}: sha256:<64 hex digits>"))?;
            content::read_index(bundle, want)
                .and_then(|entries| content::place(bundle, &stage, &entries))
                .and_then(|()| copy_top(bundle, &stage))
        }
        None if o.require_content => Err(format!(
            "{id} has no content index ([bundle] content): its files aren't covered by its \
             signature, and this machine requires that (run `opifex pack`)")),
        None => {
            eprintln!("opifex: WARNING {id} has no content index: only atrium.toml is signed, \
                       its files are not (run `opifex pack`)");
            copy_tree(bundle, &stage)
        }
    };
    if let Err(e) = res {
        let _ = fs::remove_dir_all(&stage);
        return Err(e);
    }
    Ok((m, stage))
}

fn copy_top(src: &Path, stage: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    for f in content::TOP {
        if src.join(f).is_file() {
            fs::copy(src.join(f), stage.join(f)).map_err(|e| format!("{f}: {e}"))?;
            let _ = fs::set_permissions(stage.join(f), fs::Permissions::from_mode(0o444));
        }
    }
    Ok(())
}

/// Swap the staged tree in: current → prev (dropping the older prev), staged →
/// `apps/<id>`. Returns the installed path.
pub fn commit(root: &Path, id: &str, stage: &Path) -> Result<PathBuf, String> {
    let apps = root.join("apps");
    fs::create_dir_all(&apps).map_err(|e| format!("{}: {e}", apps.display()))?;
    let dest = apps.join(id);
    let prev_dir = state(root).join("prev");
    fs::create_dir_all(&prev_dir).map_err(|e| format!("{}: {e}", prev_dir.display()))?;
    let prev = prev_dir.join(id);
    if fs::symlink_metadata(&dest).is_ok() {
        if fs::symlink_metadata(&prev).is_ok() {
            fs::remove_dir_all(&prev).map_err(|e| format!("remove {}: {e}", prev.display()))?;
        }
        fs::rename(&dest, &prev).map_err(|e| format!("{} -> {}: {e}", dest.display(), prev.display()))?;
    }
    if let Err(e) = fs::rename(stage, &dest) {
        // Put the old one back rather than leave the app missing.
        let _ = fs::rename(&prev, &dest);
        return Err(format!("{} -> {}: {e}", stage.display(), dest.display()));
    }
    Ok(dest)
}

/// `opifex rollback <id>`: swap `apps/<id>` and the kept previous version.
pub fn rollback(root: &Path, id: &str) -> Result<PathBuf, String> {
    let dest = root.join("apps").join(id);
    let prev = state(root).join("prev").join(id);
    if fs::symlink_metadata(&prev).is_err() {
        return Err(format!("{id}: no previous version kept"));
    }
    let tmp = state(root).join("staging").join(format!("{id}.rollback.{}", std::process::id()));
    fs::create_dir_all(tmp.parent().unwrap()).map_err(|e| e.to_string())?;
    let had = fs::symlink_metadata(&dest).is_ok();
    if had {
        fs::rename(&dest, &tmp).map_err(|e| format!("{}: {e}", dest.display()))?;
    }
    fs::rename(&prev, &dest).map_err(|e| format!("{}: {e}", prev.display()))?;
    if had {
        fs::rename(&tmp, &prev).map_err(|e| format!("{}: {e}", prev.display()))?;
    }
    Ok(dest)
}

/// Unchecked recursive copy, for bring-up bundles without a content index.
fn copy_tree(src: &Path, dst: &Path) -> Result<(), String> {
    for e in fs::read_dir(src).map_err(|e| format!("read {}: {e}", src.display()))?.flatten() {
        let from = e.path();
        let to = dst.join(e.file_name());
        let ft = e.file_type().map_err(|e| format!("stat {}: {e}", from.display()))?;
        if ft.is_dir() {
            fs::create_dir_all(&to).map_err(|e| format!("mkdir {}: {e}", to.display()))?;
            copy_tree(&from, &to)?;
        } else if ft.is_file() {
            fs::copy(&from, &to).map_err(|e| format!("copy {}: {e}", from.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("opifex-app-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    const MAN: &str = "[app]\nid = \"org.example.hello\"\nname = \"Hello\"\nversion = \"1\"\nentry = \"bin/hello\"\n";

    fn bundle(tag: &str, man: &str) -> PathBuf {
        let t = tmp(tag).join("b");
        fs::create_dir_all(t.join("bin")).unwrap();
        fs::write(t.join("bin/hello"), b"\x7fELF hello").unwrap();
        fs::set_permissions(t.join("bin/hello"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(t.join("atrium.toml"), man).unwrap();
        t
    }

    fn keys(dir: &Path) -> String {
        let (sk, pk) = portcullis_sig::new_key([7; 32]).unwrap();
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("k.pem"), pk).unwrap();
        sk
    }

    fn opts<'a>(root: &'a Path, trust: &'a portcullis_trust::Trust, req: bool) -> Opts<'a> {
        Opts { root, trust, allow_unsigned: false, require_content: req, catalog_key: None }
    }

    #[test]
    fn content_line_lands_in_bundle() {
        let t = with_content(MAN, "sha256:ab");
        assert!(t.ends_with("[bundle]\ncontent = \"sha256:ab\"\n"), "{t}");
        let t2 = with_content(&format!("{MAN}\n[bundle]\nentry = \"bin/hello\"\ncontent = \"sha256:old\"\n"), "sha256:cd");
        assert!(t2.contains("[bundle]\ncontent = \"sha256:cd\"\nentry"), "{t2}");
        assert!(!t2.contains("old"));
    }

    #[test]
    fn pack_install_update_rollback() {
        let d = tmp("cycle");
        let b = bundle("cycle", MAN);
        let sk = keys(&d.join("pubs"));
        let trust = portcullis_trust::Trust::new(d.join("pubs"), true);
        let root = d.join("root");
        pack(&b, Some(&sk)).unwrap();
        let (m, st) = stage(&b, &opts(&root, &trust, true)).unwrap();
        let dest = commit(&root, &m.app.id, &st).unwrap();
        assert_eq!(fs::read(dest.join("bin/hello")).unwrap(), b"\x7fELF hello");
        assert!(dest.join(CONTENT_INDEX).is_file());
        // An update keeps the old one.
        fs::write(b.join("bin/hello"), b"\x7fELF hello v2").unwrap();
        pack(&b, Some(&sk)).unwrap();
        let (_, st) = stage(&b, &opts(&root, &trust, true)).unwrap();
        commit(&root, "org.example.hello", &st).unwrap();
        assert_eq!(fs::read(dest.join("bin/hello")).unwrap(), b"\x7fELF hello v2");
        rollback(&root, "org.example.hello").unwrap();
        assert_eq!(fs::read(dest.join("bin/hello")).unwrap(), b"\x7fELF hello");
        assert_eq!(fs::read(state(&root).join("prev/org.example.hello/bin/hello")).unwrap(), b"\x7fELF hello v2");
        // Nothing but the app in apps/.
        assert_eq!(fs::read_dir(root.join("apps")).unwrap().count(), 1);
    }

    #[test]
    fn what_install_refuses() {
        let d = tmp("refuse");
        let sk = keys(&d.join("pubs"));
        let trust = portcullis_trust::Trust::new(d.join("pubs"), true);
        let root = d.join("root");
        // A file changed after signing.
        let b = bundle("r1", MAN);
        pack(&b, Some(&sk)).unwrap();
        fs::write(b.join("bin/hello"), b"evil").unwrap();
        assert!(stage(&b, &opts(&root, &trust, true)).unwrap_err().contains("not the file"));
        // A setuid file slipped in after packing.
        let b = bundle("r2", MAN);
        pack(&b, Some(&sk)).unwrap();
        fs::set_permissions(b.join("bin/hello"), fs::Permissions::from_mode(0o4755)).unwrap();
        assert!(stage(&b, &opts(&root, &trust, true)).unwrap_err().contains("setuid"));
        // A manifest edited after signing.
        let b = bundle("r3", MAN);
        pack(&b, Some(&sk)).unwrap();
        let t = fs::read_to_string(b.join(MANIFEST)).unwrap().replace("Hello", "Evil");
        fs::write(b.join(MANIFEST), t).unwrap();
        assert!(stage(&b, &opts(&root, &trust, true)).unwrap_err().contains("not signed"));
        // No index when the machine requires one.
        let b = bundle("r4", MAN);
        let text = fs::read_to_string(b.join(MANIFEST)).unwrap();
        let sig = portcullis_sig::sign(text.as_bytes(), &sk).unwrap();
        use base64::Engine;
        fs::write(b.join(SIG), base64::engine::general_purpose::STANDARD.encode(sig)).unwrap();
        assert!(stage(&b, &opts(&root, &trust, true)).unwrap_err().contains("no content index"));
        // ...accepted with a warning when it doesn't.
        assert!(stage(&b, &opts(&root, &trust, false)).is_ok());
        // dev. ids.
        let b = bundle("r5", &MAN.replace("org.example.hello", "dev.alice.ws.org.example.hello"));
        assert!(stage(&b, &opts(&root, &trust, false)).unwrap_err().contains("dev."));
        // Unsigned, with signatures required.
        let b = bundle("r6", MAN);
        pack(&b, None).unwrap();
        assert!(stage(&b, &opts(&root, &trust, true)).is_err());
        // Nothing reached apps/.
        assert!(!root.join("apps/org.example.hello").exists());
    }

    #[test]
    fn trust_config() {
        let d = tmp("trustcfg");
        assert!(!require_content(&d.join("absent.toml")));
        fs::write(d.join("t.toml"), "require_content = true\n").unwrap();
        assert!(require_content(&d.join("t.toml")));
        fs::write(d.join("bad.toml"), "require_content = = \n").unwrap();
        assert!(require_content(&d.join("bad.toml")));
    }
}
