//! `opifex` — Atrium's binary app installer for FreeBSD.
//!
//! The pkg-style counterpart to `insula` (the ports/Homebrew-style
//! compile-from-source tool): opifex takes a *pre-built*, signed Insula bundle and
//! installs it into the Portcullis app tree that the jail launcher (`portcullisd`)
//! reads — `/var/lib/atrium/apps/<id>/`.
//!
//! It is jail-aware. A Portcullis jail's rootfs is ONLY the app tree (a nullfs+
//! unionfs of `apps/<id>` + an overlay) — there is no shared `/lib` mounted in. So
//! a dynamically-linked app must carry its own runtime. opifex resolves the entry
//! binary's shared-library closure (via `ldd`) and the rtld into the app tree at
//! install time, making the bundle self-contained for its jail. (A bundle that
//! already ships its libs is fine too — they're copied first, ldd just fills gaps.)
//!
//! ```text
//! opifex install <bundle-dir> [--allow-unsigned] [--root <dir>]
//! opifex list                 [--root <dir>]
//! opifex uninstall <app-id>   [--root <dir>]
//! ```
//! `--root` is the Atrium state root (default `/var/lib/atrium`); apps install
//! under `<root>/apps/<id>/`, with the writable overlay at `<root>/overlays/<id>/`.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const DEFAULT_ROOT: &str = "/var/lib/atrium";
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
           install <bundle-dir> [--allow-unsigned] [--root <dir>]\n      \
             Install a pre-built signed bundle into the Portcullis app tree.\n  \
           list [--root <dir>]                 Show installed apps.\n  \
           uninstall <app-id> [--root <dir>]   Remove an app + its overlay.\n\n\
         --root: Atrium state root (default {DEFAULT_ROOT})."
    );
}

/// Pull `--root <dir>` (and leave positionals) out of an arg slice.
fn take_root(args: &[String]) -> (PathBuf, Vec<String>) {
    let mut root = PathBuf::from(DEFAULT_ROOT);
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--root" {
            if let Some(v) = it.next() { root = PathBuf::from(v); }
        } else {
            rest.push(a.clone());
        }
    }
    (root, rest)
}

fn cmd_install(args: &[String]) -> Result<(), String> {
    let (root, rest) = take_root(args);
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
    let added = resolve_runtime(&entry_path, &dest)?;

    println!("opifex: installed {id} -> {}", dest.display());
    println!("opifex:   entry {entry}, {added} runtime file(s) resolved into the app tree");
    Ok(())
}

fn cmd_list(args: &[String]) -> Result<(), String> {
    let (root, _) = take_root(args);
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
    let (root, rest) = take_root(args);
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
/// run inside a jail whose rootfs is only that tree. Runs `ldd` on the installed
/// binary, copies each resolved library to the SAME absolute path under `dest`,
/// and copies the rtld (`/libexec/ld-elf.so.1`). Returns the count copied.
/// Idempotent: a lib the bundle already shipped is left as-is.
fn resolve_runtime(entry: &Path, dest: &Path) -> Result<usize, String> {
    let mut count = 0;
    // The dynamic loader the kernel execs first.
    count += stage_host_file(Path::new("/libexec/ld-elf.so.1"), dest)?;
    let out = Command::new("ldd").arg(entry).output()
        .map_err(|e| format!("run ldd {}: {e}", entry.display()))?;
    if !out.status.success() {
        // Statically linked (or ldd refused) — nothing to resolve.
        return Ok(count);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        // Format: "\tlibfoo.so.7 => /lib/libfoo.so.7 (0x...)"
        let Some(idx) = line.find("=>") else { continue };
        let after = line[idx + 2..].trim();
        let path = after.split_whitespace().next().unwrap_or("");
        if path.starts_with('/') {
            count += stage_host_file(Path::new(path), dest)?;
        }
    }
    Ok(count)
}

/// Copy a host file to the same absolute path under `dest`, if not already there.
/// Returns 1 if copied, 0 if it already existed (bundle shipped it) or is absent.
fn stage_host_file(host_path: &Path, dest: &Path) -> Result<usize, String> {
    let rel = host_path.strip_prefix("/").unwrap_or(host_path);
    let to = dest.join(rel);
    if to.exists() {
        return Ok(0);
    }
    if !host_path.exists() {
        return Ok(0);
    }
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    std::fs::copy(host_path, &to).map_err(|e| format!("copy {} -> {}: {e}", host_path.display(), to.display()))?;
    Ok(1)
}
