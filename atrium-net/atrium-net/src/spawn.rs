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
//! It also opens tunnels' tun devices (`b'T'`, vpn.md §5) and loads the
//! `atrium/vpn` pf anchor (`b'F'`, §5.4): capability mode forbids opening
//! a device by name and running pfctl.
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
        assert!(!matches!(kind, b'M' | b'S' | b'U' | b'T' | b'F' | b'H'));
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

    /// atrium-mdns's pair (spec §6.1): like [`Spawner::mdns4`] but on a
    /// port of its own instead of 5353, so answers come back unicast to it
    /// (one-shot "legacy" queries, RFC 6762 §5.1) and never compete with
    /// printer discovery for 5353 (FreeBSD hands a unicast datagram to one
    /// socket only, and refuses two sockets connected the same way).
    /// Receive: bound 0.0.0.0:P, no group; send: bound P, connected.
    pub fn oneshot4(&self, addr: Ipv4Addr) -> io::Result<(OwnedFd, OwnedFd)> {
        let mut req = vec![b'U'];
        req.extend_from_slice(&addr.octets());
        let mut v = self.ask(&req, 2, "spawner could not make the one-shot mDNS sockets")?;
        let send = v.pop().unwrap();
        Ok((v.pop().unwrap(), send))
    }

    /// A TCP listener on 127.0.0.1:`port` (unbound's stub zone for
    /// `.local` talks to atrium-mdns over it; spec §6.1).
    pub fn stub_listener(&self, port: u16) -> io::Result<OwnedFd> {
        let mut req = vec![b'S'];
        req.extend_from_slice(&port.to_be_bytes());
        Ok(self.ask(&req, 1, "spawner could not make the stub listener")?.remove(0))
    }

    /// A new tun (`/dev/tun` clones one), in point-to-point mode with the
    /// 4-byte address family header (TUNSIFHEAD): its descriptor and name.
    pub fn tun(&self) -> io::Result<(OwnedFd, String)> {
        let fd = self.ask(&[b'T'], 1, "spawner could not open a tun")?.remove(0);
        let mut n = [0u8; 1];
        let s = self.s.as_raw_fd();
        if !read_exact(s, &mut n) {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let mut name = vec![0u8; n[0] as usize];
        if !read_exact(s, &mut name) {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        let name = String::from_utf8(name).map_err(|_| io::Error::other("tun name"))?;
        // tunN only: the name goes into ioctls and pf rules.
        if !name.strip_prefix("tun").is_some_and(|d| !d.is_empty() && d.len() <= 4 && d.bytes().all(|b| b.is_ascii_digit())) {
            return Err(io::Error::other("tun name"));
        }
        Ok((fd, name))
    }

    /// Load `rules` as the whole `atrium/vpn` anchor (empty: flush it), then
    /// drop every pf state on `flush_on` (interface name, may be empty), so
    /// flows opened before a block cannot carry on past it.
    pub fn pf_anchor(&self, rules: &str, flush_on: &str) -> io::Result<()> {
        if flush_on.len() > 15 || !flush_on.bytes().all(|b| b.is_ascii_alphanumeric()) || rules.len() > 1 << 20 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let mut req = vec![b'F', flush_on.len() as u8];
        req.extend_from_slice(flush_on.as_bytes());
        req.extend_from_slice(&(rules.len() as u32).to_be_bytes());
        req.extend_from_slice(rules.as_bytes());
        let fd = self.s.as_raw_fd();
        let mut off = 0;
        while off < req.len() {
            let n = unsafe { libc::write(fd, req[off..].as_ptr().cast(), req.len() - off) };
            if n <= 0 {
                return Err(io::Error::last_os_error());
            }
            off += n as usize;
        }
        let (b, _) = recv_fds(fd)?;
        if b == b'K' {
            Ok(())
        } else {
            Err(io::Error::other("pfctl refused the anchor"))
        }
    }

    /// The kernel's pf running flag. Unknown when the query fails.
    pub fn pf_running(&self) -> Option<bool> {
        let fd = self.s.as_raw_fd();
        // SAFETY: fd is the live spawner socket and the buffer is one byte.
        if unsafe { libc::write(fd, b"H".as_ptr().cast(), 1) } != 1 {
            return None;
        }
        match recv_fds(fd).ok()?.0 {
            b'1' => Some(true),
            b'0' => Some(false),
            _ => None,
        }
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

fn stub_listener(port: u16) -> io::Result<OwnedFd> {
    let s = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
    if s < 0 {
        return Err(io::Error::last_os_error());
    }
    let s = unsafe { OwnedFd::from_raw_fd(s) };
    let one: libc::c_int = 1;
    sockopt(s.as_raw_fd(), libc::SOL_SOCKET, libc::SO_REUSEADDR, &one)?;
    let a = sin([127, 0, 0, 1], port);
    if unsafe { libc::bind(s.as_raw_fd(), (&a as *const libc::sockaddr_in).cast(), std::mem::size_of::<libc::sockaddr_in>() as _) } != 0
        || unsafe { libc::listen(s.as_raw_fd(), 16) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(s)
}

const MDNS_GROUP: [u8; 4] = [224, 0, 0, 251];
const MDNS_PORT: u16 = 5353;

/// One of the two mDNS sockets (in the spawner, before any capability mode).
/// A port nobody uses now (the kernel's pick for a socket bound to 0).
fn free_port() -> io::Result<u16> {
    let s = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if s < 0 {
        return Err(io::Error::last_os_error());
    }
    let s = unsafe { OwnedFd::from_raw_fd(s) };
    let a = sin([0; 4], 0);
    if unsafe { libc::bind(s.as_raw_fd(), (&a as *const libc::sockaddr_in).cast(), std::mem::size_of::<libc::sockaddr_in>() as _) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut b: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut n = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
    if unsafe { libc::getsockname(s.as_raw_fd(), (&mut b as *mut libc::sockaddr_in).cast(), &mut n) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(u16::from_be(b.sin_port))
}

fn mdns_socket(ifaddr: [u8; 4], send: bool, port: u16, join: bool) -> io::Result<OwnedFd> {
    let s = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if s < 0 {
        return Err(io::Error::last_os_error());
    }
    let s = unsafe { OwnedFd::from_raw_fd(s) };
    let f = s.as_raw_fd();
    let one: libc::c_int = 1;
    sockopt(f, libc::SOL_SOCKET, libc::SO_REUSEADDR, &one)?;
    sockopt(f, libc::SOL_SOCKET, libc::SO_REUSEPORT, &one)?;
    let any = sin([0; 4], port);
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
    } else if join {
        let m = libc::ip_mreq { imr_multiaddr: libc::in_addr { s_addr: u32::from_ne_bytes(MDNS_GROUP) }, imr_interface: ifa };
        sockopt(f, libc::IPPROTO_IP, libc::IP_ADD_MEMBERSHIP, &m)?;
        // FreeBSD shuts an unconnected datagram socket's sending side and
        // still says ENOTCONN; Linux only says ENOTCONN (host tests).
    }
    if !send && unsafe { libc::shutdown(f, libc::SHUT_WR) } != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::ENOTCONN) {
        return Err(io::Error::last_os_error());
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
        if k == b'M' || k == b'U' {
            let mut a = [0u8; 4];
            if !read_exact(s, &mut a) {
                unsafe { libc::_exit(0) };
            }
            let port = if k == b'M' { Ok(MDNS_PORT) } else { free_port() };
            let pair = port.and_then(|p| Ok((mdns_socket(a, false, p, k == b'M')?, p))).and_then(|(r, p)| Ok((r, mdns_socket(a, true, p, false)?)));
            match pair {
                Ok((r, w)) => {
                    let _ = send_fds(s, b'K', &[r.as_raw_fd(), w.as_raw_fd()]);
                }
                Err(e) => {
                    eprintln!("atrium-net spawner: mDNS sockets: {e}");
                    let _ = send_fd(s, b'E', None);
                }
            }
            continue;
        }
        if k == b'T' {
            match open_tun() {
                Ok((t, name)) => {
                    if send_fds(s, b'K', &[t.as_raw_fd()]).is_ok() {
                        let mut b = vec![name.len() as u8];
                        b.extend_from_slice(name.as_bytes());
                        unsafe { libc::write(s, b.as_ptr().cast(), b.len()) };
                    }
                }
                Err(e) => {
                    eprintln!("atrium-net spawner: tun: {e}");
                    let _ = send_fd(s, b'E', None);
                }
            }
            continue;
        }
        if k == b'H' {
            let running = pf_running();
            let _ = send_fd(s, match running { Some(true) => b'1', Some(false) => b'0', None => b'E' }, None);
            continue;
        }
        if k == b'F' {
            let mut n = [0u8; 1];
            if !read_exact(s, &mut n) {
                unsafe { libc::_exit(0) };
            }
            let mut iface = vec![0u8; n[0] as usize];
            let mut len = [0u8; 4];
            if !read_exact(s, &mut iface) || !read_exact(s, &mut len) {
                unsafe { libc::_exit(0) };
            }
            let mut rules = vec![0u8; u32::from_be_bytes(len) as usize];
            if !read_exact(s, &mut rules) {
                unsafe { libc::_exit(0) };
            }
            let ok = load_anchor(&rules, &String::from_utf8_lossy(&iface));
            let _ = send_fd(s, if ok { b'K' } else { b'E' }, None);
            continue;
        }
        if k == b'S' {
            let mut p = [0u8; 2];
            if !read_exact(s, &mut p) {
                unsafe { libc::_exit(0) };
            }
            match stub_listener(u16::from_be_bytes(p)) {
                Ok(l) => {
                    let _ = send_fds(s, b'K', &[l.as_raw_fd()]);
                }
                Err(_) => {
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

const PFCTL: &str = "/sbin/pfctl";
const VPN_ANCHOR: &str = "atrium/vpn";

/// `pfctl -a atrium/vpn -f -` with `rules` on its stdin (empty: `-F all`),
/// then `pfctl -i IFACE -F states` when an interface is named.
fn load_anchor(rules: &[u8], iface: &str) -> bool {
    // The spawner ignores SIGCHLD so its children are reaped by the kernel;
    // pfctl's status is wanted here, so the default for the while. A helper
    // that exits meanwhile is reaped below.
    unsafe { libc::signal(libc::SIGCHLD, libc::SIG_DFL) };
    let ok = run_pfctl(rules, iface);
    unsafe {
        libc::signal(libc::SIGCHLD, libc::SIG_IGN);
        while libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) > 0 {}
    }
    ok
}

fn pf_running() -> Option<bool> {
    use std::process::Command;
    // The spawner normally auto-reaps helpers. Temporarily wait for pfctl.
    unsafe { libc::signal(libc::SIGCHLD, libc::SIG_DFL) };
    let result = Command::new(PFCTL).args(["-s", "info"]).env("LC_ALL", "C").output();
    unsafe {
        libc::signal(libc::SIGCHLD, libc::SIG_IGN);
        while libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) > 0 {}
    }
    let result = result.ok().filter(|o| o.status.success())?;
    let status = std::str::from_utf8(&result.stdout).ok()?.lines()
        .find_map(|line| line.strip_prefix("Status:"))?.split_whitespace().next()?;
    match status {
        "Enabled" => Some(true),
        "Disabled" => Some(false),
        _ => None,
    }
}

fn run_pfctl(rules: &[u8], iface: &str) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let ok = if rules.is_empty() {
        Command::new(PFCTL).args(["-a", VPN_ANCHOR, "-F", "all"]).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
    } else {
        match Command::new(PFCTL).args(["-a", VPN_ANCHOR, "-f", "-"]).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::inherit()).spawn() {
            Ok(mut c) => {
                let wrote = c.stdin.take().is_some_and(|mut i| i.write_all(rules).is_ok());
                c.wait().is_ok_and(|s| s.success()) && wrote
            }
            Err(_) => false,
        }
    };
    if ok && !iface.is_empty() {
        let _ = Command::new(PFCTL).args(["-i", iface, "-F", "states"]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
    ok
}

#[cfg(target_os = "freebsd")]
extern "C" {
    fn fdevname_r(fd: libc::c_int, buf: *mut libc::c_char, len: libc::c_int) -> *mut libc::c_char;
}

/// TUNSIFHEAD: `_IOW('t', 96, int)`.
#[cfg(target_os = "freebsd")]
const TUNSIFHEAD: libc::c_ulong = 0x8000_0000 | (4 << 16) | ((b't' as libc::c_ulong) << 8) | 96;

#[cfg(target_os = "freebsd")]
fn open_tun() -> io::Result<(OwnedFd, String)> {
    let fd = unsafe { libc::open(c"/dev/tun".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut buf = [0 as libc::c_char; 32];
    if unsafe { fdevname_r(fd.as_raw_fd(), buf.as_mut_ptr(), buf.len() as libc::c_int) }.is_null() {
        return Err(io::Error::last_os_error());
    }
    let name = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned();
    let one: libc::c_int = 1;
    if unsafe { libc::ioctl(fd.as_raw_fd(), TUNSIFHEAD, &one) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((fd, name))
}

#[cfg(not(target_os = "freebsd"))]
fn open_tun() -> io::Result<(OwnedFd, String)> {
    Err(io::ErrorKind::Unsupported.into())
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
        // The one-shot pair: its own port, both sockets on it, alongside.
        let (r2, w2) = sp.oneshot4(*a.ip()).unwrap();
        let (r2, w2) = (UdpSocket::from(r2), UdpSocket::from(w2));
        let p = r2.local_addr().unwrap().port();
        assert!(p != 5353 && w2.local_addr().unwrap().port() == p);
        assert_eq!(w2.peer_addr().unwrap(), "224.0.0.251:5353".parse().unwrap());
        // An answer sent to that port lands on the receive socket.
        let other = UdpSocket::bind("0.0.0.0:0").unwrap();
        other.send_to(b"answer", (*a.ip(), p)).unwrap();
        r2.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
        let mut b = [0u8; 16];
        assert_eq!(r2.recv(&mut b).unwrap(), 6);
    }

    #[test]
    fn stub_listener() {
        let sp = Spawner::start(&[]).unwrap();
        let l = std::net::TcpListener::from(sp.stub_listener(0).unwrap());
        let a = l.local_addr().unwrap();
        assert!(a.ip().is_loopback() && a.port() != 0);
        let _c = std::net::TcpStream::connect(a).unwrap();
        assert!(l.accept().is_ok());
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
