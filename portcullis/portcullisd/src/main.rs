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
//! root — every app runs under a dedicated, non-root, non-human per-app uid
//! (50000+) inside its jail. Root is the TCB's alone (jaild + the privileged
//! launch step). The owning HUMAN is recorded in the launch registry; the app
//! executes as its own uid that services peer-cred back to `(owner, app_id)`.
//! Implemented in `launch::resolve_app_uid`. (Longer-term the privileged
//! passwd/jail steps consolidate into jaild, the audited broker.)
//!
//! The owner is not simply the connecting peer. When the caller is itself a
//! jailed app — a launcher like Forum's dock — it is the REQUESTER, and the
//! launch belongs to the human that launcher belongs to. `launch_owner` reads
//! that from the registry, which makes it transitive down a chain of launchers.
//! Deliberately scoped to launches: Grant/Revoke stay on the peer identity, or
//! an app could write capabilities into its owner's policy and approve itself.
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
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::thread;

use portcullis_ipc::{
    read_request, write_response, GovSignal, Request, Response, PROTO_VERSION,
    SERVICE_SOCKET_PATH, SOCKET_PATH,
};
use portcullis_policy::{compute_delta, hash_manifest, now_iso8601, Grant, Policy};

mod launch;
mod manifest_trust;
#[cfg(feature = "pam")]
mod pam;

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
    /* Bind under the per-service DIRECTORY (/atrium/sockets/portcullis/), not
     * loose in the shared sockets root. A jail reaches a service by nullfs-
     * mounting that service's directory — mount_nullfs cannot mount a socket
     * node — so while the socket sat directly in /atrium/sockets/ there was no
     * way to give a jailed app the daemon WITHOUT also exposing every other
     * service's socket. That is why the dock, which IS the app launcher, could
     * not reach portcullisd to launch anything. The flat path stays as a
     * symlink below, so every existing client keeps working untouched. */
    let socket_path = socket_path.unwrap_or_else(|| PathBuf::from(SERVICE_SOCKET_PATH));

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
    /* Compat: keep the historical flat path working for every client that
     * still holds SOCKET_PATH (the CLI, scripts, anything outside a jail).
     * A symlink, not a second listener — one accept loop, one socket. */
    if socket_path == Path::new(SERVICE_SOCKET_PATH) {
        let _ = std::fs::remove_file(SOCKET_PATH);
        if let Err(e) = std::os::unix::fs::symlink(SERVICE_SOCKET_PATH, SOCKET_PATH) {
            eprintln!("portcullisd: WARNING could not link {SOCKET_PATH} -> \
                {SERVICE_SOCKET_PATH}: {e} (clients using the old path will fail)");
        }
    }
    eprintln!("portcullisd: listening on {} (multi-tenant)",
        socket_path.display());
    #[cfg(not(feature = "pam"))]
    eprintln!("portcullisd: WARNING — built WITHOUT --features pam: VerifyCredential \
        is the DEV STUB (accepts ANY password). NOT FOR PRODUCTION.");

    let shared = Arc::new(Mutex::new(Tenants { cache: HashMap::new(), session_procdescs: HashMap::new() }));

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
    /// Session-component procdescs portcullisd holds on ostiarius's behalf,
    /// keyed by jail name. Holding the fd here (the long-lived root TCB daemon)
    /// keeps each persist=0 session jail alive; dropping it on teardown kills the
    /// jail. See docs/spec/ostiarius-privsep.md §2.1.
    session_procdescs: HashMap<String, std::os::fd::OwnedFd>,
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

/// Who OWNS an app launched over this connection.
///
/// `peer_user` is getpeereid's answer — who is talking to us. `registry_owner`
/// is set when that uid is itself a Portcullis-launched app, and is the owner
/// recorded when THAT app was launched.
///
/// A launcher is the requester, not the owner, so an app's launches belong to
/// the human the launcher belongs to. Because each app's registry entry already
/// carries the owner it was launched under, resolving one level is transitively
/// correct — a chain of launchers all land on the human at its root.
fn launch_owner(peer_user: &str, registry_owner: Option<&str>) -> String {
    registry_owner.unwrap_or(peer_user).to_string()
}

fn serve(stream: UnixStream, shared: Arc<Mutex<Tenants>>) -> std::io::Result<()> {
    /* Identify the peer before reading any protocol bytes. If we
     * can't, refuse the connection — we have no way to know whose
     * policy to consult. */
    let (uid, _gid) = peer_eid(&stream)?;
    let user = username_for_uid(uid)?;
    eprintln!("portcullisd: conn from uid={uid} ({user})");

    /* ★ Owner inheritance (#118). getpeereid answers "who is talking to me",
     * which is the right owner when a human runs the CLI and the WRONG one
     * when the caller is itself a jailed app: a launcher is the REQUESTER, not
     * the owner. Without this, an app the dock launches comes out owned by the
     * dock's per-app uid — so the grant authorizing it has to live in a robot
     * identity's policy file instead of the human's, the launch registry
     * records an app as the owner of an app, and a prompt UI would have no
     * human to ask.
     *
     * The registry already maps uid -> (owner, app-id), and the requesting
     * app's own entry records the owner it was launched under. So resolving
     * one level here is transitively correct: a chain of launchers all resolve
     * to the human at the root of the chain, however deep it gets.
     *
     * ONLY the launch path uses this. Grant/Revoke stay on the peer identity
     * on purpose — a jailed app that inherited its owner for Grant could write
     * capabilities into the HUMAN's policy and approve itself. Requesting a
     * launch on the owner's behalf is delegation; editing the owner's policy
     * is escalation. */
    let registry = portcullis_peer::AppRegistry::load(portcullis_peer::DEFAULT_REGISTRY)
        .unwrap_or_default();
    if let Some((reg_owner, app)) = registry.resolve(uid) {
        eprintln!("portcullisd: uid={uid} is app {app}; launches inherit owner={reg_owner}");
    }
    let owner = launch_owner(&user, registry.resolve(uid).map(|(o, _)| o));

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
            /* `owner`, not `user`: see the inheritance note above. This is both
             * whose policy authorizes the launch and who the launched app is
             * recorded as belonging to. */
            handle_launch(app_id, bypass_policy, &owner, &shared,
                          &mut writer, &reader)?;
            continue;
        }
        /* Catalog is gated on the PEER UID like the broker verbs below, not on
         * per-tenant policy: the caller is identified through the launch
         * registry (uid -> app-id) and must hold `app-launch` in its INSTALLED
         * manifest. Nothing the caller sends is trusted here. */
        if matches!(req, Request::Catalog) {
            let resp = match portcullis_peer::AppRegistry::load(
                              portcullis_peer::DEFAULT_REGISTRY)
                          .unwrap_or_default()
                          .resolve(uid)
                          .map(|(_user, app)| app.to_string())
            {
                Some(app) if launch::app_may_launch_apps(&app) => {
                    let apps = launch::catalog();
                    eprintln!("portcullisd: catalog -> {app} (uid {uid}): {} app(s)",
                              apps.len());
                    Response::CatalogList { apps }
                }
                Some(app) => {
                    eprintln!("portcullisd: catalog DENIED to {app} (uid {uid}): \
                               no app-launch capability");
                    Response::Error { message:
                        "cap.app_launch.denied: this app may not list installed apps"
                        .into() }
                }
                /* Not in the registry → not a Portcullis-launched app. Default-deny
                 * is the rule for every capability-enforcing service (peer.rs).
                 * Logged like the other two outcomes: a silent refusal here is
                 * indistinguishable, from the daemon's side, from a client that
                 * never called — and the client just sees an empty catalog. */
                None => {
                    eprintln!("portcullisd: catalog DENIED to uid {uid}: \
                               not a launched app (no registry entry)");
                    Response::Error { message:
                        "cap.app_launch.denied: caller is not a launched app".into() }
                }
            };
            write_response(&mut writer, &resp)?;
            continue;
        }
        /* Memory-governor broker verbs are gated on the PEER UID (the caller must
         * be a memory_govern service), not the per-tenant policy `handle` uses —
         * so they're routed here, where serve() has the uid, rather than into
         * handle(). */
        if matches!(req, Request::GovernReap { .. } | Request::GovernSetRctl { .. }) {
            let resp = handle_govern(uid, req);
            write_response(&mut writer, &resp)?;
            continue;
        }
        /* Session-launcher broker verbs — same peer-uid gating shape as the
         * governor verbs (the caller must be a `session_launch` service, i.e.
         * the jailed, non-root _ostiarius). docs/spec/ostiarius-privsep.md. */
        if matches!(req,
            Request::LaunchSessionComponent { .. }
            | Request::VerifyCredential { .. }
            | Request::TeardownSessionComponent { .. })
        {
            let resp = handle_session(uid, req, &shared);
            write_response(&mut writer, &resp)?;
            continue;
        }
        /* ExecInJail relays fds (procdesc + pty master) back to the caller,
         * so — like Launch — it needs the writer directly, not the plain
         * write_response path. Peer-uid + jail_exec cap gated. */
        if matches!(req, Request::ExecInJail { .. }) {
            handle_exec_in_jail(uid, req, &mut writer)?;
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
    /* 0. Already running? Answer before touching anything.
     *
     * This is checked FIRST, ahead of the policy gate and long before any
     * mount, so a duplicate launch cannot disturb the instance that is already
     * up. Previously the request went all the way to jail(8), which failed with
     *     jail: "org_atrium_forum-bar" already exists
     * and a generic non-zero exit — indistinguishable, to a launcher, from a
     * broken app. Clicking a dock icon twice reported an error instead of
     * raising the window that was already there.
     *
     * The uid goes back with the reply because surfaces carry `owner_uid`: it
     * is what lets a jailed launcher find the live window and focus it without
     * the launch registry, which it cannot read. */
    if launch::is_single_instance(&app_id) {
        if let Some(jid) = launch::running_jid(&app_id) {
            let uid = portcullis_peer::uid_for_app(
                portcullis_peer::DEFAULT_REGISTRY, user, &app_id).unwrap_or(0);
            eprintln!("portcullisd: {app_id} already running (jid {jid}, uid {uid}) \
                       — refusing a second instance");
            return write_response(writer, &Response::AlreadyRunning {
                app_id, jid, uid,
            });
        }
    }

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

/// Service-manifest dir + jaild socket for the governor broker path.
const SERVICES_DIR: &str = "/etc/atrium/services.d";
const JAILD_SOCK:   &str = "/var/run/atrium/jaild.sock";

/// Broker a memory-governor request. The caller is a jailed governor running as a
/// non-root system uid, so it cannot reach jaild's root-only socket — portcullisd
/// forwards. INNER gate (here): the peer uid must own a services.d manifest that
/// grants `memory_govern`. OUTER gate (jaild, on forward): the jail must be one
/// jaild created and `[resource_control]` must allow. Both are required.
fn handle_govern(uid: u32, req: Request) -> Response {
    use portcullisd::jaild_client::Client;
    use portcullisd::system_services;

    // 1. inner capability gate: the peer uid must be a memory_govern service.
    let authorized = match system_services::load_dir(std::path::Path::new(SERVICES_DIR)) {
        Ok(outcome) => outcome.manifests.iter().any(|m| {
            m.capabilities.memory_govern
                && m.exec.as_ref().map_or(false, |e| e.uid == uid)
        }),
        Err(e) => return Response::Error { message: format!("services.d load: {e}") },
    };
    if !authorized {
        return Response::Error {
            message: format!(
                "cap.memory_govern.denied: uid {uid} is not a memory_govern service"),
        };
    }

    // 2. translate to the jaild request (the bounded GovSignal -> ReapSignal).
    let jreq = match req {
        Request::GovernReap { jail_name, signal } =>
            jaild::protocol::Request::Reap(jaild::protocol::ReapRequest {
                jail_name,
                signal: match signal {
                    GovSignal::Trim => jaild::protocol::ReapSignal::Trim,
                    GovSignal::Exit => jaild::protocol::ReapSignal::Exit,
                    GovSignal::Kill => jaild::protocol::ReapSignal::Kill,
                },
            }),
        Request::GovernSetRctl { jail_name, memoryuse_mb } =>
            jaild::protocol::Request::SetRctl(jaild::protocol::SetRctlRequest {
                jail_name,
                memoryuse_mb,
            }),
        _ => return Response::Error { message: "not a govern request".into() },
    };

    // 3. forward to jaild; translate the reply into the app protocol.
    let mut client = match Client::connect(JAILD_SOCK) {
        Ok(c) => c,
        Err(e) => return Response::Error { message: format!("jaild connect: {e}") },
    };
    match client.send(&jreq) {
        Ok((jaild::protocol::Response::Ok, _)) => Response::Ok,
        Ok((jaild::protocol::Response::PolicyDenied { rule, detail }, _)) =>
            Response::Error { message: format!("jaild denied [{rule}]: {detail}") },
        Ok((other, _)) => Response::Error { message: format!("jaild: {other:?}") },
        Err(e) => Response::Error { message: format!("jaild send: {e}") },
    }
}

/// Broker an `ExecInJail` request from a jailed, non-root broker client
/// (`_stoad`): exec a shell inside an EXISTING jaild-created jail on a pty
/// (Stoa's jail-target sessions, stoa.md §4.5).
///
/// INNER gate (here): the peer uid must own a services.d manifest granting
/// `jail_exec` (mirrors `handle_govern`'s `memory_govern`); a `want_root`
/// request additionally needs `jail_exec_root`. OUTER bound (jaild, on
/// forward): the target must be a jail jaild itself created, and the shell
/// runs as the jail's own app-uid unless root was granted.
///
/// On success: forwards to jaild, receives `[procdesc, pty_master]`, writes
/// `JailExecStarted`, and relays both fds to the caller over SCM_RIGHTS.
///
/// NOTE (v1): gated on the `jail_exec` capability alone. A per-human
/// ownership check (does *this* user own jail X — via the launch registry)
/// is a documented follow-up; today `_stoad` holds `jail_exec` for the seat.
fn handle_exec_in_jail(uid: u32, req: Request, writer: &mut UnixStream) -> std::io::Result<()> {
    use portcullisd::jaild_client::Client;
    use portcullisd::system_services;

    let (jail_name, path, argv, want_root, cols, rows) = match req {
        Request::ExecInJail { jail_name, path, argv, want_root, cols, rows } => {
            (jail_name, path, argv, want_root, cols, rows)
        }
        _ => {
            return write_response(writer, &Response::Error {
                message: "internal: not an ExecInJail request".into(),
            })
        }
    };

    // 1. capability gate on the peer uid's services.d manifest.
    let manifests = match system_services::load_dir(std::path::Path::new(SERVICES_DIR)) {
        Ok(o) => o.manifests,
        Err(e) => {
            return write_response(writer, &Response::Error {
                message: format!("services.d load: {e}"),
            })
        }
    };
    let svc = manifests
        .iter()
        .find(|m| m.exec.as_ref().map_or(false, |e| e.uid == uid));
    if !svc.map_or(false, |m| m.capabilities.jail_exec) {
        return write_response(writer, &Response::Error {
            message: format!("cap.jail_exec.denied: uid {uid} is not a jail_exec service"),
        });
    }
    if want_root && !svc.map_or(false, |m| m.capabilities.jail_exec_root) {
        return write_response(writer, &Response::Error {
            message: format!("cap.jail_exec_root.denied: uid {uid} may not request a root shell"),
        });
    }

    // 2. forward to jaild (the sole jail_attach caller).
    let jreq = jaild::protocol::Request::ExecInJail(jaild::protocol::ExecInJailRequest {
        name: jail_name.clone(),
        path,
        argv,
        env: Vec::new(),
        want_root,
        cols,
        rows,
    });
    let mut client = match Client::connect(JAILD_SOCK) {
        Ok(c) => c,
        Err(e) => {
            return write_response(writer, &Response::Error {
                message: format!("jaild connect: {e}"),
            })
        }
    };
    let (jresp, fds) = match client.send_recv_fds(&jreq, 2) {
        Ok(v) => v,
        Err(e) => {
            return write_response(writer, &Response::Error {
                message: format!("jaild send: {e}"),
            })
        }
    };

    let close_all = |fds: &[i32]| {
        for &fd in fds {
            // SAFETY: closing fds we received and won't relay.
            unsafe { libc::close(fd) };
        }
    };
    let (pid, juid) = match jresp {
        jaild::protocol::Response::JailExecStarted { pid, uid } => (pid, uid),
        jaild::protocol::Response::PolicyDenied { rule, detail } => {
            close_all(&fds);
            return write_response(writer, &Response::Error {
                message: format!("jaild denied [{rule}]: {detail}"),
            });
        }
        other => {
            close_all(&fds);
            return write_response(writer, &Response::Error {
                message: format!("jaild: {other:?}"),
            });
        }
    };
    if fds.len() != 2 {
        close_all(&fds);
        return write_response(writer, &Response::Error {
            message: format!("jaild returned {} fds, expected 2", fds.len()),
        });
    }

    // 3. relay to the caller: response line, then [procdesc, pty_master].
    write_response(writer, &Response::JailExecStarted { pid, uid: juid })?;
    let res = portcullis_ipc::send_fds(writer, &fds);
    close_all(&fds); // the caller has its own copies now
    res?;
    eprintln!(
        "portcullisd: ExecInJail {jail_name} pid={pid} uid={juid} → relayed [procdesc,master] to peer uid {uid}"
    );
    Ok(())
}

/// Broker a session-launcher request from the jailed, non-root `_ostiarius`.
/// INNER gate: the peer uid must own a services.d manifest granting
/// `session_launch` (mirrors `handle_govern`'s `memory_govern`). OUTER bound:
/// the session-component registry (for launches). docs/spec/ostiarius-privsep.md.
/// Declared session components live here (services.d manifest format, but
/// session-scoped: the exec.uid is a placeholder overridden per-session).
const SESSION_DIR: &str = "/etc/atrium/session.d";

fn handle_session(uid: u32, req: Request, shared: &Mutex<Tenants>) -> Response {
    use portcullisd::system_services;

    let authorized = match system_services::load_dir(std::path::Path::new(SERVICES_DIR)) {
        Ok(outcome) => outcome.manifests.iter().any(|m| {
            m.capabilities.session_launch
                && m.exec.as_ref().map_or(false, |e| e.uid == uid)
        }),
        Err(e) => return Response::Error { message: format!("services.d load: {e}") },
    };
    if !authorized {
        return Response::Error {
            message: format!(
                "cap.session_launch.denied: uid {uid} is not a session-launch service"),
        };
    }

    match req {
        Request::VerifyCredential { user, password } => {
            // Verify against PAM/shadow — portcullisd is root and can read them;
            // the hashes never leave this process (the jailed ostiarius forwards
            // the credential, can't read master.passwd itself). With --features
            // pam this runs the real /etc/pam.d/atrium-login stack; without it,
            // the dev stub (any non-empty credential).
            if user.is_empty() || password.is_empty() {
                return Response::Error { message: "authentication failed".into() };
            }
            #[cfg(feature = "pam")]
            {
                match pam::authenticate("atrium-login", &user, &password) {
                    Ok(()) => Response::CredentialVerified { user },
                    Err(e) => Response::Error { message: e },
                }
            }
            #[cfg(not(feature = "pam"))]
            {
                let _ = &password;
                Response::CredentialVerified { user }
            }
        }
        Request::LaunchSessionComponent { component_id, owner_name } =>
            launch_session_component(&component_id, &owner_name, shared),
        Request::TeardownSessionComponent { jail_name } =>
            teardown_session_component(&jail_name, shared),
        _ => Response::Error { message: "not a session request".into() },
    }
}

/// Registry-bounded session launch. Find `component_id` in the session-component
/// registry (unknown → refused, so `_ostiarius` is confined to the declared set),
/// fill the per-session owner uid, forward `CreateJail` to jaild, and HOLD the
/// returned procdesc in daemon state so the persist=0 jail stays alive.
fn launch_session_component(component_id: &str, owner_name: &str,
                            shared: &Mutex<Tenants>) -> Response {
    use portcullisd::{jaild_client::Client, system_services};
    use std::os::fd::FromRawFd;

    let manifests = match system_services::load_dir(std::path::Path::new(SESSION_DIR)) {
        Ok(o) => o.manifests,
        Err(e) => return Response::Error { message: format!("session.d load: {e}") },
    };
    let manifest = match manifests.into_iter().find(|m| m.name == component_id) {
        Some(m) => m,
        None => return Response::Error { message: format!(
            "session-component.unknown: {component_id:?} not declared in {SESSION_DIR}") },
    };
    let mut create = manifest.to_create_request();
    // Allocate + register the per-session uid HERE (in the TCB): the registry
    // write to root-owned /var/run/atrium/app-registry stays out of jailed
    // _ostiarius, which only supplied owner_name. Reuse the existing binding for
    // this (owner, component) if any, else allocate a fresh 50000+ uid.
    let reg = portcullis_peer::DEFAULT_REGISTRY;
    let owner_uid = portcullis_peer::uid_for_app(reg, owner_name, component_id)
        .unwrap_or_else(|| {
            let u = portcullis_peer::allocate(reg);
            let _ = portcullis_peer::register(reg, u, owner_name, component_id);
            u
        });
    if let Some(exec) = create.exec.as_mut() {
        exec.uid = owner_uid;
        exec.gid = owner_uid;
    }
    let jail_name = create.name.clone();

    let mut client = match Client::connect(JAILD_SOCK) {
        Ok(c) => c,
        Err(e) => return Response::Error { message: format!("jaild connect: {e}") },
    };
    match client.send(&jaild::protocol::Request::CreateJail(create)) {
        Ok((jaild::protocol::Response::JailCreated(r), fd)) => {
            if let Some(raw) = fd {
                // SAFETY: jaild_client just received this procdesc fd via SCM_RIGHTS
                // and handed us ownership. Holding the OwnedFd in daemon state keeps
                // the persist=0 jail alive until teardown drops it.
                let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(raw) };
                shared.lock().unwrap().session_procdescs.insert(jail_name.clone(), owned);
            }
            Response::SessionComponentLaunched { pid: r.pid, jid: r.jid, jail_name }
        }
        Ok((jaild::protocol::Response::PolicyDenied { rule, detail }, _)) =>
            Response::Error { message: format!("jaild denied [{rule}]: {detail}") },
        Ok((other, _)) => Response::Error { message: format!("jaild: {other:?}") },
        Err(e) => Response::Error { message: format!("jaild send: {e}") },
    }
}

/// Tear down a session component: drop the held procdesc (the kernel kills the
/// jailed process + reaps the persist=0 jail), RemoveJail to clear jaild state,
/// then unmount the create-time host-namespace mounts.
fn teardown_session_component(jail_name: &str, shared: &Mutex<Tenants>) -> Response {
    use portcullisd::{jaild_client::Client, system_services, host_mount};

    // 1. Drop the held procdesc → the kernel SIGKILLs the jailed process.
    shared.lock().unwrap().session_procdescs.remove(jail_name);

    // 2. RemoveJail (synchronous: kills any survivor + clears jaild's state record).
    let req = jaild::protocol::Request::RemoveJail {
        jid: None, name: Some(jail_name.to_string()),
    };
    let rm = Client::connect(JAILD_SOCK).and_then(|mut c| c.send(&req));

    // 3. Unmount the create-time mounts. jaild's exec path doesn't record them in
    // state, so RemoveJail can't — they'd leak and the next launch of this
    // component would EDEADLK on the identical nullfs (the same bug fixed in the
    // supervisor's handle_exit). Re-derive from the session.d registry and unmount
    // via the shared host_mount::unmount_jail_dest. After RemoveJail so the jail's
    // procs are gone (no EBUSY).
    if let Ok(o) = system_services::load_dir(std::path::Path::new(SESSION_DIR)) {
        if let Some(m) = o.manifests.into_iter().find(|m| m.name == jail_name) {
            let create = m.to_create_request();
            for mt in &create.mounts {
                if let Err(e) = host_mount::unmount_jail_dest(&create.path, &mt.dest) {
                    eprintln!("portcullisd: teardown unmount {} {}: {e}", create.path, mt.dest);
                }
            }
        }
    }
    match rm {
        Ok(_) => Response::Ok,
        Err(e) => Response::Error { message: format!("teardown RemoveJail: {e}") },
    }
}

fn handle(req: Request, user: &str, shared: &Mutex<Tenants>) -> Response {
    match req {
        Request::Hello { .. } => Response::Error {
            message: "Hello already exchanged".into(),
        },
        Request::Ping => Response::Pong,
        /* Routed in serve(), which has the peer uid this verb is gated on.
         * Reaching here would mean that routing was removed — refuse rather
         * than fall through to a policy path that cannot check the caller. */
        Request::Catalog => Response::Error {
            message: "Catalog must be handled on the peer-uid path".into(),
        },
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
        Request::GovernReap { .. } | Request::GovernSetRctl { .. } => Response::Error {
            /* Caught by serve() before reaching here (routed to handle_govern). */
            message: "internal: governor verbs must go through handle_govern".into(),
        },
        Request::LaunchSessionComponent { .. }
        | Request::VerifyCredential { .. }
        | Request::TeardownSessionComponent { .. } => Response::Error {
            /* Caught by serve() before reaching here (routed to handle_session). */
            message: "internal: session verbs must go through handle_session".into(),
        },
        Request::ExecInJail { .. } => Response::Error {
            /* Caught by serve() before reaching here (routed to
             * handle_exec_in_jail, which relays fds). */
            message: "internal: ExecInJail must go through handle_exec_in_jail".into(),
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
            let shared = Arc::new(Mutex::new(Tenants { cache: HashMap::new(), session_procdescs: HashMap::new() }));
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

#[cfg(test)]
mod owner_tests {
    use super::launch_owner;

    /// A human at the CLI owns their own launches — nothing to inherit.
    #[test]
    fn a_human_owns_their_own_launches() {
        assert_eq!(launch_owner("alice", None), "alice");
    }

    /// The #118 case: the dock (running as its per-app uid) asks for a launch.
    /// The launched app must belong to the human the DOCK belongs to, not to
    /// the dock's robot identity — otherwise the grant authorizing it has to
    /// live in a per-app policy file and no human is on the hook for it.
    #[test]
    fn a_launcher_app_passes_its_own_owner_through() {
        assert_eq!(launch_owner("atrium-app-50001", Some("alice")), "alice");
        assert_ne!(launch_owner("atrium-app-50001", Some("alice")), "atrium-app-50001");
    }

    /// Transitivity: an app launched BY a launcher already carries the human as
    /// its registry owner, so a deeper chain still resolves to that human.
    #[test]
    fn inheritance_is_transitive_through_a_chain() {
        let second = launch_owner("atrium-app-50001", Some("alice"));      // dock -> app
        let third  = launch_owner("atrium-app-50002", Some(&second));      // app  -> app
        assert_eq!(third, "alice");
    }
}
