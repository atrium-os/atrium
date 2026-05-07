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
