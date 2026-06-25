//! `stoad` — the S0 daemon skeleton.
//!
//! Listens on a local Unix socket; for each connection it spawns a shell
//! on a pty (via the OS-agnostic [`DirectSpawner`] seam) and bridges bytes
//! between the socket and the pty until either side closes. One window, no
//! envelope, no persistence — see `stoa` crate docs for what S0 omits.
//!
//! This is the dev/host build: `DirectSpawner`, no jail. The production
//! daemon swaps in `BrokerSpawner` (portcullisd → jaild) behind the same
//! `ShellSpawner` trait; the accept/bridge loop here is unchanged.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::thread;

use stoa::{default_socket, pump};
use stoa_spawn::{DirectSpawner, ShellSpawner, SpawnSpec};

fn main() {
    let sock_path = default_socket();

    // Clear a stale socket from a prior run (best-effort).
    let _ = std::fs::remove_file(&sock_path);

    let listener = match UnixListener::bind(&sock_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("stoad: bind {sock_path}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("stoad: listening on {sock_path} (S0 skeleton, DirectSpawner)");

    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                thread::spawn(move || handle(stream));
            }
            Err(e) => eprintln!("stoad: accept: {e}"),
        }
    }
}

/// One attached client: spawn a shell, bridge it to the socket.
fn handle(mut stream: UnixStream) {
    let spec = SpawnSpec::login_shell(80, 24);
    let shell = match DirectSpawner::new().spawn(&spec) {
        Ok(s) => s,
        Err(e) => {
            let _ = writeln!(stream, "stoad: spawn failed: {e}");
            return;
        }
    };
    eprintln!("stoad: attached pid={}", shell.pid());

    let pty_fd = shell.master_fd();
    let sock_fd = stream.as_raw_fd();

    // socket → pty (client keystrokes into the shell). Runs in its own
    // thread; the raw fds are plain ints, and `stream`/`shell` outlive the
    // thread because we join it before they drop.
    let writer = thread::spawn(move || {
        let _ = pump(sock_fd, pty_fd);
    });

    // pty → socket (shell output to the client). Returns when the shell
    // exits (pty EOF/EIO).
    let _ = pump(pty_fd, sock_fd);

    // Shell is gone (or the client left). Tear down: HUP+reap the shell,
    // then shut the socket read side so the writer thread's `pump` returns.
    let _ = shell.kill(libc::SIGHUP);
    let _ = shell.wait();
    let _ = stream.shutdown(std::net::Shutdown::Both);
    let _ = writer.join();
    eprintln!("stoad: detached pid={}", shell.pid());
}
