//! atrium-portcullisd-daemon — long-running aqueduct service.
//!
//! Listens on `/var/run/atrium/portcullisd.sock` (Unix domain
//! socket, length-prefixed binary aqueduct envelope). Per
//! `docs/spec/storage.md` §6.2 + the aqueduct spec, this is the
//! single jaild client; in-jail services never talk to jaild
//! directly. They request runtime AttachMount / DetachMount via
//! aqueduct messages on `CLASS_PORTCULLIS`.
//!
//! ## Capability mediation
//!
//! For each request the daemon:
//!
//!   1. Looks up the requested jail in `/etc/atrium/services.d/`
//!      manifests. (V0: services-dir is operator-configured;
//!      richer authn — peer credentials cross-checked against
//!      jail name, e.g., uid → owning jail — is V1 once we add
//!      a per-jail uid mapping table.)
//!   2. Runs `system_services::check_attach_mount` /
//!      `check_detach_mount` against the manifest's
//!      `[capabilities]` block. Refuses with
//!      `MountReply::CapabilityDenied` on miss.
//!   3. On success, forwards the request to jaild. jaild's policy
//!      file is the *outer* allow-list; the manifest is the inner
//!      per-service grant. Both must pass.
//!
//! ## Threading
//!
//! Single-threaded blocking accept loop, same convention as jaild
//! and atrium-volumes (smallest-TCB carve-out per LANGUAGE-POLICY).
//! For low-rate operator-mediated mounts that's fine. The single
//! jaild connection is held for the daemon's lifetime — opening a
//! new one per request would deadlock against jaild's
//! single-threaded accept-loop.

use std::io;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use aqueduct::{classes, envelope::flag, Connection};
use jaild::protocol::{
    AttachMountRequest as JaildAttach, DetachMountRequest as JaildDetach,
    MountKind as JaildKind, Request as JaildReq, Response as JaildResp,
};
use log::{error, info, warn};
use portcullis_protocol::{
    AttachMountReq, DetachMountReq, MountKind as ProtoKind, MountReply,
    OP_ATTACH_MOUNT, OP_DETACH_MOUNT, OP_MOUNT_REPLY,
};
use portcullisd::jaild_client::Client;
use portcullisd::system_services::{
    self, CapabilityCheck, ServiceManifest,
};

const DEFAULT_SOCKET:        &str = "/var/run/atrium/portcullisd.sock";
const DEFAULT_JAILD_SOCKET:  &str = "/var/run/atrium/jaild.sock";
const DEFAULT_SVCDIR:        &str = "/etc/atrium/services.d";

fn usage() -> ExitCode {
    eprintln!("\
usage:
  atrium-portcullisd-daemon
        [--socket <path>]            (default: {DEFAULT_SOCKET})
        [--jaild-socket <path>]      (default: {DEFAULT_JAILD_SOCKET})
        [--services-dir <path>]      (default: {DEFAULT_SVCDIR})
");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    ).init();

    let mut socket_path = PathBuf::from(DEFAULT_SOCKET);
    let mut jaild_path  = PathBuf::from(DEFAULT_JAILD_SOCKET);
    let mut svcdir      = PathBuf::from(DEFAULT_SVCDIR);

    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--socket"        => { socket_path = raw.get(i + 1).map(PathBuf::from)
                                     .unwrap_or_else(|| { let _ = usage(); std::process::exit(2); });
                                   i += 2; }
            "--jaild-socket"  => { jaild_path = raw.get(i + 1).map(PathBuf::from)
                                     .unwrap_or_else(|| { let _ = usage(); std::process::exit(2); });
                                   i += 2; }
            "--services-dir" => { svcdir = raw.get(i + 1).map(PathBuf::from)
                                     .unwrap_or_else(|| { let _ = usage(); std::process::exit(2); });
                                   i += 2; }
            "--help" | "-h"  => return usage(),
            other => { eprintln!("unknown arg: {other}"); return usage(); }
        }
    }

    /* Pre-open jaild connection. We re-use this single connection
     * for the daemon's lifetime; jaild is single-threaded accept-
     * one-at-a-time. */
    let mut jaild = match Client::connect(&jaild_path) {
        Ok(c) => c,
        Err(e) => {
            error!("connect jaild socket {}: {e}", jaild_path.display());
            return ExitCode::FAILURE;
        }
    };
    info!("portcullisd-daemon: connected to jaild at {}", jaild_path.display());

    /* Cleanup stale socket from a previous run. */
    let _ = std::fs::remove_file(&socket_path);
    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            error!("bind {}: {e}", socket_path.display());
            return ExitCode::FAILURE;
        }
    };
    /* Mode 0600 — only owner connects. Per-jail visibility is
     * achieved by jaild bind-mounting a copy of the socket into
     * each authorized jail's chroot (V1; for now the daemon
     * accepts from anyone with the same uid as itself). */
    if let Err(e) = std::fs::set_permissions(&socket_path,
        std::os::unix::fs::PermissionsExt::from_mode(0o600))
    { warn!("chmod {}: {e}", socket_path.display()); }
    info!("portcullisd-daemon: listening on {}", socket_path.display());

    for s in listener.incoming() {
        let s = match s {
            Ok(s) => s,
            Err(e) => { warn!("accept: {e}"); continue; }
        };
        if let Err(e) = handle_client(s, &mut jaild, &svcdir) {
            warn!("client handler: {e}");
        }
    }
    ExitCode::SUCCESS
}

fn handle_client(
    s:       UnixStream,
    jaild:   &mut Client,
    svcdir:  &Path,
) -> io::Result<()> {
    let peer_uid = peer_uid(&s);
    info!("portcullisd-daemon: accepted (peer uid={peer_uid:?})");
    let mut conn = Connection::wrap(s)?;

    /* Re-load manifests per connection so a manifest edited
     * after daemon start takes effect on the next request. (Cost
     * is one filesystem walk per connection, fine for V0
     * operator-rate traffic.) */
    let outcome = match system_services::load_dir(svcdir) {
        Ok(o)  => o,
        Err(e) => {
            warn!("load services dir {}: {e}", svcdir.display());
            send_reply(&mut conn, MountReply::Error {
                detail: format!("server load services dir: {e}"),
            })?;
            return Ok(());
        }
    };

    loop {
        let m = match conn.recv_message() {
            Ok(m)  => m,
            Err(e) => {
                /* Connection closed or I/O error. Exit cleanly. */
                let _ = e;
                return Ok(());
            }
        };
        if m.opcode_class != classes::CLASS_PORTCULLIS {
            warn!("portcullisd-daemon: ignoring non-portcullis class {}",
                m.opcode_class);
            continue;
        }
        let reply = match m.op {
            OP_ATTACH_MOUNT => handle_attach(&m.payload, &outcome.manifests, jaild),
            OP_DETACH_MOUNT => handle_detach(&m.payload, &outcome.manifests, jaild),
            other => MountReply::Error {
                detail: format!("unknown opcode 0x{other:04x} on CLASS_PORTCULLIS"),
            },
        };
        send_reply(&mut conn, reply)?;
    }
}

fn handle_attach(
    payload:    &[u8],
    manifests:  &[ServiceManifest],
    jaild:      &mut Client,
) -> MountReply {
    let req: AttachMountReq = match serde_json::from_slice(payload) {
        Ok(r) => r,
        Err(e) => return MountReply::Error {
            detail: format!("decode AttachMountReq: {e}"),
        },
    };

    let manifest = match manifests.iter().find(|m| m.name == req.jail_name) {
        Some(m) => m,
        None => return MountReply::CapabilityDenied {
            rule:   "manifest.not_found".into(),
            detail: format!("no manifest for jail {:?}", req.jail_name),
        },
    };
    match system_services::check_attach_mount(manifest, &req.source) {
        CapabilityCheck::Allowed => {}
        CapabilityCheck::Denied { rule, detail } => {
            return MountReply::CapabilityDenied { rule: rule.into(), detail };
        }
    }

    /* Capability gate passed — forward to jaild. */
    let jaild_req = JaildReq::AttachMount(JaildAttach {
        jail_name:  req.jail_name.clone(),
        source:     req.source.clone(),
        dest:       req.dest.clone(),
        mount_kind: match req.mount_kind {
            ProtoKind::RoNullfs => JaildKind::RoNullfs,
            ProtoKind::RwNullfs => JaildKind::RwNullfs,
            ProtoKind::Tmpfs    => JaildKind::Tmpfs,
        },
    });
    forward_to_jaild(jaild, &jaild_req)
}

fn handle_detach(
    payload:    &[u8],
    manifests:  &[ServiceManifest],
    jaild:      &mut Client,
) -> MountReply {
    let req: DetachMountReq = match serde_json::from_slice(payload) {
        Ok(r) => r,
        Err(e) => return MountReply::Error {
            detail: format!("decode DetachMountReq: {e}"),
        },
    };

    let manifest = match manifests.iter().find(|m| m.name == req.jail_name) {
        Some(m) => m,
        None => return MountReply::CapabilityDenied {
            rule:   "manifest.not_found".into(),
            detail: format!("no manifest for jail {:?}", req.jail_name),
        },
    };
    match system_services::check_detach_mount(manifest) {
        CapabilityCheck::Allowed => {}
        CapabilityCheck::Denied { rule, detail } => {
            return MountReply::CapabilityDenied { rule: rule.into(), detail };
        }
    }

    let jaild_req = JaildReq::DetachMount(JaildDetach {
        jail_name: req.jail_name.clone(),
        dest:      req.dest.clone(),
        force:     req.force,
    });
    forward_to_jaild(jaild, &jaild_req)
}

fn forward_to_jaild(jaild: &mut Client, req: &JaildReq) -> MountReply {
    match jaild.send(req) {
        Ok((JaildResp::Ok, _)) => MountReply::Ok,
        Ok((JaildResp::PolicyDenied { rule, detail }, _)) =>
            MountReply::JaildPolicyDenied { rule, detail },
        Ok((JaildResp::SyscallFailed { name, errno, msg }, _)) =>
            MountReply::JaildSyscallFailed { name, errno, msg },
        Ok((other, _)) => MountReply::Error {
            detail: format!("unexpected jaild response: {other:?}"),
        },
        Err(e) => MountReply::Error {
            detail: format!("rpc to jaild: {e}"),
        },
    }
}

fn send_reply(conn: &mut Connection, reply: MountReply) -> io::Result<()> {
    let payload = serde_json::to_vec(&reply)
        .map_err(|e| io::Error::other(format!("serialize reply: {e}")))?;
    conn.send_message(
        classes::CLASS_PORTCULLIS,
        OP_MOUNT_REPLY,
        flag::IS_RESPONSE,
        &payload,
    )?;
    Ok(())
}

#[cfg(target_os = "freebsd")]
fn peer_uid(s: &UnixStream) -> Option<u32> {
    use std::os::unix::io::AsRawFd;
    #[allow(unsafe_code)]
    unsafe {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        if libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) == 0 {
            Some(uid)
        } else {
            None
        }
    }
}

#[cfg(not(target_os = "freebsd"))]
fn peer_uid(_s: &UnixStream) -> Option<u32> { None }
