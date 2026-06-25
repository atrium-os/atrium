//! `stoactl` — S1 client: attach to a named `stoad` session over UDP.
//!
//! `stoactl attach <name>` connects to `stoad`, sends `Attach{name}`, and
//! bridges the terminal: a stdin thread seals keystrokes into `Input`
//! datagrams; the main thread admits inbound `StateDiff` datagrams to
//! stdout. Two escapes end the client:
//!
//! - **Ctrl-]** (the [`DETACH_BYTE`]) in the input stream → send `Detach`
//!   and exit, leaving the shell running in `stoad` (reattach resumes it).
//! - **stdin EOF** (a piped client) → stop sending but keep rendering until
//!   the shell exits (`Bye`).
//! - **`Bye`** from `stoad` (the shell exited) → exit.
//!
//! Send and receive run on separate threads, so the client keeps the
//! transport's two directions as independent halves (a tx seq counter; an
//! rx replay window) rather than one `Session`.

use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use stoa::{
    default_addr, seal, Control, CONTROL, DETACH_BYTE, DEV_KEY, INPUT, OUTPUT,
};
use stoa_proto::{Envelope, ReplayWindow};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("attach") | None => {
            let name = args.get(1).cloned().unwrap_or_else(|| "default".into());
            attach(&name);
        }
        Some("-h") | Some("--help") | Some("help") => usage(),
        Some(other) => {
            eprintln!("stoactl: unknown command {other:?}");
            usage();
            std::process::exit(2);
        }
    }
}

fn usage() {
    eprintln!(
        "usage: stoactl attach [name]\n\
         \n\
         Attach to (creating if absent) the named stoad session over UDP\n\
         ($STOA_ADDR, default 127.0.0.1:7654). Ctrl-] detaches (shell keeps\n\
         running); the shell exiting ends the client.\n\
         \n\
         S1: one window, raw bytes in the payload, no prediction (stoa.md §15)."
    );
}

fn attach(name: &str) {
    let stoad = default_addr();
    let sock = match UdpSocket::bind("127.0.0.1:0").and_then(|s| {
        s.connect(&stoad)?;
        Ok(s)
    }) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("stoactl: connect {stoad}: {e}");
            std::process::exit(1);
        }
    };

    // Attach is seq 0; the stdin thread continues the tx stream from 1.
    if let Err(e) = sock.send(&seal(DEV_KEY, 0, CONTROL, &Control::Attach { name: name.into() }.encode())) {
        eprintln!("stoactl: send attach: {e}");
        std::process::exit(1);
    }

    let _raw = RawMode::enable(libc::STDIN_FILENO);
    let stop = Arc::new(AtomicBool::new(false));

    // stdin → Input datagrams (background).
    {
        let ssock = sock.try_clone().expect("clone socket");
        let sstop = stop.clone();
        thread::spawn(move || stdin_loop(ssock, sstop));
    }

    // Inbound: StateDiff → stdout, Bye → done. A short read timeout lets us
    // notice a detach requested by the stdin thread.
    sock.set_read_timeout(Some(Duration::from_millis(150))).ok();
    let mut rx = ReplayWindow::new();
    let mut buf = [0u8; 65536];
    let stdout = libc::STDOUT_FILENO;

    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        match sock.recv(&mut buf) {
            Ok(n) => {
                let Ok(env) = Envelope::decode(DEV_KEY, &buf[..n]) else { continue };
                if !rx.accept(env.seq) {
                    continue;
                }
                if env.msg_type == OUTPUT {
                    write_all_fd(stdout, &env.payload);
                } else if env.msg_type == CONTROL {
                    if let Some(Control::Bye) = Control::decode(&env.payload) {
                        break;
                    }
                }
            }
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                continue;
            }
            Err(_) => break,
        }
    }
    // `_raw` drops here → terminal restored.
}

/// Forward stdin to `stoad` as `Input` datagrams until EOF or a detach byte.
fn stdin_loop(sock: UdpSocket, stop: Arc<AtomicBool>) {
    let mut seq: u32 = 1;
    let mut buf = [0u8; 4096];
    loop {
        // SAFETY: read from stdin into our buffer.
        let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            // EOF: stop forwarding, but let the main loop keep rendering
            // until the shell exits. (A real tty never EOFs in raw mode.)
            return;
        }
        let chunk = &buf[..n as usize];
        if let Some(i) = chunk.iter().position(|&b| b == DETACH_BYTE) {
            if i > 0 {
                let _ = sock.send(&seal(DEV_KEY, seq, INPUT, &chunk[..i]));
                seq = seq.wrapping_add(1);
            }
            let _ = sock.send(&seal(DEV_KEY, seq, CONTROL, &Control::Detach.encode()));
            stop.store(true, Ordering::SeqCst);
            return;
        }
        let _ = sock.send(&seal(DEV_KEY, seq, INPUT, chunk));
        seq = seq.wrapping_add(1);
    }
}

fn write_all_fd(fd: i32, mut data: &[u8]) {
    while !data.is_empty() {
        // SAFETY: write from a slice we own.
        let n = unsafe { libc::write(fd, data.as_ptr() as *const libc::c_void, data.len()) };
        if n <= 0 {
            return;
        }
        data = &data[n as usize..];
    }
}

/// RAII raw-mode guard (no-op when stdin is not a tty, e.g. piped tests).
struct RawMode {
    fd: i32,
    orig: libc::termios,
}

impl RawMode {
    fn enable(fd: i32) -> Option<RawMode> {
        // SAFETY: zeroed termios is valid scratch for tcgetattr.
        let mut orig: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut orig) } != 0 {
            return None;
        }
        let mut raw = orig;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return None;
        }
        Some(RawMode { fd, orig })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.orig) };
    }
}
