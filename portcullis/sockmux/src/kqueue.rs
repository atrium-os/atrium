//! kqueue(2) — the only unsafe code in this crate.

#![allow(unsafe_code)]

use std::io;
use std::os::unix::io::RawFd;

/// A kqueue watching fds for readability. Level-triggered: a fd with unread
/// bytes (or a pending EOF) is reported on every wait until it is drained or
/// removed. Closing a watched fd removes it implicitly.
pub struct Kqueue {
    fd: libc::c_int,
}

impl Kqueue {
    pub fn new() -> io::Result<Self> {
        // SAFETY: kqueue takes no arguments.
        let fd = unsafe { libc::kqueue() };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Kqueue { fd })
    }

    fn change(&self, fd: RawFd, flags: u16) -> io::Result<()> {
        // SAFETY: an all-zero kevent is a valid value; the fields that
        // matter are set below. Zeroing covers the per-OS extra fields.
        let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
        ev.ident  = fd as _;
        ev.filter = libc::EVFILT_READ;
        ev.flags  = flags as _;
        // SAFETY: one changelist entry, no eventlist, null timeout.
        let rc = unsafe {
            libc::kevent(self.fd, &ev, 1, std::ptr::null_mut(), 0, std::ptr::null())
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Start reporting `fd` when it is readable.
    pub fn add_read(&self, fd: RawFd) -> io::Result<()> {
        self.change(fd, libc::EV_ADD as u16)
    }

    /// Stop reporting `fd`.
    pub fn remove_read(&self, fd: RawFd) -> io::Result<()> {
        self.change(fd, libc::EV_DELETE as u16)
    }

    /// Wait for readable fds and return them. `block = false` polls.
    /// EINTR is reported as an empty wakeup, not an error.
    pub fn wait(&self, block: bool) -> io::Result<Vec<RawFd>> {
        const MAX_EVENTS: usize = 64;
        // SAFETY: as in `change`.
        let mut evs: [libc::kevent; MAX_EVENTS] = unsafe { std::mem::zeroed() };
        let zero = libc::timespec { tv_sec: 0, tv_nsec: 0 };
        let timeout: *const libc::timespec = if block { std::ptr::null() } else { &zero };
        // SAFETY: evs holds MAX_EVENTS entries; timeout is null or a live local.
        let n = unsafe {
            libc::kevent(self.fd, std::ptr::null(), 0,
                evs.as_mut_ptr(), MAX_EVENTS as _, timeout)
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EINTR) {
                return Ok(Vec::new());
            }
            return Err(e);
        }
        Ok(evs[..n as usize].iter().map(|e| e.ident as RawFd).collect())
    }
}

impl Drop for Kqueue {
    fn drop(&mut self) {
        // SAFETY: fd is owned by this struct and closed exactly once.
        unsafe { libc::close(self.fd); }
    }
}
