//! SCM_RIGHTS file-descriptor passing over connected SOCK_STREAM
//! Unix-domain sockets. Used for handing the requesting client's
//! stdin/stdout/stderr (or pty fd) to portcullisd, which then
//! attaches them to the launched jail.
//!
//! Wire shape: a single sendmsg with a 1-byte data payload (kernel
//! requires at least 1 byte of normal data to carry the ancillary
//! message) plus an SCM_RIGHTS cmsg containing the fd array. The
//! receiver does the matching recvmsg; the kernel duplicates the
//! fds into the receiver's process and the new fd numbers come
//! back via CMSG_DATA.
//!
//! Why not the `passfd` / `sendfd` crates: the operation is small
//! and we want to keep the dependency graph minimal. ~80 lines of
//! libc beats a transitive crate.
//!
//! BufReader interaction: see `protocol.rs` — the Launch path uses
//! a `ReadyForFds` round-trip so the daemon's read buffer is
//! guaranteed empty before `recv_fds` runs. Without that, BufReader
//! could swallow the 1-byte payload via plain `read()`, which
//! drops the cmsg silently.

use std::io;
use std::mem;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// Send `fds` over `stream`. The kernel requires at least one byte
/// of normal data to carry the cmsg; we send a literal 'F' byte
/// that the receiver consumes and discards.
pub fn send_fds<S: AsRawFd>(stream: &S, fds: &[RawFd]) -> io::Result<()> {
    let payload: u8 = b'F';
    let mut iov = libc::iovec {
        iov_base: &payload as *const u8 as *mut _,
        iov_len:  1,
    };

    /* Allocate the cmsg buffer. CMSG_SPACE includes the cmsghdr +
     * data + alignment padding; we lay out N RawFds (i32) inside. */
    let cmsg_space = unsafe { libc::CMSG_SPACE((fds.len() * mem::size_of::<RawFd>()) as u32) } as usize;
    let mut cmsg_buf = vec![0u8; cmsg_space];

    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov     = &mut iov;
    msg.msg_iovlen  = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut _;
    msg.msg_controllen = cmsg_space as _;

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(io::Error::other("CMSG_FIRSTHDR returned null"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type  = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len   = libc::CMSG_LEN((fds.len() * mem::size_of::<RawFd>()) as u32) as _;

        let data_ptr = libc::CMSG_DATA(cmsg) as *mut RawFd;
        std::ptr::copy_nonoverlapping(fds.as_ptr(), data_ptr, fds.len());

        let sent = libc::sendmsg(stream.as_raw_fd(), &msg, 0);
        if sent < 0 {
            return Err(io::Error::last_os_error());
        }
        if sent != 1 {
            return Err(io::Error::other(
                format!("partial sendmsg: wrote {sent} of 1 byte")));
        }
    }
    Ok(())
}

/// Receive up to `max_fds` file descriptors. Returns OwnedFds so
/// the caller controls lifetime; the kernel has dup'd into our
/// process so closing them on this side is safe and necessary.
///
/// Discards the 1-byte data payload (matching `send_fds`'s 'F').
pub fn recv_fds<S: AsRawFd>(stream: &S, max_fds: usize) -> io::Result<Vec<OwnedFd>> {
    let mut payload: u8 = 0;
    let mut iov = libc::iovec {
        iov_base: &mut payload as *mut u8 as *mut _,
        iov_len:  1,
    };

    let cmsg_space = unsafe { libc::CMSG_SPACE((max_fds * mem::size_of::<RawFd>()) as u32) } as usize;
    let mut cmsg_buf = vec![0u8; cmsg_space];

    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov     = &mut iov;
    msg.msg_iovlen  = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut _;
    msg.msg_controllen = cmsg_space as _;

    unsafe {
        let n = libc::recvmsg(stream.as_raw_fd(), &mut msg, 0);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof,
                       "peer closed before sending fds"));
        }
        if (msg.msg_flags & libc::MSG_CTRUNC) != 0 {
            return Err(io::Error::other(
                "ancillary data truncated — buffer too small for the fd array"));
        }

        let mut out = Vec::with_capacity(max_fds);
        let mut cmsg = libc::CMSG_FIRSTHDR(&msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::SOL_SOCKET && (*cmsg).cmsg_type == libc::SCM_RIGHTS {
                /* cmsg_len includes the cmsghdr + alignment; the data
                 * portion is what's left after subtracting CMSG_LEN(0). */
                let data_len = (*cmsg).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                let n_fds = data_len / mem::size_of::<RawFd>();
                let data_ptr = libc::CMSG_DATA(cmsg) as *const RawFd;
                for i in 0..n_fds {
                    let fd = *data_ptr.add(i);
                    out.push(OwnedFd::from_raw_fd(fd));
                }
            }
            cmsg = libc::CMSG_NXTHDR(&msg, cmsg);
        }
        Ok(out)
    }
}

// ── tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;

    #[test]
    fn round_trip_three_fds_via_pipes() {
        let (a, b) = UnixStream::pair().unwrap();

        /* Make 3 pipe pairs; send the read ends across the socket;
         * write a known byte into each write end on the sender side
         * and verify it reads back on the receiver-fd side. */
        let mut readers: Vec<std::os::unix::io::OwnedFd> = Vec::new();
        let mut writers: Vec<std::os::unix::io::OwnedFd> = Vec::new();
        for _ in 0..3 {
            let mut p = [0; 2];
            let r = unsafe { libc::pipe(p.as_mut_ptr()) };
            assert_eq!(r, 0);
            unsafe {
                readers.push(OwnedFd::from_raw_fd(p[0]));
                writers.push(OwnedFd::from_raw_fd(p[1]));
            }
        }
        let raw_readers: Vec<RawFd> = readers.iter().map(|f| f.as_raw_fd()).collect();
        send_fds(&a, &raw_readers).unwrap();

        let received = recv_fds(&b, 3).unwrap();
        assert_eq!(received.len(), 3);

        /* Write tag bytes into the original write-ends and read them
         * back via the dup'd read-ends to prove they're the same
         * pipes. */
        for (i, w) in writers.iter().enumerate() {
            let mut wf: std::fs::File = w.try_clone().unwrap().into();
            wf.write_all(&[0xA0 + i as u8]).unwrap();
        }
        for (i, r) in received.iter().enumerate() {
            let mut rf: std::fs::File = r.try_clone().unwrap().into();
            let mut buf = [0u8; 1];
            rf.read_exact(&mut buf).unwrap();
            assert_eq!(buf[0], 0xA0 + i as u8, "fd {i} mismatch");
        }
    }
}
