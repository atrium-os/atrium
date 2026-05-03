//! portcullis — CLI front-end.
//!
//! Phase 1: `validate <atrium.toml>`
//! Phase 2: `launch --dry-run <app-tree>` (renders jail.conf;
//!          doesn't invoke jail(8) yet)
//!          `launch --no-prompt <app-tree>` (also runs jail -c)
//!
//! Future phases add capability prompts, portcullisd integration, etc.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use portcullis_jail::{build, BuildOpts};

fn usage() -> ! {
    eprintln!("\
usage:
    portcullis validate <atrium.toml>
        Parses and validates a manifest. Exits 0 on success.

    portcullis launch [--dry-run | --no-prompt] <app-id|app-tree>
        Reads <tree>/atrium.toml, builds a jail.conf section, and
        either prints it (--dry-run) or runs `jail -c` (--no-prompt).

        <app-id|app-tree> is resolved heuristically:
            - If it contains '/' or starts with '.', it's a path.
            - If it looks like an app id (lowercase + dots/hyphens),
              resolved to /var/lib/atrium/apps/<id>/.
            - Otherwise treated as a path.

        Phase 2/3a dev mode — no policy/prompt mediation, no
        per-instance overlay yet (single instance per app).
");
    std::process::exit(2);
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 { usage(); }
    match args[1].as_str() {
        "validate" => {
            if args.len() != 3 { usage(); }
            cmd_validate(&args[2])
        }
        "launch" => {
            if args.len() < 3 { usage(); }
            let mut dry_run = false;
            let mut no_prompt = false;
            let mut tree: Option<&str> = None;
            for a in &args[2..] {
                match a.as_str() {
                    "--dry-run"   => dry_run = true,
                    "--no-prompt" => no_prompt = true,
                    other if other.starts_with("--") => {
                        eprintln!("unknown flag: {other}");
                        usage();
                    }
                    other => tree = Some(other),
                }
            }
            let Some(t) = tree else { usage() };
            if !dry_run && !no_prompt {
                eprintln!("must pass --dry-run or --no-prompt (Phase 2 has no prompt mediation yet)");
                return ExitCode::from(2);
            }
            cmd_launch(t, dry_run)
        }
        "--help" | "-h" => usage(),
        other => {
            eprintln!("portcullis: unknown subcommand {other:?}");
            usage();
        }
    }
}

fn cmd_validate(path: &str) -> ExitCode {
    let text = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("portcullis validate: {path}: {e}");
            return ExitCode::from(1);
        }
    };
    match portcullis_toml::Manifest::from_str(&text) {
        Err(e) => {
            eprintln!("portcullis validate: {path}: parse error:");
            eprintln!("    {e}");
            ExitCode::from(1)
        }
        Ok(m) => {
            let report = portcullis_toml::validate(&m);
            for w in &report.warnings { eprintln!("warning: {w}"); }
            for e in &report.errors   { eprintln!("error:   {e}"); }
            if report.is_ok() {
                println!("{path}: OK ({} warning{})",
                    report.warnings.len(),
                    if report.warnings.len() == 1 { "" } else { "s" });
                ExitCode::SUCCESS
            } else {
                eprintln!("{path}: FAILED ({} error{}, {} warning{})",
                    report.errors.len(),
                    if report.errors.len() == 1 { "" } else { "s" },
                    report.warnings.len(),
                    if report.warnings.len() == 1 { "" } else { "s" });
                ExitCode::from(1)
            }
        }
    }
}

/// Default location where `tessera-import` lands installed apps.
/// `portcullis launch <app-id>` resolves the id by joining this prefix.
const APPS_DIR: &str = "/var/lib/atrium/apps";

/// Decide whether `arg` is an app-id (resolve via APPS_DIR) or a
/// filesystem path (use directly). Heuristic:
///   - contains '/' → path
///   - starts with '.' → path
///   - matches `^[a-z][a-z0-9.-]*$` → app-id
///   - anything else → path (let fs::read_to_string emit the error)
fn looks_like_app_id(arg: &str) -> bool {
    if arg.contains('/') || arg.starts_with('.') {
        return false;
    }
    let mut chars = arg.chars();
    let Some(first) = chars.next() else { return false };
    if !first.is_ascii_lowercase() { return false; }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '.' || c == '-')
}

fn resolve_app_tree(arg: &str) -> PathBuf {
    if looks_like_app_id(arg) {
        PathBuf::from(APPS_DIR).join(arg)
    } else {
        PathBuf::from(arg)
    }
}

fn cmd_launch(tree_arg: &str, dry_run: bool) -> ExitCode {
    let tree = resolve_app_tree(tree_arg);
    if !tree.exists() {
        if looks_like_app_id(tree_arg) {
            eprintln!("portcullis launch: app id {tree_arg:?} not found at {}",
                tree.display());
            eprintln!("    install with: tessera-import <src> {}", tree.display());
        } else {
            eprintln!("portcullis launch: {} does not exist", tree.display());
        }
        return ExitCode::from(1);
    }
    let manifest_path = tree.join("atrium.toml");
    let text = match fs::read_to_string(&manifest_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("portcullis launch: {}: {e}", manifest_path.display());
            return ExitCode::from(1);
        }
    };
    let manifest = match portcullis_toml::Manifest::from_str(&text) {
        Err(e) => {
            eprintln!("portcullis launch: parse error: {e}");
            return ExitCode::from(1);
        }
        Ok(m) => m,
    };
    let report = portcullis_toml::validate(&manifest);
    for w in &report.warnings { eprintln!("warning: {w}"); }
    if !report.is_ok() {
        for e in &report.errors { eprintln!("error:   {e}"); }
        return ExitCode::from(1);
    }

    /* Defaults appropriate for Phase 2 dev launches. */
    let opts = BuildOpts {
        root_path:    tree.clone(),    /* dev mode: rootfs IS the app tree */
        host_sockets: PathBuf::from("/atrium/sockets"),
        user_home:    std::env::var_os("HOME")
                          .map(PathBuf::from)
                          .unwrap_or_else(|| PathBuf::from("/")),
        user_name:    std::env::var("USER").unwrap_or_else(|_| "atrium".into()),
        devfs_ruleset: 99,             /* Phase 4 manages allocation */
    };

    let jc = match build(&manifest, &opts) {
        Err(e) => {
            eprintln!("portcullis launch: build error: {e}");
            return ExitCode::from(1);
        }
        Ok(jc) => jc,
    };

    if dry_run {
        println!("# jail.conf section ──────────────────────────────");
        print!("{}", jc.render_jail_conf());
        println!();
        println!("# devfs.rules ruleset ────────────────────────────");
        print!("{}", jc.render_devfs_rules());
        return ExitCode::SUCCESS;
    }

    /* --no-prompt: actually invoke jail -c. Requires root + FreeBSD. */
    let conf_path = std::env::temp_dir().join(format!("portcullis-{}.conf",
        std::process::id()));
    if let Err(e) = fs::write(&conf_path, jc.render_jail_conf()) {
        eprintln!("write {}: {e}", conf_path.display());
        return ExitCode::from(1);
    }
    let status = Command::new("jail")
        .arg("-c").arg("-f").arg(&conf_path).arg(&jc.name)
        .status();
    let _ = fs::remove_file(&conf_path);
    match status {
        Ok(s) if s.success() => {
            println!("portcullis: jail '{}' running", jc.name);
            ExitCode::SUCCESS
        }
        Ok(s) => {
            eprintln!("jail -c failed: {s}");
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("could not invoke jail: {e}");
            ExitCode::from(1)
        }
    }
}

#[allow(dead_code)]
fn assert_path_exists(p: &Path) -> bool { p.exists() }
