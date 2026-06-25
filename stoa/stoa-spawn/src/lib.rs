//! # stoa-spawn — the shell-spawn seam
//!
//! Stoa runs a shell on a pty inside *some* jail. *Which* jail, and *how*
//! the spawn is privileged, is the one OS-specific seam in Stoa
//! (docs/spec/stoa.md §4.5, §11.1). Everything else — the wire protocol,
//! the predictor, the multiplexer — is OS-agnostic. So we put the spawn
//! behind a [`ShellSpawner`] trait, mirroring ostiarius's `Launcher`:
//!
//! - [`DirectSpawner`] (this crate, macOS + FreeBSD dev) — `forkpty` +
//!   `execve` a shell as the current user, **no jail**. Errors on a
//!   [`Target::Jail`] target (there are no jails off-Atrium). This is the
//!   path that lets the transport/predictor/multiplexer be developed and
//!   tested entirely on the macOS host, no VM.
//! - `BrokerSpawner` (FreeBSD, production; future) — brokers the spawn
//!   through portcullisd (`LaunchSessionComponent` / `ExecInJail`). Same
//!   trait, real jails, capability-checked.
//!
//! The handle returned, [`PtyShell`], owns the pty master fd; read/write
//! it for the terminal byte stream, `resize` on viewport changes, `wait`
//! to reap.

#![forbid(unsafe_op_in_unsafe_fn)]

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

mod direct;
pub use direct::DirectSpawner;

#[cfg(target_os = "freebsd")]
mod broker;
#[cfg(target_os = "freebsd")]
pub use broker::BrokerSpawner;

/// Which jail a session's shells attach to (stoa.md §4.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The user's per-user session jail — the default. On a dev host
    /// (no jails) this is just "the current user, no jail".
    SessionJail,
    /// A specific running jail, by name — the jexec case. Only the
    /// `BrokerSpawner` can satisfy this; `DirectSpawner` rejects it.
    Jail(String),
}

/// What to spawn.
#[derive(Debug, Clone)]
pub struct SpawnSpec {
    pub target: Target,
    /// argv; empty ⇒ the user's default login shell (`$SHELL` or `/bin/sh`).
    pub cmd: Vec<String>,
    pub cols: u16,
    pub rows: u16,
    /// Working directory; `None` ⇒ inherit.
    pub cwd: Option<String>,
    /// Extra/overriding environment on top of the inherited environment.
    pub env: Vec<(String, String)>,
}

impl SpawnSpec {
    /// A default-shell spec at the given size, in the session jail.
    pub fn login_shell(cols: u16, rows: u16) -> Self {
        SpawnSpec {
            target: Target::SessionJail,
            cmd: Vec::new(),
            cols,
            rows,
            cwd: None,
            env: Vec::new(),
        }
    }
}

/// The thing every spawner implements. One method; the trait is the seam.
pub trait ShellSpawner {
    fn spawn(&self, spec: &SpawnSpec) -> io::Result<PtyShell>;
}

/// How a [`PtyShell`]'s process is signalled and reaped — the one thing
/// that differs between the two spawners.
#[derive(Debug)]
// `Procdesc` is constructed only by the FreeBSD BrokerSpawner; on the
// macOS dev host it's unused but the match arms still reference it.
#[cfg_attr(not(target_os = "freebsd"), allow(dead_code))]
enum Reaper {
    /// We `forkpty`'d it ([`DirectSpawner`]): it's our child, so reap with
    /// `waitpid` and signal by pid.
    Child(libc::pid_t),
    /// jaild `pdfork`'d it (BrokerSpawner): we hold the procdesc, not a
    /// parent relationship. Signal with `pdkill`; closing the procdesc (on
    /// drop) reaps; observe exit via kqueue `EVFILT_PROCDESC`. FreeBSD only.
    Procdesc { fd: OwnedFd, pid: libc::pid_t },
}

/// A live shell on a pty. Owns the master fd; dropping it closes the
/// master (HUPs the slave) and, for the broker case, the procdesc (reaps).
#[derive(Debug)]
pub struct PtyShell {
    master: OwnedFd,
    reaper: Reaper,
}

impl PtyShell {
    /// The child shell's pid.
    pub fn pid(&self) -> libc::pid_t {
        match &self.reaper {
            Reaper::Child(pid) => *pid,
            Reaper::Procdesc { pid, .. } => *pid,
        }
    }

    /// Raw master fd (for poll/kqueue registration).
    pub fn master_fd(&self) -> i32 {
        self.master.as_raw_fd()
    }

    /// Push a new window size to the pty (TIOCSWINSZ). The kernel delivers
    /// SIGWINCH to the foreground process group.
    pub fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: master is a valid pty fd; TIOCSWINSZ takes a *winsize.
        let rc = unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Signal the shell (e.g. `SIGHUP`/`SIGTERM` to end a jexec session).
    pub fn kill(&self, sig: i32) -> io::Result<()> {
        let rc = match &self.reaper {
            // SAFETY: kill(2) on the recorded child pid.
            Reaper::Child(pid) => unsafe { libc::kill(*pid, sig) },
            Reaper::Procdesc { fd, .. } => pdkill(fd.as_raw_fd(), sig)?,
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Block until the shell exits; returns its exit code (or the negated
    /// signal number, mirroring shell `$?` for signal exits).
    pub fn wait(&self) -> io::Result<i32> {
        match &self.reaper {
            Reaper::Child(pid) => {
                let mut status: libc::c_int = 0;
                // SAFETY: waitpid on our own child.
                let rc = unsafe { libc::waitpid(*pid, &mut status, 0) };
                if rc < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(decode_status(status))
            }
            Reaper::Procdesc { fd, .. } => {
                procdesc_wait(fd.as_raw_fd(), false).map(|o| o.unwrap_or(-1))
            }
        }
    }

    /// Non-blocking reap. `Ok(None)` ⇒ still running.
    pub fn try_wait(&self) -> io::Result<Option<i32>> {
        match &self.reaper {
            Reaper::Child(pid) => {
                let mut status: libc::c_int = 0;
                // SAFETY: waitpid with WNOHANG on our own child.
                let rc = unsafe { libc::waitpid(*pid, &mut status, libc::WNOHANG) };
                if rc < 0 {
                    return Err(io::Error::last_os_error());
                }
                if rc == 0 { Ok(None) } else { Ok(Some(decode_status(status))) }
            }
            Reaper::Procdesc { fd, .. } => procdesc_wait(fd.as_raw_fd(), true),
        }
    }
}

/// `pdkill` the process referenced by a procdesc fd. FreeBSD only.
fn pdkill(_pd: i32, _sig: i32) -> io::Result<libc::c_int> {
    #[cfg(target_os = "freebsd")]
    {
        // SAFETY: pdkill on a procdesc fd we own.
        Ok(unsafe { libc::pdkill(_pd, _sig) })
    }
    #[cfg(not(target_os = "freebsd"))]
    {
        Err(io::Error::new(io::ErrorKind::Unsupported, "pdkill: FreeBSD only"))
    }
}

/// Wait for a procdesc'd process to exit via kqueue `EVFILT_PROCDESC`.
/// `nonblock` ⇒ return `Ok(None)` if it hasn't exited. Returns the decoded
/// exit code on exit. FreeBSD only.
fn procdesc_wait(_pd: i32, _nonblock: bool) -> io::Result<Option<i32>> {
    #[cfg(target_os = "freebsd")]
    {
        // SAFETY: kqueue + a single EVFILT_PROCDESC|NOTE_EXIT registration;
        // the returned kevent's `data` carries the wait-status.
        unsafe {
            let kq = libc::kqueue();
            if kq < 0 {
                return Err(io::Error::last_os_error());
            }
            let mut chg: libc::kevent = std::mem::zeroed();
            chg.ident = _pd as libc::uintptr_t;
            chg.filter = libc::EVFILT_PROCDESC;
            chg.flags = libc::EV_ADD | libc::EV_ONESHOT;
            chg.fflags = libc::NOTE_EXIT;
            let mut out: libc::kevent = std::mem::zeroed();
            let ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
            let tsp = if _nonblock { &ts as *const _ } else { std::ptr::null() };
            let n = libc::kevent(kq, &chg, 1, &mut out, 1, tsp);
            libc::close(kq);
            if n < 0 {
                return Err(io::Error::last_os_error());
            }
            if n == 0 {
                return Ok(None); // nonblock: not exited yet
            }
            Ok(Some(decode_status(out.data as libc::c_int)))
        }
    }
    #[cfg(not(target_os = "freebsd"))]
    {
        let _ = _nonblock;
        Err(io::Error::new(io::ErrorKind::Unsupported, "procdesc_wait: FreeBSD only"))
    }
}

fn decode_status(status: libc::c_int) -> i32 {
    if libc::WIFEXITED(status) {
        libc::WEXITSTATUS(status)
    } else if libc::WIFSIGNALED(status) {
        -libc::WTERMSIG(status)
    } else {
        -1
    }
}

impl io::Read for PtyShell {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        // SAFETY: read(2) into a caller-owned buffer of len bytes.
        let n = unsafe {
            libc::read(
                self.master.as_raw_fd(),
                buf.as_mut_ptr() as *mut libc::c_void,
                buf.len(),
            )
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            // A pty master read after the slave side closes returns EIO on
            // BSD/macOS — that is end-of-stream for us, not a real error.
            if err.raw_os_error() == Some(libc::EIO) {
                return Ok(0);
            }
            return Err(err);
        }
        Ok(n as usize)
    }
}

impl io::Write for PtyShell {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // SAFETY: write(2) from a caller-owned buffer of len bytes.
        let n = unsafe {
            libc::write(
                self.master.as_raw_fd(),
                buf.as_ptr() as *const libc::c_void,
                buf.len(),
            )
        };
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n as usize)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Construct a `PtyShell` from a raw `forkpty` master fd + pid. The process
/// is our own child (reaped with `waitpid`). Internal to [`DirectSpawner`].
pub(crate) fn pty_shell_from_raw(master: i32, pid: libc::pid_t) -> PtyShell {
    // SAFETY: `master` is a freshly-returned forkpty master fd we own.
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    PtyShell { master, reaper: Reaper::Child(pid) }
}

/// Construct a `PtyShell` from a pty master + a procdesc received over the
/// broker (the process is jaild's pdfork child). Internal to BrokerSpawner.
#[cfg(target_os = "freebsd")]
pub(crate) fn pty_shell_from_broker(master: OwnedFd, procdesc: OwnedFd, pid: libc::pid_t) -> PtyShell {
    PtyShell { master, reaper: Reaper::Procdesc { fd: procdesc, pid } }
}

/// Resolve a command name to an absolute executable path, searching `$PATH`
/// when the name is not already path-qualified. Done in the *parent* before
/// fork so the child only has to `execve` (async-signal-safe).
pub(crate) fn resolve_path(cmd: &str) -> io::Result<CString> {
    if cmd.contains('/') {
        return CString::new(cmd).map_err(|_| io::Error::other("NUL in command"));
    }
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/bin:/bin".into());
    for dir in path.split(':').filter(|d| !d.is_empty()) {
        let candidate = format!("{dir}/{cmd}");
        if is_executable(&candidate) {
            return CString::new(candidate).map_err(|_| io::Error::other("NUL in path"));
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("{cmd}: not found in PATH"),
    ))
}

fn is_executable(path: &str) -> bool {
    let c = match CString::new(path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    // SAFETY: access(2) with a valid C string; read-only probe.
    unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 }
}
