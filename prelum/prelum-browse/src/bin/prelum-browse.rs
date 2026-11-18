//! atrium-prelum-browse: the mDNS printer finder for prelumd (printing.md
//! §7.1).
//!
//! Started by prelumd through jaild in a jail with no network, only while
//! someone is looking at the printer list. Its stdin and stdout are one
//! socket to prelumd, which first sends one byte carrying the two mDNS
//! sockets atrium-net made (`prelum_proto::browse`). Then it enters
//! capability mode and runs [`prelum_browse::run`].
//!
//!   atrium-prelum-browse [--legacy]
//!
//! `--legacy`: the sockets are one socket connected to a responder (RFC 6762
//! §6.7 legacy unicast, development only); its answers carry TTLs of at most
//! 10 s, so questions are repeated every 5 s.

use std::io;
use std::net::UdpSocket;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::Duration;

/// One byte and up to two descriptors from `s`.
fn recv_fds(s: RawFd) -> io::Result<Vec<OwnedFd>> {
    let mut byte = 0u8;
    let mut iov = libc::iovec { iov_base: (&mut byte as *mut u8).cast(), iov_len: 1 };
    let mut cbuf = [0u64; 8];
    // SAFETY: a zeroed msghdr is valid; every pointer set below outlives the call.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf.as_mut_ptr().cast();
    msg.msg_controllen = std::mem::size_of_val(&cbuf) as _;
    // SAFETY: msg points at live buffers of the sizes given.
    let n = unsafe { libc::recvmsg(s, &mut msg, 0) };
    if n != 1 {
        return Err(if n == 0 { io::ErrorKind::UnexpectedEof.into() } else { io::Error::last_os_error() });
    }
    let mut fds = Vec::new();
    // SAFETY: the kernel filled the control buffer; the CMSG macros walk it
    // within msg_controllen, and each descriptor it carries is now ours.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c) as *const RawFd;
                let n = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / std::mem::size_of::<RawFd>();
                for i in 0..n {
                    fds.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(i))));
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
    }
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::other("too many descriptors"));
    }
    Ok(fds)
}

#[cfg(target_os = "freebsd")]
fn capsicum() -> io::Result<()> {
    extern "C" {
        fn cap_enter() -> libc::c_int;
    }
    // SAFETY: no arguments; only changes this process's mode.
    if unsafe { cap_enter() } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "freebsd"))]
fn capsicum() -> io::Result<()> {
    Ok(())
}

fn main() {
    let legacy = match std::env::args().nth(1).as_deref() {
        None => false,
        Some("--legacy") => true,
        Some(a) => {
            eprintln!("prelum-browse: unknown argument {a}");
            std::process::exit(2);
        }
    };
    let go = || -> io::Result<prelum_browse::Stats> {
        let mut fds = recv_fds(0)?;
        let (recv, send) = match fds.len() {
            2 => {
                let send = fds.pop().unwrap();
                (fds.pop().unwrap(), send)
            }
            n => return Err(io::Error::other(format!("{n} sockets, not 2"))),
        };
        // SAFETY: fd 0 is ours, given by our parent; owned once, here.
        let ctl = unsafe { UnixStream::from_raw_fd(0) };
        capsicum().map_err(|e| io::Error::other(format!("cap_enter: {e}")))?;
        let mut o = prelum_browse::Opts::default();
        if legacy {
            o.max_interval = Duration::from_secs(5);
        }
        prelum_browse::run(ctl, UdpSocket::from(recv), UdpSocket::from(send), &o)
    };
    if let Err(e) = go() {
        eprintln!("prelum-browse: {e}");
        std::process::exit(1);
    }
}
