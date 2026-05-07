//! atrium-portcullisd-bootstrap — V1 system-service launcher.
//!
//! Reads `/etc/atrium/services.d/*.toml`, connects to jaild,
//! sends a `CreateJail` request for each enabled manifest in
//! lexicographic order. For exec'd services the procdesc fd
//! returned via SCM_RIGHTS is held in memory until the binary
//! exits — at which point the kernel closes them, the jails
//! die, and the children get reaped (no daemon role yet; that's
//! V2).
//!
//! V0/V1 limit: this binary is a one-shot launcher. It does NOT
//! supervise — when it exits, the procdesc fds are closed and
//! the launched services die. Use V2 (kqueue + restart policy)
//! for a real long-running launcher.
//!
//! Usage:
//!     atrium-portcullisd-bootstrap [--socket /var/run/atrium/jaild.sock]
//!                                  [--services-dir /etc/atrium/services.d]
//!                                  [--keep-running]

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use jaild::protocol::{Request, Response};
use log::{error, info, warn};
use portcullisd::jaild_client::Client;
use portcullisd::system_services::{self, LoadOutcome};

const DEFAULT_SOCKET: &str = "/var/run/atrium/jaild.sock";
const DEFAULT_SVCDIR: &str = "/etc/atrium/services.d";

fn usage() -> ExitCode {
    eprintln!("\
usage:
  atrium-portcullisd-bootstrap
        [--socket <path>]            (default: {DEFAULT_SOCKET})
        [--services-dir <path>]      (default: {DEFAULT_SVCDIR})
        [--keep-running]             hold procdesc fds open + sleep
                                     forever (smoke-test mode; V2
                                     replaces this with kqueue)
        [--dry-run]                  load + log manifests, don't
                                     send anything to jaild
");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    ).init();

    let mut socket_path  = PathBuf::from(DEFAULT_SOCKET);
    let mut services_dir = PathBuf::from(DEFAULT_SVCDIR);
    let mut keep_running = false;
    let mut dry_run      = false;

    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--socket"        => match args.next() {
                Some(v) => socket_path = v.into(), None => return usage(),
            },
            "--services-dir"  => match args.next() {
                Some(v) => services_dir = v.into(), None => return usage(),
            },
            "--keep-running"  => keep_running = true,
            "--dry-run"       => dry_run      = true,
            "--help" | "-h"   => return usage(),
            other => { eprintln!("unknown arg: {other}"); return ExitCode::from(2); }
        }
    }

    let LoadOutcome { manifests, errors } = match system_services::load_dir(&services_dir) {
        Ok(o)  => o,
        Err(e) => {
            error!("read services dir {}: {e}", services_dir.display());
            return ExitCode::FAILURE;
        }
    };
    for (path, err) in &errors {
        warn!("manifest {} skipped: {err}", path.display());
    }

    let enabled: Vec<_> = manifests.into_iter().filter(|m| m.enabled).collect();
    info!("loaded {} enabled service manifest(s) from {}",
        enabled.len(), services_dir.display());

    if dry_run {
        for m in &enabled {
            info!("dry-run: {}: name={} path={} mounts={} exec={}",
                m.name, m.name, m.path, m.mounts.len(),
                m.exec.is_some());
        }
        return ExitCode::SUCCESS;
    }

    let mut client = match Client::connect(&socket_path) {
        Ok(c) => c,
        Err(e) => {
            error!("connect jaild socket {}: {e}", socket_path.display());
            return ExitCode::FAILURE;
        }
    };

    /* For V1 we hold the procdesc fds in this Vec to keep the
     * exec'd services alive — closing the fd makes the kernel
     * close the procdesc, which makes the persist=0 jail go away.
     * V2 will hand these to a kqueue loop. */
    let mut held_fds: Vec<i32> = Vec::new();
    let mut launch_failures = 0;

    for m in &enabled {
        let req = Request::CreateJail(m.to_create_request());
        match client.send(&req) {
            Ok((Response::JailCreated(r), fd)) => {
                info!("launched {}: jid={} pid={} procdesc_attached={}",
                    m.name, r.jid, r.pid, r.procdesc_attached);
                if let Some(fd) = fd { held_fds.push(fd); }
            }
            Ok((Response::PolicyDenied { rule, detail }, _)) => {
                error!("policy denied {}: {rule}: {detail}", m.name);
                launch_failures += 1;
            }
            Ok((Response::SyscallFailed { name, errno, msg }, _)) => {
                error!("syscall {} failed for {}: errno={errno} {msg}",
                    name, m.name);
                launch_failures += 1;
            }
            Ok((other, _)) => {
                error!("unexpected response for {}: {other:?}", m.name);
                launch_failures += 1;
            }
            Err(e) => {
                error!("rpc to jaild for {}: {e}", m.name);
                launch_failures += 1;
            }
        }
    }

    if !keep_running {
        info!("bootstrap done; {} launched, {} failed", held_fds.len(), launch_failures);
        /* About to drop fds → kernel closes the procdescs → jails
         * with persist=0 die. That's the V1 limitation; the user
         * called us without --keep-running so they're aware. */
        return if launch_failures == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE };
    }

    info!("bootstrap done; {} launched, {} failed; sleeping forever (--keep-running)",
        held_fds.len(), launch_failures);
    loop { std::thread::sleep(Duration::from_secs(60)); }
}
