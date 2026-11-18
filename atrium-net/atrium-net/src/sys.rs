//! FreeBSD system calls atrium-net needs: interface flags, addresses, MTU
//! (ioctls), routes (routing socket), bpf, and unbound's control socket.
//! Layouts are for LP64 FreeBSD (arm64, amd64).

#![allow(clippy::missing_safety_doc)]

use std::ffi::CString;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr};
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
const SIOCGIFMTU: u64 = ioc(IOC_IN | IOC_OUT, b'i', 51, IFREQ);
const IFGROUPREQ: usize = 40;
const SIOCAIFGROUP: u64 = ioc(IOC_IN, b'i', 135, IFGROUPREQ);
const SIOCDIFGROUP: u64 = ioc(IOC_IN, b'i', 137, IFGROUPREQ);
const IN6_IFREQ: usize = 288;
const IN6_NDIREQ: usize = 72;
const SIOCDIFADDR_IN6: u64 = ioc(IOC_IN, b'i', 25, IN6_IFREQ);
const SIOCGIFAFLAG_IN6: u64 = ioc(IOC_IN | IOC_OUT, b'i', 73, IN6_IFREQ);
pub const IN6_IFF_AUTOCONF: u32 = 0x40;
pub const IN6_IFF_TEMPORARY: u32 = 0x80;
const IN6_ALIASREQ: usize = 136;
const SIOCAIFADDR_IN6: u64 = ioc(IOC_IN, b'i', 27, IN6_ALIASREQ);
const SIOCGIFINFO_IN6: u64 = ioc(IOC_IN | IOC_OUT, b'i', 108, IN6_NDIREQ);
const SIOCSIFINFO_FLAGS: u64 = ioc(IOC_IN | IOC_OUT, b'i', 87, IN6_NDIREQ);
const ND6_IFF_PERFORMNUD: u32 = 0x1;
const ND6_IFF_ACCEPT_RTADV: u32 = 0x2;
const ND6_IFF_IFDISABLED: u32 = 0x8;
const ND6_IFF_AUTO_LINKLOCAL: u32 = 0x20;
const ND6_IFF_STABLEADDR: u32 = 0x800;
const ND6_IFF_NO_RADR: u32 = 0x40;
const SIOCSRTRFLUSH_IN6: u64 = ioc(IOC_IN | IOC_OUT, b'i', 80, IN6_IFREQ);
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
    /// AF_INET and AF_INET6 datagram sockets for interface ioctls.
    s: OwnedFd,
    s6: OwnedFd,
}

impl Ifc {
    pub fn new() -> io::Result<Ifc> {
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return err("socket");
        }
        let s = unsafe { OwnedFd::from_raw_fd(fd) };
        let fd = unsafe { libc::socket(libc::AF_INET6, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return err("socket6");
        }
        Ok(Ifc { s, s6: unsafe { OwnedFd::from_raw_fd(fd) } })
    }

    /// IPv6 on: kernel SLAAC from RAs with stable-private interface IDs
    /// (RFC 7217), a link-local address, NUD (docs/spec/atrium-net.md §5.3).
    pub fn enable_inet6(&self, name: &str) -> io::Result<()> {
        self.nd6_flags(name, ND6_IFF_ACCEPT_RTADV | ND6_IFF_AUTO_LINKLOCAL | ND6_IFF_STABLEADDR | ND6_IFF_PERFORMNUD, ND6_IFF_IFDISABLED)
    }

    /// RA processing on or off. Off while we delete SLAAC addresses: the
    /// kernel races an address deletion against an RA re-creating it and can
    /// free the prefix the new address points to (spec §5.3, kernel panic).
    pub fn accept_rtadv(&self, name: &str, on: bool) -> io::Result<()> {
        if on {
            self.nd6_flags(name, ND6_IFF_ACCEPT_RTADV, 0)
        } else {
            self.nd6_flags(name, 0, ND6_IFF_ACCEPT_RTADV)
        }
    }

    /// Take default routers from this interface's RAs, or not: only the
    /// egress interface's routers become IPv6 default routes (egress.rs).
    pub fn no_radr(&self, name: &str, on: bool) -> io::Result<()> {
        if on {
            self.nd6_flags(name, ND6_IFF_NO_RADR, 0)
        } else {
            self.nd6_flags(name, 0, ND6_IFF_NO_RADR)
        }
    }

    /// Forget every IPv6 default router (they come back from the next RAs
    /// on interfaces that take them).
    pub fn flush_routers6(&self) -> io::Result<()> {
        let mut r = [0u8; IN6_IFREQ];
        r[..16].copy_from_slice(&ifreq("lo0")[..16]);
        if unsafe { ioctl(self.s6.as_raw_fd(), SIOCSRTRFLUSH_IN6, r.as_mut_ptr()) } != 0 {
            return err("SIOCSRTRFLUSH_IN6");
        }
        Ok(())
    }

    /// The IPv6 link MTU a router advertised (0: none). The kernel keeps it
    /// until a later RA says otherwise, and refuses to raise it from
    /// userland (SIOCSIFINFO_IN6 checks against the lowered value).
    pub fn linkmtu6(&self, name: &str) -> io::Result<u32> {
        let mut r = [0u8; IN6_NDIREQ];
        r[..16].copy_from_slice(&ifreq(name)[..16]);
        if unsafe { ioctl(self.s6.as_raw_fd(), SIOCGIFINFO_IN6, r.as_mut_ptr()) } != 0 {
            return err("SIOCGIFINFO_IN6");
        }
        Ok(u32::from_ne_bytes(r[16..20].try_into().unwrap()))
    }

    fn nd6_flags(&self, name: &str, set: u32, clear: u32) -> io::Result<()> {
        let mut r = [0u8; IN6_NDIREQ];
        r[..16].copy_from_slice(&ifreq(name)[..16]);
        if unsafe { ioctl(self.s6.as_raw_fd(), SIOCGIFINFO_IN6, r.as_mut_ptr()) } != 0 {
            return err("SIOCGIFINFO_IN6");
        }
        let f = (u32::from_ne_bytes(r[36..40].try_into().unwrap()) | set) & !clear;
        r[36..40].copy_from_slice(&f.to_ne_bytes());
        if unsafe { ioctl(self.s6.as_raw_fd(), SIOCSIFINFO_FLAGS, r.as_mut_ptr()) } != 0 {
            return err("SIOCSIFINFO_FLAGS");
        }
        Ok(())
    }

    /// Add (or refresh the lifetimes of) a DHCPv6 address, as a /128: the
    /// on-link prefix, if any, comes from RAs.
    pub fn add_addr6(&self, name: &str, a: Ipv6Addr, pref: u32, valid: u32) -> io::Result<()> {
        let mut r = [0u8; IN6_ALIASREQ];
        r[..16].copy_from_slice(&ifreq(name)[..16]);
        let sin6 = |r: &mut [u8], at: usize, v: [u8; 16]| {
            r[at] = 28;
            r[at + 1] = libc::AF_INET6 as u8;
            r[at + 8..at + 24].copy_from_slice(&v);
        };
        sin6(&mut r, 16, a.octets());
        sin6(&mut r, 72, [0xff; 16]);
        // in6_addrlifetime at 104: expire, preferred (time_t; the kernel
        // computes them), vltime, pltime.
        r[120..124].copy_from_slice(&valid.to_ne_bytes());
        r[124..128].copy_from_slice(&pref.to_ne_bytes());
        if unsafe { ioctl(self.s6.as_raw_fd(), SIOCAIFADDR_IN6, r.as_mut_ptr()) } != 0 {
            return err("SIOCAIFADDR_IN6");
        }
        Ok(())
    }

    /// An IPv6 address's flags (IN6_IFF_*).
    pub fn addr6_flags(&self, name: &str, a: Ipv6Addr) -> io::Result<u32> {
        let mut r = [0u8; IN6_IFREQ];
        r[..16].copy_from_slice(&ifreq(name)[..16]);
        r[16] = 28;
        r[17] = libc::AF_INET6 as u8;
        r[24..40].copy_from_slice(&a.octets());
        if unsafe { ioctl(self.s6.as_raw_fd(), SIOCGIFAFLAG_IN6, r.as_mut_ptr()) } != 0 {
            return err("SIOCGIFAFLAG_IN6");
        }
        Ok(u32::from_ne_bytes(r[16..20].try_into().unwrap()))
    }

    /// Remove an IPv6 address (SLAAC addresses of a network we left).
    pub fn del_addr6(&self, name: &str, a: Ipv6Addr) -> io::Result<()> {
        let mut r = [0u8; IN6_IFREQ];
        r[..16].copy_from_slice(&ifreq(name)[..16]);
        r[16] = 28;
        r[17] = libc::AF_INET6 as u8;
        r[24..40].copy_from_slice(&a.octets());
        if unsafe { ioctl(self.s6.as_raw_fd(), SIOCDIFADDR_IN6, r.as_mut_ptr()) } != 0 {
            return err("SIOCDIFADDR_IN6");
        }
        Ok(())
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

    /// Put interface `name` in interface group `group`, or take it out (an
    /// interface not in the group, or gone, is not an error).
    pub fn group(&self, name: &str, group: &str, add: bool) -> io::Result<()> {
        let mut r = [0u8; IFGROUPREQ];
        r[..16].copy_from_slice(&ifreq(name)[..16]);
        let g = group.as_bytes();
        r[24..24 + g.len().min(15)].copy_from_slice(&g[..g.len().min(15)]);
        let req = if add { SIOCAIFGROUP } else { SIOCDIFGROUP };
        if unsafe { ioctl(self.s.as_raw_fd(), req, r.as_mut_ptr()) } != 0 {
            let e = io::Error::last_os_error();
            return match e.raw_os_error() {
                Some(libc::EEXIST) if add => Ok(()),
                Some(libc::ENOENT) | Some(libc::ENXIO) if !add => Ok(()),
                _ => Err(e),
            };
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

    /// A point-to-point address (a tunnel's): `a` with `prefix`, its far
    /// end `dst`.
    pub fn add_addr_p2p(&self, name: &str, a: Ipv4Addr, prefix: u8, dst: Ipv4Addr) -> io::Result<()> {
        let mut r = [0u8; 68];
        r[..16].copy_from_slice(&ifreq(name)[..16]);
        r[16..32].copy_from_slice(&sin(a));
        r[32..48].copy_from_slice(&sin(dst));
        r[48..64].copy_from_slice(&sin(Ipv4Addr::from(net_proto::mask(prefix))));
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

    pub fn mtu(&self, name: &str) -> io::Result<u32> {
        let mut r = ifreq(name);
        if unsafe { ioctl(self.s.as_raw_fd(), SIOCGIFMTU, r.as_mut_ptr()) } != 0 {
            return err("SIOCGIFMTU");
        }
        Ok(i32::from_ne_bytes(r[16..20].try_into().unwrap()) as u32)
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
    pub v6: Vec<Ipv6Addr>,
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
            out.push(IfInfo { name: name.clone(), index: 0, mac: None, v4: Vec::new(), v6: Vec::new() });
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
        } else if fam == libc::AF_INET6 {
            let a = unsafe { std::slice::from_raw_parts(sa, 28) };
            let mut o: [u8; 16] = a[8..24].try_into().unwrap();
            // KAME keeps the scope in the second word of link-local addresses.
            if o[0] == 0xfe && o[1] & 0xc0 == 0x80 {
                o[2] = 0;
                o[3] = 0;
            }
            out[idx].v6.push(Ipv6Addr::from(o));
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
pub const RTM_IFANNOUNCE: u8 = 0x11;
pub const RTM_NEWADDR: u8 = 0xc;
pub const RTM_DELADDR: u8 = 0xd;
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

    /// Delete the IPv4 default route, every path of it (no gateway named,
    /// so a multipath default goes whole).
    pub fn del_default(&mut self) -> io::Result<()> {
        self.seq += 1;
        let mut m = vec![0u8; RT_MSGHDR];
        m.extend_from_slice(&sin(Ipv4Addr::UNSPECIFIED));
        m.extend_from_slice(&sin(Ipv4Addr::UNSPECIFIED));
        let len = m.len() as u16;
        m[0..2].copy_from_slice(&len.to_ne_bytes());
        m[2] = RTM_VERSION;
        m[3] = RTM_DELETE;
        m[8..12].copy_from_slice(&RTF_UP.to_ne_bytes());
        m[12..16].copy_from_slice(&(RTA_DST | RTA_NETMASK).to_ne_bytes());
        m[20..24].copy_from_slice(&self.seq.to_ne_bytes());
        let n = unsafe { libc::write(self.s.as_raw_fd(), m.as_ptr().cast(), m.len()) };
        if n != m.len() as isize {
            return err("RTM_DELETE default");
        }
        Ok(())
    }

    /// What the kernel says about interfaces: link state, and interfaces
    /// arriving or going away (a USB Wi-Fi or Ethernet adapter).
    pub fn read_ifinfo(&self) -> Vec<IfEvent> {
        let mut out = Vec::new();
        let mut b = [0u8; 2048];
        loop {
            let n = unsafe { libc::recv(self.s.as_raw_fd(), b.as_mut_ptr().cast(), b.len(), libc::MSG_DONTWAIT) };
            if n <= 0 {
                break;
            }
            let n = n as usize;
            if n < 4 || b[2] != RTM_VERSION {
                continue;
            }
            match b[3] {
                // if_msghdr: msglen u16, version u8, type u8, addrs i32, flags
                // i32, index u16, spare u16, then if_data (type, physical,
                // addrlen, hdrlen, link_state: its 5th byte).
                RTM_IFINFO if n >= 24 => {
                    let idx = u16::from_ne_bytes([b[12], b[13]]);
                    out.push(IfEvent::Link(idx, b[16 + 4] == 2));
                }
                // if_announcemsghdr: msglen u16, version u8, type u8, index
                // u16, name[16], what u16 (0 arrival, 1 departure).
                RTM_IFANNOUNCE if n >= 24 => {
                    let name = &b[6..22];
                    let end = name.iter().position(|&c| c == 0).unwrap_or(16);
                    let Ok(name) = std::str::from_utf8(&name[..end]) else { continue };
                    let what = u16::from_ne_bytes([b[22], b[23]]);
                    out.push(if what == 0 { IfEvent::Arrived(name.to_string()) } else { IfEvent::Departed(name.to_string()) });
                }
                // ifa_msghdr (20 bytes), then the sockaddrs its address
                // bits name, each padded to 8: the netmask (bit 2) and the
                // address (bit 5) are what we keep. getifaddrs(3) needs a
                // sysctl capability mode refuses, so after start-up this is
                // how atrium-net knows the host's addresses.
                t @ (RTM_NEWADDR | RTM_DELADDR) if n >= 20 => {
                    if let Some(e) = parse_addr_msg(&b[..n], t == RTM_NEWADDR) {
                        out.push(e);
                    }
                }
                _ => {}
            }
        }
        out
    }
}

/// An RTM_NEWADDR / RTM_DELADDR message, or `None` if it is not one we can read.
pub fn parse_addr_msg(b: &[u8], add: bool) -> Option<IfEvent> {
    let bits = i32::from_ne_bytes(b.get(4..8)?.try_into().ok()?);
    let index = u16::from_ne_bytes([b[12], b[13]]);
    let mut at = 20;
    let (mut mask, mut addr) = (None, None);
    for bit in 0..8 {
        if bits & (1 << bit) == 0 {
            continue;
        }
        let len = *b.get(at)? as usize;
        let sa = b.get(at..at + len.max(2))?;
        if bit == 2 {
            mask = Some(sa.to_vec());
        }
        if bit == 5 {
            addr = Some(sa.to_vec());
        }
        at += if len == 0 { 8 } else { 1 + ((len - 1) | 7) };
    }
    let sa = addr?;
    let ip: std::net::IpAddr = match sa[1] as i32 {
        libc::AF_INET if sa.len() >= 8 => Ipv4Addr::new(sa[4], sa[5], sa[6], sa[7]).into(),
        libc::AF_INET6 if sa.len() >= 24 => Ipv6Addr::from(<[u8; 16]>::try_from(&sa[8..24]).ok()?).into(),
        _ => return None,
    };
    // The netmask's sockaddr may be cut after its last nonzero byte.
    let off = if ip.is_ipv4() { 4 } else { 8 };
    let width = if ip.is_ipv4() { 4 } else { 16 };
    let prefix = mask.map_or(if ip.is_ipv4() { 32 } else { 128 }, |m| (0..width).map(|i| m.get(off + i).copied().unwrap_or(0).count_ones() as u8).sum());
    Some(IfEvent::Addr { index, add, addr: ip, prefix })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IfEvent {
    Link(u16, bool),
    Arrived(String),
    Departed(String),
    Addr { index: u16, add: bool, addr: std::net::IpAddr, prefix: u8 },
}


// ---- bpf -----------------------------------------------------------------

#[repr(C)]
struct BpfInsn {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}


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
        let insns: Vec<BpfInsn> = crate::wrap::RX_FILTER.iter().map(|&(code, jt, jf, k)| BpfInsn { code, jt, jf, k }).collect();
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

/// A directory descriptor, opened before capability mode, through which
/// atrium-net reaches sockets and files by name afterwards.
pub struct Dir {
    fd: OwnedFd,
}

impl Dir {
    pub fn open(path: &str) -> io::Result<Dir> {
        let d = CString::new(path).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let fd = unsafe { libc::open(d.as_ptr(), libc::O_DIRECTORY | libc::O_CLOEXEC) };
        if fd < 0 {
            return err("open directory");
        }
        Ok(Dir { fd: unsafe { OwnedFd::from_raw_fd(fd) } })
    }

    /// Connect to the Unix socket `name` in this directory.
    pub fn connect(&self, name: &str) -> io::Result<std::os::unix::net::UnixStream> {
        let s = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        if s < 0 {
            return err("socket");
        }
        let s = unsafe { OwnedFd::from_raw_fd(s) };
        let mut sa: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        sa.sun_family = libc::AF_UNIX as _;
        let n = name.as_bytes();
        if n.len() >= sa.sun_path.len() {
            return Err(io::Error::from(io::ErrorKind::InvalidInput));
        }
        for (i, b) in n.iter().enumerate() {
            sa.sun_path[i] = *b as libc::c_char;
        }
        sa.sun_len = std::mem::size_of::<libc::sockaddr_un>() as u8;
        if unsafe { connectat(self.fd.as_raw_fd(), s.as_raw_fd(), (&sa as *const libc::sockaddr_un).cast(), sa.sun_len as libc::socklen_t) } != 0 {
            return err("connectat");
        }
        Ok(std::os::unix::net::UnixStream::from(s))
    }

    pub fn read(&self, name: &str, max: usize) -> io::Result<Vec<u8>> {
        let n = CString::new(name).unwrap();
        let fd = unsafe { libc::openat(self.fd.as_raw_fd(), n.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW) };
        if fd < 0 {
            return err("openat");
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut out = Vec::new();
        let mut b = [0u8; 4096];
        loop {
            let r = unsafe { libc::read(fd.as_raw_fd(), b.as_mut_ptr().cast(), b.len()) };
            if r < 0 {
                return err("read");
            }
            if r == 0 {
                break;
            }
            out.extend_from_slice(&b[..r as usize]);
            if out.len() > max {
                return Err(io::Error::other("file too large"));
            }
        }
        Ok(out)
    }

    /// Replace `name` atomically (write a temporary, fsync, rename), mode 0600.
    pub fn write(&self, name: &str, data: &[u8]) -> io::Result<()> {
        let tmp = CString::new(format!("{name}.tmp")).unwrap();
        let fin = CString::new(name).unwrap();
        let d = self.fd.as_raw_fd();
        let fd = unsafe { libc::openat(d, tmp.as_ptr(), libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC | libc::O_CLOEXEC | libc::O_NOFOLLOW, 0o600 as libc::c_uint) };
        if fd < 0 {
            return err("openat");
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let n = unsafe { libc::write(fd.as_raw_fd(), data.as_ptr().cast(), data.len()) };
        if n != data.len() as isize || unsafe { libc::fsync(fd.as_raw_fd()) } != 0 {
            return err("write");
        }
        drop(fd);
        if unsafe { libc::renameat(d, tmp.as_ptr(), d, fin.as_ptr()) } != 0 {
            return err("renameat");
        }
        Ok(())
    }
}

/// local-unbound's control socket, reached through a directory descriptor
/// so it still works in capability mode.
pub struct Unbound {
    dir: Dir,
    name: String,
}

impl Unbound {
    pub fn new(dir: &str, name: &str) -> io::Result<Unbound> {
        Ok(Unbound { dir: Dir::open(dir)?, name: name.to_string() })
    }

    /// One control command; returns unbound's answer.
    pub fn cmd(&self, c: &str) -> io::Result<String> {
        use std::io::{Read, Write};
        let mut s = self.dir.connect(&self.name)?;
        s.write_all(format!("UBCT1 {c}\n").as_bytes())?;
        let mut out = Vec::new();
        s.take(65536).read_to_end(&mut out)?;
        Ok(String::from_utf8_lossy(&out).into_owned())
    }
}

/// Set an integer sysctl (before capability mode).
pub fn sysctl_int(name: &str, v: i32) -> io::Result<()> {
    let n = CString::new(name).map_err(|_| io::Error::other("bad sysctl name"))?;
    let r = unsafe { libc::sysctlbyname(n.as_ptr(), std::ptr::null_mut(), std::ptr::null_mut(), (&v as *const i32).cast(), 4) };
    if r != 0 {
        return err(name);
    }
    Ok(())
}

pub fn cap_enter() -> io::Result<()> {
    if unsafe { libc::cap_enter() } != 0 {
        return err("cap_enter");
    }
    Ok(())
}
