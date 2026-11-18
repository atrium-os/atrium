//! A small spawner, forked before atrium-net enters capability mode, so
//! that a sandboxed helper (atrium-lease, atrium-probe) that dies can be
//! started again afterwards: capability mode forbids exec, and before this
//! a dead lease process made atrium-net exit (spec §10.1 known gap).
//!
//! The spawner keeps no state and reads one byte per request: which of the
//! programs fixed at fork time to start. It answers with one byte and, on
//! success, the parent's end of a fresh socketpair (SCM_RIGHTS); the
//! program gets the other end as fd 3 and nothing else of ours. Children
//! are reaped by the kernel (SIGCHLD ignored). The spawner exits when
//! atrium-net's end of its socket closes.
//!
//! It also makes printer discovery's two mDNS sockets (`b'M'` and the
//! interface's IPv4 address; printing.md §7.1): capability mode forbids
//! bind, and the worker that uses them may not send anywhere else.

use std::ffi::CString;
use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

pub struct Spawner {
    s: UnixStream,
    pid: libc::pid_t,
}

struct Prog {
    kind: u8,
    path: CString,
    argv: Vec<CString>,
}

fn close_from(n: RawFd) {
    #[cfg(target_os = "freebsd")]
    unsafe {
        libc::closefrom(n);
    }
    #[cfg(not(target_os = "freebsd"))]
    for fd in n..1024 {
        unsafe {
            libc::close(fd);
        }
    }
}

impl Spawner {
    /// Fork the spawner. `progs`: (request byte, program path, extra args).
    /// Call while the process is still single-threaded.
    pub fn start(progs: &[(u8, &str, &[&str])]) -> io::Result<Spawner> {
        let progs: Vec<Prog> = progs
            .iter()
            .map(|(kind, path, args)| {
                let path = CString::new(*path).map_err(|_| io::ErrorKind::InvalidInput)?;
                let mut argv = vec![path.clone()];
                for a in *args {
                    argv.push(CString::new(*a).map_err(|_| io::ErrorKind::InvalidInput)?);
                }
                Ok(Prog { kind: *kind, path, argv })
            })
            .collect::<io::Result<_>>()?;
        let (ours, theirs) = UnixStream::pair()?;
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            return Err(io::Error::last_os_error());
        }
        if pid == 0 {
            // The spawner: its socket as fd 3, stdio kept for messages.
            let fd = theirs.into_raw_fd();
            unsafe {
                if fd != 3 && libc::dup2(fd, 3) < 0 {
                    libc::_exit(1);
                }
                libc::signal(libc::SIGCHLD, libc::SIG_IGN);
            }
            close_from(4);
            serve(3, &progs);
        }
        drop(theirs);
        Ok(Spawner { s: ours, pid })
    }

    pub fn pid(&self) -> libc::pid_t {
        self.pid
    }

    /// Start program `kind`; our end of its socket.
    pub fn spawn(&self, kind: u8) -> io::Result<UnixStream> {
        assert!(kind != b'M');
        let mut v = self.ask(&[kind], 1, "spawner could not start it")?;
        Ok(UnixStream::from(v.remove(0)))
    }

    /// Printer discovery's sockets on the interface with IPv4 address
    /// `addr`: (receive, send). Both bound to port 5353 (SO_REUSEPORT).
    /// Receive: joined to 224.0.0.251 there, its sending side shut down.
    /// Send: multicast out of that interface, TTL 255, no loopback,
    /// connected to 224.0.0.251:5353 so it reaches nothing else.
    pub fn mdns4(&self, addr: Ipv4Addr) -> io::Result<(OwnedFd, OwnedFd)> {
        let mut req = vec![b'M'];
        req.extend_from_slice(&addr.octets());
        let mut v = self.ask(&req, 2, "spawner could not make the mDNS sockets")?;
        let send = v.pop().unwrap();
        Ok((v.pop().unwrap(), send))
    }

    fn ask(&self, req: &[u8], want: usize, why: &str) -> io::Result<Vec<OwnedFd>> {
        let fd = self.s.as_raw_fd();
        if unsafe { libc::write(fd, req.as_ptr().cast(), req.len()) } != req.len() as isize {
            return Err(io::Error::last_os_error());
        }
        let (b, got) = recv_fds(fd)?;
        if b == b'K' && got.len() == want {
            Ok(got)
        } else {
            Err(io::Error::other(why))
        }
    }
}

fn read_exact(s: RawFd, buf: &mut [u8]) -> bool {
    let mut n = 0;
    while n < buf.len() {
        let r = unsafe { libc::read(s, buf[n..].as_mut_ptr().cast(), buf.len() - n) };
        if r > 0 {
            n += r as usize;
        } else if !(r < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted) {
            return false;
        }
    }
    true
}

fn sockopt<T>(s: RawFd, level: libc::c_int, name: libc::c_int, v: &T) -> io::Result<()> {
    if unsafe { libc::setsockopt(s, level, name, (v as *const T).cast(), std::mem::size_of::<T>() as libc::socklen_t) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn sin(a: [u8; 4], port: u16) -> libc::sockaddr_in {
    let mut x: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    #[cfg(target_os = "freebsd")]
    {
        x.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
    }
    x.sin_family = libc::AF_INET as _;
    x.sin_port = port.to_be();
    x.sin_addr = libc::in_addr { s_addr: u32::from_ne_bytes(a) };
    x
}

const MDNS_GROUP: [u8; 4] = [224, 0, 0, 251];
const MDNS_PORT: u16 = 5353;

/// One of the two mDNS sockets (in the spawner, before any capability mode).
fn mdns_socket(ifaddr: [u8; 4], send: bool) -> io::Result<OwnedFd> {
    let s = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if s < 0 {
        return Err(io::Error::last_os_error());
    }
    let s = unsafe { OwnedFd::from_raw_fd(s) };
    let f = s.as_raw_fd();
    let one: libc::c_int = 1;
    sockopt(f, libc::SOL_SOCKET, libc::SO_REUSEADDR, &one)?;
    sockopt(f, libc::SOL_SOCKET, libc::SO_REUSEPORT, &one)?;
    let any = sin([0; 4], MDNS_PORT);
    if unsafe { libc::bind(f, (&any as *const libc::sockaddr_in).cast(), std::mem::size_of::<libc::sockaddr_in>() as _) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let ifa = libc::in_addr { s_addr: u32::from_ne_bytes(ifaddr) };
    if send {
        sockopt(f, libc::IPPROTO_IP, libc::IP_MULTICAST_IF, &ifa)?;
        let ttl: u8 = 255;
        sockopt(f, libc::IPPROTO_IP, libc::IP_MULTICAST_TTL, &ttl)?;
        let off: u8 = 0;
        sockopt(f, libc::IPPROTO_IP, libc::IP_MULTICAST_LOOP, &off)?;
        let g = sin(MDNS_GROUP, MDNS_PORT);
        if unsafe { libc::connect(f, (&g as *const libc::sockaddr_in).cast(), std::mem::size_of::<libc::sockaddr_in>() as _) } != 0 {
            return Err(io::Error::last_os_error());
        }
    } else {
        let m = libc::ip_mreq { imr_multiaddr: libc::in_addr { s_addr: u32::from_ne_bytes(MDNS_GROUP) }, imr_interface: ifa };
        sockopt(f, libc::IPPROTO_IP, libc::IP_ADD_MEMBERSHIP, &m)?;
        // FreeBSD shuts an unconnected datagram socket's sending side and
        // still says ENOTCONN; Linux only says ENOTCONN (host tests).
        if unsafe { libc::shutdown(f, libc::SHUT_WR) } != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::ENOTCONN) {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(s)
}

fn serve(s: RawFd, progs: &[Prog]) -> ! {
    loop {
        let mut k = 0u8;
        let n = unsafe { libc::read(s, (&mut k as *mut u8).cast(), 1) };
        if n == 0 || (n < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted) {
            unsafe { libc::_exit(0) };
        }
        if n != 1 {
            continue;
        }
        if k == b'M' {
            let mut a = [0u8; 4];
            if !read_exact(s, &mut a) {
                unsafe { libc::_exit(0) };
            }
            match (mdns_socket(a, false), mdns_socket(a, true)) {
                (Ok(r), Ok(w)) => {
                    let _ = send_fds(s, b'K', &[r.as_raw_fd(), w.as_raw_fd()]);
                }
                _ => {
                    let _ = send_fd(s, b'E', None);
                }
            }
            continue;
        }
        let Some(p) = progs.iter().find(|p| p.kind == k) else {
            let _ = send_fd(s, b'E', None);
            continue;
        };
        let mut sv = [0 as RawFd; 2];
        if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sv.as_mut_ptr()) } != 0 {
            let _ = send_fd(s, b'E', None);
            continue;
        }
        let mut argv: Vec<*const libc::c_char> = p.argv.iter().map(|a| a.as_ptr()).collect();
        argv.push(std::ptr::null());
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            unsafe {
                // Default SIGCHLD for the program; its socket as fd 3.
                libc::signal(libc::SIGCHLD, libc::SIG_DFL);
                if libc::dup2(sv[1], 3) < 0 {
                    libc::_exit(127);
                }
            }
            close_from(4);
            unsafe {
                libc::execv(p.path.as_ptr(), argv.as_ptr());
                libc::_exit(127);
            }
        }
        unsafe { libc::close(sv[1]) };
        if pid < 0 {
            let _ = send_fd(s, b'E', None);
        } else {
            let _ = send_fd(s, b'K', Some(sv[0]));
        }
        unsafe { libc::close(sv[0]) };
    }
}

const CMSG_SPACE: usize = 48;

fn send_fd(s: RawFd, b: u8, fd: Option<RawFd>) -> io::Result<()> {
    match fd {
        Some(f) => send_fds(s, b, &[f]),
        None => send_fds(s, b, &[]),
    }
}

fn send_fds(s: RawFd, b: u8, fds: &[RawFd]) -> io::Result<()> {
    if send_bytes_fds(s, &[b], fds)? != 1 {
        return Err(io::ErrorKind::WriteZero.into());
    }
    Ok(())
}

/// `data` with `fds` (at most 4) attached to its first byte; how much of
/// `data` went out. Also used for control-socket replies with descriptors.
pub fn send_bytes_fds(s: RawFd, data: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    assert!(fds.len() <= 4 && !data.is_empty());
    let mut iov = libc::iovec { iov_base: data.as_ptr() as *mut libc::c_void, iov_len: data.len() };
    let mut cbuf = [0u64; CMSG_SPACE / 8];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if !fds.is_empty() {
        let n = (std::mem::size_of::<RawFd>() * fds.len()) as u32;
        unsafe {
            msg.msg_control = cbuf.as_mut_ptr().cast();
            msg.msg_controllen = libc::CMSG_SPACE(n) as _;
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(n) as _;
            let d = libc::CMSG_DATA(c) as *mut RawFd;
            for (i, &f) in fds.iter().enumerate() {
                std::ptr::write_unaligned(d.add(i), f);
            }
        }
    }
    let r = unsafe { libc::sendmsg(s, &msg, 0) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(r as usize)
}

fn recv_fds(s: RawFd) -> io::Result<(u8, Vec<OwnedFd>)> {
    let mut byte = [0u8];
    let (n, fds) = recv_bytes_fds(s, &mut byte)?;
    if n != 1 {
        return Err(io::ErrorKind::UnexpectedEof.into());
    }
    Ok((byte[0], fds))
}

/// Read into `buf`, keeping the descriptors that came along (at most 4).
pub fn recv_bytes_fds(s: RawFd, buf: &mut [u8]) -> io::Result<(usize, Vec<OwnedFd>)> {
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
    let mut cbuf = [0u64; CMSG_SPACE / 8];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = CMSG_SPACE as _;
    let n = unsafe { libc::recvmsg(s, &mut msg, 0) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut fds = Vec::new();
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        if !c.is_null() && (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
            let k = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / std::mem::size_of::<RawFd>();
            let d = libc::CMSG_DATA(c) as *const RawFd;
            for i in 0..k {
                fds.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(d.add(i))));
            }
        }
    }
    Ok((n as usize, fds))
}

#[cfg(test)]
mod tests {
    use super::Spawner;
    use std::io::{Read, Write};

    #[test]
    fn spawns_again_and_again() {
        // A program that answers on fd 3 and then exits; and an unknown kind.
        let sp = Spawner::start(&[(b'L', "/bin/sh", &["-c", "read x <&3; echo \"got $x\" >&3"])]).unwrap();
        for i in 0..5 {
            let mut s = sp.spawn(b'L').unwrap();
            writeln!(s, "{i}").unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).unwrap();
            assert_eq!(out, format!("got {i}\n"));
        }
        assert!(sp.spawn(b'X').is_err());
        // The child sees only fds 0-3.
        let sp2 = Spawner::start(&[(b'L', "/bin/sh", &["-c", "ls /dev/fd/ | grep -c '^[0-9]' >&3"])]).unwrap();
        let mut s = sp2.spawn(b'L').unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        // 0, 1, 2, 3 and the directory ls is reading.
        let n: u32 = out.trim().parse().unwrap();
        assert!(n <= 5, "fds: {n}");
    }

    #[test]
    fn mdns_sockets() {
        use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
        // This host's address towards the outside, if it has one.
        let Some(SocketAddr::V4(a)) = UdpSocket::bind("0.0.0.0:0").and_then(|u| u.connect("192.0.2.1:9").and_then(|_| u.local_addr())).ok() else {
            eprintln!("no IPv4 interface: skipped");
            return;
        };
        let sp = Spawner::start(&[]).unwrap();
        let (r, w) = match sp.mdns4(*a.ip()) {
            Ok(x) => x,
            Err(e) => {
                eprintln!("no multicast here ({e}): skipped");
                return;
            }
        };
        let (r, w) = (UdpSocket::from(r), UdpSocket::from(w));
        assert_eq!(r.local_addr().unwrap().port(), 5353);
        assert_eq!(w.local_addr().unwrap().port(), 5353);
        assert_eq!(w.peer_addr().unwrap(), "224.0.0.251:5353".parse().unwrap());
        assert!(r.peer_addr().is_err());
        // The receive socket cannot send; the send socket only to the group.
        #[cfg(target_os = "freebsd")]
        assert!(r.send_to(b"x", "224.0.0.251:5353").is_err());
        // FreeBSD: EISCONN (Linux lets a connected socket send elsewhere).
        #[cfg(target_os = "freebsd")]
        assert!(w.send_to(b"x", "192.0.2.1:53").is_err());
        assert_eq!(w.send(b"\0\0\0\0\0\0\0\0\0\0\0\0").unwrap(), 12);
        assert!(!w.multicast_loop_v4().unwrap());
        assert_eq!(w.multicast_ttl_v4().unwrap(), 255);
        // A second pair while the first is open (SO_REUSEPORT), and a
        // bad address is refused, the spawner still serving.
        assert!(sp.mdns4(*a.ip()).is_ok());
        assert!(sp.mdns4(Ipv4Addr::new(192, 0, 2, 99)).is_err());
        assert!(sp.mdns4(*a.ip()).is_ok());
    }

    #[test]
    fn spawner_exits_when_dropped() {
        let sp = Spawner::start(&[(b'L', "/bin/true", &[])]).unwrap();
        let pid = sp.pid();
        drop(sp);
        let mut st = 0;
        let r = unsafe { libc::waitpid(pid, &mut st, 0) };
        assert_eq!(r, pid);
        assert!(libc::WIFEXITED(st) && libc::WEXITSTATUS(st) == 0);
    }
}
