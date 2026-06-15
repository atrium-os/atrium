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
//!                   layer (Forum+Choragus+dock for GUI; a zsh console-shell for CLI)
//!   logout()      → tear the session down + unbind the seat → back to vestibulum
//!
//! The launch is behind a [`Launcher`] seam: the real [`JaildLauncher`] drives the
//! TCB; tests use a recording mock. Invariants: only Forum carries
//! `window-management`; every session app runs as the human; ostiarius only ever
//! *requests* (no exec path here).

use std::path::PathBuf;

pub use portcullis_peer::seat;

/// Which vestibulum frontend authenticated — GUI (on Fresco) or CLI (a tty/serial
/// console, the display-down fallback). Same trusted flow either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        jail_path: format!("{dir}/jail"),
        bin: bin.to_string(),
        argv: argv.iter().map(|s| s.to_string()).collect(),
        caps,
    }
}

/// The session layer to launch for a frontend, owned by `human`.
fn session_layer(human: &str, frontend: Frontend) -> Vec<LaunchSpec> {
    match frontend {
        Frontend::Gui => vec![
            spec("org.atrium.forum", human, "/usr/local/bin/forum", &[], vec!["graphics", "window-management"]),
            spec("org.atrium.choragus", human, "/usr/local/bin/choragusd", &[], vec!["audio"]),
            spec("org.atrium.dock", human, "/usr/local/bin/dock", &[], vec!["graphics"]),
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
    running: Vec<String>,
}

impl<L: Launcher> Ostiarius<L> {
    pub fn new(launcher: L) -> Self {
        Ostiarius { launcher, seat_path: seat::ACTIVE_SESSION.to_string(), running: Vec::new() }
    }

    /// Use a non-default seat file (tests).
    pub fn with_seat_path(mut self, path: impl Into<String>) -> Self {
        self.seat_path = path.into();
        self
    }

    /// Boot: request jaild to launch the login UI (vestibulum). No session yet.
    pub fn boot(&mut self) -> Result<i32, String> {
        let v = spec("org.atrium.vestibulum", "_login", "/usr/local/bin/vestibulum", &[], vec!["graphics"]);
        self.launcher.launch(&v)
    }

    /// Authenticate a credential → the human user. STUBBED (the pam FFI seam):
    /// today any non-empty user+password succeeds, matching vestibulum's D2 stub.
    /// Production: `pam_authenticate` via libpam (a C-ABI lib) in the privileged
    /// backend — the one place that reads shadow, privsep'd from the UI.
    pub fn authenticate(&self, user: &str, password: &str) -> Result<String, String> {
        if user.is_empty() || password.is_empty() {
            return Err("authentication failed".into());
        }
        Ok(user.to_string())
    }

    /// Post-auth: bind the seat to the human and launch the session layer. Every
    /// component is launched via the TCB as the human; only Forum gets
    /// `window-management`.
    pub fn login(&mut self, human: &str, frontend: Frontend) -> Result<(), String> {
        seat::set_active_at(&self.seat_path, human).map_err(|e| format!("bind seat: {e}"))?;
        for s in session_layer(human, frontend) {
            self.launcher.launch(&s)?;
            self.running.push(s.app_id);
        }
        Ok(())
    }

    /// Logout: tear down the session and unbind the seat → back to vestibulum.
    pub fn logout(&mut self) -> Result<(), String> {
        for app in std::mem::take(&mut self.running) {
            let _ = self.launcher.teardown(&app);
        }
        seat::set_active_at(&self.seat_path, "").map_err(|e| format!("unbind seat: {e}"))?;
        Ok(())
    }

    /// The active human session, if any (the seat's bound user).
    pub fn active_human(&self) -> Option<String> {
        seat::active_at(&self.seat_path)
    }
}

/// The production launcher: drives the TCB exactly as `atrium-launch` does —
/// verify the manifest (trusted publisher) → allocate a dedicated uid → register
/// the binding → ask jaild to jail + drop-to-uid + exec. Only connects to jaild at
/// runtime; constructing it is free.
pub struct JaildLauncher {
    pub jaild_sock: String,
    pub publishers: String,
}

impl Default for JaildLauncher {
    fn default() -> Self {
        JaildLauncher {
            jaild_sock: "/var/run/atrium/jaild.sock".into(),
            publishers: "/etc/atrium/publishers".into(),
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
            name: format!("app-{}", s.app_id.replace(['.', '/'], "-")),
            path: s.jail_path.clone(),
            children_max: 0,
            mounts: vec![],
            devfs_ruleset: 0,
            network: NetworkConfig::Disable,
            exec: Some(ExecSpec {
                path: s.bin.clone(),
                argv: s.argv.clone(),
                env: vec![EnvPair { key: "PATH".into(), value: "/bin:/usr/bin:/usr/local/bin".into() }],
                uid,
                gid: uid,
            }),
        });
        let mut c = Client::connect(&self.jaild_sock).map_err(|e| format!("connect jaild: {e}"))?;
        match c.send(&req) {
            Ok((Response::JailCreated(r), _)) => Ok(r.pid),
            Ok((resp, _)) => Err(format!("jaild refused: {resp:?}")),
            Err(e) => Err(format!("jaild send: {e}")),
        }
    }

    fn teardown(&mut self, _app_id: &str) -> Result<(), String> {
        // Real teardown (DestroyJail) lands with the daemon loop; the session jails
        // also fall when their owning fds close. Best-effort no-op for now.
        Ok(())
    }
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
        assert!(apps.contains(&"org.atrium.forum") && apps.contains(&"org.atrium.choragus"));
        assert!(o.launcher.launched.iter().all(|s| s.owner == "alice"), "all run as the human");
    }

    #[test]
    fn only_forum_carries_window_management() {
        let mut o = Ostiarius::new(MockLauncher::default()).with_seat_path(seat_file("wm"));
        o.login("alice", Frontend::Gui).unwrap();
        let wm: Vec<_> = o.launcher.launched.iter().filter(|s| s.caps.contains(&"window-management")).collect();
        assert_eq!(wm.len(), 1);
        assert_eq!(wm[0].app_id, "org.atrium.forum");
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
        assert!(!o.launcher.launched.iter().any(|s| s.app_id == "org.atrium.forum"), "no graphics on the CLI path");
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
        assert!(o.launcher.torn_down.contains(&"org.atrium.forum".to_string()), "Forum torn down");
    }
}
