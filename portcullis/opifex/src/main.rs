//! `opifex` — Atrium's binary app installer for FreeBSD.
//!
//! The pkg-style counterpart to `insula` (the ports/Homebrew-style
//! compile-from-source tool): opifex takes a *pre-built*, signed Insula bundle and
//! installs it into the Portcullis app tree that the jail launcher (`portcullisd`)
//! reads — `/var/lib/atrium/apps/<id>/`.
//!
//! It is jail-aware. A Portcullis jail's rootfs is ONLY the app tree (a nullfs+
//! unionfs of `apps/<id>` + an overlay) — there is no shared `/lib` mounted in. So
//! a dynamically-linked app must carry its own runtime. opifex walks the entry
//! binary's DT_NEEDED closure and copies those libraries plus the rtld into the
//! app tree at install time, making the bundle self-contained for its jail. (A
//! bundle that already ships its libs is fine too — those are copied first and
//! left alone, though they are still walked for their own dependencies.)
//!
//! ```text
//! opifex install <bundle-dir> [--allow-unsigned] [--root <dir>] [--sysroot <dir>]
//! opifex list                 [--root <dir>]
//! opifex uninstall <app-id>   [--root <dir>]
//! ```
//! `--root` is the Atrium state root (default `/var/lib/atrium`); apps install
//! under `<root>/apps/<id>/`, with the writable overlay at `<root>/overlays/<id>/`.
//!
//! `--sysroot` is where runtime libraries are READ FROM (default `/`, i.e. the
//! running system). It exists so opifex can run on a cross-build host: point it
//! at an extracted FreeBSD sysroot and a bundle can be installed without a target
//! machine. That is why the library closure is computed by parsing ELF DT_NEEDED
//! rather than by shelling out to `ldd(1)`, which only exists on the target and
//! only ever resolves against the running system.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

const DEFAULT_ROOT: &str = "/var/lib/atrium";
/// Where runtime libraries are read from. "/" = the running system (the native,
/// on-target case). Point it at an extracted FreeBSD sysroot to install from a
/// cross-build host.
const DEFAULT_SYSROOT: &str = "/";
/// The trusted-publisher set (same root portcullisd checks). Empty = trust not
/// configured → unsigned allowed with a loud warning (auditable, never silent).
const PUBLISHERS_DIR: &str = "/etc/atrium/publishers";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
        return ExitCode::from(2);
    }
    let res = match args[1].as_str() {
        "install"   => cmd_install(&args[2..]),
        "list"      => cmd_list(&args[2..]),
        "uninstall" => cmd_uninstall(&args[2..]),
        "help" | "-h" | "--help" => { usage(); Ok(()) }
        other => { eprintln!("opifex: unknown command: {other}"); usage(); return ExitCode::from(2); }
    };
    match res {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => { eprintln!("opifex: {e}"); ExitCode::FAILURE }
    }
}

fn usage() {
    eprintln!(
        "Usage: opifex <command> [args]\n\n\
         Commands:\n  \
           install <bundle-dir> [--allow-unsigned] [--root <dir>] [--sysroot <dir>]\n      \
             Install a pre-built signed bundle into the Portcullis app tree.\n  \
           list [--root <dir>]                 Show installed apps.\n  \
           uninstall <app-id> [--root <dir>]   Remove an app + its overlay.\n\n\
         --root:    Atrium state root, installed INTO (default {DEFAULT_ROOT}).\n\
         --sysroot: where runtime libs are READ FROM (default {DEFAULT_SYSROOT});\n      \
           point at a FreeBSD sysroot to install from a cross-build host."
    );
}

/// Pull `--root <dir>` and `--sysroot <dir>` (leaving positionals) out of an arg
/// slice. `--sysroot` is where runtime libraries are read FROM; `--root` is the
/// Atrium state root they are installed INTO. They are independent: installing
/// into a mounted target root while reading libs from a cross sysroot is valid.
fn take_root(args: &[String]) -> (PathBuf, PathBuf, Vec<String>) {
    let mut root = PathBuf::from(DEFAULT_ROOT);
    let mut sysroot = PathBuf::from(DEFAULT_SYSROOT);
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--root" => { if let Some(v) = it.next() { root = PathBuf::from(v); } }
            "--sysroot" => { if let Some(v) = it.next() { sysroot = PathBuf::from(v); } }
            _ => rest.push(a.clone()),
        }
    }
    (root, sysroot, rest)
}

fn cmd_install(args: &[String]) -> Result<(), String> {
    let (root, sysroot, rest) = take_root(args);
    let mut allow_unsigned = false;
    let mut src: Option<&str> = None;
    for a in &rest {
        match a.as_str() {
            "--allow-unsigned" => allow_unsigned = true,
            other if other.starts_with("--") => return Err(format!("install: unknown flag '{other}'")),
            other => src = Some(other),
        }
    }
    let bundle = PathBuf::from(src.ok_or("install: missing <bundle-dir>")?);

    // 1. Read + validate the manifest.
    let manifest_path = bundle.join("atrium.toml");
    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("read {}: {e}", manifest_path.display()))?;
    let manifest = portcullis_toml::Manifest::from_str(&text)
        .map_err(|e| format!("parse {}: {e:?}", manifest_path.display()))?;
    let report = portcullis_toml::validate(&manifest);
    if !report.is_ok() {
        return Err(format!("invalid manifest: {}", report.errors.join("; ")));
    }
    let id = &manifest.app.id;
    let entry = manifest.entry(); // canonical: [bundle].entry → [app].entry

    // 2. Trust gate — the SAME check portcullisd applies at launch, so install and
    //    launch agree. --allow-unsigned is the explicit dev escape hatch.
    verify_signature(&bundle, &text, allow_unsigned)?;

    // 3. Place the bundle into the app tree (clean install).
    let dest = root.join("apps").join(id);
    if dest.exists() {
        std::fs::remove_dir_all(&dest).map_err(|e| format!("clean {}: {e}", dest.display()))?;
    }
    std::fs::create_dir_all(&dest).map_err(|e| format!("mkdir {}: {e}", dest.display()))?;
    copy_tree(&bundle, &dest)?;

    // 4. Make it self-contained for its jail: resolve the entry's lib closure +
    //    the rtld into the tree (the jail rootfs has no shared /lib).
    let entry_path = dest.join(entry);
    if !entry_path.exists() {
        return Err(format!("manifest entry {entry:?} not found in bundle ({})", entry_path.display()));
    }
    let added = resolve_runtime(&entry_path, &dest, &sysroot)?;

    println!("opifex: installed {id} -> {}", dest.display());
    println!("opifex:   entry {entry}, {added} runtime file(s) resolved into the app tree");
    Ok(())
}

fn cmd_list(args: &[String]) -> Result<(), String> {
    let (root, _sysroot, _rest) = take_root(args);
    let apps = root.join("apps");
    let Ok(entries) = std::fs::read_dir(&apps) else {
        println!("opifex: no apps installed under {}", apps.display());
        return Ok(());
    };
    let mut rows: Vec<(String, String, String)> = Vec::new();
    for e in entries.flatten() {
        let m = e.path().join("atrium.toml");
        if let Ok(text) = std::fs::read_to_string(&m) {
            if let Ok(man) = portcullis_toml::Manifest::from_str(&text) {
                rows.push((man.app.id, man.app.name, man.app.entry));
            }
        }
    }
    rows.sort();
    if rows.is_empty() {
        println!("opifex: no apps installed under {}", apps.display());
    }
    for (id, name, entry) in rows {
        println!("  {id:<28} {name:<20} {entry}");
    }
    Ok(())
}

fn cmd_uninstall(args: &[String]) -> Result<(), String> {
    let (root, _sysroot, rest) = take_root(args);
    let id = rest.first().ok_or("uninstall: missing <app-id>")?;
    let mut removed = false;
    for sub in ["apps", "overlays"] {
        let p = root.join(sub).join(id);
        if p.exists() {
            std::fs::remove_dir_all(&p).map_err(|e| format!("remove {}: {e}", p.display()))?;
            println!("opifex: removed {}", p.display());
            removed = true;
        }
    }
    if !removed {
        return Err(format!("{id} is not installed under {}", root.display()));
    }
    Ok(())
}

// ── trust ─────────────────────────────────────────────────────────────────────

fn verify_signature(bundle: &Path, manifest_text: &str, allow_unsigned: bool) -> Result<(), String> {
    if allow_unsigned {
        eprintln!("opifex: --allow-unsigned: skipping signature verification for {}", bundle.display());
        return Ok(());
    }
    let publishers = load_publishers(PUBLISHERS_DIR);
    if publishers.is_empty() {
        eprintln!("opifex: WARNING manifest trust not configured ({PUBLISHERS_DIR} empty); \
                   allowing UNSIGNED {}", bundle.display());
        return Ok(());
    }
    let sig = read_signature(&bundle.join("atrium.toml.sig"));
    portcullis_sig::verify_trusted(manifest_text.as_bytes(), &sig, &publishers)
        .map(|()| eprintln!("opifex: signature verified (trusted publisher)"))
        .map_err(|e| format!("manifest not signed by a trusted publisher ({e:?})"))
}

fn load_publishers(dir: &str) -> Vec<String> {
    let mut keys = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("pem") {
                if let Ok(pem) = std::fs::read_to_string(&p) { keys.push(pem); }
            }
        }
    }
    keys
}

fn read_signature(sig_path: &Path) -> Vec<u8> {
    let raw = std::fs::read(sig_path).unwrap_or_default();
    if let Ok(s) = std::str::from_utf8(&raw) {
        if let Ok(der) = portcullis_sig::sig_from_base64(s) { return der; }
    }
    raw
}

// ── filesystem ──────────────────────────────────────────────────────────────

/// Recursively copy `src`'s contents into `dst` (both dirs exist).
fn copy_tree(src: &Path, dst: &Path) -> Result<(), String> {
    for e in std::fs::read_dir(src).map_err(|e| format!("read {}: {e}", src.display()))?.flatten() {
        let from = e.path();
        let to = dst.join(e.file_name());
        let ft = e.file_type().map_err(|e| format!("stat {}: {e}", from.display()))?;
        if ft.is_dir() {
            std::fs::create_dir_all(&to).map_err(|e| format!("mkdir {}: {e}", to.display()))?;
            copy_tree(&from, &to)?;
        } else if ft.is_file() {
            std::fs::copy(&from, &to).map_err(|e| format!("copy {} -> {}: {e}", from.display(), to.display()))?;
        }
        // (sockets/symlinks in a bundle dir are not expected; skipped.)
    }
    Ok(())
}

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
fn resolve_runtime(entry: &Path, dest: &Path, sysroot: &Path) -> Result<usize, String> {
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
