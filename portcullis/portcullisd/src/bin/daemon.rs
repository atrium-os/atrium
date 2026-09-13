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
//! One thread (smallest-TCB carve-out per LANGUAGE-POLICY), every client
//! connection multiplexed over kqueue by sockmux: one request per
//! connection per round, so an in-jail client that holds its connection
//! open — atrium-portcullisd-aq lingers after its attach — never holds up
//! another jail's request. It used to serve each connection to completion,
//! the loop that let the bootstrap's long-lived connection starve this
//! daemon's AttachMount inside jaild. A fresh jaild connection is opened per
//! forwarded request (see main).

use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use aqueduct::{classes, envelope::flag, Connection};
use jaild::protocol::{
    AttachMountRequest as JaildAttach, DetachMountRequest as JaildDetach,
    MountKind as JaildKind, Request as JaildReq, Response as JaildResp,
};
use log::{error, info, warn};
use sockmux::{Mux, Session};
use portcullis_protocol::{
    AttachMountReq, DetachMountReq, MountKind as ProtoKind, MountReply,
    OP_ATTACH_MOUNT, OP_DETACH_MOUNT, OP_MOUNT_REPLY,
};
use portcullisd::jaild_client::Client;
use portcullisd::system_services::{
    self, CapabilityCheck, ServiceManifest,
};

/// Per-capability socket path. The directory is what bootstrap
/// nullfs-mounts into authorized jails at `/atrium/sockets/
/// portcullisd/`, so the in-jail path is
/// `/atrium/sockets/portcullisd/portcullisd.sock`. Per
/// `docs/spec/aqueduct.md` §6.1, capability = mount; jails with
/// no `attach_mount = true` simply don't see the directory.
const DEFAULT_SOCKET:        &str = "/var/run/atrium/caps/portcullisd/portcullisd.sock";
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

    /* A fresh jaild connection per forwarded request — the
     * connect/send/recv cost is negligible at the rates this daemon
     * serves. This comment used to claim that avoided blocking on the
     * bootstrap's connection; it did not, because jaild then served one
     * connection at a time and the bootstrap never closes its own. jaild
     * now multiplexes (jaild/src/server.rs). */

    /* Ensure the per-capability socket directory exists; bootstrap
     * nullfs-mounts the *directory* into authorized jails at
     * `/atrium/sockets/portcullisd/`, so this layout has to be
     * laid out before the daemon binds. */
    if let Some(parent) = socket_path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            error!("create socket parent {}: {e}", parent.display());
            return ExitCode::FAILURE;
        }
    }

    /* Cleanup stale socket from a previous run. */
    let _ = std::fs::remove_file(&socket_path);
    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            error!("bind {}: {e}", socket_path.display());
            return ExitCode::FAILURE;
        }
    };
    /* Mode 0666 — capability mediation is the bind-mount, not
     * the file mode. Any uid that can see this socket has
     * already been authorized by jaild's mount policy +
     * bootstrap's [capabilities] derivation; restricting by uid
     * here would just block legitimate in-jail callers (which
     * run under the service's manifest-declared uid, not 0).
     * The daemon still capability-checks each request server-
     * side (via the manifest's [capabilities] block). */
    use std::os::unix::fs::PermissionsExt;
    if let Err(e) = std::fs::set_permissions(&socket_path,
        std::fs::Permissions::from_mode(0o666))
    { warn!("chmod {}: {e}", socket_path.display()); }
    info!("portcullisd-daemon: listening on {}", socket_path.display());

    let mut mux: Mux<AqClient> = match Mux::new(listener) {
        Ok(m) => m,
        Err(e) => {
            error!("kqueue setup: {e}");
            return ExitCode::FAILURE;
        }
    };
    let mut admit = |s: UnixStream| admit_client(s, &svcdir);
    loop {
        let ready = match mux.next_round(&mut admit) {
            Ok(r) => r,
            Err(e) => {
                error!("serve loop: {e}");
                return ExitCode::FAILURE;
            }
        };
        /* One request per connection per round. */
        for fd in ready {
            let Some(client) = mux.session_mut(fd) else { continue };
            if let Err(e) = serve_one(client, &jaild_path) {
                warn!("client handler: {e}");
                mux.close(fd);
            }
        }
    }
}

/// One in-jail (or operator) client: its aqueduct connection, the peer uid
/// recovered at accept, and the manifests as they were when it connected.
struct AqClient {
    conn:      Connection,
    peer:      Option<u32>,
    manifests: Vec<ServiceManifest>,
}

impl Session for AqClient {
    fn fd(&self) -> RawFd { self.conn.as_raw_fd() }
    fn set_nonblocking(&self, nb: bool) -> io::Result<()> { self.conn.set_nonblocking(nb) }
    fn fill(&mut self) -> io::Result<bool> { self.conn.fill_available() }
    fn ready(&self) -> bool { self.conn.has_buffered_message() }
}

fn admit_client(s: UnixStream, svcdir: &Path) -> Option<AqClient> {
    let peer = peer_uid(&s);
    info!("portcullisd-daemon: accepted (peer uid={peer:?})");
    let mut conn = match Connection::wrap(s) {
        Ok(c) => c,
        Err(e) => { warn!("wrap connection: {e}"); return None; }
    };

    /* Re-load manifests per connection so a manifest edited
     * after daemon start takes effect on the next connection. (Cost
     * is one filesystem walk per connection, fine for V0
     * operator-rate traffic.) */
    match system_services::load_dir(svcdir) {
        Ok(o)  => Some(AqClient { conn, peer, manifests: o.manifests }),
        Err(e) => {
            warn!("load services dir {}: {e}", svcdir.display());
            let _ = send_reply(&mut conn, MountReply::Error {
                detail: format!("server load services dir: {e}"),
            });
            None
        }
    }
}

/// Serve the next buffered request, if any. `Err` closes the connection.
fn serve_one(client: &mut AqClient, jaild_path: &Path) -> io::Result<()> {
    /* Buffered only: CLASS_CORE traffic is handled inside and may leave
     * nothing for us this round. */
    let Some(m) = client.conn.try_recv_message()? else { return Ok(()) };
    if m.opcode_class != classes::CLASS_PORTCULLIS {
        warn!("portcullisd-daemon: ignoring non-portcullis class {}",
            m.opcode_class);
        return Ok(());
    }
    let reply = match m.op {
        OP_ATTACH_MOUNT => handle_attach(&m.payload, &client.manifests, client.peer, jaild_path),
        OP_DETACH_MOUNT => handle_detach(&m.payload, &client.manifests, client.peer, jaild_path),
        other => MountReply::Error {
            detail: format!("unknown opcode 0x{other:04x} on CLASS_PORTCULLIS"),
        },
    };
    send_reply(&mut client.conn, reply)
}

/// Map a peer uid to the manifest whose exec runs under that uid.
/// Returns:
///   - Ok(Some(manifest)) — exactly one manifest matches
///   - Ok(None)           — peer uid is 0 (operator/root); caller may
///                          target any jail (skip the cross-check)
///   - Err(MountReply)    — no match, multiple matches, or peer uid
///                          unavailable; reply ready to send back
fn peer_to_manifest<'a>(
    peer:       Option<u32>,
    manifests:  &'a [ServiceManifest],
) -> Result<Option<&'a ServiceManifest>, MountReply> {
    match peer {
        Some(0)    => Ok(None),  // operator
        None       => Err(MountReply::CapabilityDenied {
            rule:   "peer.unknown_uid".into(),
            detail: "could not recover peer uid via getpeereid".into(),
        }),
        Some(uid)  => {
            let matches: Vec<&ServiceManifest> = manifests.iter()
                .filter(|m| m.exec.as_ref().map(|e| e.uid) == Some(uid))
                .collect();
            match matches.as_slice() {
                []    => Err(MountReply::CapabilityDenied {
                    rule:   "peer.no_matching_manifest".into(),
                    detail: format!(
                        "peer uid {uid} doesn't match any manifest's exec.uid"),
                }),
                [one] => Ok(Some(*one)),
                many  => Err(MountReply::CapabilityDenied {
                    rule:   "peer.ambiguous_uid".into(),
                    detail: format!(
                        "peer uid {uid} matches {} manifests ({:?}); cannot \
                         determine owning jail", many.len(),
                        many.iter().map(|m| &m.name).collect::<Vec<_>>()),
                }),
            }
        }
    }
}

/// Cross-check that the request's claimed jail_name matches the
/// manifest derived from the peer uid. Operator (uid 0) bypasses
/// this — same authority that runs the CLI tool.
fn enforce_caller_owns_jail(
    peer_manifest:    Option<&ServiceManifest>,
    requested_jail:   &str,
) -> Result<(), MountReply> {
    if let Some(m) = peer_manifest {
        if m.name != requested_jail {
            return Err(MountReply::CapabilityDenied {
                rule:   "peer.jail_mismatch".into(),
                detail: format!(
                    "peer is jailed as {:?} but requested operations on {:?}",
                    m.name, requested_jail),
            });
        }
    }
    Ok(())
}

/// Open a one-shot jaild connection, send `req`, return the reply
/// converted to MountReply. Connection is dropped (closed) at
/// function exit, freeing jaild for the next caller.
fn jaild_round_trip(jaild_path: &Path, req: &JaildReq) -> MountReply {
    let mut client = match Client::connect(jaild_path) {
        Ok(c)  => c,
        Err(e) => return MountReply::Error {
            detail: format!("connect jaild {}: {e}", jaild_path.display()),
        },
    };
    forward_to_jaild(&mut client, req)
}

fn handle_attach(
    payload:    &[u8],
    manifests:  &[ServiceManifest],
    peer:       Option<u32>,
    jaild_path: &Path,
) -> MountReply {
    let req: AttachMountReq = match serde_json::from_slice(payload) {
        Ok(r) => r,
        Err(e) => return MountReply::Error {
            detail: format!("decode AttachMountReq: {e}"),
        },
    };

    /* Defense in depth: confirm the caller's uid matches the
     * jail it claims to be (so a uid-1001 caller in jail-A can't
     * forge `jail_name = jail-B` to ride jail-B's broader cap
     * allow-list). Bypassed for uid 0 (operator). */
    let peer_manifest = match peer_to_manifest(peer, manifests) {
        Ok(opt) => opt,
        Err(reply) => return reply,
    };
    if let Err(reply) = enforce_caller_owns_jail(peer_manifest, &req.jail_name) {
        return reply;
    }

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
    jaild_round_trip(jaild_path, &jaild_req)
}

fn handle_detach(
    payload:    &[u8],
    manifests:  &[ServiceManifest],
    peer:       Option<u32>,
    jaild_path: &Path,
) -> MountReply {
    let req: DetachMountReq = match serde_json::from_slice(payload) {
        Ok(r) => r,
        Err(e) => return MountReply::Error {
            detail: format!("decode DetachMountReq: {e}"),
        },
    };

    let peer_manifest = match peer_to_manifest(peer, manifests) {
        Ok(opt) => opt,
        Err(reply) => return reply,
    };
    if let Err(reply) = enforce_caller_owns_jail(peer_manifest, &req.jail_name) {
        return reply;
    }

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
    jaild_round_trip(jaild_path, &jaild_req)
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
