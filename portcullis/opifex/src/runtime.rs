//! The library closure for side-loaded bundles (the bring-up exception in
//! atrium-bundle-format.md): walk the DT_NEEDED graph of every program in the
//! bundle (the entry, helpers it execs, workers) and copy the
//! libraries and the rtld from a sysroot into the app tree.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

// ── filesystem ──────────────────────────────────────────────────────────────

/// Resolve the shared-library closure of every program in the app tree so it
/// can run inside a jail whose rootfs is only that tree. Walks the DT_NEEDED
/// closure of the entry and of every other ELF file the bundle ships (helpers
/// in `bin/` it execs, `[[workers]]` programs, its own libraries), copies each library
/// under `dest` to `lib/` (base libraries from /lib) or `usr/lib/` (everything
/// else, pkg's /usr/local/lib too: a jail has no ld-elf.so.hints, so its
/// loader looks only there),
/// and copies the rtld (`/libexec/ld-elf.so.1`). Returns the count copied.
/// Idempotent: a lib the bundle already shipped is left as-is (but is still
/// walked, so ITS dependencies are resolved too).
///
/// Libraries are read from `sysroot` (default `/`). Pointing that at an extracted
/// FreeBSD sysroot is what lets opifex run on a cross-build host instead of only
/// on the target — see the module docs.
///
/// We parse DT_NEEDED directly rather than shelling out to ldd(1) because ldd
/// only exists on the target, and it resolves against the RUNNING system rather
/// than a sysroot. Note ldd prints the whole transitive closure while DT_NEEDED
/// lists only direct dependencies, so this MUST recurse: for portcullisd, ldd
/// reports 5 libraries but the binary's own DT_NEEDED names 4 — libsys.so.7
/// arrives through libc.
pub fn resolve_runtime(entry: &Path, dest: &Path, sysroot: &Path) -> Result<usize, String> {
    resolve(entry, dest, sysroot, true).map(|r| r.copied)
}

/// What [`resolve`] did: files copied, and the base-system files now in the
/// tree (path in the tree, sha256), which [`refresh_base`] replaces after a
/// base update.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Resolved {
    pub copied: usize,
    pub base: Vec<(String, String)>,
}

/// The base system's library directories (the rtld's built-in path), and
/// pkg's, which only side-loads may take from.
const BASE_DIRS: [&str; 2] = ["lib", "usr/lib"];
const RTLD: &str = "libexec/ld-elf.so.1";

/// Resolve every program's library closure into `dest`:
/// - the rtld and every base-system library (`/lib`, `/usr/lib`) come from
///   the base, always, replacing a copy the bundle shipped (a publisher can't
///   pass off its own libc as the base's); they are listed in `base`;
/// - a library the bundle ships under `lib/` or `usr/lib/` that the base
///   doesn't have is the bundle's own, covered by its signature;
/// - with `local` (side-loads), pkg's `/usr/local/lib` too, copied to
///   `usr/lib/`; catalog packages must ship those themselves.
pub fn resolve(entry: &Path, dest: &Path, sysroot: &Path, local: bool) -> Result<Resolved, String> {
    let mut out = Resolved::default();
    let put_base = |rel: &str, out: &mut Resolved| -> Result<(), String> {
        let from = sysroot.join(rel);
        let bytes = std::fs::read(&from).map_err(|e| format!("{}: {e}", from.display()))?;
        let to = dest.join(rel);
        let same = std::fs::read(&to).is_ok_and(|b| b == bytes);
        if !same {
            let _ = std::fs::remove_file(&to);
            out.copied += stage_to(&Path::new("/").join(rel), Path::new(rel), dest, sysroot)?;
        }
        out.base.push((rel.to_string(), hex(&Sha256::digest(&bytes))));
        Ok(())
    };
    // The dynamic loader the kernel execs first.
    put_base(RTLD, &mut out)?;

    // Breadth-first over the DT_NEEDED graph. `seen` is keyed on the soname so a
    // diamond (two libs both needing libc) is resolved once.
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut queue: Vec<PathBuf> = vec![entry.to_path_buf()];
    // The bundle's other ELF files: an ELF the walk can't read (another
    // class or byte order, say a firmware blob) is skipped; the entry's
    // must parse.
    let mut shipped = Vec::new();
    elf_files(dest, &mut shipped);
    shipped.sort();
    for f in shipped {
        if f != entry && std::fs::read(&f).is_ok_and(|b| elf_needed(&b, &f).is_ok()) {
            queue.push(f);
        }
    }
    queue.reverse(); // the entry first
    while let Some(obj) = queue.pop() {
        let bytes = match std::fs::read(&obj) {
            Ok(b) => b,
            Err(e) => return Err(format!("read {}: {e}", obj.display())),
        };
        for soname in elf_needed(&bytes, &obj)? {
            if !seen.insert(soname.clone()) {
                continue;
            }
            // The base's: always the base's own copy.
            if let Some(d) = BASE_DIRS.iter().find(|d| sysroot.join(d).join(&soname).is_file()) {
                let rel = format!("{d}/{soname}");
                put_base(&rel, &mut out)?;
                queue.push(dest.join(&rel));
                continue;
            }
            // Shipped by the bundle where the jail's loader looks: its own.
            if let Some(have) = BASE_DIRS.iter().map(|d| dest.join(d).join(&soname)).find(|p| p.is_file()) {
                queue.push(have);
                continue;
            }
            // pkg's, for a side-load. An app's jail has no ld-elf.so.hints, so
            // its loader looks only in /lib and /usr/lib: it goes to usr/lib.
            let abs = Path::new("/usr/local/lib").join(&soname);
            if !local || !sysroot.join("usr/local/lib").join(&soname).is_file() {
                return Err(format!(
                    "{}: needs {soname}, which is neither in the base system ({}) nor in the bundle{}",
                    obj.display(), sysroot.display(),
                    if local { " nor under /usr/local/lib" } else { " (a catalog package ships its own libraries)" }));
            }
            let to = Path::new("usr/lib").join(&soname);
            out.copied += stage_to(&abs, &to, dest, sysroot)?;
            queue.push(dest.join(&to));
        }
    }
    out.base.sort();
    out.base.dedup();
    Ok(out)
}

/// After a base-system update: does `base` (an app's recorded base files)
/// still match the base under `sysroot`? An empty list (an app installed
/// before these were recorded) never does.
pub fn base_current(base: &[(String, String)], sysroot: &Path) -> bool {
    !base.is_empty() && base.iter().all(|(rel, sha)| {
        std::fs::read(sysroot.join(rel)).is_ok_and(|b| hex(&Sha256::digest(&b)) == *sha)
    })
}

/// Build `new` as the app tree `app` with its base files taken from the
/// current base: every file hard-linked (no copy), then [`resolve`], which
/// unlinks and copies only the base files whose bytes changed. `app` itself is
/// never written. The caller swaps `new` in whole, so a launch sees the old
/// set or the new one, never a mix.
pub fn rebase(app: &Path, new: &Path, entry: &str, sysroot: &Path, local: bool) -> Result<Resolved, String> {
    link_tree(app, new)?;
    resolve(&new.join(entry), new, sysroot, local)
}

fn link_tree(src: &Path, dst: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir(dst).map_err(|e| format!("{}: {e}", dst.display()))?;
    for e in std::fs::read_dir(src).map_err(|e| format!("{}: {e}", src.display()))?.flatten() {
        let (from, to) = (e.path(), dst.join(e.file_name()));
        let t = e.file_type().map_err(|e| e.to_string())?;
        if t.is_dir() {
            link_tree(&from, &to)?;
        } else if t.is_symlink() {
            let l = std::fs::read_link(&from).map_err(|e| e.to_string())?;
            std::os::unix::fs::symlink(l, &to).map_err(|e| format!("{}: {e}", to.display()))?;
        } else {
            std::fs::hard_link(&from, &to).map_err(|e| format!("{}: {e}", to.display()))?;
        }
    }
    let mode = std::fs::metadata(src).map_err(|e| e.to_string())?.permissions().mode();
    std::fs::set_permissions(dst, std::fs::Permissions::from_mode(mode)).map_err(|e| e.to_string())
}

fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }

/// Regular files under `dir` (symlinks not followed) that start with the ELF
/// magic.
fn elf_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(t) = e.file_type() else { continue };
        let p = e.path();
        if t.is_dir() {
            elf_files(&p, out);
        } else if t.is_file() {
            let mut magic = [0u8; 4];
            let is_elf = std::fs::File::open(&p)
                .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut magic)).is_ok() && &magic == b"\x7fELF";
            if is_elf {
                out.push(p);
            }
        }
    }
}

/// Copy `abs` (a path on the target, read under `sysroot`) to `dest/to`.
fn stage_to(abs: &Path, to: &Path, dest: &Path, sysroot: &Path) -> Result<usize, String> {
    let rel = abs.strip_prefix("/").unwrap_or(abs);
    let to = dest.join(to);
    if to.exists() {
        return Ok(0);
    }
    let from = sysroot.join(rel);
    if !from.exists() {
        return Err(format!(
            "{} missing under sysroot {} (looked at {})",
            abs.display(),
            sysroot.display(),
            from.display()
        ));
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    std::fs::copy(&from, &to)
        .map_err(|e| format!("copy {} -> {}: {e}", from.display(), to.display()))?;
    // Preserve the executable bit; the rtld and libs must stay executable.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(md) = std::fs::metadata(&from) {
            let _ = std::fs::set_permissions(&to, std::fs::Permissions::from_mode(md.permissions().mode()));
        }
    }
    Ok(1)
}

// ---------------------------------------------------------------------------
// Minimal ELF64 reader — just enough to list DT_NEEDED.
//
// Deliberately hand-rolled instead of pulling in a crate: opifex's whole
// dependency set is two in-tree path crates, and the newcomer build script
// should not have to fetch an ELF library to install an app. Everything below
// is bounds-checked; a malformed file yields Err, never a panic.
// ---------------------------------------------------------------------------

fn rd_u16(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}
fn rd_u64(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

/// Return the DT_NEEDED sonames of an ELF64 little-endian object. A static
/// binary (no PT_DYNAMIC) yields an empty vec, which is not an error.
fn elf_needed(b: &[u8], what: &Path) -> Result<Vec<String>, String> {
    let bad = |m: &str| format!("{}: {m}", what.display());
    if b.len() < 64 || &b[0..4] != b"\x7fELF" {
        return Err(bad("not an ELF file"));
    }
    if b[4] != 2 || b[5] != 1 {
        return Err(bad("not ELF64 little-endian"));
    }
    let phoff = rd_u64(b, 0x20).ok_or_else(|| bad("truncated e_phoff"))? as usize;
    let phentsize = rd_u16(b, 0x36).ok_or_else(|| bad("truncated e_phentsize"))? as usize;
    let phnum = rd_u16(b, 0x38).ok_or_else(|| bad("truncated e_phnum"))? as usize;

    // Collect PT_LOAD segments so a vaddr can be mapped back to a file offset,
    // and locate PT_DYNAMIC.
    let mut loads: Vec<(u64, u64, u64)> = Vec::new(); // (vaddr, filesz, offset)
    let mut dynamic: Option<(usize, usize)> = None; // (offset, filesz)
    for i in 0..phnum {
        let ph = phoff + i * phentsize;
        let p_type = rd_u16(b, ph).ok_or_else(|| bad("truncated phdr"))? as u32
            | (rd_u16(b, ph + 2).ok_or_else(|| bad("truncated phdr"))? as u32) << 16;
        let p_offset = rd_u64(b, ph + 0x08).ok_or_else(|| bad("truncated p_offset"))?;
        let p_vaddr = rd_u64(b, ph + 0x10).ok_or_else(|| bad("truncated p_vaddr"))?;
        let p_filesz = rd_u64(b, ph + 0x20).ok_or_else(|| bad("truncated p_filesz"))?;
        match p_type {
            1 => loads.push((p_vaddr, p_filesz, p_offset)), // PT_LOAD
            2 => dynamic = Some((p_offset as usize, p_filesz as usize)), // PT_DYNAMIC
            _ => {}
        }
    }
    let Some((dyn_off, dyn_sz)) = dynamic else {
        return Ok(Vec::new()); // static binary
    };

    let vaddr_to_off = |v: u64| -> Option<usize> {
        for (vaddr, filesz, offset) in &loads {
            if v >= *vaddr && v < vaddr + filesz {
                return Some((offset + (v - vaddr)) as usize);
            }
        }
        None
    };

    // First pass for DT_STRTAB (a vaddr), second for the DT_NEEDED offsets.
    let mut strtab: Option<usize> = None;
    let mut needed_offs: Vec<u64> = Vec::new();
    let mut i = dyn_off;
    while i + 16 <= dyn_off + dyn_sz {
        let tag = rd_u64(b, i).ok_or_else(|| bad("truncated Elf64_Dyn"))?;
        let val = rd_u64(b, i + 8).ok_or_else(|| bad("truncated Elf64_Dyn"))?;
        match tag {
            0 => break,                       // DT_NULL
            1 => needed_offs.push(val),       // DT_NEEDED
            5 => strtab = vaddr_to_off(val),  // DT_STRTAB
            _ => {}
        }
        i += 16;
    }
    let Some(strtab) = strtab else {
        if needed_offs.is_empty() {
            return Ok(Vec::new());
        }
        return Err(bad("has DT_NEEDED but no resolvable DT_STRTAB"));
    };

    let mut out = Vec::with_capacity(needed_offs.len());
    for n in needed_offs {
        let start = strtab + n as usize;
        let end = b[start..].iter().position(|&c| c == 0).ok_or_else(|| bad("unterminated soname"))? + start;
        out.push(String::from_utf8_lossy(&b[start..end]).into_owned());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal ELF64 LE with one PT_LOAD over the file, a PT_DYNAMIC, and
    /// DT_NEEDED entries for `needed`.
    fn elf(needed: &[&str]) -> Vec<u8> {
        let mut strtab = vec![0u8];
        let offs: Vec<u64> = needed.iter().map(|n| {
            let o = strtab.len() as u64;
            strtab.extend_from_slice(n.as_bytes());
            strtab.push(0);
            o
        }).collect();
        let (ph, str_off) = (64usize, 64 + 2 * 56);
        let dyn_off = str_off + strtab.len();
        let mut dynv: Vec<(u64, u64)> = offs.iter().map(|o| (1, *o)).collect();
        dynv.push((5, str_off as u64));
        dynv.push((0, 0));
        let total = dyn_off + dynv.len() * 16;
        let mut b = vec![0u8; total];
        b[0..4].copy_from_slice(b"\x7fELF");
        b[4] = 2;
        b[5] = 1;
        b[0x20..0x28].copy_from_slice(&(ph as u64).to_le_bytes());
        b[0x36..0x38].copy_from_slice(&56u16.to_le_bytes());
        b[0x38..0x3a].copy_from_slice(&2u16.to_le_bytes());
        let mut phdr = |i: usize, t: u32, off: u64, sz: u64| {
            let p = ph + i * 56;
            b[p..p + 4].copy_from_slice(&t.to_le_bytes());
            b[p + 8..p + 16].copy_from_slice(&off.to_le_bytes());
            b[p + 16..p + 24].copy_from_slice(&off.to_le_bytes());
            b[p + 32..p + 40].copy_from_slice(&sz.to_le_bytes());
        };
        phdr(0, 1, 0, total as u64);
        phdr(1, 2, dyn_off as u64, (dynv.len() * 16) as u64);
        b[str_off..dyn_off].copy_from_slice(&strtab);
        for (i, (t, v)) in dynv.iter().enumerate() {
            let p = dyn_off + i * 16;
            b[p..p + 8].copy_from_slice(&t.to_le_bytes());
            b[p + 8..p + 16].copy_from_slice(&v.to_le_bytes());
        }
        b
    }

    fn put(root: &Path, rel: &str, bytes: &[u8]) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    /// The Player case: the entry needs libc, a helper it execs needs libm,
    /// a worker needs libc++ (which needs libcxxrt). All of them land.
    #[test]
    fn every_program_in_the_bundle() {
        let d = std::env::temp_dir().join(format!("opifex-runtime-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let (sys, app) = (d.join("sys"), d.join("app"));
        put(&sys, "libexec/ld-elf.so.1", &elf(&[]));
        put(&sys, "lib/libc.so.7", &elf(&[]));
        put(&sys, "lib/libm.so.5", &elf(&["libc.so.7"]));
        put(&sys, "usr/lib/libc++.so.1", &elf(&["libcxxrt.so.1", "libm.so.5"]));
        put(&sys, "lib/libcxxrt.so.1", &elf(&["libc.so.7"]));
        put(&sys, "usr/lib/libunused.so.1", &elf(&[]));
        put(&sys, "usr/local/lib/libdav1d.so.7", &elf(&["libc.so.7"]));
        put(&app, "bin/player", &elf(&["libc.so.7"]));
        put(&app, "bin/helper", &elf(&["libm.so.5"]));
        put(&app, "workers/decode", &elf(&["libc++.so.1", "libdav1d.so.7", "libown.so.1"]));
        put(&app, "usr/lib/libown.so.1", &elf(&["libc.so.7"]));
        put(&app, "share/firmware.bin", b"\x7fELF\x01\x02 not ours");
        put(&app, "share/readme", b"text");
        let n = resolve_runtime(&app.join("bin/player"), &app, &sys).unwrap();
        for f in ["libexec/ld-elf.so.1", "lib/libc.so.7", "lib/libm.so.5", "usr/lib/libc++.so.1", "lib/libcxxrt.so.1",
                  "usr/lib/libdav1d.so.7"] {
            assert!(app.join(f).is_file(), "{f} missing");
        }
        // pkg's libraries go where the jail's loader looks; a shipped one is used as is.
        assert!(!app.join("usr/local").exists());
        assert!(!app.join("usr/lib/libunused.so.1").exists());
        assert_eq!(n, 6);
        // A missing library of a helper is an error, not a jail that fails later.
        put(&app, "bin/other", &elf(&["libnope.so.1"]));
        assert!(resolve_runtime(&app.join("bin/player"), &app, &sys).unwrap_err().contains("libnope.so.1"));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The base's files always come from the base: a bundle's own "libc"
    /// is replaced, and only pkg libraries a side-load may take.
    #[test]
    fn base_files_are_the_base() {
        let d = std::env::temp_dir().join(format!("opifex-runtime-base-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let (sys, app) = (d.join("sys"), d.join("app"));
        put(&sys, "libexec/ld-elf.so.1", &elf(&[]));
        put(&sys, "lib/libc.so.7", &elf(&[]));
        put(&sys, "usr/local/lib/libpkg.so.1", &elf(&["libc.so.7"]));
        put(&app, "bin/x", &elf(&["libc.so.7", "libown.so.1"]));
        put(&app, "lib/libc.so.7", b"\x7fELF a publisher's libc");
        put(&app, "usr/lib/libown.so.1", &elf(&[]));
        let r = resolve(&app.join("bin/x"), &app, &sys, false).unwrap();
        assert_eq!(std::fs::read(app.join("lib/libc.so.7")).unwrap(), elf(&[]));
        let names: Vec<&str> = r.base.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(names, ["lib/libc.so.7", "libexec/ld-elf.so.1"]);
        assert!(base_current(&r.base, &sys));
        // A catalog package doesn't get pkg's libraries; a side-load does.
        put(&app, "bin/y", &elf(&["libpkg.so.1"]));
        assert!(resolve(&app.join("bin/x"), &app, &sys, false).unwrap_err().contains("ships its own"));
        resolve(&app.join("bin/x"), &app, &sys, true).unwrap();
        assert!(app.join("usr/lib/libpkg.so.1").is_file());

        // A base update: libc changes. The rebased tree has the new libc and
        // a new dependency of it; the installed tree is untouched.
        put(&sys, "lib/libsys.so.7", &elf(&[]));
        put(&sys, "lib/libc.so.7", &elf(&["libsys.so.7"]));
        assert!(!base_current(&r.base, &sys));
        let new = d.join("new");
        let r2 = rebase(&app, &new, "bin/x", &sys, true).unwrap();
        assert!(r2.copied >= 2);
        assert_eq!(std::fs::read(new.join("lib/libc.so.7")).unwrap(), elf(&["libsys.so.7"]));
        assert!(new.join("lib/libsys.so.7").is_file());
        assert_eq!(std::fs::read(app.join("lib/libc.so.7")).unwrap(), elf(&[]), "the old tree is never written");
        assert_eq!(std::fs::read(new.join("usr/lib/libown.so.1")).unwrap(), elf(&[]));
        assert!(base_current(&r2.base, &sys));
        let _ = std::fs::remove_dir_all(&d);
    }
}
