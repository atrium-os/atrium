//! The library closure for side-loaded bundles (the bring-up exception in
//! atrium-bundle-format.md): walk the entry's DT_NEEDED graph and copy the
//! libraries and the rtld from a sysroot into the app tree.

use std::path::{Path, PathBuf};

// ── filesystem ──────────────────────────────────────────────────────────────

/// Resolve the entry binary's shared-library closure into the app tree so it can
/// run inside a jail whose rootfs is only that tree. Walks the entry binary's
/// DT_NEEDED closure, copies each library to the SAME absolute path under `dest`,
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
    let mut count = 0;
    // The dynamic loader the kernel execs first.
    count += stage_sysroot_file(Path::new("/libexec/ld-elf.so.1"), dest, sysroot)?;

    // Breadth-first over the DT_NEEDED graph. `seen` is keyed on the soname so a
    // diamond (two libs both needing libc) is resolved once.
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut queue: Vec<PathBuf> = vec![entry.to_path_buf()];
    while let Some(obj) = queue.pop() {
        let bytes = match std::fs::read(&obj) {
            Ok(b) => b,
            // A lib named in DT_NEEDED that we already staged may be unreadable
            // only if something raced us; treat as fatal rather than guess.
            Err(e) => return Err(format!("read {}: {e}", obj.display())),
        };
        for soname in elf_needed(&bytes, &obj)? {
            if !seen.insert(soname.clone()) {
                continue;
            }
            let abs = find_lib(&soname, sysroot).ok_or_else(|| {
                format!(
                    "{}: needs {soname}, not found under sysroot {} (searched {})",
                    obj.display(),
                    sysroot.display(),
                    LIB_SEARCH_DIRS.join(", ")
                )
            })?;
            count += stage_sysroot_file(&abs, dest, sysroot)?;
            // Recurse through the copy in the app tree (identical bytes, and it
            // is guaranteed present now).
            queue.push(dest.join(abs.strip_prefix("/").unwrap_or(&abs)));
        }
    }
    Ok(count)
}

/// Absolute directories searched for a DT_NEEDED soname, relative to the sysroot.
/// FreeBSD's default rtld path; we do not honour DT_RPATH/DT_RUNPATH because an
/// Atrium bundle's libs are staged to their canonical absolute paths anyway.
const LIB_SEARCH_DIRS: [&str; 3] = ["/lib", "/usr/lib", "/usr/local/lib"];

/// Resolve a soname to its absolute (target-namespace) path under `sysroot`.
fn find_lib(soname: &str, sysroot: &Path) -> Option<PathBuf> {
    for dir in LIB_SEARCH_DIRS {
        let abs = PathBuf::from(dir).join(soname);
        let on_disk = sysroot.join(abs.strip_prefix("/").unwrap_or(&abs));
        if on_disk.exists() {
            return Some(abs);
        }
    }
    None
}

/// Copy `<sysroot>/<abs>` to `<dest>/<abs>`, preserving the absolute layout the
/// rtld will look for inside the jail. Returns 1 if copied, 0 if the bundle
/// already shipped it.
///
/// A missing source is an ERROR. It used to return Ok(0), which meant that on a
/// host without FreeBSD libs opifex reported success and emitted a bundle with no
/// libraries in it at all — the failure only surfaced later as an exec failure
/// inside a jail, which is a miserable place to debug it.
fn stage_sysroot_file(abs: &Path, dest: &Path, sysroot: &Path) -> Result<usize, String> {
    let rel = abs.strip_prefix("/").unwrap_or(abs);
    let to = dest.join(rel);
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
