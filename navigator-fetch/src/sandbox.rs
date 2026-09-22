//! Capsicum capability mode + casper `cap_net` (FreeBSD).
//!
//! ★★ ORDER IS THE WHOLE DESIGN. Everything that needs the global namespace —
//! reading the trust store, forking the casper helper, limiting it — happens
//! in `enter`, BEFORE `cap_enter()`. After it, this process can name no file,
//! no address and no process: `open`, `stat`, `connect`, `bind` and friends
//! fail with `ECAPMODE` in the kernel. What it keeps is a channel to casper,
//! which will resolve a name on port 80/443 and connect a socket to an
//! address IT resolved (`CAPNET_CONNECTDNS`) — and nothing else.
//!
//! That is what M1's gate means by "filesystem access refused at the
//! syscall": not a policy the code follows, a mode the kernel enforces.

use std::ffi::{c_char, c_int, CString};
use std::io;

#[repr(C)] pub struct CapChannel { _p: [u8; 0] }
#[repr(C)] struct CapNetLimit { _p: [u8; 0] }

const CAPNET_NAME2ADDR: u64 = 0x02;
const CAPNET_CONNECTDNS: u64 = 0x40;

#[link(name = "casper")]
extern "C" {
    fn cap_init() -> *mut CapChannel;
    fn cap_service_open(chan: *const CapChannel, name: *const c_char) -> *mut CapChannel;
    fn cap_close(chan: *mut CapChannel);
}
#[link(name = "cap_net")]
extern "C" {
    fn cap_net_limit_init(chan: *mut CapChannel, mode: u64) -> *mut CapNetLimit;
    fn cap_net_limit_name2addr(limit: *mut CapNetLimit, name: *const c_char, serv: *const c_char) -> *mut CapNetLimit;
    fn cap_net_limit(limit: *mut CapNetLimit) -> c_int;
    fn cap_getaddrinfo(chan: *mut CapChannel, host: *const c_char, serv: *const c_char,
                       hints: *const libc::addrinfo, res: *mut *mut libc::addrinfo) -> c_int;
    fn cap_connect(chan: *mut CapChannel, s: c_int, name: *const libc::sockaddr, len: libc::socklen_t) -> c_int;
}
#[link(name = "nv")] extern "C" {}
extern "C" {
    fn cap_enter() -> c_int;
    fn cap_getmode(mode: *mut u32) -> c_int;
}

/// The one network capability left after `enter`.
pub struct Net { chan: *mut CapChannel }

// The channel is used from one thread at a time; casper channels are plain
// descriptors underneath.
unsafe impl Send for Net {}

impl Drop for Net {
    fn drop(&mut self) { unsafe { cap_close(self.chan) } }
}

fn err(what: &str) -> io::Error {
    let e = io::Error::last_os_error();
    io::Error::new(e.kind(), format!("{what}: {e}"))
}

/// Set up casper, then enter capability mode. Irreversible for this process.
pub fn enter() -> io::Result<Net> {
    unsafe {
        let casper = cap_init();
        if casper.is_null() { return Err(err("cap_init")) }
        let svc = CString::new("system.net").unwrap();
        let net = cap_service_open(casper, svc.as_ptr());
        cap_close(casper);
        if net.is_null() { return Err(err("cap_service_open(system.net)")) }
        // Resolve any host, but only for the web's two ports; connect only to
        // what was resolved. NULL host = any host.
        let mut lim = cap_net_limit_init(net, CAPNET_NAME2ADDR | CAPNET_CONNECTDNS);
        if lim.is_null() { cap_close(net); return Err(err("cap_net_limit_init")) }
        for port in ["443", "80"] {
            let p = CString::new(port).unwrap();
            lim = cap_net_limit_name2addr(lim, std::ptr::null(), p.as_ptr());
            if lim.is_null() { cap_close(net); return Err(err("cap_net_limit_name2addr")) }
        }
        if cap_net_limit(lim) != 0 { cap_close(net); return Err(err("cap_net_limit")) }
        if cap_enter() != 0 { cap_close(net); return Err(err("cap_enter")) }
        Ok(Net { chan: net })
    }
}

/// Whether this process is in capability mode — asked of the kernel.
pub fn in_capability_mode() -> bool {
    let mut m = 0u32;
    unsafe { cap_getmode(&mut m) == 0 && m == 1 }
}

impl Net {
    /// A TCP connection to `host:port`, resolved AND connected by casper.
    pub fn connect(&self, host: &str, port: u16) -> io::Result<std::net::TcpStream> {
        use std::os::fd::FromRawFd;
        let h = CString::new(host).map_err(|_| io::Error::other("NUL in host"))?;
        let p = CString::new(port.to_string()).unwrap();
        let mut hints: libc::addrinfo = unsafe { std::mem::zeroed() };
        hints.ai_socktype = libc::SOCK_STREAM;
        hints.ai_family = libc::AF_UNSPEC;
        let mut res: *mut libc::addrinfo = std::ptr::null_mut();
        let rc = unsafe { cap_getaddrinfo(self.chan, h.as_ptr(), p.as_ptr(), &hints, &mut res) };
        if rc != 0 {
            let msg = unsafe { std::ffi::CStr::from_ptr(libc::gai_strerror(rc)) }.to_string_lossy().into_owned();
            return Err(io::Error::other(format!("resolve {host}:{port}: {msg}")));
        }
        let mut last = io::Error::other(format!("no address for {host}"));
        let mut ai = res;
        while !ai.is_null() {
            let a = unsafe { &*ai };
            let s = unsafe { libc::socket(a.ai_family, a.ai_socktype | libc::SOCK_CLOEXEC, a.ai_protocol) };
            if s >= 0 {
                if unsafe { cap_connect(self.chan, s, a.ai_addr, a.ai_addrlen) } == 0 {
                    unsafe { libc::freeaddrinfo(res) };
                    return Ok(unsafe { std::net::TcpStream::from_raw_fd(s) });
                }
                last = err(&format!("connect {host}:{port}"));
                unsafe { libc::close(s) };
            } else {
                last = err("socket");
            }
            ai = a.ai_next;
        }
        unsafe { libc::freeaddrinfo(res) };
        Err(last)
    }

    /// Ask casper to connect to a literal address it did NOT resolve. Exists
    /// so the gate can prove `CAPNET_CONNECTDNS` refuses it.
    pub fn connect_unresolved(&self, addr: std::net::SocketAddrV4) -> io::Result<()> {
        let mut sa: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        sa.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
        sa.sin_family = libc::AF_INET as u8;
        sa.sin_port = addr.port().to_be();
        sa.sin_addr.s_addr = u32::from(*addr.ip()).to_be();
        let s = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        if s < 0 { return Err(err("socket")) }
        let rc = unsafe { cap_connect(self.chan, s, &sa as *const _ as *const libc::sockaddr, sa.sin_len as u32) };
        let e = err("cap_connect");
        unsafe { libc::close(s) };
        if rc == 0 { Ok(()) } else { Err(e) }
    }
}
