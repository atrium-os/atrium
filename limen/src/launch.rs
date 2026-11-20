//! The real [`Launcher`](crate::daemon::Launcher): portcullisd's
//! `ExecInstance` (limen.md §4, §12 L3), the way frescod starts renderers.
//!
//! The child's channel is its stdin (a Unix socket, so it talks both ways);
//! fd 1 carries the island fetch socket when requested and otherwise opens
//! on /dev/null. Its stderr is limend's. portcullisd ends the
//! jail when limend hangs up the request connection, which is how
//! [`end`](crate::daemon::Launcher::end) works.
//!
//! **Which uid the child runs as.** `ExecInstance` runs a worker as its
//! caller unless the system policy (`/etc/atrium/policy.toml`) has a worker
//! rule for the app naming the caller. A provider must never run as
//! `_limen`, so a launch without a rule `[workers."<app>"] callers =
//! ["_limen"], run_as = "<its own user>"` is refused here, before
//! portcullisd is asked, unless the app's manifest implements a role
//! (`[roles]`, app-composition.md §5.1): portcullisd then runs it as a uid
//! of the parent's instance and answers which. That uid is what limend then expects to own the
//! child's window.

use std::collections::HashMap;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

use crate::daemon::Launcher;
use crate::proto::EmbedId;

pub struct Portcullis {
    /// limend's own user name, as worker rules name callers.
    caller: String,
    policy: PathBuf,
    jails: HashMap<EmbedId, UnixStream>,
}

impl Portcullis {
    pub fn new(caller: impl Into<String>) -> Self {
        Self { caller: caller.into(), policy: portcullis_policy::SYSTEM_PATH.into(), jails: HashMap::new() }
    }

    fn run_as(&self, app: &str) -> Result<u32, String> {
        let p = portcullis_policy::Policy::load_system(&self.policy)
            .map_err(|e| format!("system policy {}: {e}", self.policy.display()))?;
        let rule = p.worker_rule(app, &self.caller)
            .ok_or_else(|| format!("no worker rule lets {} start {app} as its own user", self.caller))?;
        uid_of(&rule.run_as).ok_or_else(|| format!("no user {}", rule.run_as))
    }
}

/// An embedded child's only way to frescod is the embed-only connection
/// limend hands it (fresco-video.md §8 S1). `ExecInstance` mounts what the
/// manifest asks for, so a provider whose manifest asks for `graphics` would
/// also get the ordinary app socket and could open top-level windows: it is
/// refused. (An app that is both a provider and a normal app ships its
/// provider as a separate bundle, until portcullisd can drop the mount for
/// workers limend starts.)
fn no_app_socket(app: &str) -> Result<(), String> {
    let path = std::path::Path::new("/var/lib/atrium/apps").join(app).join("atrium.toml");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let m = portcullis_toml::Manifest::from_str(&text).map_err(|e| format!("{}: {e:?}", path.display()))?;
    if m.capabilities.graphics.is_some() {
        return Err(format!("{app} asks for graphics, so its jail would hold frescod's app socket; an embed provider must not"));
    }
    Ok(())
}

/// Whether `app`'s installed manifest implements a role.
fn implements_a_role(app: &str) -> bool {
    crate::catalogue::ROLES.iter().any(|r| crate::manifests::Installed::default().implements(app, r.name))
}

fn uid_of(name: &str) -> Option<u32> {
    let c = std::ffi::CString::new(name).ok()?;
    // SAFETY: getpwnam with a valid C string; the result is read at once.
    let pw = unsafe { libc::getpwnam(c.as_ptr()) };
    (!pw.is_null()).then(|| unsafe { (*pw).pw_uid })
}

impl Launcher for Portcullis {
    fn launch(&mut self, app: &str, embed: EmbedId, channel: UnixStream) -> Result<u32, String> {
        self.start(app, embed, channel, None, None, None)
    }

    /// The child serves `parent_uid`: under a `per_instance` worker rule it
    /// runs as a uid of the parent's instance (its person and persona), and
    /// portcullisd says which.
    fn launch_for(&mut self, app: &str, embed: EmbedId, channel: UnixStream, parent_uid: u32) -> Result<u32, String> {
        self.start(app, embed, channel, Some(parent_uid), None, None)
    }

    /// The child in another of the parent's person's personas: portcullisd
    /// starts it only while that person's `share` crossing switch from the
    /// parent's persona to `persona` is on (share-sheet.md §7).
    fn launch_in(&mut self, app: &str, embed: EmbedId, channel: UnixStream, parent_uid: u32, persona: &str) -> Result<u32, String> {
        self.start(app, embed, channel, Some(parent_uid), Some(persona), None)
    }

    fn launch_island(&mut self, embed: EmbedId, channel: UnixStream, parent_uid: u32, fetch: UnixStream) -> Result<u32, String> {
        self.start(crate::catalogue::NAVIGATOR_ENGINE, embed, channel, Some(parent_uid), None, Some(fetch))
    }

    fn end(&mut self, embed: EmbedId) {
        if let Some(s) = self.jails.remove(&embed) {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }
}

impl Portcullis {
    fn start(&mut self, app: &str, embed: EmbedId, channel: UnixStream, serves: Option<u32>, persona: Option<&str>, fetch: Option<UnixStream>) -> Result<u32, String> {
        use portcullis_ipc::{read_response, round_trip, send_fds, write_request, Request, Response};
        // A provider from its manifest's [roles] implements has no system
        // rule: portcullisd runs it as a uid of the parent's instance (as a
        // per-instance rule would) and says which.
        let mut uid = match self.run_as(app) {
            Ok(u) => Some(u),
            Err(_) if serves.is_some() && implements_a_role(app) => None,
            Err(e) => return Err(e),
        };
        no_app_socket(app)?;
        let go = || -> io::Result<UnixStream> {
            let path = portcullis_ipc::resolve_socket_path()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "portcullisd's socket is not there"))?;
            let mut s = UnixStream::connect(path)?;
            s.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
            match round_trip(&mut s, &Request::Hello { version: portcullis_ipc::PROTO_VERSION })? {
                Response::Hello { .. } => {}
                other => return Err(io::Error::other(format!("hello: {other:?}"))),
            }
            write_request(&mut s, &Request::ExecInstance {
                app_id: app.to_string(),
                instance: Some(format!("limen-{embed:016x}")),
                tmpfs_mb: 64,
                // The provider's [resources] set the caps; a machine without
                // racct refuses rather than runs it uncapped.
                memory_mb: None,
                cpu_pct: None,
                require_memory_limit: true,
                serves,
                persona: persona.map(str::to_string),
            })?;
            match read_response(&mut s)? {
                Response::ReadyForFds => {}
                Response::ReadyForFdsAs { uid: u } => uid = Some(u),
                Response::LaunchFailed { stage, message } => return Err(io::Error::other(format!("{stage}: {message}"))),
                Response::Error { message } => return Err(io::Error::other(message)),
                other => return Err(io::Error::other(format!("unexpected reply: {other:?}"))),
            }
            let stdout: std::os::fd::OwnedFd = match fetch {
                Some(socket) => socket.into(),
                None => std::fs::OpenOptions::new().write(true).open("/dev/null")?.into(),
            };
            send_fds(&s, &[channel.as_raw_fd(), stdout.as_raw_fd(), 2])?;
            s.set_read_timeout(None)?;
            Ok(s)
        };
        let s = go().map_err(|e| e.to_string())?;
        drop(channel);
        let uid = uid.ok_or_else(|| format!("portcullisd didn't say whom {app} runs as"))?;
        self.jails.insert(embed, s);
        Ok(uid)
    }
}
