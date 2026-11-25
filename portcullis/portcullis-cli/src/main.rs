//! portcullis — CLI front-end.
//!
//! Phase 1: `validate <atrium.toml>`
//! `launch <app-id>` asks portcullisd, which has jaild make the jail;
//! `launch --dry-run <app-tree>` shows the jail an app's manifest builds.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use portcullis_jail::{build, jail_name_from_app_id, BuildOpts};

mod daemon;
mod piped;

fn usage() -> ! {
    eprintln!("\
usage:
    portcullis validate <atrium.toml>
        Parses and validates a manifest. Exits 0 on success.

    portcullis trust <app-id|app-tree>
        Runs the launch trust gate on an app tree as a launch that must be
        signed would (an app from a third-party catalog: its publisher's
        keys there). Prints whose key signed it; exits 1 if refused.

    portcullis launch [--dry-run | --no-prompt] [--persona NAME] <app-id|app-tree>
        Asks portcullisd to launch the installed app <app-id>.

        --persona NAME runs the app's instance in that persona (one of
        the person's personas, personas.md): its own jail, uid and data.

        Default mode: consults the per-user policy file. If the
        manifest asks for capabilities the user hasn't granted,
        prompts on the controlling tty:
            Allow [o]nce    — this launch only; nothing persisted
            Allow [a]lways  — persist a grant; future launches skip
            [d]eny          — refuse this launch
        Non-tty contexts (scripts) get the old refusal + hint.

        --dry-run    Show the jail <app-tree>'s manifest builds and
                     exit; no mounts, no jail, no policy check.
        --no-prompt  Bypass the policy check (dev mode). All
                     manifest-declared capabilities are granted
                     for this launch only — nothing persisted.

        <app-id|app-tree> is resolved heuristically:
            - If it contains '/' or starts with '.', it's a path.
            - If it looks like an app id (lowercase + dots/hyphens),
              resolved to /var/lib/atrium/apps/<id>/.
            - Otherwise treated as a path.

    portcullis exec [--instance <tag>] [--tmpfs-size <n>] <app-id|app-tree>
        Run the app's entry in a ONE-SHOT jail whose stdio is this
        process's own, so a parent that spawned portcullis with pipes
        talks to the jailed process directly. Each --instance gets its
        own jail name and root, so many may run at once from one app;
        the writable layer is tmpfs and is discarded on exit.
        Signatures are REQUIRED on this path. Exits with the worker's
        own code (jaild reaps it). See `portcullis exec` for --user.

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

    portcullis personas [app-id]
        Your personas, Personal first; with an app, the one it last ran
        in (marked *) or that it runs once for every persona.

    portcullis workspace [list]
    portcullis workspace new <name> [--toolchain c|shell] [--network outbound|registries|none] [--uses a,b]
    portcullis workspace set <name> [--network outbound|registries|none] [--hosts a,b] [--uses a,b | --no-uses] [--object-cache on|off]
    portcullis workspace stop <name>
    portcullis workspace duplicate <name> <new-name>
    portcullis workspace run <name> [folder]
    portcullis workspace debug <name> [folder]    run it under lldb-server; prints the address to connect to
    portcullis workspace cores <name>             move its dev builds' cores into work/.atrium/cores
    portcullis workspace exec <name> [--cwd <folder>] -- <command...>
    portcullis workspace pkg <name> install|delete|upgrade|update|info|search|autoremove|clean [args]
        Your workspaces (homes.md §5). Their terminals open from Forum.

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
        "trust" => {
            if args.len() != 3 { usage(); }
            cmd_trust(&args[2])
        }
        "launch" => {
            if args.len() < 3 { usage(); }
            let mut dry_run = false;
            let mut no_prompt = false;
            let mut tree: Option<&str> = None;
            let mut persona: Option<&str> = None;
            let mut it = args[2..].iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--persona"   => match it.next() {
                        Some(p) => persona = Some(p.as_str()),
                        None => usage(),
                    },
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
            cmd_launch(t, dry_run, no_prompt, persona)
        }
        "exec" => {
            /* A one-shot jail wired to this process's pipes — the worker-pool
             * shape, as distinct from `launch`'s application shape. */
            return piped::cmd_exec(&args[2..]);
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
        "workspace" => cmd_workspace(&args[2..]),
        "personas" => {
            if args.len() > 3 { usage(); }
            cmd_personas(args.get(2).map(String::as_str))
        }
        "--help" | "-h" => usage(),
        other => {
            eprintln!("portcullis: unknown subcommand {other:?}");
            usage();
        }
    }
}

fn cmd_trust(arg: &str) -> ExitCode {
    let tree = resolve_app_tree(arg);
    let text = match fs::read_to_string(tree.join("atrium.toml")) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("portcullis trust: {}: {e}", tree.join("atrium.toml").display());
            return ExitCode::from(1);
        }
    };
    match portcullis_trust::Trust::load().verify_publisher(&tree, &text, portcullis_trust::Demand::Required) {
        Ok(Some(id)) => {
            println!("{}: signed by {}", tree.display(), id.iter().map(|b| format!("{b:02x}")).collect::<String>());
            ExitCode::SUCCESS
        }
        Ok(None) => {
            println!("{}: unsigned, allowed", tree.display());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("portcullis trust: REFUSED: {e}");
            ExitCode::from(1)
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

fn cmd_launch(tree_arg: &str, dry_run: bool, no_prompt: bool, persona: Option<&str>) -> ExitCode {
    /* A persona instance (personas.md §3) is the daemon's alone: the in-process
     * fallback below has no registry or home places for it. */
    if persona.is_some() && (dry_run || !looks_like_app_id(tree_arg)) {
        eprintln!("portcullis launch: --persona needs an installed app id and portcullisd");
        return ExitCode::from(2);
    }
    /* Daemon-first short-circuit for app-id launches: when the arg
     * is an app-id (not an explicit path) and we're not doing a
     * dry-run, just forward to portcullisd. The daemon runs on the
     * host where /var/lib/atrium/apps/<id>/ is reachable; the CLI
     * may be running inside a session jail where it isn't. So
     * don't try to read the manifest locally — let the daemon do
     * the work. (Local manifest read remains for --dry-run and
     * for explicit-path launches below.) */
    if !dry_run && looks_like_app_id(tree_arg) {
        match daemon::launch(tree_arg, no_prompt, persona) {
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

    /* Only portcullisd launches (through jaild): an app tree given by path
     * is installed first, as a dev bundle or a package, and launched by id. */
    if !dry_run {
        eprintln!("portcullis launch: {tree_arg} is a path; install it and launch it by its id");
        return ExitCode::from(2);
    }
    /* --dry-run: read the manifest here and show the jail it builds. */
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
        devfs_ruleset: portcullis_jail::APP_DEVFS_RULESET,
        instance: None,                /* one jail per app on this path */
        persist: true,
        /* ★ A dry run must not create the root-only machine secret on
         * whatever machine shows it: a placeholder, labelled. */
        host_identity: portcullis_identity::derive(&[0u8; 32], "dry-run placeholder — not this machine's identity"),
        /* Dev launches run unfiltered; portcullisd's launch path applies the
         * app's system call filter (jail-sysfil.md §5.2). */
        sysfil: None,
    };

    let jc = match build(&manifest, &opts) {
        Err(e) => {
            eprintln!("portcullis launch: build error: {e}");
            return ExitCode::from(1);
        }
        Ok(jc) => jc,
    };

    println!("# the jail portcullisd would ask jaild for ──────────────");
    println!("{jc:#?}");
    ExitCode::SUCCESS
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
        /* Not an error: the app declares instances = "single" and one is
         * already up, so the state the caller asked for already holds. It used
         * to reach jail(8) and come back as a raw "already exists" plus exit 1,
         * which a script (or a dock) could not tell from a broken app. */
        daemon::LaunchReply::AlreadyRunning { jid, uid } => {
            println!("portcullis launch: {tree_arg} is already running (jid {jid}, uid {uid})");
            ExitCode::SUCCESS
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

// ── policy oracle (daemon-first, file-fallback) ──────────────────

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
    // The same decision a launch makes — user grant, then installer grant.
    let system = portcullis_policy::Policy::load_system(std::path::Path::new(portcullis_policy::SYSTEM_PATH));
    let d = portcullis_policy::decide(id, &manifest.capabilities, &current_hash, &policy, system.as_ref());
    if let Some(why) = &d.system_unusable { eprintln!("system policy unusable: {why}") }
    let delta = d.delta;

    if delta.is_empty() {
        println!("{id}: all requested capabilities already granted ({:?} policy)",
                 d.by.expect("an empty delta names its source"));
        return ExitCode::SUCCESS;
    }
    println!("{id} wants:");
    for line in delta.describe().into_iter().chain(manifest.opens_lines()).chain(manifest.links_line()) {
        println!("  - {line}");
    }
    /* Exit 1 so scripts can detect "needs-prompt" without parsing. */
    ExitCode::from(1)
}

fn policy_grant(args: &[String]) -> ExitCode {
    if args.len() == 2 && args[0] == "--system" { return policy_grant_system(&args[1]) }
    if args.len() != 1 {
        eprintln!("usage: portcullis policy grant [--system] <app-id>");
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

/// The trusted-installer grant (portcullis.md §7): pins the CURRENT
/// manifest, for every user, in the root-owned system policy.
///
/// ★ Verified by reading it back through `load_system` — the same refusal
/// rules a launch applies. A grant written somewhere the loader will not
/// trust (not root-owned, group/other-writable) would be a grant that
/// silently does nothing; it is reported as a failure instead.
fn policy_grant_system(id: &str) -> ExitCode {
    let (text, manifest) = match load_app_manifest(id) {
        Ok(t) => t,
        Err(e) => { eprintln!("{e}"); return ExitCode::from(1); }
    };
    let path = std::path::Path::new(portcullis_policy::SYSTEM_PATH);
    let mut policy = match portcullis_policy::Policy::load_system(path) {
        Ok(p) => p,
        Err(e) => { eprintln!("{}: {e}", path.display()); return ExitCode::from(1); }
    };
    policy.grants.insert(id.to_string(), portcullis_policy::Grant {
        manifest_hash: portcullis_policy::hash_manifest(text.as_bytes()),
        granted_at:    portcullis_policy::now_iso8601(),
        capabilities:  manifest.capabilities.clone(),
    });
    if let Err(e) = policy.save_system(path) {
        eprintln!("save {}: {e} (the system policy is root's to write)", path.display());
        return ExitCode::from(1);
    }
    match portcullis_policy::Policy::load_system(path) {
        Ok(p) if p.grants.contains_key(id) => {
            println!("granted (system, this manifest only) all capabilities to {id} → {}", path.display());
            ExitCode::SUCCESS
        }
        Ok(_) => { eprintln!("{}: grant did not read back", path.display()); ExitCode::from(1) }
        Err(e) => { eprintln!("written, but NOT EFFECTIVE: {e}"); ExitCode::from(1) }
    }
}

fn policy_revoke(args: &[String]) -> ExitCode {
    if args.len() == 2 && args[0] == "--system" {
        let path = std::path::Path::new(portcullis_policy::SYSTEM_PATH);
        let mut policy = match portcullis_policy::Policy::load_system(path) {
            Ok(p) => p,
            Err(e) => { eprintln!("{}: {e}", path.display()); return ExitCode::from(1); }
        };
        if policy.grants.remove(&args[1]).is_none() {
            eprintln!("no system grant to revoke for {}", args[1]);
            return ExitCode::from(1);
        }
        if let Err(e) = policy.save_system(path) { eprintln!("save {}: {e}", path.display()); return ExitCode::from(1) }
        println!("revoked system grant for {}", args[1]);
        return ExitCode::SUCCESS;
    }
    if args.len() != 1 {
        eprintln!("usage: portcullis policy revoke [--system] <app-id>");
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

    /* The overlay is the app's own Tessera VOLUME (portcullis.md §4.2), not a
     * directory, whenever one could be built. `rm -rf` on a mounted volume
     * empties its contents and then fails to remove the mount point, so the
     * state is destroyed but the volume stays mounted and the backing image
     * stays on disk — the worst of both answers. Go through the volume
     * lifecycle instead; it degrades to a plain remove_dir_all when the
     * overlay really is just a directory. */
    // Every person's overlay of the app, and the machine-wide one (homes.md §3).
    let _ = &overlay_dir;
    for pl in portcullis_overlay::all_places(app_id) {
        if !pl.dir.exists() && !pl.img.exists() {
            continue;
        }
        if !keep_overlay {
            if let Err(e) = portcullis_overlay::destroy(&pl) {
                eprintln!("portcullis remove: {e}");
                return ExitCode::from(1);
            }
            println!("removed {} and its backing volume", pl.dir.display());
        } else {
            /* Unmount it, but keep the image: a kept overlay whose volume stays
             * mounted would hold an md(4) device open against an app that is no
             * longer installed. The next launch mounts it again. */
            if let Err(e) = portcullis_overlay::teardown(&pl) {
                eprintln!("portcullis remove: {e}");
                return ExitCode::from(1);
            }
            println!("kept {} (--keep-overlay)", pl.dir.display());
        }
    }

    ExitCode::SUCCESS
}

fn cmd_workspace(args: &[String]) -> ExitCode {
    use portcullis_ipc::Request;
    let flag = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).cloned();
    let uses = if args.iter().any(|a| a == "--no-uses") {
        Some(Vec::new())
    } else {
        flag("--uses").map(|u| u.split(',').filter(|s| !s.is_empty()).map(String::from).collect::<Vec<_>>())
    };
    let name = args.get(1).cloned().unwrap_or_default();
    if args.first().map(String::as_str) == Some("exec") {
        // exec <ws> [--cwd <dir>] -- <command...>: output to stdout.
        let Some(dd) = args.iter().position(|a| a == "--") else { usage() };
        let argv: Vec<String> = args[dd + 1..].to_vec();
        return match daemon::exec(name, argv, flag("--cwd").unwrap_or_default()) {
            Ok(Some(())) => ExitCode::SUCCESS,
            Ok(None) => {
                eprintln!("portcullis workspace: portcullisd is not running");
                ExitCode::from(1)
            }
            Err(e) => {
                eprintln!("portcullis workspace exec: {e}");
                ExitCode::from(1)
            }
        };
    }
    if args.first().map(String::as_str) == Some("pkg") {
        // pkg <ws> <subcommand> [args]: as the workspace's root, output to stdout.
        return match daemon::pkg(name, args[2.min(args.len())..].to_vec()) {
            Ok(Some(())) => ExitCode::SUCCESS,
            Ok(None) => {
                eprintln!("portcullis workspace: portcullisd is not running");
                ExitCode::from(1)
            }
            Err(e) => {
                eprintln!("portcullis workspace pkg: {e}");
                ExitCode::from(1)
            }
        };
    }
    if args.first().map(String::as_str) == Some("debug") {
        return match daemon::debug(name, args.get(2).cloned().unwrap_or_default()) {
            Ok(Some((id, addr, program))) => {
                println!("{id}\t{addr}\t{program}");
                eprintln!("in the workspace: lldb {program} -o 'gdb-remote {addr}'");
                ExitCode::SUCCESS
            }
            Ok(None) => {
                eprintln!("portcullis workspace: portcullisd is not running");
                ExitCode::from(1)
            }
            Err(e) => {
                eprintln!("portcullis workspace debug: {e}");
                ExitCode::from(1)
            }
        };
    }
    if args.first().map(String::as_str) == Some("cores") {
        return match daemon::cores(name) {
            Ok(Some(files)) => {
                for f in &files {
                    println!("{f}");
                }
                if files.is_empty() {
                    eprintln!("no cores");
                }
                ExitCode::SUCCESS
            }
            Ok(None) => {
                eprintln!("portcullis workspace: portcullisd is not running");
                ExitCode::from(1)
            }
            Err(e) => {
                eprintln!("portcullis workspace cores: {e}");
                ExitCode::from(1)
            }
        };
    }
    if args.first().map(String::as_str) == Some("run") {
        return match daemon::run(name, args.get(2).cloned().unwrap_or_default()) {
            Ok(Some((id, caps))) => {
                println!("{id}\t{}", caps.join(","));
                ExitCode::SUCCESS
            }
            Ok(None) => {
                eprintln!("portcullis workspace: portcullisd is not running");
                ExitCode::from(1)
            }
            Err(e) => {
                eprintln!("portcullis workspace run: {e}");
                ExitCode::from(1)
            }
        };
    }
    let req = match args.first().map(String::as_str).unwrap_or("list") {
        "list" => Request::Workspaces,
        "new" => Request::WorkspaceNew {
            name, toolchain: flag("--toolchain").unwrap_or_else(|| "c".into()),
            network: flag("--network"), uses: uses.unwrap_or_default(),
        },
        "set" => Request::WorkspaceSet { name, network: flag("--network"), uses, object_cache: flag("--object-cache").map(|v| v == "on"),
                                         hosts: flag("--hosts").map(|v| v.split(',').map(String::from).collect()) },
        "stop" => Request::WorkspaceStop { name },
        "duplicate" => Request::WorkspaceDuplicate { name, new_name: args.get(2).cloned().unwrap_or_default() },
        other => {
            eprintln!("portcullis workspace: unknown {other:?}");
            usage();
        }
    };
    match daemon::workspaces(req) {
        Ok(Some((list, _))) => {
            for w in list {
                println!("{}\t{}\t{}\t{}\tuses={}\t{}\t{}\thosts={}{}", w.name, w.toolchain, w.persona, w.network,
                         w.uses.join(","), if w.running { "running" } else { "stopped" }, if w.object_cache { "objects=on" } else { "objects=off" },
                         w.hosts.join(","), if w.hosts_default { " (default)" } else { "" });
            }
            ExitCode::SUCCESS
        }
        Ok(None) => {
            eprintln!("portcullis workspace: portcullisd is not running");
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("portcullis workspace: {e}");
            ExitCode::from(1)
        }
    }
}

fn cmd_personas(app: Option<&str>) -> ExitCode {
    match daemon::personas(app) {
        Ok(Some((list, last, spans))) => {
            for p in &list {
                let mark = if last.as_deref() == Some(p.name.as_str()) { "*" } else { " " };
                println!("{mark} {:<32} {}", p.name, p.colour.as_deref().unwrap_or("-"));
            }
            if spans {
                println!("{} runs once for every persona", app.unwrap_or("?"));
            }
            ExitCode::SUCCESS
        }
        Ok(None) => {
            eprintln!("portcullis: portcullisd is not running");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("portcullis: personas: {e}");
            ExitCode::FAILURE
        }
    }
}
