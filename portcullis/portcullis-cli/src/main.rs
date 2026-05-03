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

use portcullis_jail::{build, jail_name_from_app_id, BuildOpts};

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

    portcullis status
        List installed apps and which jails are currently running.

    portcullis remove [--keep-overlay] <app-id>
        Uninstall an app: removes /var/lib/atrium/apps/<id>/ and (by
        default) /var/lib/atrium/overlays/<id>/. Refuses if the jail
        is currently running. Pass --keep-overlay to preserve user
        state across reinstall.

    portcullis policy show [<app-id>]
        Print the per-user policy file (or one app's grants).

    portcullis policy diff <app-id>
        Show what the installed app's manifest asks for that the
        policy hasn't granted (the prompt the user would see).

    portcullis policy grant <app-id>
        Grant ALL capabilities the app's manifest declares. Phase 4
        bootstrap: replaces the prompt UI until portcullisd lands.

    portcullis policy revoke <app-id>
        Drop the grant record for an app id (next launch re-prompts).

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
        "status" => {
            if args.len() != 2 { usage(); }
            cmd_status()
        }
        "policy" => {
            if args.len() < 3 { usage(); }
            cmd_policy(&args[2..])
        }
        "remove" => {
            let mut keep_overlay = false;
            let mut id: Option<&str> = None;
            for a in &args[2..] {
                match a.as_str() {
                    "--keep-overlay" => keep_overlay = true,
                    other if other.starts_with("--") => {
                        eprintln!("unknown flag: {other}");
                        usage();
                    }
                    other => id = Some(other),
                }
            }
            let Some(id) = id else { usage() };
            cmd_remove(id, keep_overlay)
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
const APPS_DIR:     &str = "/var/lib/atrium/apps";
/// Per-app persistent overlay directory. Single-instance for now;
/// multi-instance UUIDs deferred to a future iteration.
const OVERLAYS_DIR: &str = "/var/lib/atrium/overlays";
/// Per-app jail mount target — the unionfs mountpoint that becomes
/// jail.path. Recreated each launch; destroyed on jail teardown.
const JAILS_DIR:    &str = "/var/lib/atrium/jails";

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

    /* --no-prompt: set up overlay mounts, run jail, tear down. */
    let app_id = manifest.app.id.clone();
    let overlay_dir = PathBuf::from(OVERLAYS_DIR).join(&app_id);
    let jail_path   = PathBuf::from(JAILS_DIR).join(&app_id);

    /* Rebuild the JailConfig with the unionfs path as jail.path
     * (overrides BuildOpts.root_path which we set to tree above). */
    let opts2 = BuildOpts { root_path: jail_path.clone(), ..opts };
    let jc = match build(&manifest, &opts2) {
        Ok(j) => j,
        Err(e) => { eprintln!("build error: {e}"); return ExitCode::from(1); }
    };

    /* Pre-launch: ensure dirs exist, set up the union mount. */
    if let Err(e) = ensure_dir(&overlay_dir) { eprintln!("{e}"); return ExitCode::from(1); }
    if let Err(e) = ensure_dir(&jail_path)   { eprintln!("{e}"); return ExitCode::from(1); }

    /* Defensive: unmount any stale layers from a previous crash.
     * Use silent=true so the common case (nothing to unmount) is
     * quiet. */
    let _ = umount_silent(&jail_path);
    let _ = umount_silent(&jail_path);

    if let Err(e) = run("mount", &["-t", "nullfs", "-o", "ro",
                                     tree.to_str().unwrap(),
                                     jail_path.to_str().unwrap()]) {
        eprintln!("nullfs mount: {e}"); return ExitCode::from(1);
    }
    if let Err(e) = run("mount", &["-t", "unionfs",
                                     overlay_dir.to_str().unwrap(),
                                     jail_path.to_str().unwrap()]) {
        eprintln!("unionfs mount: {e}");
        let _ = umount(&jail_path);
        return ExitCode::from(1);
    }

    /* Write jail.conf, run jail -c. */
    let conf_path = std::env::temp_dir().join(format!("portcullis-{}.conf",
        std::process::id()));
    if let Err(e) = fs::write(&conf_path, jc.render_jail_conf()) {
        eprintln!("write {}: {e}", conf_path.display());
        teardown(&jail_path);
        return ExitCode::from(1);
    }
    let status = Command::new("jail")
        .arg("-c").arg("-f").arg(&conf_path).arg(&jc.name)
        .status();
    let _ = fs::remove_file(&conf_path);

    /* Teardown: jail -r (idempotent if already removed by exec.start
     * exit), then umount in reverse order. */
    let _ = Command::new("jail").arg("-r").arg(&jc.name).status();
    teardown(&jail_path);

    match status {
        Ok(s) if s.success() => {
            println!("portcullis: jail '{}' completed cleanly", jc.name);
            ExitCode::SUCCESS
        }
        Ok(s) => {
            eprintln!("jail -c exited {s}");
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("could not invoke jail: {e}");
            ExitCode::from(1)
        }
    }
}

fn ensure_dir(p: &Path) -> std::io::Result<()> {
    fs::create_dir_all(p)
}

fn umount(p: &Path) -> std::io::Result<()> {
    let st = Command::new("umount").arg(p).status()?;
    if !st.success() {
        return Err(std::io::Error::other(format!("umount {} failed: {st}", p.display())));
    }
    Ok(())
}

/// Like `umount`, but suppresses umount(8)'s stderr. Used for the
/// defensive pre-launch unmount where "not mounted" is the common case
/// and the warning is just noise.
fn umount_silent(p: &Path) -> std::io::Result<()> {
    let _ = Command::new("umount")
        .arg(p)
        .stderr(std::process::Stdio::null())
        .status()?;
    Ok(())
}

fn run(cmd: &str, args: &[&str]) -> std::io::Result<()> {
    let st = Command::new(cmd).args(args).status()?;
    if !st.success() {
        return Err(std::io::Error::other(format!("{cmd} {args:?} failed: {st}")));
    }
    Ok(())
}

/// Tear down a jail's union mount. Order matters: unionfs first
/// (upper layer), then nullfs (lower). Also unmount devfs if jail
/// still has it mounted at <jail-path>/dev.
fn teardown(jail_path: &Path) {
    let dev = jail_path.join("dev");
    let _ = umount(&dev);   /* devfs from mount.devfs in jail.conf */
    let _ = umount(jail_path);  /* unionfs */
    let _ = umount(jail_path);  /* nullfs */
}

// ── policy subcommands ───────────────────────────────────────────

/// Resolve the current user's name for the policy file path.
/// Falls back to "atrium" when neither USER nor LOGNAME is set
/// (e.g. headless cron contexts) — the file will then be at
/// /var/db/atrium/atrium/policy.toml.
fn current_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "atrium".into())
}

/// Read + parse an app's manifest given its id (uses APPS_DIR).
/// Returns (raw_text, parsed_manifest) since the policy delta needs
/// the raw bytes for hashing.
fn load_app_manifest(app_id: &str) -> Result<(String, portcullis_toml::Manifest), String> {
    if !looks_like_app_id(app_id) {
        return Err(format!("{app_id:?} is not a valid app id"));
    }
    let path = PathBuf::from(APPS_DIR).join(app_id).join("atrium.toml");
    let text = fs::read_to_string(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let m = portcullis_toml::Manifest::from_str(&text)
        .map_err(|e| format!("{}: parse error: {e}", path.display()))?;
    Ok((text, m))
}

fn cmd_policy(args: &[String]) -> ExitCode {
    let sub = args[0].as_str();
    let rest = &args[1..];
    match sub {
        "show"   => policy_show(rest),
        "diff"   => policy_diff(rest),
        "grant"  => policy_grant(rest),
        "revoke" => policy_revoke(rest),
        other => {
            eprintln!("portcullis policy: unknown action {other:?}");
            usage();
        }
    }
}

fn policy_show(args: &[String]) -> ExitCode {
    let path = portcullis_policy::Policy::user_path(&current_user());
    let policy = match portcullis_policy::Policy::load(&path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("portcullis policy show: {}: {e}", path.display());
            return ExitCode::from(1);
        }
    };

    if args.is_empty() {
        println!("# {}", path.display());
        if policy.grants.is_empty() {
            println!("(no grants yet)");
            return ExitCode::SUCCESS;
        }
        for (id, g) in &policy.grants {
            println!("\n[{id}]");
            println!("  granted_at = {}", g.granted_at);
            println!("  manifest_hash = {}", g.manifest_hash);
        }
        return ExitCode::SUCCESS;
    }

    let id = &args[0];
    match policy.grants.get(id) {
        None => {
            eprintln!("no grant for {id}");
            ExitCode::from(1)
        }
        Some(g) => {
            print!("{}", g.to_toml_string());
            ExitCode::SUCCESS
        }
    }
}

fn policy_diff(args: &[String]) -> ExitCode {
    if args.len() != 1 {
        eprintln!("usage: portcullis policy diff <app-id>");
        return ExitCode::from(2);
    }
    let id = &args[0];
    let (text, manifest) = match load_app_manifest(id) {
        Ok(t) => t,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    let path = portcullis_policy::Policy::user_path(&current_user());
    let policy = match portcullis_policy::Policy::load(&path) {
        Ok(p) => p,
        Err(e) => { eprintln!("{}: {e}", path.display()); return ExitCode::from(1); }
    };

    let current_hash = portcullis_policy::hash_manifest(text.as_bytes());
    let prior        = policy.grants.get(id);
    let delta = portcullis_policy::compute_delta(
        &manifest.capabilities,
        prior.map(|g| &g.capabilities),
        prior.map(|g| g.manifest_hash.as_str()),
        &current_hash,
    );

    if delta.is_empty() {
        println!("{id}: all requested capabilities already granted");
        return ExitCode::SUCCESS;
    }
    println!("{id} wants:");
    for line in delta.describe() {
        println!("  - {line}");
    }
    /* Exit 1 so scripts can detect "needs-prompt" without parsing. */
    ExitCode::from(1)
}

fn policy_grant(args: &[String]) -> ExitCode {
    if args.len() != 1 {
        eprintln!("usage: portcullis policy grant <app-id>");
        return ExitCode::from(2);
    }
    let id = &args[0];
    let (text, manifest) = match load_app_manifest(id) {
        Ok(t) => t,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };

    let path = portcullis_policy::Policy::user_path(&current_user());
    let mut policy = match portcullis_policy::Policy::load(&path) {
        Ok(p) => p,
        Err(e) => { eprintln!("{}: {e}", path.display()); return ExitCode::from(1); }
    };

    let grant = portcullis_policy::Grant {
        manifest_hash: portcullis_policy::hash_manifest(text.as_bytes()),
        granted_at:    portcullis_policy::now_iso8601(),
        capabilities:  manifest.capabilities.clone(),
    };
    policy.grants.insert(id.clone(), grant);

    if let Err(e) = policy.save(&path) {
        eprintln!("save {}: {e}", path.display());
        return ExitCode::from(1);
    }
    println!("granted all capabilities to {id} → {}", path.display());
    ExitCode::SUCCESS
}

fn policy_revoke(args: &[String]) -> ExitCode {
    if args.len() != 1 {
        eprintln!("usage: portcullis policy revoke <app-id>");
        return ExitCode::from(2);
    }
    let id = &args[0];
    let path = portcullis_policy::Policy::user_path(&current_user());
    let mut policy = match portcullis_policy::Policy::load(&path) {
        Ok(p) => p,
        Err(e) => { eprintln!("{}: {e}", path.display()); return ExitCode::from(1); }
    };
    if policy.grants.remove(id).is_none() {
        eprintln!("no grant to revoke for {id}");
        return ExitCode::from(1);
    }
    if let Err(e) = policy.save(&path) {
        eprintln!("save {}: {e}", path.display());
        return ExitCode::from(1);
    }
    println!("revoked grant for {id}");
    ExitCode::SUCCESS
}

// ── jail status helpers ──────────────────────────────────────────

/// True if a jail with this name is currently running.
/// Uses `jls -j <name>` (exit 0 = exists). Stderr suppressed because
/// "jail not found" is a normal expected condition, not an error.
fn jail_is_running(jail_name: &str) -> bool {
    Command::new("jls")
        .arg("-j").arg(jail_name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn cmd_status() -> ExitCode {
    let apps_dir = PathBuf::from(APPS_DIR);
    let entries = match fs::read_dir(&apps_dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("(no apps installed — {} does not exist)", APPS_DIR);
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("portcullis status: {}: {e}", apps_dir.display());
            return ExitCode::from(1);
        }
    };

    let mut ids: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    ids.sort();

    if ids.is_empty() {
        println!("(no apps installed)");
        return ExitCode::SUCCESS;
    }

    println!("{:<40} {:<8} {}", "APP ID", "STATE", "OVERLAY");
    for id in &ids {
        let jname = jail_name_from_app_id(id);
        let state = if jail_is_running(&jname) { "running" } else { "stopped" };
        let overlay = PathBuf::from(OVERLAYS_DIR).join(id);
        let overlay_state = if overlay.exists() {
            format!("{}", overlay.display())
        } else {
            "—".to_string()
        };
        println!("{:<40} {:<8} {}", id, state, overlay_state);
    }
    ExitCode::SUCCESS
}

fn cmd_remove(app_id: &str, keep_overlay: bool) -> ExitCode {
    /* Sanity: id must look like an app id, not a path. Avoid
     * `portcullis remove ../foo` style accidents. */
    if !looks_like_app_id(app_id) {
        eprintln!("portcullis remove: {app_id:?} is not a valid app id");
        return ExitCode::from(2);
    }

    let app_dir     = PathBuf::from(APPS_DIR).join(app_id);
    let overlay_dir = PathBuf::from(OVERLAYS_DIR).join(app_id);
    let jail_path   = PathBuf::from(JAILS_DIR).join(app_id);

    if !app_dir.exists() && !overlay_dir.exists() {
        eprintln!("portcullis remove: {app_id} not installed");
        return ExitCode::from(1);
    }

    let jname = jail_name_from_app_id(app_id);
    if jail_is_running(&jname) {
        eprintln!("portcullis remove: jail {jname:?} is currently running; \
                   stop it first (jail -r {jname})");
        return ExitCode::from(1);
    }

    /* Defensive: if a previous launch crashed, the union mount may
     * still be live even though no jail process is running. Tear it
     * down so rmdir doesn't fail with EBUSY. */
    if jail_path.exists() {
        let _ = umount_silent(&jail_path.join("dev"));
        let _ = umount_silent(&jail_path);
        let _ = umount_silent(&jail_path);
        let _ = fs::remove_dir(&jail_path);
    }

    if app_dir.exists() {
        if let Err(e) = fs::remove_dir_all(&app_dir) {
            eprintln!("portcullis remove: {}: {e}", app_dir.display());
            return ExitCode::from(1);
        }
        println!("removed {}", app_dir.display());
    }

    if !keep_overlay && overlay_dir.exists() {
        if let Err(e) = fs::remove_dir_all(&overlay_dir) {
            eprintln!("portcullis remove: {}: {e}", overlay_dir.display());
            return ExitCode::from(1);
        }
        println!("removed {}", overlay_dir.display());
    } else if keep_overlay && overlay_dir.exists() {
        println!("kept {} (--keep-overlay)", overlay_dir.display());
    }

    ExitCode::SUCCESS
}
