//! `atrium-jaild` — entry point.
//!
//! Subcommands:
//!   atrium-jaild check-policy [--policy <path>]
//!     Parse + validate the policy file. Exits 0 on success, 1 on
//!     any parse or schema error. Used by package post-install
//!     hooks and by the rc(8) script before bringing the daemon up.
//!
//!   atrium-jaild serve [--policy <path>] [--socket <path>] [--dry-run]
//!     The actual daemon. Loads policy, binds the socket, runs
//!     the accept loop until killed.

use std::path::PathBuf;
use std::process::ExitCode;

use jaild_policy::Policy;
use log::{error, info};

const DEFAULT_POLICY: &str = "/etc/atrium/jaild.policy.toml";
const DEFAULT_SOCKET: &str = "/var/run/atrium/jaild.sock";
const DEFAULT_STATE:  &str = "/var/run/atrium/jaild.state.toml";

fn main() -> ExitCode {
    /* env_logger reads RUST_LOG; default to info for jaild. The
     * smallest-TCB carve-out limits us here — env_logger is in
     * the allowed dep set; tracing-subscriber is not. */
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    ).init();

    let mut args = std::env::args().skip(1);
    let sub = match args.next() {
        Some(s) => s,
        None    => return usage(),
    };

    let mut policy_path = PathBuf::from(DEFAULT_POLICY);
    let mut socket_path = PathBuf::from(DEFAULT_SOCKET);
    let mut state_path  = PathBuf::from(DEFAULT_STATE);
    let mut dry_run     = false;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--policy" => {
                let Some(p) = args.next() else { return usage(); };
                policy_path = PathBuf::from(p);
            }
            "--socket" => {
                let Some(p) = args.next() else { return usage(); };
                socket_path = PathBuf::from(p);
            }
            "--state" => {
                let Some(p) = args.next() else { return usage(); };
                state_path = PathBuf::from(p);
            }
            "--dry-run" => { dry_run = true; }
            "--help" | "-h" => return usage(),
            other => {
                eprintln!("unknown arg: {other}");
                return ExitCode::from(2);
            }
        }
    }

    match sub.as_str() {
        "check-policy" => match Policy::load(&policy_path) {
            Ok(p) => {
                println!(
                    "ok: schema_version={} services={} attested_drivers={}",
                    p.schema_version,
                    p.services.len(),
                    p.gpu_drivers.attested.len(),
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("policy error: {e}");
                ExitCode::FAILURE
            }
        },
        "serve" => match run_serve(&policy_path, &socket_path, &state_path, dry_run) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                error!("jaild fatal: {e}");
                ExitCode::FAILURE
            }
        },
        _ => usage(),
    }
}

fn run_serve(
    policy_path: &std::path::Path,
    socket_path: &std::path::Path,
    state_path:  &std::path::Path,
    dry_run:     bool,
) -> Result<(), jaild::JaildError> {
    let policy = Policy::load(policy_path)?;
    info!(
        "jaild: policy loaded from {} (services={})",
        policy_path.display(),
        policy.services.len()
    );

    if let Some(parent) = socket_path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let listener = jaild::server::bind(socket_path)?;
    info!("jaild: listening on {}", socket_path.display());

    jaild::server::serve(&listener, &policy, dry_run, state_path)?;
    Ok(())
}

fn usage() -> ExitCode {
    eprintln!(
        "usage:
  atrium-jaild check-policy [--policy <path>]
  atrium-jaild serve        [--policy <path>] [--socket <path>]
                            [--state  <path>] [--dry-run]
"
    );
    ExitCode::from(2)
}
