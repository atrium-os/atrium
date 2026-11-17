//! Workers through jaild (printing.md §6.1, §6.3; `[print_workers]` in its
//! policy). One jail per worker, named `system-prelum-<uid>` at
//! `/var/lib/atrium/jails/<name>`, running as a uid from prelumd's pool:
//!
//! - **prelum-render**: no network (`Disable`), the document on stdin, the
//!   socket on stdout; the system call class `renderer`; optionally rctl
//!   caps (`--rctl`, needs `kern.racct.enable=1`).
//! - **prelum-ipp**: its own vnet (`Routed`) whose pf anchor passes only TCP
//!   to the printer's address and port; the socket on stdin and stdout; the
//!   class `networked`.
//!
//! The root holds the base libraries and the installed binaries, read-only,
//! and nothing else: no sockets, no user files, no trust store (printers are
//! pinned by key).
//!
//! A worker's jail is removed when it exits; a janitor retries what can't be
//! removed at once (a vnet jail lingers while its TCP connections are in
//! TIME_WAIT) and only then returns the uid to the pool.

use std::collections::BTreeSet;
use std::io;
use std::net::SocketAddr;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jaild::protocol::{
    CreateJailRequest, ExecSpec, MountKind, MountSpec, NetDest, NetGrants, NetworkConfig, RctlLimits, Request, Response,
};

use crate::launch::{Launcher, Proc, Stopper};

pub const JAILS_DIR: &str = "/var/lib/atrium/jails";
/// jaild's `[print_workers]`: these must match its policy.
pub const NAME_PREFIX: &str = "system-prelum-";
pub const RENDER_EXEC: &str = "/usr/local/bin/atrium-prelum-render";
pub const IPP_EXEC: &str = "/usr/local/bin/atrium-prelum-ipp";
pub const UID_FIRST: u32 = 62000;
pub const UID_LAST: u32 = 62499;
/// The app devfs ruleset (portcullis_jail::APP_DEVFS_RULESET).
const DEVFS_RULESET: u32 = 22;
/// prelum-render's memory cap (§6.1: 1 GB by default).
const RENDER_MEMORY_MB: u64 = 1024;

pub fn name(uid: u32) -> String {
    format!("{NAME_PREFIX}{uid}")
}

pub fn root(uid: u32) -> PathBuf {
    Path::new(JAILS_DIR).join(name(uid))
}

struct Pool {
    free: BTreeSet<u32>,
    /// Exited, not yet torn down.
    dying: Vec<u32>,
}

pub struct Jailed {
    jaild: String,
    rctl: bool,
    pool: Arc<Mutex<Pool>>,
}

impl Jailed {
    pub fn new(jaild: &str, rctl: bool) -> Result<Jailed, String> {
        let pool = Arc::new(Mutex::new(Pool { free: (UID_FIRST..=UID_LAST).collect(), dying: Vec::new() }));
        let j = Jailed { jaild: jaild.into(), rctl, pool: pool.clone() };
        // Wreckage of an earlier prelumd: its jails go first.
        if let Ok(rd) = std::fs::read_dir(JAILS_DIR) {
            let mut p = pool.lock().unwrap();
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if let Some(uid) = n.strip_prefix(NAME_PREFIX).and_then(|u| u.parse::<u32>().ok()) {
                    if n == name(uid) && (UID_FIRST..=UID_LAST).contains(&uid) {
                        release(jaild, uid);
                        p.free.remove(&uid);
                        p.dying.push(uid);
                    }
                }
            }
        }
        std::thread::spawn(move || janitor(pool));
        Ok(j)
    }

    fn take(&self) -> io::Result<u32> {
        let mut p = self.pool.lock().unwrap();
        let uid = *p.free.iter().next().ok_or_else(|| io::Error::other("no free worker uid"))?;
        p.free.remove(&uid);
        Ok(uid)
    }

    fn give_back(&self, uid: u32) {
        release(&self.jaild, uid);
        self.pool.lock().unwrap().dying.push(uid);
    }

    fn create(&self, req: Request, stdio: [std::os::fd::BorrowedFd<'_>; 3]) -> io::Result<OwnedFd> {
        let mut c = jaild::client::Client::connect(&self.jaild).map_err(|e| io::Error::other(format!("connect {}: {e}", self.jaild)))?;
        let (resp, got) = c.send_with_fds(&req, &stdio, 1).map_err(|e| io::Error::other(format!("jaild: {e}")))?;
        // SAFETY: jaild handed these over; each is owned once.
        let mut got: Vec<OwnedFd> = got.into_iter().map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }).collect();
        match resp {
            Response::JailCreated(r) if r.procdesc_attached => got.pop().ok_or_else(|| io::Error::other("jaild sent no procdesc")),
            Response::PolicyDenied { rule, detail } => Err(io::Error::other(format!("jaild refused ({rule}): {detail}"))),
            Response::SyscallFailed { name, errno, msg } => Err(io::Error::other(format!("jaild: {name} (errno {errno}): {msg}"))),
            other => Err(io::Error::other(format!("jaild: unexpected {other:?}"))),
        }
    }

    /// Hand the jail back when the worker ends; a stopper that kills it.
    fn watch(&self, uid: u32, pd: OwnedFd) -> Stopper {
        let pd = Arc::new(pd);
        let w = pd.clone();
        let jaild = self.jaild.clone();
        let pool = self.pool.clone();
        std::thread::spawn(move || {
            let _ = wait(&w);
            release(&jaild, uid);
            pool.lock().unwrap().dying.push(uid);
        });
        Stopper::new(move || {
            // SAFETY: a procdesc we own; SIGKILL to the process it names.
            unsafe { libc::pdkill(pd.as_raw_fd(), libc::SIGKILL) };
        })
    }

    fn argv(exec: &str, args: &[String]) -> Vec<String> {
        let mut v = vec![exec.to_string()];
        v.extend(args.iter().cloned());
        v
    }
}

impl Launcher for Jailed {
    fn ipp(&self, printer: SocketAddr) -> io::Result<Proc> {
        let SocketAddr::V4(v4) = printer else {
            return Err(io::Error::other("an IPv6 printer can't be granted yet"));
        };
        let uid = self.take()?;
        let (ours, theirs) = UnixStream::pair()?;
        let grants = NetGrants {
            app_key: portcullis_identity::consent_key(&name(uid)),
            outbound_any: false,
            outbound: vec![NetDest { cidr: format!("{}/32", v4.ip()), port: Some(v4.port()), udp: false }],
            peers: vec![],
            inbound: vec![],
            no_resolver: true,
        };
        let mac = portcullis_identity::for_app(&format!("org.atrium.prelum.worker.{uid}")).map_err(|e| io::Error::other(e.to_string()))?.mac;
        let err = io::stderr();
        let r = self.start(uid, IPP_EXEC, &[], NetworkConfig::Routed { mac, grants: Some(grants) }, "networked", [theirs.as_fd(), theirs.as_fd(), err.as_fd()], None);
        drop(theirs);
        match r {
            Ok(pd) => Ok(Proc { sock: ours, stop: self.watch(uid, pd) }),
            Err(e) => {
                self.give_back(uid);
                Err(e)
            }
        }
    }

    fn render(&self, pdf: OwnedFd, args: &[String]) -> io::Result<Proc> {
        let uid = self.take()?;
        let (ours, theirs) = UnixStream::pair()?;
        let err = io::stderr();
        let rctl = self.rctl.then(|| RctlLimits { memoryuse_mb: Some(RENDER_MEMORY_MB), pcpu: Some(100) });
        let r = self.start(uid, RENDER_EXEC, args, NetworkConfig::Disable, "renderer", [pdf.as_fd(), theirs.as_fd(), err.as_fd()], rctl);
        drop(theirs);
        drop(pdf);
        match r {
            Ok(pd) => Ok(Proc { sock: ours, stop: self.watch(uid, pd) }),
            Err(e) => {
                self.give_back(uid);
                Err(e)
            }
        }
    }
}

impl Jailed {
    #[allow(clippy::too_many_arguments)]
    fn start(&self, uid: u32, exec: &str, args: &[String], network: NetworkConfig, profile: &str, stdio: [std::os::fd::BorrowedFd<'_>; 3], rctl: Option<RctlLimits>) -> io::Result<OwnedFd> {
        let root = root(uid);
        for d in ["lib", "libexec", "usr/lib", "usr/local/bin", "dev", "tmp"] {
            std::fs::create_dir_all(root.join(d))?;
        }
        let ro = |p: &str| MountSpec { source: p.into(), dest: p.into(), kind: MountKind::RoNullfs, size_mb: None, owner: None };
        let id = portcullis_identity::for_app(&format!("org.atrium.prelum.worker.{uid}")).map_err(|e| io::Error::other(format!("host identity: {e}")))?;
        let req = Request::CreateJail(CreateJailRequest {
            name: name(uid),
            path: root.to_string_lossy().into_owned(),
            children_max: 0,
            mounts: vec![ro("/lib"), ro("/libexec"), ro("/usr/lib"), ro("/usr/local/bin")],
            devfs_ruleset: DEVFS_RULESET,
            network,
            exec: Some(ExecSpec { path: exec.into(), argv: Self::argv(exec, args), env: Vec::new(), uid, gid: uid, stdio: true }),
            rctl,
            hostname: Some(name(uid)),
            hostid: Some(id.hostid),
            hostuuid: Some(id.hostuuid.clone()),
            allow_mlock: false,
            allow_tessera_keys: false,
            syscall_profile: Some(profile.into()),
            devfs_grant: None,
            devfs_grants: vec![],
            fuse: None,
        });
        self.create(req, stdio)
    }
}

/// Drop jaild's record of the jail (and its network).
fn release(jaild: &str, uid: u32) {
    if let Ok(mut c) = jaild::client::Client::connect(jaild) {
        let _ = c.send(&Request::RemoveJail { jid: None, name: Some(name(uid)) });
    }
}

fn jail_exists(name: &str) -> bool {
    std::process::Command::new("jls")
        .args(["-d", "-j", name, "jid"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// One attempt to take a worker's jail apart; true once nothing is left.
/// Never waits (a dying vnet jail stays while TIME_WAIT lasts).
fn try_teardown(uid: u32) -> bool {
    let (name, root) = (name(uid), root(uid));
    let _ = std::process::Command::new("jail").arg("-r").arg(&name).stderr(std::process::Stdio::null()).status();
    if jail_exists(&name) {
        return false;
    }
    if !root.exists() {
        return true;
    }
    if !portcullis_mounts::converge(&[root.as_path()], portcullis_mounts::Force::Yes).is_empty() {
        return false;
    }
    // ★ Never remove_dir_all: a surviving mount would take the removal
    // into /lib. Only the empty directories made here.
    for d in ["lib", "libexec", "usr/lib", "usr/local/bin", "usr/local", "usr", "dev", "tmp", ""] {
        let _ = std::fs::remove_dir(root.join(d));
    }
    !root.exists()
}

fn janitor(pool: Arc<Mutex<Pool>>) {
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let dying: Vec<u32> = std::mem::take(&mut pool.lock().unwrap().dying);
        let mut left = Vec::new();
        for uid in dying {
            if try_teardown(uid) {
                pool.lock().unwrap().free.insert(uid);
            } else {
                left.push(uid);
            }
        }
        pool.lock().unwrap().dying.extend(left);
    }
}

/// Block until the process behind `pd` exits.
fn wait(pd: &OwnedFd) -> io::Result<i32> {
    // SAFETY: kqueue returns a new descriptor or -1.
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: just created, owned here.
    let kq = unsafe { OwnedFd::from_raw_fd(kq) };
    // SAFETY: a zeroed kevent is a valid value to fill in.
    let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
    ev.ident = pd.as_raw_fd() as usize;
    ev.filter = libc::EVFILT_PROCDESC;
    ev.flags = libc::EV_ADD | libc::EV_ONESHOT;
    ev.fflags = libc::NOTE_EXIT;
    loop {
        // SAFETY: as above.
        let mut out: libc::kevent = unsafe { std::mem::zeroed() };
        // SAFETY: one change, one event, both valid.
        let n = unsafe { libc::kevent(kq.as_raw_fd(), &ev, 1, &mut out, 1, std::ptr::null()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if n == 1 && out.fflags & libc::NOTE_EXIT != 0 {
            return Ok(out.data as i32);
        }
    }
}
