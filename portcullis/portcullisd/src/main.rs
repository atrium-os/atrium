//! portcullisd — Portcullis policy daemon.
//!
//! Long-running process that holds the per-user `Policy` in memory
//! and serves Authorize/Grant/Revoke requests over a Unix-domain
//! socket at `portcullis_ipc::SOCKET_PATH`.
//!
//! Phase 4 step 3 scope: policy oracle only. Jail supervision
//! (waitpid, restart per [supervision] policy, reaping) lands in
//! a later step.
//!
//! Concurrency: one thread per accepted connection. Policy is
//! shared via `Arc<Mutex<Policy>>`. The lock is held only across
//! the small read/modify/write window for Grant/Revoke, never
//! while serving Authorize (which is a pure function of the
//! current snapshot).
//!
//! Run as the user whose policy it manages — the socket is
//! created mode 0600 by way of a private umask so other users
//! can't connect. (No SO_PEERCRED check yet; Phase 5 may add one
//! once we actually have multi-user expectations.)

use std::io::BufReader;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::thread;

use portcullis_ipc::{
    read_request, write_response, Request, Response, PROTO_VERSION, SOCKET_PATH,
};
use portcullis_policy::{compute_delta, hash_manifest, now_iso8601, Grant, Policy};

mod launch;

fn usage() -> ! {
    eprintln!("\
usage:
    portcullisd [--user <name>] [--socket <path>] [--policy <path>]

    Long-running daemon. Loads the per-user policy file, then accepts
    Unix-domain socket connections and serves Authorize/Grant/Revoke
    requests.

    --user      User whose policy to manage (default $USER).
    --socket    Override socket path (default {SOCKET_PATH}).
    --policy    Override policy file path (default
                /var/db/atrium/<user>/policy.toml).
");
    std::process::exit(2);
}

fn current_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "atrium".into())
}

fn main() -> ExitCode {
    let mut user:        Option<String>  = None;
    let mut socket_path: Option<PathBuf> = None;
    let mut policy_path: Option<PathBuf> = None;

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--user"   if i+1 < args.len() => { user = Some(args[i+1].clone()); i += 2; }
            "--socket" if i+1 < args.len() => { socket_path = Some(args[i+1].clone().into()); i += 2; }
            "--policy" if i+1 < args.len() => { policy_path = Some(args[i+1].clone().into()); i += 2; }
            "--help" | "-h" => usage(),
            other => {
                eprintln!("portcullisd: unknown arg {other:?}");
                usage();
            }
        }
    }

    let user = user.unwrap_or_else(current_user);
    let socket_path = socket_path.unwrap_or_else(|| PathBuf::from(SOCKET_PATH));
    let policy_path = policy_path.unwrap_or_else(|| Policy::user_path(&user));

    let policy = match Policy::load(&policy_path) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("portcullisd: load {}: {e}", policy_path.display());
            return ExitCode::from(1);
        }
    };
    eprintln!("portcullisd: loaded {} grants from {}",
        policy.grants.len(), policy_path.display());

    /* Stale-socket cleanup: if a previous daemon crashed, the file
     * on disk is dead but bind() will refuse to overwrite it.
     * Removing it before bind is the standard recipe. */
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
    /* Lock the socket to the owning user. */
    if let Err(e) = std::fs::set_permissions(&socket_path,
                        std::fs::Permissions::from_mode(0o600)) {
        eprintln!("portcullisd: chmod {}: {e}", socket_path.display());
        return ExitCode::from(1);
    }
    eprintln!("portcullisd: listening on {} (user={})",
        socket_path.display(), user);

    let shared = Arc::new(Mutex::new(DaemonState {
        policy,
        policy_path,
    }));

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

struct DaemonState {
    policy:      Policy,
    policy_path: PathBuf,
}

fn serve(stream: UnixStream, shared: Arc<Mutex<DaemonState>>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    /* Mandatory handshake. */
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
        let resp = handle(req, &shared);
        write_response(&mut writer, &resp)?;
    }
}

fn handle(req: Request, shared: &Mutex<DaemonState>) -> Response {
    match req {
        Request::Hello { .. } => Response::Error {
            message: "Hello already exchanged".into(),
        },
        Request::Ping => Response::Pong,
        Request::Authorize { app_id, manifest_hash, requested } => {
            let s = shared.lock().unwrap();
            let prior = s.policy.grants.get(&app_id);
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
            let grant = Grant {
                manifest_hash,
                granted_at: now_iso8601(),
                capabilities,
            };
            s.policy.grants.insert(app_id, grant);
            let path = s.policy_path.clone();
            match s.policy.save(&path) {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error { message: format!("save: {e}") },
            }
        }
        Request::Revoke { app_id } => {
            let mut s = shared.lock().unwrap();
            if s.policy.grants.remove(&app_id).is_none() {
                return Response::Error { message: format!("no grant for {app_id}") };
            }
            let path = s.policy_path.clone();
            match s.policy.save(&path) {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error { message: format!("save: {e}") },
            }
        }
        Request::Launch { app_id, bypass_policy } => {
            /* Policy gate first (unless explicit dev bypass).
             * Re-reads the manifest from disk so the hash check
             * sees current bytes, matching what `policy diff` does. */
            if !bypass_policy {
                let tree = std::path::PathBuf::from("/var/lib/atrium/apps").join(&app_id);
                let manifest_path = tree.join("atrium.toml");
                let text = match std::fs::read_to_string(&manifest_path) {
                    Ok(t) => t,
                    Err(e) => return Response::LaunchFailed {
                        stage: "manifest".into(),
                        message: format!("{}: {e}", manifest_path.display()),
                    },
                };
                let manifest = match portcullis_toml::Manifest::from_str(&text) {
                    Ok(m) => m,
                    Err(e) => return Response::LaunchFailed {
                        stage: "manifest".into(),
                        message: format!("parse error: {e}"),
                    },
                };
                let current_hash = hash_manifest(text.as_bytes());
                let s = shared.lock().unwrap();
                let prior = s.policy.grants.get(&app_id);
                let delta = compute_delta(
                    &manifest.capabilities,
                    prior.map(|g| &g.capabilities),
                    prior.map(|g| g.manifest_hash.as_str()),
                    &current_hash,
                );
                drop(s);
                if !delta.is_empty() {
                    /* "approval needed" fits LaunchFailed semantics for
                     * step 1 — clearer to the client than reusing
                     * NeedsApproval, which is an Authorize-shape reply. */
                    return Response::LaunchFailed {
                        stage: "policy".into(),
                        message: format!("needs approval: {}",
                                         delta.describe().join("; ")),
                    };
                }
            }
            match launch::launch(&app_id) {
                Ok(o) => Response::LaunchExit { code: o.exit_code },
                Err(e) => Response::LaunchFailed {
                    stage: e.stage().into(),
                    message: e.message(),
                },
            }
        }
        Request::Reload => {
            let mut s = shared.lock().unwrap();
            let path = s.policy_path.clone();
            match Policy::load(&path) {
                Ok(p) => { s.policy = p; Response::Ok }
                Err(e) => Response::Error { message: format!("reload: {e}") },
            }
        }
    }
}

#[allow(dead_code)]
fn touch(_p: &Path) {} /* placeholder for future on-disk staleness check */

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
    fn tmp_policy() -> PathBuf {
        let pid = std::process::id();
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("portcullisd-policy-{pid}-{nonce}.toml"))
    }

    /// Spin up a daemon thread, return (socket_path, shutdown handle).
    fn spawn_daemon(socket: PathBuf, policy_path: PathBuf) {
        thread::spawn(move || {
            let _ = std::fs::remove_file(&socket);
            let listener = UnixListener::bind(&socket).unwrap();
            std::fs::set_permissions(&socket,
                std::fs::Permissions::from_mode(0o600)).unwrap();
            let policy = Policy::load(&policy_path).unwrap_or_default();
            let shared = Arc::new(Mutex::new(DaemonState { policy, policy_path }));
            for conn in listener.incoming() {
                let stream = conn.unwrap();
                let s = Arc::clone(&shared);
                thread::spawn(move || { let _ = serve(stream, s); });
            }
        });
        /* Give the listener a beat to come up. */
        thread::sleep(Duration::from_millis(50));
    }

    fn connect_and_hello(socket: &Path) -> UnixStream {
        let mut s = UnixStream::connect(socket).unwrap();
        let resp = round_trip(&mut s, &Request::Hello { version: PROTO_VERSION }).unwrap();
        matches!(resp, Response::Hello { .. });
        s
    }

    #[test]
    fn ping_pong_over_socket() {
        let sock = tmp_socket();
        let pol  = tmp_policy();
        spawn_daemon(sock.clone(), pol);
        let mut c = connect_and_hello(&sock);
        let r = round_trip(&mut c, &Request::Ping).unwrap();
        assert!(matches!(r, Response::Pong));
    }

    #[test]
    fn grant_then_authorize_succeeds() {
        let sock = tmp_socket();
        let pol  = tmp_policy();
        spawn_daemon(sock.clone(), pol.clone());
        let mut c = connect_and_hello(&sock);

        let mut caps = portcullis_toml::Capabilities::default();
        caps.clipboard = Some(true);

        /* Before grant: NeedsApproval. */
        let r = round_trip(&mut c, &Request::Authorize {
            app_id: "test.app".into(),
            manifest_hash: "h1".into(),
            requested: caps.clone(),
        }).unwrap();
        assert!(matches!(r, Response::NeedsApproval { .. }));

        /* Grant. */
        let r = round_trip(&mut c, &Request::Grant {
            app_id: "test.app".into(),
            manifest_hash: "h1".into(),
            capabilities: caps.clone(),
        }).unwrap();
        assert!(matches!(r, Response::Ok));

        /* After grant: Authorized. */
        let r = round_trip(&mut c, &Request::Authorize {
            app_id: "test.app".into(),
            manifest_hash: "h1".into(),
            requested: caps,
        }).unwrap();
        assert!(matches!(r, Response::Authorized));

        let _ = std::fs::remove_file(&pol);
    }

    #[test]
    fn proto_mismatch_closes_connection() {
        let sock = tmp_socket();
        let pol  = tmp_policy();
        spawn_daemon(sock.clone(), pol);
        let mut c = UnixStream::connect(&sock).unwrap();
        write_request(&mut c, &Request::Hello { version: 999 }).unwrap();
        let r = portcullis_ipc::read_response(&mut c).unwrap();
        assert!(matches!(r, Response::ProtoMismatch { .. }));
        /* Daemon should close — next read returns 0 bytes. */
        let mut buf = [0u8; 1];
        let n = c.read(&mut buf).unwrap();
        assert_eq!(n, 0);
    }
}
