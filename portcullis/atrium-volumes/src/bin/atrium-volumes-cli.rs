//! atrium-volumes-cli — operator/debug client for atrium-volumes.
//!
//! Companion to atrium-portcullisd-jclient: simple length-prefixed
//! JSON over the daemon's Unix socket. Useful for inspecting state,
//! manually provisioning/destroying volumes, listing configured
//! backends.
//!
//! Usage:
//!     atrium-volumes-cli <socket> ping
//!     atrium-volumes-cli <socket> backends
//!     atrium-volumes-cli <socket> status [<jail>]
//!     atrium-volumes-cli <socket> provision <jail> <name> <kind> <mount_at> [backend]
//!     atrium-volumes-cli <socket> destroy <jail> <name> [really_yes]
//!
//! kind = persistent | tmpfs

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;

use atrium_volumes::protocol::{
    DestroyRequest, MAX_FRAME_BYTES, ProvisionRequest, Request, Response,
    StatusRequest, VolumeKind, VolumeSpec,
};

fn usage() -> ExitCode {
    eprintln!("\
usage:
  atrium-volumes-cli <socket> ping
  atrium-volumes-cli <socket> backends
  atrium-volumes-cli <socket> status [<jail>]
  atrium-volumes-cli <socket> provision <jail> <name> <kind> <mount_at> [backend]
  atrium-volumes-cli <socket> destroy <jail> <name> [really_yes]

kind = persistent | tmpfs
");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 { return usage(); }
    let socket_path = &args[0];
    let cmd         = &args[1];
    let rest        = &args[2..];

    let req = match cmd.as_str() {
        "ping"     => Request::Ping,
        "backends" => Request::ListBackends,
        "status"   => Request::Status(StatusRequest {
            jail_name: rest.first().cloned(),
        }),
        "provision" => {
            if rest.len() < 4 { return usage(); }
            let kind = match rest[2].as_str() {
                "persistent" => VolumeKind::Persistent,
                "tmpfs"      => VolumeKind::Tmpfs,
                other => { eprintln!("bad kind: {other:?}"); return usage(); }
            };
            Request::Provision(ProvisionRequest {
                jail_name: rest[0].clone(),
                volume: VolumeSpec {
                    name:      rest[1].clone(),
                    kind,
                    backend:   rest.get(4).cloned(),
                    mount_at:  rest[3].clone(),
                    mode:      0o755,
                    owner_uid: 0,
                    owner_gid: 0,
                    size_max:  None,
                },
            })
        }
        "destroy" => {
            if rest.len() < 2 { return usage(); }
            let really_yes = rest.get(2).map(|s| s == "really_yes").unwrap_or(false);
            Request::Destroy(DestroyRequest {
                jail_name:  rest[0].clone(),
                volume:     rest[1].clone(),
                really_yes,
            })
        }
        _ => return usage(),
    };

    let mut stream = match UnixStream::connect(socket_path) {
        Ok(s)  => s,
        Err(e) => { eprintln!("connect {socket_path}: {e}"); return ExitCode::FAILURE; }
    };

    let body = serde_json::to_vec(&req).expect("serialize");
    if body.len() as u32 > MAX_FRAME_BYTES {
        eprintln!("request too large ({} bytes)", body.len());
        return ExitCode::FAILURE;
    }
    if let Err(e) = stream.write_all(&(body.len() as u32).to_le_bytes()) {
        eprintln!("write len: {e}"); return ExitCode::FAILURE;
    }
    if let Err(e) = stream.write_all(&body) {
        eprintln!("write body: {e}"); return ExitCode::FAILURE;
    }

    let mut len_buf = [0u8; 4];
    if let Err(e) = stream.read_exact(&mut len_buf) {
        eprintln!("read len: {e}"); return ExitCode::FAILURE;
    }
    let len = u32::from_le_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        eprintln!("response too large ({len})"); return ExitCode::FAILURE;
    }
    let mut buf = vec![0u8; len as usize];
    if let Err(e) = stream.read_exact(&mut buf) {
        eprintln!("read body: {e}"); return ExitCode::FAILURE;
    }
    let resp: Response = match serde_json::from_slice(&buf) {
        Ok(r)  => r,
        Err(e) => { eprintln!("parse response: {e}"); return ExitCode::FAILURE; }
    };
    println!("{}", serde_json::to_string_pretty(&resp).unwrap_or_default());
    match resp {
        Response::Ok | Response::Provisioned { .. } | Response::AlreadyProvisioned { .. }
        | Response::Destroyed | Response::Status { .. } | Response::Backends { .. } => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}
