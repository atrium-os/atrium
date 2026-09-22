//! `recvmsg` with SCM_RIGHTS — the only unsafe code in this crate.
//!
//! ★★ Every received descriptor is CLOSE-ON-EXEC from the moment it exists.
//! The brokers this crate serves fork children that exec into OTHER jails
//! (jaild pdforks a service per request). A descriptor received for one
//! client and not yet consumed would otherwise be inherited by whatever the
//! next request execs — one client's pipe handed to another jail. FreeBSD and
//! Linux close that window atomically with MSG_CMSG_CLOEXEC; on macOS (tests
//! only) it is set immediately after, which is fine for a single-threaded
//! test process.

use std::io;
use std::mem;
use std::os::unix::io::{FromRawFd, OwnedFd, RawFd};

/// One `recvmsg`: `n` bytes into the buffer, plus any descriptors that rode
/// with them. `control_truncated` means the peer sent more ancillary data
/// than we accept — the kernel has already closed what did not fit, and the
/// caller must treat the connection as broken rather than guess.
pub(crate) struct Received {
    pub n:                 usize,
    pub fds:               Vec<OwnedFd>,
    pub control_truncated: bool,
}

#[cfg(any(target_os = "freebsd", target_os = "linux"))]
const RECV_FLAGS: libc::c_int = libc::MSG_CMSG_CLOEXEC;
#[cfg(not(any(target_os = "freebsd", target_os = "linux")))]
const RECV_FLAGS: libc::c_int = 0;

/// Receive into `buf`, accepting at most `max_fds` descriptors.
///
/// ★ Room for at least ONE descriptor is always offered, even when the caller
/// accepts none. With no control buffer at all, macOS discards a peer's
/// rights WITHOUT setting MSG_CTRUNC — measured — so "this connection takes
/// no descriptors" would be unenforceable. Receiving one makes it visible, and
/// the caller refuses it (the OwnedFd closes it).
#[allow(unsafe_code)]
pub(crate) fn recv(sock: RawFd, buf: &mut [u8], max_fds: usize) -> io::Result<Received> {
    // SAFETY: pure arithmetic macro.
    let space = unsafe {
        libc::CMSG_SPACE((max_fds.max(1) * mem::size_of::<RawFd>()) as u32) as usize
    };
    // u64-aligned backing so the cmsghdr casts below are aligned.
    let mut cbuf = vec![0u64; space.div_ceil(8).max(1)];

    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
    // SAFETY: zeroed msghdr is a valid "empty" value; fields set below.
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = space as _;

    // SAFETY: msg points at live buffers for the duration of the call.
    let n = unsafe { libc::recvmsg(sock, &mut msg, RECV_FLAGS) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }

    let mut fds = Vec::new();
    {
        // SAFETY: walking the control buffer the kernel just filled, with the
        // libc macros that bound each header by msg_controllen.
        unsafe {
            let mut c = libc::CMSG_FIRSTHDR(&msg);
            while !c.is_null() {
                if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                    let data = libc::CMSG_DATA(c) as *const RawFd;
                    let bytes = (*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                    for i in 0..bytes / mem::size_of::<RawFd>() {
                        let fd = std::ptr::read_unaligned(data.add(i));
                        // Owned immediately, so every early return closes it.
                        fds.push(OwnedFd::from_raw_fd(fd));
                        if RECV_FLAGS == 0 {
                            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
                        }
                    }
                }
                c = libc::CMSG_NXTHDR(&msg, c);
            }
        }
    }
    Ok(Received {
        n: n as usize,
        fds,
        control_truncated: msg.msg_flags & libc::MSG_CTRUNC != 0,
    })
}
