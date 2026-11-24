//! Descriptors a parent hands its child with a message (limen.md §2.2,
//! §4): only for roles that take one (`media-player`'s `Load`), at most one
//! per message, in the same frame as the message. limend never reads it; it
//! only checks what it is and narrows what it can do.
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};

/// A regular file or a pipe, open for reading only: nothing that reaches
/// anywhere else (sockets, devices, directories), and nothing writable.
/// A memory file sealed against every change (`memfd_create` with
/// `F_SEAL_WRITE`, `F_SEAL_GROW` and `F_SEAL_SHRINK`) counts as read-only
/// though it was opened read-write: nobody can change it now. That is how
/// an app hands over a document it made in memory (`pergola::print`).
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
    if flags & libc::O_ACCMODE != libc::O_RDONLY && !pipe_rw_ok && !(kind == libc::S_IFREG && sealed(fd)) {
        return Err("not read-only");
    }
    Ok(())
}

/// Seals that make a memory file unchangeable.
#[cfg(any(target_os = "freebsd", target_os = "linux"))]
const SEALED: libc::c_int = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK;

/// Whether `fd` is a memory file sealed against writing, growing and
/// shrinking.
pub fn sealed(fd: &OwnedFd) -> bool {
    #[cfg(any(target_os = "freebsd", target_os = "linux"))]
    {
        // SAFETY: fcntl(F_GET_SEALS) on a descriptor we own; no memory.
        let s = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GET_SEALS) };
        s >= 0 && s & SEALED == SEALED
    }
    #[cfg(not(any(target_os = "freebsd", target_os = "linux")))]
    {
        let _ = fd;
        false
    }
}

/// A memory file holding `bytes`, sealed against any change: what
/// [`check`] takes as read-only. `name` shows only in debugging.
#[cfg(any(target_os = "freebsd", target_os = "linux"))]
pub fn sealed_file(name: &str, bytes: &[u8]) -> io::Result<OwnedFd> {
    use std::io::Write;
    sealed_file_with(name, |file| file.write_all(bytes))
}

/// Write directly into a memory file, then rewind and seal it for handoff.
#[cfg(any(target_os = "freebsd", target_os = "linux"))]
pub fn sealed_file_with(
    name: &str,
    write: impl FnOnce(&mut std::fs::File) -> io::Result<()>,
) -> io::Result<OwnedFd> {
    use std::io::Seek;
    use std::os::fd::FromRawFd;
    let c = std::ffi::CString::new(name).map_err(|_| io::Error::other("name"))?;
    // SAFETY: a valid C string; the flags are memfd_create's.
    let raw = unsafe { libc::memfd_create(c.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: memfd_create returned a new descriptor that nothing else owns.
    let fd = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut f = std::fs::File::from(fd);
    write(&mut f)?;
    f.rewind()?;
    // SAFETY: fcntl(F_ADD_SEALS) on a descriptor we own; no memory.
    if unsafe { libc::fcntl(f.as_raw_fd(), libc::F_ADD_SEALS, SEALED | libc::F_SEAL_SEAL) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(f.into())
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

/// What a child may hand its parent (`EMIT_FD`, the picker's file): a
/// regular file only, open for reading or for reading and writing. Returns
/// whether it is writable. Not a pipe, socket, device or directory: those
/// reach further than the one file the user picked.
pub fn check_returned(fd: &OwnedFd) -> Result<bool, &'static str> {
    // SAFETY: fstat and fcntl(F_GETFL) on a descriptor we own.
    let (mode, flags) = unsafe {
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(fd.as_raw_fd(), &mut st) != 0 {
            return Err("fstat failed");
        }
        (st.st_mode, libc::fcntl(fd.as_raw_fd(), libc::F_GETFL))
    };
    if mode & libc::S_IFMT != libc::S_IFREG {
        return Err("not a regular file");
    }
    if flags < 0 {
        return Err("fcntl failed");
    }
    match flags & libc::O_ACCMODE {
        libc::O_RDONLY => Ok(false),
        libc::O_RDWR => Ok(true),
        _ => Err("write-only"),
    }
}

/// Narrow a returned file to read, seek and fstat, plus write, truncate
/// and fsync when `writable` (FreeBSD Capsicum). No `fchmod`, `fchown`,
/// `fexecve` or `mmap` beyond that: the parent gets the file's bytes, not
/// its metadata. Elsewhere a no-op.
pub fn narrow_returned(fd: &OwnedFd, writable: bool) -> io::Result<()> {
    #[cfg(target_os = "freebsd")]
    // SAFETY: as in `narrow`.
    unsafe {
        let mut r: libc::cap_rights_t = std::mem::zeroed();
        if writable {
            libc::__cap_rights_init(libc::CAP_RIGHTS_VERSION, &mut r, libc::CAP_READ, libc::CAP_SEEK, libc::CAP_FSTAT, libc::CAP_WRITE, libc::CAP_FTRUNCATE, libc::CAP_FSYNC, 0u64);
        } else {
            libc::__cap_rights_init(libc::CAP_RIGHTS_VERSION, &mut r, libc::CAP_READ, libc::CAP_SEEK, libc::CAP_FSTAT, 0u64);
        }
        if libc::cap_rights_limit(fd.as_raw_fd(), &r) != 0 {
            return Err(io::Error::last_os_error());
        }
    }
    #[cfg(not(target_os = "freebsd"))]
    let _ = (fd, writable);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(target_os = "freebsd", target_os = "linux"))]
    #[test]
    fn a_sealed_memory_file_counts_as_read_only() {
        let fd = sealed_file("doc.pdf", b"%PDF-1.7").unwrap();
        assert!(sealed(&fd));
        assert_eq!(check(&fd), Ok(()));
        // Not sealed: refused like any read-write file.
        let c = std::ffi::CString::new("open").unwrap();
        // SAFETY: as in sealed_file.
        let raw = unsafe { libc::memfd_create(c.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
        use std::os::fd::FromRawFd;
        // SAFETY: a new descriptor nothing else owns.
        let open = unsafe { OwnedFd::from_raw_fd(raw) };
        assert_eq!(check(&open), Err("not read-only"));
    }
}
