//! Request-handling loop. Single-threaded, blocking accept;
//! one request → one response per connection-iteration.
//!
//! No async runtime (smallest-TCB carve-out), no per-connection
//! thread (yet — rationale: V0 is a low-rate broker, ~10 requests
//! per boot + 1 per app launch. Single-threaded handles that fine.
//! V1 may add a small thread pool if profiling shows contention).

use std::io::BufReader;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use jaild_policy::Policy;
use log::{error, info, warn};

use crate::ffi::{self, JailCreateSpec};
use crate::protocol::{
    self, CreateJailRequest, CreateJailResponse, ExecSpec, MountKind,
    Request, Response,
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
        let body = serde_json::to_vec(&Response::Error {
            detail: "non-root peer".into(),
        })?;
        let _ = ffi::send_frame_with_optional_fd(stream.as_raw_fd(), &body, None);
        return Ok(());
    }

    let socket_fd = stream.as_raw_fd();
    let mut reader = BufReader::new(stream.try_clone()?);

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
                send_response(socket_fd, &resp, None)?;
                continue;
            }
        };

        /* For CreateJail-with-exec, dispatch returns the fd to
         * attach via SCM_RIGHTS; everything else returns None.
         * This branch keeps the raw send for the fd path while
         * everything else goes through the same single-call path. */
        let (resp, fd_to_attach) = dispatch(req, policy, dry_run);
        send_response(socket_fd, &resp, fd_to_attach)?;

        /* If we sent an fd over SCM_RIGHTS, our copy is no longer
         * needed (kernel keeps the procdesc alive while the
         * receiver holds it). */
        if let Some(fd) = fd_to_attach {
            let _ = ffi::close_fd(fd);
        }
    }
}

fn dispatch(
    req:     Request,
    policy:  &Policy,
    dry_run: bool,
) -> (Response, Option<i32>) {
    match req {
        Request::Ping => (Response::Ok, None),

        Request::CreateJail(spec) => {
            match handle_create(&spec, policy, dry_run) {
                Ok(CreateOutcome { resp, procdesc_fd }) => {
                    (Response::JailCreated(resp), procdesc_fd)
                }
                Err(JaildError::PolicyViolation { rule, detail }) => {
                    (Response::PolicyDenied { rule: rule.into(), detail }, None)
                }
                Err(JaildError::Syscall { name, errno, msg }) => {
                    (Response::SyscallFailed { name: name.into(), errno, msg }, None)
                }
                Err(other) => (Response::Error { detail: format!("{other}") }, None),
            }
        }

        Request::RemoveJail { jid } => {
            if dry_run {
                info!("jaild: dry-run RemoveJail jid={jid}");
                return (Response::Ok, None);
            }
            match ffi::remove_jail(jid) {
                Ok(()) => (Response::Ok, None),
                Err(e) => (Response::SyscallFailed {
                    name:  "jail_remove".into(),
                    errno: e.raw_os_error().unwrap_or(-1),
                    msg:   format!("{e}"),
                }, None),
            }
        }
    }
}

/// Internal carrier so handle_create can return both the JSON
/// response body AND a procdesc fd to attach via SCM_RIGHTS.
struct CreateOutcome {
    resp:        CreateJailResponse,
    procdesc_fd: Option<i32>,
}

fn handle_create(
    req:     &CreateJailRequest,
    policy:  &Policy,
    dry_run: bool,
) -> Result<CreateOutcome, JaildError> {
    validator::validate_create(req, policy)?;

    if dry_run {
        info!(
            "jaild: dry-run CreateJail name={} path={} mounts={} exec={}",
            req.name, req.path, req.mounts.len(),
            req.exec.is_some());
        return Ok(CreateOutcome {
            resp: CreateJailResponse {
                jid: 99, pid: 0, procdesc_attached: false,
            },
            procdesc_fd: None,
        });
    }

    if let Some(exec) = &req.exec {
        return handle_create_with_exec(req, exec);
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
    Ok(CreateOutcome {
        resp: CreateJailResponse {
            jid: created.jid, pid: 0, procdesc_attached: false,
        },
        procdesc_fd: None,
    })
}

fn handle_create_with_exec(
    req:  &CreateJailRequest,
    exec: &ExecSpec,
) -> Result<CreateOutcome, JaildError> {
    /* Pre-resolve mount targets into absolute paths under the jail
     * root so the child can apply them with a single nmount per
     * mount. Validator already screened sources + traversal. */
    let resolved_mounts: Vec<(String, String, MountKind)> = req.mounts.iter()
        .map(|m| {
            let dest = if m.dest.starts_with('/') {
                PathBuf::from(&m.dest)
            } else {
                PathBuf::from(&req.path).join(&m.dest)
            };
            (m.source.clone(), dest.to_string_lossy().into_owned(), m.kind)
        })
        .collect();

    let pdf = ffi::pdfork().map_err(|e| JaildError::Syscall {
        name:  "pdfork",
        errno: e.raw_os_error().unwrap_or(-1),
        msg:   format!("{e}"),
    })?;

    if pdf.pid == 0 {
        /* ====== child ====== */
        for (src, dst, kind) in &resolved_mounts {
            let res = match kind {
                MountKind::RoNullfs => ffi::nullfs_mount(src, dst, true),
                MountKind::RwNullfs => ffi::nullfs_mount(src, dst, false),
                MountKind::Tmpfs    => ffi::tmpfs_mount(dst),
            };
            if let Err(e) = res {
                eprintln!("jaild-child: mount {kind:?} {src} -> {dst}: {e}");
                ffi::child_exit(101);
            }
        }

        let spec = JailCreateSpec {
            name:         &req.name,
            path:         &req.path,
            persist:      0,    // ATTACH path: jail dies with the process
            children_max: req.children_max as i32,
        };
        if let Err(e) = ffi::jail_create_and_attach(&spec) {
            eprintln!("jaild-child: jail_create_and_attach: {e}");
            ffi::child_exit(102);
        }
        if let Err(e) = ffi::drop_privileges(exec.uid, exec.gid) {
            eprintln!("jaild-child: drop_privileges({}, {}): {e}",
                exec.uid, exec.gid);
            ffi::child_exit(103);
        }
        let env_pairs: Vec<(String, String)> = exec.env.iter()
            .map(|p| (p.key.clone(), p.value.clone()))
            .collect();
        match ffi::execve(&exec.path, &exec.argv, &env_pairs) {
            Ok(_) => unreachable!("execve returned Ok on success"),
            Err(e) => {
                eprintln!("jaild-child: execve {}: {e}", exec.path);
                ffi::child_exit(104);
            }
        }
    }

    /* ====== parent ====== */
    info!("jaild: pdfork ok pid={} pdfd={}", pdf.pid, pdf.procdesc_fd);

    /* jid not knowable from the parent without enumerating the
     * kernel jail list; sentinel 0. The procdesc fd is the
     * authoritative lifecycle handle. */
    Ok(CreateOutcome {
        resp: CreateJailResponse {
            jid: 0,
            pid: pdf.pid,
            procdesc_attached: true,
        },
        procdesc_fd: Some(pdf.procdesc_fd),
    })
}

fn send_response(
    socket_fd: i32,
    resp:      &Response,
    fd:        Option<i32>,
) -> Result<(), JaildError> {
    let body = serde_json::to_vec(resp)?;
    ffi::send_frame_with_optional_fd(socket_fd, &body, fd)?;
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
