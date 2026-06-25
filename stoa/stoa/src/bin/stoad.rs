//! `stoad` — daemon: a local mint control plane + per-session UDP ports.
//!
//! Two planes:
//! - **Control** (a local Unix socket): a `MINT <name>` request — sent
//!   directly for a local client, or by `stoa-shell` over an SSH channel
//!   for a remote one — allocates (or resumes) a session and returns its
//!   `{udp_port, K_sess}`. The peer is identified by `getpeereid` (the
//!   kernel's word, not the client's).
//! - **Data** (one UDP socket per session): the minted port carries the
//!   MAC'd, sequenced datagrams. Because each session has its own port,
//!   `stoad` knows which `K_sess` to authenticate with before decoding.
//!
//! The shell lives in the session, independent of any client: a client is
//! just the address `stoad` last received a valid datagram from, so a
//! dropped/roamed client (new source address) resumes the same shell with
//! no explicit reattach. The shell is spawned lazily on the client's first
//! datagram, so its opening prompt is routed to a known address.
//!
//! **Reattach rekeys** (stoa.md §2): a fresh mint of an existing session
//! keeps the shell + port but issues a new `K_sess` and resets the
//! sequence/replay window — a fresh transport session over the same shell.
//! The key therefore lives under the per-session lock, where the recv/reader
//! threads read it.
//!
//! DirectSpawner (no jail); production swaps `BrokerSpawner` behind the
//! same trait.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use stoa::{default_ctl, gen_key, seal, to_hex, Control, CONTROL, KEY_LEN, OUTPUT};
use stoa_proto::{Envelope, MsgType, ReplayWindow};
use stoa_spawn::{DirectSpawner, PtyShell, ShellSpawner, SpawnSpec, Target};

/// Mutable per-session state, behind one lock (low-rate access). The key is
/// here (not in `SessionState`) because reattach rekeys it live.
struct Inner {
    key: [u8; KEY_LEN],
    tx_seq: u32,
    rx: ReplayWindow,
    last_addr: Option<SocketAddr>,
    pty_fd: Option<i32>,
    shell: Option<PtyShell>,
    started: bool,
}

struct SessionState {
    port: u16,
    /// Where this session's shell runs: `"session"` (the user's session
    /// jail, via DirectSpawner on dev) or `"jail:<id>"` (jexec into a
    /// running jail, via the portcullisd broker — stoa.md §4.5).
    target: String,
    dead: AtomicBool,
    inner: Mutex<Inner>,
}

type Reg = Arc<Mutex<HashMap<String, Arc<SessionState>>>>;

fn main() {
    let ctl_path = default_ctl();
    let _ = std::fs::remove_file(&ctl_path);
    let listener = match UnixListener::bind(&ctl_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("stoad: bind control {ctl_path}: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("stoad: control on {ctl_path} (mint per-session UDP ports, DirectSpawner)");

    let reg: Reg = Arc::new(Mutex::new(HashMap::new()));
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                let reg = reg.clone();
                thread::spawn(move || handle_mint(stream, reg));
            }
            Err(e) => eprintln!("stoad: control accept: {e}"),
        }
    }
}

/// Serve one `MINT <name>` request, then close. Reply: `<port> <keyhex>`.
fn handle_mint(stream: UnixStream, reg: Reg) {
    let (mut uid, mut gid): (libc::uid_t, libc::gid_t) = (0, 0);
    // SAFETY: getpeereid on a connected unix-socket fd.
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        eprintln!("stoad: getpeereid failed; refusing mint");
        return;
    }

    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    // `MINT <name> [target]` — target is "session" (default) or "jail:<id>".
    let mut parts = line.split_whitespace();
    if parts.next() != Some("MINT") {
        let _ = (&stream).write_all(b"ERR expected MINT\n");
        return;
    }
    let name = parts.next().unwrap_or("default").to_string();
    let target = parts.next().unwrap_or("session").to_string();

    let (port, key) = mint(&reg, &name, &target);
    eprintln!("stoad: mint {name:?} target={target} → port {port} for uid {uid}");
    let _ = (&stream).write_all(format!("{port} {}\n", to_hex(&key)).as_bytes());
}

/// Find-and-rekey or create the session; return its `{port, key}`.
fn mint(reg: &Reg, name: &str, target: &str) -> (u16, [u8; KEY_LEN]) {
    let mut map = reg.lock().unwrap();
    if let Some(s) = map.get(name) {
        if !s.dead.load(Ordering::SeqCst) {
            // Resume: keep the shell + port; rekey + reset the transport
            // window (a fresh path is a fresh transport session, same shell).
            let mut inner = s.inner.lock().unwrap();
            let key = gen_key();
            inner.key = key;
            inner.tx_seq = 0;
            inner.rx = ReplayWindow::new();
            inner.last_addr = None;
            return (s.port, key);
        }
        // A dead session (shell exited, or spawn failed) — drop it and make
        // a fresh one below so the name is reusable.
        map.remove(name);
    }
    // New session: its own UDP port + key; the shell spawns lazily on the
    // first datagram (so its prompt has somewhere to go).
    let udp = UdpSocket::bind("0.0.0.0:0").expect("bind session udp");
    let port = udp.local_addr().unwrap().port();
    let key = gen_key();
    let state = Arc::new(SessionState {
        port,
        target: target.to_string(),
        dead: AtomicBool::new(false),
        inner: Mutex::new(Inner {
            key,
            tx_seq: 0,
            rx: ReplayWindow::new(),
            last_addr: None,
            pty_fd: None,
            shell: None,
            started: false,
        }),
    });
    map.insert(name.to_string(), state.clone());
    let name = name.to_string();
    let reg = reg.clone();
    thread::spawn(move || recv_loop(name, state, udp, reg));
    (port, key)
}

/// Per-session receive loop: authenticate, anti-replay, learn the client
/// address, lazily spawn the shell, route Input to the pty.
fn recv_loop(name: String, state: Arc<SessionState>, udp: UdpSocket, reg: Reg) {
    udp.set_read_timeout(Some(Duration::from_millis(200))).ok();
    let mut buf = [0u8; 65536];
    loop {
        if state.dead.load(Ordering::SeqCst) {
            return;
        }
        let (n, src) = match udp.recv_from(&mut buf) {
            Ok(v) => v,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                continue;
            }
            Err(_) => continue,
        };

        // Decode + admit under the lock (the key may be rekeyed by a
        // concurrent mint). Produce the post-unlock action, if any.
        let action: Option<(MsgType, Option<i32>, Vec<u8>)> = {
            let mut inner = state.inner.lock().unwrap();
            match Envelope::decode(&inner.key, &buf[..n]) {
                Err(_) => None,
                Ok(env) => {
                    if !inner.rx.accept(env.seq) {
                        None
                    } else {
                        inner.last_addr = Some(src);
                        if !inner.started {
                            spawn_shell(&name, &state, &mut inner, &udp, &reg);
                        }
                        Some((env.msg_type, inner.pty_fd, env.payload))
                    }
                }
            }
        };

        if let Some((MsgType::Input, Some(fd), payload)) = action {
            write_all_fd(fd, &payload);
        }
    }
}

/// Parse a session target string → a spawner [`Target`]. `"jail:<id>"` →
/// `Target::Jail(id)`; anything else → `Target::SessionJail`.
fn parse_target(target: &str) -> Target {
    match target.strip_prefix("jail:") {
        Some(id) if !id.is_empty() => Target::Jail(id.to_string()),
        _ => Target::SessionJail,
    }
}

/// Spawn the shell for a session target: SessionJail via DirectSpawner;
/// Jail(id) via the portcullisd BrokerSpawner (FreeBSD only).
fn spawn_for(target: &Target) -> std::io::Result<PtyShell> {
    let spec = SpawnSpec {
        target: target.clone(),
        cmd: Vec::new(),
        cols: 80,
        rows: 24,
        cwd: None,
        env: Vec::new(),
    };
    match target {
        Target::SessionJail => DirectSpawner::new().spawn(&spec),
        Target::Jail(_) => spawn_broker(&spec),
    }
}

#[cfg(target_os = "freebsd")]
fn spawn_broker(spec: &SpawnSpec) -> std::io::Result<PtyShell> {
    use stoa_spawn::BrokerSpawner;
    BrokerSpawner::new().spawn(spec)
}
#[cfg(not(target_os = "freebsd"))]
fn spawn_broker(_spec: &SpawnSpec) -> std::io::Result<PtyShell> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "jail target needs the FreeBSD portcullisd broker (BrokerSpawner)",
    ))
}

/// Lazily spawn the session's shell on the first client datagram and start
/// its reader thread. Caller holds `inner`.
fn spawn_shell(name: &str, state: &Arc<SessionState>, inner: &mut Inner, udp: &UdpSocket, reg: &Reg) {
    match spawn_for(&parse_target(&state.target)) {
        Ok(shell) => {
            let fd = shell.master_fd();
            inner.pty_fd = Some(fd);
            inner.shell = Some(shell);
            inner.started = true;
            let rstate = state.clone();
            let rudp = udp.try_clone().expect("clone udp");
            let rname = name.to_string();
            let rreg = reg.clone();
            thread::spawn(move || reader(rname, fd, rstate, rudp, rreg));
            eprintln!("stoad: session {name:?} shell spawned on port {}", state.port);
        }
        Err(e) => {
            eprintln!("stoad: session {name:?} spawn failed: {e}");
            // Tell the client there's nothing to attach to so it doesn't
            // hang waiting for output/Bye. Mark dead (recv_loop exits next
            // iteration; a re-mint recreates — see `mint`). Don't touch the
            // registry here: we hold `inner`, and mint takes reg→inner, so
            // locking reg here would invert the order.
            if let Some(addr) = inner.last_addr {
                let wire = seal(&inner.key, inner.tx_seq, CONTROL, &Control::Bye.encode());
                inner.tx_seq = inner.tx_seq.wrapping_add(1);
                let _ = udp.send_to(&wire, addr);
            }
            state.dead.store(true, Ordering::SeqCst);
        }
    }
}

/// Per-session pty reader: shell output → the last-known client address.
fn reader(name: String, pty_fd: i32, state: Arc<SessionState>, udp: UdpSocket, reg: Reg) {
    let mut buf = [0u8; 8192];
    loop {
        // SAFETY: read the session's pty master into our buffer.
        let n = unsafe { libc::read(pty_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            // Shell exited: mark dead (stops recv_loop), drop from the
            // registry (so a re-mint makes a fresh session), say Bye, reap.
            state.dead.store(true, Ordering::SeqCst);
            reg.lock().unwrap().remove(&name);
            let bye = {
                let mut inner = state.inner.lock().unwrap();
                let out = inner.last_addr.map(|a| {
                    let w = seal(&inner.key, inner.tx_seq, CONTROL, &Control::Bye.encode());
                    inner.tx_seq = inner.tx_seq.wrapping_add(1);
                    (a, w)
                });
                if let Some(sh) = inner.shell.take() {
                    let _ = sh.wait();
                }
                out
            };
            if let Some((addr, wire)) = bye {
                let _ = udp.send_to(&wire, addr);
            }
            eprintln!("stoad: session {name:?} ended (shell exited)");
            return;
        }
        let bytes = &buf[..n as usize];
        let out = {
            let mut inner = state.inner.lock().unwrap();
            inner.last_addr.map(|a| {
                let w = seal(&inner.key, inner.tx_seq, OUTPUT, bytes);
                inner.tx_seq = inner.tx_seq.wrapping_add(1);
                (a, w)
            })
        };
        if let Some((addr, wire)) = out {
            let _ = udp.send_to(&wire, addr);
        }
    }
}

fn write_all_fd(fd: i32, mut data: &[u8]) {
    while !data.is_empty() {
        // SAFETY: write from a slice we own.
        let n = unsafe { libc::write(fd, data.as_ptr() as *const libc::c_void, data.len()) };
        if n < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return;
        }
        if n == 0 {
            return;
        }
        data = &data[n as usize..];
    }
}
