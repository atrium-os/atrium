//! PTY + child-shell management for FreeBSD.
//!
//! Uses libc::forkpty(3): single call that opens a pty pair, forks,
//! and in the child sets up stdin/stdout/stderr → slave + execvp's
//! the shell. The parent gets back the master fd.

use std::ffi::{CString, OsStr};
use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::ptr;

extern "C" {
    fn forkpty(
        amaster: *mut libc::c_int,
        name: *mut libc::c_char,
        termp: *const libc::termios,
        winp: *const libc::winsize,
    ) -> libc::pid_t;
}

pub struct Shell {
    pub master: OwnedFd,
    pub pid:    libc::pid_t,
}

impl Shell {
    /// Spawn `program` with `args` in a fresh pty of size cols × rows.
    /// Returns master fd + child pid.
    pub fn spawn(program: &OsStr, args: &[&str], cols: u16, rows: u16) -> io::Result<Self> {
        let win = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        let mut master_fd: libc::c_int = -1;
        let pid = unsafe { forkpty(&mut master_fd, ptr::null_mut(), ptr::null(), &win) };
        if pid < 0 {
            return Err(io::Error::last_os_error());
        }
        if pid == 0 {
            // ── child ──
            // Set the TERM environment variable so the shell uses
            // sensible escape sequences. "xterm" is widely supported;
            // we'll grow our VT subset toward that target.
            let term_var = CString::new("TERM=xterm-256color").unwrap();
            unsafe { libc::putenv(term_var.into_raw()); }

            let prog = CString::new(program.as_bytes()).unwrap();
            let mut argv: Vec<CString> = Vec::with_capacity(args.len() + 2);
            argv.push(prog.clone());
            for a in args { argv.push(CString::new(*a).unwrap()); }
            let argv_ptrs: Vec<*const libc::c_char> = argv.iter()
                .map(|s| s.as_ptr())
                .chain(std::iter::once(ptr::null()))
                .collect();
            unsafe { libc::execvp(prog.as_ptr(), argv_ptrs.as_ptr()); }
            // exec failed
            unsafe { libc::_exit(127); }
        }

        // ── parent ──
        let master = unsafe { OwnedFd::from_raw_fd(master_fd) };

        // Make master non-blocking so our event loop can drain on demand.
        unsafe {
            let flags = libc::fcntl(master_fd, libc::F_GETFL);
            libc::fcntl(master_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        Ok(Shell { master, pid })
    }

    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let fd: RawFd = std::os::fd::AsRawFd::as_raw_fd(&self.master);
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.raw_os_error() == Some(libc::EAGAIN) {
                return Ok(0);
            }
            return Err(e);
        }
        Ok(n as usize)
    }

    #[allow(dead_code)]
    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        let fd: RawFd = std::os::fd::AsRawFd::as_raw_fd(&self.master);
        let n = unsafe { libc::write(fd, buf.as_ptr() as *const _, buf.len()) };
        if n < 0 { return Err(io::Error::last_os_error()); }
        Ok(n as usize)
    }

    /// Push a new TIOCSWINSZ to the master fd; the kernel signals
    /// SIGWINCH to the foreground process group so curses-style apps
    /// re-layout. Use after the host window changed size.
    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let fd: RawFd = std::os::fd::AsRawFd::as_raw_fd(&self.master);
        let win = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        let r = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &win) };
        if r < 0 { return Err(io::Error::last_os_error()); }
        Ok(())
    }
}
