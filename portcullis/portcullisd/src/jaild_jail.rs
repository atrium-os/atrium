//! Jails portcullisd makes for a person (apps, workspaces, consoles), made by
//! jaild from a typed request (app-jails-via-jaild.md §4). portcullisd never
//! creates, enters or removes a jail itself (no jail(8), `jail_set` or
//! `jail_attach`): it asks jaild, and holds the process descriptor jaild
//! hands back. It may still read `jls` to see what runs.
//!
//! Unsafe code here is limited to taking ownership of received descriptors
//! and the kqueue calls that wait on a process descriptor.
#![allow(unsafe_code)]

use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

use jaild::client::Client;
use jaild::protocol::{CreateJailRequest, ExecInJailRequest, Request, Response};

pub const JAILD_SOCK: &str = "/var/run/atrium/jaild.sock";

fn connect() -> Result<Client, String> {
    Client::connect(JAILD_SOCK).map_err(|e| format!("connect {JAILD_SOCK}: {e}"))
}

fn owned(fds: Vec<i32>) -> Vec<OwnedFd> {
    // SAFETY: each descriptor was just received over SCM_RIGHTS by the
    // client and is held nowhere else.
    fds.into_iter().map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }).collect()
}

fn refusal(r: Response) -> String {
    match r {
        Response::PolicyDenied { rule, detail } => format!("jaild refused ({rule}): {detail}"),
        Response::SyscallFailed { name, errno, msg } => format!("jaild: {name} failed (errno {errno}): {msg}"),
        Response::Error { detail } => format!("jaild: {detail}"),
        other => format!("jaild: unexpected reply {other:?}"),
    }
}

/// A process jaild started in a jail, by its process descriptor. Dropping
/// it kills the process (a procdesc's last close does), so the holder
/// decides how long it lives.
pub struct Running {
    pub pid:  i32,
    procdesc: OwnedFd,
    kq:       OwnedFd,
    /// The exit, once seen: the kqueue reports it once (`EV_ONESHOT`).
    exited:   std::cell::Cell<Option<i32>>,
}

impl Running {
    fn new(pid: i32, procdesc: OwnedFd) -> io::Result<Running> {
        let kq = kqueue()?;
        watch_exit(&kq, &procdesc)?;
        Ok(Running { pid, procdesc, kq, exited: std::cell::Cell::new(None) })
    }

    /// Wait up to `timeout` (forever with `None`) for the exit; its wait(2)
    /// status, or `None` if it still runs. Asked again after the exit, the
    /// same status.
    pub fn wait_for(&self, timeout: Option<Duration>) -> io::Result<Option<i32>> {
        if let Some(s) = self.exited.get() {
            return Ok(Some(s));
        }
        let s = next_exit(&self.kq, timeout)?;
        self.exited.set(s);
        Ok(s)
    }

    pub fn wait(&self) -> io::Result<i32> {
        loop {
            if let Some(s) = self.wait_for(None)? {
                return Ok(s);
            }
        }
    }

    pub fn procdesc(&self) -> std::os::fd::BorrowedFd<'_> {
        self.procdesc.as_fd()
    }

    /// The process descriptor alone, for a holder that waits its own way
    /// (stoad, a poll thread). Its last close still kills the process.
    pub fn into_procdesc(self) -> OwnedFd {
        self.procdesc
    }
}

/// An exit status as `ExitStatus::code` reads it: the code of a normal exit,
/// `None` for a signal.
pub fn exit_code(status: i32) -> Option<i32> {
    libc::WIFEXITED(status).then(|| libc::WEXITSTATUS(status))
}

/// Create `req`'s jail. With an exec, its entry runs on `stdio` (this
/// process's own when `None`), and the process comes back; an empty jail
/// returns `None`.
///
/// Our copies of the caller's descriptors are dropped as soon as the request
/// is sent: a reader of the entry's stdout would otherwise wait on us for
/// its EOF.
pub fn create(req: CreateJailRequest, stdio: Option<[OwnedFd; 3]>) -> Result<Option<Running>, String> {
    create_with(req, stdio, Vec::new())
}

/// [`create`], passing `extra` on to the entry at fd 3, 4, ... (jaild
/// takes them only with the three stdio).
pub fn create_with(req: CreateJailRequest, stdio: Option<[OwnedFd; 3]>, extra: Vec<OwnedFd>) -> Result<Option<Running>, String> {
    let has_exec = req.exec.is_some();
    if !extra.is_empty() && (stdio.is_none() || !has_exec) {
        return Err("descriptors for the entry need its stdio".into());
    }
    let mut client = connect()?;
    let req = Request::CreateJail(req);
    let sent = match (&stdio, has_exec) {
        (Some([i, o, e]), true) => {
            let mut fds = vec![i.as_fd(), o.as_fd(), e.as_fd()];
            fds.extend(extra.iter().map(|f| f.as_fd()));
            client.send_with_fds(&req, &fds, 1)
        }
        (None, true) => client.send_with_fds(&req, &[io::stdin().as_fd(), io::stdout().as_fd(), io::stderr().as_fd()], 1),
        (_, false) => client.send_recv_fds(&req, 1),
    };
    drop(stdio);
    drop(extra);
    let (resp, got) = sent.map_err(|e| format!("jaild: {e}"))?;
    let mut got = owned(got);
    match resp {
        Response::JailCreated(r) if has_exec => {
            let pd = got.pop().filter(|_| r.procdesc_attached)
                .ok_or_else(|| "jaild started the entry and sent no process descriptor".to_string())?;
            Running::new(r.pid, pd).map(Some).map_err(|e| format!("watch the entry: {e}"))
        }
        Response::JailCreated(_) => Ok(None),
        other => Err(refusal(other)),
    }
}

/// End a jail and drop jaild's record of it (its devfs, rctl rules and
/// network go with it). A jail that is already gone is not an error.
pub fn remove(name: &str) -> Result<(), String> {
    let mut client = connect()?;
    match client.send(&Request::RemoveJail { jid: None, name: Some(name.into()) }) {
        Ok((Response::Ok, _)) => Ok(()),
        Ok((other, fd)) => {
            drop(fd.map(|f| owned(vec![f])));
            Err(refusal(other))
        }
        Err(e) => Err(format!("jaild: {e}")),
    }
}

/// Replace a running jail's `listen` rules (background-apps.md §4.3).
pub fn set_listen(name: &str, listen: Vec<jaild::protocol::NetListen>) -> Result<(), String> {
    let mut client = connect()?;
    match client.send(&Request::SetListen { jail_name: name.into(), listen }) {
        Ok((Response::Ok, _)) => Ok(()),
        Ok((other, fd)) => {
            drop(fd.map(|f| owned(vec![f])));
            Err(refusal(other))
        }
        Err(e) => Err(format!("jaild: {e}")),
    }
}

/// The live jid of a jail jaild made, or `None` when it is not running (or
/// jaild does not know it).
pub fn jid(name: &str) -> Option<i32> {
    let mut client = connect().ok()?;
    match client.send(&Request::JailInfo { name: name.into() }).ok()? {
        (Response::JailInfo { jid, .. }, _) => jid,
        _ => None,
    }
}

/// A jail jaild made: its resident memory and smallest memory cap, in
/// bytes, each `None` where jaild or racct can't say.
pub fn memory(name: &str) -> (Option<u64>, Option<u64>) {
    let Ok(mut client) = connect() else { return (None, None) };
    match client.send(&Request::JailInfo { name: name.into() }) {
        Ok((Response::JailInfo { memory, memory_cap, .. }, _)) => (memory, memory_cap),
        _ => (None, None),
    }
}

/// Run a process in a jail jaild made, as `req` says: on a pty (the master
/// comes back) or on `stdio`.
pub fn exec_in(req: ExecInJailRequest, stdio: Option<[OwnedFd; 3]>) -> Result<(Running, Option<OwnedFd>), String> {
    let mut client = connect()?;
    let r = Request::ExecInJail(req);
    let sent = match &stdio {
        Some([i, o, e]) => client.send_with_fds(&r, &[i.as_fd(), o.as_fd(), e.as_fd()], 2),
        None => client.send_recv_fds(&r, 2),
    };
    drop(stdio);
    let (resp, got) = sent.map_err(|e| format!("jaild: {e}"))?;
    let mut got = owned(got).into_iter();
    match resp {
        Response::JailExecStarted { pid, .. } => {
            let pd = got.next().ok_or_else(|| "jaild sent no process descriptor".to_string())?;
            let master = got.next();
            let run = Running::new(pid, pd).map_err(|e| format!("watch the process: {e}"))?;
            Ok((run, master))
        }
        other => Err(refusal(other)),
    }
}

/// A socket listening on 127.0.0.1:`port` (0: any) in a jail's own network
/// stack (a workspace's, or an app's under the debugger).
pub fn listen_in(name: &str, port: u16) -> Result<OwnedFd, String> {
    let mut client = connect()?;
    let (resp, got) = client.send_recv_fds(&Request::ListenInJail { name: name.into(), port }, 1)
        .map_err(|e| format!("jaild: {e}"))?;
    let mut got = owned(got);
    match resp {
        Response::Ok => got.pop().ok_or_else(|| "jaild sent no socket".to_string()),
        other => Err(refusal(other)),
    }
}

#[cfg(target_os = "freebsd")]
fn kqueue() -> io::Result<OwnedFd> {
    // SAFETY: kqueue returns a new descriptor or -1.
    let kq = unsafe { libc::kqueue() };
    if kq < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor, ours alone.
    Ok(unsafe { OwnedFd::from_raw_fd(kq) })
}

/// Register for the process's exit. A process that has already exited
/// reports at once (sys_procdesc.c, the test after registration).
#[cfg(target_os = "freebsd")]
fn watch_exit(kq: &OwnedFd, pd: &OwnedFd) -> io::Result<()> {
    // SAFETY: an all-zero kevent is valid; the fields used are set below.
    let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
    ev.ident = pd.as_raw_fd() as libc::uintptr_t;
    ev.filter = libc::EVFILT_PROCDESC;
    ev.flags = libc::EV_ADD | libc::EV_ONESHOT;
    ev.fflags = libc::NOTE_EXIT;
    // SAFETY: one change, no events out, on descriptors we hold.
    let rc = unsafe { libc::kevent(kq.as_raw_fd(), &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
    if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(target_os = "freebsd")]
fn next_exit(kq: &OwnedFd, timeout: Option<Duration>) -> io::Result<Option<i32>> {
    let ts = timeout.map(|d| libc::timespec {
        tv_sec: d.as_secs().min(i64::MAX as u64) as libc::time_t,
        tv_nsec: d.subsec_nanos() as libc::c_long,
    });
    let tsp = ts.as_ref().map_or(std::ptr::null(), |t| t as *const libc::timespec);
    loop {
        // SAFETY: as in watch_exit; one event out into `out`.
        let mut out: libc::kevent = unsafe { std::mem::zeroed() };
        let n = unsafe { libc::kevent(kq.as_raw_fd(), std::ptr::null(), 0, &mut out, 1, tsp) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted { continue }
            return Err(e);
        }
        if n == 0 {
            return Ok(None);
        }
        if out.fflags & libc::NOTE_EXIT != 0 {
            return Ok(Some(out.data as i32));
        }
    }
}

#[cfg(not(target_os = "freebsd"))]
fn kqueue() -> io::Result<OwnedFd> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "kqueue: FreeBSD only"))
}
#[cfg(not(target_os = "freebsd"))]
fn watch_exit(_kq: &OwnedFd, _pd: &OwnedFd) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "procdesc: FreeBSD only"))
}
#[cfg(not(target_os = "freebsd"))]
fn next_exit(_kq: &OwnedFd, _t: Option<Duration>) -> io::Result<Option<i32>> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "procdesc: FreeBSD only"))
}
