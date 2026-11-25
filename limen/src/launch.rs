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
    /// Keep a warmed engine per Navigator (navigator-islands.md D7). Off
    /// unless `LIMEN_ISLAND_STANDBY=1`, until the installed engine knows the
    /// standby phase: an older engine given no fetch socket exits at once.
    standby: bool,
}

impl Portcullis {
    pub fn new(caller: impl Into<String>) -> Self {
        Self {
            caller: caller.into(),
            policy: portcullis_policy::SYSTEM_PATH.into(),
            jails: HashMap::new(),
            standby: std::env::var("LIMEN_ISLAND_STANDBY").is_ok_and(|v| v == "1"),
        }
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

/// The memory limit for an engine started any other way than as a whole
/// live page (a plain `web-island` request, the I0 fixtures): a part's.
/// Only [`Launcher::launch_island`] with `None` gets the manifest's own.
fn not_a_page(app: &str) -> Option<u64> {
    (app == crate::catalogue::NAVIGATOR_ENGINE).then_some(crate::web_island::PART_MEMORY_MB)
}

impl Launcher for Portcullis {
    fn launch(&mut self, app: &str, embed: EmbedId, channel: UnixStream) -> Result<u32, String> {
        self.start_as(Exec::Instance, app, embed, channel, None, None, None, not_a_page(app))
    }

    /// The child serves `parent_uid`: under a `per_instance` worker rule it
    /// runs as a uid of the parent's instance (its person and persona), and
    /// portcullisd says which.
    fn launch_for(&mut self, app: &str, embed: EmbedId, channel: UnixStream, parent_uid: u32) -> Result<u32, String> {
        self.start_as(Exec::Instance, app, embed, channel, Some(parent_uid), None, None, not_a_page(app))
    }

    /// The child in another of the parent's person's personas: portcullisd
    /// starts it only while that person's `share` crossing switch from the
    /// parent's persona to `persona` is on (share-sheet.md §7).
    fn launch_in(&mut self, app: &str, embed: EmbedId, channel: UnixStream, parent_uid: u32, persona: &str) -> Result<u32, String> {
        self.start_as(Exec::Instance, app, embed, channel, Some(parent_uid), Some(persona), None, not_a_page(app))
    }

    /// The app's own settings pane for Settings (curia.md §15):
    /// portcullisd checks the parent and the persona, and runs the pane's
    /// entry as the app's uid there, with its data and no capability.
    fn launch_settings(&mut self, app: &str, embed: EmbedId, channel: UnixStream, parent_uid: u32, persona: &str) -> Result<u32, String> {
        self.start_as(Exec::SettingsPane, app, embed, channel, Some(parent_uid), Some(persona), None, None)
    }

    fn launch_island(&mut self, embed: EmbedId, channel: UnixStream, parent_uid: u32, fetch: UnixStream, memory_mb: Option<u64>) -> Result<u32, String> {
        self.start_as(Exec::Instance, crate::catalogue::NAVIGATOR_ENGINE, embed, channel, Some(parent_uid), None, Some(fetch), memory_mb)
    }

    fn launch_island_standby(&mut self, embed: EmbedId, channel: UnixStream, parent_uid: u32) -> Result<u32, String> {
        if !self.standby { return Err("standby engines are off (LIMEN_ISLAND_STANDBY)".into()); }
        // No fetch socket: stdout is /dev/null, which is how the engine
        // knows to warm up and then wait for one on its channel.
        self.start_as(Exec::Instance, crate::catalogue::NAVIGATOR_ENGINE, embed, channel, Some(parent_uid), None, None, Some(crate::web_island::PART_MEMORY_MB))
    }

    /// A plugin: as any provider for the parent, held to the plugin
    /// limits whatever its manifest asks (dev-tools.md §3.8).
    /// `workspace` is the parent's to name and portcullisd's to check.
    fn launch_plugin(&mut self, app: &str, embed: EmbedId, channel: UnixStream, parent_uid: u32, workspace: Option<&str>) -> Result<u32, String> {
        self.start_limited(Exec::Instance, app, embed, channel, Some(parent_uid), None, None,
            Some(crate::catalogue::PLUGIN_MEMORY_MB), Some(crate::catalogue::PLUGIN_CPU_PCT), workspace)
    }

    fn end(&mut self, embed: EmbedId) {
        if let Some(s) = self.jails.remove(&embed) {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }

    fn usage(&mut self, embed: EmbedId) -> Option<Box<dyn crate::daemon::UsageProbe>> {
        let s = self.jails.get(&embed)?.try_clone().ok()?;
        Some(Box::new(JailUsage(s)))
    }
}

/// portcullisd answers `InstanceUsage` on the connection that holds an
/// instance: holding it is what lets limend ask.
struct JailUsage(UnixStream);

impl crate::daemon::UsageProbe for JailUsage {
    fn ask(&mut self) -> Option<(u64, Option<u64>)> {
        use portcullis_ipc::{read_response_unbuffered, write_request, Request, Response};
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // portcullisd answers one a second; two seconds is a missed answer.
        self.0.set_read_timeout(Some(std::time::Duration::from_secs(2))).ok()?;
        write_request(&mut self.0, &Request::InstanceUsage { seq }).ok()?;
        // An answer to an earlier ask that timed out may come first: skip it.
        for _ in 0..4 {
            match read_response_unbuffered(&mut self.0).ok()? {
                Response::InstanceUsage { seq: s, memory, memory_cap } if s == seq => return memory.map(|m| (m, memory_cap)),
                Response::InstanceUsage { .. } => continue,
                _ => return None,
            }
        }
        None
    }
}

/// Which of portcullisd's launches.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Exec {
    /// `ExecInstance`: a provider, a worker, an engine.
    Instance,
    /// `ExecSettingsPane`: an app's settings pane, which portcullisd runs
    /// with no capability of the app's, so with no app socket.
    SettingsPane,
}

impl Portcullis {
    #[allow(clippy::too_many_arguments)]
    fn start_as(&mut self, exec: Exec, app: &str, embed: EmbedId, channel: UnixStream, serves: Option<u32>, persona: Option<&str>, fetch: Option<UnixStream>, memory_mb: Option<u64>) -> Result<u32, String> {
        self.start_limited(exec, app, embed, channel, serves, persona, fetch, memory_mb, None, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn start_limited(&mut self, exec: Exec, app: &str, embed: EmbedId, channel: UnixStream, serves: Option<u32>, persona: Option<&str>, fetch: Option<UnixStream>, memory_mb: Option<u64>, cpu_pct: Option<u32>, workspace: Option<&str>) -> Result<u32, String> {
        use portcullis_ipc::{read_response, round_trip, send_fds, write_request, Request, Response};
        // A provider from its manifest's [roles] implements has no system
        // rule: portcullisd runs it as a uid of the parent's instance (as a
        // per-instance rule would) and says which.
        let mut uid = match self.run_as(app) {
            _ if exec == Exec::SettingsPane => None,
            Ok(u) => Some(u),
            Err(_) if serves.is_some() && implements_a_role(app) => None,
            Err(e) => return Err(e),
        };
        if exec == Exec::Instance { no_app_socket(app)? }
        let engine = app == "org.atrium.navigator.engine";
        let go = || -> io::Result<UnixStream> {
            let path = portcullis_ipc::resolve_socket_path()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "portcullisd's socket is not there"))?;
            let mut s = UnixStream::connect(path)?;
            s.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
            match round_trip(&mut s, &Request::Hello { version: portcullis_ipc::PROTO_VERSION })? {
                Response::Hello { .. } => {}
                other => return Err(io::Error::other(format!("hello: {other:?}"))),
            }
            if engine { crate::start_trace("limend", "exec-instance", app) }
            let instance = Some(format!("limen-{embed:016x}"));
            let req = match (exec, serves, persona) {
                (Exec::SettingsPane, Some(serves), Some(persona)) =>
                    Request::ExecSettingsPane { app_id: app.to_string(), instance, serves, persona: persona.to_string() },
                (Exec::SettingsPane, ..) => return Err(io::Error::other("a settings pane serves a parent in a persona")),
                (Exec::Instance, ..) => Request::ExecInstance {
                    app_id: app.to_string(),
                    instance,
                    tmpfs_mb: 64,
                    // The provider's [resources] set the caps, and `memory_mb`
                    // only tightens them (a part of a page); a machine without
                    // racct refuses rather than runs it uncapped.
                    memory_mb,
                    cpu_pct,
                    require_memory_limit: true,
                    serves,
                    persona: persona.map(str::to_string),
                    workspace: workspace.map(str::to_string),
                },
            };
            write_request(&mut s, &req)?;
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
            if engine { crate::start_trace("limend", "ready-for-fds", app) }
            send_fds(&s, &[channel.as_raw_fd(), stdout.as_raw_fd(), 2])?;
            if engine { crate::start_trace("limend", "fds-sent", app) }
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

#[cfg(test)]
mod usage_tests {
    use crate::daemon::UsageProbe;
    use portcullis_ipc::{read_request, write_response, Request, Response};

    /// The answer to this ask, past a late one to an earlier ask; and
    /// nothing from a connection whose instance has ended.
    #[test]
    fn a_late_answer_is_skipped_and_an_exit_is_no_answer() {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let t = std::thread::spawn(move || {
            let mut r = std::io::BufReader::new(theirs.try_clone().unwrap());
            let mut w = theirs;
            let Request::InstanceUsage { seq } = read_request(&mut r).unwrap() else { panic!() };
            write_response(&mut w, &Response::InstanceUsage { seq: seq.wrapping_sub(1), memory: Some(1), memory_cap: None }).unwrap();
            write_response(&mut w, &Response::InstanceUsage { seq, memory: Some(73_400_320), memory_cap: Some(1 << 30) }).unwrap();
            let Request::InstanceUsage { .. } = read_request(&mut r).unwrap() else { panic!() };
            write_response(&mut w, &Response::LaunchExit { code: Some(0) }).unwrap();
        });
        let mut p = super::JailUsage(ours);
        assert_eq!(p.ask(), Some((73_400_320, Some(1 << 30))));
        assert_eq!(p.ask(), None);
        t.join().unwrap();
    }
}

#[cfg(test)]
mod limit_tests {
    #[test]
    fn only_a_whole_page_launch_gets_the_engine_manifests_limit() {
        assert_eq!(super::not_a_page(crate::catalogue::NAVIGATOR_ENGINE), Some(crate::web_island::PART_MEMORY_MB));
        assert_eq!(super::not_a_page("org.example.viewer"), None, "other providers keep their manifest's");
    }
}
