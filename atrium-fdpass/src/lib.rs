//! Descriptor passing over Unix sockets (SCM_RIGHTS), written once.
//!
//! Every Atrium daemon that takes descriptors from a peer uses this crate
//! rather than its own `recvmsg`. What it guarantees, for any peer:
//!
//! - Each received descriptor is an [`OwnedFd`] from the moment it exists,
//!   so every error path closes it.
//! - Each received descriptor is close-on-exec atomically
//!   (`MSG_CMSG_CLOEXEC` on FreeBSD and Linux). Brokers fork children that
//!   exec into other jails; a descriptor one client sent must never be
//!   inherited by the next request's child. On macOS (host tests only) the
//!   flag is set right after `recvmsg`.
//! - A peer that sends more descriptors than the caller accepts, or more
//!   ancillary data than fits (`MSG_CTRUNC`), gets an error, and every
//!   descriptor that did arrive is closed. Callers drop that peer.
//!
//! The only `unsafe` code is in this file.

use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// Most descriptors one message may carry, on either side. Ancillary space is
/// sized from the caller's limit, so this bounds what a caller can ask for.
pub const MAX_FDS: usize = 64;

#[cfg(any(target_os = "freebsd", target_os = "linux"))]
const RECV_FLAGS: libc::c_int = libc::MSG_CMSG_CLOEXEC;
#[cfg(not(any(target_os = "freebsd", target_os = "linux")))]
const RECV_FLAGS: libc::c_int = 0;

/// One message: `n` bytes in the caller's buffer and the descriptors that came
/// with them (at most the caller's `max_fds`). `n == 0` is end of stream.
#[derive(Debug)]
pub struct Received {
    pub n: usize,
    pub fds: Vec<OwnedFd>,
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

fn space_for(fds: usize) -> usize {
    // SAFETY: CMSG_SPACE is arithmetic on its argument only. `fds` is at
    // most MAX_FDS, so the u32 conversion cannot truncate.
    unsafe { libc::CMSG_SPACE((fds * mem::size_of::<RawFd>()) as u32) as usize }
}

/// One `recvmsg` into `buf`, accepting at most `max_fds` descriptors.
///
/// Errors: `max_fds > MAX_FDS`; truncated ancillary data; more than `max_fds`
/// descriptors. In each case every descriptor received is closed.
///
/// Room for one descriptor is always offered even when `max_fds` is 0: with
/// no control buffer at all macOS discards a peer's descriptors without
/// setting `MSG_CTRUNC`, so a refusal would be invisible. Here it is an error.
pub fn recv(sock: RawFd, buf: &mut [u8], max_fds: usize) -> io::Result<Received> {
    if max_fds > MAX_FDS {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "atrium-fdpass: max_fds above MAX_FDS"));
    }
    let space = space_for(max_fds.max(1));
    // u64 backing keeps the cmsghdr reads below aligned.
    let mut cbuf = vec![0u64; space.div_ceil(8)];
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
    // SAFETY: an all-zero msghdr is a valid empty message; fields set below.
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = space as _;

    let n = loop {
        // SAFETY: msg points at iov, buf and cbuf, all live across the call.
        let n = unsafe { libc::recvmsg(sock, &mut msg, RECV_FLAGS) };
        if n >= 0 {
            break n as usize;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    };

    let mut fds = Vec::new();
    // SAFETY: walks the control buffer the kernel just filled, using the libc
    // macros that bound each header by msg_controllen. Each descriptor is
    // wrapped in an OwnedFd before anything else can fail.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c) as *const RawFd;
                let len = ((*c).cmsg_len as usize).saturating_sub(data as usize - c as usize);
                for i in 0..len / mem::size_of::<RawFd>() {
                    let fd = std::ptr::read_unaligned(data.add(i));
                    fds.push(OwnedFd::from_raw_fd(fd));
                    if RECV_FLAGS == 0 {
                        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                    }
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(bad("atrium-fdpass: peer sent more ancillary data than fits"));
    }
    if fds.len() > max_fds {
        return Err(bad("atrium-fdpass: peer sent more descriptors than accepted"));
    }
    Ok(Received { n, fds })
}

/// Exactly one descriptor riding on one payload byte (the shape
/// [`send_fd`] writes). End of stream, no descriptor, or more than one is an
/// error.
pub fn recv_fd(sock: RawFd) -> io::Result<OwnedFd> {
    let mut byte = [0u8; 1];
    let mut r = recv(sock, &mut byte, 1)?;
    if r.n == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "atrium-fdpass: peer closed before sending a descriptor"));
    }
    r.fds.pop().ok_or_else(|| bad("atrium-fdpass: no descriptor in message"))
}

/// One `sendmsg` of `data` with `fds` attached; how many bytes of `data`
/// went out (the descriptors ride on the first byte). `data` must not be
/// empty, since some platforms drop ancillary data with no payload.
pub fn send(sock: RawFd, data: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    if data.is_empty() || fds.len() > MAX_FDS {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "atrium-fdpass: empty payload or too many descriptors"));
    }
    let mut iov = libc::iovec { iov_base: data.as_ptr() as *mut libc::c_void, iov_len: data.len() };
    // SAFETY: an all-zero msghdr is a valid empty message; fields set below.
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    let space = space_for(fds.len());
    let mut cbuf = vec![0u64; space.div_ceil(8)];
    if !fds.is_empty() {
        msg.msg_control = cbuf.as_mut_ptr().cast();
        msg.msg_controllen = space as _;
        // SAFETY: cbuf holds CMSG_SPACE for fds.len() descriptors, so the
        // first header and its data fit; writes are unaligned-safe.
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&msg);
            if c.is_null() {
                return Err(io::Error::other("atrium-fdpass: no room for the control header"));
            }
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN((fds.len() * mem::size_of::<RawFd>()) as u32) as _;
            let d = libc::CMSG_DATA(c) as *mut RawFd;
            for (i, &f) in fds.iter().enumerate() {
                std::ptr::write_unaligned(d.add(i), f);
            }
        }
    }
    loop {
        // SAFETY: msg points at iov, data and cbuf, all live across the call.
        let r = unsafe { libc::sendmsg(sock, &msg, 0) };
        if r >= 0 {
            return Ok(r as usize);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// One descriptor on one payload byte: the counterpart of [`recv_fd`].
pub fn send_fd(sock: RawFd, fd: RawFd) -> io::Result<()> {
    match send(sock, b"F", &[fd])? {
        1 => Ok(()),
        _ => Err(io::Error::other("atrium-fdpass: descriptor byte not sent")),
    }
}

/// [`send`] for anything with a descriptor.
pub fn send_on<S: AsRawFd>(sock: &S, data: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    send(sock.as_raw_fd(), data, fds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    fn pipe() -> (File, File) {
        let mut p = [0; 2];
        // SAFETY: p has room for the two descriptors pipe(2) writes.
        assert_eq!(unsafe { libc::pipe(p.as_mut_ptr()) }, 0);
        // SAFETY: pipe(2) just returned these and nothing else owns them.
        unsafe { (File::from_raw_fd(p[0]), File::from_raw_fd(p[1])) }
    }

    /// Whether every copy of a pipe's write end is closed: the read end then
    /// reports end of stream instead of "would block".
    fn writers_all_closed(r: &File) -> bool {
        // SAFETY: fcntl on a descriptor `r` owns.
        unsafe { libc::fcntl(r.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
        let mut b = [0u8; 1];
        matches!((&*r).read(&mut b), Ok(0))
    }

    #[test]
    fn descriptors_arrive_owned_and_close_on_exec() {
        let (a, b) = UnixStream::pair().unwrap();
        let (r, mut w) = pipe();
        assert_eq!(send(a.as_raw_fd(), b"hello", &[r.as_raw_fd()]).unwrap(), 5);
        let mut buf = [0u8; 16];
        let got = recv(b.as_raw_fd(), &mut buf, 2).unwrap();
        assert_eq!(&buf[..got.n], b"hello");
        assert_eq!(got.fds.len(), 1);
        let fd = got.fds[0].as_raw_fd();
        // SAFETY: F_GETFD only queries the descriptor table.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert!(flags & libc::FD_CLOEXEC != 0, "received descriptor must be close-on-exec");
        w.write_all(b"x").unwrap();
        let mut f = File::from(got.fds.into_iter().next().unwrap());
        let mut one = [0u8; 1];
        f.read_exact(&mut one).unwrap();
        assert_eq!(&one, b"x");
    }

    /// Two descriptors fit in the padding of a one-descriptor control buffer
    /// on 64-bit Linux and FreeBSD (the count check refuses them); three do
    /// not (the kernel truncates). Either way: an error, and nothing leaks.
    #[test]
    fn more_descriptors_than_accepted_is_an_error_and_all_are_closed() {
        for count in [2, 3, 8] {
            let (a, b) = UnixStream::pair().unwrap();
            let pipes: Vec<_> = (0..count).map(|_| pipe()).collect();
            let writers: Vec<RawFd> = pipes.iter().map(|(_, w)| w.as_raw_fd()).collect();
            send(a.as_raw_fd(), b"F", &writers).unwrap();
            let readers: Vec<File> = pipes.into_iter().map(|(r, _w)| r).collect();
            let mut buf = [0u8; 1];
            let e = recv(b.as_raw_fd(), &mut buf, 1).unwrap_err();
            assert_eq!(e.kind(), io::ErrorKind::InvalidData, "{count} descriptors");
            for r in &readers {
                assert!(writers_all_closed(r), "a refused descriptor stayed open ({count} sent)");
            }
        }
    }

    #[test]
    fn a_connection_that_takes_none_refuses_one() {
        let (a, b) = UnixStream::pair().unwrap();
        let (r, _w) = pipe();
        send(a.as_raw_fd(), b"F", &[r.as_raw_fd()]).unwrap();
        let mut buf = [0u8; 1];
        assert!(recv(b.as_raw_fd(), &mut buf, 0).is_err());
    }

    #[test]
    fn recv_fd_wants_exactly_one() {
        let (a, b) = UnixStream::pair().unwrap();
        let (r, _w) = pipe();
        send_fd(a.as_raw_fd(), r.as_raw_fd()).unwrap();
        recv_fd(b.as_raw_fd()).unwrap();

        // A byte with no descriptor.
        (&a).write_all(b"F").unwrap();
        assert_eq!(recv_fd(b.as_raw_fd()).unwrap_err().kind(), io::ErrorKind::InvalidData);

        // Two descriptors.
        let (r2, _w2) = pipe();
        send(a.as_raw_fd(), b"F", &[r.as_raw_fd(), r2.as_raw_fd()]).unwrap();
        assert!(recv_fd(b.as_raw_fd()).is_err());

        drop(a);
        assert_eq!(recv_fd(b.as_raw_fd()).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn limits_are_checked_before_any_syscall() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut buf = [0u8; 1];
        assert_eq!(recv(b.as_raw_fd(), &mut buf, MAX_FDS + 1).unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert_eq!(send(a.as_raw_fd(), b"", &[]).unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }
}
