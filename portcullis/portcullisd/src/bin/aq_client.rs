//! atrium-portcullisd-aq — aqueduct client for portcullisd.
//!
//! Companion to `atrium-portcullisd-attach`, but goes through the
//! daemon at `/var/run/atrium/portcullisd.sock` instead of
//! contacting jaild directly. This is the path an in-jail service
//! would take once jaild bind-mounts the daemon's socket into
//! its jail (V1).
//!
//! Capability mediation lives at the daemon — this client passes
//! requests through unmodified and prints whatever reply the
//! daemon returns.
//!
//! Usage:
//!     atrium-portcullisd-aq attach <jail> <source> <dest> <kind> [--socket <path>]
//!     atrium-portcullisd-aq detach <jail> <dest> [force] [--socket <path>]
//!
//! kind = ro_nullfs | rw_nullfs | tmpfs

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use aqueduct::{classes, envelope::flag, Connection};
use portcullis_protocol::{
    AttachMountReq, DetachMountReq, MountKind, MountReply,
    OP_ATTACH_MOUNT, OP_DETACH_MOUNT, OP_MOUNT_REPLY,
};

/// In-jail path. Bootstrap nullfs-mounts the daemon's per-
/// capability directory at `/atrium/sockets/portcullisd/`, so
/// the socket inside the jail is at this path.
const DEFAULT_SOCKET: &str = "/atrium/sockets/portcullisd/portcullisd.sock";

fn usage() -> ExitCode {
    eprintln!("\
usage:
  atrium-portcullisd-aq attach <jail> <source> <dest> <kind> [--socket <path>]
  atrium-portcullisd-aq detach <jail> <dest> [force] [--socket <path>]

kind = ro_nullfs | rw_nullfs | tmpfs

The daemon at <path> (default {DEFAULT_SOCKET}) runs the manifest-side
capability check + forwards to jaild on success.
");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if raw.is_empty() { return usage(); }
    let cmd = raw[0].clone();

    let mut socket_path = PathBuf::from(DEFAULT_SOCKET);
    let mut positional: Vec<String> = Vec::new();
    let mut i = 1;
    while i < raw.len() {
        match raw[i].as_str() {
            "--socket" => {
                let Some(v) = raw.get(i + 1) else { return usage(); };
                socket_path = v.into();
                i += 2;
            }
            other if other.starts_with("--") => {
                eprintln!("unknown arg: {other}");
                return usage();
            }
            _ => { positional.push(raw[i].clone()); i += 1; }
        }
    }

    let mut conn = match Connection::connect(&socket_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("connect daemon socket {}: {e}", socket_path.display());
            return ExitCode::FAILURE;
        }
    };

    let (op, payload) = match cmd.as_str() {
        "attach" => {
            if positional.len() < 4 {
                eprintln!("attach needs <jail> <source> <dest> <kind>");
                return usage();
            }
            let kind = match positional[3].as_str() {
                "ro_nullfs" => MountKind::RoNullfs,
                "rw_nullfs" => MountKind::RwNullfs,
                "tmpfs"     => MountKind::Tmpfs,
                other       => { eprintln!("bad kind: {other:?}"); return usage(); }
            };
            let req = AttachMountReq {
                jail_name:  positional[0].clone(),
                source:     positional[1].clone(),
                dest:       positional[2].clone(),
                mount_kind: kind,
            };
            (OP_ATTACH_MOUNT, serde_json::to_vec(&req).expect("serialize"))
        }
        "detach" => {
            if positional.len() < 2 {
                eprintln!("detach needs <jail> <dest>");
                return usage();
            }
            let force = positional.get(2).map(|s| s == "force").unwrap_or(false);
            let req = DetachMountReq {
                jail_name: positional[0].clone(),
                dest:      positional[1].clone(),
                force,
            };
            (OP_DETACH_MOUNT, serde_json::to_vec(&req).expect("serialize"))
        }
        _ => return usage(),
    };

    if let Err(e) = conn.send_message(
        classes::CLASS_PORTCULLIS, op, flag::RESPONSE_EXPECTED, &payload,
    ) {
        eprintln!("send: {e}");
        return ExitCode::FAILURE;
    }

    let m = match conn.recv_message_or_timeout(Duration::from_secs(10)) {
        Ok(Some(m)) => m,
        Ok(None)    => { eprintln!("timeout waiting for reply"); return ExitCode::FAILURE; }
        Err(e)      => { eprintln!("recv: {e}"); return ExitCode::FAILURE; }
    };
    if m.opcode_class != classes::CLASS_PORTCULLIS || m.op != OP_MOUNT_REPLY {
        eprintln!("unexpected reply: class={} op=0x{:04x}", m.opcode_class, m.op);
        return ExitCode::FAILURE;
    }
    let reply: MountReply = match serde_json::from_slice(&m.payload) {
        Ok(r) => r,
        Err(e) => { eprintln!("decode reply: {e}"); return ExitCode::FAILURE; }
    };
    println!("{}", serde_json::to_string_pretty(&reply).unwrap_or_default());
    match reply {
        MountReply::Ok => ExitCode::SUCCESS,
        _ => ExitCode::FAILURE,
    }
}
