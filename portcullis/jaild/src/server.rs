//! Request-handling loop. Single-threaded, blocking accept;
//! one request → one response per connection-iteration.
//!
//! No async runtime (smallest-TCB carve-out), no per-connection
//! thread (yet — rationale: V0 is a low-rate broker, ~10 requests
//! per boot + 1 per app launch. Single-threaded handles that fine.
//! V1 may add a small thread pool if profiling shows contention).

use std::io::{BufReader, BufWriter, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use jaild_policy::Policy;
use log::{error, info, warn};

use crate::ffi::{self, JailCreateSpec};
use crate::protocol::{
    self, CreateJailRequest, CreateJailResponse, Request, Response,
};
use crate::validator;
use crate::JaildError;

/// Run the accept loop. Blocks until `listener` is dropped or an
/// unrecoverable error occurs.
///
/// Each accepted connection is handled to completion before the
/// next is accepted. That's intentional: if a client misbehaves
/// (slow, blocked), it doesn't affect other clients beyond a
/// queueing delay. Real load is too low for that to matter.
///
/// `dry_run`: if true, the validator runs but `jail_set` is not
/// invoked — useful for testing the protocol path without
/// privileges.
pub fn serve(
    listener: &UnixListener,
    policy:   &Policy,
    dry_run:  bool,
) -> Result<(), JaildError> {
    info!("jaild: ready (dry_run={dry_run})");
    for inbound in listener.incoming() {
        let stream = match inbound {
            Ok(s) => s,
            Err(e) => {
                error!("accept: {e}");
                return Err(JaildError::Io(e));
            }
        };
        if let Err(e) = handle_connection(stream, policy, dry_run) {
            warn!("connection closed with error: {e}");
        }
    }
    Ok(())
}

fn handle_connection(
    stream:  UnixStream,
    policy:  &Policy,
    dry_run: bool,
) -> Result<(), JaildError> {
    let peer_uid = peer_uid(&stream).unwrap_or(u32::MAX);
    info!("jaild: accepted conn (peer uid={peer_uid})");

    /* Caller-must-be-root check. portcullisd will be root
     * (it's started by jaild itself in the boot sequence). A non-
     * root caller is suspicious; refuse. */
    if peer_uid != 0 {
        warn!("non-root peer uid={peer_uid}; refusing");
        let resp = Response::Error {
            detail: "non-root peer".into(),
        };
        let _ = send_response(&stream, &resp);
        return Ok(());
    }

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);

    loop {
        let body = match protocol::read_frame(&mut reader)? {
            Some(b) => b,
            None    => return Ok(()),       // peer closed cleanly
        };

        let req: Request = match serde_json::from_slice(&body) {
            Ok(r) => r,
            Err(e) => {
                let resp = Response::Error {
                    detail: format!("malformed request: {e}"),
                };
                send_response_writer(&mut writer, &resp)?;
                continue;
            }
        };

        let resp = dispatch(req, policy, dry_run);
        send_response_writer(&mut writer, &resp)?;
    }
}

fn dispatch(req: Request, policy: &Policy, dry_run: bool) -> Response {
    match req {
        Request::Ping => Response::Ok,

        Request::CreateJail(spec) => match handle_create(&spec, policy, dry_run) {
            Ok(jid) => Response::JailCreated(CreateJailResponse { jid }),
            Err(JaildError::PolicyViolation { rule, detail }) => {
                Response::PolicyDenied { rule: rule.into(), detail }
            }
            Err(JaildError::Syscall { name, errno, msg }) => {
                Response::SyscallFailed { name: name.into(), errno, msg }
            }
            Err(other) => Response::Error { detail: format!("{other}") },
        },

        Request::RemoveJail { jid } => {
            if dry_run {
                info!("jaild: dry-run RemoveJail jid={jid}");
                return Response::Ok;
            }
            match ffi::remove_jail(jid) {
                Ok(()) => Response::Ok,
                Err(e) => Response::SyscallFailed {
                    name:  "jail_remove".into(),
                    errno: e.raw_os_error().unwrap_or(-1),
                    msg:   format!("{e}"),
                },
            }
        }
    }
}

fn handle_create(
    req:     &CreateJailRequest,
    policy:  &Policy,
    dry_run: bool,
) -> Result<i32, JaildError> {
    validator::validate_create(req, policy)?;

    if dry_run {
        info!(
            "jaild: dry-run CreateJail name={} path={} children_max={}",
            req.name, req.path, req.children_max);
        return Ok(99);  // sentinel jid
    }

    let spec = JailCreateSpec {
        name:         &req.name,
        path:         &req.path,
        persist:      1,
        children_max: req.children_max as i32,
    };
    let created = ffi::create_persistent_jail(&spec).map_err(|e| {
        JaildError::Syscall {
            name:  "jail_set",
            errno: e.raw_os_error().unwrap_or(-1),
            msg:   format!("{e}"),
        }
    })?;
    info!("jaild: jail_set OK name={} jid={}", req.name, created.jid);
    Ok(created.jid)
}

fn send_response_writer<W: Write>(
    w:    &mut W,
    resp: &Response,
) -> Result<(), JaildError> {
    let body = serde_json::to_vec(resp)?;
    protocol::write_frame(&mut *w, &body)?;
    w.flush()?;
    Ok(())
}

fn send_response(
    stream: &UnixStream,
    resp:   &Response,
) -> Result<(), JaildError> {
    let body = serde_json::to_vec(resp)?;
    let mut s = stream;
    protocol::write_frame(&mut s, &body)?;
    Ok(())
}

fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    use std::os::unix::io::AsRawFd;
    crate::ffi::getpeereid(stream.as_raw_fd())
}

/// Bind a listener at `socket_path`, removing any stale socket
/// first. Returns the listener; caller is responsible for keeping
/// it alive for the duration of `serve`.
pub fn bind(socket_path: &Path) -> Result<UnixListener, JaildError> {
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }
    let listener = UnixListener::bind(socket_path)?;
    /* mode 0600 — only root can connect. Future: chown to a
     * dedicated _jaild user once we have per-user system uids
     * allocated. */
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(socket_path, perms)?;
    Ok(listener)
}
