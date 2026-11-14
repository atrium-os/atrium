//! The share server's dialer (network-shares.md §4.1, S4): a process forked
//! before anything else, outside capability mode, that does two things
//! when asked and nothing else: connect to the one address and port the
//! share was given, and connect to Arca's socket. Each comes back as a
//! descriptor over a socket pair. The process that parses the network
//! enters capability mode and can still reconnect: through this.
//!
//! The dialer keeps no descriptor of the share's (FUSE, control): it closes
//! them on the way in. It ends when the share server does (its socket
//! closes).

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

/// How long one connect may take. Short: a reconnect runs inside a FUSE
/// request, which the kernel gives up on after 30 s.
pub const CONNECT_WAIT: Duration = Duration::from_secs(6);

fn send_fd(s: &UnixStream, fd: Option<RawFd>) -> io::Result<()> {
    let mut byte = [u8::from(fd.is_some())];
    let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
    // SAFETY: a zeroed msghdr with one iovec and, when sending, one
    // SCM_RIGHTS control message sized by CMSG_SPACE.
    unsafe {
        let space = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as usize;
        let mut ctl = vec![0u8; space];
        let mut m: libc::msghdr = std::mem::zeroed();
        m.msg_iov = &mut iov;
        m.msg_iovlen = 1;
        if let Some(fd) = fd {
            m.msg_control = ctl.as_mut_ptr().cast();
            m.msg_controllen = space as _;
            let c = libc::CMSG_FIRSTHDR(&m);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<RawFd>(), fd);
        }
        if libc::sendmsg(s.as_raw_fd(), &m, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

fn recv_fd(s: &UnixStream) -> io::Result<Option<RawFd>> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
    // SAFETY: as in send_fd; the received descriptor is taken only from a
    // control message of the right level, type and length.
    unsafe {
        let space = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as usize;
        let mut ctl = vec![0u8; space];
        let mut m: libc::msghdr = std::mem::zeroed();
        m.msg_iov = &mut iov;
        m.msg_iovlen = 1;
        m.msg_control = ctl.as_mut_ptr().cast();
        m.msg_controllen = space as _;
        let n = libc::recvmsg(s.as_raw_fd(), &mut m, 0);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let c = libc::CMSG_FIRSTHDR(&m);
        if byte[0] == 1 && !c.is_null() && (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
            return Ok(Some(std::ptr::read_unaligned(libc::CMSG_DATA(c).cast::<RawFd>())));
        }
        Ok(None)
    }
}

/// The share server's end of the dialer.
pub struct Dialer(UnixStream);

impl Dialer {
    /// Fork the dialer for `to` and, if given, Arca's socket at `arca`.
    /// Call before any thread starts.
    pub fn start(to: SocketAddr, arca: Option<String>) -> io::Result<Dialer> {
        let (ours, theirs) = UnixStream::pair()?;
        // SAFETY: called before any thread starts; the child only connects
        // sockets and passes them back.
        match unsafe { libc::fork() } {
            -1 => Err(io::Error::last_os_error()),
            0 => {
                drop(ours);
                // Nothing of the share's (FUSE, control) stays open here.
                let keep = theirs.as_raw_fd();
                for fd in 3..1024 {
                    if fd != keep {
                        // SAFETY: closing descriptors this child doesn't use.
                        unsafe { libc::close(fd) };
                    }
                }
                let mut s = theirs;
                let mut b = [0u8; 1];
                while matches!(s.read(&mut b), Ok(1)) {
                    let r = match (b[0], &arca) {
                        (b'd', _) => TcpStream::connect_timeout(&to, CONNECT_WAIT).map(|t| {
                            let fd = t.as_raw_fd();
                            std::mem::forget(t);
                            fd
                        }),
                        (b'a', Some(p)) => UnixStream::connect(p).map(|u| {
                            let fd = u.as_raw_fd();
                            std::mem::forget(u);
                            fd
                        }),
                        _ => Err(io::ErrorKind::Unsupported.into()),
                    };
                    let _ = send_fd(&s, r.as_ref().ok().copied());
                    if let Ok(fd) = r {
                        // SAFETY: ours to close, now that it is passed.
                        unsafe { libc::close(fd) };
                    }
                }
                // SAFETY: leave without running the parent's atexit handlers.
                unsafe { libc::_exit(0) }
            }
            _ => Ok(Dialer(ours)),
        }
    }

    fn ask(&mut self, what: u8) -> io::Result<RawFd> {
        self.0.write_all(&[what])?;
        recv_fd(&self.0)?.ok_or_else(|| io::Error::new(io::ErrorKind::ConnectionRefused, "can't be reached"))
    }

    /// A new connection to the share's server.
    pub fn tcp(&mut self) -> io::Result<TcpStream> {
        // SAFETY: the dialer just passed us this connected socket.
        self.ask(b'd').map(|fd| unsafe { TcpStream::from_raw_fd(fd) })
    }

    /// A new connection to Arca.
    pub fn arca(&mut self) -> io::Result<UnixStream> {
        // SAFETY: the dialer just passed us this connected socket.
        self.ask(b'a').map(|fd| unsafe { UnixStream::from_raw_fd(fd) })
    }
}

/// When to try a lost server again: soon, then less often, so a server
/// that is gone doesn't make every access wait on a connect.
#[derive(Debug, Default)]
pub struct Backoff {
    fails: u32,
    next: Option<Instant>,
}

const STEPS: [u64; 5] = [2, 5, 10, 20, 30];

impl Backoff {
    pub fn ready(&self) -> bool {
        self.next.is_none_or(|t| Instant::now() >= t)
    }

    pub fn failed(&mut self) {
        let wait = STEPS[(self.fails as usize).min(STEPS.len() - 1)];
        self.fails += 1;
        self.next = Some(Instant::now() + Duration::from_secs(wait));
    }

    /// Failures since the last success.
    pub fn fails(&self) -> u32 {
        self.fails
    }

    pub fn ok(&mut self) {
        self.fails = 0;
        self.next = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_steps() {
        let mut b = Backoff::default();
        assert!(b.ready());
        b.failed();
        assert!(!b.ready());
        b.ok();
        assert!(b.ready());
    }

    #[test]
    fn the_dialer_connects_and_passes_back() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let to = l.local_addr().unwrap();
        let dir = std::env::temp_dir().join(format!("dialer-test-{}", std::process::id()));
        let _ = std::fs::remove_file(&dir);
        let ul = std::os::unix::net::UnixListener::bind(&dir).unwrap();
        let mut d = Dialer::start(to, Some(dir.to_string_lossy().into_owned())).unwrap();
        let mut t = d.tcp().unwrap();
        let (mut a, _) = l.accept().unwrap();
        t.write_all(b"hi").unwrap();
        let mut b = [0u8; 2];
        a.read_exact(&mut b).unwrap();
        assert_eq!(&b, b"hi");
        let _u = d.arca().unwrap();
        assert!(ul.accept().is_ok());
        drop(l);
        let _ = std::fs::remove_file(&dir);
    }
}
