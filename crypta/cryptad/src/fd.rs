//! Descriptors over Unix sockets (SCM_RIGHTS): the tun from atrium-net, a
//! `.conf` from whoever asks for an import. The syscalls are atrium-fdpass's:
//! received descriptors are close-on-exec, and a peer that sends more than
//! [`MAX_FDS`] in one message gets an error with all of them closed.

use std::io;
use std::os::fd::{OwnedFd, RawFd};

/// Most descriptors one message may carry, either way.
pub const MAX_FDS: usize = 4;

/// `data` with `fds` (at most [`MAX_FDS`]) attached to its first byte; how
/// much of `data` went out.
pub fn send(s: RawFd, data: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    if fds.len() > MAX_FDS {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "more than 4 descriptors"));
    }
    atrium_fdpass::send(s, data, fds)
}

/// Read into `buf`, keeping descriptors that came along (at most [`MAX_FDS`]).
pub fn recv(s: RawFd, buf: &mut [u8]) -> io::Result<(usize, Vec<OwnedFd>)> {
    let got = atrium_fdpass::recv(s, buf, MAX_FDS)?;
    Ok((got.n, got.fds))
}
