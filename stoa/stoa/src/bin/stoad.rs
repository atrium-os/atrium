//! `stoad` — S1 daemon: UDP transport + a session table.
//!
//! The shell lives here, in the session table, independent of any client.
//! A client attaches by name over UDP; its keystrokes (`Input` datagrams)
//! go to the session's pty, and a per-session reader thread sends the pty's
//! output (`StateDiff` datagrams) to whichever client is currently
//! attached. A client that goes silent or detaches leaves the shell
//! running — reattaching by the same name resumes the *same* shell. The
//! shell ends only when it exits (`Bye` to the client), not when a client
//! leaves.
//!
//! Single UDP port + shared [`DEV_KEY`]: the datagram is authenticated once
//! with the dev key, then routed/deduped per session. Production gives each
//! session its own port + `K_sess` from the SSH handoff, so the port
//! identifies the session before decode; that is the only structural change.
//!
//! DirectSpawner (no jail) — production swaps `BrokerSpawner` behind the
//! same trait; this loop is unchanged.

use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::thread;

use stoa::{default_addr, Control, CONTROL, DEV_KEY, OUTPUT};
use stoa_net::Session;
use stoa_proto::{Envelope, MsgType};
use stoa_spawn::{DirectSpawner, PtyShell, ShellSpawner, SpawnSpec};

/// One persistent session: a shell on a pty, plus the currently-attached
/// client (if any) and the per-attach transport state.
struct SessionEntry {
    shell: PtyShell,
    pty_fd: i32,
    client: Option<SocketAddr>,
    net: Session,
}

#[derive(Default)]
struct Daemon {
    sessions: HashMap<String, SessionEntry>,
    /// Reverse map: client addr → session name, for routing Input/Detach.
    addrs: HashMap<SocketAddr, String>,
}

type Shared = Arc<Mutex<Daemon>>;

fn main() {
    let addr = default_addr();
    let sock = match UdpSocket::bind(&addr) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("stoad: bind {addr}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("stoad: listening on {addr} (S1: UDP + session table, DirectSpawner)");

    let shared: Shared = Arc::new(Mutex::new(Daemon::default()));
    let mut buf = [0u8; 65536];

    loop {
        let (n, src) = match sock.recv_from(&mut buf) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("stoad: recv: {e}");
                continue;
            }
        };
        // Authenticate with the shared dev key; a forged/garbled datagram
        // is dropped silently.
        let env = match Envelope::decode(DEV_KEY, &buf[..n]) {
            Ok(e) => e,
            Err(_) => continue,
        };
        match env.msg_type {
            MsgType::Control => handle_control(&sock, &shared, src, &env.payload),
            MsgType::Input => handle_input(&shared, src, &env),
            _ => {} // S1 ignores client-side StateDiff/Ack/Keepalive
        }
    }
}

fn handle_control(sock: &UdpSocket, shared: &Shared, src: SocketAddr, payload: &[u8]) {
    let Some(ctl) = Control::decode(payload) else { return };
    match ctl {
        Control::Attach { name } => {
            let reply = {
                let mut d = shared.lock().unwrap();
                if !d.sessions.contains_key(&name) {
                    // New session: spawn the shell and its reader thread.
                    match DirectSpawner::new().spawn(&SpawnSpec::login_shell(80, 24)) {
                        Ok(shell) => {
                            let pty_fd = shell.master_fd();
                            eprintln!("stoad: created session {name:?} (pid {}) for {src}", shell.pid());
                            d.sessions.insert(
                                name.clone(),
                                SessionEntry { shell, pty_fd, client: Some(src), net: Session::new(DEV_KEY) },
                            );
                            let rsock = sock.try_clone().expect("clone udp socket");
                            let rshared = shared.clone();
                            let rname = name.clone();
                            thread::spawn(move || reader(rname, pty_fd, rsock, rshared));
                        }
                        Err(e) => {
                            eprintln!("stoad: spawn for {name:?} failed: {e}");
                            return;
                        }
                    }
                } else {
                    // Reattach: rebind the client and rekey the window/seq
                    // (a fresh path is a fresh Session under the same key).
                    let e = d.sessions.get_mut(&name).unwrap();
                    e.client = Some(src);
                    e.net = Session::new(DEV_KEY);
                    eprintln!("stoad: reattach session {name:?} for {src}");
                }
                d.addrs.insert(src, name.clone());
                d.sessions.get_mut(&name).unwrap().net.seal(CONTROL, &Control::Attached.encode())
            };
            let _ = sock.send_to(&reply, src);
        }
        Control::Detach => {
            let mut d = shared.lock().unwrap();
            if let Some(name) = d.addrs.remove(&src) {
                if let Some(e) = d.sessions.get_mut(&name) {
                    if e.client == Some(src) {
                        e.client = None;
                    }
                }
                eprintln!("stoad: detach {src} from {name:?} (shell persists)");
            }
        }
        // Server→client controls are never received here.
        Control::Attached | Control::Bye => {}
    }
}

fn handle_input(shared: &Shared, src: SocketAddr, env: &Envelope) {
    let mut d = shared.lock().unwrap();
    let Some(name) = d.addrs.get(&src).cloned() else { return };
    if let Some(e) = d.sessions.get_mut(&name) {
        // Anti-replay before touching the pty.
        if e.net.admit_seq(env.seq) {
            write_all_fd(e.pty_fd, &env.payload);
        }
    }
}

/// Per-session pty reader: pump shell output → the attached client.
fn reader(name: String, pty_fd: i32, sock: UdpSocket, shared: Shared) {
    let mut buf = [0u8; 8192];
    loop {
        // SAFETY: read from the session's pty master fd into our buffer.
        let n = unsafe { libc::read(pty_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            // EOF/EIO ⇒ the shell exited. Tear the session down and tell the
            // client there's nothing to resume.
            let mut d = shared.lock().unwrap();
            if let Some(mut e) = d.sessions.remove(&name) {
                let bye = e.client.map(|addr| (addr, e.net.seal(CONTROL, &Control::Bye.encode())));
                if let Some(addr) = e.client {
                    d.addrs.remove(&addr);
                }
                drop(d);
                let _ = e.shell.wait(); // reap the zombie
                if let Some((addr, wire)) = bye {
                    let _ = sock.send_to(&wire, addr);
                }
                eprintln!("stoad: session {name:?} ended (shell exited)");
            }
            return;
        }
        let bytes = &buf[..n as usize];
        // Seal under the lock (net is shared with the input path), send after.
        let out = {
            let mut d = shared.lock().unwrap();
            match d.sessions.get_mut(&name) {
                Some(e) => e.client.map(|addr| (addr, e.net.seal(OUTPUT, bytes))),
                None => return, // session gone out from under us
            }
        };
        if let Some((addr, wire)) = out {
            let _ = sock.send_to(&wire, addr);
        }
    }
}

/// Write the whole slice to a raw fd, retrying short writes / EINTR.
fn write_all_fd(fd: i32, mut data: &[u8]) {
    while !data.is_empty() {
        // SAFETY: write from a slice we own.
        let n = unsafe { libc::write(fd, data.as_ptr() as *const libc::c_void, data.len()) };
        if n < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return; // pty gone; drop the input
        }
        if n == 0 {
            return;
        }
        data = &data[n as usize..];
    }
}
