//! Per-app executor jails (docs/spec/tier2-robust-access.md §4).
//!
//! App GPU work is app-supplied code (Tier-2 shaders run as native
//! code) and app-supplied resource ids, so apps must not share an
//! executor. In broker mode (`--transport broker`) this process only
//! accepts connections: it never reads a byte from a client. Each
//! connection is handed, as stdin, to a worker of its own:
//!
//! - by default a one-shot jail of [`WORKER_APP`] from portcullisd
//!   (the system policy's worker rule picks its uid; its bundle sets
//!   its memory cap). If portcullisd can't start it, the client is
//!   refused: the broker fails closed, never runs an unconfined worker;
//! - `AQUEDUCT_GPU_WORKER_LAUNCHER=unconfined` runs this binary again
//!   with `--transport stdin` and the broker's other arguments, for
//!   development and tests only.
//!
//! A worker (`--transport stdin`) serves exactly one session on fd 0
//! and exits when the client hangs up, which ends its jail.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::collections::HashMap;

use anyhow::{Context, Result};

/// The worker bundle the broker asks portcullisd for by default.
pub const WORKER_APP: &str = "org.atrium.aqueduct.gpu";

/// Most workers alive at once.
pub const MAX_WORKERS: usize = 64;
/// Most workers alive at once for one client uid.
pub const MAX_WORKERS_PER_UID: usize = 16;

/// How a worker is started.
#[derive(Debug, Clone)]
pub enum WorkerLaunch {
    /// One-shot jail of this bundle id from portcullisd (production).
    Portcullisd {
        /// The worker bundle's app id.
        app: String,
    },
    /// This binary with these arguments (unconfined; development only).
    Unconfined {
        /// The binary to run.
        exe: PathBuf,
        /// Its arguments.
        args: Vec<String>,
    },
}

impl WorkerLaunch {
    /// From the environment, see the module docs. `worker_args` are the
    /// arguments an unconfined worker gets.
    pub fn from_env(worker_args: Vec<String>) -> Result<Self> {
        match std::env::var("AQUEDUCT_GPU_WORKER_LAUNCHER").ok().as_deref() {
            Some("unconfined") => {
                let exe = std::env::current_exe().context("current_exe")?;
                log::warn!("broker: workers are NOT confined \
                            (AQUEDUCT_GPU_WORKER_LAUNCHER=unconfined)");
                Ok(WorkerLaunch::Unconfined { exe, args: worker_args })
            }
            Some("") | None => {
                let app = std::env::var("AQUEDUCT_GPU_WORKER_APP").ok()
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| WORKER_APP.to_string());
                match portcullis_ipc::resolve_socket_path() {
                    Some(p) => log::info!("broker: workers are {app} jails from portcullisd ({p})"),
                    None => log::warn!("broker: portcullisd's socket is missing; \
                                        every client is refused until it is there"),
                }
                Ok(WorkerLaunch::Portcullisd { app })
            }
            Some(other) => anyhow::bail!(
                "AQUEDUCT_GPU_WORKER_LAUNCHER={other}: expected unconfined or unset"),
        }
    }
}

/// A running worker.
enum Proc {
    Child(Child),
    /// portcullisd answers on `sock` when the jail ends, and ends the
    /// jail if we hang up on it.
    Jail { sock: UnixStream, exited: Arc<AtomicBool> },
}

impl Proc {
    fn spawn(launch: &WorkerLaunch, instance: &str, client: UnixStream) -> io::Result<Proc> {
        match launch {
            WorkerLaunch::Unconfined { exe, args } => {
                let child = Command::new(exe).args(args)
                    .stdin(Stdio::from(OwnedFd::from(client)))
                    .stdout(Stdio::null())
                    .spawn()?;
                Ok(Proc::Child(child))
            }
            WorkerLaunch::Portcullisd { app } => jail(app, instance, client.into()),
        }
    }

    fn describe(&self) -> String {
        match self {
            Proc::Child(c) => format!("pid {}", c.id()),
            Proc::Jail { .. } => "jail".into(),
        }
    }

    /// Block until the worker is gone.
    fn wait(&mut self) {
        match self {
            Proc::Child(c) => { let _ = c.wait(); }
            Proc::Jail { exited, sock } => {
                while !exited.load(Ordering::Acquire) {
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
                let _ = sock.shutdown(std::net::Shutdown::Both);
            }
        }
    }
}

/// Ask portcullisd to run `app` as `instance` with `stdin` (the client
/// socket) as its stdin. Same protocol as frescod's renderer jails.
fn jail(app: &str, instance: &str, stdin: OwnedFd) -> io::Result<Proc> {
    use portcullis_ipc::{read_response, round_trip, send_fds, write_request, Request, Response};
    let path = portcullis_ipc::resolve_socket_path()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "portcullisd's socket is not there"))?;
    let mut s = UnixStream::connect(path).map_err(|e| io::Error::new(e.kind(), format!("{path}: {e}")))?;
    s.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
    match round_trip(&mut s, &Request::Hello { version: portcullis_ipc::PROTO_VERSION })? {
        Response::Hello { .. } => {}
        other => return Err(io::Error::other(format!("hello: {other:?}"))),
    }
    write_request(&mut s, &Request::ExecInstance {
        app_id: app.to_string(),
        instance: Some(instance.to_string()),
        // The worker writes no files (its shader cache is in memory);
        // this is portcullisd's usual scratch layer.
        tmpfs_mb: 64,
        memory_mb: None,
        cpu_pct: None,
        // The bundle's [resources] set the caps; a machine without
        // racct refuses rather than runs a worker uncapped.
        require_memory_limit: true,
    })?;
    match read_response(&mut s)? {
        Response::ReadyForFds => {}
        Response::LaunchFailed { stage, message } => return Err(io::Error::other(format!("{stage}: {message}"))),
        Response::Error { message } => return Err(io::Error::other(message)),
        other => return Err(io::Error::other(format!("unexpected reply: {other:?}"))),
    }
    // stdout and stderr both go to our stderr (the service log): a
    // worker prints nothing on stdout, and the broker's jail has no
    // /dev/null to offer.
    send_fds(&s, &[stdin.as_raw_fd(), 2, 2])?;
    drop(stdin);
    s.set_read_timeout(None)?;
    let exited = Arc::new(AtomicBool::new(false));
    {
        let (mut r, exited, name) = (s.try_clone()?, exited.clone(), format!("{app}/{instance}"));
        std::thread::Builder::new().name(format!("gpu-jail-{instance}")).spawn(move || {
            match read_response(&mut r) {
                Ok(Response::LaunchExit { code }) => log::info!("broker: worker {name} exited ({code:?})"),
                Ok(Response::LaunchFailed { stage, message }) => log::error!("broker: worker {name} failed: {stage}: {message}"),
                Ok(other) => log::error!("broker: worker {name}: unexpected reply {other:?}"),
                Err(e) => log::info!("broker: worker {name}: portcullisd connection: {e}"),
            }
            exited.store(true, Ordering::Release);
        })?;
    }
    Ok(Proc::Jail { sock: s, exited })
}

/// The connecting process's uid.
fn peer_uid(s: &UnixStream) -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: cred/len are a valid out buffer for SO_PEERCRED.
        let r = unsafe {
            libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED,
                &mut cred as *mut _ as *mut libc::c_void, &mut len)
        };
        (r == 0).then_some(cred.uid)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: two out-params.
        (unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) } == 0).then_some(uid)
    }
}

/// Accept on `socket` forever, one worker per connection.
pub fn run(socket: &Path, launch: WorkerLaunch) -> Result<()> {
    let _ = std::fs::remove_file(socket);
    let listener = UnixListener::bind(socket)
        .with_context(|| format!("bind {}", socket.display()))?;
    // Any local process may connect: who can reach the socket is
    // decided by which jails mount its directory, and a connection
    // only gets a worker jail of its own.
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o666))
            .with_context(|| format!("chmod {}", socket.display()))?;
    }
    log::info!("broker: listening on {}", socket.display());
    let live: Arc<Mutex<HashMap<u32, usize>>> = Arc::new(Mutex::new(HashMap::new()));
    let mut next: u64 = 1;
    for incoming in listener.incoming() {
        let client = match incoming {
            Ok(c) => c,
            Err(e) => { log::warn!("broker: accept: {e}"); continue; }
        };
        let uid = peer_uid(&client).unwrap_or(u32::MAX);
        {
            let mut l = live.lock().unwrap();
            let total: usize = l.values().sum();
            let mine = l.get(&uid).copied().unwrap_or(0);
            if total >= MAX_WORKERS || mine >= MAX_WORKERS_PER_UID {
                log::warn!("broker: uid {uid}: REFUSED, at the worker cap \
                            ({mine} of {MAX_WORKERS_PER_UID}, {total} of {MAX_WORKERS})");
                continue; // drops the client
            }
            *l.entry(uid).or_insert(0) += 1;
        }
        // Named by a counter that never repeats, so a slow-to-go jail
        // can't block the next client.
        let instance = format!("gpu{next}");
        next += 1;
        let mut proc_ = match Proc::spawn(&launch, &instance, client) {
            Ok(p) => p,
            Err(e) => {
                log::error!("broker: uid {uid}: REFUSED, no worker: {e}");
                release(&live, uid);
                continue;
            }
        };
        log::info!("broker: uid {uid} -> worker {instance} ({})", proc_.describe());
        let live = live.clone();
        std::thread::Builder::new().name(format!("gpu-worker-{instance}")).spawn(move || {
            proc_.wait();
            release(&live, uid);
        })?;
    }
    Ok(())
}

fn release(live: &Mutex<HashMap<u32, usize>>, uid: u32) {
    let mut l = live.lock().unwrap();
    if let Some(n) = l.get_mut(&uid) {
        *n -= 1;
        if *n == 0 { l.remove(&uid); }
    }
}
