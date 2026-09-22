//! `portcullis exec` — argument parsing for a one-shot jail.
//!
//! ★ The OPERATION lives in `portcullis-oneshot`, so the daemon runs the same
//! code rather than its own copy. This file is the command-line face of it and
//! nothing else; when `--daemon` is passed, even the jail work moves to
//! `portcullisd` and this process only forwards its descriptors.

use portcullis_oneshot::{OneShot, Spec};
use std::process::ExitCode;

pub fn usage() -> ! {
    eprintln!("\
usage:
    portcullis exec [--instance <tag>] [--tmpfs-size <n>] [--daemon]
                    [--user <name>] <app-id|app-tree>

        Run the app's entry in a ONE-SHOT jail whose stdin/stdout/stderr are
        this process's own — so a parent that spawned portcullis with pipes
        talks to the jailed process directly.

        Each --instance gets its own jail name and root, so many may run
        concurrently from one app. The writable layer is tmpfs and is
        discarded on exit; nothing persists between runs.

        Signatures are REQUIRED on this path regardless of
        /etc/atrium/trust.toml, because a worker pool launches continuously.

        --daemon  Ask portcullisd to create the jail and hand it these
                  descriptors, instead of creating it here. The privilege
                  then lives in the daemon and this process needs none. The
                  worker runs as THIS process's user; a root caller is refused.

        --user <name>  Without --daemon (which needs root): the unprivileged
                  user the worker runs as. Required, because a worker never
                  runs as root inside its jail and the run-as user is never
                  taken from the environment.

        --memory <MiB>            a STATIC per-jail memoryuse cap via rctl.
                                  Off by default: memoryuse is RSS, RSS caps
                                  can only KILL, and memfed already budgets
                                  jails dynamically (never below current RSS,
                                  so it freezes rather than kills). Use this
                                  only where the federation cannot see the
                                  jail.
        --require-memory-limit    refuse to run UNCAPPED. Needs a machine
                                  booted with kern.racct.enable=1; RACCT is a
                                  loader tunable, so a machine without it
                                  cannot be capped until it reboots.

        Exits with the jailed process's own code (jaild reaps it), 128 if it
        died on a signal, and 1 if it never ran (refused or failed).");
    std::process::exit(2)
}

pub fn cmd_exec(args: &[String]) -> ExitCode {
    let (mut instance, mut tmpfs_mb, mut target, mut via_daemon) =
        (None::<String>, 64u32, None::<String>, false);
    let mut memory_mb: Option<u64> = None;
    let mut require_memory_limit = false;
    let mut run_as: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--instance" => { i += 1; instance = args.get(i).cloned(); }
            "--daemon" => via_daemon = true,
            "--user" => { i += 1; run_as = args.get(i).cloned(); if run_as.is_none() { usage() } }
            "--require-memory-limit" => require_memory_limit = true,
            "--memory" => {
                i += 1;
                memory_mb = match args.get(i).and_then(|s| s.parse().ok()) {
                    Some(n) => Some(n), None => usage(),
                };
            }
            "--tmpfs-size" => {
                i += 1;
                tmpfs_mb = match args.get(i).and_then(|s| s.parse().ok()) {
                    Some(n) => n, None => usage(),
                };
            }
            s if s.starts_with("--") => usage(),
            s => target = Some(s.to_string()),
        }
        i += 1;
    }
    let Some(target) = target else { usage() };

    if via_daemon {
        if run_as.is_some() {
            // The daemon runs the worker as the CONNECTING user (peer
            // credentials); a flag cannot choose someone else.
            eprintln!("portcullis: --user does not apply to --daemon: the worker runs as you");
            return ExitCode::from(2);
        }
        return match crate::daemon::exec_instance(&target, instance.as_deref(), tmpfs_mb) {
            Ok(Some(Some(c))) => ExitCode::from(c.clamp(0, 255) as u8),
            Ok(Some(None)) => ExitCode::from(128),
            // ★ REFUSED, not silently done here instead. `--daemon` is a
            // request to put the privilege in the daemon; falling back to
            // creating the jail in this process would grant exactly what the
            // caller asked to avoid, and would do it without saying so.
            Ok(None) => {
                eprintln!("portcullis: portcullisd is not running; \
                           --daemon will not fall back to creating the jail here");
                ExitCode::from(1)
            }
            Err(e) => { eprintln!("portcullis: {e}"); ExitCode::from(1) }
        };
    }

    let spec = Spec {
        target,
        instance,
        tmpfs_mb,
        memory_mb,
        require_memory_limit,
        // ★★ Never from the environment. This path needs root, and `$USER`
        // is whatever the caller's shell left behind — `su -m` keeps root's,
        // so it silently named root (and a sudo'd shell could name anyone).
        // An explicit --user, checked against passwd in portcullis_oneshot.
        user_name: match run_as {
            Some(u) => u,
            None => {
                eprintln!("portcullis: exec without --daemon needs --user <name>: the \
                           unprivileged user the worker runs as (never root)");
                return ExitCode::from(2);
            }
        },
    };
    match portcullis_oneshot::run(&spec) {
        // ★ The worker's own exit code now (jaild reaps it), not jail(8)'s 0/1.
        OneShot::Exit { code: Some(c), .. } => ExitCode::from(c.clamp(0, 255) as u8),
        OneShot::Exit { ok, code: None } => ExitCode::from(if ok { 0 } else { 128 }),
        OneShot::Refused(why) => { eprintln!("portcullis: REFUSED — {why}"); ExitCode::from(1) }
        OneShot::Failed(why) => { eprintln!("portcullis: {why}"); ExitCode::from(1) }
    }
}
