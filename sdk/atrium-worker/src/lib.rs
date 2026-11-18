//! An app's `[[workers]]` (app-composition.md §4, dev-tools-plan.md M5).
//!
//! A worker is the part of an app that parses what the app doesn't trust:
//! another entry in the same bundle, under the same signature, run in a
//! jail of its own as a uid of its own, with no capability at all
//! (decision C3). The core calls it with a typed [`Message`] and,
//! for `input = "fd"`, an open file; the worker answers with a message.
//!
//! The core:
//!
//! ```ignore
//! let workers = atrium_worker::Workers::get()?;
//! let thumb: Thumbnail = workers.call("thumbnailer", &Ask { size: 256 }, Some(file))?;
//! ```
//!
//! The worker (its own binary, `entry` in its `[[workers]]` entry):
//!
//! ```ignore
//! fn thumbnail(ask: Ask, file: Option<std::fs::File>) -> Result<Thumbnail, String> { … }
//! atrium_worker::main!(thumbnail);
//! ```
//!
//! How a worker runs, chosen by [`Workers::get`]:
//!
//! - **jail** (`isolation = "jail"`, the default): portcullisd's
//!   `WorkerCall`, a one-shot jail per call (or per [`Session`]).
//! - **capsicum** (`isolation = "capsicum"`): a child process of the core,
//!   in the core's jail, that enters capability mode before it reads
//!   anything (decision C4: for many tiny calls).
//! - **local** (`ATRIUM_WORKERS=local`): a plain child, unconfined, for
//!   development and tests off Atrium.
//! - **in-process** ([`Workers::in_process`]): a thread, for unit tests.
//!
//! A call that crashes the worker, is killed at a limit or takes longer
//! than the worker's `cpu` time comes back as an [`Error`], never as a
//! panic in the core.
//!
//! The wire, on the worker's standard input (a socket): a call is a flag
//! byte (1 when a descriptor rides with it), a u32 little-endian length and
//! a versioned frame; the answer is a status byte (0 ok, 1 the worker's
//! error), a length and the frame or the error's text.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use atrium_message::Message;

mod wire;

/// The most a call or an answer may be, unless the caller says otherwise.
pub const DEFAULT_MAX: usize = 64 << 20;
/// A call's time when the worker's entry names none.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Why a call has no answer.
#[derive(Debug)]
pub enum Error {
    /// No `[[workers]]` entry of this name in the app's manifest.
    NoSuchWorker(String),
    /// It could not be started (portcullisd refused, or the program is not
    /// there).
    Start(String),
    /// It ran past its time and was ended.
    Timeout(Duration),
    /// It ended without answering: `Some(code)`, its exit status; `None`,
    /// killed by a signal (a memory limit ends a worker that way).
    Crashed(Option<i32>),
    /// It answered with an error of its own.
    Worker(String),
    /// Its answer was not a valid message (or ours was refused).
    Message(atrium_message::Error),
    Io(io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NoSuchWorker(n) => write!(f, "no worker {n:?} in the manifest"),
            Error::Start(why) => write!(f, "worker did not start: {why}"),
            Error::Timeout(d) => write!(f, "worker took longer than {d:?} and was ended"),
            Error::Crashed(Some(c)) => write!(f, "worker exited with {c} without answering"),
            Error::Crashed(None) => write!(f, "worker was killed (a limit, or a signal) without answering"),
            Error::Worker(why) => write!(f, "worker: {why}"),
            Error::Message(e) => write!(f, "worker's answer: {e}"),
            Error::Io(e) => write!(f, "worker: {e}"),
        }
    }
}
impl std::error::Error for Error {}
impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self { Error::Io(e) }
}

/// One worker as the manifest declares it.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct WorkerDef {
    pub name: String,
    pub entry: String,
    #[serde(default)]
    pub input: Option<String>,
    #[serde(default)]
    pub lifetime: Option<String>,
    #[serde(default)]
    pub memory: Option<String>,
    #[serde(default)]
    pub cpu: Option<String>,
    #[serde(default)]
    pub isolation: Option<String>,
}

impl WorkerDef {
    /// Its time per call: `cpu` ("10s", "2m", "500ms"), else the default.
    pub fn timeout(&self) -> Duration {
        self.cpu.as_deref().and_then(parse_duration).unwrap_or(DEFAULT_TIMEOUT)
    }
}

/// "10s", "2m", "500ms", "1h".
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit())?);
    let n: u64 = n.parse().ok()?;
    Some(match unit {
        "ms" => Duration::from_millis(n),
        "s" => Duration::from_secs(n),
        "m" => Duration::from_secs(n.checked_mul(60)?),
        "h" => Duration::from_secs(n.checked_mul(3600)?),
        _ => return None,
    })
}

#[derive(serde::Deserialize)]
struct ManifestWorkers {
    #[serde(default)]
    workers: Vec<WorkerDef>,
}

type Handler = Box<dyn Fn(&[u8], Option<File>) -> Result<Vec<u8>, String> + Send + Sync>;

enum Mode {
    /// portcullisd's WorkerCall, or a capsicum child, by the entry's isolation.
    Atrium { root: PathBuf },
    /// Plain children from `root`.
    Local { root: PathBuf },
    InProcess { handlers: BTreeMap<String, std::sync::Arc<Handler>> },
}

/// The app's workers, from the core's side.
pub struct Workers {
    defs: BTreeMap<String, WorkerDef>,
    mode: Mode,
    max: usize,
}

impl Workers {
    /// The workers of the running app: its manifest at `$ATRIUM_APP_ROOT`
    /// (`/` in its jail, where the bundle is). `ATRIUM_WORKERS=local` runs
    /// them as plain children instead, for development off Atrium.
    pub fn get() -> Result<Workers, Error> {
        let root = PathBuf::from(std::env::var_os("ATRIUM_APP_ROOT").unwrap_or_else(|| "/".into()));
        let local = std::env::var("ATRIUM_WORKERS").is_ok_and(|v| v == "local");
        Self::from_root(&root, local)
    }

    /// The workers of the bundle at `root`; `local` as for [`get`](Self::get).
    pub fn from_root(root: &Path, local: bool) -> Result<Workers, Error> {
        let text = std::fs::read_to_string(root.join("atrium.toml"))
            .map_err(|e| Error::Start(format!("{}: {e}", root.join("atrium.toml").display())))?;
        let m: ManifestWorkers = toml::from_str(&text).map_err(|e| Error::Start(format!("atrium.toml: {e}")))?;
        let defs = m.workers.into_iter().map(|w| (w.name.clone(), w)).collect();
        let mode = if local { Mode::Local { root: root.into() } } else { Mode::Atrium { root: root.into() } };
        Ok(Workers { defs, mode, max: DEFAULT_MAX })
    }

    /// Workers that are functions in this process, for unit tests: the
    /// same encoding and checks, no isolation.
    pub fn in_process() -> Workers {
        Workers { defs: BTreeMap::new(), mode: Mode::InProcess { handlers: BTreeMap::new() }, max: DEFAULT_MAX }
    }

    /// Add `f` as worker `name` (in-process only).
    pub fn register<Req, Resp, F>(mut self, name: &str, timeout: Duration, f: F) -> Self
    where Req: Message, Resp: Message, F: Fn(Req, Option<File>) -> Result<Resp, String> + Send + Sync + 'static {
        let max = self.max;
        if let Mode::InProcess { handlers } = &mut self.mode {
            let h: Handler = Box::new(move |frame: &[u8], fd: Option<File>| {
                let req = Req::from_frame(frame, max).map_err(|e| format!("call: {e}"))?;
                f(req, fd).map(|r| r.to_frame())
            });
            handlers.insert(name.to_string(), std::sync::Arc::new(h));
            self.defs.insert(name.to_string(), WorkerDef {
                name: name.into(), entry: String::new(), input: None, lifetime: None, memory: None,
                cpu: Some(format!("{}ms", timeout.as_millis())), isolation: None,
            });
        }
        self
    }

    /// The largest answer to accept (default [`DEFAULT_MAX`]).
    pub fn max_answer(mut self, bytes: usize) -> Self { self.max = bytes; self }

    /// The declared workers.
    pub fn defs(&self) -> impl Iterator<Item = &WorkerDef> { self.defs.values() }

    /// One call in a worker of its own, which ends with it.
    pub fn call<Req: Message, Resp: Message>(&self, name: &str, req: &Req, input: Option<File>) -> Result<Resp, Error> {
        let mut s = self.session(name)?;
        s.call(req, input)
    }

    /// A worker kept for many calls (`lifetime = "session"`), until the
    /// [`Session`] is dropped.
    pub fn session(&self, name: &str) -> Result<Session, Error> {
        let def = self.defs.get(name).ok_or_else(|| Error::NoSuchWorker(name.into()))?.clone();
        let (ours, theirs) = UnixStream::pair()?;
        let err = dup_stderr()?;
        let proc = match &self.mode {
            Mode::InProcess { handlers } => {
                let h = handlers[name].clone();
                let t = std::thread::Builder::new().name(format!("worker-{name}")).spawn(move || {
                    let mut s = theirs;
                    let _ = wire::serve_on(&mut s, |frame, fd| h(frame, fd));
                })?;
                Proc::Thread(Some(t))
            }
            Mode::Local { root } => Proc::Child(spawn(root, &def, theirs, err, false)?),
            Mode::Atrium { root } => {
                if def.isolation.as_deref() == Some("capsicum") {
                    Proc::Child(spawn(root, &def, theirs, err, true)?)
                } else {
                    Proc::Jail(worker_call(name, &theirs, &err)?)
                }
            }
        };
        Ok(Session { sock: ours, proc, timeout: def.timeout(), max: self.max, broken: false })
    }
}

fn dup_stderr() -> io::Result<OwnedFd> {
    std::os::fd::AsFd::as_fd(&io::stderr()).try_clone_to_owned()
}

fn spawn(root: &Path, def: &WorkerDef, sock: UnixStream, err: OwnedFd, capsicum: bool) -> Result<std::process::Child, Error> {
    use std::process::{Command, Stdio};
    let prog = root.join(def.entry.trim_start_matches('/'));
    let out: OwnedFd = err.try_clone()?;
    let mut c = Command::new(&prog);
    c.stdin(Stdio::from(OwnedFd::from(sock))).stdout(Stdio::from(out)).stderr(Stdio::from(err))
        .env_clear().env("PATH", "/usr/bin:/bin");
    if capsicum { c.env(wire::CAPSICUM_ENV, "1"); }
    c.spawn().map_err(|e| Error::Start(format!("{}: {e}", prog.display())))
}

/// portcullisd: `PORTCULLIS_SOCKET`, else its worker-call socket.
fn portcullisd() -> io::Result<UnixStream> {
    let path = std::env::var("PORTCULLIS_SOCKET").ok().or_else(|| {
        // The worker-call socket an app with [[workers]] has in its jail,
        // then portcullisd's own (outside a jail).
        ["/atrium/sockets/workers/workers.sock", "/atrium/sockets/portcullis/portcullis.sock", "/atrium/sockets/portcullis.sock"]
            .into_iter().find(|p| Path::new(p).exists()).map(String::from)
    }).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "portcullisd's socket is not there"))?;
    UnixStream::connect(path)
}

fn json_line(s: &mut UnixStream, v: &serde_json::Value) -> io::Result<()> {
    let mut b = serde_json::to_vec(v).map_err(io::Error::other)?;
    b.push(b'\n');
    s.write_all(&b)
}

/// One line, a byte at a time, so nothing after it is read ahead.
fn read_line(s: &mut UnixStream) -> io::Result<serde_json::Value> {
    let mut line = Vec::new();
    let mut b = [0u8; 1];
    loop {
        if s.read(&mut b)? == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "portcullisd closed"));
        }
        if b[0] == b'\n' { break }
        line.push(b[0]);
        if line.len() > 1 << 20 { return Err(io::Error::other("portcullisd's answer is too long")) }
    }
    serde_json::from_slice(&line).map_err(io::Error::other)
}

/// `WorkerCall`: the worker's jail, on `sock` (stdin) and `err` (stdout,
/// stderr). Returns the connection it lives by.
fn worker_call(name: &str, sock: &UnixStream, err: &OwnedFd) -> Result<UnixStream, Error> {
    let start = |e: io::Error| Error::Start(format!("portcullisd: {e}"));
    let mut s = portcullisd().map_err(start)?;
    s.set_read_timeout(Some(Duration::from_secs(30))).map_err(start)?;
    json_line(&mut s, &serde_json::json!({"op": "hello", "version": 1})).map_err(start)?;
    let hello = read_line(&mut s).map_err(start)?;
    if hello["kind"] != "hello" {
        return Err(Error::Start(format!("portcullisd: {hello}")));
    }
    json_line(&mut s, &serde_json::json!({"op": "worker_call", "worker": name})).map_err(start)?;
    let ready = read_line(&mut s).map_err(start)?;
    match ready["kind"].as_str() {
        Some("ready_for_fds") => {}
        Some("launch_failed") => return Err(Error::Start(format!("{}: {}",
            ready["stage"].as_str().unwrap_or("?"), ready["message"].as_str().unwrap_or("?")))),
        _ => return Err(Error::Start(format!("portcullisd: {ready}"))),
    }
    wire::send_with_fds(&s, &[sock.as_raw_fd(), err.as_raw_fd(), err.as_raw_fd()], b"F").map_err(start)?;
    s.set_read_timeout(None).map_err(start)?;
    Ok(s)
}

enum Proc {
    Jail(UnixStream),
    Child(std::process::Child),
    Thread(Option<std::thread::JoinHandle<()>>),
}

/// A running worker.
pub struct Session {
    sock: UnixStream,
    proc: Proc,
    timeout: Duration,
    max: usize,
    broken: bool,
}

impl Session {
    /// Change the time a call may take.
    pub fn set_timeout(&mut self, d: Duration) { self.timeout = d }

    /// Call it: `req`, and for an `fd` worker the file to read.
    pub fn call<Req: Message, Resp: Message>(&mut self, req: &Req, input: Option<File>) -> Result<Resp, Error> {
        if self.broken {
            return Err(Error::Crashed(self.exit()));
        }
        let frame = req.to_frame();
        if let Err(e) = wire::send_call(&self.sock, &frame, input.as_ref().map(|f| f.as_raw_fd())) {
            self.broken = true;
            return Err(match e.kind() {
                io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset => Error::Crashed(self.exit()),
                _ => Error::Io(e),
            });
        }
        drop(input);
        self.sock.set_read_timeout(Some(self.timeout))?;
        match wire::read_answer(&mut self.sock, self.max) {
            Ok(Ok(frame)) => Resp::from_frame(&frame, self.max).map_err(Error::Message),
            Ok(Err(why)) => Err(Error::Worker(why)),
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                self.broken = true;
                self.end();
                Err(Error::Timeout(self.timeout))
            }
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                self.broken = true;
                Err(Error::Crashed(self.exit()))
            }
            Err(e) => { self.broken = true; self.end(); Err(Error::Io(e)) }
        }
    }

    /// End it now.
    fn end(&mut self) {
        let _ = self.sock.shutdown(std::net::Shutdown::Both);
        match &mut self.proc {
            // Hanging up ends the jail (portcullisd removes it).
            Proc::Jail(c) => { let _ = c.shutdown(std::net::Shutdown::Both); }
            Proc::Child(c) => { let _ = c.kill(); let _ = c.wait(); }
            Proc::Thread(_) => {}
        }
    }

    /// How it ended, once it has.
    fn exit(&mut self) -> Option<i32> {
        match &mut self.proc {
            Proc::Jail(c) => {
                let _ = c.set_read_timeout(Some(Duration::from_secs(10)));
                match read_line(c) {
                    Ok(v) if v["kind"] == "launch_exit" => v["code"].as_i64().map(|c| c as i32),
                    _ => None,
                }
            }
            Proc::Child(c) => {
                use std::os::unix::process::ExitStatusExt;
                c.wait().ok().and_then(|s| if s.signal().is_some() { None } else { s.code() })
            }
            Proc::Thread(t) => {
                // A handler that panicked is a crash.
                match t.take().map(|t| t.join()) { Some(Err(_)) => None, _ => Some(0) }
            }
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // End of input: the worker's loop ends, then it does.
        let _ = self.sock.shutdown(std::net::Shutdown::Write);
        match &mut self.proc {
            Proc::Jail(c) => { let _ = c.shutdown(std::net::Shutdown::Both); }
            Proc::Child(c) => {
                // A worker that ignores end of input is ended.
                for _ in 0..50 {
                    if matches!(c.try_wait(), Ok(Some(_))) { return }
                    std::thread::sleep(Duration::from_millis(10));
                }
                let _ = c.kill();
                let _ = c.wait();
            }
            Proc::Thread(t) => { if let Some(t) = t.take() { let _ = t.join(); } }
        }
    }
}

/// The worker's side: answer calls on standard input until it closes,
/// then exit. Under `isolation = "capsicum"` it enters capability mode
/// first. A handler's `Err` goes back to the core as [`Error::Worker`].
pub fn serve<Req, Resp, F>(mut f: F) -> !
where Req: Message, Resp: Message, F: FnMut(Req, Option<File>) -> Result<Resp, String> {
    if std::env::var_os(wire::CAPSICUM_ENV).is_some() {
        if let Err(e) = wire::cap_enter() {
            eprintln!("atrium-worker: capability mode: {e}");
            std::process::exit(70);
        }
    }
    // SAFETY: standard input is the socket the core gave us, and nothing
    // else in this process owns descriptor 0.
    let mut s = unsafe { UnixStream::from_raw_fd(0) };
    let max = wire::max_call();
    let r = wire::serve_on(&mut s, |frame, fd| {
        let req = Req::from_frame(frame, max).map_err(|e| format!("call: {e}"))?;
        f(req, fd).map(|r| r.to_frame())
    });
    std::process::exit(match r { Ok(()) => 0, Err(e) => { eprintln!("atrium-worker: {e}"); 1 } })
}

/// `fn main` for a worker: `atrium_worker::main!(handler);`, where
/// `handler(req, file) -> Result<Resp, String>`.
#[macro_export]
macro_rules! main {
    ($f:path) => {
        fn main() { $crate::serve($f) }
    };
}
