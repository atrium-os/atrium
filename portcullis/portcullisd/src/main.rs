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
use std::os::unix::io::{AsRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};

/// ★ ONE LIMITER FOR THE WHOLE DAEMON, process-wide rather than per
/// connection: the point is to bound what all clients together can create,
/// and a limiter that a client got a fresh copy of by reconnecting would
/// bound nothing at all.
static LIMITER: std::sync::LazyLock<Arc<Mutex<portcullisd::ratelimit::Limiter>>> =
    std::sync::LazyLock::new(|| Arc::new(Mutex::new(portcullisd::ratelimit::Limiter::default())));
use std::thread;

use portcullis_ipc::{
    read_request, write_response, GovSignal, Request, Response, PROTO_VERSION,
    SERVICE_SOCKET_PATH, SOCKET_PATH,
};
use portcullis_policy::{decide, hash_manifest, now_iso8601, Grant, Policy, SYSTEM_PATH};

mod calendar;
mod console;
mod dev;
mod egress;
mod empty;
mod ide;
mod host_dns;
mod launch;
mod ssh_agent;
mod workspace;
use portcullisd::manifest_trust;
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
    // The registries proxy's helper (egress.rs): bind in a workspace's
    // jail and hand the socket back; never the daemon.
    if args.get(1).map(String::as_str) == Some(egress::BIND_ARG) {
        egress::bind_in_jail_main(args.get(2).map(String::as_str).unwrap_or(""), args.get(3).map(String::as_str));
    }
    // A debug run doesn't outlive portcullisd: no stale "debugging" tag.
    dev::clear_debugging();
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
    // ★ Reclaim one-shot instances a previous daemon was stopped in the
    // middle of tearing down (portcullis_oneshot::reclaim_abandoned). Only
    // after the bind: a daemon that cannot start must not sweep.
    for name in portcullis_oneshot::reclaim_abandoned(std::time::Duration::from_secs(60)) {
        eprintln!("portcullisd: reclaimed abandoned one-shot instance {name}");
    }
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

    /* ★ The worker-call socket (portcullis_ipc::WORKERS_SOCKET_PATH): what
     * an app with [[workers]] gets in its jail. Same daemon, same peer-uid
     * identity, but only Hello and WorkerCall are answered on it. */
    {
        let wpath = Path::new(portcullis_ipc::WORKERS_SOCKET_PATH);
        let _ = std::fs::remove_file(wpath);
        if let Some(d) = wpath.parent() { let _ = std::fs::create_dir_all(d); }
        match UnixListener::bind(wpath) {
            Ok(l) => {
                let _ = std::fs::set_permissions(wpath, std::fs::Permissions::from_mode(0o666));
                let shared = Arc::clone(&shared);
                thread::spawn(move || {
                    for conn in l.incoming().flatten() {
                        let Some(slot) = ConnSlot::take(&conn) else { continue };
                        let s = Arc::clone(&shared);
                        thread::spawn(move || {
                            let _slot = slot;
                            if let Err(e) = serve(conn, s, true) {
                                eprintln!("portcullisd: worker connection: {e}");
                            }
                        });
                    }
                });
                eprintln!("portcullisd: worker calls on {}", wpath.display());
            }
            Err(e) => eprintln!("portcullisd: bind {}: {e} (no [[workers]] calls)", wpath.display()),
        }
    }

    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let Some(slot) = ConnSlot::take(&stream) else { continue };
                let s = Arc::clone(&shared);
                thread::spawn(move || {
                    let _slot = slot;
                    if let Err(e) = serve(stream, s, false) {
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

/// The launch registry, re-read for a decision made mid-connection.
///
/// `serve` refuses a connection whose registry cannot be read, so this
/// failing means the file became unreadable while portcullisd runs. Every
/// identity decision would then be made blind, and many of them treat "not
/// an app" as "a person" (workspace exec, link and window requests), so
/// portcullisd stops rather than answer with an empty registry (review
/// 2026-11-24, H4). The file is root's and local: this does not happen in
/// normal operation.
fn load_registry() -> portcullis_peer::AppRegistry {
    match portcullis_peer::AppRegistry::load(portcullis_peer::DEFAULT_REGISTRY) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("portcullisd: FATAL: launch registry {} unreadable: {e}; stopping rather than decide without it",
                portcullis_peer::DEFAULT_REGISTRY);
            std::process::exit(70);
        }
    }
}

/// Connections one non-root uid may hold open at once, across both
/// sockets. Each is a thread here; without a cap any local process, a jailed
/// app included, could open connections until the broker runs out of
/// threads. Long-lived holders (the dock, the
/// session bootstrap) use a handful.
const MAX_CONNS_PER_UID: usize = 64;

static CONNS_PER_UID: Mutex<Option<HashMap<u32, usize>>> = Mutex::new(None);

/// One counted connection; dropping it gives the slot back.
struct ConnSlot(u32);

impl ConnSlot {
    /// A slot for this connection's peer, or `None` (connection dropped,
    /// logged) when the peer is unknown or already holds the maximum. Root
    /// is not counted.
    fn take(stream: &UnixStream) -> Option<ConnSlot> {
        let Ok((uid, _)) = peer_eid(stream) else { return None };
        let mut g = CONNS_PER_UID.lock().unwrap_or_else(|p| p.into_inner());
        let n = g.get_or_insert_with(HashMap::new).entry(uid).or_insert(0);
        if uid != 0 && *n >= MAX_CONNS_PER_UID {
            eprintln!("portcullisd: uid {uid} already holds {MAX_CONNS_PER_UID} connections; refusing another");
            return None;
        }
        *n += 1;
        Some(ConnSlot(uid))
    }
}

impl Drop for ConnSlot {
    fn drop(&mut self) {
        let mut g = CONNS_PER_UID.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(n) = g.as_mut().and_then(|m| m.get_mut(&self.0)) {
            *n = n.saturating_sub(1);
        }
    }
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
            publish_colours(user, &policy);
            self.cache.insert(user.to_string(), TenantPolicy {
                policy, policy_path: path,
            });
        }
        Ok(self.cache.get_mut(user).unwrap())
    }
}

/// Tell frescod `user`'s persona colours (personas.md §7).
fn publish_colours(user: &str, policy: &Policy) {
    let (named, _) = portcullisd::crossing::listing(policy);
    if let Err(e) = portcullisd::persona_colours::publish(user, &named) {
        eprintln!("portcullisd: persona colours for {user} not written: {e}");
    }
    // And each persona's VPN, for the route in the chip (vpn.md §7.4).
    let routes: Vec<(String, String)> = policy.personas.iter().filter_map(|(n, d)| Some((n.clone(), d.vpn.clone()?))).collect();
    if let Err(e) = portcullisd::persona_colours::publish_routes(user, &routes) {
        eprintln!("portcullisd: persona routes for {user} not written: {e}");
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

/// One connection. The worker-call socket takes WorkerCall and the
/// gesture-gated NewWorkbook request from Sheets.
fn serve(stream: UnixStream, shared: Arc<Mutex<Tenants>>, workers_only: bool) -> std::io::Result<()> {
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
    /* Who the peer is: an app or a person. An unreadable registry would
     * make every app look like a person, so refuse the connection instead
     *. */
    let registry = portcullis_peer::AppRegistry::load(portcullis_peer::DEFAULT_REGISTRY)
        .map_err(|e| std::io::Error::other(format!("launch registry unreadable, refusing uid {uid}: {e}")))?;
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
        if let Request::NameWorkbookRecovery { id, name } = req {
            let response = match launch::name_workbook_recovery(uid, &registry, id, name) {
                Ok(()) => Response::Ok,
                Err(e) => Response::Error { message: e.to_string() },
            };
            write_response(&mut writer, &response)?;
            continue;
        }
        if let Request::NewWorkbook { activation } | Request::RecoverWorkbook { activation }
            | Request::RecoverSelectedWorkbook { activation, .. } = req {
            let action = match req {
                Request::RecoverSelectedWorkbook { id, .. } => launch::WorkbookAction::Select(id),
                Request::RecoverWorkbook { .. } => launch::WorkbookAction::Recover,
                _ => launch::WorkbookAction::New,
            };
            let result = (|| {
                if !registry.resolve(uid).is_some_and(|(_, app)| app == portcullis_policy::opens::SHEETS) {
                    return Err("Only Sheets can request a new workbook".into());
                }
                may_run_unasked(portcullis_policy::opens::SHEETS, &owner, &shared)?;
                portcullisd::opens::spend_press(&portcullisd::opens::activation_socket(), activation, uid)?;
                if action != launch::WorkbookAction::New && registry.document_slot(uid).is_some() {
                    return Err("Choose Recover in the Sheets launcher".into());
                }
                launch::new_workbook(&owner, registry.persona(uid).flatten(), action, registry.publisher(uid).flatten())
                    .map_err(|e| e.to_string())
            })();
            let response = match result {
                Ok(books) if !books.is_empty() => Response::PendingWorkbooks { books },
                Ok(_) => Response::Opened { app: "Sheets".into(), name: "Untitled".into() },
                Err(message) => Response::Error { message },
            };
            write_response(&mut writer, &response)?;
            continue;
        }
        if workers_only && !matches!(req, Request::WorkerCall { .. }) {
            write_response(&mut writer, &Response::Error {
                message: "request is unavailable on the worker-call socket".into() })?;
            return Ok(());
        }
        if let Request::ExecInstance { app_id, instance, tmpfs_mb, memory_mb, cpu_pct, require_memory_limit, serves, persona: into } = req {
            /* ★ The fetcher only through OpenFetcher: started here, it would
             * skip the caller's `net-fetch` check, and the system grant of the
             * fetcher's own network would make it anyone's raw fetch. */
            if app_id == portcullis_ipc::NUNTIUS_APP {
                write_response(&mut writer, &Response::LaunchFailed { stage: "policy".into(),
                    message: format!("{app_id} is started only by OpenFetcher, for an app holding net-fetch") })?;
                continue;
            }
            // Likewise the CDM host: started here it would get no library and
            // no store of the caller's, and run as the CDM's uid for anyone.
            if app_id == portcullis_ipc::HOSPES_CDM_APP {
                write_response(&mut writer, &Response::LaunchFailed { stage: "policy".into(),
                    message: format!("{app_id} is started only by OpenCdm, for an app holding protected-media") })?;
                continue;
            }
            let limits = (memory_mb, cpu_pct, require_memory_limit);
            let mut rule = Policy::load_system(std::path::Path::new(SYSTEM_PATH)).ok()
                .and_then(|p| p.worker_rule_for(&app_id, &owner, registry.resolve(uid).map(|(_, a)| a)).cloned());
            /* ★ A Limen provider from its manifest (app-composition.md §5.1,
             * dev-tools-plan.md M6): an installed app whose `[roles]
             * implements` names a role, started by limend for a parent it
             * serves, runs per instance (a uid of that parent's person and
             * persona), as a system `[workers."id"] callers = ["_limen"],
             * per_instance = true` rule would have it. Only for limend. */
            if rule.is_none() && owner == LIMEN_USER && serves.is_some()
                && launch::installed_manifest(&app_id).and_then(|m| m.roles).is_some_and(|r| !r.implements.is_empty()) {
                rule = Some(portcullis_policy::WorkerRule {
                    callers: vec![LIMEN_USER.to_string()], caller_apps: Vec::new(), run_as: String::new(),
                    per_instance: true, concurrent: Some(PROVIDERS_PER_APP),
                });
            }
            /* ★ An app with no system rule starts only its own id, or an id
             * under it (the Navigator's `org.atrium.navigator.converter`),
             * and only as itself: not another app's bundle as its own uid,
             * which would let it run any installed program with its grants.
             * Its own parsers are `[[workers]]`, through WorkerCall. A
             * person's own processes (the CLI) keep this lane as before. */
            if rule.is_none() {
                if let Some((_, caller)) = registry.resolve(uid) {
                    if app_id != caller && !app_id.starts_with(&format!("{caller}.")) {
                        eprintln!("portcullisd: ExecInstance of {app_id} DENIED to {caller} (uid {uid}): not its own");
                        write_response(&mut writer, &Response::LaunchFailed { stage: "policy".into(),
                            message: format!("{caller} may start only its own id or one under it; \
                                              its parsers are [[workers]] (WorkerCall)") })?;
                        continue;
                    }
                }
            }
            /* ★ A per-instance worker (personas.md §3): its uid is the
             * served instance's, one per (person, worker app, persona). The
             * served instance is the caller itself, or for a system caller
             * the rule names by user (limend) the uid it says it serves (the
             * embed's parent: limend has it from that parent's peer cred).
             * Neither resolving in the launch registry: refused, never the
             * shared `run_as`. */
            /* ★ A share across personas (share-sheet.md §7): limend, for a
             * provider it starts for an instance, may ask for it in another
             * of that instance's person's personas, and only while the
             * person's `share` switch from the instance's persona to that
             * one is on (Settings, Between personas, turned on only after
             * Sigillum confirmed it's them). Anyone else: refused. */
            if into.is_some() && !(owner == LIMEN_USER && serves.is_some() && rule.as_ref().is_some_and(|r| r.per_instance)) {
                eprintln!("portcullisd: ExecInstance of {app_id} in persona {into:?} DENIED to uid {uid}: only limend, for a provider");
                write_response(&mut writer, &Response::LaunchFailed { stage: "policy".into(),
                    message: "only limend starts a provider in another persona".into() })?;
                continue;
            }
            let mut as_uid = None;
            // The caller's own persona; a per-instance worker's, below.
            let mut domain = caller_domain(uid);
            if let Some(r) = rule.as_mut().filter(|r| r.per_instance) {
                let served = match serves {
                    Some(s) if r.callers.iter().any(|c| c == &owner) => s,
                    _ => uid,
                };
                let reg = load_registry();
                let inst = reg.resolve(served).map(|(o, a)| (o.to_string(), a.to_string(), reg.persona(served).flatten().map(String::from)));
                let Some((person, served_app, mut persona)) = inst else {
                    write_response(&mut writer, &Response::LaunchFailed { stage: "worker".into(),
                        message: format!("{app_id} serves an app instance, and uid {served} is none") })?;
                    continue;
                };
                if let Some(to) = into.as_deref() {
                    let to = (to != portcullis_policy::PERSONAL).then_some(to);
                    let pol = Policy::load(&Policy::user_path(&person)).unwrap_or_default();
                    let shown = |p: Option<&str>| p.unwrap_or(portcullis_policy::PERSONAL).to_string();
                    if to == persona.as_deref() || !pol.has_persona(to) || !pol.may_cross(persona.as_deref(), to, "share") {
                        eprintln!("portcullisd: {app_id} for {person}'s {served_app} in {} REFUSED: sharing from {} to {} is off",
                                  shown(to), shown(persona.as_deref()), shown(to));
                        write_response(&mut writer, &Response::LaunchFailed { stage: "policy".into(),
                            message: format!("cap.share.crossing: sharing from {} to {} is off (Settings, Apps, Between personas)",
                                             shown(persona.as_deref()), shown(to)) })?;
                        continue;
                    }
                    eprintln!("portcullisd: {app_id} for {person}'s {served_app}: a share from {} to {}", shown(persona.as_deref()), shown(to));
                    persona = to.map(str::to_string);
                }
                domain = Some((person.clone(), persona.clone()));
                // One allocation at a time: two pages at once must not both
                // allocate a uid for the same instance.
                static WORKER_UIDS: Mutex<()> = Mutex::new(());
                let held = WORKER_UIDS.lock().unwrap_or_else(|e| e.into_inner());
                let got = launch::resolve_instance_uid(&person, &app_id, None, persona.as_deref());
                drop(held);
                match got {
                    Ok((wuid, name)) => {
                        eprintln!("portcullisd: {app_id} for {person}'s {served_app}{} runs as {name}",
                                  persona.as_deref().map(|p| format!(" ({p})")).unwrap_or_default());
                        r.run_as = name;
                        as_uid = Some(wuid);
                    }
                    Err(e) => {
                        write_response(&mut writer, &Response::LaunchFailed { stage: "worker".into(),
                            message: format!("{app_id}'s uid for {person}: {e:?}") })?;
                        continue;
                    }
                }
            }
            // Asked (limend), the answer says whom the worker runs as.
            let as_uid = if serves.is_some() {
                as_uid.or_else(|| rule.as_ref().and_then(|r| uid_of_user(&r.run_as))).or(Some(uid))
            } else { None };
            handle_exec_instance(app_id, instance, tmpfs_mb, limits, rule, Vec::new(), &owner, &shared,
                                 &LIMITER, &mut writer, &reader, None, as_uid, None, domain, false)?;
            continue;
        }
        /* ★ WorkerCall (app-composition.md §4, dev-tools-plan.md M5): one
         * of the caller's own `[[workers]]`, named in its INSTALLED manifest,
         * found by the PEER UID through the launch registry. It runs in a
         * one-shot jail of its own with no capability at all, as a uid of
         * its own per (person, app, worker, persona): never the caller's,
         * so a parser that is taken over holds nothing of the core's. */
        if let Request::WorkerCall { worker } = &req {
            let reg = load_registry();
            let refuse = |why: String| Response::LaunchFailed { stage: "worker".into(), message: why };
            let Some((person, app, persona)) = reg.resolve(uid)
                .map(|(o, a)| (o.to_string(), a.to_string(), reg.persona(uid).flatten().map(String::from))) else {
                eprintln!("portcullisd: worker {worker:?} DENIED to uid {uid}: not a launched app");
                write_response(&mut writer, &refuse("only an app calls its workers: uid is not a launched app".into()))?;
                continue;
            };
            if app.starts_with("worker:") {
                write_response(&mut writer, &refuse("a worker calls no workers".into()))?;
                continue;
            }
            let Some(m) = launch::installed_manifest(&app) else {
                write_response(&mut writer, &refuse(format!("{app} is not installed")))?;
                continue;
            };
            let Some(w) = m.worker(worker) else {
                write_response(&mut writer, &refuse(format!("{app} has no worker {worker:?} in its manifest")))?;
                continue;
            };
            if w.isolation != portcullis_toml::WorkerIsolation::Jail {
                write_response(&mut writer, &refuse(format!("worker {worker:?} is isolation = capsicum: the SDK starts it in the core's own jail")))?;
                continue;
            }
            static WORKER_CALL_UIDS: Mutex<()> = Mutex::new(());
            let held = WORKER_CALL_UIDS.lock().unwrap_or_else(|e| e.into_inner());
            let got = launch::resolve_instance_uid(&person, &format!("worker:{app}:{worker}"), None, persona.as_deref());
            drop(held);
            let run_as = match got {
                Ok((_, name)) => name,
                Err(e) => {
                    write_response(&mut writer, &refuse(format!("worker {worker:?}'s uid for {person}: {e:?}")))?;
                    continue;
                }
            };
            let instance = worker_instance(worker, uid);
            eprintln!("portcullisd: worker {app}/{worker} for {person}{} runs as {run_as} ({instance})",
                      persona.as_deref().map(|p| format!(" ({p})")).unwrap_or_default());
            let rule = portcullis_policy::WorkerRule {
                callers: Vec::new(), caller_apps: vec![app.clone()], run_as, per_instance: false,
                concurrent: Some(WORKER_CALLS_PER_APP),
            };
            handle_exec_instance(app, Some(instance), 32, (None, None, false), Some(rule), Vec::new(),
                                 &person, &shared, &LIMITER, &mut writer, &reader, None, None, Some(worker.clone()),
                                 Some((person.clone(), persona.clone())), false)?;
            continue;
        }
        /* ★ OpenFetcher (docs/spec/nuntius.md §2): a Nuntius of the caller's
         * own. Gated like Catalog, on the PEER UID through the launch
         * registry: the caller's INSTALLED manifest must hold `net-fetch`.
         * The fetcher always runs as the system worker rule's uid for
         * NUNTIUS_APP, never as the caller; without that rule it is refused. */
        if matches!(req, Request::OpenFetcher) {
            let app = portcullis_peer::AppRegistry::load(portcullis_peer::DEFAULT_REGISTRY)
                .unwrap_or_default().resolve(uid).map(|(_, app)| app.to_string());
            let refuse = |why: String| Response::LaunchFailed { stage: "net-fetch".into(), message: why };
            let Some(app) = app else {
                eprintln!("portcullisd: fetcher DENIED to uid {uid}: not a launched app");
                write_response(&mut writer, &refuse("cap.net_fetch.denied: caller is not a launched app".into()))?;
                continue;
            };
            if !launch::app_may_fetch(&app) {
                eprintln!("portcullisd: fetcher DENIED to {app} (uid {uid}): no net-fetch capability");
                write_response(&mut writer, &refuse("cap.net_fetch.denied: this app's manifest does not hold net-fetch".into()))?;
                continue;
            }
            // ★ The app's site list (nuntius.md §4), from its INSTALLED
            // manifest, checked here so a bad list is the caller's error
            // and not a fetcher that dies at start.
            let sites = match launch::app_fetch_sites(&app) {
                Ok(s) => s,
                Err(why) => {
                    eprintln!("portcullisd: fetcher for {app} REFUSED: {why}");
                    write_response(&mut writer, &refuse(format!("net-fetch-sites: {why}")))?;
                    continue;
                }
            };
            let extra_env: Vec<(String, String)> = sites.map(|s| (nuntius::sites::SITES_ENV.to_string(), s)).into_iter().collect();
            let Some(rule) = Policy::load_system(std::path::Path::new(SYSTEM_PATH)).ok()
                .and_then(|p| p.workers.get(portcullis_ipc::NUNTIUS_APP).cloned()) else {
                eprintln!("portcullisd: fetcher for {app} REFUSED: no system worker rule for {}", portcullis_ipc::NUNTIUS_APP);
                write_response(&mut writer, &refuse(format!("no system worker rule for {}: the fetcher never runs as its caller", portcullis_ipc::NUNTIUS_APP)))?;
                continue;
            };
            let instance = fetcher_instance(uid);
            eprintln!("portcullisd: fetcher {instance} for {app} (uid {uid})");
            // A fetcher needs no scratch space beyond its own small tmpfs.
            handle_exec_instance(portcullis_ipc::NUNTIUS_APP.to_string(), Some(instance), 16, (None, None, true), Some(rule), extra_env,
                                 &owner, &shared, &LIMITER, &mut writer, &reader, None, None, None, caller_domain(uid), true)?;
            continue;
        }
        /* ★ OpenPortalFetcher (atrium-net.md §8.1): a captive portal's
         * sign-in window. Gated on the PEER UID like OpenFetcher, on the
         * restricted `network-portal` capability. WE ask atrium-net (root
         * only `network-portal` socket) for the network's DNS servers; the
         * caller names nothing. The fetcher gets them and the private-network
         * grant in its environment, and atrium-net watches (5 s probes) for as
         * long as it runs. */
        if matches!(req, Request::OpenPortalFetcher) {
            let app = portcullis_peer::AppRegistry::load(portcullis_peer::DEFAULT_REGISTRY)
                .unwrap_or_default().resolve(uid).map(|(_, app)| app.to_string());
            let refuse = |why: String| Response::LaunchFailed { stage: "network-portal".into(), message: why };
            let Some(app) = app.filter(|a| launch::app_may_open_portal(a)) else {
                eprintln!("portcullisd: portal fetcher DENIED to uid {uid}: no network-portal capability");
                write_response(&mut writer, &refuse("cap.network_portal.denied: the caller's manifest does not hold network-portal".into()))?;
                continue;
            };
            // Only the sign-in window's own instance (PortalWindow started
            // it): the person's everyday windows never reach private
            // addresses or the network's resolvers.
            if !is_portal_instance(uid) {
                eprintln!("portcullisd: portal fetcher DENIED to {app} (uid {uid}): not its sign-in window");
                write_response(&mut writer, &refuse("cap.network_portal.denied: only the sign-in window's instance".into()))?;
                continue;
            }
            let info = portcullisd::portal::info();
            let (dns, iface) = match info {
                Ok(i) if !i.dns.is_empty() => (i.dns.join(","), i.iface),
                Ok(i) => {
                    write_response(&mut writer, &refuse(format!("{} has no DNS servers of its own", i.iface)))?;
                    continue;
                }
                Err(why) => {
                    eprintln!("portcullisd: portal fetcher for {app}: {why}");
                    write_response(&mut writer, &refuse(why))?;
                    continue;
                }
            };
            let Some(rule) = Policy::load_system(std::path::Path::new(SYSTEM_PATH)).ok()
                .and_then(|p| p.workers.get(portcullis_ipc::NUNTIUS_APP).cloned()) else {
                write_response(&mut writer, &refuse(format!("no system worker rule for {}", portcullis_ipc::NUNTIUS_APP)))?;
                continue;
            };
            let extra_env = vec![
                ("ATRIUM_NUNTIUS_RESOLVERS".to_string(), dns.clone()),
                // Workers' environments carry ATRIUM_ keys only.
                ("ATRIUM_NUNTIUS_PRIVATE_NETWORK".to_string(), "1".to_string()),
                // The LAN, but never the machine: its jail cannot see these.
                ("ATRIUM_NUNTIUS_OWN".to_string(), portcullisd::portal::host_addresses()),
            ];
            let instance = fetcher_instance(uid);
            eprintln!("portcullisd: portal fetcher {instance} for {app} (uid {uid}) on {iface}, DNS {dns}");
            portcullisd::portal::watch(true);
            // The sign-in page is the network's own: never in a tunnel.
            let r = handle_exec_instance(portcullis_ipc::NUNTIUS_APP.to_string(), Some(instance), 16, (None, None, true), Some(rule), extra_env,
                                         &owner, &shared, &LIMITER, &mut writer, &reader, None, None, None, None, false);
            portcullisd::portal::watch(false);
            r?;
            continue;
        }
        /* ★ OpenCdm (docs/spec/hospes.md §3, §8.4): a content decryption
         * module of the caller's own. Gated like OpenFetcher, on the PEER UID:
         * the caller's INSTALLED manifest must hold `protected-media`. The
         * host always runs as the system worker rule's uid for HOSPES_CDM_APP.
         * WE choose the library (from the system's CDM install, by key
         * system) and the store (per caller app, key system and origin);
         * the caller names only the key system and the origin. */
        if let Request::OpenCdm { key_system, origin } = &req {
            let app = portcullis_peer::AppRegistry::load(portcullis_peer::DEFAULT_REGISTRY)
                .unwrap_or_default().resolve(uid).map(|(_, app)| app.to_string());
            let refuse = |why: String| Response::LaunchFailed { stage: "protected-media".into(), message: why };
            let Some(app) = app else {
                eprintln!("portcullisd: CDM DENIED to uid {uid}: not a launched app");
                write_response(&mut writer, &refuse("cap.protected_media.denied: caller is not a launched app".into()))?;
                continue;
            };
            if !launch::app_may_play_protected(&app) {
                eprintln!("portcullisd: CDM DENIED to {app} (uid {uid}): no protected-media capability");
                write_response(&mut writer, &refuse("cap.protected_media.denied: this app's manifest does not hold protected-media".into()))?;
                continue;
            }
            let Some(rule) = Policy::load_system(std::path::Path::new(SYSTEM_PATH)).ok()
                .and_then(|p| p.workers.get(portcullis_ipc::HOSPES_CDM_APP).cloned()) else {
                eprintln!("portcullisd: CDM for {app} REFUSED: no system worker rule for {}", portcullis_ipc::HOSPES_CDM_APP);
                write_response(&mut writer, &refuse(format!("no system worker rule for {}: the CDM never runs as its caller", portcullis_ipc::HOSPES_CDM_APP)))?;
                continue;
            };
            let (lib, store) = match portcullisd::cdm::open(&app, key_system, origin, &rule.run_as) {
                Ok(v) => v,
                Err(why) => {
                    eprintln!("portcullisd: CDM for {app} REFUSED: {why}");
                    write_response(&mut writer, &refuse(why))?;
                    continue;
                }
            };
            let line = format!("{key_system} {}\n", portcullisd::cdm::MEM_MIB);
            let prepare = move |[ctrl, data, err]: [OwnedFd; 3]| -> std::io::Result<[OwnedFd; 3]> {
                let (ours, theirs) = UnixStream::pair()?;
                portcullis_ipc::send_fds_with(&ours, &[ctrl.as_raw_fd(), data.as_raw_fd(), lib.as_raw_fd(), store.as_raw_fd()], line.as_bytes())?;
                let out = theirs.try_clone()?;
                Ok([OwnedFd::from(theirs), OwnedFd::from(out), err])
            };
            let instance = fetcher_instance(uid);
            eprintln!("portcullisd: CDM {instance} ({key_system}) for {app} (uid {uid})");
            handle_exec_instance(portcullis_ipc::HOSPES_CDM_APP.to_string(), Some(instance), 16, (None, None, true), Some(rule), Vec::new(),
                                 &owner, &shared, &LIMITER, &mut writer, &reader, Some(&prepare), None, None, caller_domain(uid), false)?;
            continue;
        }
        /* ★ Opening files (opening-files.md §6.2): gated on the PEER UID, as
         * Catalog is: only the session's file manager, by its installed
         * manifest. OpenFile also takes a descriptor, so it needs the
         * connection itself. */
        if matches!(req, Request::OpenApps { .. } | Request::OpenFile { .. } | Request::WorkbookVersion { .. } | Request::WorkbookBookmark { .. }) {
            handle_open(uid, &owner, req, &shared, &mut writer, &reader)?;
            continue;
        }
        /* ★ Attachments (mail.md §8.2): from a mail app, by its installed
         * manifest's `open-attachments`; a sealed memory file follows. */
        if let Request::CalendarUpdate = req {
            handle_calendar_update(uid, &owner, &shared, &mut writer)?;
            return Ok(());
        }
        if let Request::CalendarInvitation { activation } = req {
            handle_calendar_invitation(uid, &owner, activation, &shared, &mut writer)?;
            continue;
        }
        if let Request::OpenAttachment { name, activation, app } = req {
            handle_open_attachment(uid, &owner, &name, activation, app.as_deref(), &shared, &mut writer, &reader)?;
            continue;
        }
        /* ★ Downloads (personas.md §4.1): from the Navigator, into its
         * persona's Downloads; a sealed memory file follows. */
        if let Request::OpenDownload { name } = req {
            handle_open_download(uid, &owner, &name, &shared, &mut writer, &reader)?;
            continue;
        }
        if let Request::SaveDownload { name, origin } = req {
            handle_save_download(uid, &owner, &name, &origin, &mut writer, &reader)?;
            continue;
        }
        /* ★ Opening links (opening-links.md §5): only from Monstrator, by
         * its system user; the person is the asking app's. */
        if matches!(req, Request::LinkGrant { .. } | Request::DeliverLink { .. }) {
            let resp = handle_link(&user, req, &shared);
            write_response(&mut writer, &resp)?;
            continue;
        }
        /* ★ Workspaces (homes.md §5): a launcher, or the person. */
        if let Request::WorkspaceRun { name, project, activation } = req {
            let resp = handle_run(uid, &owner, &name, &project, activation, false, &shared);
            write_response(&mut writer, &resp)?;
            continue;
        }
        /* ★ Under the debugger (dev-tools-plan.md M10): as Run. */
        if let Request::WorkspaceDebug { name, project, activation } = req {
            let resp = handle_run(uid, &owner, &name, &project, activation, true, &shared);
            write_response(&mut writer, &resp)?;
            continue;
        }
        if let Request::WorkspaceCores { name } = req {
            let resp = match run_caller(uid, &owner, &name) {
                Ok(_) => match dev::take_cores(&owner, &name) {
                    Ok(files) => Response::WorkspaceCores { files },
                    Err(message) => Response::Error { message },
                },
                Err(message) => Response::Error { message },
            };
            write_response(&mut writer, &resp)?;
            continue;
        }
        /* ★ An IDE (`workspace-exec`, homes.md W6): picks with a press, and
         * runs commands only in what was picked. */
        if let Request::WorkspacePick { name, activation } = req {
            let resp = handle_pick(uid, &owner, &name, activation);
            write_response(&mut writer, &resp)?;
            continue;
        }
        if let Request::WorkspaceExec { name, argv, cwd } = req {
            handle_ws_exec(uid, &owner, &name, &argv, &cwd, &mut writer)?;
            continue;
        }
        /* ★ `pkg` as the workspace jail's root (dev-tools-plan.md M2): a
         * press in the Terminal, the Workspaces app or an IDE, or the
         * person's own CLI. */
        if let Request::WorkspaceAdmin { name, argv, activation } = req {
            handle_ws_admin(uid, &owner, &name, &argv, activation, &mut writer)?;
            continue;
        }
        if matches!(req, Request::Workspaces | Request::WorkspaceNew { .. } | Request::WorkspaceSet { .. } | Request::WorkspaceStop { .. } | Request::WorkspaceDelete { .. } | Request::WorkspaceDuplicate { .. }) {
            let resp = handle_workspace(uid, &owner, req, &shared);
            write_response(&mut writer, &resp)?;
            continue;
        }
        if matches!(req, Request::WorkspaceCredentials { .. } | Request::SetWorkspaceCredential { .. }) {
            let resp = handle_workspace_credentials(uid, req);
            write_response(&mut writer, &resp)?;
            continue;
        }
        /* ★ App consoles (homes.md §4): gated on the PEER UID, a launcher. */
        if let Request::Console { app_id, activation, tab_of, new_window } = req {
            let resp = handle_console(uid, &owner, &app_id, activation, tab_of, new_window, &shared);
            write_response(&mut writer, &resp)?;
            continue;
        }
        /* ★ Settings' Apps page (curia.md §13): gated on the PEER UID, the
         * settings app by its installed manifest (`app-grants`), and about
         * the person it belongs to (the registry's owner), never the
         * caller's own uid. Allow also needs a press frescod confirms. */
        if let Request::SetCrossing { from, to, kind, on, activation } = req {
            let resp = handle_set_crossing(uid, &from, &to, &kind, on, activation, &shared, &portcullisd::crossing::Sigillum::default());
            write_response(&mut writer, &resp)?;
            continue;
        }
        if matches!(req, Request::Access | Request::RemoveAccess { .. } | Request::ApproveAccess { .. } | Request::Links | Request::SetLinkDefault { .. } | Request::ShareTargets | Request::SetShareTargets { .. } | Request::Crossings | Request::PersonaVpns) {
            let resp = handle_access(uid, req, &shared);
            write_response(&mut writer, &resp)?;
            continue;
        }
        /* ★ A New window in another persona (navigator-ui.md §2.4): the
         * caller's own app, by the PEER UID, with a press. */
        /* ★ The captive portal's sign-in window (atrium-net.md §8.1 S3). */
        if let Request::PortalWindow { activation } = req {
            let resp = handle_portal_window(uid, &owner, activation, &shared);
            write_response(&mut writer, &resp)?;
            continue;
        }
        if matches!(req, Request::PortalStatus) {
            write_response(&mut writer, &handle_portal_status(uid))?;
            continue;
        }
        /* ★ A corporate VPN's sign-in window (vpn.md §9.3, SAML). */
        if let Request::VpnSignInWindow { profile, start_url, final_url, activation } = req {
            let resp = handle_vpn_sign_in_window(uid, &owner, VpnSignIn { profile, start_url, final_url }, activation, &shared);
            write_response(&mut writer, &resp)?;
            continue;
        }
        if matches!(req, Request::VpnSignInStatus) {
            write_response(&mut writer, &handle_vpn_sign_in_status(uid))?;
            continue;
        }
        if let Request::NewWindowIn { persona, activation } = req {
            let resp = handle_new_window_in(uid, &persona, activation, &shared);
            write_response(&mut writer, &resp)?;
            continue;
        }

        /* ★ Settings' Personas page (personas.md §2, §4.1). */
        if matches!(req, Request::AddPersona { .. } | Request::SetPersonaColour { .. } | Request::RemovePersona { .. } | Request::SetPersonaVpn { .. }) {
            let resp = handle_persona_change(uid, req, &shared, &portcullisd::crossing::Sigillum::default());
            write_response(&mut writer, &resp)?;
            continue;
        }
        /* ★ The launch question (personas.md §3): a launcher, Settings or
         * the person, about the person the caller runs for. */
        if let Request::PersonaList { app_id } = req {
            let resp = handle_persona_list(uid, &owner, app_id.as_deref(), &shared);
            write_response(&mut writer, &resp)?;
            continue;
        }
        /* ★ The caller's own persona (personas.md §3): from the launch
         * registry by the PEER UID, its colour from the owner's policy. */
        if matches!(req, Request::Persona) {
            let reg = load_registry();
            let resp = match (reg.persona(uid), reg.resolve(uid)) {
                (Some(here), Some((user, _))) => {
                    let mut st = shared.lock().unwrap_or_else(|e| e.into_inner());
                    let pol = st.get_or_load(user).ok().map(|tp| &tp.policy).filter(|p| !p.personas.is_empty() && p.check_personas().is_ok());
                    let colour = here.and_then(|p| pol.and_then(|pol| pol.personas.get(p).map(|d| d.colour.clone())));
                    let others = pol.map(|pol| std::iter::once(portcullis_policy::PERSONAL).chain(pol.personas.keys().map(|k| k.as_str()))
                        .filter(|n| *n != here.unwrap_or(portcullis_policy::PERSONAL)).map(String::from).collect()).unwrap_or_default();
                    Response::Persona { name: here.map(String::from), colour, others }
                }
                _ => Response::Error { message: "persona: caller is not a launched app".into() },
            };
            write_response(&mut writer, &resp)?;
            continue;
        }
        if let Request::Launch { app_id, bypass_policy, persona } = req {
            /* `owner`, not `user`: see the inheritance note above. This is both
             * whose policy authorizes the launch and who the launched app is
             * recorded as belonging to. */
            handle_launch(app_id, bypass_policy, persona, &owner, &shared,
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
                          .map(|(user, app)| (user.to_string(), app.to_string()))
            {
                Some((user, app)) if launch::app_may_launch_apps(&app) => {
                    let apps = launch::catalog(Some(&user));
                    eprintln!("portcullisd: catalog -> {app} (uid {uid}): {} app(s)",
                              apps.len());
                    Response::CatalogList { apps }
                }
                Some((_, app)) => {
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
            | Request::TeardownSessionComponent { .. }
            | Request::SessionEnded { .. })
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
/// One-shot, per-instance, pipe-wired launch — the worker-pool lifecycle.
///
/// ★★ DELIBERATELY NOT `handle_launch` WITH A FLAG. It skips the policy
/// prompt, the per-app uid allocation, the persistent overlay and the
/// first-run setup — not because those are inconvenient here but because none
/// of them means anything for a jail that exists for one document and is
/// discarded. Threading a boolean through `launch_with_stdio` to switch half
/// of it off would leave the two lifecycles entangled in the one function
/// that must never be wrong.
///
/// ★ It runs the SAME code as `portcullis exec` — `portcullis_oneshot::run` —
/// so the daemon and the CLI cannot drift about what a one-shot jail is. The
/// trust gate already taught this crate what three copies of one decision
/// costs.
///
/// The signature requirement is inside `portcullis_oneshot`, and is
/// `Demand::Required`: a worker pool launches continuously, so "unsigned is
/// allowed while trust is unconfigured" would be a standing condition rather
/// than a fresh machine's grace period.
///
/// ★★★ THE POLICY QUESTION, SETTLED: **no prompt, and no ungranted
/// capability either.**
///
/// This lane skipped the policy gate entirely, which left the signature as
/// the only check — so a signed manifest declaring `network = "full"` and
/// `filesystem = ["~/Documents"]` would have received all of it, unasked, on
/// a path built for workers that need nothing. Installing a signed app was
/// therefore equivalent to granting it everything it declared, provided it
/// was launched this way rather than the other.
///
/// The two obvious answers are both wrong:
///
///   - **Prompt, like `Launch`.** There is nobody to ask. A broker opens
///     sixteen workers because sixteen pages are open; a tty prompt per
///     worker is not a consent mechanism, it is a hang. And "non-tty gets a
///     refusal" would mean the lane cannot work at all.
///   - **Require the manifest to declare NO capabilities.** Tempting, since
///     that is the case the lane was built for — but a rendering worker
///     legitimately wants the font set, and a rule that forbids it forces
///     every future worker back onto the application path it does not fit.
///
/// So: the delta is computed exactly as `Launch` computes it, and a non-empty
/// one is a REFUSAL rather than a prompt, naming what must be granted first.
/// A worker declaring nothing has an empty delta and runs with no setup at
/// all — the common case stays frictionless — while a worker that wants
/// something gets it only after a human has already said yes through
/// `portcullis policy grant`.
///
/// ★ AND THERE IS NO `bypass_policy` HERE, deliberately. `Launch` has one for
/// development (`--no-prompt`); on a path a program drives in a loop, a
/// bypass flag is not a developer convenience but a permanent hole with a
/// friendly name.
/// A launched app's (person, persona), by its uid, for its helpers' route
/// domain; None for anything else (a person's own CLI).
fn caller_domain(uid: u32) -> Option<(String, Option<String>)> {
    let reg = load_registry();
    let (person, _) = reg.resolve(uid)?;
    Some((person.to_string(), reg.persona(uid).flatten().map(String::from)))
}

fn handle_exec_instance(
    app_id:   String,
    instance: Option<String>,
    tmpfs_mb: u32,
    limits:   (Option<u64>, Option<u32>, bool),
    rule:     Option<portcullis_policy::WorkerRule>,
    mut extra_env: Vec<(String, String)>,
    user:     &str,
    shared:   &Mutex<Tenants>,
    limiter:  &Arc<Mutex<portcullisd::ratelimit::Limiter>>,
    writer:   &mut UnixStream,
    reader:   &BufReader<UnixStream>,
    /* What the entry gets in place of the caller's three descriptors
     * (OpenCdm: its channels go to the CDM host over a socket of its own,
     * beside the library and the store). None: the caller's, as sent. */
    prepare:  Option<&dyn Fn([OwnedFd; 3]) -> std::io::Result<[OwnedFd; 3]>>,
    /* Some: answer `ReadyForFdsAs` with this uid (the caller asked). */
    as_uid:   Option<u32>,
    /* Some: the app's `[[workers]]` entry of this name, which runs with no
     * capability at all, so the capability gate has nothing to ask. */
    worker:   Option<String>,
    /* The (person, persona) whose route domain a networked worker joins
     * (vpn.md §5.4): its caller's, so a Work app's helpers leave by Work's
     * tunnel. None: the host's own table (the portal's sign-in fetcher). */
    domain:   Option<(String, Option<String>)>,
    ordinary_fetcher: bool,
) -> std::io::Result<()> {
    /* ★ LIMITED BEFORE THE FD HANDSHAKE, not after. Answering ReadyForFds
     * first would have the client send three descriptors the daemon is about
     * to refuse, and a refusal that still costs the caller work is a refusal
     * a loop can use as a service. */
    /* A delegation in the SYSTEM policy (root-owned; a user's own file may
     * not delegate a uid): `user` may start this app as a worker that runs
     * as the rule's `run_as`, on the rule's own budget. Without one, the
     * worker runs as the caller, as it always has. */
    let engine = app_id == "org.atrium.navigator.engine";
    if engine { portcullis_oneshot::start_trace("portcullisd", "exec-begin", &app_id) }
    if let Some(r) = &rule {
        eprintln!("portcullisd: {user} starts {app_id} as {} (system worker rule)", r.run_as);
    }
    let acquired = match &rule {
        Some(r) => portcullisd::ratelimit::Slot::acquire_reserved(
            limiter, &format!("worker:{app_id}"), r.concurrent.unwrap_or(32), now_millis()),
        None => portcullisd::ratelimit::Slot::acquire(limiter, user, now_millis()),
    };
    let slot = match acquired {
        Ok(s) => s,
        Err(why) => {
            eprintln!("portcullisd: rate-limited {user}: {why}");
            return write_response(writer, &Response::LaunchFailed {
                stage: "ratelimit".into(), message: why.to_string(),
            });
        }
    };

    /* ★ The capability gate, BEFORE the fd handshake for the same reason the
     * rate limit is: a refusal that first makes the client hand over three
     * descriptors has charged it for nothing.
     *
     * The signature is checked again inside portcullis_oneshot — this read is
     * for the capabilities, and the gate there is the one that decides. */
    let tree = std::path::PathBuf::from("/var/lib/atrium/apps").join(&app_id);
    let text = match std::fs::read_to_string(tree.join("atrium.toml")) {
        Ok(t) => t,
        Err(e) => return write_response(writer, &Response::LaunchFailed {
            stage: "manifest".into(), message: format!("read {}: {e}", tree.display()),
        }),
    };
    let manifest = match portcullis_toml::Manifest::from_str(&text) {
        Ok(m) => m,
        Err(e) => return write_response(writer, &Response::LaunchFailed {
            stage: "manifest".into(), message: format!("parse error: {e:?}"),
        }),
    };
    let current_hash = hash_manifest(text.as_bytes());
    if let Some(w) = &worker {
        if manifest.worker(w).is_none() {
            return write_response(writer, &Response::LaunchFailed {
                stage: "worker".into(), message: format!("{app_id} has no worker {w:?}"),
            });
        }
    }
    let (delta, system_unusable) = if worker.is_some() { (Default::default(), None) } else {
        let mut st = shared.lock().unwrap_or_else(|e| e.into_inner());
        let tp = match st.get_or_load(user) {
            Ok(tp) => tp,
            Err(e) => return write_response(writer, &Response::Error {
                message: format!("policy load for {user}: {e}"),
            }),
        };
        let system = Policy::load_system(std::path::Path::new(SYSTEM_PATH));
        let d = decide(&app_id, &manifest.capabilities, &current_hash, &tp.policy, system.as_ref());
        if let Some(why) = &d.system_unusable { eprintln!("portcullisd: system policy unusable: {why}") }
        if let Some(by) = d.by { eprintln!("portcullisd: {app_id} authorized by {by:?} policy (one-shot lane, user {user})") }
        (d.delta, d.system_unusable)
    };
    if !delta.is_empty() {
        let why = format!(
            "{} needs capabilities that are not granted: {}. \
             There is nobody to prompt on this path — grant them first with \
             `portcullis policy grant {}` (or, for a headless component, \
             `portcullis policy grant --system {}` as root), or launch it as an application.{}",
            app_id, delta.describe().join("; "), app_id, app_id,
            system_unusable.map(|w| format!(" (The system policy was not used: {w}.)")).unwrap_or_default());
        eprintln!("portcullisd: REFUSED {app_id} on the one-shot lane — {why}");
        return write_response(writer, &Response::LaunchFailed {
            stage: "policy".into(), message: why,
        });
    }

    /* The caller's route domain, before the fd handshake like every other
     * refusal: a persona with a VPN and no routing table is refused, never
     * run on the host's table. */
    let fib = match &domain {
        None => None,
        Some((person, persona)) => match portcullisd::route_domain::for_persona(person, persona.as_deref()) {
            Ok(f) => f,
            Err(why) => {
                eprintln!("portcullisd: {app_id} REFUSED: {why}");
                return write_response(writer, &Response::LaunchFailed { stage: "network".into(), message: why });
            }
        },
    };
    // The ordinary fetcher may talk to the host's configured loopback
    // resolver, but a VPN-bound or portal fetcher must keep its own DNS path.
    let host_resolver = if ordinary_fetcher && fib.is_none() {
        match host_dns::socket() {
            Ok(socket) => socket,
            Err(e) => return write_response(writer, &Response::LaunchFailed {
                stage: "resolver".into(), message: e.to_string(),
            }),
        }
    } else {
        None
    };
    if engine { portcullis_oneshot::start_trace("portcullisd", "ready-for-fds", &app_id) }
    write_response(writer, &match as_uid {
        Some(uid) => Response::ReadyForFdsAs { uid },
        None => Response::ReadyForFds,
    })?;

    if !reader.buffer().is_empty() {
        return write_response(writer, &Response::LaunchFailed {
            stage: "fdpass".into(),
            message: "protocol violation: data buffered before fd handoff".into(),
        });
    }
    /* A worker (WorkerCall) may get a few descriptors past its stdio, at 3,
     * 4, ... in its jail: what its app hands it (the Player's codec and CDM
     * sockets). Everything else takes exactly three. */
    let most = if worker.is_some() { 3 + jaild::protocol::MAX_EXTRA_FDS } else { 3 };
    let fds = match portcullis_ipc::recv_fds(reader.get_ref(), most) {
        Ok(v) => v,
        Err(e) => return write_response(writer, &Response::LaunchFailed {
            stage: "fdpass".into(), message: format!("recv_fds: {e}"),
        }),
    };
    if fds.len() < 3 || fds.len() > most {
        return write_response(writer, &Response::LaunchFailed {
            stage: "fdpass".into(),
            message: if most > 3 { format!("expected 3 to {most} fds, got {}", fds.len()) }
                     else { format!("expected 3 fds, got {}", fds.len()) },
        });
    }
    if engine { portcullis_oneshot::start_trace("portcullisd", "fds-received", &app_id) }
    let mut it = fds.into_iter();
    let stdio = [it.next().unwrap(), it.next().unwrap(), it.next().unwrap()];
    let mut extra_fds: Vec<OwnedFd> = it.collect();
    if let Some(socket) = host_resolver {
        debug_assert!(extra_fds.is_empty());
        extra_fds.push(socket);
        extra_env.push((nuntius::HOST_DNS_FD_ENV.into(), "3".into()));
    }
    let stdio = match prepare {
        None => stdio,
        Some(f) => match f(stdio) {
            Ok(s) => s,
            Err(e) => return write_response(writer, &Response::LaunchFailed {
                stage: "fdpass".into(), message: format!("prepare: {e}"),
            }),
        },
    };

    let spec = portcullis_oneshot::Spec {
        target: app_id.clone(),
        instance,
        tmpfs_mb,
        // ★ The CALLER's limits, forwarded (§6.5.2e), combined with the
        // manifest's [resources] by portcullis_oneshot (tighter wins) and
        // applied by jaild at create. No limit is invented here: a constant
        // would fight memfed, which budgets the jail's pool dynamically and
        // never below RSS; a memory figure that IS given is a ceiling memfed
        // may lower and never raise (jaild clamps SetRctl to it).
        memory_mb: limits.0,
        cpu_pct: limits.1,
        require_memory_limit: limits.2,
        // The home jail(8) chdirs into is resolved from this user's passwd
        // inside portcullis_oneshot — see Spec::user_name.
        user_name: rule.as_ref().map(|r| r.run_as.clone()).unwrap_or_else(|| user.to_string()),
        extra_env,
        worker,
        fib,
    };
    /* ★ A worker does not outlive its caller. The caller holds this
     * connection for the jail's whole life; if it hangs up (it exited, or it
     * is ending a worker that ignored end-of-file on its stdin, as frescod
     * does for a renderer), the jail is removed. Without this the caller had
     * no way to end a worker that runs as another uid (a worker rule). */
    let watch = jail_name_for(&spec).and_then(|name| {
        reader.get_ref().try_clone().ok().map(|c| end_jail_on_hangup(c, name))
    });
    // A worker's caller has its answer once the jail is removed; its mounts
    // are unwound after (a dying jail can pin them for seconds).
    let run = if spec.worker.is_some() { portcullis_oneshot::run_with_fds_detached } else { portcullis_oneshot::run_with_fds };
    let resp = match run(&spec, Some(stdio), extra_fds) {
        // The worker's own status (jaild reaps it); None = killed by a signal.
        portcullis_oneshot::OneShot::Exit { code, .. } =>
            Response::LaunchExit { code },
        // Logged here too: a caller that gives up when its channels close (a
        // player whose CDM never came) exits before it reads this response.
        portcullis_oneshot::OneShot::Refused(why) => {
            eprintln!("portcullisd: {} refused: {why}", spec.target);
            Response::LaunchFailed { stage: "refused".into(), message: why }
        }
        portcullis_oneshot::OneShot::Failed(why) => {
            eprintln!("portcullisd: {} failed: {why}", spec.target);
            Response::LaunchFailed { stage: "oneshot".into(), message: why }
        }
    };
    // The slot is held for the jail's whole life — `run_with_stdio` blocks
    // until the entry exits — and released here, or by the guard's Drop on
    // any early return or panic above.
    drop(slot);
    if let Some(done) = watch { done.store(true, std::sync::atomic::Ordering::Release) }
    write_response(writer, &resp)
}

/// limend's user: the only caller a manifest's `[roles]` provider is
/// started for without a system rule.
const LIMEN_USER: &str = "_limen";
/// How many embeds of one manifest provider may run at once.
const PROVIDERS_PER_APP: usize = 32;

/// How many calls of one app's workers may run at once.
const WORKER_CALLS_PER_APP: usize = 16;

/// A worker call's jail instance: the worker's name and a sequence number,
/// unique per call, within the 64 bytes a jail name may take.
fn worker_instance(worker: &str, uid: u32) -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!("w-{worker}-{uid}-{}", SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

/// A fetcher's jail instance: unique per start (the caller's uid and a
/// sequence number), within the 64 bytes a jail name may take.
fn fetcher_instance(uid: u32) -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    format!("u{uid}-{}", SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
}

fn jail_name_for(spec: &portcullis_oneshot::Spec) -> Option<String> {
    portcullis_jail::jaild_instance_name(&spec.target, spec.instance.as_deref())
}

/// Watch `caller` until `done` is set; if it hangs up first, remove the
/// jail `name` (its processes are killed, and `run_with_stdio` returns).
fn end_jail_on_hangup(caller: UnixStream, name: String) -> Arc<std::sync::atomic::AtomicBool> {
    use std::sync::atomic::Ordering;
    let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let d = done.clone();
    let _ = std::thread::Builder::new().name("portcullisd-exec-hup".into()).spawn(move || {
        while !d.load(Ordering::Acquire) {
            let mut p = libc::pollfd { fd: caller.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            let n = unsafe { libc::poll(&mut p, 1, 250) };
            if n <= 0 || d.load(Ordering::Acquire) { continue }
            let hung_up = p.revents & (libc::POLLHUP | libc::POLLERR) != 0 || {
                let mut b = [0u8; 1];
                let r = unsafe { libc::recv(caller.as_raw_fd(), b.as_mut_ptr().cast(), 1, libc::MSG_PEEK | libc::MSG_DONTWAIT) };
                r == 0
            };
            if hung_up {
                eprintln!("portcullisd: caller hung up; removing worker jail {name}");
                // SIGKILL first: `jail -r` alone sends SIGTERM and waits ten
                // seconds for a worker that may be ignoring it (or stopped).
                let _ = std::process::Command::new("pkill").args(["-9", "-j", &name])
                    .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
                let _ = std::process::Command::new("jail").arg("-r").arg(&name)
                    .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
                return;
            }
            // The caller wrote something it should not have; do not spin on it.
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    });
    done
}

/// Monotonic milliseconds for the limiter. ★ Monotonic, not wall clock: a
/// rate limit measured against a clock NTP can step backwards is a rate limit
/// that can be widened by changing the time.
fn now_millis() -> u64 {
    use std::sync::OnceLock;
    static START: OnceLock<std::time::Instant> = OnceLock::new();
    START.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

fn handle_launch(
    app_id:        String,
    bypass_policy: bool,
    persona:       Option<String>,
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
    /* The persona (personas.md §3): Personal, or one of the person's own
     * named personas from their policy file. Checked before anything else,
     * including under bypass: --no-prompt skips the capability prompt, never
     * whose persona this is. */
    if let Some(p) = persona.as_deref() {
        let known = portcullis_peer::valid_persona(p) && {
            let mut s = shared.lock().unwrap();
            s.get_or_load(user).map(|tp| tp.policy.has_persona(Some(p))).unwrap_or(false)
        };
        if !known {
            return write_response(writer, &Response::LaunchFailed {
                stage: "persona".into(),
                message: format!("{user} has no persona {p:?}"),
            });
        }
        if let Some(why) = launch::not_instanced(&app_id) {
            return write_response(writer, &Response::LaunchFailed {
                stage: "persona".into(),
                message: format!("{app_id} runs once for every persona: {why}"),
            });
        }
    }
    let persona = persona.as_deref();
    if launch::is_single_instance(&app_id) {
        if let Some(jid) = launch::running_jid_instance(&app_id, persona) {
            let uid = launch::instance_uid(user, &app_id, persona).unwrap_or(0);
            remember_persona(user, &app_id, persona, shared);
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
        let system = Policy::load_system(std::path::Path::new(SYSTEM_PATH));
        let d = decide(&app_id, &manifest.capabilities, &current_hash, &tp.policy, system.as_ref());
        if let Some(why) = &d.system_unusable { eprintln!("portcullisd: system policy unusable: {why}") }
        let delta = d.delta;
        drop(s);
        if !delta.is_empty() {
            // What it opens is part of what the user approves (opening-files.md §5.1).
            let mut lines = delta.describe();
            lines.extend(manifest.opens_lines());
            lines.extend(manifest.links_line());
            return write_response(writer, &Response::LaunchNeedsApproval {
                delta: lines,
            });
        }
    }

    /* 2. ReadyForFds round-trip. */
    remember_persona(user, &app_id, persona, shared);
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
    let resp = match launch::launch_instance(&app_id, persona, user, Some(stdio)) {
        Ok(o) => Response::LaunchExit { code: o.exit_code },
        Err(e) => Response::LaunchFailed {
            stage: e.stage().into(),
            message: e.message(),
        },
    };
    write_response(writer, &resp)
}

/// Apps portcullisd is starting, or running, for a file opened in them,
/// so two opens in quick succession start one instance.
static STARTED_FOR_OPEN: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Whether `app_id` may run for `user` now without asking (the Launch
/// gate, without its prompt: an open is not where a new grant is given).
fn may_run_unasked(app_id: &str, user: &str, shared: &Mutex<Tenants>) -> Result<(), String> {
    granted_manifest(app_id, user, shared).map(drop)
}

fn granted_manifest(
    app_id: &str,
    user: &str,
    shared: &Mutex<Tenants>,
) -> Result<(portcullis_toml::Manifest, Option<[u8; 32]>), String> {
    let tree = std::path::PathBuf::from("/var/lib/atrium/apps").join(app_id);
    let text =
        std::fs::read_to_string(tree.join("atrium.toml")).map_err(|e| format!("{app_id}: {e}"))?;
    let publisher = manifest_trust::verify_publisher(&tree, &text)?;
    let manifest =
        portcullis_toml::Manifest::from_str(&text).map_err(|e| format!("{app_id}: {e}"))?;
    let hash = hash_manifest(text.as_bytes());
    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
    let tp = s
        .get_or_load(user)
        .map_err(|e| format!("policy for {user}: {e}"))?;
    let system = Policy::load_system(std::path::Path::new(SYSTEM_PATH));
    let d = decide(
        app_id,
        &manifest.capabilities,
        &hash,
        &tp.policy,
        system.as_ref(),
    );
    if d.delta.is_empty() {
        Ok((manifest, publisher))
    } else {
        Err(format!(
            "{} needs your approval first: open it once from Applications",
            manifest.app.name
        ))
    }
}

/// OpenApps and OpenFile (opening-files.md §6.1, §6.2). `uid` is the
/// caller's peer uid; `owner` the human it belongs to, whose defaults and
/// policy apply.
fn handle_open(
    uid:    u32,
    owner:  &str,
    req:    Request,
    shared: &Mutex<Tenants>,
    writer: &mut UnixStream,
    reader: &BufReader<UnixStream>,
) -> std::io::Result<()> {
    use portcullis_ipc::OpenChoice;
    use portcullis_policy::opens::Defaults;
    use portcullis_toml::OpenMode;
    use portcullisd::opens;
    let refuse = |w: &mut UnixStream, why: String| {
        eprintln!("portcullisd: open REFUSED to uid {uid}: {why}");
        write_response(w, &Response::Error { message: why })
    };
    let caller = portcullis_peer::AppRegistry::load(portcullis_peer::DEFAULT_REGISTRY)
        .unwrap_or_default().resolve(uid).map(|(_, a)| a.to_string());
    // A mail app may ask which apps open a type (Open with…), not open files.
    let mail_asks = matches!(req, Request::OpenApps { .. }) && caller.as_deref().is_some_and(launch::app_opens_attachments);
    if !mail_asks && !caller.as_deref().is_some_and(launch::app_is_file_manager) {
        return refuse(writer, format!("cap.open.denied: only the file manager opens files in apps (caller {})",
                                      caller.as_deref().unwrap_or("is not a launched app")));
    }
    let declarable = |mime: &str| atrium_types::by_mime(mime).filter(|t| t.family.declarable());
    match req {
        Request::WorkbookBookmark { expected } => {
            if caller.as_deref() != Some("org.atrium.scrinium") {
                return refuse(writer, "Workbook bookmarks are served by Scrinium.".into());
            }
            if !reader.buffer().is_empty() {
                return refuse(writer, "protocol violation: data buffered before the descriptor".into());
            }
            write_response(writer, &Response::ReadyForFds)?;
            let fds = portcullis_ipc::recv_fds(reader.get_ref(), 1)?;
            let Ok([file]): Result<[OwnedFd; 1], _> = fds.try_into() else {
                return refuse(writer, "expected one workbook descriptor".into());
            };
            if let Err(why) = opens::check(&file, false) {
                return refuse(writer, why);
            }
            match portcullisd::workbook_versions::bookmark(&std::fs::File::from(file), expected.as_ref()) {
                Ok(bookmark) => write_response(writer, &Response::WorkbookBookmark { bookmark }),
                Err(err) => refuse(writer, format!("Workbook bookmark: {err}")),
            }
        }
        Request::WorkbookVersion { generation } => {
            if caller.as_deref() != Some("org.atrium.scrinium") {
                return refuse(writer, "Workbook versions are served by Scrinium.".into());
            }
            if !reader.buffer().is_empty() {
                return refuse(writer, "protocol violation: data buffered before the descriptors".into());
            }
            write_response(writer, &Response::ReadyForFds)?;
            let fds = portcullis_ipc::recv_fds(reader.get_ref(), 2)?;
            let Ok([current, previous]): Result<[OwnedFd; 2], _> = fds.try_into() else {
                return refuse(writer, "expected current and previous workbook descriptors".into());
            };
            if let Err(why) = opens::check(&current, false).and_then(|()| opens::check(&previous, false)) {
                return refuse(writer, why);
            }
            let retained = match portcullisd::workbook_versions::retained(
                &std::fs::File::from(current), &std::fs::File::from(previous), generation,
            ) {
                Ok(retained) => retained,
                Err(err) => return refuse(writer, format!("Workbook version lookup: {err}")),
            };
            let Some((number, file)) = retained else {
                return write_response(writer, &Response::WorkbookVersion { generation: None });
            };
            if generation.is_some() {
                let fd = OwnedFd::from(file);
                write_response(writer, &Response::WorkbookVersion { generation: Some(number) })?;
                portcullis_ipc::send_fds(writer, &[fd.as_raw_fd()])?;
                Ok(())
            } else {
                write_response(writer, &Response::WorkbookVersion { generation: Some(number) })
            }
        }
        Request::OpenApps { mime } => {
            if declarable(&mime).is_none() {
                return refuse(writer, format!("{mime}: not a type Atrium opens in apps"));
            }
            let apps = launch::apps_opening(&mime);
            let defaults = Defaults::load(&Defaults::user_path(owner)).unwrap_or_default();
            let def = defaults.app_for(&mime, |id| apps.iter().any(|(a, _, _)| a == id));
            let mut list: Vec<OpenChoice> = apps.into_iter()
                .map(|(id, name, mode)| OpenChoice { default: def.as_deref() == Some(id.as_str()), id, name, edit: mode == OpenMode::Edit })
                .collect();
            list.sort_by_key(|c| !c.default);
            write_response(writer, &Response::OpenApps { apps: list })
        }
        Request::OpenFile {
            app, mime, name, write, activation, always, workbook_picker,
            workbook_commit, prepare_workbook_commit,
        } => {
            let registry = load_registry();
            if (workbook_commit.is_some() || prepare_workbook_commit)
                && (!write || app != portcullis_policy::opens::SHEETS || caller.as_deref() != Some("org.atrium.scrinium"))
            {
                return refuse(writer, "save grant requires a writable Sheets workbook from Scrinium".into());
            }
            if workbook_commit.is_some() && prepare_workbook_commit {
                return refuse(writer, "expected one workbook save grant".into());
            }
            let persona = match workbook_picker {
                Some(parent) if app == portcullis_policy::opens::SHEETS && !always
                    && registry.resolve(parent) == Some((owner, app.as_str())) => registry.persona(parent).flatten(),
                Some(_) => return refuse(writer, "invalid workbook picker parent".into()),
                None => None,
            };
            let Some(t) = declarable(&mime) else {
                return refuse(writer, format!("{mime}: not a type Atrium opens in apps"));
            };
            if name.is_empty() || name.contains(['/', '\0']) || name.len() > atrium_open::MAX_NAME {
                return refuse(writer, "the file's name, not a path".into());
            }
            let Some((mode, app_name)) = launch::app_opens(&app, &mime) else {
                return refuse(writer, format!("{app} doesn't open {}", t.words));
            };
            if write && mode != OpenMode::Edit {
                return refuse(writer, format!("{app_name} only views {}", t.words));
            }
            if let Err(why) = may_run_unasked(&app, owner, shared) {
                return refuse(writer, why);
            }
            // Last of the checks: a refusal above doesn't spend the press.
            if let Err(why) = opens::spend_press(&opens::activation_socket(), activation, uid) {
                return refuse(writer, why);
            }
            if !reader.buffer().is_empty() {
                return refuse(writer, "protocol violation: data buffered before the descriptor".into());
            }
            let stream = reader.get_ref();
            let workbook_commit = if prepare_workbook_commit {
                write_response(writer, &Response::ReadyForWorkbookCommit)?;
                Some(opens::receive_workbook_commit(stream, std::time::Duration::from_secs(60))?)
            } else {
                write_response(writer, &Response::ReadyForFds)?;
                workbook_commit
            };
            let received = portcullis_ipc::recv_fds(stream, 1);
            if prepare_workbook_commit {
                stream.set_read_timeout(None)?;
            }
            let mut fds = match received {
                Ok(v) if v.len() == 1 => v,
                Ok(v) => return refuse(writer, format!("expected one descriptor, got {}", v.len())),
                Err(e) => return refuse(writer, format!("recv_fds: {e}")),
            };
            let fd = fds.pop().unwrap();
            if let Err(why) = opens::check(&fd, write) {
                return refuse(writer, format!("“{name}”: {why}"));
            }
            if let Err(e) = opens::narrow(&fd, write) {
                return refuse(writer, format!("cap_rights_limit: {e}"));
            }
            if always {
                let p = Defaults::user_path(owner);
                let mut d = Defaults::load(&p).unwrap_or_default();
                if let Err(e) = d.choose(&mime, &app).map_err(std::io::Error::other).and_then(|_| d.save(&p)) {
                    eprintln!("portcullisd: default for {mime}: {e}");
                } else {
                    eprintln!("portcullisd: {owner}'s default for {mime} is now {app}");
                }
            }
            if let Err(e) = deliver_open(owner, &app, persona, &name, &mime, write, fd, workbook_commit) {
                return refuse(writer, e.to_string());
            }
            eprintln!("portcullisd: “{name}” ({mime}) opened in {app} for {owner}{}", if write { " (write)" } else { "" });
            write_response(writer, &Response::Opened { app: app_name, name })
        }
        _ => unreachable!("routed by serve()"),
    }
}

fn deliver_open(
    owner: &str, app: &str, persona: Option<&str>, name: &str,
    mime: &str, write: bool, fd: OwnedFd, commit: Option<[u8; 16]>,
) -> std::io::Result<()> {
    use portcullisd::opens::{self, Delivery, Hub};
    if app == portcullis_policy::opens::SHEETS {
        return launch::open_workbook(owner, persona, name, mime, write, fd, commit);
    }
    let key = opens::channel_key(app, persona);
    let hub = Hub::global();
    hub.listen(&key)?;
    if hub.deliver(owner, &key, name, mime, write, fd)? == Delivery::Queued {
        start_for_instance(app, persona, owner, "to open a file");
    }
    Ok(())
}

/// The most a download may be (navigator-control's `FILE_MAX` is lower).
const DOWNLOAD_MAX: u64 = 4 << 30;

/// SaveDownload (personas.md §4.1): the Navigator's file, written into the
/// Downloads of the persona the caller runs in.
fn handle_save_download(
    uid:    u32,
    owner:  &str,
    name:   &str,
    origin: &str,
    writer: &mut UnixStream,
    reader: &BufReader<UnixStream>,
) -> std::io::Result<()> {
    let refuse = |w: &mut UnixStream, why: String| {
        eprintln!("portcullisd: download REFUSED to uid {uid}: {why}");
        write_response(w, &Response::Error { message: why })
    };
    let reg = load_registry();
    let caller = reg.resolve(uid).map(|(_, a)| a.to_string());
    if !caller.as_deref().is_some_and(launch::app_is_navigator) {
        return refuse(writer, format!("cap.navigator_broker.denied: {} may not save downloads",
                                      caller.as_deref().unwrap_or("a process that is not a launched app")));
    }
    let persona = reg.persona(uid).flatten().map(String::from);
    if name.is_empty() || name.contains(['/', '\0']) || name.len() > atrium_open::MAX_NAME || name.starts_with('.') || name.chars().any(char::is_control) {
        return refuse(writer, "the file's name, not a path".into());
    }
    if origin.len() > 2048 || origin.chars().any(char::is_control) {
        return refuse(writer, "the file's origin".into());
    }
    write_response(writer, &Response::ReadyForFds)?;
    if !reader.buffer().is_empty() {
        return refuse(writer, "protocol violation: data buffered before the descriptor".into());
    }
    let mut fds = match portcullis_ipc::recv_fds(reader.get_ref(), 1) {
        Ok(v) if v.len() == 1 => v,
        Ok(v) => return refuse(writer, format!("expected one descriptor, got {}", v.len())),
        Err(e) => return refuse(writer, format!("recv_fds: {e}")),
    };
    let fd = fds.pop().unwrap();
    if let Err(why) = portcullisd::opens::check_sealed(&fd) {
        return refuse(writer, format!("“{name}”: {why}"));
    }
    let (dir, gid) = match portcullisd::home::downloads_for(owner, persona.as_deref()) {
        Ok(d) => d,
        Err(e) => return refuse(writer, format!("{owner}'s Downloads: {e}")),
    };
    let person_uid = match portcullisd::home::person_uid(owner) {
        Some(u) => u,
        None => return refuse(writer, format!("no such person {owner}")),
    };
    match portcullisd::home::save_into(&dir, name, std::fs::File::from(fd), DOWNLOAD_MAX, person_uid, gid, origin) {
        Ok(saved) => {
            let place = match &persona {
                Some(p) => format!("{}'s Downloads", portcullisd::home::persona_words(p)),
                None => "Downloads".into(),
            };
            eprintln!("portcullisd: a download from {} saved for {owner} in {place} as “{saved}”",
                      caller.as_deref().unwrap_or("?"));
            write_response(writer, &Response::Downloaded { name: saved, place })
        }
        Err(e) => refuse(writer, e),
    }
}

/// OpenDownload (navigator-ui.md §6.4): the Navigator's file, opened
/// read-only in the person's default app for its type, in the caller's
/// persona.
fn handle_open_download(
    uid:    u32,
    owner:  &str,
    name:   &str,
    shared: &Mutex<Tenants>,
    writer: &mut UnixStream,
    reader: &BufReader<UnixStream>,
) -> std::io::Result<()> {
    use portcullis_policy::opens::Defaults;
    use portcullisd::opens;
    let refuse = |w: &mut UnixStream, why: String| {
        eprintln!("portcullisd: download open REFUSED to uid {uid}: {why}");
        write_response(w, &Response::Error { message: why })
    };
    let reg = load_registry();
    let caller = reg.resolve(uid).map(|(_, a)| a.to_string());
    if !caller.as_deref().is_some_and(launch::app_is_navigator) {
        return refuse(writer, format!("cap.navigator_broker.denied: {} may not open downloads",
                                      caller.as_deref().unwrap_or("a process that is not a launched app")));
    }
    let persona = reg.persona(uid).flatten().map(String::from);
    if name.is_empty() || name.contains(['/', '\0']) || name.len() > atrium_open::MAX_NAME || name.starts_with('.') || name.chars().any(char::is_control) {
        return refuse(writer, "the file's name, not a path".into());
    }
    write_response(writer, &Response::ReadyForFds)?;
    if !reader.buffer().is_empty() {
        return refuse(writer, "protocol violation: data buffered before the descriptor".into());
    }
    let mut fds = match portcullis_ipc::recv_fds(reader.get_ref(), 1) {
        Ok(v) if v.len() == 1 => v,
        Ok(v) => return refuse(writer, format!("expected one descriptor, got {}", v.len())),
        Err(e) => return refuse(writer, format!("recv_fds: {e}")),
    };
    let fd = fds.pop().unwrap();
    let head = match opens::check_sealed(&fd) {
        Ok(h) => h,
        Err(why) => return refuse(writer, format!("“{name}”: {why}")),
    };
    let d = atrium_types::detect(name, &head);
    let Some(t) = d.known().filter(|t| t.family.declarable()) else {
        return refuse(writer, d.question().unwrap_or_else(|| "No app opens this kind of file.".into()));
    };
    let apps = launch::apps_opening(t.mime);
    let defaults = Defaults::load(&Defaults::user_path(owner)).unwrap_or_default();
    let app = defaults.app_for(t.mime, |id| apps.iter().any(|(a, _, _)| a == id)).or_else(|| apps.first().map(|a| a.0.clone()));
    let Some(app) = app else {
        return refuse(writer, format!("No app opens {} yet", t.words));
    };
    let app_name = apps.iter().find(|a| a.0 == app).map(|a| a.1.clone()).unwrap_or_else(|| app.clone());
    if let Err(why) = may_run_unasked(&app, owner, shared) {
        return refuse(writer, why);
    }
    // The app's instance in the Navigator's persona (one that spans
    // personas has just the one).
    let persona = if launch::not_instanced(&app).is_some() { None } else { persona };
    let key = opens::channel_key(&app, persona.as_deref());
    if let Err(e) = opens::narrow(&fd, false) {
        return refuse(writer, format!("cap_rights_limit: {e}"));
    }
    if let Err(e) = deliver_open(owner, &app, persona.as_deref(), name, t.mime, false, fd, None) {
        return refuse(writer, e.to_string());
    }
    eprintln!("portcullisd: a download “{name}” ({}) from {} opened in {key} for {owner}", t.mime, caller.as_deref().unwrap_or("?"));
    write_response(writer, &Response::Opened { app: app_name, name: name.to_string() })
}

/// Only the broker connects to Fasti as root. Mail gets a uniform receipt
/// and never receives that socket or a Calendar query or editing token.
fn handle_calendar_update(
    uid: u32,
    owner: &str,
    shared: &Mutex<Tenants>,
    mail: &mut UnixStream,
) -> std::io::Result<()> {
    use portcullis_ipc::calendar::{self as protocol, BackgroundUpdate};
    let result = (|| -> Result<(), String> {
        let reg = portcullis_peer::AppRegistry::load(portcullis_peer::DEFAULT_REGISTRY)
            .map_err(|e| e.to_string())?;
        let signer = reg.publisher(uid).flatten().ok_or("Signed Mail required")?;
        let pin = std::fs::read_to_string("/etc/atrium/calendar-publisher")
            .map_err(|_| "Calendar unavailable")?;
        if reg.resolve(uid) != Some((owner, "org.atrium.mail")) || hex::encode(signer) != pin.trim()
        {
            return Err("Calendar updates unavailable".into());
        }
        let (mail_manifest, mail_signer) = granted_manifest("org.atrium.mail", owner, shared)?;
        let (cal_manifest, cal_signer) = granted_manifest(calendar::APP, owner, shared)?;
        if mail_signer != Some(signer)
            || cal_signer != Some(signer)
            || mail_manifest.app.id != "org.atrium.mail"
            || cal_manifest.app.id != calendar::APP
            || mail_manifest.capabilities.calendar_updates != Some(true)
        {
            return Err("Calendar updates unavailable".into());
        }
        let persona = reg.persona(uid).ok_or("Unknown Mail persona")?;
        let calendar_uid = reg
            .uid_for_instance(owner, calendar::APP, Some(&signer), persona)
            .ok_or("Calendar has not been opened for this persona")?;
        let dir = calendar::endpoint(owner, persona, pin.trim(), calendar_uid)
            .map_err(|_| "Calendar unavailable")?;
        let timeout = Some(std::time::Duration::from_secs(3));
        mail.set_read_timeout(timeout).map_err(|e| e.to_string())?;
        mail.set_write_timeout(timeout).map_err(|e| e.to_string())?;
        write_response(mail, &Response::Ok).map_err(|e| e.to_string())?;
        let update: BackgroundUpdate =
            protocol::read_deposit(mail).map_err(|_| "Invalid meeting update")?;
        if !update.source.valid()
            || !update.source.verified
            || !update.source.reply_destination_matches
            || update.proposal.len() > 4096
        {
            return Err("Invalid meeting update".into());
        }
        let mut service =
            UnixStream::connect(dir.join("calendar.sock")).map_err(|_| "Calendar unavailable")?;
        service
            .set_read_timeout(timeout)
            .map_err(|e| e.to_string())?;
        service
            .set_write_timeout(timeout)
            .map_err(|e| e.to_string())?;
        use aqueduct::{classes::CLASS_CALENDAR, envelope, flag, Header};
        let bytes = serde_json::to_vec(&update).map_err(|_| "Invalid meeting update")?;
        let header = Header::new(
            CLASS_CALENDAR,
            protocol::BACKGROUND_OP,
            flag::RESPONSE_EXPECTED,
            bytes.len() as u32,
        );
        envelope::write_message(&mut service, header, &bytes)
            .map_err(|_| "Calendar unavailable")?;
        let mut receipt = [0; 5];
        let (header, kept) = envelope::read_message(&mut service, &mut receipt)
            .map_err(|_| "Calendar unavailable")?;
        if header.opcode_class != CLASS_CALENDAR
            || header.op != protocol::BACKGROUND_OP
            || header.flags != flag::IS_RESPONSE
            || kept != b"true"
        {
            return Err("Calendar could not keep the update".into());
        }
        Ok(())
    })();
    write_response(
        mail,
        &match result {
            Ok(()) => Response::Ok,
            Err(message) => Response::Error { message },
        },
    )
}

/// Bind one private conversation between the signed Mail launch and its persona's
/// Calendar. Neither application can name another recipient or persona.
fn handle_calendar_invitation(
    uid: u32,
    owner: &str,
    activation: [u8; 16],
    shared: &Mutex<Tenants>,
    writer: &mut UnixStream,
) -> std::io::Result<()> {
    use portcullisd::opens;
    let result = (|| -> Result<UnixStream, String> {
        let reg = portcullis_peer::AppRegistry::load(portcullis_peer::DEFAULT_REGISTRY)
            .map_err(|e| e.to_string())?;
        let pin = std::fs::read_to_string("/etc/atrium/calendar-publisher")
            .map_err(|_| "Calendar is not provisioned")?;
        let publisher = reg
            .publisher(uid)
            .flatten()
            .ok_or("A signed Mail launch is required")?;
        if reg.resolve(uid) != Some((owner, "org.atrium.mail"))
            || hex::encode(publisher) != pin.trim()
            || !launch::app_opens_attachments("org.atrium.mail")
        {
            return Err("This Mail launch cannot offer invitations".into());
        }
        let persona = reg.persona(uid).ok_or("Unknown Mail persona")?;
        may_run_unasked(calendar::APP, owner, shared)?;
        let socket = opens::activation_socket();
        opens::spend_press_for(
            &socket,
            activation,
            uid,
            fresco_protocol::activation::purpose::LINK,
        )?;
        let (mail, cal) = UnixStream::pair().map_err(|e| e.to_string())?;
        let key = opens::channel_key(calendar::APP, persona);
        let hub = opens::Hub::global();
        hub.listen(&key).map_err(|e| e.to_string())?;
        match hub
            .deliver(
                owner,
                &key,
                "Invitation",
                portcullis_ipc::calendar::MIME,
                false,
                cal.into(),
            )
            .map_err(|e| e.to_string())?
        {
            opens::Delivery::Queued => {
                start_for_instance(calendar::APP, persona, owner, "to review an invitation")
            }
            opens::Delivery::Sent => {
                if let Some(target) = launch::instance_uid(owner, calendar::APP, persona) {
                    if let Err(why) = opens::raise(&socket, target) {
                        eprintln!("portcullisd: could not raise Calendar: {why}");
                    }
                }
            }
        }
        Ok(mail)
    })();
    match result {
        Ok(s) => {
            write_response(writer, &Response::ReadyForFds)?;
            portcullis_ipc::send_fds(writer, &[s.as_raw_fd()])
        }
        Err(message) => write_response(writer, &Response::Error { message }),
    }
}

/// OpenAttachment (mail.md §8.2): the caller holds `open-attachments`;
/// the descriptor is a sealed, read-only memory file; its type comes from
/// its bytes and name, decided here; it goes read-only to the person's
/// default app for that type. The press is spent last, so a refusal
/// doesn't use it up.
fn handle_open_attachment(
    uid:        u32,
    owner:      &str,
    name:       &str,
    activation: [u8; 16],
    chosen:     Option<&str>,
    shared:     &Mutex<Tenants>,
    writer:     &mut UnixStream,
    reader:     &BufReader<UnixStream>,
) -> std::io::Result<()> {
    use portcullis_policy::opens::Defaults;
    use portcullisd::opens;
    let refuse = |w: &mut UnixStream, why: String| {
        eprintln!("portcullisd: attachment REFUSED to uid {uid}: {why}");
        write_response(w, &Response::Error { message: why })
    };
    let registry = load_registry();
    let persona = registry.persona(uid).flatten();
    let caller = registry.resolve(uid).map(|(_, a)| a.to_string());
    if !caller.as_deref().is_some_and(launch::app_opens_attachments) {
        return refuse(writer, format!("cap.open_attachments.denied: {} may not open attachments",
                                      caller.as_deref().unwrap_or("a process that is not a launched app")));
    }
    if name.is_empty() || name.contains(['/', '\0']) || name.len() > atrium_open::MAX_NAME || name == "." || name == ".." {
        return refuse(writer, "the attachment's name, not a path".into());
    }
    write_response(writer, &Response::ReadyForFds)?;
    if !reader.buffer().is_empty() {
        return refuse(writer, "protocol violation: data buffered before the descriptor".into());
    }
    let mut fds = match portcullis_ipc::recv_fds(reader.get_ref(), 1) {
        Ok(v) if v.len() == 1 => v,
        Ok(v) => return refuse(writer, format!("expected one descriptor, got {}", v.len())),
        Err(e) => return refuse(writer, format!("recv_fds: {e}")),
    };
    let fd = fds.pop().unwrap();
    let head = match opens::check_sealed(&fd) {
        Ok(h) => h,
        Err(why) => return refuse(writer, format!("“{name}”: {why}")),
    };
    let d = atrium_types::detect(name, &head);
    let Some(t) = d.known().filter(|t| t.family.declarable()) else {
        return refuse(writer, d.question().unwrap_or_else(|| "No app opens this kind of file.".into()));
    };
    let apps = launch::apps_opening(t.mime);
    let defaults = Defaults::load(&Defaults::user_path(owner)).unwrap_or_default();
    // Open with…: only an app that opens the type decided here.
    let app = match chosen {
        Some(c) if apps.iter().any(|(a, _, _)| a == c) => Some(c.to_string()),
        Some(c) => return refuse(writer, format!("{c} doesn't open {}", t.words)),
        None => defaults.app_for(t.mime, |id| apps.iter().any(|(a, _, _)| a == id)).or_else(|| apps.first().map(|a| a.0.clone())),
    };
    let Some(app) = app else {
        return refuse(writer, format!("No app opens {} yet", t.words));
    };
    let app_name = apps.iter().find(|a| a.0 == app).map(|a| a.1.clone()).unwrap_or_else(|| app.clone());
    if let Err(why) = may_run_unasked(&app, owner, shared) {
        return refuse(writer, why);
    }
    if let Err(e) = opens::narrow(&fd, false) {
        return refuse(writer, format!("cap_rights_limit: {e}"));
    }
    if let Err(why) = opens::spend_press(&opens::activation_socket(), activation, uid) {
        return refuse(writer, why);
    }
    if let Err(e) = deliver_open(owner, &app, persona, name, t.mime, false, fd, None) {
        return refuse(writer, e.to_string());
    }
    eprintln!("portcullisd: attachment “{name}” ({}) opened in {app} for {owner}", t.mime);
    write_response(writer, &Response::Opened { app: app_name, name: name.to_string() })
}

/// LinkGrant and DeliverLink (opening-links.md §5). `user` is the
/// caller's own user name: Monstrator's system user, or refused.
fn handle_link(user: &str, req: Request, shared: &Mutex<Tenants>) -> Response {
    use portcullis_ipc::{LinkChoice, PersonaChoice};
    use portcullis_policy::PERSONAL;
    use portcullis_policy::links::LinkDefaults;
    use portcullisd::links::{Grant, Grants, MONSTRATOR_USER};
    use portcullisd::opens::{self, Delivery, Hub};
    let refuse = |why: String| {
        eprintln!("portcullisd: link REFUSED ({user}): {why}");
        Response::Error { message: why }
    };
    if user != MONSTRATOR_USER {
        return refuse(format!("cap.link.denied: only the link opener ({MONSTRATOR_USER}) asks this"));
    }
    let grants = Grants::global();
    match req {
        Request::LinkGrant { asker_uid, activation, scheme, app } => {
            let reg = load_registry();
            let Some((person, asker_app)) = reg.resolve(asker_uid).map(|(o, a)| (o.to_string(), a.to_string())) else {
                return refuse(format!("uid {asker_uid} is not a launched app"));
            };
            // The asker's persona, by its uid (personas.md §3).
            let persona = reg.persona(asker_uid).flatten().map(String::from);
            let now = std::time::Instant::now();
            if let Err(why) = grants.may_ask(&asker_app, now) {
                return refuse(why);
            }
            // The apps it may go to, before the press is spent.
            let mut asker_takes = false;
            let mut install = false;
            let apps: Vec<(String, String)> = if scheme == atrium_links::APP {
                let Some(id) = app else { return refuse("an atrium-app: link names its app".into()) };
                match launch::app_name(&id) {
                    Some(name) if id != asker_app => vec![(id, name)],
                    Some(_) => return refuse("an app's link to itself".into()),
                    // Not here: the install sheet asks the person first (L5);
                    // atrium-links checked the id's form.
                    None => {
                        install = true;
                        vec![(id.clone(), id)]
                    }
                }
            } else {
                if let Err(why) = atrium_links::declarable(&scheme) {
                    return refuse(why);
                }
                let all: Vec<(String, String)> = launch::apps_taking_link(&scheme).into_iter()
                    .filter(|(id, _)| may_run_unasked(id, &person, shared).is_ok())
                    .collect();
                asker_takes = all.iter().any(|(id, _)| id == &asker_app) && launch::not_instanced(&asker_app).is_none();
                all.into_iter().filter(|(id, _)| id != &asker_app).collect()
            };
            /* The person's personas (personas.md §6), the asker's first:
             * whether the link may cross into each, by the person's own
             * switches. The asker's own app is a choice only in another
             * persona (the Work Navigator's link in the Personal one). */
            let personas: Vec<PersonaChoice> = {
                let mut st = shared.lock().unwrap_or_else(|e| e.into_inner());
                match st.get_or_load(&person) {
                    Ok(tp) if !tp.policy.personas.is_empty() && tp.policy.check_personas().is_ok() => {
                        let pol = &tp.policy;
                        let here = persona.as_deref();
                        let mut v: Vec<PersonaChoice> = std::iter::once(None).chain(pol.personas.keys().map(|k| Some(k.as_str())))
                            .map(|n| {
                                let is_here = n == here;
                                let allowed = pol.may_cross(here, n, "links");
                                let also = if !is_here && asker_takes {
                                    vec![LinkChoice { id: asker_app.clone(), name: launch::app_name(&asker_app).unwrap_or_else(|| asker_app.clone()), default: false }]
                                } else { vec![] };
                                PersonaChoice { name: n.unwrap_or(PERSONAL).to_string(), colour: n.and_then(|k| pol.personas.get(k)).map(|d| d.colour.clone()),
                                                here: is_here, allowed, also }
                            }).collect();
                        v.sort_by_key(|p| !p.here);
                        v
                    }
                    _ => vec![],
                }
            };
            if apps.is_empty() && personas.iter().all(|p| p.also.is_empty()) {
                return refuse(format!("no app here opens {scheme} links"));
            }
            // Last of the checks: a refusal above doesn't spend the press.
            if let Err(why) = opens::spend_press_for(&opens::activation_socket(), activation, asker_uid, fresco_protocol::activation::purpose::LINK) {
                return refuse(why);
            }
            let def = if scheme == atrium_links::APP { None } else {
                LinkDefaults::load(&LinkDefaults::user_path(&person)).unwrap_or_default()
                    .app_for(&scheme, |id| apps.iter().any(|(a, _)| a == id))
            };
            let mut may: Vec<String> = apps.iter().map(|(a, _)| a.clone()).collect();
            if personas.iter().any(|p| !p.also.is_empty()) {
                may.push(asker_app.clone());
            }
            let grant = match grants.issue(Grant { at: now, person: person.clone(), asker_app: asker_app.clone(), persona: persona.clone(), scheme: scheme.clone(),
                                                   apps: may }) {
                Ok(g) => g,
                Err(why) => return refuse(why),
            };
            let mut list: Vec<LinkChoice> = apps.into_iter()
                .map(|(id, name)| LinkChoice { default: def.as_deref() == Some(id.as_str()), id, name })
                .collect();
            list.sort_by_key(|c| !c.default);
            eprintln!("portcullisd: a {scheme} link from {asker_app} for {person}{}: {} app(s){}", persona.as_deref().map(|p| format!(" ({p})")).unwrap_or_default(), list.len(),
                      if install { ", not installed: to install first" } else { "" });
            Response::LinkGranted { grant, person, apps: list, personas, install }
        }
        Request::DeliverLink { grant, app, url, always, persona: into } => {
            let g = match grants.take(&grant, &app, &url, std::time::Instant::now()) {
                Ok(g) => g,
                Err(why) => return refuse(why),
            };
            let name = launch::app_name(&app).unwrap_or_else(|| app.clone());
            /* ★ The same persona's instance (personas.md §3): a link from
             * Work opens in Work's, never in Personal's. An app that runs
             * once for every persona (Settings, the file manager) is the
             * one exception: it has no other instance. Crossing on purpose
             * is P3. */
            /* Into another persona (personas.md §6): only by the person's
             * switch for that direction, read again now. */
            let target = match into.as_deref() {
                None => g.persona.clone(),
                Some(PERSONAL) => None,
                Some(n) => Some(n.to_string()),
            };
            let shown = |p: &Option<String>| p.as_deref().map(|n| { let mut c = n.chars(); c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default() }).unwrap_or_else(|| "Personal".into());
            if target != g.persona {
                let ok = shared.lock().unwrap_or_else(|e| e.into_inner()).get_or_load(&g.person)
                    .map(|tp| tp.policy.may_cross(g.persona.as_deref(), target.as_deref(), "links")).unwrap_or(false);
                if !ok {
                    return refuse(format!("cap.link.crossing: links from {} to {} are off (Settings, Apps, Between personas)", shown(&g.persona), shown(&target)));
                }
                eprintln!("portcullisd: a {} link crosses from {} to {} for {}, by the person's switch", g.scheme, shown(&g.persona), shown(&target), g.person);
            } else if app == g.asker_app {
                return refuse("an app's link to itself".into());
            }
            // Crossed: the app hears from which persona (a Work app opens it
            // as a stranger would, personas.md §6).
            let crossed_from = (target != g.persona).then(|| g.persona.clone().unwrap_or_else(|| PERSONAL.to_string()));
            let persona = if launch::not_instanced(&app).is_some() { None } else { target };
            let persona = persona.as_deref();
            let key = opens::channel_key(&app, persona);
            if g.scheme == atrium_links::APP {
                start_for_instance(&app, persona, &g.person, "an atrium-app link");
                eprintln!("portcullisd: {app} opened from {} for {}", g.asker_app, g.person);
                return Response::LinkDelivered { app: name };
            }
            if always {
                let p = LinkDefaults::user_path(&g.person);
                let mut d = LinkDefaults::load(&p).unwrap_or_default();
                match d.choose(&g.scheme, &app).map_err(std::io::Error::other).and_then(|_| d.save(&p)) {
                    Ok(()) => eprintln!("portcullisd: {}'s default for {} links is now {app}", g.person, g.scheme),
                    Err(e) => eprintln!("portcullisd: default for {} links: {e}", g.scheme),
                }
            }
            let hub = Hub::global();
            if let Err(e) = hub.listen(&key) {
                return refuse(format!("{key}'s document channel: {e}"));
            }
            match hub.deliver_link_from(&g.person, &key, &g.scheme, &url, crossed_from.as_deref()) {
                // Running: bring its window forward, as a new one would be.
                Ok(Delivery::Sent) => match launch::instance_uid(&g.person, &app, persona) {
                    Some(uid) => match opens::raise(&opens::activation_socket(), uid) {
                        Ok(w) => eprintln!("portcullisd: {app}'s window {w} brought forward"),
                        Err(why) => eprintln!("portcullisd: {app}'s window not brought forward: {why}"),
                    },
                    None => eprintln!("portcullisd: {app}'s window not brought forward: no uid for it"),
                },
                Ok(Delivery::Queued) => start_for_instance(&app, persona, &g.person, "to open a link"),
                Err(e) => return refuse(e.to_string()),
            }
            eprintln!("portcullisd: a {} link from {} opened in {key} for {}", g.scheme, g.asker_app, g.person);
            Response::LinkDelivered { app: name }
        }
        _ => unreachable!("routed by serve()"),
    }
}

/// A user's uid, by name.
fn uid_of_user(name: &str) -> Option<u32> {
    let c = std::ffi::CString::new(name).ok()?;
    // SAFETY: getpwnam with a valid C string; the field is read at once.
    // (Rare here: only an `ExecInstance` that asked whom its worker runs as.)
    let pw = unsafe { libc::getpwnam(c.as_ptr()) };
    (!pw.is_null()).then(|| unsafe { (*pw).pw_uid })
}

/// Start `app` for `owner` on a thread, unless it is starting already.
fn start_for(app: &str, owner: &str, why: &'static str) {
    start_for_instance(app, None, owner, why)
}

/// [`start_for`], for `app`'s instance in `persona`.
fn start_for_instance(app: &str, persona: Option<&str>, owner: &str, why: &'static str) {
    let key = portcullisd::opens::channel_key(app, persona);
    let mut started = STARTED_FOR_OPEN.lock().unwrap();
    if started.iter().any(|a| a == &key) || launch::running_jid_instance(app, persona).is_some() {
        return;
    }
    started.push(key.clone());
    let (a, o, p) = (app.to_string(), owner.to_string(), persona.map(String::from));
    eprintln!("portcullisd: starting {key} for {o} ({why})");
    thread::spawn(move || {
        match launch::launch_instance(&a, p.as_deref(), &o, None) {
            Ok(out) => eprintln!("portcullisd: {key} ({why}) exited {:?}", out.exit_code),
            Err(e) => eprintln!("portcullisd: {key} ({why}) failed at {}: {}", e.stage(), e.message()),
        }
        STARTED_FOR_OPEN.lock().unwrap().retain(|x| x != &key);
    });
}

/// Whether `uid` is an instance in the reserved sign-in persona.
fn is_portal_instance(uid: u32) -> bool {
    load_registry()
        .persona(uid).flatten() == Some(portcullis_peer::PORTAL_PERSONA)
}

/// PortalWindow (atrium-net.md §8.1 S3/S4): the sign-in window for the
/// network the machine is on, as the person `owner`'s Navigator (the
/// installed app holding `network-portal`) in the reserved portal persona.
/// Started if it isn't running; brought forward on a press if it is.
fn handle_portal_window(uid: u32, owner: &str, activation: Option<[u8; 16]>, shared: &Mutex<Tenants>) -> Response {
    use portcullisd::opens;
    let refuse = |why: String| {
        eprintln!("portcullisd: sign-in window REFUSED to uid {uid}: {why}");
        Response::Error { message: why }
    };
    if is_portal_instance(uid) {
        return refuse("that is the sign-in window".into());
    }
    if shared.lock().unwrap_or_else(|e| e.into_inner()).get_or_load(owner).is_err() {
        return refuse(format!("{owner} is not a person with a policy"));
    }
    let Some(app) = launch::portal_app() else {
        return refuse("no installed app holds network-portal".into());
    };
    let p = Some(portcullis_peer::PORTAL_PERSONA);
    if launch::running_jid_instance(&app, p).is_some() {
        let Some(press) = activation else {
            eprintln!("portcullisd: sign-in window for {owner}: already open");
            return Response::Ok;
        };
        if let Err(why) = opens::spend_press_for(&opens::activation_socket(), press, uid, fresco_protocol::activation::purpose::LINK) {
            return refuse(why);
        }
        match launch::instance_uid(owner, &app, p).map(|u| opens::raise(&opens::activation_socket(), u)) {
            Some(Ok(w)) => eprintln!("portcullisd: sign-in window for {owner}: window {w} brought forward"),
            Some(Err(why)) => eprintln!("portcullisd: sign-in window for {owner}: not brought forward: {why}"),
            None => eprintln!("portcullisd: sign-in window for {owner}: no uid for it"),
        }
    } else {
        eprintln!("portcullisd: sign-in window for {owner}: starting {app} (asked by uid {uid})");
        start_for_instance(&app, p, owner, "the sign-in window");
    }
    Response::Ok
}

/// A VPN sign-in the person asked for: the profile and its addresses, for
/// the window to ask (`VpnSignInStatus`).
#[derive(Clone, Debug, PartialEq, Eq)]
struct VpnSignIn {
    profile: String,
    start_url: String,
    final_url: String,
}

/// Each person's latest VPN sign-in, by owner.
type VpnSignInEntry = (String, VpnSignIn, String);
static VPN_SIGN_INS: Mutex<Vec<VpnSignInEntry>> = Mutex::new(Vec::new());
static QUEUED_VPN_SIGN_IN: Mutex<Option<(VpnSignInEntry, std::time::Instant)>> = Mutex::new(None);

/// The reserved instance remains busy until the previous launch has torn down.
fn vpn_window_busy(app: &str) -> bool {
    let persona = Some(portcullis_peer::VPN_PERSONA);
    let key = portcullisd::opens::channel_key(app, persona);
    STARTED_FOR_OPEN.lock().unwrap().contains(&key) || launch::running_jid_instance(app, persona).is_some()
}

/// Wait for the stale window without changing the attempt it sees.
fn open_queued_vpn_sign_in(app: String) {
    use std::time::{Duration, Instant};
    loop {
        let mut queue = QUEUED_VPN_SIGN_IN.lock().unwrap();
        let Some(((_, sign_in, attempt), queued_at)) = queue.as_ref() else { return };
        if Instant::now().duration_since(*queued_at) >= Duration::from_secs(10)
            || vpn_sign_in_attempt(&sign_in.profile).as_deref() != Some(attempt.as_str())
        {
            eprintln!("portcullisd: discarded queued VPN sign-in for {}", sign_in.profile);
            queue.take();
            return;
        }
        if !vpn_window_busy(&app) {
            let (entry, _) = queue.take().unwrap();
            let owner = entry.0.clone();
            {
                let mut entries = VPN_SIGN_INS.lock().unwrap();
                entries.retain(|(o, _, _)| *o != owner);
                entries.push(entry);
            }
            start_for_instance(&app, Some(portcullis_peer::VPN_PERSONA), &owner, "a queued VPN sign-in window");
            return;
        }
        drop(queue);
        thread::sleep(Duration::from_millis(250));
    }
}

/// Query Crypta without granting its root control socket to the sign-in jail.
fn vpn_sign_in_attempt(profile: &str) -> Option<String> {
    use std::io::{BufRead, Read, Write};
    use std::time::Duration;

    let path = std::env::var("PORTCULLISD_CRYPTA_SOCK").unwrap_or_else(|_| "/var/run/atrium/crypta.sock".into());
    let mut stream = UnixStream::connect(path).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(3))).ok()?;
    stream.set_write_timeout(Some(Duration::from_secs(1))).ok()?;
    writeln!(stream, "sso-attempt name={profile}").ok()?;
    let mut line = String::new();
    BufReader::new(stream.take(128)).read_line(&mut line).ok()?;
    let attempt = line.strip_prefix("sso-attempt id=")?.strip_suffix('\n')?;
    (attempt.len() == 32 && attempt.bytes().all(|b| b.is_ascii_hexdigit())).then(|| attempt.to_string())
}

/// Whether `uid` is an instance in the reserved VPN sign-in persona.
fn is_vpn_instance(uid: u32) -> bool {
    load_registry()
        .persona(uid).flatten() == Some(portcullis_peer::VPN_PERSONA)
}

impl VpnSignIn {
    /// A profile name as Crypta's, and two https addresses.
    fn check(&self) -> Result<(), String> {
        let p = &self.profile;
        if p.is_empty() || p.len() > 32 || p.starts_with('-') || !p.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') {
            return Err("not a VPN profile's name".into());
        }
        for u in [&self.start_url, &self.final_url] {
            if u.len() > 8192 || !u.starts_with("https://") || u.chars().any(|c| c.is_control() || c == ' ') {
                return Err("a sign-in address must be https".into());
            }
        }
        Ok(())
    }
}

/// VpnSignInWindow (vpn.md §9.3): the person `owner`'s Navigator in the
/// reserved `vpn` persona, at the gateway's sign-on address. Started if it
/// isn't running, brought forward on a press for the same attempt, or
/// queued for a reconnect while the old window closes.
fn handle_vpn_sign_in_window(uid: u32, owner: &str, ask: VpnSignIn, activation: Option<[u8; 16]>, shared: &Mutex<Tenants>) -> Response {
    use portcullisd::opens;
    let refuse = |why: String| {
        eprintln!("portcullisd: VPN sign-in window REFUSED to uid {uid}: {why}");
        Response::Error { message: why }
    };
    if is_vpn_instance(uid) || is_portal_instance(uid) {
        return refuse("that is a sign-in window".into());
    }
    if let Err(why) = ask.check() {
        return refuse(why);
    }
    if shared.lock().unwrap_or_else(|e| e.into_inner()).get_or_load(owner).is_err() {
        return refuse(format!("{owner} is not a person with a policy"));
    }
    let Some(app) = launch::portal_app() else {
        return refuse("no installed app holds network-portal (the Navigator)".into());
    };
    let profile = ask.profile.clone();
    let mut queue = QUEUED_VPN_SIGN_IN.lock().unwrap();
    let Some(attempt) = vpn_sign_in_attempt(&profile) else {
        return refuse("Crypta has no live sign-in attempt for this profile".into());
    };
    let p = Some(portcullis_peer::VPN_PERSONA);
    let entry = (owner.to_string(), ask, attempt);
    if let Some((queued, _)) = queue.as_ref() {
        if queued == &entry {
            return Response::Ok;
        }
        if queued.0 != owner || queued.1.profile != profile || queued.2 == entry.2 {
            return refuse("another VPN sign-in window is queued".into());
        }
        *queue = Some((entry, std::time::Instant::now()));
        eprintln!("portcullisd: replaced queued VPN sign-in window for {owner} ({profile})");
        return Response::Ok;
    }
    let running = vpn_window_busy(&app);
    {
        let mut v = VPN_SIGN_INS.lock().unwrap_or_else(|e| e.into_inner());
        if running && !v.contains(&entry) {
            if !v.iter().any(|(o, sign_in, id)| o == owner && sign_in.profile == profile && id != &entry.2) {
                return refuse("the previous VPN sign-in window is still open".into());
            }
            *queue = Some((entry, std::time::Instant::now()));
            eprintln!("portcullisd: queued VPN sign-in window for {owner} ({profile})");
            thread::spawn(move || open_queued_vpn_sign_in(app));
            return Response::Ok;
        }
        v.retain(|(o, _, _)| o != owner);
        v.push(entry);
    }
    if running {
        drop(queue);
        let Some(press) = activation else {
            eprintln!("portcullisd: VPN sign-in window for {owner}: already open");
            return Response::Ok;
        };
        if let Err(why) = opens::spend_press_for(&opens::activation_socket(), press, uid, fresco_protocol::activation::purpose::LINK) {
            return refuse(why);
        }
        match launch::instance_uid(owner, &app, p).map(|u| opens::raise(&opens::activation_socket(), u)) {
            Some(Ok(w)) => eprintln!("portcullisd: VPN sign-in window for {owner}: window {w} brought forward"),
            Some(Err(why)) => eprintln!("portcullisd: VPN sign-in window for {owner}: not brought forward: {why}"),
            None => eprintln!("portcullisd: VPN sign-in window for {owner}: no uid for it"),
        }
    } else {
        eprintln!("portcullisd: VPN sign-in window for {owner} ({profile}): starting {app} (asked by uid {uid})");
        start_for_instance(&app, p, owner, "a VPN sign-in window");
    }
    Response::Ok
}

/// VpnSignInStatus: the profile and addresses, for that window only.
fn handle_vpn_sign_in_status(uid: u32) -> Response {
    if !is_vpn_instance(uid) {
        eprintln!("portcullisd: VPN sign-in status REFUSED to uid {uid}: not a VPN sign-in window");
        return Response::Error { message: "only a VPN sign-in window's instance".into() };
    }
    let reg = load_registry();
    let Some(owner) = reg.resolve(uid).map(|(u, _)| u.to_string()) else {
        return Response::Error { message: "not a launched app".into() };
    };
    let requested = VPN_SIGN_INS.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|(o, _, _)| *o == owner).cloned();
    let Some((_, sign_in, attempt)) = requested else {
        return Response::Error { message: "no VPN sign-in was asked for".into() };
    };
    if vpn_sign_in_attempt(&sign_in.profile).as_deref() != Some(attempt.as_str()) {
        return Response::Error { message: "the VPN sign-in attempt ended".into() };
    }
    Response::VpnSignInStatus { profile: sign_in.profile, start_url: sign_in.start_url, final_url: sign_in.final_url, attempt }
}

/// PortalStatus: the sign-in window's network, for that window only.
fn handle_portal_status(uid: u32) -> Response {
    if !is_portal_instance(uid) {
        eprintln!("portcullisd: portal status REFUSED to uid {uid}: not the sign-in window");
        return Response::Error { message: "cap.network_portal.denied: only the sign-in window's instance".into() };
    }
    match portcullisd::portal::info() {
        Ok(i) => {
            let network = i.ssid.as_deref().map(|b| String::from_utf8_lossy(b).into_owned())
                .filter(|n| !n.trim().is_empty())
                // A wired network has no name of its own; an interface's
                // name (vtnet1) means nothing to the person.
                .unwrap_or_else(|| "the wired network".to_string());
            Response::PortalStatus { start_url: i.start_url(), online: i.online == atrium_netconf::Online::Yes, network }
        }
        Err(why) => Response::Error { message: why },
    }
}

/// NewWindowIn: the caller's own app in another of its person's personas,
/// started or brought forward. Only an instanced app, only a persona the
/// person has, never the caller's own; nothing goes with it, so no switch
/// is asked. The press is spent last (purpose LINK, which also lets the
/// running instance's window come forward).
fn handle_new_window_in(uid: u32, persona: &str, activation: [u8; 16], shared: &Mutex<Tenants>) -> Response {
    use portcullisd::opens;
    let refuse = |why: String| {
        eprintln!("portcullisd: new window REFUSED to uid {uid}: {why}");
        Response::Error { message: why }
    };
    let reg = load_registry();
    let (Some((owner, app)), Some(here)) = (reg.resolve(uid).map(|(u, a)| (u.to_string(), a.to_string())), reg.persona(uid).map(|p| p.map(String::from))) else {
        return refuse("not a launched app".into());
    };
    if launch::not_instanced(&app).is_some() {
        return refuse(format!("{app} runs once for every persona"));
    }
    if matches!(here.as_deref(), Some(portcullis_peer::PORTAL_PERSONA | portcullis_peer::VPN_PERSONA)) {
        return refuse("the sign-in window opens no other windows".into());
    }
    let target = (persona != portcullis_policy::PERSONAL).then(|| persona.to_string());
    if target == here {
        return refuse("that is this window's persona".into());
    }
    let known = shared.lock().unwrap_or_else(|e| e.into_inner()).get_or_load(&owner)
        .map(|tp| !tp.policy.personas.is_empty() && tp.policy.check_personas().is_ok() && tp.policy.has_persona(target.as_deref())).unwrap_or(false);
    if !known {
        return refuse(format!("{persona}: not one of {owner}'s personas"));
    }
    if let Err(why) = opens::spend_press_for(&opens::activation_socket(), activation, uid, fresco_protocol::activation::purpose::LINK) {
        return refuse(why);
    }
    let shown = target.as_deref().unwrap_or(portcullis_policy::PERSONAL);
    if launch::running_jid_instance(&app, target.as_deref()).is_some() {
        match launch::instance_uid(&owner, &app, target.as_deref()).map(|u| opens::raise(&opens::activation_socket(), u)) {
            Some(Ok(w)) => eprintln!("portcullisd: a new window for {app} in {shown}: running, window {w} brought forward (from {}, for {owner})", here.as_deref().unwrap_or("personal")),
            Some(Err(why)) => eprintln!("portcullisd: a new window for {app} in {shown}: running, not brought forward: {why}"),
            None => eprintln!("portcullisd: a new window for {app} in {shown}: running, no uid for it"),
        }
    } else {
        eprintln!("portcullisd: a new window for {app} in {shown}: starting it (from {}, for {owner})", here.as_deref().unwrap_or("personal"));
        start_for_instance(&app, target.as_deref(), &owner, "a new window");
    }
    Response::Ok
}

/// Access, RemoveAccess and ApproveAccess (curia.md §13). `uid` is the
/// caller's peer uid.
fn handle_access(uid: u32, req: Request, shared: &Mutex<Tenants>) -> Response {
    use portcullisd::{access, opens};
    let refuse = |why: String| {
        eprintln!("portcullisd: app access REFUSED to uid {uid}: {why}");
        Response::Error { message: why }
    };
    let Some((owner, caller)) = portcullis_peer::AppRegistry::load(portcullis_peer::DEFAULT_REGISTRY)
        .unwrap_or_default().resolve(uid).map(|(o, a)| (o.to_string(), a.to_string())) else {
        return refuse("cap.app_grants.denied: caller is not a launched app".into());
    };
    if !launch::app_manages_grants(&caller) {
        return refuse(format!("cap.app_grants.denied: {caller} may not see or change apps' access"));
    }
    let system = Policy::load_system(std::path::Path::new(SYSTEM_PATH));
    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
    let tp = match s.get_or_load(&owner) {
        Ok(tp) => tp,
        Err(e) => return refuse(format!("policy for {owner}: {e}")),
    };
    let row = |text: &str, m: &portcullis_toml::Manifest, user: &Policy| {
        access::entry(m, &hash_manifest(text.as_bytes()), user, system.as_ref())
    };
    match req {
        Request::Access => {
            let mut apps: Vec<_> = launch::installed_texts().iter().map(|(_, t, m)| row(t, m, &tp.policy)).collect();
            access::sort(&mut apps);
            eprintln!("portcullisd: access -> {caller} (uid {uid}) for {owner}: {} app(s)", apps.len());
            Response::Access { apps }
        }
        Request::RemoveAccess { app_id } => {
            let Some((_, t, m)) = launch::installed_texts().into_iter().find(|(_, _, m)| m.app.id == app_id) else {
                // Uninstalled: its grant can still go.
                if tp.policy.grants.remove(&app_id).is_none() {
                    return refuse(format!("{app_id} is not installed"));
                }
                return match tp.policy.save(&tp.policy_path.clone()) {
                    Ok(()) => Response::Ok,
                    Err(e) => refuse(format!("save: {e}")),
                };
            };
            if let Some(why) = access::may_remove(&row(&t, &m, &tp.policy)) {
                return refuse(why);
            }
            tp.policy.grants.remove(&app_id);
            match tp.policy.save(&tp.policy_path.clone()) {
                Ok(()) => {
                    eprintln!("portcullisd: {owner} removed {app_id}'s access (from {caller})");
                    Response::Ok
                }
                Err(e) => refuse(format!("save: {e}")),
            }
        }
        Request::ApproveAccess { app_id, activation } => {
            let Some((tree, t, m)) = launch::installed_texts().into_iter().find(|(_, _, m)| m.app.id == app_id) else {
                return refuse(format!("{app_id} is not installed"));
            };
            if let Some(why) = access::may_approve(&row(&t, &m, &tp.policy)) {
                return refuse(why);
            }
            // The launch's own trust gate: never allow a manifest it would refuse.
            if let Err(why) = manifest_trust::verify(&tree, &t) {
                return refuse(why);
            }
            // Last of the checks: a refusal above doesn't spend the press.
            if let Err(why) = opens::spend_press_for(&opens::activation_socket(), activation, uid, fresco_protocol::activation::purpose::APPROVE) {
                return refuse(why);
            }
            tp.policy.grants.insert(app_id.clone(), Grant {
                manifest_hash: hash_manifest(t.as_bytes()),
                granted_at: now_iso8601(),
                capabilities: m.capabilities.clone(),
            });
            match tp.policy.save(&tp.policy_path.clone()) {
                Ok(()) => {
                    eprintln!("portcullisd: {owner} allowed {app_id} (from {caller})");
                    Response::Ok
                }
                Err(e) => refuse(format!("save: {e}")),
            }
        }
        Request::Links => {
            use portcullis_policy::links::LinkDefaults;
            let d = LinkDefaults::load(&LinkDefaults::user_path(&owner)).unwrap_or_default();
            let schemes = launch::link_schemes().into_iter().map(|scheme| {
                let apps = launch::apps_taking_link(&scheme);
                let def = d.app_for(&scheme, |id| apps.iter().any(|(a, _)| a == id));
                let yours = def.is_some() && d.chosen.get(&scheme) == def.as_ref();
                let apps = apps.into_iter().map(|(id, name)| portcullis_ipc::LinkChoice { default: def.as_deref() == Some(id.as_str()), id, name }).collect();
                portcullis_ipc::LinkScheme { scheme, apps, yours }
            }).collect::<Vec<_>>();
            eprintln!("portcullisd: links -> {caller} (uid {uid}) for {owner}: {} scheme(s)", schemes.len());
            Response::Links { schemes }
        }
        Request::SetLinkDefault { scheme, app, activation } => {
            use portcullis_policy::links::LinkDefaults;
            if let Err(why) = atrium_links::declarable(&scheme) {
                return refuse(why);
            }
            if let Some(a) = &app {
                if !launch::apps_taking_link(&scheme).iter().any(|(id, _)| id == a) {
                    return refuse(format!("{a} doesn't open {scheme} links"));
                }
            }
            // Last of the checks: a refusal above doesn't spend the press.
            if let Err(why) = opens::spend_press_for(&opens::activation_socket(), activation, uid, fresco_protocol::activation::purpose::APPROVE) {
                return refuse(why);
            }
            let p = LinkDefaults::user_path(&owner);
            let mut d = LinkDefaults::load(&p).unwrap_or_default();
            let r = match &app {
                Some(a) => d.choose(&scheme, a),
                None => {
                    d.chosen.remove(&scheme);
                    Ok(())
                }
            };
            match r.map_err(std::io::Error::other).and_then(|_| d.save(&p)) {
                Ok(()) => {
                    eprintln!("portcullisd: {owner}'s default for {scheme} links is now {} (from {caller})", app.as_deref().unwrap_or("ask each time"));
                    Response::Ok
                }
                Err(e) => refuse(format!("save: {e}")),
            }
        }
        Request::ShareTargets => {
            use portcullis_policy::share::SharePrefs;
            let d = SharePrefs::load(&SharePrefs::user_path(&owner)).unwrap_or_default();
            let targets = d.arrange(launch::share_targets_for(&owner), |(id, _)| id.as_str()).into_iter()
                .map(|(id, name)| portcullis_ipc::ShareTargetRow { hidden: d.hidden.contains(&id), id, name })
                .collect::<Vec<_>>();
            eprintln!("portcullisd: share targets -> {caller} (uid {uid}) for {owner}: {}", targets.len());
            Response::ShareTargets { targets }
        }
        Request::SetShareTargets { order, hidden, activation } => {
            use portcullis_policy::share::SharePrefs;
            let known: Vec<String> = launch::share_targets_for(&owner).into_iter().map(|(id, _)| id).collect();
            let d = SharePrefs::chosen(&order, &hidden, &known);
            // Last of the checks: a refusal above doesn't spend the press.
            if let Err(why) = opens::spend_press_for(&opens::activation_socket(), activation, uid, fresco_protocol::activation::purpose::APPROVE) {
                return refuse(why);
            }
            match d.save(&SharePrefs::user_path(&owner)) {
                Ok(()) => {
                    eprintln!("portcullisd: {owner}'s share sheet: order {:?}, hidden {:?} (from {caller})", d.order, d.hidden);
                    Response::Ok
                }
                Err(e) => refuse(format!("save: {e}")),
            }
        }
        Request::PersonaVpns => {
            let routes: Vec<(String, String)> = tp.policy.personas.iter().filter_map(|(n, d)| Some((n.clone(), d.vpn.clone()?))).collect();
            Response::PersonaVpns { routes }
        }
        Request::Crossings => {
            let (personas, on) = portcullisd::crossing::listing(&tp.policy);
            eprintln!("portcullisd: crossings -> {caller} (uid {uid}) for {owner}: {} persona(s), {} switch(es) on", personas.len(), on.len());
            Response::Crossings { personas, on }
        }
        _ => Response::Error { message: "not an access request".into() },
    }
}

/// Settings adds, recolours or removes a persona (personas.md §2, §4.1):
/// `app-grants` by peer uid, about the person the app runs for, with a press
/// portcullisd spends (APPROVE). Removing also asks the person on
/// Sigillum's sheet, with no lock held, and is refused while any of the
/// persona's apps runs: their data is about to go.
fn handle_persona_change(uid: u32, req: Request, shared: &Mutex<Tenants>, verify: &dyn portcullisd::crossing::Verify) -> Response {
    use portcullisd::opens;
    let refuse = |why: String| {
        eprintln!("portcullisd: persona change REFUSED to uid {uid}: {why}");
        Response::Error { message: why }
    };
    let Some((owner, caller)) = portcullis_peer::AppRegistry::load(portcullis_peer::DEFAULT_REGISTRY)
        .unwrap_or_default().resolve(uid).map(|(o, a)| (o.to_string(), a.to_string())) else {
        return refuse("cap.app_grants.denied: caller is not a launched app".into());
    };
    if !launch::app_manages_grants(&caller) {
        return refuse(format!("cap.app_grants.denied: {caller} may not change personas"));
    }
    let (name, activation) = match &req {
        Request::AddPersona { name, activation, .. } | Request::SetPersonaColour { name, activation, .. } | Request::RemovePersona { name, activation } | Request::SetPersonaVpn { name, activation, .. } => (name.clone(), *activation),
        _ => return refuse("not a persona change".into()),
    };
    // Checked on a copy before the press is spent: a refusal spends nothing.
    let check = |p: &mut Policy| -> Result<bool, String> {
        match &req {
            Request::AddPersona { name, colour, .. } => p.add_persona(name, colour).map(|_| true),
            Request::SetPersonaColour { name, colour, .. } => p.set_persona_colour(name, colour),
            Request::RemovePersona { name, .. } => p.remove_persona(name).map(|_| true),
            Request::SetPersonaVpn { name, vpn, .. } => p.set_persona_vpn(name, vpn.as_deref()),
            _ => unreachable!(),
        }
    };
    {
        let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
        let tp = match s.get_or_load(&owner) {
            Ok(tp) => tp,
            Err(e) => return refuse(format!("policy for {owner}: {e}")),
        };
        let mut copy = Policy::default();
        copy.personas = tp.policy.personas.clone();
        copy.crossings = tp.policy.crossings.clone();
        if let Err(why) = check(&mut copy) {
            return refuse(why);
        }
    }
    let removing = matches!(req, Request::RemovePersona { .. });
    if matches!(req, Request::SetPersonaVpn { .. }) {
        // Its instances took their routing table when they started.
        let running = launch::running_in_persona(&owner, &name);
        if !running.is_empty() {
            return refuse(format!("{} is open in {name}: close it first", running.join(", ")));
        }
    }
    if removing {
        let running = launch::running_in_persona(&owner, &name);
        if !running.is_empty() {
            return refuse(format!("{} is still open in {name}: close it first", running.join(", ")));
        }
        let sticks = portcullisd::home::mounted_sticks(&name);
        if !sticks.is_empty() {
            return refuse(format!("{} of {name} is in use: eject it first", sticks.join(", ")));
        }
    }
    if let Err(why) = opens::spend_press_for(&opens::activation_socket(), activation, uid, fresco_protocol::activation::purpose::APPROVE) {
        return refuse(why);
    }
    if removing {
        eprintln!("portcullisd: {owner} removing persona {name}: asking Sigillum");
        if let Err(why) = verify.verify(&owner, sigillum_proto::client::Purpose::RemovePersona { name: name.clone() }) {
            return refuse(why);
        }
        // Again: an app may have started while the sheet was up.
        let running = launch::running_in_persona(&owner, &name);
        if !running.is_empty() {
            return refuse(format!("{} opened in {name} meanwhile: close it first; nothing was removed", running.join(", ")));
        }
        let sticks = portcullisd::home::mounted_sticks(&name);
        if !sticks.is_empty() {
            return refuse(format!("{} of {name} was plugged in meanwhile: eject it first; nothing was removed", sticks.join(", ")));
        }
    }
    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
    let tp = match s.get_or_load(&owner) {
        Ok(tp) => tp,
        Err(e) => return refuse(format!("policy for {owner}: {e}")),
    };
    match check(&mut tp.policy) {
        Ok(false) => return Response::Ok,
        Ok(true) => {}
        Err(why) => return refuse(why),
    }
    if let Err(e) = tp.policy.save(&tp.policy_path.clone()) {
        return refuse(format!("save: {e}"));
    }
    publish_colours(&owner, &tp.policy);
    if removing {
        calendar::stop(&owner, Some(&name));
        let gone = portcullis_overlay::remove_persona_places(&owner, &name);
        // Its files too (personas.md §4.1): encrypted, the key goes and the
        // kernel crypto-erases them (tessera-encryption.md E4.6).
        let files = match portcullisd::home::remove_persona(&owner, &name) {
            Ok(true) => "its folders deleted",
            Ok(false) => "it had no folders",
            Err(e) => {
                eprintln!("portcullisd: {owner}'s {name} folders: {e}");
                "its folders NOT deleted"
            }
        };
        eprintln!("portcullisd: {owner} removed persona {name} (from {caller}, verified): {} app folder(s) deleted, {files}", gone.len());
    } else {
        let folders_ready = match portcullisd::home::ensure_persona(&owner, &name) {
            Ok(_) => true,
            Err(e) => {
                eprintln!("portcullisd: {owner}'s {name} folders not made: {e}");
                false
            }
        };
        eprintln!("portcullisd: {owner}'s persona {name} is now {:?}, vpn {:?} (from {caller})", tp.policy.personas.get(&name).map(|d| d.colour.as_str()), tp.policy.personas.get(&name).and_then(|d| d.vpn.as_deref()));
        drop(s);
        if folders_ready && matches!(req, Request::AddPersona { .. }) {
            launch::refresh_files_groups(&owner);
        }
    }
    Response::Ok
}

/// The launch question (personas.md §3): the person's personas, Personal
/// first, and for `app_id` the one last used and whether it spans personas.
/// Only for a launcher, Settings, or the person's own CLI.
fn handle_persona_list(uid: u32, owner: &str, app_id: Option<&str>, shared: &Mutex<Tenants>) -> Response {
    let caller = portcullis_peer::AppRegistry::load(portcullis_peer::DEFAULT_REGISTRY)
        .unwrap_or_default().resolve(uid).map(|(_, a)| a.to_string());
    let allowed = match &caller {
        Some(app) => launch::app_may_launch_apps(app) || launch::app_manages_grants(app),
        None => portcullis_overlay::home_of(owner).is_some() && uid >= 1000,
    };
    if !allowed {
        eprintln!("portcullisd: persona list REFUSED to uid {uid} ({})", caller.as_deref().unwrap_or("?"));
        return Response::Error { message: format!("cap.app_launch.denied: only a launcher, Settings or the person lists personas (caller {})",
                                                   caller.as_deref().unwrap_or("unknown")) };
    }
    let mut personas = vec![portcullis_ipc::PersonaOption { name: portcullis_policy::PERSONAL.into(), colour: None }];
    let dir = {
        let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
        match s.get_or_load(owner) {
            Ok(tp) => {
                let (named, _) = portcullisd::crossing::listing(&tp.policy);
                personas.extend(named.into_iter().map(|(name, colour)| portcullis_ipc::PersonaOption { name, colour: Some(colour) }));
                tp.policy_path.parent().map(|d| d.to_path_buf())
            }
            Err(e) => return Response::Error { message: format!("policy for {owner}: {e}") },
        }
    };
    let (last, spans) = match app_id {
        Some(a) => {
            let last = dir.as_deref().and_then(|d| portcullisd::persona_last::get(d, a))
                .filter(|p| personas.iter().any(|q| &q.name == p));
            (last, launch::not_instanced(a).is_some())
        }
        None => (None, false),
    };
    Response::PersonaList { personas, last, spans }
}

/// Remember the persona `app_id` ran in, for the next launch question; only
/// for a person with a named persona (nothing is asked otherwise).
fn remember_persona(user: &str, app_id: &str, persona: Option<&str>, shared: &Mutex<Tenants>) {
    let dir = {
        let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
        match s.get_or_load(user) {
            Ok(tp) if tp.policy.check_personas().is_ok() && !tp.policy.personas.is_empty() => tp.policy_path.parent().map(|d| d.to_path_buf()),
            _ => None,
        }
    };
    if let Some(d) = dir {
        if let Err(e) = portcullisd::persona_last::set(&d, app_id, persona.unwrap_or(portcullis_policy::PERSONAL)) {
            eprintln!("portcullisd: could not remember {user}'s persona for {app_id}: {e}");
        }
    }
}

/// Settings turns a crossing switch (personas.md §6): `app-grants` by peer
/// uid, about the person that app runs for, with a press portcullisd
/// spends (APPROVE); turning one on then asks the person on Sigillum's
/// sheet, which can take minutes, so no lock is held while it does, and
/// the policy is read again after.
#[allow(clippy::too_many_arguments)]
fn handle_set_crossing(uid: u32, from: &str, to: &str, kind: &str, on: bool, activation: [u8; 16], shared: &Mutex<Tenants>, verify: &dyn portcullisd::crossing::Verify) -> Response {
    use portcullisd::{crossing, opens};
    let refuse = |why: String| {
        eprintln!("portcullisd: crossing switch REFUSED to uid {uid}: {why}");
        Response::Error { message: why }
    };
    let Some((owner, caller)) = portcullis_peer::AppRegistry::load(portcullis_peer::DEFAULT_REGISTRY)
        .unwrap_or_default().resolve(uid).map(|(o, a)| (o.to_string(), a.to_string())) else {
        return refuse("cap.app_grants.denied: caller is not a launched app".into());
    };
    if !launch::app_manages_grants(&caller) {
        return refuse(format!("cap.app_grants.denied: {caller} may not change crossings"));
    }
    {
        let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
        match s.get_or_load(&owner) {
            Ok(tp) => {
                if let Err(why) = crossing::check(&tp.policy, from, to, kind) {
                    return refuse(why);
                }
            }
            Err(e) => return refuse(format!("policy for {owner}: {e}")),
        }
    }
    // Last of the checks: a refusal above doesn't spend the press.
    if let Err(why) = opens::spend_press_for(&opens::activation_socket(), activation, uid, fresco_protocol::activation::purpose::APPROVE) {
        return refuse(why);
    }
    if on {
        eprintln!("portcullisd: {owner} turning on {kind} from {from} to {to}: asking Sigillum");
        if let Err(why) = verify.verify(&owner, sigillum_proto::client::Purpose::Crossing { from: from.into(), to: to.into(), kind: kind.into() }) {
            return refuse(why);
        }
    }
    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
    let tp = match s.get_or_load(&owner) {
        Ok(tp) => tp,
        Err(e) => return refuse(format!("policy for {owner}: {e}")),
    };
    match tp.policy.set_crossing(from, to, kind, on) {
        Ok(false) => Response::Ok,
        Ok(true) => match tp.policy.save(&tp.policy_path.clone()) {
            Ok(()) => {
                eprintln!("portcullisd: {owner}'s switch for {kind} from {from} to {to} is now {} (from {caller}{})", if on { "on" } else { "off" }, if on { ", verified" } else { "" });
                Response::Ok
            }
            Err(e) => refuse(format!("save: {e}")),
        },
        Err(why) => refuse(why),
    }
}

/// Console (homes.md §4). `uid` is the caller's peer uid; `owner` the
/// person it belongs to, whose app instance the shell joins.
/// Workspaces (homes.md §5). Asked by a launcher (Forum) for the person it
/// runs for, or by the person themselves (the CLI), never from inside an
/// app or a workspace: a workspace's settings aren't changed from inside it.
/// Run (homes.md §5.3, W5): install what workspace `ws` built as a dev app
/// and start it. Asked by the Workspaces app or a launcher (with a press) or
/// by the person's own CLI.
/// Who may Run a workspace's builds: `Ok(true)` an app (it needs a press),
/// `Ok(false)` the person's own CLI.
fn run_caller(uid: u32, owner: &str, ws: &str) -> Result<bool, String> {
    let caller = portcullis_peer::AppRegistry::load(portcullis_peer::DEFAULT_REGISTRY)
        .unwrap_or_default().resolve(uid).map(|(_, a)| a.to_string());
    match &caller {
        // The Terminal too, for `make run` in a workspace's console
        // (atrium-run, OSC 7770 `run`), on the person's press.
        Some(a) if launch::app_may_launch_apps(a) || launch::app_manages_workspaces(a) || launch::app_shows_consoles(a) => Ok(true),
        // An IDE (`workspace-exec`), in a workspace the person gave it
        // (dev-tools.md §3.4: its Run of a windowed app is W5's).
        Some(a) if launch::app_execs_in_workspaces(a) && ide::picked(owner, a).iter().any(|w| w == ws) => Ok(true),
        Some(a) if launch::app_execs_in_workspaces(a) => Err(format!("cap.workspace.denied: {a} wasn't given workspace {ws}")),
        Some(a) => Err(format!("cap.workspace.denied: {a} may not Run builds")),
        None if portcullis_overlay::home_of(owner).is_some() && uid >= 1000 => Ok(false),
        None => Err("cap.workspace.denied: not a person or a launcher".into()),
    }
}

/// How long a debug run waits for its debugger.
const DEBUG_WAIT_MS: i32 = 5 * 60 * 1000;

fn handle_run(uid: u32, owner: &str, ws: &str, project: &str, activation: [u8; 16], debug: bool, shared: &Mutex<Tenants>) -> Response {
    let what = if debug { "Debug" } else { "Run" };
    let refuse = |why: String| {
        eprintln!("portcullisd: {what} of {ws}/{project} for {owner} REFUSED: {why}");
        Response::Error { message: why }
    };
    let app = match run_caller(uid, owner, ws) {
        Ok(a) => a,
        Err(why) => return refuse(why),
    };
    let p = match dev::prepare(owner, ws, project) {
        Ok(p) => p,
        Err(why) => return refuse(why),
    };
    if launch::running_jid(&p.id).is_some() {
        return refuse(format!("{} is running: close it first", p.id));
    }
    /* The debugger's way in: a socket listening on the workspace's own
     * loopback, so only what runs in the workspace reaches it. */
    let listener = if debug {
        let jid = match workspace::start(owner, ws) {
            Ok(r) => r.jid,
            Err(why) => return refuse(why),
        };
        match egress::bind_in(jid, 0) {
            Ok(l) => Some(l),
            Err(why) => return refuse(why),
        }
    } else {
        None
    };
    // Last of the checks: a refusal above doesn't spend the press.
    if app {
        if let Err(why) = portcullisd::opens::spend_press_for(&portcullisd::opens::activation_socket(), activation, uid,
                                                              fresco_protocol::activation::purpose::APPROVE) {
            return refuse(why);
        }
    }
    workspace::hide_bundle(owner, ws, &p.id);
    let installed = dev::install(&p);
    workspace::show_bundle(owner, ws, &p.id);
    if let Err(why) = installed {
        return refuse(why);
    }
    let m = match portcullis_toml::Manifest::from_str(&p.manifest) {
        Ok(m) => m,
        Err(e) => return refuse(e.to_string()),
    };
    /* The Run press is the person's decision on what this manifest
     * declares (nothing system-only: dev::prepare refused that). */
    {
        let mut t = shared.lock().unwrap_or_else(|e| e.into_inner());
        let tp = match t.get_or_load(owner) {
            Ok(tp) => tp,
            Err(e) => return refuse(format!("policy for {owner}: {e}")),
        };
        tp.policy.grants.insert(p.id.clone(), Grant {
            manifest_hash: hash_manifest(p.manifest.as_bytes()),
            granted_at: now_iso8601(),
            capabilities: m.capabilities.clone(),
        });
        if let Err(e) = tp.policy.save(&tp.policy_path.clone()) {
            return refuse(format!("save: {e}"));
        }
    }
    let caps: Vec<String> = p.manifest.parse::<toml::Value>().ok()
        .and_then(|v| v.get("capabilities").and_then(|c| c.as_table()).map(|t| t.keys().cloned().collect()))
        .unwrap_or_default();
    eprintln!("portcullisd: {what}: {} for {owner} from {ws}/{project}, granted {caps:?}", p.id);
    let Some(listener) = listener else {
        start_for(&p.id, owner, "a Run");
        return Response::WorkspaceRan { app_id: p.id, capabilities: caps };
    };
    let port = match local_port(&listener) {
        Ok(p) => p,
        Err(why) => return refuse(why),
    };
    let (id, o) = (p.id.clone(), owner.to_string());
    thread::spawn(move || {
        let mut pfd = libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one pollfd we own, then accept on the same descriptor.
        let conn = unsafe {
            if libc::poll(&mut pfd, 1, DEBUG_WAIT_MS) != 1 {
                eprintln!("portcullisd: debug of {id}: no debugger came; not started");
                return;
            }
            let c = libc::accept4(listener.as_raw_fd(), std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC);
            if c < 0 {
                eprintln!("portcullisd: debug of {id}: accept: {}", std::io::Error::last_os_error());
                return;
            }
            <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(c)
        };
        drop(listener);
        eprintln!("portcullisd: debug of {id}: a debugger connected");
        // Its windows say so while it runs (frescod reads the mark).
        dev::mark_debugging(&id, true);
        match launch::launch_debug(&id, &o, conn) {
            Ok(out) => eprintln!("portcullisd: {id} (debug) exited {:?}", out.exit_code),
            Err(e) => eprintln!("portcullisd: {id} (debug) failed at {}: {}", e.stage(), e.message()),
        }
        dev::mark_debugging(&id, false);
    });
    let address = format!("127.0.0.1:{port}");
    let program = format!("{}/{}/{}", dev::APPS_DIR, p.id, m.entry());
    Response::WorkspaceDebugging { app_id: p.id, capabilities: caps, address, program }
}

/// The port a listening socket has.
fn local_port(s: &OwnedFd) -> Result<u16, String> {
    // SAFETY: getsockname into a sockaddr_in we own.
    unsafe {
        let mut sin: libc::sockaddr_in = std::mem::zeroed();
        let mut len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
        if libc::getsockname(s.as_raw_fd(), (&mut sin as *mut libc::sockaddr_in).cast(), &mut len) != 0 {
            return Err(format!("getsockname: {}", std::io::Error::last_os_error()));
        }
        Ok(u16::from_be(sin.sin_port))
    }
}

/// The IDE asking (by its uid), if it is one.
fn ide_of(uid: u32) -> Result<String, String> {
    match load_registry().resolve(uid) {
        Some((_, a)) if launch::app_execs_in_workspaces(a) => Ok(a.to_string()),
        Some((_, a)) => Err(format!("cap.workspace_exec.denied: {a} has no workspace-exec")),
        None => Err("cap.workspace_exec.denied: not an app".into()),
    }
}

fn handle_pick(uid: u32, owner: &str, name: &str, activation: [u8; 16]) -> Response {
    let refuse = |why: String| {
        eprintln!("portcullisd: workspace pick of {name} for {owner} REFUSED: {why}");
        Response::Error { message: why }
    };
    let app = match ide_of(uid) {
        Ok(a) => a,
        Err(why) => return refuse(why),
    };
    if let Err(why) = workspace::work_dir(owner, name) {
        return refuse(why);
    }
    if let Err(why) = portcullisd::opens::spend_press_for(&portcullisd::opens::activation_socket(), activation, uid,
                                                          fresco_protocol::activation::purpose::APPROVE) {
        return refuse(why);
    }
    match ide::pick(owner, &app, name) {
        Ok(()) => {
            eprintln!("portcullisd: {app} was given workspace {name} of {owner}");
            Response::Ok
        }
        Err(why) => refuse(why),
    }
}

fn handle_ws_exec(uid: u32, owner: &str, name: &str, argv: &[String], cwd: &str, writer: &mut UnixStream) -> std::io::Result<()> {
    let refuse = |w: &mut UnixStream, why: String| {
        eprintln!("portcullisd: exec in workspace {name} of {owner} REFUSED: {why}");
        write_response(w, &Response::Error { message: why })
    };
    let in_app = load_registry().resolve(uid).is_some();
    let app = if in_app {
        let app = match ide_of(uid) {
            Ok(a) => a,
            Err(why) => return refuse(writer, why),
        };
        if !ide::picked(owner, &app).iter().any(|w| w == name) {
            return refuse(writer, format!("cap.workspace_exec.denied: {app} wasn't given workspace {name}"));
        }
        app
    } else if portcullis_overlay::home_of(owner).is_some() && uid >= 1000 {
        // The person's own CLI (`portcullis workspace exec`): their workspaces.
        format!("{owner}'s CLI")
    } else {
        return refuse(writer, "cap.workspace_exec.denied: not a person or an IDE".into());
    };
    let (pid, procdesc, stdin, output) = match ide::exec(owner, name, argv, cwd) {
        Ok(s) => s,
        Err(why) => return refuse(writer, why),
    };
    eprintln!("portcullisd: {app} runs {:?} in workspace {name} of {owner} (pid {pid})", argv.first().map(String::as_str).unwrap_or(""));
    write_response(writer, &Response::WorkspaceExecStarted { pid })?;
    // The caller's copies are all that remain: closing the descriptor ends it.
    portcullis_ipc::send_fds(writer, &[procdesc.as_raw_fd(), stdin.as_raw_fd(), output.as_raw_fd()])
}

fn handle_ws_admin(uid: u32, owner: &str, name: &str, argv: &[String], activation: [u8; 16], writer: &mut UnixStream) -> std::io::Result<()> {
    let refuse = |w: &mut UnixStream, why: String| {
        eprintln!("portcullisd: pkg in workspace {name} of {owner} REFUSED: {why}");
        write_response(w, &Response::Error { message: why })
    };
    let caller = load_registry()
        .resolve(uid).map(|(_, a)| a.to_string());
    let who = match &caller {
        Some(a) if launch::app_shows_consoles(a) || launch::app_manages_workspaces(a) => a.clone(),
        Some(a) if launch::app_execs_in_workspaces(a) && ide::picked(owner, a).iter().any(|w| w == name) => a.clone(),
        Some(a) => return refuse(writer, format!("cap.workspace_admin.denied: {a} may not run pkg in {name}")),
        None if portcullis_overlay::home_of(owner).is_some() && uid >= 1000 => format!("{owner}'s CLI"),
        None => return refuse(writer, "cap.workspace_admin.denied: not a person or an app".into()),
    };
    if let Err(why) = ide::check_pkg_argv(argv) {
        return refuse(writer, why);
    }
    if let Err(why) = workspace::work_dir(owner, name) {
        return refuse(writer, why);
    }
    // Last of the checks: a refusal above doesn't spend the press.
    if caller.is_some() {
        if let Err(why) = portcullisd::opens::spend_press_for(&portcullisd::opens::activation_socket(), activation, uid,
                                                              fresco_protocol::activation::purpose::APPROVE) {
            return refuse(writer, why);
        }
    }
    let (pid, procdesc, stdin, output) = match ide::admin(owner, name, argv) {
        Ok(s) => s,
        Err(why) => return refuse(writer, why),
    };
    eprintln!("portcullisd: {who} runs {} as root in workspace {name} of {owner} (pid {pid})", argv.join(" "));
    write_response(writer, &Response::WorkspaceExecStarted { pid })?;
    portcullis_ipc::send_fds(writer, &[procdesc.as_raw_fd(), stdin.as_raw_fd(), output.as_raw_fd()])
}

fn handle_workspace_credentials(uid: u32, req: Request) -> Response {
    use portcullisd::opens;
    let handle = || -> Result<Response, String> {
        let registry = portcullis_peer::AppRegistry::load(portcullis_peer::DEFAULT_REGISTRY)
            .map_err(|_| "Could not verify Settings' launch identity")?;
        let (owner, _) = registry.resolve(uid)
            .filter(|(_, caller)| launch::app_manages_grants(caller))
            .ok_or("Only Settings can manage workspace credentials")?;
        let (workspace, persona) = match &req {
            Request::WorkspaceCredentials { workspace, persona }
            | Request::SetWorkspaceCredential { workspace, persona, .. } => (workspace, persona),
            _ => return Err("Unexpected workspace credential request".into()),
        };
        let check_persona = || {
            if workspace::credential_persona(owner, workspace)? == *persona {
                Ok(())
            } else {
                Err("Workspace persona changed. Reopen Workspace access".to_string())
            }
        };
        check_persona()?;
        if let Request::SetWorkspaceCredential { credential, granted, activation, .. } = &req {
            opens::spend_press_for(&opens::activation_socket(), *activation, uid,
                                   fresco_protocol::activation::purpose::APPROVE)?;
            let _scope = workspace::CREDENTIAL_SCOPE.lock().unwrap_or_else(|e| e.into_inner());
            check_persona()?;
            ssh_agent::set_grant(owner, persona, workspace, credential, *granted)?;
            return Ok(Response::Ok);
        }
        let credentials = ssh_agent::credentials(owner, persona, workspace)?;
        Ok(Response::WorkspaceCredentials { credentials })
    };
    handle().unwrap_or_else(|message| Response::Error { message })
}

fn handle_workspace(uid: u32, owner: &str, req: Request, shared: &Mutex<Tenants>) -> Response {
    let reg = load_registry();
    let caller = reg.resolve(uid).map(|(_, a)| a.to_string());
    let allowed = match &caller {
        // The terminal lists them for its switcher, and Settings for
        // workspace access. Neither permission authorizes workspace changes.
        Some(app) => launch::app_may_launch_apps(app) || launch::app_manages_workspaces(app)
            || (matches!(req, Request::Workspaces) && (launch::app_shows_consoles(app) || launch::app_execs_in_workspaces(app)
                || launch::app_manages_grants(app))),
        // Not an app: a person, with a home, asking about their own.
        None => portcullis_overlay::home_of(owner).is_some() && uid >= 1000,
    };
    if !allowed {
        eprintln!("portcullisd: workspaces REFUSED to uid {uid} ({})", caller.as_deref().unwrap_or("?"));
        return Response::Error { message: format!("cap.workspace.denied: only a launcher or the person manages workspaces (caller {})",
                                                   caller.as_deref().unwrap_or("unknown")) };
    }
    let done = match req {
        Request::Workspaces => Ok(()),
        Request::WorkspaceNew { name, toolchain, network, uses } => workspace::create(owner, workspace::Record {
            name, toolchain, persona: "personal".into(), network: network.unwrap_or_else(|| "outbound".into()), uses, object_cache: false, layers: vec![], hosts: vec![],
        }).map(|_| ()),
        Request::WorkspaceSet { name, network, uses, object_cache, hosts } => workspace::set(owner, &name, network, uses, object_cache, hosts).map(|_| ()),
        Request::WorkspaceDuplicate { name, new_name } => workspace::duplicate(owner, &name, &new_name).map(|_| ()),
        Request::WorkspaceStop { name } => {
            if workspace::valid_name(&name) {
                workspace::stop(owner, &name);
            }
            Ok(())
        }
        /* Deleting loses the person's files: a press on Delete, confirmed by
         * frescod, as Settings' Allow is (purpose APPROVE). */
        Request::WorkspaceDelete { name, activation } => workspace::check_delete(owner, &name)
            .and_then(|_| match dev::bundles_of(owner, &name).into_iter().find(|id| launch::running_jid(id).is_some()) {
                Some(id) => Err(format!("{id} is running: close it first")),
                None => Ok(()),
            })
            .and_then(|_| portcullisd::opens::spend_press_for(&portcullisd::opens::activation_socket(), activation, uid,
                                                               fresco_protocol::activation::purpose::APPROVE))
            .and_then(|_| workspace::delete(owner, &name))
            .map(|_| {
                // Its dev builds go with it, and their grants.
                let ids = dev::bundles_of(owner, &name);
                let mut t = shared.lock().unwrap_or_else(|e| e.into_inner());
                if let Ok(tp) = t.get_or_load(owner) {
                    for id in &ids {
                        tp.policy.grants.remove(id);
                    }
                    let _ = tp.policy.save(&tp.policy_path.clone());
                }
                for id in &ids {
                    dev::remove(id);
                }
            }),
        _ => Err("not a workspace request".into()),
    };
    if let Err(message) = done {
        eprintln!("portcullisd: workspace request of {owner} refused: {message}");
        return Response::Error { message };
    }
    match workspace::list(owner) {
        Ok(l) => Response::WorkspaceList {
            workspaces: l.into_iter().map(|(r, running)| portcullis_ipc::WorkspaceInfo {
                object_cache: r.object_cache, hosts: r.egress_hosts(), hosts_default: r.hosts.is_empty(),
                name: r.name, toolchain: r.toolchain, persona: r.persona, network: r.network, uses: r.uses, running,
            }).collect(),
            toolchains: workspace::toolchains().into_iter().map(|(id, name, available)| portcullis_ipc::Toolchain {
                id: id.to_string(), name: name.to_string(), available,
            }).collect(),
        },
        Err(message) => Response::Error { message },
    }
}

fn handle_console(uid: u32, owner: &str, app_id: &str, activation: [u8; 16], tab_of: Option<String>, new_window: bool, shared: &Mutex<Tenants>) -> Response {
    use portcullisd::opens::{self, Delivery, Hub};
    let refuse = |why: String| {
        eprintln!("portcullisd: console for {app_id} REFUSED to uid {uid}: {why}");
        Response::Error { message: why }
    };
    let caller = portcullis_peer::AppRegistry::load(portcullis_peer::DEFAULT_REGISTRY)
        .unwrap_or_default().resolve(uid).map(|(_, a)| a.to_string());
    /* A launcher opens any app's console; the terminal (`consoles`) only
     * another tab or window in an app this person has one in already: a tab
     * in one of the person's Stoa sessions for that app. */
    let stoa = console::stoad_up();
    let mine = if stoa { console::sessions(owner, app_id) } else { Vec::new() };
    // The Workspaces app opens workspaces' terminals as a launcher does.
    // An IDE (`workspace-exec`) opens the terminal of a workspace the
    // person picked in it (Artifex's terminal button, dev-tools-plan M11),
    // with a press like a launcher's.
    let ide_pick = |c: &str| app_id.strip_prefix(console::WORKSPACE_PREFIX)
        .is_some_and(|ws| launch::app_execs_in_workspaces(c) && ide::picked(owner, c).iter().any(|p| p == ws));
    let launcher = caller.as_deref().is_some_and(|c| launch::app_may_launch_apps(c)
        || (app_id.starts_with(console::WORKSPACE_PREFIX) && launch::app_manages_workspaces(c))
        || ide_pick(c)) && tab_of.is_none();
    let shows = caller.as_deref().is_some_and(launch::app_shows_consoles);
    /* homes.md §5.3 (W7): the terminal may open the empty console at any
     * time (it reaches nothing), and may switch one of its tabs into any
     * other console (`tab_of` none, not a new window): that one takes the
     * press like a launcher's. */
    let to_empty = app_id == empty::TARGET;
    let switch = shows && tab_of.is_none() && !new_window && !to_empty;
    let terminal = shows && (to_empty || switch || match &tab_of {
        Some(t) => mine.iter().any(|(n, _)| n == t),
        None => new_window && (console::live(owner, app_id) || !mine.is_empty()),
    });
    if !launcher && !terminal {
        return refuse(format!("cap.console.denied: only a launcher opens consoles, and the terminal more of one it shows (caller {})",
                              caller.as_deref().unwrap_or("is not a launched app")));
    }
    // A workspace (homes.md §5) or an app (§4): where the shell goes.
    let ws_groups: Vec<u32>;
    let mut ws_env: Vec<String> = Vec::new();
    let (jid, app_uid, gid, home, kind, title) = if to_empty {
        // Nothing to reach, so no press: a new tab or window of it is free.
        if let Err(why) = may_run_unasked(console::TERMINAL, owner, shared) {
            return refuse(format!("the terminal: {why}"));
        }
        let (jid, uid, gid) = match empty::start(owner) {
            Ok(v) => v,
            Err(why) => return refuse(why),
        };
        ws_groups = Vec::new();
        (jid, uid, gid, "/tmp".to_string(), console::Kind::App, "Terminal".to_string())
    } else if let Some(ws) = app_id.strip_prefix(console::WORKSPACE_PREFIX) {
        if !workspace::valid_name(ws) {
            return refuse(format!("{ws:?} is not a workspace name"));
        }
        // Its existence is checked before the press is spent.
        if let Err(why) = workspace::list(owner).and_then(|l| l.iter().any(|(r, _)| r.name == ws).then_some(()).ok_or_else(|| format!("{owner} has no workspace {ws}"))) {
            return refuse(why);
        }
        if let Err(why) = may_run_unasked(console::TERMINAL, owner, shared) {
            return refuse(format!("the terminal: {why}"));
        }
        if let Err(why) = opens::spend_press_for(&opens::activation_socket(), activation, uid, fresco_protocol::activation::purpose::CONSOLE) {
            return refuse(why);
        }
        let r = match workspace::start(owner, ws) {
            Ok(r) => r,
            Err(why) => return refuse(why),
        };
        ws_groups = vec![r.files];
        ws_env = r.env.clone();
        (r.jid, r.uid, r.gid, r.home, console::Kind::Workspace, format!("Workspace: {ws}"))
    } else {
        let Some(m) = launch::installed_manifest(app_id) else {
            return refuse(format!("{app_id} is not installed"));
        };
        let name = if m.app.name.is_empty() { m.app.id.clone() } else { m.app.name.clone() };
        if let Some(why) = launch::no_console(&m) {
            return refuse(why);
        }
        if let Err(why) = may_run_unasked(app_id, owner, shared) {
            return refuse(why);
        }
        if let Err(why) = may_run_unasked(console::TERMINAL, owner, shared) {
            return refuse(format!("the terminal: {why}"));
        }
        // Last of the checks: a refusal above doesn't spend the press.
        if let Err(why) = opens::spend_press_for(&opens::activation_socket(), activation, uid, fresco_protocol::activation::purpose::CONSOLE) {
            return refuse(why);
        }
        // The app's jail: joined if it runs, started if not.
        start_for(app_id, owner, "a console");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let jid = loop {
            if let Some(j) = launch::running_jid(app_id) {
                break j;
            }
            if std::time::Instant::now() > deadline {
                return refuse(format!("{name} did not start"));
            }
            thread::sleep(std::time::Duration::from_millis(100));
        };
        let Some(app_uid) = portcullis_peer::AppRegistry::load(portcullis_peer::DEFAULT_REGISTRY)
            .unwrap_or_default().uid_for(owner, app_id) else {
            return refuse(format!("{name} has no account for {owner}"));
        };
        // ★ Running, but for someone else: not this person's to enter.
        if !console::jail_runs_as(jid, app_uid) {
            return refuse(format!("{name} is open for another person"));
        }
        let Some(gid) = console::gid_of(app_uid) else {
            return refuse(format!("no group for uid {app_uid}"));
        };
        let app_root = PathBuf::from(launch::JAILS_DIR).join(app_id);
        if let Err(why) = console::mount_toolbox(&app_root) {
            return refuse(why);
        }
        // The shell goes in the console jail beside the app's, never in the
        // app's own (dev-tools-plan.md M0, P1): the app keeps its class.
        let jid = match console::console_jail(app_id, &app_root) {
            Ok(j) => j,
            Err(why) => return refuse(format!("the console's jail: {why}")),
        };
        ws_groups = Vec::new();
        (jid, app_uid, gid, "/nonexistent".to_string(), console::Kind::App, format!("Terminal: {name}"))
    };
    let spec = console::Spec { owner, jid, uid: app_uid, gid, home: &home, app_id, cols: 80, rows: 24, groups: &ws_groups, kind, env: &ws_env };
    let hub = Hub::global();
    if let Err(e) = hub.listen(console::TERMINAL) {
        return refuse(format!("the terminal's channel: {e}"));
    }
    let (fd, mime) = if stoa {
        /* Stoa: Terminal on the app comes back to a session left running
         * (the window was closed); otherwise a new shell, in a new session
         * or, for a tab, the terminal's. */
        let back = ((launcher || switch || to_empty) && !new_window).then(|| mine.iter().find(|(_, att)| !att).map(|(n, _)| n.clone())).flatten();
        let session = match back {
            Some(n) => {
                eprintln!("portcullisd: console {n} reattached for {owner}");
                n
            }
            None => {
                let n = tab_of.clone().unwrap_or_else(|| console::new_session(owner, app_id));
                let shell = match console::spawn_raw(&spec) {
                    Ok(s) => s,
                    Err(e) => return refuse(format!("the shell: {e}")),
                };
                if let Err(e) = console::adopt(&n, app_id, shell) {
                    return refuse(e);
                }
                eprintln!("portcullisd: console in {app_id} (jid {jid}) as uid {app_uid} for {owner}: {n}");
                n
            }
        };
        if tab_of.is_some() {
            // The terminal's link shows the session's new active window.
            return Response::ConsoleOpened { title };
        }
        match console::attach(&session) {
            Ok((fd, key)) => (fd, format!("{}; app={app_id}; session={session}; key={key}", console::STOA_MIME)),
            Err(e) => return refuse(e),
        }
    } else {
        match console::spawn(&spec) {
            Ok(m) => {
                eprintln!("portcullisd: console in {app_id} (jid {jid}) as uid {app_uid} for {owner}");
                (m, format!("{}; app={app_id}", console::MIME))
            }
            Err(e) => return refuse(format!("the shell: {e}")),
        }
    };
    match hub.deliver(owner, console::TERMINAL, &title, &mime, true, fd) {
        Ok(Delivery::Sent) => {}
        Ok(Delivery::Queued) => start_for(console::TERMINAL, owner, "a console"),
        Err(e) => return refuse(e.to_string()),
    }
    Response::ConsoleOpened { title }
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
                pcpu: None,
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

/// Per-person state of session components: STATE_ROOT/<person>/<component>,
/// mounted read-write at STATE_DEST inside the component's jail (forum.md §6.1).
const STATE_ROOT: &str = "/var/db/atrium/state";
const STATE_DEST: &str = "atrium/state";

/// One path segment: no separators, no dot names, nothing odd.
fn plain_segment(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s != "." && s != ".."
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-@".contains(&b))
}

/// Homes whose `.atrium/state/<component>` jaild's policy takes as a mount
/// source (`/usr/home/*/.atrium/state/*`, `/root/.atrium/state/*`): a
/// person whose home is elsewhere keeps state at STATE_ROOT.
fn home_for_state(home: &std::path::Path) -> bool {
    let s = home.to_string_lossy();
    s == "/root" || s.strip_prefix("/usr/home/").is_some_and(plain_segment)
}

/// Make (or re-own) STATE_ROOT/<owner>/<app_id> for `uid`, 0700, and return it.
/// The person's folder is root's (0755), so one component can't reach another's.
fn prepare_state_dir(owner: &str, app_id: &str, uid: u32) -> std::io::Result<String> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if !plain_segment(owner) || !plain_segment(app_id) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a plain name"));
    }
    /* homes.md §2: in the person's home, at ~/.atrium/state/<component>
     * (`.atrium` and `state` root's, 0755), when they have a home; the
     * machine-wide STATE_ROOT/<person> otherwise. A folder left at the old
     * place is moved in the first time. */
    let legacy = std::path::Path::new(STATE_ROOT).join(owner);
    let person = match portcullis_overlay::home_of(owner).and_then(|h| std::fs::canonicalize(h).ok()).filter(|h| home_for_state(h)) {
        Some(home) => {
            use std::os::unix::fs::MetadataExt;
            portcullis_overlay::apps_root(&home).map_err(std::io::Error::other)?;
            // In the person's Personal key group (E6), before it is mounted.
            portcullisd::home::ensure_app_data(owner, None).map_err(std::io::Error::other)?;
            let st = home.join(".atrium").join("state");
            match std::fs::symlink_metadata(&st) {
                // root's real folder, so the person can't point it at
                // another person's state (or anywhere else).
                Ok(m) if m.is_dir() && m.uid() == 0 => {}
                Ok(_) => return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied,
                    format!("{} is not root's own folder", st.display()))),
                Err(_) => {
                    std::fs::DirBuilder::new().mode(0o755).create(&st)?;
                    std::os::unix::fs::chown(&st, Some(0), Some(0))?;
                }
            }
            st
        }
        None => legacy.clone(),
    };
    std::fs::DirBuilder::new().recursive(true).mode(0o755).create(&person)?;
    let dir = person.join(app_id);
    let old = legacy.join(app_id);
    if person != legacy && std::fs::symlink_metadata(&dir).is_err() && std::fs::symlink_metadata(&old).is_ok_and(|m| m.is_dir()) {
        if std::fs::rename(&old, &dir).is_err() {
            let ok = std::process::Command::new("cp").arg("-Rp").arg(&old).arg(&dir).status().is_ok_and(|s| s.success());
            if ok {
                let _ = std::fs::remove_dir_all(&old);
            }
        }
    }
    match std::fs::symlink_metadata(&dir) {
        Ok(m) if !m.file_type().is_dir() =>
            return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "not a folder")),
        Ok(_) => {}
        Err(_) => std::fs::DirBuilder::new().mode(0o700).create(&dir)?,
    }
    std::os::unix::fs::chown(&dir, Some(uid), Some(uid))?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    Ok(dir.to_string_lossy().into_owned())
}

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
        Request::SessionEnded { user } => session_ended(&user),
        _ => Response::Error { message: "not a session request".into() },
    }
}

/// `user` is logging out (E6): their app overlays are in key groups that
/// lock with their Arca, so everything of theirs that holds one stops
/// first. Their apps' processes (every persona) are asked to end, then
/// killed; their workspaces stop; then each overlay is unmounted (a jail's
/// teardown may take a moment to release it). What stays in use is
/// reported: the kernel flushes it as the keys go, and its reads fail.
fn session_ended(user: &str) -> Response {
    if !portcullis_overlay::plain_segment(user) || portcullis_overlay::home_of(user).is_none() {
        return Response::Ok;
    }
    launch::stop_files_for_logout(user);
    let uids = launch::user_uids(user);
    launch::end_processes(&uids);
    calendar::stop(user, None);
    if let Ok(list) = workspace::list(user) {
        for (r, running) in list {
            if running {
                workspace::stop(user, &r.name);
            }
        }
    }
    let mut busy = Vec::new();
    for i in 0..20 {
        busy = portcullis_overlay::teardown_person(user);
        if busy.is_empty() {
            break;
        }
        if i == 10 {
            launch::end_processes(&uids);
        }
        thread::sleep(std::time::Duration::from_millis(250));
    }
    if busy.is_empty() {
        eprintln!("portcullisd: {user} logging out: {} app uid(s) stopped, overlays unmounted", uids.len());
        Response::Ok
    } else {
        eprintln!("portcullisd: {user} logging out: still in use: {}", busy.join("; "));
        Response::Error { message: format!("still in use: {}", busy.join("; ")) }
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
    // Marked `session` (frescod grants the bar's layer only to these, so an
    // app installed under the bar's id never gets it). A binding from before
    // the mark is not reused: the component gets a fresh, marked uid.
    let app_id = manifest.app_id.clone().unwrap_or_else(|| component_id.to_string());
    let owner_uid = {
        let _guard = match portcullis_peer::lock_registry(reg) {
            Ok(lock) => lock,
            Err(e) => return Response::Error { message: format!("session registry: {e}") },
        };
        match portcullis_peer::uid_for_session(reg, owner_name, &app_id) {
            Some(uid) => uid,
            None => {
                let uid = match portcullis_peer::allocate(reg) {
                    Ok(uid) => uid,
                    Err(e) => return Response::Error { message: format!("allocate session uid: {e}") },
                };
                if let Err(e) = portcullis_peer::register_session(reg, uid, owner_name, &app_id) {
                    return Response::Error { message: format!("register session uid: {e}") };
                }
                uid
            }
        }
    };
    if let Some(exec) = create.exec.as_mut() {
        exec.uid = owner_uid;
        exec.gid = owner_uid;
    }
    // A passwd entry, as an app's uid has: portcullisd refuses peers it can't
    // name, so without one the dock (a session component) could never ask for
    // the catalog or a launch.
    if let Err(e) = launch::ensure_passwd(owner_uid) {
        return Response::Error { message: format!("session uid: {e}") };
    }
    // The component's own state folder (forum.md §6.1): kept across sessions,
    // writable only by this component's session uid, at STATE_DEST in the jail.
    match prepare_state_dir(owner_name, &app_id, owner_uid) {
        Ok(src) => {
            create.mounts.push(jaild::protocol::MountSpec {
                source: src, dest: STATE_DEST.into(),
                kind: jaild::protocol::MountKind::RwNullfs, size_mb: None, owner: None,
            });
            if let Some(exec) = create.exec.as_mut() {
                exec.env.push(jaild::protocol::EnvPair {
                    key: "ATRIUM_STATE_DIR".into(), value: format!("/{STATE_DEST}"),
                });
            }
        }
        // Not fatal: the component runs, it just keeps nothing.
        Err(e) => eprintln!("portcullisd: state folder for {owner_name}/{app_id}: {e}"),
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
            // The state folder was mounted last, so it goes first.
            if let Err(e) = host_mount::unmount_jail_dest(&create.path, STATE_DEST) {
                if e.raw_os_error() != Some(libc::EINVAL) && e.kind() != std::io::ErrorKind::NotFound {
                    eprintln!("portcullisd: teardown unmount {} {STATE_DEST}: {e}", create.path);
                }
            }
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
            let system = Policy::load_system(std::path::Path::new(SYSTEM_PATH));
            let d = decide(&app_id, &requested, &manifest_hash, &tp.policy, system.as_ref());
            if let Some(why) = &d.system_unusable { eprintln!("portcullisd: system policy unusable: {why}") }
            let delta = d.delta;
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
        /* Both launch verbs are routed in serve(), which holds the fd-passing
         * socket this handler does not. Reaching here means that routing was
         * removed — refuse rather than fall through to something that cannot
         * receive the caller's descriptors. */
        Request::ExecInstance { .. } => Response::Error {
            message: "ExecInstance must be routed through the fd-passing path".into(),
        },
        Request::OpenCdm { .. } => Response::Error {
            message: "OpenCdm must be routed through the peer-uid, fd-passing path".into(),
        },
        Request::WorkerCall { .. } => Response::Error {
            message: "WorkerCall must be routed through the peer-uid, fd-passing path".into(),
        },
        Request::PortalWindow { .. } | Request::PortalStatus | Request::VpnSignInWindow { .. } | Request::VpnSignInStatus => Response::Error {
            message: "internal: the sign-in window's verbs go through the peer-uid path".into(),
        },
        Request::OpenPortalFetcher => Response::Error {
            message: "OpenPortalFetcher must be routed through the peer-uid, fd-passing path".into(),
        },
        Request::OpenFetcher => Response::Error {
            message: "OpenFetcher must be routed through the peer-uid, fd-passing path".into(),
        },
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
        | Request::TeardownSessionComponent { .. }
        | Request::SessionEnded { .. } => Response::Error {
            /* Caught by serve() before reaching here (routed to handle_session). */
            message: "internal: session verbs must go through handle_session".into(),
        },
        Request::Access | Request::RemoveAccess { .. } | Request::ApproveAccess { .. } | Request::Links | Request::SetLinkDefault { .. } | Request::Persona
        | Request::ShareTargets | Request::SetShareTargets { .. }

        | Request::PersonaList { .. } | Request::AddPersona { .. } | Request::SetPersonaColour { .. } | Request::RemovePersona { .. } | Request::Crossings | Request::SetCrossing { .. } | Request::NewWindowIn { .. }
        | Request::PersonaVpns | Request::SetPersonaVpn { .. } => Response::Error {
            message: "access verbs must be handled on the peer-uid path".into(),
        },
        Request::WorkspaceCredentials { .. } | Request::SetWorkspaceCredential { .. }
        | Request::Workspaces | Request::WorkspaceNew { .. } | Request::WorkspaceSet { .. } | Request::WorkspaceStop { .. } | Request::WorkspaceDelete { .. } | Request::WorkspaceRun { .. } | Request::WorkspaceDuplicate { .. }
        | Request::WorkspaceDebug { .. } | Request::WorkspaceCores { .. }
        | Request::WorkspacePick { .. } | Request::WorkspaceExec { .. } | Request::WorkspaceAdmin { .. } => Response::Error {
            message: "internal: workspaces must go through handle_workspace".into(),
        },
        Request::Console { .. } => Response::Error {
            message: "internal: Console must go through handle_console".into(),
        },
        Request::LinkGrant { .. } | Request::DeliverLink { .. } => Response::Error {
            message: "internal: link verbs must go through handle_link".into(),
        },
        Request::NewWorkbook { .. } | Request::RecoverWorkbook { .. } | Request::RecoverSelectedWorkbook { .. } | Request::NameWorkbookRecovery { .. } | Request::OpenApps { .. } | Request::OpenFile { .. } | Request::WorkbookVersion { .. } | Request::WorkbookBookmark { .. } | Request::CalendarInvitation { .. } | Request::CalendarUpdate | Request::OpenAttachment { .. } | Request::SaveDownload { .. } | Request::OpenDownload { .. } => Response::Error {
            /* Caught by serve() before reaching here (routed to handle_open). */
            message: "internal: open verbs must go through handle_open".into(),
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

    #[test]
    fn the_sign_in_persona_is_named_alike_on_both_sides() {
        assert_eq!(portcullis_ipc::PORTAL_PERSONA, portcullis_peer::PORTAL_PERSONA);
        assert_eq!(portcullis_ipc::VPN_PERSONA, portcullis_peer::VPN_PERSONA);
    }
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
                thread::spawn(move || { let _ = serve(stream, s, false); });
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
