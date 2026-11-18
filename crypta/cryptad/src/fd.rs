//! Descriptors over Unix sockets (SCM_RIGHTS): the tun from atrium-net, a
//! `.conf` from whoever asks for an import.

use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

const CMSG_SPACE: usize = 64;

/// `data` with `fds` (at most 4) attached to its first byte; how much of
/// `data` went out.
pub fn send(s: RawFd, data: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    assert!(fds.len() <= 4 && !data.is_empty());
    let mut iov = libc::iovec { iov_base: data.as_ptr() as *mut libc::c_void, iov_len: data.len() };
    let mut cbuf = [0u64; CMSG_SPACE / 8];
    // SAFETY: a zeroed msghdr is valid; filled below with live buffers.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if !fds.is_empty() {
        let n = (std::mem::size_of::<RawFd>() * fds.len()) as u32;
        // SAFETY: the control buffer is large enough for 4 descriptors.
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
    // SAFETY: msg points at live buffers.
    let r = unsafe { libc::sendmsg(s, &msg, 0) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(r as usize)
}

/// Read into `buf`, keeping descriptors that came along (at most 4).
pub fn recv(s: RawFd, buf: &mut [u8]) -> io::Result<(usize, Vec<OwnedFd>)> {
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
    let mut cbuf = [0u64; CMSG_SPACE / 8];
    // SAFETY: as in send.
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
    let mut fds = Vec::new();
    // SAFETY: walking the control message the kernel filled.
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
