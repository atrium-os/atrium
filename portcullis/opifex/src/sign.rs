//! `opifex sign`: sign packed manifests away from where they were built
//! (opifex.md §10.4). A signature covers only `atrium.toml`, which names its
//! `content.index` by hash, which names every file by hash; so a release
//! built on a remote or cloud machine is signed by copying just its
//! manifests (and indexes) to the machine that holds the platform key,
//! often a hardware token (`opifex token ref`), and copying the
//! `atrium.toml.sig` files back.
//!
//! ```text
//! opifex sign --list <dir>…                       one line per package: id version content
//! opifex sign <dir>… --key <key> [--expect <file>]
//! ```
//!
//! Each `<dir>` holds a packed package's `atrium.toml` and, if it was copied
//! too, its `content.index`, which must then hash to what the manifest
//! names. `--expect` takes `--list` lines from a second, independent build
//! (a reproducible build): a package is signed only if its content is the
//! one that build got, and nothing is signed if any differs.

use std::path::Path;

use crate::content::{digest_text, CONTENT_INDEX, MANIFEST, SIG};

const KINDS: [(&str, &str); 6] = [("app", "bundle"), ("font", "font"), ("driver", "driver"),
                                  ("firmware", "firmware"), ("layer", "layer"), ("service", "bundle")];

/// What a manifest says it is: (id, version, content).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub id: String,
    pub version: String,
    pub content: String,
}

impl Package {
    pub fn line(&self) -> String { format!("{} {} {}", self.id, self.version, self.content) }
}

fn plain(content: &str) -> bool {
    content.len() == 71 && content.starts_with("sha256:") && content[7..].bytes().all(|b| b.is_ascii_hexdigit())
}

/// Read and check one packed package folder.
pub fn read(dir: &Path) -> Result<Package, String> {
    let what = dir.display().to_string();
    let text = std::fs::read_to_string(dir.join(MANIFEST)).map_err(|e| format!("{what}/{MANIFEST}: {e}"))?;
    let v: toml::Value = text.parse().map_err(|e| format!("{what}/{MANIFEST}: {e}"))?;
    let s = |t: &str, k: &str| v.get(t).and_then(|x| x.get(k)).and_then(|x| x.as_str()).map(str::to_string);
    let (id, version, content) = KINDS.iter()
        .find_map(|(head, holder)| {
            let id = s(head, "id").or_else(|| s(head, "name"))?;
            Some((id, s(head, "version").unwrap_or_default(), s(holder, "content").unwrap_or_default()))
        })
        .ok_or_else(|| format!("{what}: not a package manifest"))?;
    if !plain(&content) {
        return Err(format!("{what}: {id} is not packed (no content index named): run opifex pack where it was built"));
    }
    match std::fs::read_to_string(dir.join(CONTENT_INDEX)) {
        Ok(idx) if digest_text(&idx) != content => return Err(format!("{what}: {CONTENT_INDEX} is not the one {MANIFEST} names")),
        _ => {}
    }
    Ok(Package { id, version, content })
}

/// `--expect` lines: id, version and content of each package a second build made.
pub fn expected(text: &str) -> Vec<Package> {
    text.lines().filter_map(|l| {
        let mut it = l.split_whitespace();
        let (id, version, content) = (it.next()?, it.next()?, it.next()?);
        Some(Package { id: id.into(), version: version.into(), content: content.into() })
    }).collect()
}

/// Sign every folder's manifest with `key` (a PEM or a token key file), or
/// none: all are read and checked first.
pub fn sign(dirs: &[&Path], key: &str, expect: Option<&[Package]>) -> Result<Vec<Package>, String> {
    let pkgs: Vec<Package> = dirs.iter().map(|d| read(d)).collect::<Result<_, _>>()?;
    if let Some(want) = expect {
        let bad: Vec<String> = pkgs.iter().filter(|p| !want.contains(p)).map(|p| match want.iter().find(|w| w.id == p.id && w.version == p.version) {
            Some(w) => format!("{} {}: built as {}, the other build got {}", p.id, p.version, p.content, w.content),
            None => format!("{} {}: not in the other build's list", p.id, p.version),
        }).collect();
        if !bad.is_empty() {
            return Err(format!("nothing signed; the builds differ:\n  {}", bad.join("\n  ")));
        }
    }
    let public = portcullis_sig::public_pem(key).map_err(|e| format!("key: {e:?}"))?;
    for d in dirs {
        let text = std::fs::read(d.join(MANIFEST)).map_err(|e| e.to_string())?;
        let sig = portcullis_sig::sign(&text, key).map_err(|e| format!("{}: sign: {e:?}", d.display()))?;
        portcullis_sig::verify_one(&text, &sig, &public).map_err(|e| format!("{}: the signature doesn't verify: {e:?}", d.display()))?;
        use base64::Engine;
        std::fs::write(d.join(SIG), base64::engine::general_purpose::STANDARD.encode(&sig) + "\n")
            .map_err(|e| format!("{}/{SIG}: {e}", d.display()))?;
    }
    Ok(pkgs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packed(at: &Path, id: &str, body: &[u8]) -> std::path::PathBuf {
        let d = at.join(id);
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("bin")).unwrap();
        std::fs::write(d.join("bin/hello"), body).unwrap();
        std::fs::write(d.join(MANIFEST), format!("[app]\nid = \"{id}\"\nname = \"H\"\nversion = \"1.0\"\nentry = \"bin/hello\"\n")).unwrap();
        crate::app::pack(&d, None).unwrap();
        d
    }

    #[test]
    fn signs_manifests_built_elsewhere() {
        let t = std::env::temp_dir().join(format!("opifex-sign-{}", std::process::id()));
        let (sk, pk) = portcullis_sig::new_key([8; 32]).unwrap();
        // Built "remotely"; only the manifest and index come over.
        let a = packed(&t.join("build"), "org.atrium.a", b"one");
        let b = packed(&t.join("build"), "org.atrium.b", b"two");
        let here = t.join("here");
        for d in [&a, &b] {
            let to = here.join(d.file_name().unwrap());
            std::fs::create_dir_all(&to).unwrap();
            for f in [MANIFEST, CONTENT_INDEX] {
                std::fs::copy(d.join(f), to.join(f)).unwrap();
            }
        }
        let (ha, hb) = (here.join("org.atrium.a"), here.join("org.atrium.b"));
        // A second build that got the same bytes: signed, and the signature
        // carried back makes the remote tree install-ready.
        let list: String = [&a, &b].iter().map(|d| read(d).unwrap().line() + "\n").collect();
        let got = sign(&[&ha, &hb], &sk, Some(&expected(&list))).unwrap();
        assert_eq!(got.len(), 2);
        std::fs::copy(ha.join(SIG), a.join(SIG)).unwrap();
        let text = std::fs::read(a.join(MANIFEST)).unwrap();
        let sig = portcullis_sig::sig_from_base64(std::fs::read_to_string(a.join(SIG)).unwrap().trim()).unwrap();
        portcullis_sig::verify_one(&text, &sig, &pk).unwrap();
        // A second build that got other bytes for b: nothing signed.
        std::fs::remove_file(ha.join(SIG)).unwrap();
        let other = packed(&t.join("other"), "org.atrium.b", b"TWO");
        let list2 = format!("{}\n{}\n", read(&a).unwrap().line(), read(&other).unwrap().line());
        let e = sign(&[&ha, &hb], &sk, Some(&expected(&list2))).unwrap_err();
        assert!(e.contains("org.atrium.b 1.0: built as") && e.contains("nothing signed"), "{e}");
        assert!(!ha.join(SIG).exists(), "not even the one that matched");
        // An index that isn't the manifest's, and an unpacked manifest: refused.
        std::fs::write(hb.join(CONTENT_INDEX), "tampered\n").unwrap();
        assert!(sign(&[&hb], &sk, None).unwrap_err().contains("is not the one"));
        std::fs::write(hb.join(MANIFEST), "[app]\nid = \"org.atrium.b\"\nversion = \"1.0\"\nentry = \"bin/hello\"\n").unwrap();
        assert!(read(&hb).unwrap_err().contains("not packed"));
        let _ = std::fs::remove_dir_all(&t);
    }
}
