//! Ostiarius — the doorkeeper. The privileged session-manager (the display-manager
//! role): a trusted system service, NOT an Insula app, brought up by the boot
//! service manager outside the manifest gate.
//!
//! It answers "who launches the session, if vestibulum and Forum can't?" (they are
//! Insula apps — no jail_set/setuid/exec). Ostiarius doesn't exec either: it
//! **requests** launches from Portcullis → **jaild** (the sole launcher), via the
//! same verify → allocate-uid → register → CreateJail path as `atrium-launch`.
//! What it owns is the orchestration:
//!
//!   boot()        → request jaild to launch vestibulum (the login UI), full-screen
//!   authenticate  → (pam; stubbed here) credential → human uid
//!   login(human)  → bind the seat (`portcullis_peer::seat`) + launch the session
//!                   layer (forum-wm + forum-bar + forum-dock + choragus for GUI;
//!                   a zsh console-shell for CLI)
//!   logout()      → tear the session down + unbind the seat → back to vestibulum
//!
//! The launch is behind a [`Launcher`] seam: the real [`JaildLauncher`] drives the
//! TCB; tests use a recording mock. Invariants: only Forum carries
//! `window-management`; every session app runs as the human; ostiarius only ever
//! *requests* (no exec path here).

use std::collections::HashMap;
use std::path::PathBuf;

pub use portcullis_peer::seat;

#[cfg(feature = "pam")]
pub mod pam;

/// Which vestibulum frontend authenticated — GUI (on Fresco) or CLI (a tty/serial
/// console, the display-down fallback). Same trusted flow either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Frontend {
    Gui,
    Cli,
}

/// A launch ostiarius asks the TCB to perform. jaild verifies, allocates a
/// dedicated uid under `owner` (the human), registers it, jails + execs `bin`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub app_id: String,
    pub owner: String,
    pub manifest: PathBuf,
    pub sig: PathBuf,
    pub jail_path: String,
    pub bin: String,
    pub argv: Vec<String>,
    /// Manifest caps (for context/audit; portcullisd verifies/grants them).
    pub caps: Vec<&'static str>,
}

/// The launcher seam — the only thing that reaches the TCB. Production:
/// [`JaildLauncher`] (verify → allocate → register → jaild). Tests: a recorder.
pub trait Launcher {
    /// Request a jailed launch; returns the pid jaild reports.
    fn launch(&mut self, spec: &LaunchSpec) -> Result<i32, String>;
    /// Tear a previously-launched app down (best-effort).
    fn teardown(&mut self, app_id: &str) -> Result<(), String>;
}

const APPS_DIR: &str = "/usr/local/share/atrium/apps";

fn spec(app_id: &str, owner: &str, bin: &str, argv: &[&str], caps: Vec<&'static str>) -> LaunchSpec {
    let dir = format!("{APPS_DIR}/{app_id}");
    LaunchSpec {
        app_id: app_id.to_string(),
        owner: owner.to_string(),
        manifest: format!("{dir}/atrium.toml").into(),
        sig: format!("{dir}/atrium.toml.sig").into(),
        // V1 stand-in: jail at "/" (the app sees the host rootfs — libs + the
        // capability-mounted service sockets). Per-jail rootfs trees (a real
        // `{dir}/jail`) land in D5; jaild's policy only allows the host root for
        // now (see portcullis.md "deferred V1"). Using a per-app dir here gets a
        // `path.not_in_allowlist` refusal.
        jail_path: "/".to_string(),
        // A leading-"/" bin is an absolute system path (the CLI console shell);
        // otherwise it's the component's bundle binary, apps/<id>/bin/<name> —
        // which is what jaild's exec allow-list permits (the apps/ prefix).
        bin: if bin.starts_with('/') { bin.to_string() } else { format!("{dir}/bin/{bin}") },
        argv: argv.iter().map(|s| s.to_string()).collect(),
        caps,
    }
}

/// The session layer to launch for a frontend, owned by `human`.
fn session_layer(human: &str, frontend: Frontend) -> Vec<LaunchSpec> {
    match frontend {
        // The Forum desktop, each component its own jailed app (forum.md §3): the
        // WM core (sole window-management) + the chrome (graphics + forum-control,
        // never window-management) + audio. App-ids/caps match each component's
        // atrium.toml. The overview is launched on-demand, not at session start.
        Frontend::Gui => vec![
            spec("org.atrium.forum-wm", human, "forum-wm", &[], vec!["graphics", "window-management", "notify"]),
            spec("org.atrium.forum-bar", human, "forum-bar", &[], vec!["graphics", "forum-control"]),
            spec("org.atrium.forum-dock", human, "forum-dock", &[], vec!["graphics", "forum-control"]),
            spec("org.atrium.choragus", human, "choragusd", &[], vec!["audio"]),
        ],
        // Display-down fallback: a login zsh in the human's session jail (the
        // decided shell; the jail is the boundary, not a custom shell). No Forum.
        Frontend::Cli => vec![
            spec("org.atrium.console-shell", human, "/usr/local/bin/zsh", &["-l"], vec!["console"]),
        ],
    }
}

/// The doorkeeper.
pub struct Ostiarius<L: Launcher> {
    launcher: L,
    /// Path to the seat's active-session file (default `seat::ACTIVE_SESSION`).
    seat_path: String,
    /// PAM service to authenticate against (`/etc/pam.d/<service>`). `None` → the
    /// dev/test stub (any non-empty credential).
    pam_service: Option<String>,
    /// All live sessions: human → the app-ids launched for them. Fast-user-
    /// switching keeps every session here ALIVE; the seat selects which one is
    /// active (only the active session's audio/windows are live — the seat-aware
    /// Choragus/Forum gate on it).
    sessions: HashMap<String, Vec<String>>,
}

impl<L: Launcher> Ostiarius<L> {
    pub fn new(launcher: L) -> Self {
        Ostiarius {
            launcher,
            seat_path: seat::ACTIVE_SESSION.to_string(),
            pam_service: None,
            sessions: HashMap::new(),
        }
    }

    /// Use a non-default seat file (tests).
    pub fn with_seat_path(mut self, path: impl Into<String>) -> Self {
        self.seat_path = path.into();
        self
    }

    /// Authenticate against this PAM service instead of the stub (production).
    pub fn with_pam(mut self, service: impl Into<String>) -> Self {
        self.pam_service = Some(service.into());
        self
    }

    /// Boot: request jaild to launch the login UI (vestibulum). No session yet.
    pub fn boot(&mut self) -> Result<i32, String> {
        let v = spec("org.atrium.vestibulum", "_login", "vestibulum", &[], vec!["graphics"]);
        self.launcher.launch(&v)
    }

    /// Authenticate a credential → the human user. STUBBED (the pam FFI seam):
    /// today any non-empty user+password succeeds, matching vestibulum's D2 stub.
    /// Production: `pam_authenticate` via libpam (a C-ABI lib) in this privileged
    /// backend — the one place that reads `master.passwd`, privsep'd from the UI.
    ///
    /// SETTLED — we use **only PAM's `auth` + `account` facilities** (verify *who*
    /// the human is and *whether* they may log in — both model-agnostic). We do
    /// **not** use the PAM `session` facility: it is built for the traditional
    /// "one login = one process tree running as the human's uid" model and
    /// decorates *that* process (env, limits, home, lastlog, the seat). Atrium has
    /// no such process — a session is the seat-bound set of the human's apps, each
    /// launched by jaild in its own jail under a *dedicated* uid (the human is the
    /// owner, not the uid; authorization is by capability). So session
    /// establishment is Atrium's own: the **seat** primitive + the **jaild**-
    /// launched, capability-gated components; per-component env/limits/jail/uid come
    /// from the **manifest + Portcullis**, not `pam_open_session`; login audit is
    /// native (insula-logd / Tessera). PAM says *who walked in*; jaild + the seat +
    /// capabilities decide what running as them means.
    pub fn authenticate(&self, user: &str, password: &str) -> Result<String, String> {
        if user.is_empty() || password.is_empty() {
            return Err("authentication failed".into());
        }
        if let Some(service) = &self.pam_service {
            #[cfg(feature = "pam")]
            {
                pam::authenticate(service, user, password)?;
                return Ok(user.to_string());
            }
            #[cfg(not(feature = "pam"))]
            {
                let _ = service;
                return Err("pam requested but not compiled (build --features pam)".into());
            }
        }
        // dev/test stub: any non-empty credential succeeds (matches vestibulum D2).
        Ok(user.to_string())
    }

    /// Post-auth: make `human` the active session. Binds the seat to them; if they
    /// have no live session yet, launch their layer (via the TCB, as the human;
    /// only Forum gets `window-management`). If they already have one — a
    /// fast-user-switch — just rebind: their session is reactivated, and whoever was
    /// active stays ALIVE but detached (the seat now points elsewhere). So a second
    /// `login` does not tear the first session down — that is FUS.
    pub fn login(&mut self, human: &str, frontend: Frontend) -> Result<(), String> {
        seat::set_active_at(&self.seat_path, human).map_err(|e| format!("bind seat: {e}"))?;
        if !self.sessions.contains_key(human) {
            let mut ids = Vec::new();
            // Launch each session component; a single one failing (e.g. audio with
            // no device) must not abort the whole desktop — log it and carry on,
            // recording only what actually launched (so teardown matches).
            for s in session_layer(human, frontend) {
                match self.launcher.launch(&s) {
                    Ok(_) => ids.push(s.app_id),
                    Err(e) => eprintln!("ostiarius: session component {} failed: {e}", s.app_id),
                }
            }
            self.sessions.insert(human.to_string(), ids);
        }
        Ok(())
    }

    /// Logout the *active* session: tear it down + remove it; the seat unbinds
    /// (back to the login screen). Other humans' sessions stay alive (detached).
    pub fn logout(&mut self) -> Result<(), String> {
        if let Some(active) = self.active_human() {
            if let Some(ids) = self.sessions.remove(&active) {
                for id in ids {
                    let _ = self.launcher.teardown(&id);
                }
            }
        }
        seat::set_active_at(&self.seat_path, "").map_err(|e| format!("unbind seat: {e}"))?;
        Ok(())
    }

    /// The humans with a live session right now (active + any detached).
    pub fn live_sessions(&self) -> Vec<&str> {
        self.sessions.keys().map(|s| s.as_str()).collect()
    }

    /// The active human session, if any (the seat's bound user).
    pub fn active_human(&self) -> Option<String> {
        seat::active_at(&self.seat_path)
    }
}

/// The control wire: vestibulum hands an authenticated login to ostiarius, and
/// asks for logout. Newline-delimited JSON over a Unix socket (the workspace IPC
/// shape), peer-gated by `getpeereid` — only the trusted login UI may drive it.
pub mod control {
    use super::{Frontend, Launcher, Ostiarius};
    use serde::{Deserialize, Serialize};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::{UnixListener, UnixStream};

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "op", rename_all = "snake_case")]
    pub enum Request {
        /// vestibulum forwards a credential + which frontend it is.
        Login { user: String, password: String, frontend: Frontend },
        /// End the active session.
        Logout,
    }

    #[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
    #[serde(tag = "status", rename_all = "snake_case")]
    pub enum Response {
        Ok { active: Option<String> },
        Err { message: String },
    }

    /// Handle one request against the orchestrator. Pure of the socket — the unit
    /// of behavior the daemon loop wraps. Login authenticates then establishes the
    /// session; logout tears it down.
    pub fn handle<L: Launcher>(ost: &mut Ostiarius<L>, req: Request) -> Response {
        let r = match req {
            Request::Login { user, password, frontend } => ost
                .authenticate(&user, &password)
                .and_then(|human| ost.login(&human, frontend)),
            Request::Logout => ost.logout(),
        };
        match r {
            Ok(()) => Response::Ok { active: ost.active_human() },
            Err(message) => Response::Err { message },
        }
    }

    /// The connecting peer's app-id, via `getpeereid` → the launch registry. The
    /// daemon admits only the trusted login UI (`org.atrium.vestibulum`).
    fn peer_is_vestibulum(stream: &UnixStream) -> bool {
        use std::os::fd::AsRawFd;
        let reg = portcullis_peer::AppRegistry::load(portcullis_peer::DEFAULT_REGISTRY)
            .unwrap_or_default();
        match portcullis_peer::resolve(stream.as_raw_fd(), &reg) {
            Ok(p) => p.app_id.as_deref() == Some("org.atrium.vestibulum"),
            Err(_) => false,
        }
    }

    /// The daemon service loop: listen on `sock_path`, and for each connection from
    /// the trusted login UI, handle newline-delimited JSON requests. `gate` lets a
    /// test bypass the `getpeereid` peer check; production passes
    /// `peer_is_vestibulum`.
    pub fn serve<L: Launcher>(
        ost: &mut Ostiarius<L>,
        sock_path: &str,
        gate: impl Fn(&UnixStream) -> bool,
    ) -> std::io::Result<()> {
        let _ = std::fs::remove_file(sock_path);
        let listener = UnixListener::bind(sock_path)?;
        // 0666 so the (jailed, per-app uid) vestibulum can connect to this
        // root-owned socket; `gate` (the org.atrium.vestibulum peer check) is the
        // real authorization, the mode just permits the connection. Same pattern
        // as the fresco / forum-ctl / portcullisd sockets.
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(sock_path, std::fs::Permissions::from_mode(0o666));
        }
        for conn in listener.incoming() {
            let stream = match conn {
                Ok(s) => s,
                Err(_) => continue,
            };
            if !gate(&stream) {
                eprintln!("ostiarius: refused a non-vestibulum peer");
                continue;
            }
            let mut writer = stream.try_clone()?;
            let reader = BufReader::new(stream);
            for line in reader.lines() {
                let line = match line {
                    Ok(l) if !l.trim().is_empty() => l,
                    _ => continue,
                };
                let resp = match serde_json::from_str::<Request>(&line) {
                    Ok(req) => handle(ost, req),
                    Err(e) => Response::Err { message: format!("bad request: {e}") },
                };
                let mut body = serde_json::to_vec(&resp).unwrap_or_default();
                body.push(b'\n');
                if writer.write_all(&body).is_err() {
                    break;
                }
            }
        }
        Ok(())
    }

    /// The production peer gate (re-exported for the daemon binary).
    pub fn vestibulum_gate(stream: &UnixStream) -> bool {
        peer_is_vestibulum(stream)
    }
}

/// The production launcher: drives the TCB exactly as `atrium-launch` does —
/// verify the manifest (trusted publisher) → allocate a dedicated uid → register
/// the binding → ask jaild to jail + drop-to-uid + exec. Only connects to jaild at
/// runtime; constructing it is free.
pub struct JaildLauncher {
    pub jaild_sock: String,
    pub publishers: String,
    /// Held procdesc fds, per app-id. jaild `pdfork`s without `PD_DAEMON` and
    /// passes the procdesc back over SCM_RIGHTS; the kernel keeps the jailed
    /// process alive only while someone holds that fd. As the session
    /// supervisor, ostiarius holds them here for the session's lifetime —
    /// dropping one (on `teardown`/logout) lets the kernel reap that app. This
    /// is THE reason supervision lives in ostiarius, not the one-shot launcher.
    held: std::collections::HashMap<String, std::os::fd::OwnedFd>,
}

impl Default for JaildLauncher {
    fn default() -> Self {
        JaildLauncher {
            jaild_sock: "/var/run/atrium/jaild.sock".into(),
            publishers: "/etc/atrium/publishers".into(),
            held: std::collections::HashMap::new(),
        }
    }
}

impl Launcher for JaildLauncher {
    fn launch(&mut self, s: &LaunchSpec) -> Result<i32, String> {
        use jaild::protocol::{CreateJailRequest, EnvPair, ExecSpec, NetworkConfig, Request, Response};
        use portcullisd::jaild_client::Client;

        // 1. verify the manifest is signed by a trusted publisher.
        let manifest = std::fs::read(&s.manifest).map_err(|e| format!("read {:?}: {e}", s.manifest))?;
        let sig = read_sig(&s.sig);
        let keys = load_publishers(&self.publishers);
        if keys.is_empty() {
            return Err(format!("no trusted publishers in {}", self.publishers));
        }
        portcullis_sig::verify_trusted(&manifest, &sig, &keys)
            .map_err(|e| format!("{} not signed by a trusted publisher: {e:?}", s.app_id))?;

        // 2. allocate a dedicated uid; 3. register uid → (owner, app-id).
        let uid = portcullis_peer::allocate(portcullis_peer::DEFAULT_REGISTRY);
        portcullis_peer::register(portcullis_peer::DEFAULT_REGISTRY, uid, &s.owner, &s.app_id)
            .map_err(|e| format!("register uid {uid}: {e}"))?;

        // 4. ask jaild to jail + drop-to-uid + exec.
        let req = Request::CreateJail(CreateJailRequest {
            name: jail_name(&s.app_id),
            path: s.jail_path.clone(),
            children_max: 0,
            mounts: vec![],
            devfs_ruleset: 0,
            network: NetworkConfig::Disable,
            exec: Some(ExecSpec {
                path: s.bin.clone(),
                // argv[0] is the program itself (jaild requires a non-empty argv),
                // followed by any declared args.
                argv: std::iter::once(s.bin.clone()).chain(s.argv.iter().cloned()).collect(),
                env: vec![EnvPair { key: "PATH".into(), value: "/bin:/usr/bin:/usr/local/bin".into() }],
                uid,
                gid: uid,
            }),
        });
        let mut c = Client::connect(&self.jaild_sock).map_err(|e| format!("connect jaild: {e}"))?;
        match c.send(&req) {
            Ok((Response::JailCreated(r), pdfd)) => {
                // Retain the procdesc fd for the session's lifetime — without
                // this the kernel SIGKILLs the jailed app the moment the last fd
                // closes. (A later refinement: kqueue EVFILT_PROCDESC on these
                // for exit-notification + the manifest's restart policy.)
                if let Some(fd) = pdfd {
                    use std::os::fd::FromRawFd;
                    // SAFETY: `fd` is the procdesc jaild passed via SCM_RIGHTS.
                    let owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
                    self.held.insert(s.app_id.clone(), owned);
                }
                Ok(r.pid)
            }
            Ok((resp, _)) => Err(format!("jaild refused: {resp:?}")),
            Err(e) => Err(format!("jaild send: {e}")),
        }
    }

    fn teardown(&mut self, app_id: &str) -> Result<(), String> {
        use jaild::protocol::{Request, Response};
        use portcullisd::jaild_client::Client;
        // Drop the held procdesc (closing it lets the kernel reap the jailed
        // process), then RemoveJail to clear the jail entry.
        self.held.remove(app_id);
        // RemoveJail by name (the exec'd-jail case): idempotent — a jail already
        // gone returns success. The component's session-jail is named on launch.
        let req = Request::RemoveJail { jid: None, name: Some(jail_name(app_id)) };
        let mut c = Client::connect(&self.jaild_sock).map_err(|e| format!("connect jaild: {e}"))?;
        match c.send(&req) {
            Ok((Response::SyscallFailed { msg, .. }, _)) => Err(format!("jaild teardown: {msg}")),
            Ok(_) => Ok(()),
            Err(e) => Err(format!("jaild teardown send: {e}")),
        }
    }
}

/// The jail name for a component — shared by launch + teardown so RemoveJail
/// targets exactly what CreateJail made.
fn jail_name(app_id: &str) -> String {
    format!("app-{}", app_id.replace(['.', '/'], "-"))
}

fn read_sig(p: &std::path::Path) -> Vec<u8> {
    let raw = std::fs::read(p).unwrap_or_default();
    if let Ok(s) = std::str::from_utf8(&raw) {
        if let Ok(der) = portcullis_sig::sig_from_base64(s) {
            return der;
        }
    }
    raw
}

fn load_publishers(dir: &str) -> Vec<String> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if e.path().extension().and_then(|x| x.to_str()) == Some("pem") {
                if let Ok(p) = std::fs::read_to_string(e.path()) {
                    v.push(p);
                }
            }
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recording launcher — proves the orchestration without a live jaild.
    #[derive(Default)]
    struct MockLauncher {
        launched: Vec<LaunchSpec>,
        torn_down: Vec<String>,
    }
    impl Launcher for MockLauncher {
        fn launch(&mut self, s: &LaunchSpec) -> Result<i32, String> {
            self.launched.push(s.clone());
            Ok(50_000 + self.launched.len() as i32)
        }
        fn teardown(&mut self, app: &str) -> Result<(), String> {
            self.torn_down.push(app.to_string());
            Ok(())
        }
    }

    fn seat_file(tag: &str) -> String {
        let p = format!("/tmp/ostiarius-test-{}-{}", std::process::id(), tag);
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn boot_requests_only_the_login_ui() {
        let mut o = Ostiarius::new(MockLauncher::default()).with_seat_path(seat_file("boot"));
        o.boot().unwrap();
        assert!(o.active_human().is_none(), "no session at boot");
        assert_eq!(o.launcher.launched.len(), 1);
        assert_eq!(o.launcher.launched[0].app_id, "org.atrium.vestibulum");
        assert!(!o.launcher.launched[0].caps.contains(&"window-management"));
    }

    #[test]
    fn login_binds_the_seat_and_launches_the_gui_layer_as_the_human() {
        let mut o = Ostiarius::new(MockLauncher::default()).with_seat_path(seat_file("gui"));
        o.login("alice", Frontend::Gui).unwrap();
        assert_eq!(o.active_human().as_deref(), Some("alice"), "seat bound to the human");
        let apps: Vec<_> = o.launcher.launched.iter().map(|s| s.app_id.as_str()).collect();
        assert!(apps.contains(&"org.atrium.forum-wm") && apps.contains(&"org.atrium.choragus"));
        assert!(o.launcher.launched.iter().all(|s| s.owner == "alice"), "all run as the human");
    }

    #[test]
    fn only_forum_carries_window_management() {
        let mut o = Ostiarius::new(MockLauncher::default()).with_seat_path(seat_file("wm"));
        o.login("alice", Frontend::Gui).unwrap();
        let wm: Vec<_> = o.launcher.launched.iter().filter(|s| s.caps.contains(&"window-management")).collect();
        assert_eq!(wm.len(), 1);
        assert_eq!(wm[0].app_id, "org.atrium.forum-wm");
    }

    #[test]
    fn cli_login_launches_a_zsh_console_shell_in_the_jail_no_forum() {
        let mut o = Ostiarius::new(MockLauncher::default()).with_seat_path(seat_file("cli"));
        o.login("alice", Frontend::Cli).unwrap();
        assert_eq!(o.active_human().as_deref(), Some("alice"));
        assert_eq!(o.launcher.launched.len(), 1);
        let sh = &o.launcher.launched[0];
        assert_eq!(sh.app_id, "org.atrium.console-shell");
        assert_eq!(sh.bin, "/usr/local/bin/zsh", "the decided shell");
        assert_eq!(sh.owner, "alice", "in the human's session jail, not root");
        assert!(!o.launcher.launched.iter().any(|s| s.app_id == "org.atrium.forum-wm"), "no graphics on the CLI path");
    }

    #[test]
    fn auth_rejects_empty_credentials() {
        let o = Ostiarius::new(MockLauncher::default());
        assert!(o.authenticate("", "x").is_err());
        assert!(o.authenticate("alice", "").is_err());
        assert_eq!(o.authenticate("alice", "pw").unwrap(), "alice");
    }

    #[test]
    fn logout_tears_down_and_unbinds() {
        let mut o = Ostiarius::new(MockLauncher::default()).with_seat_path(seat_file("logout"));
        o.login("alice", Frontend::Gui).unwrap();
        o.logout().unwrap();
        assert!(o.active_human().is_none(), "seat unbound → back to login");
        assert!(o.launcher.torn_down.contains(&"org.atrium.forum-wm".to_string()), "Forum torn down");
    }

    #[test]
    fn fus_keeps_both_sessions_alive_and_the_seat_selects_the_active_one() {
        let mut o = Ostiarius::new(MockLauncher::default()).with_seat_path(seat_file("fus"));
        o.login("alice", Frontend::Gui).unwrap();
        let after_alice = o.launcher.launched.len();
        o.login("bob", Frontend::Gui).unwrap(); // fast-user-switch to bob
        assert_eq!(o.active_human().as_deref(), Some("bob"), "seat now points at bob");
        let mut live = o.live_sessions();
        live.sort();
        assert_eq!(live, vec!["alice", "bob"], "BOTH sessions alive — alice not torn down");
        assert!(o.launcher.torn_down.is_empty(), "FUS tears nothing down");
        assert!(o.launcher.launched.len() > after_alice, "bob's layer was launched");
    }

    #[test]
    fn switching_back_to_a_live_session_relaunches_nothing() {
        let mut o = Ostiarius::new(MockLauncher::default()).with_seat_path(seat_file("fus2"));
        o.login("alice", Frontend::Gui).unwrap();
        o.login("bob", Frontend::Gui).unwrap();
        let n = o.launcher.launched.len();
        o.login("alice", Frontend::Gui).unwrap(); // switch back to alice's live session
        assert_eq!(o.active_human().as_deref(), Some("alice"));
        assert_eq!(o.launcher.launched.len(), n, "alice's session already live → no relaunch");
    }

    #[test]
    fn logout_drops_only_the_active_session() {
        let mut o = Ostiarius::new(MockLauncher::default()).with_seat_path(seat_file("fus3"));
        o.login("alice", Frontend::Gui).unwrap();
        o.login("bob", Frontend::Gui).unwrap(); // bob active
        o.logout().unwrap(); // logs bob out
        assert!(o.active_human().is_none(), "seat unbound → login screen");
        assert_eq!(o.live_sessions(), vec!["alice"], "alice's detached session survives");
    }

    #[test]
    fn control_handle_login_then_logout() {
        use crate::control::{handle, Request, Response};
        let mut o = Ostiarius::new(MockLauncher::default()).with_seat_path(seat_file("ctl"));
        let login = Request::Login { user: "alice".into(), password: "pw".into(), frontend: Frontend::Gui };
        assert_eq!(handle(&mut o, login), Response::Ok { active: Some("alice".into()) });
        assert!(o.launcher.launched.iter().any(|s| s.app_id == "org.atrium.forum-wm"));
        assert_eq!(handle(&mut o, Request::Logout), Response::Ok { active: None });
    }

    #[test]
    fn control_handle_rejects_bad_credentials() {
        use crate::control::{handle, Request, Response};
        let mut o = Ostiarius::new(MockLauncher::default()).with_seat_path(seat_file("ctlbad"));
        let bad = Request::Login { user: "alice".into(), password: "".into(), frontend: Frontend::Gui };
        assert!(matches!(handle(&mut o, bad), Response::Err { .. }));
        assert!(o.active_human().is_none(), "no session on failed auth");
    }

    #[test]
    fn control_request_round_trips_as_json() {
        use crate::control::Request;
        let r = Request::Login { user: "alice".into(), password: "pw".into(), frontend: Frontend::Cli };
        let s = serde_json::to_string(&r).unwrap();
        assert!(s.contains("\"op\":\"login\"") && s.contains("\"frontend\":\"cli\""));
        let back: Request = serde_json::from_str(&s).unwrap();
        assert!(matches!(back, Request::Login { frontend: Frontend::Cli, .. }));
    }
}
