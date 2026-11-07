//! The Bluetooth HID hand-off (docs/spec/bluetooth.md §10).
//!
//! `vinculumd` listens on `/atrium/sockets/bluetooth-hid/hid.sock`
//! (directory `_vinculum:_tactus` 0750, peer uid `_tactus` checked);
//! tactusd connects once. The **control stream** carries [`ToVinculum`] and
//! [`ToTactus`] messages, each `u32 LE length | postcard`, at most
//! [`MAX_MESSAGE`] bytes. An [`ToTactus::Attach`] carries one fd (SCM_RIGHTS,
//! [`send_with_fd`] / [`recv_with_fd`]): one end of a `SOCK_SEQPACKET`
//! socketpair for that device, like a `hidraw` node.
//!
//! Each **packet on a device fd** is a report: a [`ReportHeader`] (10 bytes)
//! and the report bytes as `hidraw` would give them, without the report id
//! byte. Input packets are in order; when tactusd falls behind, vinculumd
//! drops the oldest and sends a `Dropped { count }` packet so held keys can
//! be released.
//!
//! This crate has no dependency on the rest of Vinculum so tactusd can use
//! it directly.
#![deny(unsafe_op_in_unsafe_fn)]

use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;
pub const MAX_MESSAGE: usize = 4096;
/// Descriptor bytes carried in one message (Attach or Descriptor).
pub const DESCRIPTOR_CHUNK: usize = 3072;
/// HID report descriptors are at most this long (§10).
pub const MAX_DESCRIPTOR: usize = 64 * 1024;
/// Largest packet on a device fd: header plus report.
pub const MAX_PACKET: usize = HEADER_LEN + 4096;
pub const HEADER_LEN: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToVinculum {
    Hello { version: u32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Transport {
    Classic,
    Le,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub name: String,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToTactus {
    /// A device is ready; its fd travels with this message. `descriptor`
    /// holds the first [`DESCRIPTOR_CHUNK`] bytes of `descriptor_len`; the
    /// rest follow as `Descriptor` messages before any other message for
    /// this device. `keyboard_allowed`: the bond may carry keyboard reports
    /// (§7); when false, Tactus takes only pointer and consumer usages.
    Attach {
        device: u64,
        record: Record,
        transport: Transport,
        keyboard_allowed: bool,
        descriptor_len: u32,
        descriptor: Vec<u8>,
    },
    Descriptor { device: u64, offset: u32, bytes: Vec<u8> },
    /// The link is gone or the device was removed; its fd also reaches EOF.
    Detach { device: u64 },
    Battery { device: u64, percent: u8 },
}

impl ToTactus {
    /// An Attach and its follow-up Descriptor chunks for a descriptor of any
    /// length up to [`MAX_DESCRIPTOR`].
    pub fn attach(device: u64, record: Record, transport: Transport, keyboard_allowed: bool, descriptor: &[u8]) -> Vec<ToTactus> {
        assert!(descriptor.len() <= MAX_DESCRIPTOR);
        let mut chunks = descriptor.chunks(DESCRIPTOR_CHUNK);
        let first = chunks.next().unwrap_or(&[]).to_vec();
        let mut v = vec![ToTactus::Attach {
            device,
            record,
            transport,
            keyboard_allowed,
            descriptor_len: descriptor.len() as u32,
            descriptor: first,
        }];
        let mut off = DESCRIPTOR_CHUNK;
        for c in chunks {
            v.push(ToTactus::Descriptor { device, offset: off as u32, bytes: c.to_vec() });
            off += c.len();
        }
        v
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    Input = 1,
    Output = 2,
    FeatureGet = 3,
    FeatureSet = 4,
    FeatureReply = 5,
    /// Payload: u32 LE count of Input packets dropped.
    Dropped = 6,
}

impl Kind {
    pub fn from_u8(v: u8) -> Option<Kind> {
        Some(match v {
            1 => Kind::Input,
            2 => Kind::Output,
            3 => Kind::FeatureGet,
            4 => Kind::FeatureSet,
            5 => Kind::FeatureReply,
            6 => Kind::Dropped,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportHeader {
    pub kind: Kind,
    /// 0 when the descriptor has no report ids.
    pub report_id: u8,
    /// CLOCK_MONOTONIC when vinculumd received the ACL packet (Input only).
    pub ts_ns: u64,
}

/// One device-fd packet.
pub fn packet(h: ReportHeader, report: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(HEADER_LEN + report.len());
    v.push(h.kind as u8);
    v.push(h.report_id);
    v.extend_from_slice(&h.ts_ns.to_le_bytes());
    v.extend_from_slice(report);
    v
}

/// Splits a device-fd packet; None if malformed.
pub fn parse_packet(p: &[u8]) -> Option<(ReportHeader, &[u8])> {
    if p.len() < HEADER_LEN || p.len() > MAX_PACKET {
        return None;
    }
    let kind = Kind::from_u8(p[0])?;
    let ts_ns = u64::from_le_bytes(p[2..10].try_into().ok()?);
    Some((ReportHeader { kind, report_id: p[1], ts_ns }, &p[HEADER_LEN..]))
}

/// One control-stream message, framed.
pub fn encode<T: Serialize>(m: &T) -> Vec<u8> {
    let body = postcard::to_stdvec(m).expect("serialize");
    assert!(body.len() <= MAX_MESSAGE, "message over MAX_MESSAGE");
    let mut out = (body.len() as u32).to_le_bytes().to_vec();
    out.extend_from_slice(&body);
    out
}

#[derive(Debug, PartialEq, Eq)]
pub enum WireError {
    TooLong,
    Malformed,
}

/// Accumulates the control stream and yields whole messages.
#[derive(Debug, Default)]
pub struct Reader {
    buf: Vec<u8>,
}

impl Reader {
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    pub fn next<T: for<'de> Deserialize<'de>>(&mut self) -> Result<Option<T>, WireError> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let n = u32::from_le_bytes(self.buf[..4].try_into().expect("4")) as usize;
        if n > MAX_MESSAGE {
            return Err(WireError::TooLong);
        }
        if self.buf.len() < 4 + n {
            return Ok(None);
        }
        let m = postcard::from_bytes(&self.buf[4..4 + n]).map_err(|_| WireError::Malformed)?;
        self.buf.drain(..4 + n);
        Ok(Some(m))
    }
}

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

/// Sends `bytes` on a stream socket with `fd` attached (SCM_RIGHTS), all in
/// one sendmsg so the fd arrives with the first byte of the message.
pub fn send_with_fd(sock: &UnixStream, bytes: &[u8], fd: RawFd) -> std::io::Result<()> {
    let mut iov = libc::iovec { iov_base: bytes.as_ptr() as *mut libc::c_void, iov_len: bytes.len() };
    // SAFETY: CMSG_SPACE is a pure size computation.
    let space = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as usize;
    let mut cbuf = vec![0u8; space];
    // SAFETY: an all-zero msghdr is a valid empty header.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    // SAFETY: msg_control points at `space` bytes, enough for one cmsg
    // holding one fd, so CMSG_FIRSTHDR is non-null and its data area holds
    // a RawFd.
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<RawFd>(), fd);
    }
    // SAFETY: msg and everything it points to are valid for the call.
    let n = unsafe { libc::sendmsg(sock.as_raw_fd(), &msg, 0) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if n as usize != bytes.len() {
        return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "short sendmsg with fd"));
    }
    Ok(())
}

/// Reads up to `buf.len()` bytes, collecting any fds that arrive (at most
/// 4 per call; extras are closed).
pub fn recv_with_fd(sock: &UnixStream, buf: &mut [u8]) -> std::io::Result<(usize, Vec<OwnedFd>)> {
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
    // SAFETY: pure size computation.
    let space = unsafe { libc::CMSG_SPACE((4 * std::mem::size_of::<RawFd>()) as u32) } as usize;
    let mut cbuf = vec![0u8; space];
    // SAFETY: an all-zero msghdr is a valid empty header.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = space as _;
    #[cfg(target_os = "linux")]
    let flags = libc::MSG_CMSG_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = libc::MSG_CMSG_CLOEXEC;
    // SAFETY: msg points at valid buffers for the call.
    let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut msg, flags) };
    if n < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut fds = Vec::new();
    // SAFETY: walking the control buffer the kernel filled, with the
    // CMSG_* macros bounded by msg_controllen.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c).cast::<RawFd>();
                let count = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / std::mem::size_of::<RawFd>();
                for i in 0..count {
                    fds.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(i))));
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    Ok((n as usize, fds))
}

/// A SOCK_SEQPACKET socketpair (close-on-exec): one end per device.
pub fn seqpacket_pair() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as RawFd; 2];
    #[cfg(target_os = "linux")]
    let ty = libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let ty = libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC;
    // SAFETY: fds is a valid out-array of two ints.
    if unsafe { libc::socketpair(libc::AF_UNIX, ty, 0, fds.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: socketpair returned two fresh fds we now own.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn attach_chunks_and_packets() {
        let desc: Vec<u8> = (0..7000u32).map(|i| i as u8).collect();
        let rec = Record { name: "kbd".into(), vendor: 1, product: 2, version: 3 };
        let msgs = ToTactus::attach(9, rec, Transport::Classic, true, &desc);
        assert_eq!(msgs.len(), 3);
        let mut got = Vec::new();
        let mut r = Reader::default();
        for m in &msgs {
            r.push(&encode(m));
        }
        while let Some(m) = r.next::<ToTactus>().unwrap() {
            match m {
                ToTactus::Attach { descriptor, descriptor_len, .. } => {
                    assert_eq!(descriptor_len, 7000);
                    got.extend(descriptor);
                }
                ToTactus::Descriptor { offset, bytes, .. } => {
                    assert_eq!(offset as usize, got.len());
                    got.extend(bytes);
                }
                _ => unreachable!(),
            }
        }
        assert_eq!(got, desc);
        let p = packet(ReportHeader { kind: Kind::Input, report_id: 1, ts_ns: 42 }, &[0, 0, 4, 0, 0, 0, 0, 0]);
        let (h, rep) = parse_packet(&p).unwrap();
        assert_eq!((h.kind, h.report_id, h.ts_ns, rep.len()), (Kind::Input, 1, 42, 8));
        assert!(parse_packet(&[9; 10]).is_none());
        assert!(parse_packet(&[1; 5]).is_none());
    }

    #[test]
    fn fd_passing() {
        let (a, b) = UnixStream::pair().unwrap();
        let (mine, theirs) = seqpacket_pair().unwrap();
        send_with_fd(&a, &encode(&ToTactus::Detach { device: 1 }), theirs.as_raw_fd()).unwrap();
        drop(theirs);
        let mut buf = [0u8; 64];
        let (n, fds) = recv_with_fd(&b, &mut buf).unwrap();
        assert_eq!(fds.len(), 1);
        let mut r = Reader::default();
        r.push(&buf[..n]);
        assert_eq!(r.next::<ToTactus>().unwrap(), Some(ToTactus::Detach { device: 1 }));
        // The passed end works: a packet written on ours arrives there whole.
        let mut ours = std::fs::File::from(mine);
        ours.write_all(&packet(ReportHeader { kind: Kind::Input, report_id: 0, ts_ns: 1 }, &[7])).unwrap();
        let mut got = [0u8; 64];
        let k = unsafe { libc::read(fds[0].as_raw_fd(), got.as_mut_ptr().cast(), got.len()) };
        assert_eq!(k, 11);
    }
}
