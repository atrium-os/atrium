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
//! The syscalls are in `atrium-fdpass`, the one audited copy: received
//! descriptors are close-on-exec at once (portcullisd forks children that
//! exec into jails), and a client that sends more than `max_fds` or
//! truncated ancillary data gets an error with every descriptor closed.
//!
//! BufReader interaction: see `protocol.rs` — the Launch path uses
//! a `ReadyForFds` round-trip so the daemon's read buffer is
//! guaranteed empty before `recv_fds` runs. Without that, BufReader
//! could swallow the 1-byte payload via plain `read()`, which
//! drops the cmsg silently.

use std::io;
use std::os::unix::io::{AsRawFd, OwnedFd, RawFd};

/// Send `fds` over `stream`. The kernel requires at least one byte
/// of normal data to carry the cmsg; we send a literal 'F' byte
/// that the receiver consumes and discards.
pub fn send_fds<S: AsRawFd>(stream: &S, fds: &[RawFd]) -> io::Result<()> {
    send_fds_with(stream, fds, b"F")
}

/// [`send_fds`] with `data` (at least one byte) as the normal data: for a
/// receiver that reads a short message beside the descriptors (hospes-cdm
/// reads its key system and memory limit that way).
pub fn send_fds_with<S: AsRawFd>(stream: &S, fds: &[RawFd], data: &[u8]) -> io::Result<()> {
    let sent = atrium_fdpass::send(stream.as_raw_fd(), data, fds)?;
    if sent != data.len() {
        return Err(io::Error::other(format!("partial sendmsg: wrote {sent} of {} bytes", data.len())));
    }
    Ok(())
}

/// Receive at most `max_fds` descriptors riding on one payload byte, which
/// is discarded (matching `send_fds`'s 'F'). More than `max_fds`, truncated
/// ancillary data, or end of stream is an error.
pub fn recv_fds<S: AsRawFd>(stream: &S, max_fds: usize) -> io::Result<Vec<OwnedFd>> {
    let mut payload = [0u8; 1];
    let got = atrium_fdpass::recv(stream.as_raw_fd(), &mut payload, max_fds)?;
    if got.n == 0 {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "peer closed before sending fds"));
    }
    Ok(got.fds)
}

// ── tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::os::unix::io::FromRawFd;
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
