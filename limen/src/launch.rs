//! The real [`Launcher`](crate::daemon::Launcher): portcullisd's
//! `ExecInstance` (limen.md §4, §12 L3), the way frescod starts renderers.
//!
//! The child's channel is its stdin (a Unix socket, so it talks both ways);
//! its stdout goes nowhere and its stderr is limend's. portcullisd ends the
//! jail when limend hangs up the request connection, which is how
//! [`end`](crate::daemon::Launcher::end) works.
//!
//! **Which uid the child runs as.** `ExecInstance` runs a worker as its
//! caller unless the system policy (`/etc/atrium/policy.toml`) has a worker
//! rule for the app naming the caller. A provider must never run as
//! `_limen`, so a launch without a rule `[workers."<app>"] callers =
//! ["_limen"], run_as = "<its own user>"` is refused here, before
//! portcullisd is asked. That uid is what limend then expects to own the
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

fn uid_of(name: &str) -> Option<u32> {
    let c = std::ffi::CString::new(name).ok()?;
    // SAFETY: getpwnam with a valid C string; the result is read at once.
    let pw = unsafe { libc::getpwnam(c.as_ptr()) };
    (!pw.is_null()).then(|| unsafe { (*pw).pw_uid })
}

impl Launcher for Portcullis {
    fn launch(&mut self, app: &str, embed: EmbedId, channel: UnixStream) -> Result<u32, String> {
        use portcullis_ipc::{read_response, round_trip, send_fds, write_request, Request, Response};
        let uid = self.run_as(app)?;
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
            })?;
            match read_response(&mut s)? {
                Response::ReadyForFds => {}
                Response::LaunchFailed { stage, message } => return Err(io::Error::other(format!("{stage}: {message}"))),
                Response::Error { message } => return Err(io::Error::other(message)),
                other => return Err(io::Error::other(format!("unexpected reply: {other:?}"))),
            }
            let null = std::fs::OpenOptions::new().write(true).open("/dev/null")?;
            send_fds(&s, &[channel.as_raw_fd(), null.as_raw_fd(), 2])?;
            s.set_read_timeout(None)?;
            Ok(s)
        };
        let s = go().map_err(|e| e.to_string())?;
        drop(channel);
        self.jails.insert(embed, s);
        Ok(uid)
    }

    fn end(&mut self, embed: EmbedId) {
        if let Some(s) = self.jails.remove(&embed) {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
    }
}
