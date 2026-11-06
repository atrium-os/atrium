//! arca-courierd: Arca's courier launcher (arca.md §5.2).
//!
//! An app asks for a courier to one https endpoint. The launcher, not the
//! app, decides everything about it:
//!
//! - **Who:** the app is the kernel's peer uid resolved through the launch
//!   registry, as arcad does it. The courier carries that app's endpoint
//!   persona (`arcad::courier::app_persona`), so it can fill only items
//!   the app stored for this endpoint.
//! - **Where:** the host is resolved here, once, to IPv4 addresses the
//!   address policy admits (no loopback, LAN or metadata endpoint). The
//!   courier's jail gets a pf anchor that passes only those addresses on
//!   that port, and the courier is told the same addresses.
//! - **As whom:** a fresh uid from the courier range, labelled in the
//!   courier registry before the courier starts and retired when it exits.
//!
//! The app gets one end of a socketpair; the courier has the other as its
//! stdin and stdout. Nothing else of the courier's is the app's to touch.
//!
//! Wire: the app sends `u32 LE len | u8 scheme (1 https) | str host | u16
//! port` (arcad's endpoint encoding) and reads one status byte; on
//! [`reply::OK`] the courier's socket rides on that byte (SCM_RIGHTS).

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;

use arca_store::codec::{Reader, Writer};
use arca_store::Recipient;
use arcad::courier::{UID_FIRST, UID_LAST};
use navigator_fetch::addr;

#[cfg(target_os = "freebsd")]
pub mod jail;

pub const DEFAULT_SOCKET: &str = "/atrium/sockets/arca-courier/courierd.sock";

/// Most live couriers one app may hold: a courier is a jail, so an app
/// that could ask for any number could take the range from every other.
pub const PER_APP: usize = 16;

/// Most addresses one courier's anchor passes.
pub const MAX_ADDRS: usize = 8;

/// Largest request frame (an endpoint is at most a few hundred bytes).
const MAX_REQUEST: usize = 512;

/// The status byte.
pub mod reply {
    pub const OK: u8 = 0;
    /// The caller is not a launched app with a verified publisher.
    pub const NOT_AN_APP: u8 = 1;
    /// Not an https endpoint Arca can label.
    pub const BAD_ENDPOINT: u8 = 2;
    /// The host has no address the policy admits.
    pub const NO_ADDRESS: u8 = 3;
    /// The app holds [`super::PER_APP`] couriers, or the range is spent.
    pub const TOO_MANY: u8 = 4;
    /// The jail could not be made.
    pub const FAILED: u8 = 5;
}

pub fn encode_request(host: &str, port: u16) -> Vec<u8> {
    let mut w = Writer::new();
    arcad::proto::write_endpoint(&mut w, host, port);
    let body = w.buf;
    let mut out = (body.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(&body);
    out
}

/// The endpoint an app asked for, or `None` for anything out of shape.
pub fn decode_request(body: &[u8]) -> Option<(String, u16)> {
    let mut r = Reader::new(body);
    let ep = arcad::proto::read_endpoint(&mut r).ok()?;
    r.end().ok()?;
    match ep {
        Recipient::Endpoint { host, port, partition: None, .. } => Some((host, port)),
        _ => None,
    }
}

pub fn read_request(s: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len)?;
    let n = u32::from_le_bytes(len) as usize;
    if n > MAX_REQUEST {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "request too long"));
    }
    let mut b = vec![0u8; n];
    s.read_exact(&mut b)?;
    Ok(b)
}

/// The addresses a courier may reach: the resolved ones the policy admits,
/// IPv4 only (jaild's routed jails are IPv4), each once, at most
/// [`MAX_ADDRS`]. One refused address does not refuse the rest; the
/// anchor simply never passes it.
pub fn admit(resolved: impl IntoIterator<Item = SocketAddr>, policy: addr::Policy) -> Vec<Ipv4Addr> {
    let mut out: Vec<Ipv4Addr> = Vec::new();
    for a in resolved {
        let IpAddr::V4(ip) = a.ip() else { continue };
        if policy.admit(IpAddr::V4(ip)).is_ok() && !out.contains(&ip) && out.len() < MAX_ADDRS {
            out.push(ip);
        }
    }
    out
}

/// Courier uids in use, and whose.
#[derive(Default)]
pub struct Pool {
    live: BTreeMap<u32, String>,
}

impl Pool {
    /// The lowest free uid for `owner` (an app's key), or the refusal.
    pub fn take(&mut self, owner: &str) -> Result<u32, u8> {
        if self.live.values().filter(|o| *o == owner).count() >= PER_APP {
            return Err(reply::TOO_MANY);
        }
        let uid = (UID_FIRST..=UID_LAST).find(|u| !self.live.contains_key(u)).ok_or(reply::TOO_MANY)?;
        self.live.insert(uid, owner.to_string());
        Ok(uid)
    }

    /// Only after the uid's label is retired.
    pub fn give_back(&mut self, uid: u32) {
        self.live.remove(&uid);
    }

    pub fn len(&self) -> usize {
        self.live.len()
    }

    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }
}

/// Start the registry over, in place: at launcher start no courier is
/// live. The file is truncated, never replaced, because arcad's jail has
/// it nullfs-mounted and would keep the old one.
pub fn reset_registry(path: &str) -> io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let f = std::fs::OpenOptions::new().write(true).create(true).mode(0o644).open(path)?;
    f.set_len(0)
}

/// Send the status byte, with `fd` attached when given.
pub fn send_reply(s: &UnixStream, status: u8, fd: Option<BorrowedFd<'_>>) -> io::Result<()> {
    let mut b = [status];
    let mut iov = libc::iovec { iov_base: b.as_mut_ptr().cast(), iov_len: 1 };
    // SAFETY: CMSG_SPACE is a pure size computation.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as u32) } as usize;
    let mut cbuf = vec![0u8; space];
    // SAFETY: zeroed msghdr is a valid empty message.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if let Some(fd) = fd {
        msg.msg_control = cbuf.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        // SAFETY: msg_control points at `space` bytes, room for one fd.
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as u32) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<libc::c_int>(), fd.as_raw_fd());
        }
    }
    // SAFETY: msg is fully initialised above.
    let n = unsafe { libc::sendmsg(s.as_raw_fd(), &msg, 0) };
    if n == 1 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Read the status byte and any descriptor with it.
pub fn recv_reply(s: &UnixStream) -> io::Result<(u8, Option<OwnedFd>)> {
    let mut b = [0u8];
    let mut iov = libc::iovec { iov_base: b.as_mut_ptr().cast(), iov_len: 1 };
    // SAFETY: CMSG_SPACE is a pure size computation.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as u32) } as usize;
    let mut cbuf = vec![0u8; space];
    // SAFETY: zeroed msghdr is a valid empty message.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: buffers outlive the call.
    let n = unsafe { libc::recvmsg(s.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if n != 1 {
        return Err(if n == 0 { io::ErrorKind::UnexpectedEof.into() } else { io::Error::last_os_error() });
    }
    let mut fd = None;
    // SAFETY: walking the control buffer the kernel filled.
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        if !c.is_null() && (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
            let raw = std::ptr::read_unaligned(libc::CMSG_DATA(c).cast::<libc::c_int>());
            fd = Some(OwnedFd::from_raw_fd(raw));
        }
    }
    Ok((b[0], fd))
}

/// The app's side: a courier to `host:port`, as a stream of courier frames
/// (`arca_courier::schema`), or the launcher's status byte.
pub fn open(socket: &str, host: &str, port: u16) -> Result<UnixStream, u8> {
    let mut s = UnixStream::connect(socket).map_err(|_| reply::FAILED)?;
    s.write_all(&encode_request(host, port)).map_err(|_| reply::FAILED)?;
    match recv_reply(&s) {
        Ok((reply::OK, Some(fd))) => Ok(UnixStream::from(fd)),
        Ok((reply::OK, None)) => Err(reply::FAILED),
        Ok((code, _)) => Err(code),
        Err(_) => Err(reply::FAILED),
    }
}
