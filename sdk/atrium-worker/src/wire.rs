//! The bytes between a core and its worker (see the crate header).

use std::fs::File;
use std::io::{self, Read, Write};
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;

/// Set for a capsicum child: enter capability mode before reading.
pub const CAPSICUM_ENV: &str = "ATRIUM_WORKER_CAPSICUM";

/// `data` (at least a byte) with `fds` in one sendmsg.
pub fn send_with_fds(s: &UnixStream, fds: &[RawFd], data: &[u8]) -> io::Result<()> {
    let mut iov = libc::iovec { iov_base: data.as_ptr() as *mut _, iov_len: data.len() };
    let space = unsafe { libc::CMSG_SPACE((fds.len() * mem::size_of::<RawFd>()) as u32) } as usize;
    let mut buf = vec![0u8; space];
    let mut msg: libc::msghdr = unsafe { mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = buf.as_mut_ptr() as *mut _;
    msg.msg_controllen = space as _;
    unsafe {
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN((fds.len() * mem::size_of::<RawFd>()) as u32) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(c) as *mut RawFd, fds.len());
        let n = libc::sendmsg(s.as_raw_fd(), &msg, flags());
        if n < 0 { return Err(io::Error::last_os_error()) }
        if n as usize != data.len() { return Err(io::Error::other("short sendmsg")) }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn flags() -> libc::c_int { libc::MSG_NOSIGNAL }
#[cfg(not(target_os = "linux"))]
fn flags() -> libc::c_int { 0 }

/// Read exactly `buf.len()` bytes; any descriptors that ride with them are
/// returned (at most one is kept: more is the peer's error).
fn recv_exact(s: &UnixStream, buf: &mut [u8]) -> io::Result<Option<File>> {
    let mut got = 0;
    let mut file = None;
    while got < buf.len() {
        let mut iov = libc::iovec { iov_base: buf[got..].as_mut_ptr() as *mut _, iov_len: buf.len() - got };
        let space = unsafe { libc::CMSG_SPACE((4 * mem::size_of::<RawFd>()) as u32) } as usize;
        let mut cbuf = vec![0u8; space];
        let mut msg: libc::msghdr = unsafe { mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cbuf.as_mut_ptr() as *mut _;
        msg.msg_controllen = space as _;
        let n = unsafe { libc::recvmsg(s.as_raw_fd(), &mut msg, cmsg_cloexec()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted { continue }
            return Err(e);
        }
        unsafe {
            let mut c = libc::CMSG_FIRSTHDR(&msg);
            while !c.is_null() {
                if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                    let len = (*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                    let p = libc::CMSG_DATA(c) as *const RawFd;
                    for i in 0..len / mem::size_of::<RawFd>() {
                        let f = File::from_raw_fd(*p.add(i));
                        if file.is_none() { file = Some(f) }
                    }
                }
                c = libc::CMSG_NXTHDR(&msg, c);
            }
        }
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed"));
        }
        got += n as usize;
    }
    Ok(file)
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn cmsg_cloexec() -> libc::c_int { libc::MSG_CMSG_CLOEXEC }
#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
fn cmsg_cloexec() -> libc::c_int { 0 }

/// A call: flag, length, frame; `fd` rides with the header.
pub fn send_call(s: &UnixStream, frame: &[u8], fd: Option<RawFd>) -> io::Result<()> {
    let len = u32::try_from(frame.len()).map_err(|_| io::Error::other("call too large"))?;
    let mut head = [0u8; 5];
    head[0] = fd.is_some() as u8;
    head[1..].copy_from_slice(&len.to_le_bytes());
    match fd {
        Some(fd) => send_with_fds(s, &[fd], &head)?,
        None => write_all(s, &head)?,
    }
    write_all(s, frame)
}

fn write_all(s: &UnixStream, mut b: &[u8]) -> io::Result<()> {
    while !b.is_empty() {
        let n = unsafe { libc::send(s.as_raw_fd(), b.as_ptr() as *const _, b.len(), flags()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted { continue }
            return Err(e);
        }
        b = &b[n as usize..];
    }
    Ok(())
}

/// An answer: `Ok(frame)` or the worker's `Err(text)`.
pub fn read_answer(s: &mut UnixStream, max: usize) -> io::Result<Result<Vec<u8>, String>> {
    let mut head = [0u8; 5];
    s.read_exact(&mut head)?;
    let len = u32::from_le_bytes(head[1..].try_into().unwrap()) as usize;
    if len > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("answer of {len} bytes, at most {max}")));
    }
    let mut body = vec![0u8; len];
    s.read_exact(&mut body)?;
    match head[0] {
        0 => Ok(Ok(body)),
        1 => Ok(Err(String::from_utf8_lossy(&body).into_owned())),
        b => Err(io::Error::new(io::ErrorKind::InvalidData, format!("answer status {b}"))),
    }
}

/// The largest call a worker takes: `ATRIUM_WORKER_MAX`, or 64 MiB.
pub fn max_call() -> usize {
    std::env::var("ATRIUM_WORKER_MAX").ok().and_then(|v| v.parse().ok()).unwrap_or(crate::DEFAULT_MAX)
}

/// The worker's loop on `s`: each call to `f`, its answer back, until the
/// core closes. A call over `ATRIUM_WORKER_MAX` (or 64 MiB) is refused.
pub fn serve_on<F>(s: &mut UnixStream, mut f: F) -> io::Result<()>
where F: FnMut(&[u8], Option<File>) -> Result<Vec<u8>, String> {
    let max = max_call();
    loop {
        let mut head = [0u8; 5];
        let fd = match recv_exact(s, &mut head) {
            Ok(fd) => fd,
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        let len = u32::from_le_bytes(head[1..].try_into().unwrap()) as usize;
        if len > max {
            return Err(io::Error::other(format!("call of {len} bytes, at most {max}")));
        }
        if (head[0] == 1) != fd.is_some() {
            return Err(io::Error::other("call's descriptor flag does not match"));
        }
        let mut body = vec![0u8; len];
        s.read_exact(&mut body)?;
        let (status, out) = match f(&body, fd) {
            Ok(frame) => (0u8, frame),
            Err(why) => (1u8, why.into_bytes()),
        };
        let mut head = [status, 0, 0, 0, 0];
        head[1..].copy_from_slice(&(out.len() as u32).to_le_bytes());
        s.write_all(&head)?;
        s.write_all(&out)?;
    }
}

#[cfg(target_os = "freebsd")]
pub fn cap_enter() -> io::Result<()> {
    extern "C" { fn cap_enter() -> libc::c_int; }
    if unsafe { cap_enter() } != 0 { return Err(io::Error::last_os_error()) }
    Ok(())
}

#[cfg(not(target_os = "freebsd"))]
pub fn cap_enter() -> io::Result<()> {
    Err(io::Error::other("capability mode is FreeBSD's (Capsicum)"))
}
