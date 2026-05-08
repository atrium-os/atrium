//! atrium-portcullisd-jclient — small CLI that exercises
//! portcullisd's `jaild_client` library against a running
//! `atrium-jaild`. Same usage as scratch/jail-smoke/jaild_client.py
//! but in Rust, so cross-language equivalence of the SCM_RIGHTS
//! recv path can be checked.
//!
//! Eventually deprecated when portcullisd-the-daemon uses
//! `jaild_client` directly. For Phase 4 v0 it's the smoke harness.
//!
//! Usage:
//!     atrium-portcullisd-jclient <socket> ping
//!     atrium-portcullisd-jclient <socket> create <name> <path> [<children_max>]
//!     atrium-portcullisd-jclient <socket> remove <jid>
//!     atrium-portcullisd-jclient <socket> exec <name> <path> <bin> [<arg>...]

use std::process::ExitCode;

use jaild::protocol::{
    AttachMountRequest, CreateJailRequest, DetachMountRequest, EnvPair,
    ExecSpec, MountKind, NetworkConfig, Request, Response,
};
use portcullisd::jaild_client::Client;

fn usage() -> ExitCode {
    eprintln!("\
usage:
  atrium-portcullisd-jclient <socket> ping
  atrium-portcullisd-jclient <socket> create <name> <path> [<children_max>]
  atrium-portcullisd-jclient <socket> remove <jid>
  atrium-portcullisd-jclient <socket> exec <name> <path> <bin> [<arg>...]
  atrium-portcullisd-jclient <socket> attach <jail> <source> <dest> <kind>
  atrium-portcullisd-jclient <socket> detach <jail> <dest> [force]

kind = ro_nullfs | rw_nullfs | tmpfs
");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 { return usage(); }
    let socket_path = &args[0];
    let cmd         = &args[1];
    let rest        = &args[2..];

    let mut client = match Client::connect(socket_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("connect {socket_path}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let req = match cmd.as_str() {
        "ping" => Request::Ping,
        "create" => {
            if rest.len() < 2 { return usage(); }
            Request::CreateJail(CreateJailRequest {
                name:          rest[0].clone(),
                path:          rest[1].clone(),
                children_max:  rest.get(2).and_then(|s| s.parse().ok()).unwrap_or(0),
                mounts:        vec![],
                devfs_ruleset: 0,
                network:       NetworkConfig::Disable,
                exec:          None,
            })
        }
        "remove" => {
            if rest.len() < 1 { return usage(); }
            // jid OR name; numeric → jid, else → name.
            if let Ok(jid) = rest[0].parse::<i32>() {
                Request::RemoveJail { jid: Some(jid), name: None }
            } else {
                Request::RemoveJail { jid: None, name: Some(rest[0].clone()) }
            }
        }
        "exec" => {
            if rest.len() < 3 { return usage(); }
            let name = rest[0].clone();
            let path = rest[1].clone();
            let bin  = rest[2].clone();
            let argv: Vec<String> = rest[2..].to_vec();
            Request::CreateJail(CreateJailRequest {
                name,
                path,
                children_max:  0,
                mounts:        vec![],
                devfs_ruleset: 0,
                network:       NetworkConfig::Disable,
                exec: Some(ExecSpec {
                    path: bin,
                    argv,
                    env:  vec![EnvPair {
                        key:   "PATH".into(),
                        value: "/bin:/usr/bin".into(),
                    }],
                    uid:  1001,
                    gid:  1001,
                }),
            })
        }
        "attach" => {
            if rest.len() < 4 { return usage(); }
            let kind = match rest[3].as_str() {
                "ro_nullfs" => MountKind::RoNullfs,
                "rw_nullfs" => MountKind::RwNullfs,
                "tmpfs"     => MountKind::Tmpfs,
                _ => { eprintln!("bad kind: {:?}", rest[3]); return usage(); }
            };
            Request::AttachMount(AttachMountRequest {
                jail_name:  rest[0].clone(),
                source:     rest[1].clone(),
                dest:       rest[2].clone(),
                mount_kind: kind,
            })
        }
        "detach" => {
            if rest.len() < 2 { return usage(); }
            let force = rest.get(2).map(|s| s == "force").unwrap_or(false);
            Request::DetachMount(DetachMountRequest {
                jail_name: rest[0].clone(),
                dest:      rest[1].clone(),
                force,
            })
        }
        _ => return usage(),
    };

    match client.send(&req) {
        Ok((resp, fd)) => {
            println!("{}", serde_json::to_string_pretty(&resp).unwrap_or_default());
            if let Some(fd) = fd {
                eprintln!("[procdesc fd received: {fd}]");
                /* Real client would EVFILT_PROCDESC + retain.
                 * Smoke client just closes immediately. */
                let _ = unsafe_close(fd);
            }
            match resp {
                Response::Ok | Response::JailCreated(_) => ExitCode::SUCCESS,
                _ => ExitCode::FAILURE,
            }
        }
        Err(e) => {
            eprintln!("send: {e}");
            ExitCode::FAILURE
        }
    }
}

/// One-line wrapper for `libc::close(fd)`. The crate is
/// `#![deny(unsafe_code)]`; this is the only call site that
/// needs it in a binary. (jaild_client itself contains all the
/// unsafe for the protocol.)
fn unsafe_close(fd: i32) -> std::io::Result<()> {
    #[allow(unsafe_code)]
    unsafe {
        if libc::close(fd) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}
