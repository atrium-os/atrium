//! Passing an open fd to a peer over a Unix socket (`SCM_RIGHTS`) — the kernel
//! duplicates the fd into the receiver. lyrad uses it to hand a data-plane ring's
//! anonymous-shm fd to a source: the fd is the capability to write that stream.
//!
//! The syscalls are atrium-fdpass's: the received descriptor is close-on-exec,
//! and a message with none, or more than one, is an error with all closed.

use std::io;
use std::os::fd::{OwnedFd, RawFd};

/// Send `fd` over the connected socket `sock` on one filler payload byte
/// (some platforms drop a control message that carries no data).
pub fn send_fd(sock: RawFd, fd: RawFd) -> io::Result<()> {
    atrium_fdpass::send_fd(sock, fd)
}

/// Receive exactly one fd from the connected socket `sock`.
pub fn recv_fd(sock: RawFd) -> io::Result<OwnedFd> {
    atrium_fdpass::recv_fd(sock)
}
