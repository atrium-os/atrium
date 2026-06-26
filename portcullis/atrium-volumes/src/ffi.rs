//! Localised `unsafe` for atrium-volumes. Just `getpeereid` for
//! the per-connection peer-uid check + `chown(2)` (libc's
//! wrappers in `std::fs` don't cover ownership).

#![allow(unsafe_code)]

use std::ffi::CString;
use std::io;
use std::path::Path;

#[cfg(target_os = "freebsd")]
pub fn getpeereid(fd: i32) -> io::Result<u32> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: caller passes an open socket fd; getpeereid only
    // writes the two out parameters. uid/gid are stack locals.
    let rc = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(uid)
}

#[cfg(not(target_os = "freebsd"))]
pub fn getpeereid(_fd: i32) -> io::Result<u32> {
    // Build-host (macOS); jaild peer check is FreeBSD-only.
    Ok(0)
}

/// dup an open fd and wrap as a UnixStream so std::io::Write
/// can be used on it. Caller's original fd stays open.
pub fn dup_to_stream(fd: i32) -> std::os::unix::net::UnixStream {
    use std::os::unix::io::FromRawFd;
    // SAFETY: dup returns a fresh fd referencing the same open
    // file table entry; UnixStream::from_raw_fd takes ownership
    // of the duplicate so std drops it cleanly.
    unsafe {
        let d = libc::dup(fd);
        assert!(d >= 0, "dup failed");
        std::os::unix::net::UnixStream::from_raw_fd(d)
    }
}

/// `chown(path, uid, gid)`. std::fs has no equivalent.
pub fn chown(path: &Path, uid: u32, gid: u32) -> io::Result<()> {
    let c_path = CString::new(path.as_os_str().to_string_lossy().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "nul in path"))?;
    // SAFETY: c_path outlives the syscall; uid/gid are scalars.
    let rc = unsafe { libc::chown(c_path.as_ptr(), uid, gid) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Set a Tessera per-directory quota limit on `path` (the volume root) via
/// the kmod ioctl `TESSERA_IOC_QUOTA_SET = _IOW('T', 1, uint64_t)`. On
/// FreeBSD that encodes to `_IOC(IN=0x8000_0000, 'T', 1, sizeof(u64)=8)` =
/// `0x8000_0000 | (8 << 16) | (0x54 << 8) | 1` = `0x8008_5401`. Returns
/// `ENOTTY` if `path` isn't on a Tessera mount (or the kmod lacks the op),
/// which the caller treats as "this backend doesn't enforce size_max".
#[cfg(target_os = "freebsd")]
pub fn tessera_set_quota(path: &Path, limit_bytes: u64) -> io::Result<()> {
    const TESSERA_IOC_QUOTA_SET: libc::c_ulong = 0x8008_5401;
    let c_path = CString::new(path.as_os_str().to_string_lossy().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "nul in path"))?;
    // SAFETY: open a directory read-only for the ioctl; close on every path.
    let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut lim = limit_bytes;
    let rc = unsafe {
        libc::ioctl(fd, TESSERA_IOC_QUOTA_SET, &mut lim as *mut u64)
    };
    let err = io::Error::last_os_error();
    unsafe { libc::close(fd) };
    if rc != 0 {
        return Err(err);
    }
    Ok(())
}

#[cfg(not(target_os = "freebsd"))]
pub fn tessera_set_quota(_path: &Path, _limit_bytes: u64) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "tessera quota ioctl is FreeBSD-only",
    ))
}
