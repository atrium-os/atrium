//! The SDK crate registry (dev-tools.md §5.4, opifex.md §8.4): a static
//! cargo sparse registry, built from the path crates an SDK project
//! depends on, with the index signed by the platform key.
//!
//! Layout under the output folder (served as `<url>/`):
//!
//! ```text
//! index/config.json             {"dl": "<url>/crates/{crate}/{crate}-{version}.crate"}
//! index/<cargo's prefix>/<name> one JSON line per version
//! index/index.files             "<sha256>  <path>" for config.json and every index file
//! index/index.sig               base64 DER ECDSA P-256 over index.files (platform key)
//! crates/<name>/<name>-<version>.crate
//! ```
//!
//! Cargo checks each `.crate` against the index's `cksum` but nothing about
//! the index, so `cargo atrium registry verify` checks `index.sig` and every
//! listed file before cargo is let near the registry. The copy inside the
//! Rust layer is covered by the layer's own signature.
//!
//! Crates are packed the way `cargo package` would, but from `cargo
//! metadata` rather than from each crate's own manifest, so workspace
//! inheritance and path dependencies are resolved already: path
//! dependencies on other SDK crates become `=<version>` requirements on
//! this registry, crates.io ones stay crates.io, dev-dependencies are
//! dropped (nobody building against the SDK runs its tests).

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const CRATES_IO: &str = "https://github.com/rust-lang/crates.io-index";
const CRATES_IO_SOURCE: &str = "registry+https://github.com/rust-lang/crates.io-index";
pub const FILES: &str = "index.files";
pub const SIG: &str = "index.sig";
/// The mtime cargo itself gives every entry of a `.crate`.
const MTIME: u64 = 1_153_704_088;

/// Where cargo looks for `name` under the index root.
pub fn index_path(name: &str) -> String {
    let n = name.to_lowercase();
    match n.len() {
        0 => String::new(),
        1 => format!("1/{n}"),
        2 => format!("2/{n}"),
        3 => format!("3/{}/{n}", &n[..1]),
        _ => format!("{}/{}/{n}", &n[..2], &n[2..4]),
    }
}

pub fn sha256_hex(b: &[u8]) -> String {
    Sha256::digest(b).iter().map(|x| format!("{x:02x}")).collect()
}

/// A crate the registry will hold.
pub struct Pkg<'a> {
    pub meta: &'a Value,
    pub name: String,
    pub version: String,
    pub dir: PathBuf,
}

/// Every path package in `metadata` (the SDK crates a project reaches)
/// except those named in `skip` (the seed project itself).
pub fn sdk_packages<'a>(metadata: &'a Value, skip: &[String]) -> Result<Vec<Pkg<'a>>, String> {
    let mut v = Vec::new();
    for p in metadata["packages"].as_array().ok_or("cargo metadata: no packages")? {
        if !p["source"].is_null() {
            let s = p["source"].as_str().unwrap_or("");
            if s != CRATES_IO_SOURCE {
                return Err(format!("{} comes from {s}; the SDK takes crates.io and its own crates only", p["name"]));
            }
            continue;
        }
        let name = p["name"].as_str().unwrap_or_default().to_string();
        if skip.contains(&name) {
            continue;
        }
        let mp = PathBuf::from(p["manifest_path"].as_str().unwrap_or_default());
        let dir = mp.parent().map(Path::to_path_buf).unwrap_or_default();
        v.push(Pkg { meta: p, name, version: p["version"].as_str().unwrap_or_default().to_string(), dir });
    }
    v.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(v)
}

/// The dependencies a consumer needs (no dev-dependencies, no path
/// dependencies outside the SDK).
fn deps<'a>(p: &'a Pkg, sdk: &BTreeMap<String, String>) -> Vec<&'a Value> {
    p.meta["dependencies"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|d| d["kind"].as_str() != Some("dev"))
                .filter(|d| d["path"].is_null() || sdk.contains_key(d["name"].as_str().unwrap_or("")))
                .collect()
        })
        .unwrap_or_default()
}

fn req(d: &Value, sdk: &BTreeMap<String, String>) -> String {
    match sdk.get(d["name"].as_str().unwrap_or("")) {
        Some(v) if !d["path"].is_null() => format!("={v}"),
        _ => d["req"].as_str().unwrap_or("*").to_string(),
    }
}

fn rel(p: &Pkg, path: &str) -> String {
    Path::new(path).strip_prefix(&p.dir).map(|r| r.to_string_lossy().into_owned()).unwrap_or_else(|_| path.to_string())
}

fn is_v2(f: &str) -> bool { f.starts_with("dep:") || f.contains("?/") }

/// The normalized Cargo.toml that goes in the `.crate`.
pub fn manifest(p: &Pkg, sdk: &BTreeMap<String, String>, index_url: &str) -> Result<String, String> {
    use toml::{Table, Value as T};
    let m = p.meta;
    let s = |k: &str| m[k].as_str().map(|x| T::String(x.into()));
    let mut pkg = Table::new();
    pkg.insert("name".into(), T::String(p.name.clone()));
    pkg.insert("version".into(), T::String(p.version.clone()));
    for k in ["edition", "license", "description", "rust_version", "links", "repository"] {
        if let Some(v) = s(k) {
            pkg.insert(k.replace('_', "-"), v);
        }
    }
    if m["license"].is_null() {
        return Err(format!("{}: no license", p.name));
    }
    for k in ["autobins", "autoexamples", "autotests", "autobenches"] {
        pkg.insert(k.into(), T::Boolean(false));
    }
    let mut doc = Table::new();
    let mut bins = Vec::new();
    for t in m["targets"].as_array().into_iter().flatten() {
        let kinds: Vec<&str> = t["kind"].as_array().into_iter().flatten().filter_map(|k| k.as_str()).collect();
        let path = rel(p, t["src_path"].as_str().unwrap_or_default());
        let name = t["name"].as_str().unwrap_or_default();
        if kinds.contains(&"custom-build") {
            pkg.insert("build".into(), T::String(path));
        } else if kinds.contains(&"bin") {
            let mut b = Table::new();
            b.insert("name".into(), T::String(name.into()));
            b.insert("path".into(), T::String(path));
            if let Some(rf) = t["required-features"].as_array() {
                b.insert("required-features".into(), T::Array(rf.iter().filter_map(|x| x.as_str()).map(|x| T::String(x.into())).collect()));
            }
            bins.push(T::Table(b));
        } else if kinds.iter().any(|k| matches!(*k, "lib" | "rlib" | "dylib" | "cdylib" | "staticlib" | "proc-macro")) {
            let mut l = Table::new();
            l.insert("name".into(), T::String(name.replace('-', "_")));
            l.insert("path".into(), T::String(path));
            if kinds.contains(&"proc-macro") {
                l.insert("proc-macro".into(), T::Boolean(true));
            } else if kinds != ["lib"] {
                l.insert("crate-type".into(), T::Array(kinds.iter().map(|k| T::String((*k).into())).collect()));
            }
            doc.insert("lib".into(), T::Table(l));
        }
    }
    doc.insert("package".into(), T::Table(pkg));
    if !bins.is_empty() {
        doc.insert("bin".into(), T::Array(bins));
    }
    let mut targets: BTreeMap<String, Table> = BTreeMap::new();
    for d in deps(p, sdk) {
        let real = d["name"].as_str().unwrap_or_default();
        let key = d["rename"].as_str().unwrap_or(real);
        let mut e = Table::new();
        e.insert("version".into(), T::String(req(d, sdk)));
        if d["rename"].is_string() {
            e.insert("package".into(), T::String(real.into()));
        }
        if d["optional"].as_bool() == Some(true) {
            e.insert("optional".into(), T::Boolean(true));
        }
        if d["uses_default_features"].as_bool() == Some(false) {
            e.insert("default-features".into(), T::Boolean(false));
        }
        if let Some(f) = d["features"].as_array().filter(|f| !f.is_empty()) {
            e.insert("features".into(), T::Array(f.iter().filter_map(|x| x.as_str()).map(|x| T::String(x.into())).collect()));
        }
        if !d["path"].is_null() {
            e.insert("registry-index".into(), T::String(index_url.into()));
        }
        let section = if d["kind"].as_str() == Some("build") { "build-dependencies" } else { "dependencies" };
        let tkey = match d["target"].as_str() {
            Some(t) => format!("target\u{0}{t}\u{0}{section}"),
            None => section.to_string(),
        };
        targets.entry(tkey).or_default().insert(key.into(), T::Table(e));
    }
    let mut tgt = Table::new();
    for (k, v) in targets {
        match k.split('\u{0}').collect::<Vec<_>>()[..] {
            ["target", t, section] => {
                let entry = tgt.entry(t.to_string()).or_insert_with(|| T::Table(Table::new()));
                if let T::Table(tt) = entry {
                    tt.insert(section.into(), T::Table(v));
                }
            }
            _ => {
                doc.insert(k, T::Table(v));
            }
        }
    }
    if !tgt.is_empty() {
        doc.insert("target".into(), T::Table(tgt));
    }
    if let Some(f) = m["features"].as_object().filter(|f| !f.is_empty()) {
        let mut ft = Table::new();
        for (k, v) in f {
            ft.insert(k.clone(), T::Array(v.as_array().into_iter().flatten().filter_map(|x| x.as_str()).map(|x| T::String(x.into())).collect()));
        }
        doc.insert("features".into(), T::Table(ft));
    }
    let body = toml::to_string(&doc).map_err(|e| e.to_string())?;
    Ok(format!("# Normalized by cargo atrium registry from the Atrium tree.\n{body}"))
}

/// The index line for one version.
pub fn index_line(p: &Pkg, sdk: &BTreeMap<String, String>, cksum: &str) -> Value {
    let ds: Vec<Value> = deps(p, sdk)
        .into_iter()
        .map(|d| {
            let real = d["name"].as_str().unwrap_or_default();
            let mut e = json!({
                "name": d["rename"].as_str().unwrap_or(real),
                "req": req(d, sdk),
                "features": d["features"].clone(),
                "optional": d["optional"].as_bool().unwrap_or(false),
                "default_features": d["uses_default_features"].as_bool().unwrap_or(true),
                "target": d["target"].clone(),
                "kind": d["kind"].as_str().unwrap_or("normal"),
                "registry": if d["path"].is_null() { json!(CRATES_IO) } else { Value::Null },
            });
            if d["rename"].is_string() {
                e["package"] = json!(real);
            }
            e
        })
        .collect();
    let (mut f1, mut f2) = (serde_json::Map::new(), serde_json::Map::new());
    for (k, v) in p.meta["features"].as_object().into_iter().flatten() {
        let v2 = v.as_array().into_iter().flatten().any(|x| x.as_str().map(is_v2).unwrap_or(false));
        if v2 { &mut f2 } else { &mut f1 }.insert(k.clone(), v.clone());
    }
    let mut l = json!({
        "name": p.name, "vers": p.version, "deps": ds, "cksum": cksum,
        "features": f1, "yanked": false, "links": p.meta["links"].clone(),
    });
    if !f2.is_empty() {
        l["features2"] = Value::Object(f2);
        l["v"] = json!(2);
    }
    if let Some(r) = p.meta["rust_version"].as_str() {
        l["rust_version"] = json!(r);
    }
    l
}

/// The files of `p` as git sees them (tracked and new but not ignored),
/// without nested packages, `target/` or the manifest (rewritten).
pub fn files(p: &Pkg) -> Result<Vec<String>, String> {
    let o = std::process::Command::new("git")
        .args(["ls-files", "-z", "--cached", "--others", "--exclude-standard", "--", "."])
        .current_dir(&p.dir)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !o.status.success() {
        return Err(format!("{}: git ls-files failed: {}", p.dir.display(), String::from_utf8_lossy(&o.stderr)));
    }
    let all: Vec<String> = o.stdout.split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect();
    let nested: Vec<String> = all
        .iter()
        .filter(|f| f.ends_with("/Cargo.toml"))
        .map(|f| f.trim_end_matches("Cargo.toml").to_string())
        .collect();
    let mut v: Vec<String> = all
        .into_iter()
        .filter(|f| f != "Cargo.toml" && f != "Cargo.lock" && !f.starts_with("target/"))
        .filter(|f| !nested.iter().any(|n| f.starts_with(n.as_str())))
        .filter(|f| p.dir.join(f).is_file())
        .collect();
    v.sort();
    v.dedup();
    Ok(v)
}

/// The `.crate`: a gzipped tar of `<name>-<version>/`, deterministic.
pub fn pack(p: &Pkg, files: &[String], manifest: &str) -> Result<Vec<u8>, String> {
    let gz = flate2::GzBuilder::new().mtime(0).write(Vec::new(), flate2::Compression::best());
    let mut t = tar::Builder::new(gz);
    t.mode(tar::HeaderMode::Deterministic);
    let root = format!("{}-{}", p.name, p.version);
    let orig = std::fs::read(p.dir.join("Cargo.toml")).map_err(|e| format!("{}: {e}", p.dir.display()))?;
    let mut entries: Vec<(String, Vec<u8>, u32)> = vec![
        ("Cargo.toml".into(), manifest.as_bytes().to_vec(), 0o644),
        ("Cargo.toml.orig".into(), orig, 0o644),
    ];
    for f in files {
        let path = p.dir.join(f);
        let data = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            if std::fs::metadata(&path).map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false) { 0o755 } else { 0o644 }
        };
        #[cfg(not(unix))]
        let mode = 0o644;
        entries.push((f.clone(), data, mode));
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, data, mode) in entries {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(mode);
        h.set_mtime(MTIME);
        h.set_entry_type(tar::EntryType::Regular);
        t.append_data(&mut h, format!("{root}/{name}"), &data[..]).map_err(|e| e.to_string())?;
    }
    let gz = t.into_inner().map_err(|e| e.to_string())?;
    gz.finish().map_err(|e| e.to_string())
}

/// The signed listing: one `<sha256>  <path>` line per index file.
pub fn listing(index_dir: &Path) -> Result<String, String> {
    let mut v = Vec::new();
    walk(index_dir, index_dir, &mut v)?;
    v.retain(|(r, _)| r != FILES && r != SIG);
    v.sort();
    Ok(v.into_iter().map(|(r, h)| format!("{h}  {r}\n")).collect())
}

fn walk(root: &Path, d: &Path, out: &mut Vec<(String, String)>) -> Result<(), String> {
    for e in std::fs::read_dir(d).map_err(|e| format!("{}: {e}", d.display()))? {
        let e = e.map_err(|e| e.to_string())?;
        let p = e.path();
        let ft = e.file_type().map_err(|e| e.to_string())?;
        if ft.is_dir() {
            walk(root, &p, out)?;
        } else if ft.is_file() {
            let b = std::fs::read(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            out.push((p.strip_prefix(root).unwrap_or(&p).to_string_lossy().into_owned(), sha256_hex(&b)));
        } else {
            return Err(format!("{}: not a file or folder", p.display()));
        }
    }
    Ok(())
}

pub struct BuildOpts<'a> {
    pub out: &'a Path,
    /// Where `out` is served: `https://host/sdk` (no trailing slash).
    pub url: &'a str,
    pub sign_key_pem: Option<&'a str>,
}

/// Write the registry for `pkgs`. Returns (crate, version, bytes) per crate.
pub fn build(pkgs: &[Pkg], o: &BuildOpts) -> Result<Vec<(String, String, usize)>, String> {
    let url = o.url.trim_end_matches('/');
    let index_url = format!("sparse+{url}/index/");
    let sdk: BTreeMap<String, String> = pkgs.iter().map(|p| (p.name.clone(), p.version.clone())).collect();
    let idx = o.out.join("index");
    std::fs::create_dir_all(&idx).map_err(|e| e.to_string())?;
    let config = json!({"dl": format!("{url}/crates/{{crate}}/{{crate}}-{{version}}.crate")});
    std::fs::write(idx.join("config.json"), format!("{}\n", serde_json::to_string_pretty(&config).unwrap_or_default())).map_err(|e| e.to_string())?;
    let mut lines: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut made = Vec::new();
    for p in pkgs {
        let m = manifest(p, &sdk, &index_url)?;
        let fs = files(p)?;
        let bytes = pack(p, &fs, &m)?;
        let dir = o.out.join("crates").join(&p.name);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        std::fs::write(dir.join(format!("{}-{}.crate", p.name, p.version)), &bytes).map_err(|e| e.to_string())?;
        let line = serde_json::to_string(&index_line(p, &sdk, &sha256_hex(&bytes))).map_err(|e| e.to_string())?;
        lines.entry(index_path(&p.name)).or_default().insert(line);
        made.push((p.name.clone(), p.version.clone(), bytes.len()));
    }
    for (path, ls) in lines {
        let f = idx.join(&path);
        if let Some(d) = f.parent() {
            std::fs::create_dir_all(d).map_err(|e| e.to_string())?;
        }
        // Earlier versions already in the index stay; this version's line
        // replaces any line for the same version.
        let mut keep: Vec<String> = std::fs::read_to_string(&f)
            .unwrap_or_default()
            .lines()
            .filter(|l| {
                let v: Value = serde_json::from_str(l).unwrap_or(Value::Null);
                !ls.iter().any(|n| serde_json::from_str::<Value>(n).map(|n| n["vers"] == v["vers"]).unwrap_or(false))
            })
            .map(str::to_string)
            .collect();
        keep.extend(ls);
        let mut w = std::fs::File::create(&f).map_err(|e| e.to_string())?;
        for l in keep {
            writeln!(w, "{l}").map_err(|e| e.to_string())?;
        }
    }
    let list = listing(&idx)?;
    std::fs::write(idx.join(FILES), &list).map_err(|e| e.to_string())?;
    match o.sign_key_pem {
        Some(k) => {
            let sig = portcullis_sig::sign(list.as_bytes(), k).map_err(|e| format!("signing: {e:?}"))?;
            std::fs::write(idx.join(SIG), format!("{}\n", base64::engine::general_purpose::STANDARD.encode(sig))).map_err(|e| e.to_string())?;
        }
        None => {
            let _ = std::fs::remove_file(idx.join(SIG));
        }
    }
    Ok(made)
}

/// Check a registry index: `fetch(path)` returns the bytes of a file
/// under `index/`. The listing must be signed by one of `keys` and every
/// listed file must match. Returns how many files were checked.
pub fn verify(fetch: &dyn Fn(&str) -> Result<Vec<u8>, String>, keys: &[String]) -> Result<usize, String> {
    let list = fetch(FILES)?;
    let sig = fetch(SIG).map_err(|e| format!("no {SIG}: the index isn't signed ({e})"))?;
    let der = base64::engine::general_purpose::STANDARD
        .decode(String::from_utf8_lossy(&sig).trim())
        .map_err(|_| format!("{SIG}: not base64"))?;
    portcullis_sig::verify_trusted(&list, &der, keys).map_err(|e| format!("{SIG}: not signed by a platform key ({e:?})"))?;
    let text = String::from_utf8(list).map_err(|_| format!("{FILES}: not UTF-8"))?;
    let mut n = 0;
    for l in text.lines() {
        let (h, path) = l.split_once("  ").ok_or_else(|| format!("{FILES}: bad line {l:?}"))?;
        if path.starts_with('/') || path.split('/').any(|c| c == ".." || c.is_empty()) {
            return Err(format!("{FILES}: bad path {path:?}"));
        }
        let b = fetch(path)?;
        if sha256_hex(&b) != h {
            return Err(format!("{path}: doesn't match the signed listing"));
        }
        n += 1;
    }
    if !text.lines().any(|l| l.ends_with("  config.json")) {
        return Err(format!("{FILES}: config.json isn't covered"));
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes() {
        assert_eq!(index_path("a"), "1/a");
        assert_eq!(index_path("ab"), "2/ab");
        assert_eq!(index_path("abc"), "3/a/abc");
        assert_eq!(index_path("Pergola"), "pe/rg/pergola");
    }

    #[test]
    fn sign_and_verify() {
        let d = std::env::temp_dir().join(format!("atrium-registry-{}", std::process::id()));
        let idx = d.join("index");
        std::fs::create_dir_all(idx.join("pe/rg")).unwrap();
        std::fs::write(idx.join("config.json"), "{}").unwrap();
        std::fs::write(idx.join("pe/rg/pergola"), "{\"name\":\"pergola\"}\n").unwrap();
        let (k, pubk) = portcullis_sig::new_key([7; 32]).unwrap();
        let list = listing(&idx).unwrap();
        assert_eq!(list.lines().count(), 2);
        std::fs::write(idx.join(FILES), &list).unwrap();
        let sig = portcullis_sig::sign(list.as_bytes(), &k).unwrap();
        std::fs::write(idx.join(SIG), base64::engine::general_purpose::STANDARD.encode(sig)).unwrap();
        let fetch = |p: &str| std::fs::read(idx.join(p)).map_err(|e| e.to_string());
        assert_eq!(verify(&fetch, &[pubk.clone()]).unwrap(), 2);
        let (_, other) = portcullis_sig::new_key([9; 32]).unwrap();
        assert!(verify(&fetch, &[other]).unwrap_err().contains("not signed"));
        std::fs::write(idx.join("pe/rg/pergola"), "{\"name\":\"evil\"}\n").unwrap();
        assert!(verify(&fetch, &[pubk]).unwrap_err().contains("doesn't match"));
        std::fs::remove_dir_all(&d).unwrap();
    }
}
