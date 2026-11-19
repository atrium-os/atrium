//! Outside the sandbox: a connector child, and the secrets helper's socket.
//!
//! Capability mode forbids `connect`, and an AnyConnect engine needs a new
//! TCP connection for every sign-in, tunnel and reconnect. So before the
//! engine enters capability mode it forks a child that does one thing:
//! on each byte it reads, it connects to the one gateway address cryptad
//! named and passes the socket back (SCM_RIGHTS). It keeps no other
//! descriptor; the jail's pf passes nothing but that address and port.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, SocketAddrV4, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use aura_ipc::Line;
use zeroize::Zeroizing;

const CMSG_SPACE: usize = 64;

fn send_fd(s: RawFd, byte: u8, fd: Option<RawFd>) -> io::Result<()> {
    let data = [byte];
    let mut iov = libc::iovec { iov_base: data.as_ptr() as *mut libc::c_void, iov_len: 1 };
    let mut cbuf = [0u64; CMSG_SPACE / 8];
    // SAFETY: a zeroed msghdr is valid; filled below with live buffers.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if let Some(f) = fd {
        let n = std::mem::size_of::<RawFd>() as u32;
        // SAFETY: the control buffer has room for one descriptor.
        unsafe {
            msg.msg_control = cbuf.as_mut_ptr().cast();
            msg.msg_controllen = libc::CMSG_SPACE(n) as _;
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(n) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(c) as *mut RawFd, f);
        }
    }
    // SAFETY: msg points at live buffers.
    if unsafe { libc::sendmsg(s, &msg, 0) } < 1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn recv_fd(s: RawFd) -> io::Result<(u8, Option<OwnedFd>)> {
    let mut data = [0u8; 1];
    let mut iov = libc::iovec { iov_base: data.as_mut_ptr().cast(), iov_len: 1 };
    let mut cbuf = [0u64; CMSG_SPACE / 8];
    // SAFETY: as in send_fd.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = CMSG_SPACE as _;
    // SAFETY: as above.
    let n = unsafe { libc::recvmsg(s, &mut msg, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    if n == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the connector is gone"));
    }
    let mut fd = None;
    // SAFETY: walking the control message the kernel filled.
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        if !c.is_null() && (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
            fd = Some(OwnedFd::from_raw_fd(std::ptr::read_unaligned(libc::CMSG_DATA(c) as *const RawFd)));
        }
    }
    Ok((data[0], fd))
}

pub struct Connector {
    s: UnixStream,
}

impl Connector {
    /// Fork the connector. Called before the engine's limits and sandbox;
    /// `keep` is the log descriptor the child may write to.
    pub fn spawn(gw: SocketAddrV4, keep: RawFd) -> io::Result<Connector> {
        let (ours, theirs) = UnixStream::pair()?;
        // SAFETY: the engine is single-threaded here; the child only calls
        // close, connect and sendmsg before it exits.
        match unsafe { libc::fork() } {
            -1 => Err(io::Error::last_os_error()),
            0 => {
                let me = theirs.as_raw_fd();
                for fd in 0..256 {
                    if fd != me && fd != keep {
                        // SAFETY: closing what the child does not use.
                        unsafe { libc::close(fd) };
                    }
                }
                let zero = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
                // SAFETY: setrlimit with a valid struct.
                unsafe {
                    libc::setrlimit(libc::RLIMIT_NPROC, &zero);
                    libc::setrlimit(libc::RLIMIT_CORE, &zero);
                }
                let mut s = theirs;
                let mut b = [0u8; 1];
                while let Ok(1) = s.read(&mut b) {
                    let r = match TcpStream::connect_timeout(&SocketAddr::V4(gw), Duration::from_secs(15)) {
                        Ok(t) => send_fd(me, b'k', Some(t.as_raw_fd())),
                        Err(_) => send_fd(me, b'e', None),
                    };
                    if r.is_err() {
                        break;
                    }
                }
                // SAFETY: leave without running the parent's destructors.
                unsafe { libc::_exit(0) }
            }
            _ => {
                drop(theirs);
                Ok(Connector { s: ours })
            }
        }
    }

    /// A new connection to the gateway.
    pub fn connect(&mut self) -> io::Result<TcpStream> {
        self.s.write_all(b"c")?;
        self.s.set_read_timeout(Some(Duration::from_secs(20)))?;
        match recv_fd(self.s.as_raw_fd())? {
            (b'k', Some(fd)) => {
                let t = TcpStream::from(fd);
                t.set_nodelay(true)?;
                Ok(t)
            }
            _ => Err(io::Error::new(io::ErrorKind::ConnectionRefused, "the gateway did not answer")),
        }
    }
}

/// The engine's secrets helper (fd 4): cryptad's key helper running for
/// this one profile, the only way passwords, tokens and the session cookie
/// reach the engine (vpn.md §9.3). One line out, one line back.
#[derive(Debug)]
pub struct Secrets {
    s: UnixStream,
    buf: Vec<u8>,
}

impl Secrets {
    pub fn new(s: UnixStream) -> io::Result<Secrets> {
        s.set_read_timeout(Some(Duration::from_secs(15)))?;
        Ok(Secrets { s, buf: Vec::new() })
    }

    pub fn call(&mut self, l: &Line) -> io::Result<Line> {
        l.send(&mut self.s)?;
        loop {
            if let Some(p) = self.buf.iter().position(|&b| b == b'\n') {
                let raw = Zeroizing::new(self.buf.drain(..=p).collect::<Vec<u8>>());
                let r = std::str::from_utf8(&raw[..p]).ok().and_then(Line::parse);
                return r.ok_or_else(|| io::Error::other("the secrets helper said nonsense"));
            }
            if self.buf.len() > aura_ipc::MAX_LINE * 2 {
                return Err(io::Error::other("the secrets helper's line is too long"));
            }
            let mut b = Zeroizing::new([0u8; 4096]);
            let n = self.s.read(&mut b[..])?;
            if n == 0 {
                return Err(io::Error::other("the secrets helper is gone"));
            }
            self.buf.extend_from_slice(&b[..n]);
        }
    }

    /// A value the helper hands over (`ok value=HEX`), as UTF-8.
    pub fn value(&mut self, l: &Line) -> Result<Zeroizing<String>, String> {
        let a = self.call(l).map_err(|e| e.to_string())?;
        if a.verb != "ok" {
            return Err(format!("Arca refused ({})", a.get("status").unwrap_or("?")));
        }
        let v = Zeroizing::new(a.hex("value").ok_or("no value")?);
        String::from_utf8(v.to_vec()).map(Zeroizing::new).map_err(|_| "not text".to_string())
    }

    pub fn ok(&mut self, l: &Line) -> bool {
        self.call(l).is_ok_and(|a| a.verb == "ok")
    }
}
