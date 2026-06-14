//! Passing an open fd to a peer over a Unix socket (`SCM_RIGHTS`) — the kernel
//! duplicates the fd into the receiver. lyrad uses it to hand a data-plane ring's
//! anonymous-shm fd to a source: the fd is the capability to write that stream.

use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

/// Send `fd` over the connected socket `sock` (with one filler payload byte —
/// some platforms drop a control message that carries no data).
pub fn send_fd(sock: RawFd, fd: RawFd) -> io::Result<()> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec { iov_base: byte.as_mut_ptr() as *mut _, iov_len: 1 };
    let mut cmsgbuf = [0u8; 64];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsgbuf.as_mut_ptr() as *mut _;
    msg.msg_controllen = unsafe { libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) } as _;
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::copy_nonoverlapping(&fd, libc::CMSG_DATA(cmsg) as *mut RawFd, 1);
        if libc::sendmsg(sock, &msg, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Receive one fd from the connected socket `sock`.
pub fn recv_fd(sock: RawFd) -> io::Result<OwnedFd> {
    let mut byte = [0u8; 1];
    let mut iov = libc::iovec { iov_base: byte.as_mut_ptr() as *mut _, iov_len: 1 };
    let mut cmsgbuf = [0u8; 64];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsgbuf.as_mut_ptr() as *mut _;
    msg.msg_controllen = cmsgbuf.len() as _;
    unsafe {
        if libc::recvmsg(sock, &mut msg, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null()
            || (*cmsg).cmsg_level != libc::SOL_SOCKET
            || (*cmsg).cmsg_type != libc::SCM_RIGHTS
        {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "no fd in message"));
        }
        let mut fd: RawFd = -1;
        std::ptr::copy_nonoverlapping(libc::CMSG_DATA(cmsg) as *const RawFd, &mut fd, 1);
        if fd < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad fd received"));
        }
        Ok(OwnedFd::from_raw_fd(fd))
    }
}
