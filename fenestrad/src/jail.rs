//! Sessions in their own jails through jaild (fenestra.md §3.2, §11.3;
//! `[fenestra_sessions]` in its policy). One jail per session, named
//! `system-fenestra-<uid>` at `/var/lib/atrium/jails/<name>`, running
//! `atrium-fenestra-session` as a uid from fenestrad's pool, with
//! fenestrad's link as its stdin and stdout and the mint socket as fd 3.
//! Its network is its own vnet (`Routed`) whose one grant is a listen port
//! from the pool, UDP and the same port over TCP (the UDP-blocked
//! fallback), from the local network fenestrad was told of (`--lan IFACE
//! SUBNET`); nothing outbound, no resolver.
//!
//! The root holds the base libraries and the installed binaries, read-only,
//! and nothing else. A session's jail is removed when it exits; a janitor
//! retries what can't be removed at once (a vnet jail lingers while its
//! sockets drain) and only then returns its uid and port to the pool.
//!
//! The teardown follows prelumd's workers (prelum/prelumd/src/jail.rs).

use std::collections::BTreeSet;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use jaild::protocol::{CreateJailRequest, ExecSpec, ListenFrom, MountKind, MountSpec, NetGrants, NetListen, NetworkConfig, Request, Response};

pub const JAILS_DIR: &str = "/var/lib/atrium/jails";
/// jaild's `[fenestra_sessions]`: these must match its policy.
pub const NAME_PREFIX: &str = "system-fenestra-";
pub const EXEC: &str = "/usr/local/bin/atrium-fenestra-session";
pub const UID_FIRST: u32 = 60500;
pub const UID_LAST: u32 = 60599;
pub const PORT_FIRST: u16 = 24300;
pub const PORT_LAST: u16 = 24315;
/// The app devfs ruleset (portcullis_jail::APP_DEVFS_RULESET).
const DEVFS_RULESET: u32 = 22;
const DIRS: [&str; 6] = ["lib", "libexec", "usr/lib", "usr/local/bin", "dev", "tmp"];

pub fn name(uid: u32) -> String {
    format!("{NAME_PREFIX}{uid}")
}

fn root(uid: u32) -> PathBuf {
    Path::new(JAILS_DIR).join(name(uid))
}

struct Pool {
    uids: BTreeSet<u32>,
    ports: BTreeSet<u16>,
    /// Exited, not yet torn down: (uid, port).
    dying: Vec<(u32, u16)>,
}

pub struct Jails {
    jaild: String,
    iface: String,
    subnet: String,
    pool: Arc<Mutex<Pool>>,
}

/// A running session's jail: its port, and a way to end it.
pub struct Running {
    pub port: u16,
    pd: Arc<OwnedFd>,
    exited: Arc<std::sync::atomic::AtomicBool>,
}

impl Running {
    /// Whether its process is still running.
    pub fn alive(&self) -> bool {
        !self.exited.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn kill(&self) {
        // SAFETY: a procdesc we own; SIGKILL to the process it names.
        unsafe { libc::pdkill(self.pd.as_raw_fd(), libc::SIGKILL) };
    }
}

impl Jails {
    pub fn new(jaild: &str, iface: &str, subnet: &str) -> Jails {
        let pool = Arc::new(Mutex::new(Pool {
            uids: (UID_FIRST..=UID_LAST).collect(),
            ports: (PORT_FIRST..=PORT_LAST).collect(),
            dying: Vec::new(),
        }));
        // An earlier fenestrad's jails go first.
        if let Ok(rd) = std::fs::read_dir(JAILS_DIR) {
            let mut p = pool.lock().unwrap_or_else(|e| e.into_inner());
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if let Some(uid) = n.strip_prefix(NAME_PREFIX).and_then(|u| u.parse::<u32>().ok()) {
                    if n == name(uid) && (UID_FIRST..=UID_LAST).contains(&uid) {
                        release(jaild, uid);
                        p.uids.remove(&uid);
                        // Its port was not recorded; it stays free.
                        p.dying.push((uid, 0));
                    }
                }
            }
        }
        let j = Jails { jaild: jaild.into(), iface: iface.into(), subnet: subnet.into(), pool: pool.clone() };
        std::thread::spawn(move || janitor(pool));
        j
    }

    /// Start a session in a new jail: `link` as its stdin and stdout,
    /// `mint` as fd 3, `args` after the binary (fenestrad adds `--bind` with
    /// the port it took).
    pub fn start(&self, link: BorrowedFd<'_>, mint: BorrowedFd<'_>, args: &[String]) -> io::Result<Running> {
        let (uid, port) = {
            let mut p = self.pool.lock().unwrap_or_else(|e| e.into_inner());
            let uid = *p.uids.iter().next().ok_or_else(|| io::Error::other("no free session uid"))?;
            let port = *p.ports.iter().next().ok_or_else(|| io::Error::other("no free session port"))?;
            p.uids.remove(&uid);
            p.ports.remove(&port);
            (uid, port)
        };
        match self.create(uid, port, link, mint, args) {
            Ok(pd) => {
                let pd = Arc::new(pd);
                let exited = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let (w, jaild, pool, ex) = (pd.clone(), self.jaild.clone(), self.pool.clone(), exited.clone());
                std::thread::spawn(move || {
                    let _ = wait(&w);
                    ex.store(true, std::sync::atomic::Ordering::Relaxed);
                    release(&jaild, uid);
                    pool.lock().unwrap_or_else(|e| e.into_inner()).dying.push((uid, port));
                });
                Ok(Running { port, pd, exited })
            }
            Err(e) => {
                release(&self.jaild, uid);
                self.pool.lock().unwrap_or_else(|e| e.into_inner()).dying.push((uid, port));
                Err(e)
            }
        }
    }

    fn create(&self, uid: u32, port: u16, link: BorrowedFd<'_>, mint: BorrowedFd<'_>, args: &[String]) -> io::Result<OwnedFd> {
        let root = root(uid);
        for d in DIRS {
            std::fs::create_dir_all(root.join(d))?;
        }
        let ro = |p: &str| MountSpec { noexec: false, source: p.into(), dest: p.into(), kind: MountKind::RoNullfs, size_mb: None, owner: None };
        let app = format!("org.atrium.fenestra.session.{uid}");
        let id = portcullis_identity::for_app(&app).map_err(|e| io::Error::other(format!("host identity: {e}")))?;
        let grants = NetGrants {
            app_key: portcullis_identity::consent_key(&name(uid)),
            outbound_any: false,
            outbound: vec![],
            peers: vec![],
            inbound: vec![],
            no_resolver: true,
            tag: None,
            // UDP, and the same port over TCP for networks that block UDP.
            listen: [true, false]
                .map(|udp| NetListen { port, udp, from: ListenFrom::Lan { iface: self.iface.clone(), subnet: self.subnet.clone() } })
                .to_vec(),
        };
        let mut argv = vec![EXEC.to_string()];
        argv.extend(args.iter().cloned());
        argv.extend(["--bind".to_string(), format!("0.0.0.0:{port}")]);
        let req = Request::CreateJail(CreateJailRequest {
            class: None,
            devices: vec![],
            name: name(uid),
            path: root.to_string_lossy().into_owned(),
            children_max: 0,
            mounts: vec![ro("/lib"), ro("/libexec"), ro("/usr/lib"), ro("/usr/local/bin")],
            devfs_ruleset: DEVFS_RULESET,
            network: NetworkConfig::Routed { mac: id.mac.clone(), grants: Some(grants), fib: None },
            exec: Some(ExecSpec { cwd: None, groups: vec![], path: EXEC.into(), argv, env: Vec::new(), uid, gid: uid, stdio: true, listen: Vec::new(), ready: false }),
            rctl: None,
            hostname: Some(name(uid)),
            hostid: Some(id.hostid),
            hostuuid: Some(id.hostuuid.clone()),
            allow_mlock: false,
            allow_tessera_keys: false,
            syscall_profile: Some("networked".into()),
            devfs_grant: None,
            devfs_grants: vec![],
            fuse: None,
        });
        let err = io::stderr();
        let fds = [link, link, err.as_fd(), mint];
        let mut c = jaild::client::Client::connect(&self.jaild).map_err(|e| io::Error::other(format!("connect {}: {e}", self.jaild)))?;
        let (resp, got) = c.send_with_fds(&req, &fds, 1).map_err(|e| io::Error::other(format!("jaild: {e}")))?;
        // SAFETY: jaild handed these over; each is owned once.
        let mut got: Vec<OwnedFd> = got.into_iter().map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }).collect();
        match resp {
            Response::JailCreated(r) if r.procdesc_attached => got.pop().ok_or_else(|| io::Error::other("jaild sent no procdesc")),
            Response::PolicyDenied { rule, detail } => Err(io::Error::other(format!("jaild refused ({rule}): {detail}"))),
            Response::SyscallFailed { name, errno, msg } => Err(io::Error::other(format!("jaild: {name} (errno {errno}): {msg}"))),
            other => Err(io::Error::other(format!("jaild: unexpected {other:?}"))),
        }
    }
}

/// Drop jaild's record of the jail (and its network and listen rules).
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

/// One attempt to take a session's jail apart; true once nothing is left.
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
    // Never remove_dir_all: a surviving mount would take the removal into
    // /lib. Only the empty directories made here.
    for d in ["lib", "libexec", "usr/lib", "usr/local/bin", "usr/local", "usr", "dev", "tmp", ""] {
        let _ = std::fs::remove_dir(root.join(d));
    }
    !root.exists()
}

fn janitor(pool: Arc<Mutex<Pool>>) {
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let dying: Vec<(u32, u16)> = std::mem::take(&mut pool.lock().unwrap_or_else(|e| e.into_inner()).dying);
        let mut left = Vec::new();
        for (uid, port) in dying {
            if try_teardown(uid) {
                let mut p = pool.lock().unwrap_or_else(|e| e.into_inner());
                p.uids.insert(uid);
                if port != 0 {
                    p.ports.insert(port);
                }
            } else {
                left.push((uid, port));
            }
        }
        pool.lock().unwrap_or_else(|e| e.into_inner()).dying.extend(left);
    }
}

/// Block until the process behind `pd` exits.
fn wait(pd: &OwnedFd) -> io::Result<()> {
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
            return Ok(());
        }
    }
}
