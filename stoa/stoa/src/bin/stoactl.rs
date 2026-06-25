//! `stoactl` — client: mint a session, then bridge the terminal over UDP.
//!
//! ```text
//! stoactl attach <name>                 # local stoad (mint over the control socket)
//! stoactl attach <name> --host user@host # remote: ssh user@host stoa-shell <name>
//! ```
//!
//! Mint yields `{udp_host, port, K_sess}`; the client then sends a
//! keepalive (registering its address + lazily spawning the shell), and
//! bridges the terminal: a stdin thread seals keystrokes into `Input`
//! datagrams; the main thread renders inbound `StateDiff` to stdout.
//! Ctrl-] detaches (the shell keeps running in `stoad`); the shell exiting
//! (`Bye`) ends the client.

use std::io::Read;
use std::net::UdpSocket;
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use stoa::predict::Predictor;
use stoa::{default_ctl, from_hex, seal, Control, CONTROL, DETACH_BYTE, INPUT, OUTPUT, PREFIX_BYTE};
use stoa_proto::{Envelope, ReplayWindow};

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    match argv.first().map(String::as_str) {
        Some("attach") | None => {
            let mut name = "default".to_string();
            let mut host: Option<String> = None;
            let mut jail: Option<String> = None;
            let mut it = argv.iter().skip(1);
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--host" => host = it.next().cloned(),
                    "--jail" => jail = it.next().cloned(),
                    other => name = other.to_string(),
                }
            }
            attach(&name, host.as_deref(), jail.as_deref());
        }
        Some("list") => list_sessions(),
        Some("kill") => match argv.get(1) {
            Some(name) => kill_session(name),
            None => {
                eprintln!("usage: stoactl kill <name>");
                std::process::exit(2);
            }
        },
        Some("-h") | Some("--help") | Some("help") => usage(),
        Some(other) => {
            eprintln!("stoactl: unknown command {other:?}");
            usage();
            std::process::exit(2);
        }
    }
}

/// One control-socket request/reply against the local stoad. Returns the
/// reply body (lines).
fn ctl_request(line: &str) -> Result<String, String> {
    use std::io::{Read, Write};
    let ctl = default_ctl();
    let mut stream = UnixStream::connect(&ctl)
        .map_err(|e| format!("connect {ctl}: {e} (is stoad running?)"))?;
    stream
        .write_all(format!("{line}\n").as_bytes())
        .map_err(|e| format!("send: {e}"))?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply).map_err(|e| format!("read: {e}"))?;
    Ok(reply)
}

fn list_sessions() {
    match ctl_request("LIST") {
        Ok(body) => {
            let lines: Vec<&str> = body.lines().filter(|l| !l.is_empty()).collect();
            if lines.is_empty() {
                println!("(no sessions)");
                return;
            }
            for l in lines {
                // `<name>\t<nwindows>\t<active title>`
                let mut f = l.split('\t');
                let name = f.next().unwrap_or("?");
                let nw = f.next().unwrap_or("?");
                let title = f.next().unwrap_or("").trim();
                let w = if nw == "1" { "window" } else { "windows" };
                if title.is_empty() {
                    println!("{name}  ({nw} {w})");
                } else {
                    println!("{name}  ({nw} {w})  {title}");
                }
            }
        }
        Err(e) => {
            eprintln!("stoactl: {e}");
            std::process::exit(1);
        }
    }
}

fn kill_session(name: &str) {
    match ctl_request(&format!("KILL {name}")) {
        Ok(body) if body.trim_start().starts_with("OK") => println!("killed {name}"),
        Ok(body) => {
            eprintln!("stoactl: kill {name}: {}", body.trim());
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("stoactl: {e}");
            std::process::exit(1);
        }
    }
}

fn usage() {
    eprintln!(
        "usage:\n\
         \x20 stoactl attach <name> [--host user@host] [--jail <id>]\n\
         \x20 stoactl list                 list sessions on the local stoad\n\
         \x20 stoactl kill <name>          kill a session\n\
         \n\
         attach mints/resumes the session and bridges the terminal over UDP.\n\
         Without --host, talks to the local stoad ($STOA_CTL); with --host,\n\
         runs `ssh user@host stoa-shell <name>`. --jail <id> jexecs into a\n\
         running jail. Ctrl-B d / Ctrl-] detaches; Ctrl-B r redraws."
    );
}

/// (udp_host, udp_port, key). `jail` selects the session target: `None` →
/// the user's session jail; `Some(id)` → jexec into that running jail.
fn mint(name: &str, host: Option<&str>, jail: Option<&str>) -> Result<(String, u16, Vec<u8>), String> {
    let target = match jail {
        Some(id) => format!("jail:{id}"),
        None => "session".to_string(),
    };
    match host {
        None => {
            // Local: talk to the control socket directly.
            let ctl = default_ctl();
            let stream = UnixStream::connect(&ctl)
                .map_err(|e| format!("connect {ctl}: {e} (is stoad running?)"))?;
            use std::io::{BufRead, BufReader, Write};
            (&stream)
                .write_all(format!("MINT {name} {target}\n").as_bytes())
                .map_err(|e| format!("mint request: {e}"))?;
            let mut reply = String::new();
            BufReader::new(&stream)
                .read_line(&mut reply)
                .map_err(|e| format!("mint reply: {e}"))?;
            let (port, key) = parse_mint(reply.trim())?;
            Ok(("127.0.0.1".to_string(), port, key))
        }
        Some(hostspec) => {
            // Remote: a transport (ssh by default) runs stoa-shell; its
            // stdout returns over the channel. $STOA_SSH overrides the
            // transport prefix (e.g. "ssh -p 2222 -T", or a mosh-style
            // launcher); we append <hostspec> stoa-shell <name>.
            let transport = std::env::var("STOA_SSH").unwrap_or_else(|_| "ssh -T".into());
            let mut toks = transport.split_whitespace();
            let prog = toks.next().unwrap_or("ssh");
            let mut cmd = Command::new(prog);
            for t in toks {
                cmd.arg(t);
            }
            cmd.arg(hostspec).arg("stoa-shell").arg(name);
            if let Some(id) = jail {
                cmd.arg("--jail").arg(id);
            }
            let out = cmd
                .output()
                .map_err(|e| format!("spawn transport {prog:?}: {e}"))?;
            if !out.status.success() {
                return Err(format!(
                    "{prog} stoa-shell failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                ));
            }
            let stdout = String::from_utf8_lossy(&out.stdout);
            let line = stdout
                .lines()
                .find_map(|l| l.trim().strip_prefix("STOA_SESSION "))
                .ok_or_else(|| format!("no STOA_SESSION line in ssh output:\n{stdout}"))?;
            let (port, key) = parse_mint(line)?;
            // UDP host = the host part of user@host.
            let udp_host = hostspec.rsplit('@').next().unwrap_or(hostspec).to_string();
            Ok((udp_host, port, key))
        }
    }
}

fn parse_mint(s: &str) -> Result<(u16, Vec<u8>), String> {
    let mut p = s.split_whitespace();
    let port = p
        .next()
        .and_then(|x| x.parse::<u16>().ok())
        .ok_or_else(|| format!("bad mint reply: {s:?}"))?;
    let key = p
        .next()
        .and_then(from_hex)
        .ok_or_else(|| format!("bad key in mint reply: {s:?}"))?;
    Ok((port, key))
}

fn attach(name: &str, host: Option<&str>, jail: Option<&str>) {
    let (udp_host, port, key) = match mint(name, host, jail) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("stoactl: {e}");
            std::process::exit(1);
        }
    };

    let sock = match UdpSocket::bind("0.0.0.0:0").and_then(|s| {
        s.connect((udp_host.as_str(), port))?;
        Ok(s)
    }) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("stoactl: udp connect {udp_host}:{port}: {e}");
            std::process::exit(1);
        }
    };

    let _raw = RawMode::enable(libc::STDIN_FILENO);
    let stop = Arc::new(AtomicBool::new(false));
    // One monotonic tx seq shared by every client→server datagram (the
    // initial Resize, stdin Input, and resize-poll Resize).
    let tx_seq = Arc::new(AtomicU32::new(0));
    // Predictive echo (opt-in via $STOA_PREDICT) — hides round-trip latency.
    // Shared between the stdin thread (predicts) and this loop (reconciles).
    // Holding it also serializes stdout writes between the two.
    let predictor: Option<Arc<Mutex<Predictor>>> = std::env::var_os("STOA_PREDICT")
        .map(|_| Arc::new(Mutex::new(Predictor::new())));

    // Initial Resize: registers our address + lazily spawns the shell at the
    // client's ACTUAL terminal size (80×24 if stdout isn't a tty, e.g. piped).
    let (c0, r0) = winsize(libc::STDOUT_FILENO).unwrap_or((80, 24));
    let _ = sock.send(&seal(
        &key,
        tx_seq.fetch_add(1, Ordering::SeqCst),
        CONTROL,
        &Control::Resize { cols: c0, rows: r0 }.encode(),
    ));

    // stdin → Input datagrams.
    {
        let ssock = sock.try_clone().expect("clone socket");
        let skey = key.clone();
        let sstop = stop.clone();
        let sseq = tx_seq.clone();
        let spred = predictor.clone();
        thread::spawn(move || stdin_loop(ssock, skey, sseq, sstop, spred));
    }
    // resize poller → a Resize datagram whenever the terminal size changes.
    {
        let ssock = sock.try_clone().expect("clone socket");
        let skey = key.clone();
        let sseq = tx_seq.clone();
        let sstop = stop.clone();
        thread::spawn(move || resize_loop(ssock, skey, sseq, sstop, (c0, r0)));
    }

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
                let Ok(env) = Envelope::decode(&key, &buf[..n]) else { continue };
                if !rx.accept(env.seq) {
                    continue;
                }
                if env.msg_type == OUTPUT {
                    match &predictor {
                        // Reconcile server output against predictions: write
                        // the result + suppress confirmed echoes; on divergence
                        // request a repaint from the server's grid mirror. The
                        // write happens under the lock to serialize with the
                        // stdin thread's local echo.
                        Some(p) => {
                            let redraw = {
                                let mut g = p.lock().unwrap();
                                let (bytes, redraw) = g.on_output(&env.payload);
                                write_all_fd(stdout, &bytes);
                                redraw
                            };
                            if redraw {
                                let _ = sock.send(&seal(
                                    &key,
                                    tx_seq.fetch_add(1, Ordering::SeqCst),
                                    CONTROL,
                                    &Control::Redraw.encode(),
                                ));
                            }
                        }
                        None => write_all_fd(stdout, &env.payload),
                    }
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

fn stdin_loop(
    sock: UdpSocket,
    key: Vec<u8>,
    seq: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    pred: Option<Arc<Mutex<Predictor>>>,
) {
    // Flush accumulated keystrokes as one Input datagram (preserving order
    // relative to commands).
    let flush = |pending: &mut Vec<u8>| {
        if !pending.is_empty() {
            let _ = sock.send(&seal(&key, seq.fetch_add(1, Ordering::SeqCst), INPUT, pending));
            pending.clear();
        }
    };
    let send_ctl = |c: Control| {
        let _ = sock.send(&seal(&key, seq.fetch_add(1, Ordering::SeqCst), CONTROL, &c.encode()));
    };

    let mut chunk = [0u8; 4096];
    let mut prefix = false; // saw Ctrl-B; next byte is a command
    let mut pending: Vec<u8> = Vec::new();
    loop {
        let n = match std::io::stdin().lock().read(&mut chunk) {
            Ok(0) | Err(_) => {
                flush(&mut pending);
                return; // EOF: stop sending, let main render until Bye
            }
            Ok(n) => n,
        };
        for &b in &chunk[..n] {
            if prefix {
                prefix = false;
                flush(&mut pending); // commands run after queued input
                match b {
                    b'd' | DETACH_BYTE => {
                        stop.store(true, Ordering::SeqCst);
                        return; // Ctrl-B d = detach
                    }
                    b'r' => send_ctl(Control::Redraw), // repaint from the mirror
                    b'c' => send_ctl(Control::NewWindow), // new window
                    b'n' => send_ctl(Control::NextWindow), // next window
                    b'p' => send_ctl(Control::PrevWindow), // previous window
                    b'l' => send_ctl(Control::LastWindow), // last (toggle) window
                    b'0'..=b'9' => send_ctl(Control::SwitchWindow(b - b'0')), // window n
                    b'[' => send_ctl(Control::ScrollUp), // scrollback page up
                    b']' => send_ctl(Control::ScrollDown), // scrollback page down
                    PREFIX_BYTE => pending.push(PREFIX_BYTE), // literal Ctrl-B
                    _ => {} // unknown command: ignored
                }
            } else if b == PREFIX_BYTE {
                flush(&mut pending); // keep input before the prefix in order
                prefix = true;
            } else if b == DETACH_BYTE {
                flush(&mut pending);
                stop.store(true, Ordering::SeqCst);
                return;
            } else {
                // Predictive echo: show the keystroke locally now (under the
                // predictor lock, serializing with the recv loop's output).
                if let Some(p) = &pred {
                    let mut g = p.lock().unwrap();
                    if let Some(echo) = g.on_input(b) {
                        // SAFETY: write one byte to stdout.
                        unsafe {
                            libc::write(libc::STDOUT_FILENO, [echo].as_ptr() as *const libc::c_void, 1);
                        }
                    }
                }
                pending.push(b);
            }
        }
        flush(&mut pending);
    }
}

/// Poll the terminal size; send a Resize datagram whenever it changes. A
/// poll (vs a SIGWINCH handler) keeps this async-signal-safe and simple —
/// resize isn't latency-critical. Exits when the session stops.
fn resize_loop(sock: UdpSocket, key: Vec<u8>, seq: Arc<AtomicU32>, stop: Arc<AtomicBool>, initial: (u16, u16)) {
    let mut last = initial;
    loop {
        thread::sleep(Duration::from_millis(400));
        if stop.load(Ordering::SeqCst) {
            return;
        }
        if let Some(sz) = winsize(libc::STDOUT_FILENO) {
            if sz != last {
                last = sz;
                let _ = sock.send(&seal(
                    &key,
                    seq.fetch_add(1, Ordering::SeqCst),
                    CONTROL,
                    &Control::Resize { cols: sz.0, rows: sz.1 }.encode(),
                ));
            }
        }
    }
}

/// Current terminal size of `fd` (cols, rows) via TIOCGWINSZ. `None` if not
/// a tty (e.g. piped) or zero-sized.
fn winsize(fd: i32) -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: TIOCGWINSZ writes the winsize struct for a tty fd.
    let rc = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    if rc == 0 && ws.ws_col > 0 {
        Some((ws.ws_col, ws.ws_row))
    } else {
        None
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
