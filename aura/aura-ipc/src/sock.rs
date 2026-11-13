//! Unix-socket plumbing shared by the daemons: a listener whose clients
//! are known by uid, each with a bounded line reader.

use crate::{Got, Line, Reader};
use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

/// The uid of the process at the other end of a connected Unix socket.
#[allow(unsafe_code)]
pub fn peer_uid(s: &UnixStream) -> Option<u32> {
    #[cfg(any(target_os = "freebsd", target_os = "macos"))]
    {
        let mut uid: libc::uid_t = 0;
        let mut gid: libc::gid_t = 0;
        // SAFETY: valid fd and out-pointers.
        let r = unsafe { libc::getpeereid(s.as_raw_fd(), &mut uid, &mut gid) };
        (r == 0).then_some(uid)
    }
    #[cfg(target_os = "linux")]
    {
        let mut c = libc::ucred { pid: 0, uid: 0, gid: 0 };
        let mut n = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: valid fd, buffer of the size given.
        let r = unsafe {
            libc::getsockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, &mut c as *mut _ as *mut libc::c_void, &mut n)
        };
        (r == 0).then_some(c.uid)
    }
}

pub struct Conn {
    pub s: UnixStream,
    pub uid: u32,
    r: Reader,
}

impl Conn {
    pub fn new(s: UnixStream) -> io::Result<Conn> {
        s.set_nonblocking(true)?;
        let uid = peer_uid(&s).ok_or_else(|| io::Error::other("no peer credentials"))?;
        Ok(Conn { s, uid, r: Reader::default() })
    }

    /// Complete lines received so far; `Err` when the peer is gone or broke
    /// the framing (the caller drops the connection).
    pub fn lines(&mut self) -> io::Result<Vec<Got>> {
        self.r.fill(&mut self.s)
    }

    pub fn send(&mut self, l: &Line) -> io::Result<()> {
        // Lines are small; a peer that stops reading until the socket
        // buffer fills is dropped rather than blocking the daemon.
        l.send(&mut self.s)
    }

    pub fn fd(&self) -> RawFd {
        self.s.as_raw_fd()
    }
}

/// Binds a listening socket, replacing a stale one. Peers are checked by
/// uid on accept; `mode` is the outer fence.
pub fn listen(path: &Path, mode: u32) -> io::Result<UnixListener> {
    let _ = std::fs::remove_file(path);
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let l = UnixListener::bind(path)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    l.set_nonblocking(true)?;
    Ok(l)
}

/// poll(2) over fds for reading, `ms` timeout; returns readable flags.
#[allow(unsafe_code)]
pub fn poll_read(fds: &[RawFd], ms: i32) -> Vec<bool> {
    let mut p: Vec<libc::pollfd> = fds.iter().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
    // SAFETY: p is a valid array of pollfds of the length given.
    unsafe { libc::poll(p.as_mut_ptr(), p.len() as _, ms) };
    p.iter().map(|x| x.revents != 0).collect()
}
