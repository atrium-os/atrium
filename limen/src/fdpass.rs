//! Descriptors a parent hands its child with a message (limen.md §2.2,
//! §4): only for roles that take one (`media-player`'s `Load`), at most one
//! per message, in the same frame as the message. limend never reads it; it
//! only checks what it is and narrows what it can do.
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};

/// A regular file or a pipe, open for reading only: nothing that reaches
/// anywhere else (sockets, devices, directories), and nothing writable.
pub fn check(fd: &OwnedFd) -> Result<(), &'static str> {
    // SAFETY: fstat and fcntl(F_GETFL) on a descriptor we own, into a stat
    // we own.
    let (mode, flags) = unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd.as_raw_fd(), &mut st) != 0 {
            return Err("fstat failed");
        }
        (st.st_mode, libc::fcntl(fd.as_raw_fd(), libc::F_GETFL))
    };
    let kind = mode & libc::S_IFMT;
    if kind != libc::S_IFREG && kind != libc::S_IFIFO {
        return Err("not a regular file or a pipe");
    }
    if flags < 0 {
        return Err("fcntl failed");
    }
    // FreeBSD's pipes are bidirectional, both ends open read-write; what
    // makes one read-only there is `narrow`, which every passed descriptor
    // gets. Elsewhere a pipe end says which way it goes.
    let pipe_rw_ok = cfg!(target_os = "freebsd") && kind == libc::S_IFIFO;
    if flags & libc::O_ACCMODE != libc::O_RDONLY && !pipe_rw_ok {
        return Err("not read-only");
    }
    Ok(())
}

/// Narrow `fd` to read, seek and fstat (FreeBSD Capsicum); rights travel
/// with the descriptor, so the child can't widen them. Elsewhere a no-op.
pub fn narrow(fd: &OwnedFd) -> io::Result<()> {
    #[cfg(target_os = "freebsd")]
    // SAFETY: __cap_rights_init takes a zero-terminated list of rights into
    // a cap_rights_t we own; cap_rights_limit only reads it.
    unsafe {
        let mut r: libc::cap_rights_t = std::mem::zeroed();
        libc::__cap_rights_init(libc::CAP_RIGHTS_VERSION, &mut r, libc::CAP_READ, libc::CAP_SEEK, libc::CAP_FSTAT, 0u64);
        if libc::cap_rights_limit(fd.as_raw_fd(), &r) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(not(target_os = "freebsd"))]
    let _ = fd;
    Ok(())
}
