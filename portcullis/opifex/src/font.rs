//! Font packages (opifex.md §8.1, fonts.md §5; build step C6).
//!
//! ```text
//! opifex font pack <tree> --manifest <atrium.toml> [--sign <key.pem>]
//! ```
//!
//! A packed font package is a flat folder: the `.ttf` faces its manifest
//! names (opifex_catalog::font), any licence or readme files, and the three
//! top files every signed tree has (`atrium.toml`, `content.index`,
//! `atrium.toml.sig`). opifexd installs it into
//! `<fonts>/<id>/` (`/usr/share/fonts/atrium-extra`) and rewrites
//! `<fonts>/pins` atomically: one line per face, in the format
//! `atrium_fonts::extra` reads, with the BLAKE3 and size of the file as
//! placed, so the library maps a face only if its bytes are the ones
//! installed. frescod notices the new list by itself (fonts.md §5); apps see
//! the faces at their next start.

use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::content::{index_text, index_tree, own, place, read_index, Kind, CONTENT_INDEX, MANIFEST, SIG, TOP};
use opifex_catalog::font::FontManifest;

pub const SYSTEM_FONTS: &str = "/usr/share/fonts/atrium-extra";
pub const PINS: &str = "pins";
const HEADER: &str = "# atrium-fonts pins 1";

/// `opifex font pack`: index the folder, fill in `content`, sign.
pub fn pack(tree: &Path, template: &str, key_pem: Option<&str>) -> Result<FontManifest, String> {
    let mut m: FontManifest = toml::from_str(template).map_err(|e| format!("font manifest: {e}"))?;
    let entries = index_tree(tree)?;
    flat(&m, &entries)?;
    let idx = index_text(&entries);
    m.font.content = crate::content::digest_text(&idx);
    m.validate()?;
    for f in &m.faces {
        truetype(&tree.join(&f.file))?;
    }
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

/// Only plain files at the top, and every face among them.
fn flat(m: &FontManifest, entries: &[crate::content::Entry]) -> Result<(), String> {
    for e in entries {
        if e.kind != Kind::File || e.path.contains('/') {
            return Err(format!("{}: a font package holds only files at its top", e.path));
        }
    }
    for f in &m.faces {
        if !entries.iter().any(|e| e.path == f.file) {
            return Err(format!("{}: not in the package", f.file));
        }
    }
    Ok(())
}

/// Static TrueType (`glyf`): the sfnt version 0x00010000 or `true`. CFF
/// (`OTTO`) and collections are refused (the PDF writer subsets `glyf` only).
fn truetype(p: &Path) -> Result<(), String> {
    let mut b = [0u8; 4];
    fs::File::open(p).and_then(|mut f| f.read_exact(&mut b)).map_err(|e| format!("{}: {e}", p.display()))?;
    match &b {
        [0, 1, 0, 0] | b"true" => Ok(()),
        _ => Err(format!("{}: not a static TrueType font", p.display())),
    }
}

fn blake3_file(p: &Path) -> Result<(String, u64), String> {
    let mut f = crate::content::open_nofollow(p)?;
    let mut h = blake3::Hasher::new();
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
    Ok((format!("blake3:{}", h.finalize().to_hex()), n))
}

/// Install an unpacked, signed font package: checked against `keys` (the
/// release's signer), copied file by file against the signed index, swapped
/// in, and its faces written into the pin list.
pub fn install(src: &Path, fonts: &Path, keys: &[String]) -> Result<(FontManifest, PathBuf), String> {
    let text = fs::read_to_string(src.join(MANIFEST)).map_err(|e| format!("{MANIFEST}: {e}"))?;
    let m = FontManifest::from_str(&text)?;
    let id = m.font.id.clone();
    let want = m.require_content()?;
    let raw = fs::read(src.join(SIG)).map_err(|_| format!("{id} is not signed (no {SIG})"))?;
    let sig = std::str::from_utf8(&raw).ok().and_then(|s| portcullis_sig::sig_from_base64(s).ok())
        .ok_or_else(|| format!("{SIG}: not base64"))?;
    portcullis_sig::verify_trusted(text.as_bytes(), &sig, keys)
        .map_err(|e| format!("{id} is not signed by its publisher ({e:?})"))?;
    let entries = read_index(src, want)?;
    flat(&m, &entries)?;

    fs::create_dir_all(fonts).map_err(|e| format!("{}: {e}", fonts.display()))?;
    let dest = fonts.join(&id);
    let stage = fonts.join(format!(".staging-{id}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&stage);
    fs::create_dir(&stage).map_err(|e| format!("{}: {e}", stage.display()))?;
    let lines = (|| {
        place(src, &stage, &entries)?;
        for f in TOP {
            if src.join(f).is_file() {
                fs::copy(src.join(f), stage.join(f)).map_err(|e| format!("{f}: {e}"))?;
                let _ = fs::set_permissions(stage.join(f), fs::Permissions::from_mode(0o444));
            }
        }
        let mut lines = Vec::new();
        for f in &m.faces {
            let p = stage.join(&f.file);
            truetype(&p)?;
            let _ = fs::set_permissions(&p, fs::Permissions::from_mode(0o444));
            let (address, size) = blake3_file(&p)?;
            lines.push(format!("{id}\t{}\t{id}/{}\t{}\t{}\t{}\t{address}\t{size}", f.family, f.file, f.weight, f.italic as u8, f.ranges));
        }
        own(&stage)?;
        fs::set_permissions(&stage, fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
        Ok::<_, String>(lines)
    })();
    let lines = match lines {
        Ok(l) => l,
        Err(e) => {
            let _ = fs::remove_dir_all(&stage);
            return Err(e);
        }
    };
    // Swap the folder, then the list: a face is drawn only once both agree.
    let old = fonts.join(format!(".old-{id}-{}", std::process::id()));
    let had = fs::symlink_metadata(&dest).is_ok();
    if had {
        fs::rename(&dest, &old).map_err(|e| format!("{}: {e}", dest.display()))?;
    }
    if let Err(e) = fs::rename(&stage, &dest) {
        if had {
            let _ = fs::rename(&old, &dest);
        }
        let _ = fs::remove_dir_all(&stage);
        return Err(format!("{}: {e}", dest.display()));
    }
    write_pins(fonts, &id, &lines)?;
    if had {
        let _ = fs::remove_dir_all(&old);
    }
    Ok((m, dest))
}

/// Uninstall: the lines first, then the folder.
pub fn remove(fonts: &Path, id: &str) -> Result<(), String> {
    if !opifex_catalog::font::id_ok(id) {
        return Err(format!("{id:?}: not a font package id"));
    }
    write_pins(fonts, id, &[])?;
    let d = fonts.join(id);
    if fs::symlink_metadata(&d).is_ok() {
        fs::remove_dir_all(&d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    Ok(())
}

/// The pin list with `id`'s lines replaced by `lines` (where its first line
/// was, so install order and with it Han fallback order stay; else at the
/// end). Lines of other packages are kept as they are; the result must read
/// back whole in atrium-fonts, or nothing is written.
fn write_pins(fonts: &Path, id: &str, lines: &[String]) -> Result<(), String> {
    let path = fonts.join(PINS);
    let old = fs::read_to_string(&path).unwrap_or_default();
    let mut keep: Vec<String> = Vec::new();
    let mut placed = false;
    for l in old.lines().skip_while(|l| *l != HEADER).skip(1) {
        if l.split('\t').next() == Some(id) {
            if !placed {
                keep.extend(lines.iter().cloned());
                placed = true;
            }
        } else if !l.trim().is_empty() {
            keep.push(l.to_string());
        }
    }
    if !placed {
        keep.extend(lines.iter().cloned());
    }
    let text = format!("{HEADER}\n{}", keep.iter().map(|l| format!("{l}\n")).collect::<String>());
    let read_back = atrium_fonts::extra::parse(&text).len();
    let mine = lines.len();
    if keep.len() > atrium_fonts::extra::MAX_FACES {
        return Err(format!("{}: more than {} faces", path.display(), atrium_fonts::extra::MAX_FACES));
    }
    // Lines of others that were already broken stay broken; ours must all read.
    let others_ok = atrium_fonts::extra::parse(&format!("{HEADER}\n{}",
        keep.iter().filter(|l| l.split('\t').next() != Some(id)).map(|l| format!("{l}\n")).collect::<String>())).len();
    if read_back != others_ok + mine {
        return Err(format!("{id}: a face line atrium-fonts would not read"));
    }
    let tmp = fonts.join(format!(".pins-{}", std::process::id()));
    fs::write(&tmp, &text).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(0o644));
    own(&tmp)?;
    fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("opifex-font-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    pub fn template(id: &str, v: &str) -> String {
        format!("kind = \"font\"\n[font]\nid = \"{id}\"\nname = \"Test JP\"\nversion = \"{v}\"\nlicense = \"OFL-1.1\"\n\
                 languages = [\"ja\"]\n\n[[face]]\nfamily = \"Test Sans JP\"\nfile = \"TestJP-Regular.ttf\"\nweight = 400\n\
                 ranges = \"3040-30FF,4E00-9FFF\"\n\n[[face]]\nfamily = \"Test Sans JP\"\nfile = \"TestJP-Bold.ttf\"\nweight = 700\n\
                 ranges = \"3040-30FF,4E00-9FFF\"\n")
    }

    pub fn tree(at: &Path, tag: u8) -> PathBuf {
        let _ = fs::remove_dir_all(at);
        fs::create_dir_all(at).unwrap();
        for f in ["TestJP-Regular.ttf", "TestJP-Bold.ttf"] {
            fs::write(at.join(f), [&[0u8, 1, 0, 0][..], &[tag; 60]].concat()).unwrap();
        }
        fs::write(at.join("OFL.txt"), "SIL Open Font License 1.1\n").unwrap();
        at.to_path_buf()
    }

    #[test]
    fn pack_install_update_remove() {
        let d = tmp("cycle");
        let (sk, pk) = portcullis_sig::new_key([9; 32]).unwrap();
        let fonts = d.join("extra");
        // Another package already there, which must keep its line and place.
        fs::create_dir_all(&fonts).unwrap();
        let other = format!("org.atrium.font.other\tOther\torg.atrium.font.other/O.ttf\t400\t0\t0E00-0E7F\tblake3:{}\t9\n", "0".repeat(64));
        fs::write(fonts.join(PINS), format!("{HEADER}\n{other}")).unwrap();

        let t = tree(&d.join("t1"), 1);
        pack(&t, &template("org.atrium.font.test-jp", "1.0"), Some(&sk)).unwrap();
        install(&t, &fonts, &[pk.clone()]).unwrap();
        let pins = fs::read_to_string(fonts.join(PINS)).unwrap();
        let faces = atrium_fonts::extra::parse(&pins);
        assert_eq!(faces.len(), 3);
        assert_eq!(faces[1].0.file, "org.atrium.font.test-jp/TestJP-Regular.ttf");
        let body = fs::read(fonts.join("org.atrium.font.test-jp/TestJP-Regular.ttf")).unwrap();
        assert_eq!(faces[1].0.address, format!("blake3:{}", blake3::hash(&body).to_hex()));
        assert_eq!((faces[2].0.weight, faces[1].0.size), (700, 64));

        // An update replaces its lines in place.
        let t2 = tree(&d.join("t2"), 2);
        pack(&t2, &template("org.atrium.font.test-jp", "1.1"), Some(&sk)).unwrap();
        install(&t2, &fonts, &[pk.clone()]).unwrap();
        let pins2 = fs::read_to_string(fonts.join(PINS)).unwrap();
        assert_eq!(pins2.lines().count(), 4);
        assert!(pins2.lines().nth(1).unwrap().starts_with("org.atrium.font.other"));
        assert_ne!(pins, pins2);
        assert!(fs::read_dir(&fonts).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().starts_with('.')));

        remove(&fonts, "org.atrium.font.test-jp").unwrap();
        assert_eq!(fs::read_to_string(fonts.join(PINS)).unwrap(), format!("{HEADER}\n{other}"));
        assert!(!fonts.join("org.atrium.font.test-jp").exists());
    }

    #[test]
    fn refusals() {
        let d = tmp("refuse");
        let (sk, pk) = portcullis_sig::new_key([9; 32]).unwrap();
        let (_, other_pk) = portcullis_sig::new_key([8; 32]).unwrap();
        let fonts = d.join("extra");
        let id = "org.atrium.font.test-jp";
        // CFF, a subfolder, a missing face.
        let t = tree(&d.join("cff"), 1);
        fs::write(t.join("TestJP-Bold.ttf"), b"OTTO and the rest").unwrap();
        assert!(pack(&t, &template(id, "1.0"), Some(&sk)).unwrap_err().contains("not a static TrueType"));
        let t = tree(&d.join("sub"), 1);
        fs::create_dir(t.join("more")).unwrap();
        assert!(pack(&t, &template(id, "1.0"), Some(&sk)).unwrap_err().contains("only files at its top"));
        let t = tree(&d.join("miss"), 1);
        fs::remove_file(t.join("TestJP-Bold.ttf")).unwrap();
        assert!(pack(&t, &template(id, "1.0"), Some(&sk)).unwrap_err().contains("not in the package"));
        // Another key; a file changed after signing.
        let t = tree(&d.join("ok"), 1);
        pack(&t, &template(id, "1.0"), Some(&sk)).unwrap();
        assert!(install(&t, &fonts, &[other_pk]).unwrap_err().contains("not signed by its publisher"));
        fs::write(t.join("TestJP-Bold.ttf"), [&[0u8, 1, 0, 0][..], &[7; 60]].concat()).unwrap();
        assert!(install(&t, &fonts, &[pk]).unwrap_err().contains("not the file the index lists"));
        assert!(!fonts.join(id).exists() && !fonts.join(PINS).exists());
    }
}
