//! FreeBSD system calls atrium-net needs: interface flags, addresses, MTU
//! (ioctls), routes (routing socket), bpf, and unbound's control socket.
//! Layouts are for LP64 FreeBSD (arm64, amd64).

#![allow(clippy::missing_safety_doc)]

use std::ffi::CString;
use std::io;
use std::net::Ipv4Addr;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

const IOC_OUT: u64 = 0x4000_0000;
const IOC_IN: u64 = 0x8000_0000;
const IOC_VOID: u64 = 0x2000_0000;
const fn ioc(dir: u64, g: u8, n: u8, len: usize) -> u64 {
    dir | ((len as u64 & 0x1fff) << 16) | ((g as u64) << 8) | n as u64
}
const IFREQ: usize = 32;
const SIOCSIFFLAGS: u64 = ioc(IOC_IN, b'i', 16, IFREQ);
const SIOCGIFFLAGS: u64 = ioc(IOC_IN | IOC_OUT, b'i', 17, IFREQ);
const SIOCDIFADDR: u64 = ioc(IOC_IN, b'i', 25, IFREQ);
const SIOCAIFADDR: u64 = ioc(IOC_IN, b'i', 43, 68);
const SIOCSIFMTU: u64 = ioc(IOC_IN, b'i', 52, IFREQ);
const BIOCGBLEN: u64 = ioc(IOC_OUT, b'B', 102, 4);
const BIOCSETF: u64 = ioc(IOC_IN, b'B', 103, 16);
const BIOCSETIF: u64 = ioc(IOC_IN, b'B', 108, IFREQ);
const BIOCIMMEDIATE: u64 = ioc(IOC_IN, b'B', 112, 4);
const BIOCSHDRCMPLT: u64 = ioc(IOC_IN, b'B', 117, 4);
const BIOCSDIRECTION: u64 = ioc(IOC_IN, b'B', 119, 4);
const BIOCLOCK: u64 = ioc(IOC_VOID, b'B', 122, 0);
const IFF_UP: i16 = 0x1;

/// The last OS error, kept raw (callers match on errno); `what` only names
/// the call in messages where the caller has no better context.
fn err<T>(what: &str) -> io::Result<T> {
    let e = io::Error::last_os_error();
    if std::env::var_os("ATRIUM_NET_TRACE").is_some() {
        eprintln!("atrium-net: {what}: {e}");
    }
    Err(e)
}

fn ifreq(name: &str) -> [u8; IFREQ] {
    let mut r = [0u8; IFREQ];
    let n = name.as_bytes();
    r[..n.len().min(15)].copy_from_slice(&n[..n.len().min(15)]);
    r
}

fn sin(a: Ipv4Addr) -> [u8; 16] {
    let mut s = [0u8; 16];
    s[0] = 16;
    s[1] = libc::AF_INET as u8;
    s[4..8].copy_from_slice(&a.octets());
    s
}

unsafe fn ioctl(fd: RawFd, req: u64, arg: *mut u8) -> libc::c_int {
    libc::ioctl(fd, req as libc::c_ulong, arg)
}

pub struct Ifc {
    /// An AF_INET datagram socket for interface ioctls.
    s: OwnedFd,
}

impl Ifc {
    pub fn new() -> io::Result<Ifc> {
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return err("socket");
        }
        Ok(Ifc { s: unsafe { OwnedFd::from_raw_fd(fd) } })
    }

    pub fn up(&self, name: &str) -> io::Result<()> {
        let mut r = ifreq(name);
        if unsafe { ioctl(self.s.as_raw_fd(), SIOCGIFFLAGS, r.as_mut_ptr()) } != 0 {
            return err("SIOCGIFFLAGS");
        }
        let f = i16::from_ne_bytes([r[16], r[17]]) | IFF_UP;
        r[16..18].copy_from_slice(&f.to_ne_bytes());
        if unsafe { ioctl(self.s.as_raw_fd(), SIOCSIFFLAGS, r.as_mut_ptr()) } != 0 {
            return err("SIOCSIFFLAGS");
        }
        Ok(())
    }

    pub fn add_addr(&self, name: &str, a: Ipv4Addr, prefix: u8) -> io::Result<()> {
        let m = net_proto::mask(prefix);
        let mut r = [0u8; 68];
        r[..16].copy_from_slice(&ifreq(name)[..16]);
        r[16..32].copy_from_slice(&sin(a));
        r[32..48].copy_from_slice(&sin(Ipv4Addr::from(u32::from(a) | !m)));
        r[48..64].copy_from_slice(&sin(Ipv4Addr::from(m)));
        if unsafe { ioctl(self.s.as_raw_fd(), SIOCAIFADDR, r.as_mut_ptr()) } != 0 {
            return err("SIOCAIFADDR");
        }
        Ok(())
    }

    pub fn del_addr(&self, name: &str, a: Ipv4Addr) -> io::Result<()> {
        let mut r = ifreq(name);
        r[16..32].copy_from_slice(&sin(a));
        if unsafe { ioctl(self.s.as_raw_fd(), SIOCDIFADDR, r.as_mut_ptr()) } != 0 {
            return err("SIOCDIFADDR");
        }
        Ok(())
    }

    pub fn set_mtu(&self, name: &str, mtu: u16) -> io::Result<()> {
        let mut r = ifreq(name);
        r[16..20].copy_from_slice(&(mtu as i32).to_ne_bytes());
        if unsafe { ioctl(self.s.as_raw_fd(), SIOCSIFMTU, r.as_mut_ptr()) } != 0 {
            return err("SIOCSIFMTU");
        }
        Ok(())
    }
}

/// Every interface: (name, index, MAC if Ethernet-like, IPv4 (addr, prefix) list).
pub struct IfInfo {
    pub name: String,
    pub index: u16,
    pub mac: Option<[u8; 6]>,
    pub v4: Vec<(Ipv4Addr, u8)>,
}

pub fn interfaces() -> io::Result<Vec<IfInfo>> {
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    if unsafe { libc::getifaddrs(&mut head) } != 0 {
        return err("getifaddrs");
    }
    let mut out: Vec<IfInfo> = Vec::new();
    let mut p = head;
    while !p.is_null() {
        let ifa = unsafe { &*p };
        p = ifa.ifa_next;
        if ifa.ifa_addr.is_null() {
            continue;
        }
        let name = unsafe { std::ffi::CStr::from_ptr(ifa.ifa_name) }.to_string_lossy().into_owned();
        let idx = out.iter().position(|i| i.name == name).unwrap_or_else(|| {
            out.push(IfInfo { name: name.clone(), index: 0, mac: None, v4: Vec::new() });
            out.len() - 1
        });
        let sa = ifa.ifa_addr as *const u8;
        let fam = unsafe { *sa.add(1) } as i32;
        if fam == libc::AF_LINK {
            let b = unsafe { std::slice::from_raw_parts(sa, 8 + 46) };
            out[idx].index = u16::from_ne_bytes([b[2], b[3]]);
            let (nlen, alen) = (b[5] as usize, b[6] as usize);
            if alen == 6 && 8 + nlen + 6 <= b.len() {
                out[idx].mac = Some(b[8 + nlen..8 + nlen + 6].try_into().unwrap());
            }
        } else if fam == libc::AF_INET {
            let a = unsafe { std::slice::from_raw_parts(sa, 16) };
            let addr = Ipv4Addr::new(a[4], a[5], a[6], a[7]);
            let prefix = if ifa.ifa_netmask.is_null() {
                32
            } else {
                let m = unsafe { std::slice::from_raw_parts(ifa.ifa_netmask as *const u8, 8) };
                u32::from_be_bytes([m[4], m[5], m[6], m[7]]).leading_ones() as u8
            };
            out[idx].v4.push((addr, prefix));
        }
    }
    unsafe { libc::freeifaddrs(head) };
    Ok(out)
}

// ---- routing socket ------------------------------------------------------

const RTM_VERSION: u8 = 5;
pub const RTM_ADD: u8 = 1;
pub const RTM_DELETE: u8 = 2;
pub const RTM_CHANGE: u8 = 3;
pub const RTM_IFINFO: u8 = 0xe;
const RTF_UP: i32 = 0x1;
const RTF_GATEWAY: i32 = 0x2;
const RTF_STATIC: i32 = 0x800;
const RTA_DST: i32 = 0x1;
const RTA_GATEWAY: i32 = 0x2;
const RTA_NETMASK: i32 = 0x4;
const RT_MSGHDR: usize = 152;

pub struct Rtsock {
    pub s: OwnedFd,
    seq: i32,
}

impl Rtsock {
    pub fn new() -> io::Result<Rtsock> {
        let fd = unsafe { libc::socket(libc::PF_ROUTE, libc::SOCK_RAW | libc::SOCK_CLOEXEC, libc::AF_UNSPEC) };
        if fd < 0 {
            return err("routing socket");
        }
        Ok(Rtsock { s: unsafe { OwnedFd::from_raw_fd(fd) }, seq: 0 })
    }

    /// Add, change or delete an IPv4 route. `gw` unspecified means on-link
    /// through `ifaddr`.
    pub fn route(&mut self, op: u8, dst: Ipv4Addr, prefix: u8, gw: Ipv4Addr, ifaddr: Ipv4Addr) -> io::Result<()> {
        self.seq += 1;
        let mut m = vec![0u8; RT_MSGHDR];
        let mut flags = RTF_UP | RTF_STATIC;
        let gwa = if gw.is_unspecified() { ifaddr } else {
            flags |= RTF_GATEWAY;
            gw
        };
        m.extend_from_slice(&sin(dst));
        m.extend_from_slice(&sin(gwa));
        m.extend_from_slice(&sin(Ipv4Addr::from(net_proto::mask(prefix))));
        let len = m.len() as u16;
        m[0..2].copy_from_slice(&len.to_ne_bytes());
        m[2] = RTM_VERSION;
        m[3] = op;
        m[8..12].copy_from_slice(&flags.to_ne_bytes());
        m[12..16].copy_from_slice(&(RTA_DST | RTA_GATEWAY | RTA_NETMASK).to_ne_bytes());
        m[20..24].copy_from_slice(&self.seq.to_ne_bytes());
        let n = unsafe { libc::write(self.s.as_raw_fd(), m.as_ptr().cast(), m.len()) };
        if n != m.len() as isize {
            return err(match op { RTM_ADD => "RTM_ADD", RTM_DELETE => "RTM_DELETE", _ => "RTM_CHANGE" });
        }
        Ok(())
    }

    /// Link-state changes from the kernel: (interface index, link up).
    pub fn read_ifinfo(&self) -> Vec<(u16, bool)> {
        let mut out = Vec::new();
        let mut b = [0u8; 2048];
        loop {
            let n = unsafe { libc::recv(self.s.as_raw_fd(), b.as_mut_ptr().cast(), b.len(), libc::MSG_DONTWAIT) };
            if n <= 0 {
                break;
            }
            let n = n as usize;
            // if_msghdr: msglen u16, version u8, type u8, addrs i32, flags i32,
            // index u16, spare u16, then if_data (type, physical, addrlen,
            // hdrlen, link_state: its 5th byte).
            if n >= 24 && b[2] == RTM_VERSION && b[3] == RTM_IFINFO {
                let idx = u16::from_ne_bytes([b[12], b[13]]);
                let link = b[16 + 4];
                out.push((idx, link == 2));
            }
        }
        out
    }
}

// ---- bpf -----------------------------------------------------------------

#[repr(C)]
struct BpfInsn {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

/// IPv4, UDP, not a fragment, source port 67, destination port 68.
const DHCP_FILTER: [(u16, u8, u8, u32); 13] = [
    (0x28, 0, 0, 12),
    (0x15, 0, 10, 0x0800),
    (0x30, 0, 0, 23),
    (0x15, 0, 8, 17),
    (0x28, 0, 0, 20),
    (0x45, 6, 0, 0x1fff),
    (0xb1, 0, 0, 14),
    (0x48, 0, 0, 14),
    (0x15, 0, 3, 67),
    (0x48, 0, 0, 16),
    (0x15, 0, 1, 68),
    (0x06, 0, 0, 1514),
    (0x06, 0, 0, 0),
];

pub struct Bpf {
    pub fd: OwnedFd,
    buf: Vec<u8>,
}

impl Bpf {
    pub fn open(ifname: &str) -> io::Result<Bpf> {
        let fd = unsafe { libc::open(c"/dev/bpf".as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            return err("/dev/bpf");
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let r = fd.as_raw_fd();
        let mut one: u32 = 1;
        let mut zero: u32 = 0;
        let mut blen: u32 = 0;
        let mut ir = ifreq(ifname);
        let insns: Vec<BpfInsn> = DHCP_FILTER.iter().map(|&(code, jt, jf, k)| BpfInsn { code, jt, jf, k }).collect();
        #[repr(C)]
        struct Prog {
            len: u32,
            insns: *const BpfInsn,
        }
        let mut prog = Prog { len: insns.len() as u32, insns: insns.as_ptr() };
        unsafe {
            if ioctl(r, BIOCSETIF, ir.as_mut_ptr()) != 0 {
                return err("BIOCSETIF");
            }
            if ioctl(r, BIOCIMMEDIATE, (&mut one as *mut u32).cast()) != 0 {
                return err("BIOCIMMEDIATE");
            }
            if ioctl(r, BIOCSHDRCMPLT, (&mut one as *mut u32).cast()) != 0 {
                return err("BIOCSHDRCMPLT");
            }
            // BPF_D_IN: only frames we receive, not our own.
            if ioctl(r, BIOCSDIRECTION, (&mut zero as *mut u32).cast()) != 0 {
                return err("BIOCSDIRECTION");
            }
            if ioctl(r, BIOCSETF, (&mut prog as *mut Prog).cast()) != 0 {
                return err("BIOCSETF");
            }
            if ioctl(r, BIOCGBLEN, (&mut blen as *mut u32).cast()) != 0 {
                return err("BIOCGBLEN");
            }
            if ioctl(r, BIOCLOCK, std::ptr::null_mut()) != 0 {
                return err("BIOCLOCK");
            }
        }
        Ok(Bpf { fd, buf: vec![0; blen as usize] })
    }

    /// Frames received since the last read (bpf headers come from the kernel).
    pub fn read(&mut self) -> Vec<Vec<u8>> {
        let n = unsafe { libc::read(self.fd.as_raw_fd(), self.buf.as_mut_ptr().cast(), self.buf.len()) };
        let mut out = Vec::new();
        if n <= 0 {
            return out;
        }
        let n = n as usize;
        let mut off = 0;
        let hl = std::mem::size_of::<libc::bpf_hdr>();
        while off + hl <= n {
            let h = unsafe { std::ptr::read_unaligned(self.buf.as_ptr().add(off) as *const libc::bpf_hdr) };
            let (cap, hdr) = (h.bh_caplen as usize, h.bh_hdrlen as usize);
            if off + hdr + cap > n {
                break;
            }
            out.push(self.buf[off + hdr..off + hdr + cap].to_vec());
            off += (hdr + cap + 7) & !7;
        }
        out
    }

    pub fn write(&self, f: &[u8]) -> io::Result<()> {
        let n = unsafe { libc::write(self.fd.as_raw_fd(), f.as_ptr().cast(), f.len()) };
        if n != f.len() as isize {
            return err("bpf write");
        }
        Ok(())
    }
}

// ---- unbound ---------------------------------------------------------------

extern "C" {
    fn connectat(fd: libc::c_int, s: libc::c_int, name: *const libc::sockaddr, len: libc::socklen_t) -> libc::c_int;
}

/// local-unbound's control socket, reached through a directory descriptor
/// so it still works in capability mode.
pub struct Unbound {
    dir: OwnedFd,
    name: CString,
}

impl Unbound {
    pub fn new(dir: &str, name: &str) -> io::Result<Unbound> {
        let d = CString::new(dir).unwrap();
        let fd = unsafe { libc::open(d.as_ptr(), libc::O_DIRECTORY | libc::O_CLOEXEC) };
        if fd < 0 {
            return err("unbound socket directory");
        }
        Ok(Unbound { dir: unsafe { OwnedFd::from_raw_fd(fd) }, name: CString::new(name).unwrap() })
    }

    /// One control command; returns unbound's answer.
    pub fn cmd(&self, c: &str) -> io::Result<String> {
        let s = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        if s < 0 {
            return err("socket");
        }
        let s = unsafe { OwnedFd::from_raw_fd(s) };
        let mut sa: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        sa.sun_family = libc::AF_UNIX as _;
        let n = self.name.as_bytes();
        for (i, b) in n.iter().enumerate().take(sa.sun_path.len() - 1) {
            sa.sun_path[i] = *b as libc::c_char;
        }
        sa.sun_len = std::mem::size_of::<libc::sockaddr_un>() as u8;
        if unsafe { connectat(self.dir.as_raw_fd(), s.as_raw_fd(), (&sa as *const libc::sockaddr_un).cast(), sa.sun_len as libc::socklen_t) } != 0 {
            return err("connect to unbound");
        }
        let msg = format!("UBCT1 {c}\n");
        if unsafe { libc::write(s.as_raw_fd(), msg.as_ptr().cast(), msg.len()) } != msg.len() as isize {
            return err("write to unbound");
        }
        let mut out = Vec::new();
        let mut b = [0u8; 512];
        loop {
            let n = unsafe { libc::read(s.as_raw_fd(), b.as_mut_ptr().cast(), b.len()) };
            if n <= 0 {
                break;
            }
            out.extend_from_slice(&b[..n as usize]);
            if out.len() > 65536 {
                break;
            }
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
}

pub fn cap_enter() -> io::Result<()> {
    if unsafe { libc::cap_enter() } != 0 {
        return err("cap_enter");
    }
    Ok(())
}
