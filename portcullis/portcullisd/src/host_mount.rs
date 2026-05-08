//! Host-namespace mount cleanup helpers. FreeBSD doesn't give us
//! per-jail mount namespaces (jails share the global mount table
//! with the host), so jail-applied nullfs/tmpfs mounts survive
//! the jailed process's exit. Unless we explicitly unmount them,
//! they leak and break the next jail that wants the same target.
//!
//! Used by `init_phase` after the init jail terminates and by
//! `bootstrap` at `--once` exit. The supervisor's handle_exit
//! path uses it for permanently-failed services.

#![allow(unsafe_code)]

use std::io;

/// `close(2)` wrapper. Used at `--once` teardown to explicitly
/// close held procdesc fds in a controlled order before cleanup,
/// rather than letting Rust's drop close them at process exit
/// after we've already done the cleanup work.
pub fn close_fd(fd: i32) -> io::Result<()> {
    // SAFETY: caller owns the fd; libc::close on a valid fd is
    // benign even if the close itself fails (EINTR, EIO, ...).
    let rc = unsafe { libc::close(fd) };
    if rc < 0 { return Err(io::Error::last_os_error()); }
    Ok(())
}

/// `unmount(2)` wrapper. Returns Ok if the mount was removed or
/// was never there (EINVAL "target not mounted"); Err on busy
/// or other failures the caller should log.
#[cfg(target_os = "freebsd")]
pub fn unmount(target: &str) -> io::Result<()> {
    let c = std::ffi::CString::new(target)
        .map_err(|_| io::Error::other("unmount: NUL in path"))?;
    // SAFETY: c is a valid CStr; flags=0 = default (don't force).
    let rc = unsafe { libc::unmount(c.as_ptr(), 0) };
    if rc < 0 {
        let e = io::Error::last_os_error();
        // EINVAL on a path that isn't a mountpoint is "already
        // unmounted" — fine.
        if e.raw_os_error() == Some(libc::EINVAL) {
            return Ok(());
        }
        return Err(e);
    }
    Ok(())
}

#[cfg(not(target_os = "freebsd"))]
pub fn unmount(_target: &str) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "FreeBSD only"))
}
