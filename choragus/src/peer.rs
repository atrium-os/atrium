//! Verified peer identity via `getpeereid(2)` — the connecting process's real
//! uid/gid, not a self-declared id.
//!
//! This is the pattern Portcullis uses (jaild and portcullisd both authenticate
//! peers this way). Choragus mirrors it: the uid resolves to the *user* whose
//! Portcullis grant store applies, so the grant Choragus enforces is the one the
//! platform actually approved — and the identity is the kernel's, not the app's
//! word. (When Portcullis launches each app in a distinct-uid jail, the uid
//! identifies the *app*; until then it identifies the user.)

use std::io;
use std::os::fd::RawFd;

/// The connected peer's (uid, gid) — the kernel's authenticated answer.
pub fn uid_gid(fd: RawFd) -> io::Result<(u32, u32)> {
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    let r = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((uid, gid))
}

/// The username for a uid (via the passwd database).
pub fn username(uid: u32) -> Option<String> {
    let pw = unsafe { libc::getpwuid(uid) };
    if pw.is_null() {
        return None;
    }
    let name = unsafe { std::ffi::CStr::from_ptr((*pw).pw_name) };
    Some(name.to_string_lossy().into_owned())
}
