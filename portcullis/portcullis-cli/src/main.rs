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

mod daemon;

fn usage() -> ! {
    eprintln!("\
usage:
    portcullis validate <atrium.toml>
        Parses and validates a manifest. Exits 0 on success.

    portcullis launch [--dry-run | --no-prompt] <app-id|app-tree>
        Reads <tree>/atrium.toml, builds a jail.conf section, and
        launches the jail.

        Default mode: consults the per-user policy file. If the
        manifest asks for capabilities the user hasn't granted,
        prompts on the controlling tty:
            Allow [o]nce    — this launch only; nothing persisted
            Allow [a]lways  — persist a grant; future launches skip
            [d]eny          — refuse this launch
        Non-tty contexts (scripts) get the old refusal + hint.

        --dry-run    Render jail.conf + devfs.rules and exit; no
                     mounts, no jail, no policy check.
        --no-prompt  Bypass the policy check (dev mode). All
                     manifest-declared capabilities are granted
                     for this launch only — nothing persisted.

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

    portcullis reinstall <app-id>
        Force the app's first-run setup to re-execute on the next
        launch by deleting the .atrium-firstrun-done sentinel from
        the overlay. Refuses if the jail is currently running.
        Use this after editing setup.command or after the app's
        own /usr/local state goes wrong.

    portcullis link-apps
        Walk /var/lib/atrium/apps/ and drop a wrapper script
        <id>/<id> into each app directory so users can run installed
        apps from inside their session jail with ./apps/<id>/<id>.
        Idempotent. Wrapper is a 3-line shell script that execs
        portcullis launch with the script basename.

    portcullis daemon ping
        Test connectivity to portcullisd. Exit 0 if it answers, 1 if
        the socket is missing or the daemon errors. Honours
        $PORTCULLIS_SOCKET as an override for the default
        /var/run/portcullisd.sock.

    portcullis daemon reload
        Tell portcullisd to re-read policy.toml from disk (use after
        editing the file by hand).

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
            cmd_launch(t, dry_run, no_prompt)
        }
        "status" => {
            if args.len() != 2 { usage(); }
            cmd_status()
        }
        "policy" => {
            if args.len() < 3 { usage(); }
            cmd_policy(&args[2..])
        }
        "daemon" => {
            if args.len() < 3 { usage(); }
            cmd_daemon(&args[2..])
        }
        "link-apps" => {
            if args.len() != 2 { usage(); }
            cmd_link_apps()
        }
        "reinstall" => {
            if args.len() != 3 { usage(); }
            cmd_reinstall(&args[2])
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

fn cmd_launch(tree_arg: &str, dry_run: bool, no_prompt: bool) -> ExitCode {
    /* Daemon-first short-circuit for app-id launches: when the arg
     * is an app-id (not an explicit path) and we're not doing a
     * dry-run, just forward to portcullisd. The daemon runs on the
     * host where /var/lib/atrium/apps/<id>/ is reachable; the CLI
     * may be running inside a session jail where it isn't. So
     * don't try to read the manifest locally — let the daemon do
     * the work. (Local manifest read remains for --dry-run and
     * for explicit-path launches below.) */
    if !dry_run && looks_like_app_id(tree_arg) {
        match daemon::launch(tree_arg, no_prompt) {
            Ok(Some(reply)) => {
                return handle_daemon_launch_reply(reply, tree_arg, no_prompt);
            }
            Ok(None) => {
                /* Daemon offline. For app-id launches we can't fall
                 * back to in-process — that requires root + visibility
                 * into /var/lib/atrium/apps/, which a session-jail CLI
                 * doesn't have. Refuse with a clear message. */
                eprintln!("portcullis launch: portcullisd not running ({} absent)",
                    portcullis_ipc::SOCKET_PATH);
                eprintln!("    start it on the host (or check the rc.d service)");
                return ExitCode::from(1);
            }
            Err(e) => {
                eprintln!("portcullisd launch: {e}");
                return ExitCode::from(1);
            }
        }
    }

    /* Path-based or --dry-run path: read manifest locally. Same as
     * before — used for development workflows that pass an app tree
     * by path, and for `--dry-run` rendering. */
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

    /* Default mode: consult the policy. Try portcullisd first;
     * fall back to direct file access if the daemon isn't running.
     * --no-prompt is the dev-mode bypass — skips the policy check
     * AND the interactive prompt entirely. */
    if !no_prompt {
        let current_hash = portcullis_policy::hash_manifest(text.as_bytes());
        loop {
            let approval = check_authorization(&manifest.app.id, &current_hash,
                                               &manifest.capabilities);
            let lines = match approval {
                Ok(None) => break,                 /* authorized */
                Ok(Some(delta_lines)) => delta_lines,
                Err(e) => {
                    eprintln!("portcullis launch: policy check failed: {e}");
                    return ExitCode::from(1);
                }
            };
            /* Phase 5 prompt — same shape as the daemon-forward
             * path but driven from the in-CLI delta. */
            if !stdin_is_tty() {
                eprintln!("portcullis launch: {} needs capabilities not yet granted:",
                    manifest.app.id);
                for line in &lines { eprintln!("    - {line}"); }
                eprintln!();
                eprintln!("    grant with: portcullis policy grant {}", manifest.app.id);
                eprintln!("    or bypass:  portcullis launch --no-prompt {tree_arg}");
                return ExitCode::from(1);
            }
            match prompt_for_approval(&manifest.app.id, &lines) {
                ApprovalDecision::AllowOnce => {
                    /* Skip remaining policy enforcement on this
                     * launch only (no persistence). */
                    break;
                }
                ApprovalDecision::AllowAlways => {
                    if let Err(e) = persist_grant_for(
                        &manifest.app.id, &text, &manifest.capabilities,
                    ) {
                        eprintln!("portcullis launch: persist grant: {e}");
                        return ExitCode::from(1);
                    }
                    /* Re-run the check; should be empty delta now. */
                    continue;
                }
                ApprovalDecision::Deny => {
                    eprintln!("portcullis launch: denied by user");
                    return ExitCode::from(1);
                }
            }
        }
    }

    /* Daemon-first: if portcullisd is running, it owns the
     * privileged side of the launch (mount + jail -c + teardown).
     * The CLI degrades to performing the launch in-process when
     * the daemon isn't running — useful for headless development
     * and as a fallback if the daemon dies. */
    if looks_like_app_id(tree_arg) {
        let mut bypass = no_prompt;
        loop {
            match daemon::launch(&manifest.app.id, bypass) {
                Ok(Some(daemon::LaunchReply::Exited { code })) => {
                    return match code {
                        Some(0) => ExitCode::SUCCESS,
                        Some(c) => ExitCode::from(c.min(255).max(1) as u8),
                        None    => ExitCode::from(1),  /* signal */
                    };
                }
                Ok(Some(daemon::LaunchReply::Failed { stage, message })) => {
                    eprintln!("portcullis launch [{stage}]: {message}");
                    return ExitCode::from(1);
                }
                Ok(Some(daemon::LaunchReply::NeedsApproval { delta })) => {
                    /* Phase 5: prompt on tty, refuse otherwise. */
                    if !stdin_is_tty() {
                        eprintln!("portcullis launch: {} needs capabilities not yet granted:",
                            manifest.app.id);
                        for line in &delta { eprintln!("    - {line}"); }
                        eprintln!();
                        eprintln!("    grant with: portcullis policy grant {}", manifest.app.id);
                        eprintln!("    or bypass:  portcullis launch --no-prompt {tree_arg}");
                        return ExitCode::from(1);
                    }
                    match prompt_for_approval(&manifest.app.id, &delta) {
                        ApprovalDecision::AllowOnce => {
                            bypass = true;
                            continue;   /* retry the launch with bypass */
                        }
                        ApprovalDecision::AllowAlways => {
                            if let Err(e) = persist_grant_for(
                                &manifest.app.id, &text, &manifest.capabilities,
                            ) {
                                eprintln!("portcullis launch: persist grant: {e}");
                                return ExitCode::from(1);
                            }
                            /* Loop: daemon will now Authorize. bypass
                             * stays at its original value (likely
                             * false) — we want the daemon to verify
                             * the persisted grant covers everything. */
                            continue;
                        }
                        ApprovalDecision::Deny => {
                            eprintln!("portcullis launch: denied by user");
                            return ExitCode::from(1);
                        }
                    }
                }
                Ok(None) => break,  /* daemon offline → local fallback below */
                Err(e) => {
                    eprintln!("portcullisd launch: {e}");
                    return ExitCode::from(1);
                }
            }
        }
    }
    /* If the user passed an explicit path (not an app id), the
     * daemon doesn't have a way to find it — daemon only knows
     * APPS_DIR-installed apps. Falls through to local launch. */

    /* Approved (or bypassed) and daemon offline: set up overlay
     * mounts, run jail, tear down — same as before this commit. */
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

/// Map a daemon LaunchReply to a CLI ExitCode. Used by the
/// short-circuit "app-id → daemon" path that doesn't have the
/// manifest text locally for persist_grant_for(). The full prompt
/// loop with AllowAlways persistence lives in the path-based
/// launch flow further down (it has manifest text in scope).
fn handle_daemon_launch_reply(
    reply: daemon::LaunchReply,
    tree_arg: &str,
    no_prompt: bool,
) -> ExitCode {
    match reply {
        daemon::LaunchReply::Exited { code } => match code {
            Some(0) => ExitCode::SUCCESS,
            Some(c) => ExitCode::from(c.min(255).max(1) as u8),
            None    => ExitCode::from(1),
        },
        daemon::LaunchReply::Failed { stage, message } => {
            eprintln!("portcullis launch [{stage}]: {message}");
            ExitCode::from(1)
        }
        daemon::LaunchReply::NeedsApproval { delta } => {
            /* Short-circuit path — no manifest text on the CLI side
             * (we may be running inside a session jail without
             * /var/lib/atrium/apps/ access), so we can't run the
             * full Allow-Always-persists-grant flow. Print the
             * delta + suggest the way out. A future improvement
             * would be a daemon RPC like `GrantFromManifest{app_id}`
             * that lets the in-jail CLI persist a grant via the
             * daemon's host-side manifest read. */
            if no_prompt {
                /* Shouldn't reach here — bypass would have prevented
                 * the policy gate from refusing. Defensive. */
                eprintln!("portcullis launch [policy]: NeedsApproval despite --no-prompt");
                return ExitCode::from(1);
            }
            eprintln!("portcullis launch: {tree_arg} needs capabilities not yet granted:");
            for line in &delta { eprintln!("    - {line}"); }
            eprintln!();
            eprintln!("    grant from a host shell: portcullis policy grant {tree_arg}");
            eprintln!("    or bypass:               portcullis launch --no-prompt {tree_arg}");
            ExitCode::from(1)
        }
    }
}

// ── Phase 5: interactive approval prompt ─────────────────────────

#[derive(Debug)]
enum ApprovalDecision {
    AllowOnce,    /* this launch only — bypass_policy=true on retry  */
    AllowAlways,  /* persist a grant + retry                          */
    Deny,         /* refuse the launch                                */
}

/// True when stdin AND stderr (where the prompt lives) are both tty's.
/// We require both because:
///   - if stderr isn't a tty, the prompt is invisible (logged somewhere
///     the user isn't looking),
///   - if stdin isn't a tty, there's nobody at the keyboard.
fn stdin_is_tty() -> bool {
    /* libc::isatty returns 1 for tty, 0 otherwise. */
    unsafe {
        extern "C" { fn isatty(fd: i32) -> i32; }
        isatty(0) == 1 && isatty(2) == 1
    }
}

/// Print the delta + the three-way prompt; loop on bad input.
/// Returns Deny on EOF (Ctrl-D).
fn prompt_for_approval(app_id: &str, delta: &[String]) -> ApprovalDecision {
    use std::io::{BufRead, Write};
    eprintln!();
    eprintln!("'{app_id}' wants to:");
    for line in delta {
        eprintln!("    • {line}");
    }
    eprintln!();
    let stdin = std::io::stdin();
    let mut buf = String::new();
    loop {
        eprint!("Allow? [o]nce, [a]lways, [d]eny: ");
        let _ = std::io::stderr().flush();
        buf.clear();
        match stdin.lock().read_line(&mut buf) {
            Ok(0) => return ApprovalDecision::Deny,   /* EOF */
            Ok(_) => {}
            Err(_) => return ApprovalDecision::Deny,
        }
        match buf.trim().to_ascii_lowercase().as_str() {
            "o" | "once"   => return ApprovalDecision::AllowOnce,
            "a" | "always" => return ApprovalDecision::AllowAlways,
            "d" | "deny" | "n" | "no" | "" => return ApprovalDecision::Deny,
            _ => eprintln!("    (please answer o, a, or d)"),
        }
    }
}

/// Persist a grant for ALL the manifest's requested capabilities.
/// Used by the "Allow always" branch so the user doesn't have to
/// re-prompt on every launch. Tries daemon first, falls back to
/// direct file write — same shape as `policy_grant`.
fn persist_grant_for(
    app_id:        &str,
    manifest_text: &str,
    caps:          &portcullis_toml::Capabilities,
) -> Result<(), String> {
    let manifest_hash = portcullis_policy::hash_manifest(manifest_text.as_bytes());
    match daemon::grant(app_id, &manifest_hash, caps) {
        Ok(Some(())) => return Ok(()),
        Ok(None) => { /* daemon offline, fall through */ }
        Err(e) => return Err(format!("portcullisd grant: {e}")),
    }
    let path = portcullis_policy::Policy::user_path(&current_user());
    let mut policy = portcullis_policy::Policy::load(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    policy.grants.insert(app_id.to_string(), portcullis_policy::Grant {
        manifest_hash,
        granted_at:    portcullis_policy::now_iso8601(),
        capabilities:  caps.clone(),
    });
    policy.save(&path).map_err(|e| format!("save {}: {e}", path.display()))
}

// ── policy oracle (daemon-first, file-fallback) ──────────────────

/// Returns `Ok(None)` if authorized, `Ok(Some(lines))` if approval
/// is needed (with human-readable delta lines), `Err(_)` on real
/// failure. Tries portcullisd over its socket first; on connection
/// failure falls back to reading policy.toml directly so the CLI
/// keeps working in headless / pre-daemon contexts.
fn check_authorization(
    app_id: &str,
    manifest_hash: &str,
    requested: &portcullis_toml::Capabilities,
) -> std::io::Result<Option<Vec<String>>> {
    match daemon::authorize(app_id, manifest_hash, requested)? {
        Some(daemon::AuthorizeOutcome::Authorized)            => return Ok(None),
        Some(daemon::AuthorizeOutcome::NeedsApproval(lines))  => return Ok(Some(lines)),
        None => { /* daemon not running, fall through to file path */ }
    }
    let policy_path = portcullis_policy::Policy::user_path(&current_user());
    let policy = portcullis_policy::Policy::load(&policy_path)?;
    let prior = policy.grants.get(app_id);
    let delta = portcullis_policy::compute_delta(
        requested,
        prior.map(|g| &g.capabilities),
        prior.map(|g| g.manifest_hash.as_str()),
        manifest_hash,
    );
    if delta.is_empty() {
        Ok(None)
    } else {
        Ok(Some(delta.describe()))
    }
}

// ── policy subcommands ───────────────────────────────────────────

/// Resolve the current user's name for the policy file path.
///
/// ★ From the REAL uid, not from $USER. portcullisd identifies a caller with
/// getpeereid(2), so a CLI that trusted the environment could read a different
/// user's policy than the one `grant`/`launch` act on — and it did: under
/// `su -m` (which preserves $USER) `policy grant` correctly wrote
/// /var/db/atrium/atrium-app-50001/policy.toml via the daemon, while
/// `policy show` read root's and reported "no grant for ...". That fails in the
/// worst direction: it says a grant did not take when it did.
///
/// Falls back to the environment, then to "atrium", only if the uid has no
/// passwd entry (headless/odd contexts) — never in preference to it.
fn current_user() -> String {
    portcullis_peer::current_username()
        .or_else(|| std::env::var("USER").ok())
        .or_else(|| std::env::var("LOGNAME").ok())
        .unwrap_or_else(|| "atrium".into())
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

    let manifest_hash = portcullis_policy::hash_manifest(text.as_bytes());

    /* Daemon-first: if portcullisd is running it's the canonical
     * writer, and a parallel direct-file write would race its
     * in-memory copy. */
    match daemon::grant(id, &manifest_hash, &manifest.capabilities) {
        Ok(Some(())) => {
            println!("granted all capabilities to {id} (via portcullisd)");
            return ExitCode::SUCCESS;
        }
        Ok(None) => { /* daemon offline, fall through */ }
        Err(e) => {
            eprintln!("portcullisd grant: {e}");
            return ExitCode::from(1);
        }
    }

    let path = portcullis_policy::Policy::user_path(&current_user());
    let mut policy = match portcullis_policy::Policy::load(&path) {
        Ok(p) => p,
        Err(e) => { eprintln!("{}: {e}", path.display()); return ExitCode::from(1); }
    };

    let grant = portcullis_policy::Grant {
        manifest_hash,
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

/// Force the app's first-run setup to re-execute by removing
/// the `.atrium-firstrun-done` sentinel. Refuses if the jail
/// is running so we don't yank the sentinel out from under a
/// half-completed launch.
fn cmd_reinstall(app_id: &str) -> ExitCode {
    if !looks_like_app_id(app_id) {
        eprintln!("portcullis reinstall: {app_id:?} is not a valid app id");
        return ExitCode::from(2);
    }
    let jname = portcullis_jail::jail_name_from_app_id(app_id);
    if jail_is_running(&jname) {
        eprintln!("portcullis reinstall: jail {jname:?} is currently running; \
                   stop it first");
        return ExitCode::from(1);
    }
    let sentinel = PathBuf::from(OVERLAYS_DIR).join(app_id).join(".atrium-firstrun-done");
    match fs::remove_file(&sentinel) {
        Ok(()) => {
            println!("removed {} (next launch will re-run setup)", sentinel.display());
            ExitCode::SUCCESS
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("portcullis reinstall: no sentinel at {} (already due to re-setup)",
                sentinel.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("portcullis reinstall: {}: {e}", sentinel.display());
            ExitCode::from(1)
        }
    }
}

/// Generate a per-app wrapper script at /var/lib/atrium/apps/<id>/<id>.
/// Inside the session jail this dir appears at /apps/<id>/, so users
/// type `./apps/<id>/<id>` (or `cd /apps/<id> && ./<id>`) to launch.
fn cmd_link_apps() -> ExitCode {
    use std::os::unix::fs::PermissionsExt;
    let apps = PathBuf::from(APPS_DIR);
    let entries = match fs::read_dir(&apps) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("portcullis link-apps: {}: {e}", apps.display());
            return ExitCode::from(1);
        }
    };
    let mut count = 0;
    for ent in entries.filter_map(|e| e.ok()) {
        let id = match ent.file_name().into_string() {
            Ok(s) => s,
            Err(_) => continue,
        };
        if !looks_like_app_id(&id) { continue; }
        let manifest = ent.path().join("atrium.toml");
        if !manifest.exists() { continue; }
        let wrapper = ent.path().join(&id);
        let script = format!(
            "#!/bin/sh\n# Atrium app launcher (auto-generated by `portcullis link-apps`).\nexec /usr/local/bin/portcullis launch \"$(basename \"$0\")\" \"$@\"\n"
        );
        if let Err(e) = fs::write(&wrapper, script) {
            eprintln!("portcullis link-apps: write {}: {e}", wrapper.display());
            return ExitCode::from(1);
        }
        if let Err(e) = fs::set_permissions(&wrapper,
                            fs::Permissions::from_mode(0o755)) {
            eprintln!("portcullis link-apps: chmod {}: {e}", wrapper.display());
            return ExitCode::from(1);
        }
        println!("linked {}", wrapper.display());
        count += 1;
    }
    println!("({count} app{} linked)", if count == 1 { "" } else { "s" });
    ExitCode::SUCCESS
}

fn cmd_daemon(args: &[String]) -> ExitCode {
    match args[0].as_str() {
        "ping" => match daemon::ping() {
            Ok(Some(())) => { println!("portcullisd: pong"); ExitCode::SUCCESS }
            Ok(None) => {
                eprintln!("portcullisd: not running ({} not present)",
                    portcullis_ipc::SOCKET_PATH);
                ExitCode::from(1)
            }
            Err(e) => { eprintln!("portcullisd: {e}"); ExitCode::from(1) }
        },
        "reload" => match daemon::reload() {
            Ok(Some(())) => { println!("portcullisd: reloaded"); ExitCode::SUCCESS }
            Ok(None) => {
                eprintln!("portcullisd: not running");
                ExitCode::from(1)
            }
            Err(e) => { eprintln!("portcullisd: {e}"); ExitCode::from(1) }
        },
        other => {
            eprintln!("portcullis daemon: unknown action {other:?}");
            usage();
        }
    }
}

fn policy_revoke(args: &[String]) -> ExitCode {
    if args.len() != 1 {
        eprintln!("usage: portcullis policy revoke <app-id>");
        return ExitCode::from(2);
    }
    let id = &args[0];

    match daemon::revoke(id) {
        Ok(Some(())) => {
            println!("revoked grant for {id} (via portcullisd)");
            return ExitCode::SUCCESS;
        }
        Ok(None) => { /* daemon offline, fall through */ }
        Err(e) => {
            eprintln!("portcullisd revoke: {e}");
            return ExitCode::from(1);
        }
    }

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
