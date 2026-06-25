//! `stoactl` — the S0 client skeleton.
//!
//! `stoactl attach` (the default) connects to `stoad`, puts the local
//! terminal in raw mode, and bridges stdin/stdout to the socket until the
//! shell exits. No predictor, no multiplexer keybindings yet — raw bytes
//! both ways. Those land in S1/S2 on top of this same bridge.

use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::thread;

use stoa::{default_socket, pump};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("attach");
    match cmd {
        "attach" => attach(),
        "-h" | "--help" | "help" => usage(),
        other => {
            eprintln!("stoactl: unknown command {other:?}");
            usage();
            std::process::exit(2);
        }
    }
}

fn usage() {
    eprintln!(
        "usage: stoactl [attach]\n\
         \n\
         attach   connect to stoad ($STOA_SOCK or the default per-uid socket)\n\
         \n\
         S0 skeleton: one window, raw bytes, no prediction (stoa.md §15)."
    );
}

fn attach() {
    let sock_path = default_socket();
    let stream = match UnixStream::connect(&sock_path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("stoactl: connect {sock_path}: {e}");
            eprintln!("stoactl: is stoad running?");
            std::process::exit(1);
        }
    };

    // Put the terminal in raw mode for the duration of the session; the
    // guard restores the original settings on drop (including the normal
    // return from `attach`, so the user's shell isn't left raw).
    let _raw = RawMode::enable(libc::STDIN_FILENO);

    let sock_fd = stream.as_raw_fd();
    let stdin_fd = libc::STDIN_FILENO;
    let stdout_fd = libc::STDOUT_FILENO;

    // stdin → socket, in a background thread. Not joined: when the main
    // bridge returns and `main` exits, the process ends and this thread
    // with it (it may be blocked in read(stdin)).
    thread::spawn(move || {
        let _ = pump(stdin_fd, sock_fd);
    });

    // socket → stdout, on the main thread. Returns when stoad closes the
    // connection (shell exited).
    let _ = pump(sock_fd, stdout_fd);
    // `_raw` drops here → terminal restored, then `main` returns.
}

/// RAII raw-mode guard for a tty fd.
struct RawMode {
    fd: i32,
    orig: libc::termios,
}

impl RawMode {
    fn enable(fd: i32) -> Option<RawMode> {
        // SAFETY: zeroed termios is a valid scratch struct for tcgetattr.
        let mut orig: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: tcgetattr on a tty fd into our struct.
        if unsafe { libc::tcgetattr(fd, &mut orig) } != 0 {
            // Not a tty (e.g. piped in a test) — nothing to do.
            return None;
        }
        let mut raw = orig;
        // SAFETY: cfmakeraw mutates our termios in place.
        unsafe { libc::cfmakeraw(&mut raw) };
        // SAFETY: apply the raw settings now.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return None;
        }
        Some(RawMode { fd, orig })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: restore the saved termios on the same fd.
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.orig) };
    }
}
