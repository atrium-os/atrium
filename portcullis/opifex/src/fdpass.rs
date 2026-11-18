//! One request line with an open file beside it (`SCM_RIGHTS`): how a file
//! the person picked reaches opifexd without opifexd, or the sheet, opening a
//! path someone named (opifex.md §7.4, side-loading through the sheet).
//!
//! The descriptor rides on the first bytes of the line, so the receiver reads
//! the start of every request with [`read_line`], which takes it if present.

use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;

/// Write `line` (ending in `\n`) with `fd` attached.
pub fn write_line(s: &UnixStream, line: &[u8], fd: Option<RawFd>) -> io::Result<()> {
    let Some(fd) = fd else {
        return io::Write::write_all(&mut &*s, line);
    };
    let mut iov = libc::iovec { iov_base: line.as_ptr() as *mut _, iov_len: line.len() };
    let mut cmsgbuf = [0u64; 8];
    // SAFETY: a zeroed msghdr pointing at our iovec and control buffer, both
    // alive for the call; one SCM_RIGHTS header for one int fits in 64 bytes.
    let n = unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsgbuf.as_mut_ptr().cast();
        msg.msg_controllen = libc::CMSG_SPACE(std::mem::size_of::<RawFd>() as u32) as _;
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<RawFd>() as u32) as _;
        std::ptr::copy_nonoverlapping(&fd, libc::CMSG_DATA(c).cast::<RawFd>(), 1);
        libc::sendmsg(s.as_raw_fd(), &msg, 0)
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let n = n as usize;
    if n < line.len() {
        io::Write::write_all(&mut &*s, &line[n..])?;
    }
    Ok(())
}

/// Read one line (at most `max` bytes, without the `\n`) and the descriptor
/// sent with it, if any. Bytes after the newline are not expected (one
/// request per connection) and are dropped.
pub fn read_line(s: &UnixStream, max: usize) -> io::Result<(String, Option<OwnedFd>)> {
    let mut buf = vec![0u8; max.min(64 * 1024)];
    let mut cmsgbuf = [0u64; 8];
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
    let mut fd = None;
    // SAFETY: as in write_line; the kernel fills buf and the control buffer,
    // and any descriptor it put there is ours to own (taken once below).
    let n = unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = cmsgbuf.as_mut_ptr().cast();
        msg.msg_controllen = std::mem::size_of_val(&cmsgbuf) as _;
        let n = libc::recvmsg(s.as_raw_fd(), &mut msg, 0);
        if n >= 0 {
            let mut c = libc::CMSG_FIRSTHDR(&msg);
            while !c.is_null() {
                if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                    let count = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize) / std::mem::size_of::<RawFd>();
                    for i in 0..count {
                        let mut f: RawFd = -1;
                        std::ptr::copy_nonoverlapping(libc::CMSG_DATA(c).cast::<RawFd>().add(i), &mut f, 1);
                        // The first is the one; any others are closed.
                        if f >= 0 {
                            let owned = OwnedFd::from_raw_fd(f);
                            if fd.is_none() { fd = Some(owned) }
                        }
                    }
                }
                c = libc::CMSG_NXTHDR(&msg, c);
            }
            if msg.msg_flags & libc::MSG_CTRUNC != 0 {
                fd = None;
            }
        }
        n
    };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut have = buf[..n as usize].to_vec();
    while !have.contains(&b'\n') {
        if have.len() >= max {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "request too long"));
        }
        let mut more = vec![0u8; (max - have.len()).min(16 * 1024)];
        let k = (&*s).read(&mut more)?;
        if k == 0 {
            break;
        }
        have.extend_from_slice(&more[..k]);
    }
    let end = have.iter().position(|&b| b == b'\n').unwrap_or(have.len());
    have.truncate(end);
    let line = String::from_utf8(have).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "not UTF-8"))?;
    Ok((line, fd))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, Write};

    #[test]
    fn a_line_and_a_file() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut f = std::fs::File::from(OwnedFd::from(tempfile()));
        f.write_all(b"bytes of the file").unwrap();
        f.rewind().unwrap();
        write_line(&a, b"{\"ask\":\"x\"}\n", Some(f.as_raw_fd())).unwrap();
        let (line, fd) = read_line(&b, 1024).unwrap();
        assert_eq!(line, "{\"ask\":\"x\"}");
        let mut got = String::new();
        std::fs::File::from(fd.unwrap()).read_to_string(&mut got).unwrap();
        assert_eq!(got, "bytes of the file");
        // Without a file, and a line longer than one read.
        let long = format!("{}\n", "y".repeat(100_000));
        let t = std::thread::spawn(move || write_line(&a, long.as_bytes(), None).unwrap());
        let (line, fd) = read_line(&b, 200_000).unwrap();
        t.join().unwrap();
        assert_eq!((line.len(), fd.is_none()), (100_000, true));
    }

    fn tempfile() -> OwnedFd {
        let p = std::env::temp_dir().join(format!("fdpass-{}", std::process::id()));
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(&p).unwrap();
        std::fs::remove_file(&p).unwrap();
        f.into()
    }
}
