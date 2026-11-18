//! The workers navigatord starts: converters and fetchers
//! (navigator-ui.md §2.3a, J1).
//!
//! A worker is named either by a command (`prog arg… {instance}`, run as
//! an ordinary child of navigatord, for tests and the unconfined lane) or
//! by `jail:<app id>`: a one-shot jail that portcullisd starts with
//! `ExecInstance` on three pipe ends navigatord passes it, the same way
//! limend starts page children. The jailed form needs no `portcullis`
//! program, so navigatord can run in a jail of its own with only
//! portcullisd's socket.
//!
//! A jailed worker lives while its portcullisd connection is open:
//! hanging up ends it, and its exit comes back on that connection.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Spec {
    /// A program and its arguments; `{instance}` in an argument is
    /// replaced by the run's instance name.
    Command(String, Vec<String>),
    /// An installed app run by portcullisd's `ExecInstance`.
    Jail(String),
}

impl Spec {
    /// `jail:<app id>`, or whitespace-separated words (no shell).
    pub fn parse(v: &str) -> Option<Spec> {
        let v = v.trim();
        if let Some(app) = v.strip_prefix("jail:") {
            let app = app.trim();
            return (!app.is_empty() && !app.contains(char::is_whitespace)).then(|| Spec::Jail(app.to_string()));
        }
        let w: Vec<String> = v.split_whitespace().map(String::from).collect();
        let (p, a) = w.split_first()?;
        Some(Spec::Command(p.clone(), a.to_vec()))
    }
}

enum Kind {
    Proc(std::process::Child),
    Jail { conn: UnixStream, exited: mpsc::Receiver<()>, done: bool },
}

/// A running worker and the ends navigatord keeps.
pub struct Worker {
    kind: Kind,
    pub stdin: Option<Box<dyn Write + Send>>,
    pub stdout: Option<Box<dyn Read + Send>>,
    /// `None` when the worker's errors go to navigatord's own.
    pub stderr: Option<Box<dyn Read + Send>>,
}

/// portcullisd's socket: `PORTCULLIS_SOCKET`, else where portcullisd puts it.
fn portcullisd() -> io::Result<UnixStream> {
    let path = crate::settings::get("PORTCULLIS_SOCKET")
        .or_else(|| portcullis_ipc::resolve_socket_path().map(String::from))
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "portcullisd's socket is not there"))?;
    UnixStream::connect(path)
}

/// Our persona (personas.md §3), as portcullisd's launch registry has it
/// by our uid: `None` for Personal, else its name and colour; and the
/// person's other personas (`personal` for Personal).
pub fn persona() -> io::Result<(Option<(String, String)>, Vec<String>)> {
    use portcullis_ipc::{round_trip, Request, Response};
    let mut s = portcullisd()?;
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    match round_trip(&mut s, &Request::Hello { version: portcullis_ipc::PROTO_VERSION })? {
        Response::Hello { .. } => {}
        other => return Err(io::Error::other(format!("portcullisd hello: {other:?}"))),
    }
    match round_trip(&mut s, &Request::Persona)? {
        Response::Persona { name, colour, others } => Ok((name.map(|n| (n, colour.unwrap_or_default())), others)),
        other => Err(io::Error::other(format!("portcullisd persona: {other:?}"))),
    }
}

/// Save a held download (`SaveDownload`, personas.md §4.1): portcullisd
/// writes it into the Downloads of our persona; we never hold the
/// person's folders. Its name there, and the folder in words.
pub fn save_download(name: &str, origin: &str, bytes: &[u8]) -> Result<(String, String), String> {
    use portcullis_ipc::{round_trip, send_fds, Request, Response};
    use std::os::fd::AsRawFd;
    let fd = crate::files::sealed(bytes).map_err(|e| format!("a memory file: {e}"))?;
    let ask = || -> io::Result<Response> {
        let mut s = portcullisd()?;
        s.set_read_timeout(Some(Duration::from_secs(60)))?;
        match round_trip(&mut s, &Request::Hello { version: portcullis_ipc::PROTO_VERSION })? {
            Response::Hello { .. } => {}
            other => return Err(io::Error::other(format!("portcullisd hello: {other:?}"))),
        }
        match round_trip(&mut s, &Request::SaveDownload { name: name.into(), origin: origin.into() })? {
            Response::ReadyForFds => {}
            other => return Ok(other),
        }
        send_fds(&s, &[fd.as_raw_fd()])?;
        portcullis_ipc::read_response(&mut s)
    };
    match ask() {
        Ok(Response::Downloaded { name, place }) => Ok((name, place)),
        Ok(Response::Error { message }) => Err(message),
        Ok(other) => Err(format!("unexpected answer: {other:?}")),
        Err(e) => Err(format!("portcullisd: {e}")),
    }
}

/// Open a held download in the person's app for it (`OpenDownload`,
/// navigator-ui.md §6.4): the app's name.
pub fn open_download(name: &str, bytes: &[u8]) -> Result<String, String> {
    use portcullis_ipc::{round_trip, send_fds, Request, Response};
    use std::os::fd::AsRawFd;
    let fd = crate::files::sealed(bytes).map_err(|e| format!("a memory file: {e}"))?;
    let ask = || -> io::Result<Response> {
        let mut s = portcullisd()?;
        s.set_read_timeout(Some(Duration::from_secs(60)))?;
        match round_trip(&mut s, &Request::Hello { version: portcullis_ipc::PROTO_VERSION })? {
            Response::Hello { .. } => {}
            other => return Err(io::Error::other(format!("portcullisd hello: {other:?}"))),
        }
        match round_trip(&mut s, &Request::OpenDownload { name: name.into() })? {
            Response::ReadyForFds => {}
            other => return Ok(other),
        }
        send_fds(&s, &[fd.as_raw_fd()])?;
        portcullis_ipc::read_response(&mut s)
    };
    match ask() {
        Ok(Response::Opened { app, .. }) => Ok(app),
        Ok(Response::Error { message }) => Err(message),
        Ok(other) => Err(format!("unexpected answer: {other:?}")),
        Err(e) => Err(format!("portcullisd: {e}")),
    }
}

/// A window of ours in `persona` (`NewWindowIn`), with the person's press.
pub fn new_window_in(persona: &str, press: [u8; 16]) -> Result<(), String> {
    use portcullis_ipc::{round_trip, Request, Response};
    let ask = || -> io::Result<Response> {
        let mut s = portcullisd()?;
        s.set_read_timeout(Some(Duration::from_secs(10)))?;
        match round_trip(&mut s, &Request::Hello { version: portcullis_ipc::PROTO_VERSION })? {
            Response::Hello { .. } => {}
            other => return Err(io::Error::other(format!("portcullisd hello: {other:?}"))),
        }
        round_trip(&mut s, &Request::NewWindowIn { persona: persona.into(), activation: press })
    };
    match ask() {
        Ok(Response::Ok) => Ok(()),
        Ok(Response::Error { message }) => Err(message),
        Ok(other) => Err(format!("unexpected answer: {other:?}")),
        Err(e) => Err(format!("portcullisd: {e}")),
    }
}

impl Worker {
    /// Start `spec` as run `instance`; its errors are captured when
    /// `capture_err`, else they go to navigatord's.
    pub fn start(spec: &Spec, instance: &str, capture_err: bool) -> io::Result<Worker> {
        match spec {
            Spec::Command(p, a) => {
                use std::process::{Command, Stdio};
                let mut c = Command::new(p);
                c.args(a.iter().map(|x| x.replace("{instance}", instance)));
                c.stdin(Stdio::piped()).stdout(Stdio::piped())
                    .stderr(if capture_err { Stdio::piped() } else { Stdio::inherit() });
                let mut ch = c.spawn()?;
                let stdin: Box<dyn Write + Send> = Box::new(ch.stdin.take().expect("piped"));
                let stdout: Box<dyn Read + Send> = Box::new(ch.stdout.take().expect("piped"));
                let stderr = ch.stderr.take().map(|e| Box::new(e) as Box<dyn Read + Send>);
                Ok(Worker { kind: Kind::Proc(ch), stdin: Some(stdin), stdout: Some(stdout), stderr })
            }
            Spec::Jail(app) => Self::jailed(app, instance, capture_err),
        }
    }

    /// portcullisd's `ExecInstance` of `app` on `fds` (stdin, stdout,
    /// stderr); the connection the worker lives by.
    fn exec(app: &str, instance: &str, fds: &[std::os::fd::RawFd; 3]) -> io::Result<UnixStream> {
        use portcullis_ipc::{read_response, round_trip, send_fds, write_request, Request, Response};
        let mut s = portcullisd()?;
        s.set_read_timeout(Some(Duration::from_secs(30)))?;
        match round_trip(&mut s, &Request::Hello { version: portcullis_ipc::PROTO_VERSION })? {
            Response::Hello { .. } => {}
            other => return Err(io::Error::other(format!("portcullisd hello: {other:?}"))),
        }
        write_request(&mut s, &Request::ExecInstance {
            app_id: app.to_string(),
            instance: Some(instance.to_string()),
            tmpfs_mb: 64,
            // The worker's own [resources] set its caps; a machine that
            // can't enforce a memory cap refuses rather than runs it uncapped.
            memory_mb: None,
            cpu_pct: None,
            require_memory_limit: true,
            serves: None,
        })?;
        match read_response(&mut s)? {
            Response::ReadyForFds => {}
            Response::LaunchFailed { stage, message } => return Err(io::Error::other(format!("{app}: {stage}: {message}"))),
            Response::Error { message } => return Err(io::Error::other(format!("{app}: {message}"))),
            other => return Err(io::Error::other(format!("{app}: unexpected reply {other:?}"))),
        }
        send_fds(&s, fds)?;
        s.set_read_timeout(None)?;
        Ok(s)
    }

    /// A jailed worker on connection `s`, its end over when the connection
    /// says so.
    fn watched(s: UnixStream, stdin: Option<Box<dyn Write + Send>>, stdout: Option<Box<dyn Read + Send>>, stderr: Option<Box<dyn Read + Send>>) -> io::Result<Worker> {
        let (tx, exited) = mpsc::channel();
        let mut r = s.try_clone()?;
        std::thread::Builder::new().name("navigatord-worker".into()).spawn(move || {
            // LaunchExit, an error or a hang-up: the worker is over either way.
            let _ = portcullis_ipc::read_response(&mut r);
            let _ = tx.send(());
        })?;
        Ok(Worker { kind: Kind::Jail { conn: s, exited, done: false }, stdin, stdout, stderr })
    }

    fn jailed(app: &str, instance: &str, capture_err: bool) -> io::Result<Worker> {
        let (in_r, in_w) = io::pipe()?;
        let (out_r, out_w) = io::pipe()?;
        let (err_r, err_w): (Option<io::PipeReader>, OwnedFd) = if capture_err {
            let (r, w) = io::pipe()?;
            (Some(r), w.into())
        } else {
            (None, std::os::fd::AsFd::as_fd(&io::stderr()).try_clone_to_owned()?)
        };
        let s = Self::exec(app, instance, &[in_r.as_raw_fd(), out_w.as_raw_fd(), err_w.as_raw_fd()])?;
        // Only the worker holds these ends now: its exit closes them.
        drop((in_r, out_w, err_w));
        Self::watched(s, Some(Box::new(in_w)), Some(Box::new(out_r)), err_r.map(|r| Box::new(r) as Box<dyn Read + Send>))
    }

    /// Start `spec` on `channel`, a socket that is its stdin (its stdout
    /// and stderr are ours): a worker that talks both ways and is passed
    /// descriptors, as the UI is (navigator-ui.md §2.3a J3).
    pub fn start_on(spec: &Spec, instance: &str, channel: OwnedFd) -> io::Result<Worker> {
        match spec {
            Spec::Command(p, a) => {
                use std::process::{Command, Stdio};
                let ch = Command::new(p).args(a.iter().map(|x| x.replace("{instance}", instance)))
                    .stdin(Stdio::from(channel)).stdout(Stdio::inherit()).stderr(Stdio::inherit()).spawn()?;
                Ok(Worker { kind: Kind::Proc(ch), stdin: None, stdout: None, stderr: None })
            }
            Spec::Jail(app) => {
                let err = std::os::fd::AsFd::as_fd(&io::stderr()).try_clone_to_owned()?;
                let s = Self::exec(app, instance, &[channel.as_raw_fd(), err.as_raw_fd(), err.as_raw_fd()])?;
                drop(channel);
                Self::watched(s, None, None, None)
            }
        }
    }

    /// Whether it has ended.
    pub fn try_wait(&mut self) -> io::Result<bool> {
        match &mut self.kind {
            Kind::Proc(c) => Ok(c.try_wait()?.is_some()),
            Kind::Jail { exited, done, .. } => {
                if !*done {
                    *done = !matches!(exited.try_recv(), Err(mpsc::TryRecvError::Empty));
                }
                Ok(*done)
            }
        }
    }

    /// End it now.
    pub fn kill(&mut self) {
        match &mut self.kind {
            Kind::Proc(c) => {
                let _ = c.kill();
                let _ = c.wait();
            }
            Kind::Jail { conn, exited, done } => {
                let _ = conn.shutdown(std::net::Shutdown::Both);
                if !*done {
                    let _ = exited.recv_timeout(Duration::from_secs(5));
                    *done = true;
                }
            }
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Kind::Jail { conn, .. } = &self.kind {
            let _ = conn.shutdown(std::net::Shutdown::Both);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs() {
        assert_eq!(Spec::parse("jail:org.atrium.navigator.converter"), Some(Spec::Jail("org.atrium.navigator.converter".into())));
        assert_eq!(Spec::parse("jail: "), None);
        assert_eq!(Spec::parse("jail:a b"), None);
        assert_eq!(Spec::parse("/usr/local/bin/portcullis exec --instance f{instance} x"),
            Some(Spec::Command("/usr/local/bin/portcullis".into(), vec!["exec".into(), "--instance".into(), "f{instance}".into(), "x".into()])));
        assert_eq!(Spec::parse("  "), None);
    }

    #[test]
    fn a_command_worker_runs_and_ends() {
        let mut w = Worker::start(&Spec::Command("cat".into(), vec![]), "t", true).unwrap();
        w.stdin.as_mut().unwrap().write_all(b"hi").unwrap();
        drop(w.stdin.take());
        let mut s = String::new();
        w.stdout.take().unwrap().read_to_string(&mut s).unwrap();
        assert_eq!(s, "hi");
        for _ in 0..100 {
            if w.try_wait().unwrap() { return; }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("cat did not end");
    }
}
