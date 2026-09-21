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
                    <app-id|app-tree>

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
                  then lives in the daemon and this process needs none.

        Exits 0 if the jailed process succeeded, 1 if it did not.
        NOT the child's own code: jail(8) collapses every nonzero
        exec.start status to 1, so success and failure are
        distinguishable here and the exact code is not.");
    std::process::exit(2)
}

pub fn cmd_exec(args: &[String]) -> ExitCode {
    let (mut instance, mut tmpfs_mb, mut target, mut via_daemon) =
        (None::<String>, 64u32, None::<String>, false);
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--instance" => { i += 1; instance = args.get(i).cloned(); }
            "--daemon" => via_daemon = true,
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
        return match crate::daemon::exec_instance(&target, instance.as_deref(), tmpfs_mb) {
            Ok(Some(true)) => ExitCode::SUCCESS,
            Ok(Some(false)) => ExitCode::from(1),
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
        user_name: std::env::var("USER").unwrap_or_else(|_| "root".into()),
    };
    match portcullis_oneshot::run(&spec) {
        OneShot::Exit { ok: true } => ExitCode::SUCCESS,
        OneShot::Exit { ok: false } => ExitCode::from(1),
        OneShot::Refused(why) => { eprintln!("portcullis: REFUSED — {why}"); ExitCode::from(1) }
        OneShot::Failed(why) => { eprintln!("portcullis: {why}"); ExitCode::from(1) }
    }
}
