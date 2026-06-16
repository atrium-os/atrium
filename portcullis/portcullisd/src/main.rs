//! portcullisd — Portcullis policy + launch daemon.
//!
//! Long-running multi-tenant process. Listens at
//! `portcullis_ipc::SOCKET_PATH` (mode 0666, world-connectable);
//! authenticates each connection via `getpeereid(2)` to identify
//! the calling user, then serves Authorize/Grant/Revoke/Launch
//! requests against THAT user's policy file at
//! `/var/db/atrium/<user>/policy.toml`. Per-user policy state is
//! cached lazily in-process.
//!
//! PRIVILEGE INVARIANT (see portcullis.md §9.0, insula.md): no app runs as
//! root — every app runs under a dedicated, non-root per-app uid inside its
//! jail. Root is the TCB's alone (jaild + the privileged launch step).
//! KNOWN DEVIATION (bring-up): this path currently sets `exec.jail_user` to the
//! *connecting user*, so a root-driven launch wrongly runs the app as root. The
//! fix is to allocate a per-app uid (portcullis_peer) and route exec through
//! jaild's uid-range validation. Until then the privilege boundary is NOT proven.
//!
//! Concurrency: one thread per accepted connection. Per-tenant
//! policy state lives behind `Arc<Mutex<Tenants>>`; lock is held
//! only across the small read/modify/write window for Grant/Revoke.

use std::collections::HashMap;
use std::ffi::CStr;
use std::io::BufReader;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::thread;

use portcullis_ipc::{
    read_request, write_response, Request, Response, PROTO_VERSION, SOCKET_PATH,
};
use portcullis_policy::{compute_delta, hash_manifest, now_iso8601, Grant, Policy};

mod launch;
mod manifest_trust;

fn usage() -> ! {
    eprintln!("\
usage:
    portcullisd [--socket <path>]

    Long-running daemon. Multi-tenant: identifies each connecting
    user via getpeereid(2) and serves their per-user policy from
    /var/db/atrium/<user>/policy.toml. Loads tenant policies lazily
    on first request.

    --socket  Override default socket path ({SOCKET_PATH}).
              Useful for tests; production uses the default.
");
    std::process::exit(2);
}

fn main() -> ExitCode {
    let mut socket_path: Option<PathBuf> = None;
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--socket" if i + 1 < args.len() => {
                socket_path = Some(args[i + 1].clone().into());
                i += 2;
            }
            "--help" | "-h" => usage(),
            other => {
                eprintln!("portcullisd: unknown arg {other:?}");
                usage();
            }
        }
    }
    let socket_path = socket_path.unwrap_or_else(|| PathBuf::from(SOCKET_PATH));

    /* Stale-socket cleanup: a previous crashed daemon may have left
     * the file behind; bind() refuses to overwrite. */
    let _ = std::fs::remove_file(&socket_path);
    if let Some(parent) = socket_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let listener = match UnixListener::bind(&socket_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("portcullisd: bind {}: {e}", socket_path.display());
            return ExitCode::from(1);
        }
    };
    /* World-connectable: any user on the host can connect. Per-user
     * authorization happens inside the daemon via getpeereid + per-
     * tenant policy lookup; the socket itself is just a rendezvous
     * point. (Earlier the socket was 0600 owned by a single user,
     * which broke multi-user use; multi-tenancy + peer-cred check is
     * the proper model.) */
    if let Err(e) = std::fs::set_permissions(&socket_path,
                        std::fs::Permissions::from_mode(0o666)) {
        eprintln!("portcullisd: chmod {}: {e}", socket_path.display());
        return ExitCode::from(1);
    }
    eprintln!("portcullisd: listening on {} (multi-tenant)",
        socket_path.display());

    let shared = Arc::new(Mutex::new(Tenants { cache: HashMap::new() }));

    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let s = Arc::clone(&shared);
                thread::spawn(move || {
                    if let Err(e) = serve(stream, s) {
                        eprintln!("portcullisd: connection: {e}");
                    }
                });
            }
            Err(e) => {
                eprintln!("portcullisd: accept: {e}");
            }
        }
    }
    ExitCode::SUCCESS
}

// ── per-tenant policy cache ──────────────────────────────────────

struct TenantPolicy {
    policy:      Policy,
    policy_path: PathBuf,
}

struct Tenants {
    cache: HashMap<String, TenantPolicy>,
}

impl Tenants {
    /// Get the tenant's policy state, lazily loading from disk on
    /// first access. Subsequent calls return the cached version (and
    /// reflect any in-memory Grant/Revoke updates).
    fn get_or_load(&mut self, user: &str) -> std::io::Result<&mut TenantPolicy> {
        if !self.cache.contains_key(user) {
            let path = Policy::user_path(user);
            let policy = Policy::load(&path)?;
            self.cache.insert(user.to_string(), TenantPolicy {
                policy, policy_path: path,
            });
        }
        Ok(self.cache.get_mut(user).unwrap())
    }
}

// ── peer credential lookup (SO_PEERCRED equivalent on FreeBSD) ───

/// Returns (uid, gid) of the peer connected to `stream`. On FreeBSD
/// `getpeereid(2)` is the canonical API for AF_UNIX peer credentials.
fn peer_eid(stream: &UnixStream) -> std::io::Result<(u32, u32)> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    let r = unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) };
    if r < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok((uid, gid))
    }
}

/// Resolve a uid to its login name via getpwuid_r. Returns an
/// io::Error with Other kind if the uid isn't in passwd (which
/// means the connecting process is somehow attached to a uid we
/// can't account for — refuse to serve).
fn username_for_uid(uid: u32) -> std::io::Result<String> {
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0u8 as libc::c_char; 4096];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    let r = unsafe {
        libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result)
    };
    if r != 0 {
        return Err(std::io::Error::from_raw_os_error(r));
    }
    if result.is_null() {
        return Err(std::io::Error::other(format!("uid {uid} not in passwd")));
    }
    let name = unsafe { CStr::from_ptr(pwd.pw_name) };
    Ok(name.to_string_lossy().into_owned())
}

// ── connection handling ──────────────────────────────────────────

fn serve(stream: UnixStream, shared: Arc<Mutex<Tenants>>) -> std::io::Result<()> {
    /* Identify the peer before reading any protocol bytes. If we
     * can't, refuse the connection — we have no way to know whose
     * policy to consult. */
    let (uid, _gid) = peer_eid(&stream)?;
    let user = username_for_uid(uid)?;
    eprintln!("portcullisd: conn from uid={uid} ({user})");

    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    /* Mandatory handshake (unchanged). */
    let first = read_request(&mut reader)?;
    match first {
        Request::Hello { version } => {
            if version != PROTO_VERSION {
                write_response(&mut writer,
                    &Response::ProtoMismatch { server_version: PROTO_VERSION })?;
                return Ok(());
            }
            write_response(&mut writer, &Response::Hello { version: PROTO_VERSION })?;
        }
        _ => {
            write_response(&mut writer, &Response::Error {
                message: "expected Hello as first message".into() })?;
            return Ok(());
        }
    }

    loop {
        let req = match read_request(&mut reader) {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        if let Request::Launch { app_id, bypass_policy } = req {
            handle_launch(app_id, bypass_policy, &user, &shared,
                          &mut writer, &reader)?;
            continue;
        }
        let resp = handle(req, &user, &shared);
        write_response(&mut writer, &resp)?;
    }
}

/// Launch handler with SCM_RIGHTS fd handoff (see Phase 4.4 step 2).
/// Adds per-user policy lookup and `exec.jail_user = <user>` so the
/// app runs as the connecting user inside its per-app jail.
fn handle_launch(
    app_id:        String,
    bypass_policy: bool,
    user:          &str,
    shared:        &Mutex<Tenants>,
    writer:        &mut UnixStream,
    reader:        &BufReader<UnixStream>,
) -> std::io::Result<()> {
    /* 1. policy gate (per-tenant). */
    if !bypass_policy {
        let tree = std::path::PathBuf::from("/var/lib/atrium/apps").join(&app_id);
        let manifest_path = tree.join("atrium.toml");
        let text = match std::fs::read_to_string(&manifest_path) {
            Ok(t) => t,
            Err(e) => {
                return write_response(writer, &Response::LaunchFailed {
                    stage: "manifest".into(),
                    message: format!("{}: {e}", manifest_path.display()),
                });
            }
        };
        /* manifest TRUST gate (Sigstore Option A, keyed) — the shared check used
         * by every user-app launch vector. */
        if let Err(msg) = manifest_trust::verify(&tree, &text) {
            eprintln!("portcullisd: REFUSED {app_id} — {msg}");
            return write_response(writer, &Response::LaunchFailed {
                stage: "signature".into(),
                message: msg,
            });
        }

        let manifest = match portcullis_toml::Manifest::from_str(&text) {
            Ok(m) => m,
            Err(e) => {
                return write_response(writer, &Response::LaunchFailed {
                    stage: "manifest".into(),
                    message: format!("parse error: {e}"),
                });
            }
        };
        let current_hash = hash_manifest(text.as_bytes());
        let mut s = shared.lock().unwrap();
        let tp = match s.get_or_load(user) {
            Ok(tp) => tp,
            Err(e) => return write_response(writer, &Response::Error {
                message: format!("policy load for {user}: {e}"),
            }),
        };
        let prior = tp.policy.grants.get(&app_id);
        let delta = compute_delta(
            &manifest.capabilities,
            prior.map(|g| &g.capabilities),
            prior.map(|g| g.manifest_hash.as_str()),
            &current_hash,
        );
        drop(s);
        if !delta.is_empty() {
            return write_response(writer, &Response::LaunchNeedsApproval {
                delta: delta.describe(),
            });
        }
    }

    /* 2. ReadyForFds round-trip. */
    write_response(writer, &Response::ReadyForFds)?;

    /* 3. recv_fds. */
    if !reader.buffer().is_empty() {
        return write_response(writer, &Response::LaunchFailed {
            stage: "fdpass".into(),
            message: "protocol violation: data buffered before fd handoff".into(),
        });
    }
    let fds = match portcullis_ipc::recv_fds(reader.get_ref(), 3) {
        Ok(v) => v,
        Err(e) => {
            return write_response(writer, &Response::LaunchFailed {
                stage: "fdpass".into(),
                message: format!("recv_fds: {e}"),
            });
        }
    };
    if fds.len() != 3 {
        return write_response(writer, &Response::LaunchFailed {
            stage: "fdpass".into(),
            message: format!("expected 3 fds, got {}", fds.len()),
        });
    }
    let mut iter = fds.into_iter();
    let stdio = [iter.next().unwrap(), iter.next().unwrap(), iter.next().unwrap()];

    /* 4. launch as the connecting user. launch_with_stdio sets
     *    BuildOpts.user_name = user, which becomes exec.jail_user
     *    in the rendered jail.conf. */
    let resp = match launch::launch_with_stdio(&app_id, user, Some(stdio)) {
        Ok(o) => Response::LaunchExit { code: o.exit_code },
        Err(e) => Response::LaunchFailed {
            stage: e.stage().into(),
            message: e.message(),
        },
    };
    write_response(writer, &resp)
}

fn handle(req: Request, user: &str, shared: &Mutex<Tenants>) -> Response {
    match req {
        Request::Hello { .. } => Response::Error {
            message: "Hello already exchanged".into(),
        },
        Request::Ping => Response::Pong,
        Request::Authorize { app_id, manifest_hash, requested } => {
            let mut s = shared.lock().unwrap();
            let tp = match s.get_or_load(user) {
                Ok(tp) => tp,
                Err(e) => return Response::Error {
                    message: format!("policy load for {user}: {e}"),
                },
            };
            let prior = tp.policy.grants.get(&app_id);
            let delta = compute_delta(
                &requested,
                prior.map(|g| &g.capabilities),
                prior.map(|g| g.manifest_hash.as_str()),
                &manifest_hash,
            );
            if delta.is_empty() {
                Response::Authorized
            } else {
                Response::NeedsApproval { delta: delta.describe() }
            }
        }
        Request::Grant { app_id, manifest_hash, capabilities } => {
            let mut s = shared.lock().unwrap();
            let tp = match s.get_or_load(user) {
                Ok(tp) => tp,
                Err(e) => return Response::Error {
                    message: format!("policy load for {user}: {e}"),
                },
            };
            let grant = Grant {
                manifest_hash,
                granted_at: now_iso8601(),
                capabilities,
            };
            tp.policy.grants.insert(app_id, grant);
            let path = tp.policy_path.clone();
            match tp.policy.save(&path) {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error { message: format!("save: {e}") },
            }
        }
        Request::Revoke { app_id } => {
            let mut s = shared.lock().unwrap();
            let tp = match s.get_or_load(user) {
                Ok(tp) => tp,
                Err(e) => return Response::Error {
                    message: format!("policy load for {user}: {e}"),
                },
            };
            if tp.policy.grants.remove(&app_id).is_none() {
                return Response::Error { message: format!("no grant for {app_id}") };
            }
            let path = tp.policy_path.clone();
            match tp.policy.save(&path) {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error { message: format!("save: {e}") },
            }
        }
        Request::Reload => {
            let mut s = shared.lock().unwrap();
            /* Drop this tenant's cached entry; next request re-loads
             * from disk. Other tenants' caches are untouched. */
            s.cache.remove(user);
            Response::Ok
        }
        Request::Launch { .. } => Response::Error {
            /* Caught by serve() before reaching here. */
            message: "internal: Launch must go through handle_launch".into(),
        },
    }
}

// ── tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use portcullis_ipc::{round_trip, write_request};
    use std::io::Read;
    use std::time::Duration;

    fn tmp_socket() -> PathBuf {
        let pid = std::process::id();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("portcullisd-test-{pid}-{nonce}.sock"))
    }

    /// Spin up a daemon thread on a tmp socket. Tests use the
    /// connecting process's own uid/username — works because every
    /// host the tests run on has a real $USER with a passwd entry.
    fn spawn_daemon(socket: PathBuf) {
        thread::spawn(move || {
            let _ = std::fs::remove_file(&socket);
            let listener = UnixListener::bind(&socket).unwrap();
            std::fs::set_permissions(&socket,
                std::fs::Permissions::from_mode(0o666)).unwrap();
            let shared = Arc::new(Mutex::new(Tenants { cache: HashMap::new() }));
            for conn in listener.incoming() {
                let stream = conn.unwrap();
                let s = Arc::clone(&shared);
                thread::spawn(move || { let _ = serve(stream, s); });
            }
        });
        thread::sleep(Duration::from_millis(50));
    }

    fn connect_and_hello(socket: &std::path::Path) -> UnixStream {
        let mut s = UnixStream::connect(socket).unwrap();
        let resp = round_trip(&mut s, &Request::Hello { version: PROTO_VERSION }).unwrap();
        matches!(resp, Response::Hello { .. });
        s
    }

    #[test]
    fn ping_pong_over_socket() {
        let sock = tmp_socket();
        spawn_daemon(sock.clone());
        let mut c = connect_and_hello(&sock);
        let r = round_trip(&mut c, &Request::Ping).unwrap();
        assert!(matches!(r, Response::Pong));
    }

    #[test]
    fn proto_mismatch_closes_connection() {
        let sock = tmp_socket();
        spawn_daemon(sock.clone());
        let mut c = UnixStream::connect(&sock).unwrap();
        write_request(&mut c, &Request::Hello { version: 999 }).unwrap();
        let r = portcullis_ipc::read_response(&mut c).unwrap();
        assert!(matches!(r, Response::ProtoMismatch { .. }));
        let mut buf = [0u8; 1];
        let n = c.read(&mut buf).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn peer_uid_is_resolved() {
        /* Smoke check: peer_eid returns our own uid for a self-connection. */
        let (a, _b) = UnixStream::pair().unwrap();
        let (uid, _gid) = peer_eid(&a).unwrap();
        let me = unsafe { libc::geteuid() };
        assert_eq!(uid, me);
    }
}
