//! `cargo atrium bundle` (dev-tools.md §4.5 and §5.5, opifex.md §7.4 and
//! §10): an installable bundle from a project.
//!
//! The bundle is a tree as in atrium-bundle-format.md: `atrium.toml`, the
//! core and each worker at their `entry` paths, the icon, and the runtime
//! closure (§5 there): every library the programs need, found by reading
//! their `DT_NEEDED` (never by running them), and the rtld. Base
//! libraries keep their paths (`lib/`, `usr/lib/`); one found anywhere
//! else (`usr/local/lib` from pkg, a `DT_RUNPATH`, `$ORIGIN`, a linked
//! workspace) goes to `usr/lib/`. An app's jail has no
//! `ld-elf.so.hints`, so the loader looks only in its built-in `/lib` and
//! `/usr/lib`. `opifex pack` then writes the content
//! index and, with `--sign`, the signature, and `opifex archive` the
//! `.insula` file to side-load.
//!
//! Inside a workspace the libraries come from what the workspace sees
//! (its own `/usr/local`, `/workspaces/<name>`); from Linux or macOS they
//! come from the SDK sysroot, so an app there can only need base libraries.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// What a program's dynamic section says.
#[derive(Debug, Default, PartialEq)]
pub struct Dynamic {
    /// `PT_INTERP`: the rtld; none for a static program.
    pub interp: Option<String>,
    pub needed: Vec<String>,
    /// `DT_RUNPATH`, or `DT_RPATH` when there is none, split at `:`.
    pub runpath: Vec<String>,
}

fn u16_at(b: &[u8], o: usize) -> Option<u16> { b.get(o..o + 2).map(|s| u16::from_le_bytes([s[0], s[1]])) }
fn u32_at(b: &[u8], o: usize) -> Option<u32> { b.get(o..o + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap())) }
fn u64_at(b: &[u8], o: usize) -> Option<u64> { b.get(o..o + 8).map(|s| u64::from_le_bytes(s.try_into().unwrap())) }

/// Read an ELF64 little-endian file's interpreter, needed libraries and
/// run path. Bounds-checked throughout: it reads what the build made, but
/// a truncated file is an error, not a panic.
pub fn dynamic(b: &[u8]) -> Result<Dynamic, String> {
    if b.len() < 64 || &b[..4] != b"\x7fELF" {
        return Err("not an ELF file".into());
    }
    if b[4] != 2 || b[5] != 1 {
        return Err("not ELF64 little-endian".into());
    }
    let t = || "truncated".to_string();
    let phoff = u64_at(b, 0x20).ok_or_else(t)? as usize;
    let phentsize = u16_at(b, 0x36).ok_or_else(t)? as usize;
    let phnum = u16_at(b, 0x38).ok_or_else(t)? as usize;
    let mut loads = Vec::new();
    let mut dynseg = None;
    let mut out = Dynamic::default();
    for i in 0..phnum {
        let ph = phoff.checked_add(i.checked_mul(phentsize).ok_or_else(t)?).ok_or_else(t)?;
        let kind = u32_at(b, ph).ok_or_else(t)?;
        let off = u64_at(b, ph + 0x08).ok_or_else(t)?;
        let vaddr = u64_at(b, ph + 0x10).ok_or_else(t)?;
        let filesz = u64_at(b, ph + 0x20).ok_or_else(t)?;
        match kind {
            1 => loads.push((vaddr, filesz, off)),
            2 => dynseg = Some((off as usize, filesz as usize)),
            3 => {
                let s = b.get(off as usize..(off as usize).saturating_add(filesz as usize)).ok_or_else(t)?;
                let s = s.split(|c| *c == 0).next().unwrap_or(&[]);
                out.interp = Some(String::from_utf8_lossy(s).into_owned());
            }
            _ => {}
        }
    }
    let Some((doff, dsz)) = dynseg else { return Ok(out) };
    let to_off = |v: u64| loads.iter().find(|(va, sz, _)| v >= *va && v < va + sz).map(|(va, _, o)| (o + (v - va)) as usize);
    let (mut strtab, mut needed, mut runpath, mut rpath) = (None, Vec::new(), None, None);
    let mut i = doff;
    while i + 16 <= doff.saturating_add(dsz) {
        let tag = u64_at(b, i).ok_or_else(t)?;
        let val = u64_at(b, i + 8).ok_or_else(t)?;
        match tag {
            0 => break,
            1 => needed.push(val),
            5 => strtab = to_off(val),
            15 => rpath = Some(val),
            29 => runpath = Some(val),
            _ => {}
        }
        i += 16;
    }
    let str_at = |o: u64| -> Result<String, String> {
        let base = strtab.ok_or("DT_NEEDED without DT_STRTAB")?;
        let s = b.get(base.checked_add(o as usize).ok_or_else(t)?..).ok_or_else(t)?;
        let end = s.iter().position(|c| *c == 0).ok_or_else(t)?;
        Ok(String::from_utf8_lossy(&s[..end]).into_owned())
    };
    for n in needed {
        out.needed.push(str_at(n)?);
    }
    if let Some(r) = runpath.or(rpath) {
        out.runpath = str_at(r)?.split(':').filter(|s| !s.is_empty()).map(str::to_string).collect();
    }
    Ok(out)
}

/// Where libraries are looked for, under the root they come from.
pub const LIB_DIRS: [&str; 3] = ["lib", "usr/lib", "usr/local/lib"];

/// Where a library found in `dir` goes in the bundle: one of the rtld's
/// built-in directories, which are all it searches in an app's jail (no
/// hints file there).
fn staged_dir(dir: &str) -> &'static str {
    if dir == "lib" { "lib" } else { "usr/lib" }
}

/// One file of the closure: where it is, and where it goes in the bundle.
#[derive(Debug, PartialEq)]
pub struct Staged {
    pub from: PathBuf,
    pub to: String,
}

/// `[bundle] extra-libs` (what the programs dlopen): file names, `*` and
/// `?` allowed.
pub fn extra_libs(atrium_toml: &str) -> Result<Vec<String>, String> {
    let m: toml::Value = atrium_toml.parse().map_err(|e| format!("atrium.toml: {e}"))?;
    let mut out = Vec::new();
    for v in m.get("bundle").and_then(|b| b.get("extra-libs")).and_then(|e| e.as_array()).into_iter().flatten() {
        let p = v.as_str().ok_or("[bundle] extra-libs: strings only")?;
        if !portcullis_toml::schema::valid_extra_lib(p) {
            return Err(format!("[bundle] extra-libs {p:?}: a library's file name (`*` and `?` allowed), not a path"));
        }
        out.push(p.to_string());
    }
    Ok(out)
}

/// The runtime closure of `programs`: the rtld and every library they
/// need, breadth-first, each soname once, and each `extra` library (a
/// pattern matched in [`LIB_DIRS`]) with what it needs. `root` is where
/// the base libraries are (`/` in a workspace, the SDK sysroot elsewhere).
pub fn closure(programs: &[PathBuf], root: &Path, extra: &[String]) -> Result<Vec<Staged>, String> {
    let mut out: Vec<Staged> = Vec::new();
    let mut seen = BTreeMap::new();
    let mut queue: Vec<(PathBuf, String)> = programs.iter().map(|p| (p.clone(), p.display().to_string())).collect();
    for pat in extra {
        let mut hit = false;
        for d in LIB_DIRS {
            let mut names: Vec<String> = std::fs::read_dir(root.join(d)).into_iter().flatten().flatten()
                .filter_map(|e| e.file_name().to_str().map(str::to_string))
                .filter(|n| portcullis_toml::schema::glob_match(pat, n)).collect();
            names.sort();
            for n in names {
                let from = root.join(d).join(&n);
                if !from.is_file() || seen.contains_key(&n) {
                    continue;
                }
                hit = true;
                seen.insert(n.clone(), ());
                queue.push((from.clone(), n.clone()));
                out.push(Staged { from, to: format!("{}/{n}", staged_dir(d)) });
            }
        }
        if !hit {
            return Err(format!("[bundle] extra-libs {pat:?} matches nothing in {} ({})", root.display(), LIB_DIRS.join(", ")));
        }
    }
    let mut need_rtld = None;
    while !queue.is_empty() {
        let (obj, label) = queue.remove(0);
        let bytes = std::fs::read(&obj).map_err(|e| format!("{}: {e}", obj.display()))?;
        let d = dynamic(&bytes).map_err(|e| format!("{label}: {e}"))?;
        if need_rtld.is_none() && programs.iter().any(|p| *p == obj) {
            need_rtld = d.interp.clone();
        }
        let origin = obj.parent().map(Path::to_path_buf).unwrap_or_default();
        for so in &d.needed {
            if seen.contains_key(so) {
                continue;
            }
            if so.contains('/') {
                return Err(format!("{label}: needs {so:?}, a path, not a soname"));
            }
            let mut found = None;
            for rp in &d.runpath {
                let dir = if let Some(rest) = rp.strip_prefix("$ORIGIN").or_else(|| rp.strip_prefix("${ORIGIN}")) {
                    origin.join(rest.trim_start_matches('/'))
                } else {
                    root.join(rp.trim_start_matches('/'))
                };
                let p = dir.join(so);
                if p.is_file() {
                    found = Some((p, format!("usr/lib/{so}")));
                    break;
                }
            }
            if found.is_none() {
                for d in LIB_DIRS {
                    let p = root.join(d).join(so);
                    if p.is_file() {
                        found = Some((p, format!("{}/{so}", staged_dir(d))));
                        break;
                    }
                }
            }
            let Some((from, to)) = found else {
                return Err(format!(
                    "{label}: needs {so}, which isn't in {} (looked in {}{})",
                    root.display(),
                    LIB_DIRS.join(", "),
                    if d.runpath.is_empty() { String::new() } else { format!(" and its run path {}", d.runpath.join(":")) }
                ));
            };
            seen.insert(so.clone(), ());
            queue.push((from.clone(), so.clone()));
            out.push(Staged { from, to });
        }
    }
    if let Some(rtld) = need_rtld {
        let rel = rtld.trim_start_matches('/').to_string();
        let from = root.join(&rel);
        if !from.is_file() {
            return Err(format!("the programs' loader {rtld} isn't in {}", root.display()));
        }
        out.insert(0, Staged { from, to: rel });
    }
    Ok(out)
}

/// The project's programs: the core's entry and each worker's, as
/// (bundle path, cargo binary name).
pub fn programs(atrium_toml: &str) -> Result<Vec<(String, String)>, String> {
    let m: toml::Value = atrium_toml.parse().map_err(|e| format!("atrium.toml: {e}"))?;
    let entry = m.get("bundle").and_then(|b| b.get("entry"))
        .or_else(|| m.get("app").and_then(|a| a.get("entry")))
        .and_then(|e| e.as_str()).ok_or("atrium.toml has no [app] entry")?;
    let mut out = vec![entry.to_string()];
    for w in m.get("workers").and_then(|w| w.as_array()).into_iter().flatten() {
        if let Some(e) = w.get("entry").and_then(|e| e.as_str()) {
            out.push(e.to_string());
        }
    }
    for e in &out {
        if !inside(e) {
            return Err(format!("entry {e:?} isn't a path in the bundle"));
        }
    }
    Ok(out.into_iter().map(|e| {
        let bin = e.rsplit('/').next().unwrap_or(&e).to_string();
        (e, bin)
    }).collect())
}

/// A relative path that stays inside the tree.
pub fn inside(p: &str) -> bool {
    !p.is_empty() && !p.starts_with('/') && p.split('/').all(|c| !c.is_empty() && c != ".." && c != ".")
}

/// The files the manifest names besides the programs: the icon, when it
/// is an SVG in the bundle rather than a named one.
pub fn named_files(atrium_toml: &str) -> Vec<String> {
    let Ok(m) = atrium_toml.parse::<toml::Value>() else { return Vec::new() };
    m.get("app").and_then(|a| a.get("icon")).and_then(|i| i.as_str())
        .filter(|i| portcullis_toml::icon::is_bundle_path(i)).map(|i| vec![i.to_string()]).unwrap_or_default()
}

/// Copy `from` to `tree/to`, keeping the mode.
pub fn place(from: &Path, tree: &Path, to: &str) -> Result<(), String> {
    let dest = tree.join(to);
    if let Some(d) = dest.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    std::fs::copy(from, &dest).map_err(|e| format!("{} -> {}: {e}", from.display(), dest.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(from).map(|m| m.permissions().mode() & 0o755).unwrap_or(0o644);
        let _ = std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(mode | 0o444));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal ELF64 with PT_INTERP, PT_LOAD and PT_DYNAMIC.
    fn elf(needed: &[&str], runpath: Option<&str>) -> Vec<u8> {
        let mut strtab = vec![0u8];
        let mut offs = Vec::new();
        for n in needed {
            offs.push(strtab.len() as u64);
            strtab.extend_from_slice(n.as_bytes());
            strtab.push(0);
        }
        let rp = runpath.map(|r| {
            let o = strtab.len() as u64;
            strtab.extend_from_slice(r.as_bytes());
            strtab.push(0);
            o
        });
        let interp = b"/libexec/ld-elf.so.1\0";
        let (phoff, phnum) = (64usize, 3usize);
        let interp_off = phoff + 56 * phnum;
        let str_off = interp_off + interp.len();
        let dyn_off = (str_off + strtab.len() + 7) & !7;
        let mut dynv: Vec<(u64, u64)> = offs.iter().map(|o| (1, *o)).collect();
        dynv.push((5, 0x1000 + str_off as u64));
        if let Some(o) = rp {
            dynv.push((29, o));
        }
        dynv.push((0, 0));
        let total = dyn_off + dynv.len() * 16;
        let mut b = vec![0u8; total];
        b[..4].copy_from_slice(b"\x7fELF");
        b[4] = 2;
        b[5] = 1;
        b[0x20..0x28].copy_from_slice(&(phoff as u64).to_le_bytes());
        b[0x36..0x38].copy_from_slice(&56u16.to_le_bytes());
        b[0x38..0x3a].copy_from_slice(&(phnum as u16).to_le_bytes());
        let mut ph = |i: usize, kind: u32, off: u64, vaddr: u64, sz: u64| {
            let p = phoff + i * 56;
            b[p..p + 4].copy_from_slice(&kind.to_le_bytes());
            b[p + 8..p + 16].copy_from_slice(&off.to_le_bytes());
            b[p + 16..p + 24].copy_from_slice(&vaddr.to_le_bytes());
            b[p + 32..p + 40].copy_from_slice(&sz.to_le_bytes());
        };
        ph(0, 3, interp_off as u64, 0, interp.len() as u64);
        ph(1, 1, 0, 0x1000, total as u64);
        ph(2, 2, dyn_off as u64, 0x1000 + dyn_off as u64, (dynv.len() * 16) as u64);
        b[interp_off..interp_off + interp.len()].copy_from_slice(interp);
        b[str_off..str_off + strtab.len()].copy_from_slice(&strtab);
        for (i, (t, v)) in dynv.iter().enumerate() {
            let o = dyn_off + i * 16;
            b[o..o + 8].copy_from_slice(&t.to_le_bytes());
            b[o + 8..o + 16].copy_from_slice(&v.to_le_bytes());
        }
        b
    }

    #[test]
    fn reads_dynamic() {
        let d = dynamic(&elf(&["libthr.so.3", "libc.so.7"], Some("$ORIGIN/../lib:/opt/x"))).unwrap();
        assert_eq!(d.interp.as_deref(), Some("/libexec/ld-elf.so.1"));
        assert_eq!(d.needed, ["libthr.so.3", "libc.so.7"]);
        assert_eq!(d.runpath, ["$ORIGIN/../lib", "/opt/x"]);
        let b = elf(&["libc.so.7"], None);
        for n in [10, 70, 200, b.len() - 3] {
            assert!(dynamic(&b[..n]).is_err(), "{n}");
        }
        assert!(dynamic(b"#!/bin/sh\n").is_err());
    }

    #[test]
    fn closes_over_libraries() {
        let dir = std::env::temp_dir().join(format!("atrium-bundle-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("root");
        for (p, needs) in [("lib/libc.so.7", vec![]), ("lib/libthr.so.3", vec!["libc.so.7"]), ("usr/lib/libz.so.6", vec!["libc.so.7"])] {
            std::fs::create_dir_all(root.join(p).parent().unwrap()).unwrap();
            std::fs::write(root.join(p), elf(&needs, None)).unwrap();
        }
        std::fs::create_dir_all(root.join("libexec")).unwrap();
        std::fs::write(root.join("libexec/ld-elf.so.1"), b"rtld").unwrap();
        std::fs::create_dir_all(dir.join("app/vendor")).unwrap();
        std::fs::write(dir.join("app/vendor/libmine.so.1"), elf(&["libz.so.6"], None)).unwrap();
        let prog = dir.join("app/prog");
        std::fs::write(&prog, elf(&["libthr.so.3", "libmine.so.1"], Some("$ORIGIN/vendor"))).unwrap();
        let c = closure(&[prog.clone()], &root, &[]).unwrap();
        let tos: Vec<&str> = c.iter().map(|s| s.to.as_str()).collect();
        assert_eq!(tos, ["libexec/ld-elf.so.1", "lib/libthr.so.3", "usr/lib/libmine.so.1", "lib/libc.so.7", "usr/lib/libz.so.6"]);
        std::fs::write(root.join("usr/lib/libp_a.so"), elf(&["libc.so.7"], None)).unwrap();
        std::fs::write(root.join("usr/lib/libp_b.so"), elf(&["libthr.so.3"], None)).unwrap();
        let c = closure(&[prog.clone()], &root, &["libp_*.so".into()]).unwrap();
        let tos: Vec<&str> = c.iter().map(|s| s.to.as_str()).collect();
        assert!(tos.contains(&"usr/lib/libp_a.so") && tos.contains(&"usr/lib/libp_b.so"), "{tos:?}");
        assert_eq!(tos.iter().filter(|t| **t == "lib/libthr.so.3").count(), 1, "{tos:?}");
        // pkg's libraries (usr/local/lib) go where the jail's loader looks.
        std::fs::create_dir_all(root.join("usr/local/lib")).unwrap();
        std::fs::write(root.join("usr/local/lib/libdav1d.so.7"), elf(&["libthr.so.3"], None)).unwrap();
        std::fs::write(root.join("usr/local/lib/libx_1.so"), elf(&[], None)).unwrap();
        let p2 = dir.join("app/prog2");
        std::fs::write(&p2, elf(&["libdav1d.so.7"], None)).unwrap();
        let c = closure(&[p2], &root, &["libx_*.so".into()]).unwrap();
        let tos: Vec<&str> = c.iter().map(|s| s.to.as_str()).collect();
        assert!(tos.contains(&"usr/lib/libdav1d.so.7") && tos.contains(&"usr/lib/libx_1.so"), "{tos:?}");
        assert!(!tos.iter().any(|t| t.starts_with("usr/local")), "{tos:?}");
        let e = closure(&[prog.clone()], &root, &["libq_*.so".into()]).unwrap_err();
        assert!(e.contains("libq_*.so"), "{e}");
        std::fs::write(&prog, elf(&["libnope.so.1"], None)).unwrap();
        let e = closure(&[prog], &root, &[]).unwrap_err();
        assert!(e.contains("needs libnope.so.1"), "{e}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_programs() {
        let m = "[app]\nid = \"org.x.y\"\nname = \"Y\"\nversion = \"1\"\nentry = \"bin/y\"\nicon = \"icons/y.svg\"\n\n[[workers]]\nname = \"d\"\nentry = \"bin/y-d\"\ninput = \"fd\"\n";
        assert_eq!(programs(m).unwrap(), [("bin/y".to_string(), "y".to_string()), ("bin/y-d".into(), "y-d".into())]);
        assert_eq!(named_files(m), ["icons/y.svg"]);
        assert!(named_files(&m.replace("icons/y.svg", "terminal")).is_empty());
        assert!(programs(&m.replace("bin/y-d", "../y-d")).is_err());
    }
}
