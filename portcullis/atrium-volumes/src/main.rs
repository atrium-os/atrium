//! `atrium-volumes` — entry point.
//!
//! Subcommands:
//!   atrium-volumes check-policy [--policy <path>]
//!     Parse + validate the policy file. Exits 0 on success,
//!     1 on parse / schema / validation error.
//!
//!   atrium-volumes serve [--policy <path>] [--socket <path>]
//!                        [--state <path>]
//!     Long-running daemon. Loads policy, binds the socket,
//!     runs the accept loop until killed.

use std::path::PathBuf;
use std::process::ExitCode;

use atrium_volumes::policy::Policy;
use log::{error, info};

const DEFAULT_POLICY: &str = "/etc/atrium/volumes.policy.toml";
const DEFAULT_SOCKET: &str = "/var/run/atrium/atrium-volumes.sock";
const DEFAULT_STATE:  &str = "/var/run/atrium/volumes.state.toml";

fn main() -> ExitCode {
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

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--policy" => match args.next() {
                Some(v) => policy_path = v.into(), None => return usage(),
            },
            "--socket" => match args.next() {
                Some(v) => socket_path = v.into(), None => return usage(),
            },
            "--state" => match args.next() {
                Some(v) => state_path = v.into(), None => return usage(),
            },
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
                println!("ok: schema_version={} backends={}",
                    p.schema_version, p.backends.len());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("policy error: {e}");
                ExitCode::FAILURE
            }
        },
        "serve" => match run_serve(&policy_path, &socket_path, &state_path) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                error!("atrium-volumes fatal: {e}");
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
) -> Result<(), atrium_volumes::VolumesError> {
    let policy = Policy::load(policy_path)?;
    info!("policy loaded from {} ({} backends)",
        policy_path.display(), policy.backends.len());

    if let Some(parent) = socket_path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let listener = atrium_volumes::server::bind(socket_path)?;
    info!("listening on {}", socket_path.display());

    atrium_volumes::server::serve(&listener, &policy, state_path)?;
    Ok(())
}

fn usage() -> ExitCode {
    eprintln!("\
usage:
  atrium-volumes check-policy [--policy <path>]
  atrium-volumes serve        [--policy <path>] [--socket <path>]
                              [--state  <path>]
");
    ExitCode::from(2)
}
