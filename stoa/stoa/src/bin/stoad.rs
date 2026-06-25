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

/// One window of a session: a shell on a pty + a server-side grid mirror.
/// A session has 1..N windows; only the *active* window's output reaches the
/// client. Closed windows become tombstones (`alive = false`) so live window
/// indices stay stable (Ctrl-B `<n>` selects by index).
struct Window {
    pty_fd: i32,
    shell: Option<PtyShell>,
    /// Mirror of this window's screen — fed every byte, rendered as a
    /// snapshot on (re)attach and on window switch.
    term: stoa_term::Terminal,
    alive: bool,
}

/// Mutable per-session state, behind one lock (low-rate access). The key is
/// here (not in `SessionState`) because reattach rekeys it live.
struct Inner {
    key: [u8; KEY_LEN],
    tx_seq: u32,
    rx: ReplayWindow,
    last_addr: Option<SocketAddr>,
    started: bool, // window 0 has been spawned
    /// Last terminal size the client reported (Control::Resize); windows
    /// spawn at this size and live ptys are resized to it. Default 80×24.
    cols: u16,
    rows: u16,
    /// The session's windows + the active index. `active` always refers to a
    /// live window once `started`.
    windows: Vec<Window>,
    active: usize,
    /// The window active before the current one — for Ctrl-B l (last window).
    last_active: usize,
    /// Set on reattach / window-switch: the next client datagram triggers a
    /// snapshot of the active window.
    need_snapshot: bool,
    /// Scrollback view offset (lines above the live bottom); 0 = live. While
    /// >0, the active window's live output is held (the client is viewing
    /// history); any keystroke returns to 0.
    scroll: usize,
}

impl Inner {
    fn active_pty(&self) -> Option<i32> {
        self.windows.get(self.active).filter(|w| w.alive).map(|w| w.pty_fd)
    }
    /// Render the active window's mirror + seal it as an OUTPUT datagram for
    /// the current client. Computes the snapshot bytes first (dropping the
    /// window borrow) so `tx_seq` can be bumped after.
    fn seal_active_snapshot(&mut self) -> Option<(SocketAddr, Vec<u8>)> {
        let addr = self.last_addr?;
        let bytes = {
            let w = self.windows.get(self.active).filter(|w| w.alive)?;
            stoa_term::render_snapshot(w.term.grid())
        };
        let wire = seal(&self.key, self.tx_seq, OUTPUT, &bytes);
        self.tx_seq = self.tx_seq.wrapping_add(1);
        Some((addr, wire))
    }
    /// Render the active window's scrollback at the current `scroll` offset.
    fn seal_scrollback(&mut self) -> Option<(SocketAddr, Vec<u8>)> {
        let addr = self.last_addr?;
        let off = self.scroll;
        let bytes = {
            let w = self.windows.get(self.active).filter(|w| w.alive)?;
            stoa_term::render_scrollback(w.term.grid(), off)
        };
        let wire = seal(&self.key, self.tx_seq, OUTPUT, &bytes);
        self.tx_seq = self.tx_seq.wrapping_add(1);
        Some((addr, wire))
    }
    /// History length of the active window (max scroll offset).
    fn active_history_len(&self) -> usize {
        self.windows
            .get(self.active)
            .map(|w| w.term.grid().history().len())
            .unwrap_or(0)
    }
    /// First live window index at/after `from` (wrapping forward); `None` if none.
    fn next_live(&self, from: usize) -> Option<usize> {
        let n = self.windows.len();
        (0..n).map(|k| (from + k) % n).find(|&i| self.windows[i].alive)
    }
    /// First live window index at/before `from` (wrapping backward).
    fn prev_live(&self, from: usize) -> Option<usize> {
        let n = self.windows.len();
        (0..n).map(|k| (from + n - k) % n).find(|&i| self.windows[i].alive)
    }
    /// Make `idx` active, remembering the prior active for Ctrl-B l. Sets
    /// `need_snapshot` to repaint. No-op if already active.
    fn set_active(&mut self, idx: usize) {
        if idx != self.active && self.windows.get(idx).is_some_and(|w| w.alive) {
            self.last_active = self.active;
            self.active = idx;
            self.need_snapshot = true;
        }
    }
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

/// Serve one control-socket command, then close. Commands:
/// - `MINT <name> [target]` → `<port> <keyhex>` (mint/resume a session)
/// - `LIST`                 → one live session name per line
/// - `KILL <name>`          → `OK` / `ERR not found`
fn handle_mint(stream: UnixStream, reg: Reg) {
    let (mut uid, mut gid): (libc::uid_t, libc::gid_t) = (0, 0);
    // SAFETY: getpeereid on a connected unix-socket fd.
    if unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) } != 0 {
        eprintln!("stoad: getpeereid failed; refusing request");
        return;
    }

    let mut reader = BufReader::new(&stream);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let mut parts = line.split_whitespace();
    match parts.next() {
        Some("MINT") => {
            // `MINT <name> [target]` — target "session" (default) or "jail:<id>".
            let name = parts.next().unwrap_or("default").to_string();
            let target = parts.next().unwrap_or("session").to_string();
            let (port, key) = mint(&reg, &name, &target);
            eprintln!("stoad: mint {name:?} target={target} → port {port} for uid {uid}");
            let _ = (&stream).write_all(format!("{port} {}\n", to_hex(&key)).as_bytes());
        }
        Some("LIST") => {
            // One session per line: `<name>\t<nwindows>\t<active title>`.
            let body: String = list_sessions(&reg)
                .into_iter()
                .map(|(n, nw, title)| format!("{n}\t{nw}\t{title}\n"))
                .collect();
            let _ = (&stream).write_all(body.as_bytes());
        }
        Some("KILL") => {
            let ok = match parts.next() {
                Some(name) => kill_session(&reg, name),
                None => false,
            };
            let _ = (&stream).write_all(if ok { b"OK\n" } else { b"ERR not found\n" });
        }
        _ => {
            let _ = (&stream).write_all(b"ERR unknown command\n");
        }
    }
}

/// All live sessions as `(name, live_window_count, active_window_title)`,
/// sorted by name. Holds `reg` then each `inner` (the reg→inner order).
fn list_sessions(reg: &Reg) -> Vec<(String, usize, String)> {
    let map = reg.lock().unwrap();
    let mut out: Vec<(String, usize, String)> = map
        .iter()
        .filter(|(_, s)| !s.dead.load(Ordering::SeqCst))
        .map(|(n, s)| {
            let inner = s.inner.lock().unwrap();
            let nwin = inner.windows.iter().filter(|w| w.alive).count().max(1);
            let title = inner
                .windows
                .get(inner.active)
                .map(|w| w.term.title().replace(['\t', '\n'], " "))
                .unwrap_or_default();
            (n.clone(), nwin, title)
        })
        .collect();
    out.sort();
    out
}

/// Kill a session: HUP its shell (the pty EOF leads the reader to clean up
/// + send Bye) and mark it dead. Returns false if no such live session.
fn kill_session(reg: &Reg, name: &str) -> bool {
    let map = reg.lock().unwrap();
    match map.get(name) {
        Some(s) if !s.dead.load(Ordering::SeqCst) => {
            let inner = s.inner.lock().unwrap();
            for w in inner.windows.iter().filter(|w| w.alive) {
                if let Some(sh) = &w.shell {
                    let _ = sh.kill(libc::SIGHUP);
                }
            }
            s.dead.store(true, Ordering::SeqCst);
            eprintln!("stoad: killed session {name:?}");
            true
        }
        _ => false,
    }
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
            // The reattaching client should see the current screen.
            inner.need_snapshot = inner.started;
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
            started: false,
            cols: 80,
            rows: 24,
            windows: Vec::new(),
            active: 0,
            last_active: 0,
            need_snapshot: false,
            scroll: 0,
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
        // concurrent mint). Produce the post-unlock action + any snapshot.
        let mut snapshot: Option<(SocketAddr, Vec<u8>)> = None;
        let action: Option<(MsgType, Option<i32>, Vec<u8>)> = {
            let mut inner = state.inner.lock().unwrap();
            match Envelope::decode(&inner.key, &buf[..n]) {
                Err(_) => None,
                Ok(env) => {
                    if !inner.rx.accept(env.seq) {
                        None
                    } else {
                        inner.last_addr = Some(src);
                        if env.msg_type == MsgType::Control {
                            match Control::decode(&env.payload) {
                                // Resize: record + apply to every live window's
                                // pty + mirror. Before lazy-spawn so window 0
                                // starts at the client's size.
                                Some(Control::Resize { cols, rows }) => {
                                    inner.cols = cols;
                                    inner.rows = rows;
                                    for w in inner.windows.iter_mut().filter(|w| w.alive) {
                                        if let Some(sh) = &w.shell {
                                            let _ = sh.resize(cols, rows);
                                        }
                                        w.term.resize(cols, rows);
                                    }
                                }
                                // Redraw → repaint the active window from its
                                // mirror (the snapshot block fires below).
                                Some(Control::Redraw) => {
                                    if inner.started {
                                        inner.need_snapshot = true;
                                    }
                                }
                                // Ctrl-B c — new window, make it active.
                                Some(Control::NewWindow) => {
                                    if inner.started {
                                        if let Some(idx) =
                                            spawn_window(&name, &state, &mut inner, &udp, &reg)
                                        {
                                            inner.set_active(idx);
                                        }
                                    }
                                }
                                // Ctrl-B <n> — switch to window n if it's live.
                                Some(Control::SwitchWindow(n)) => inner.set_active(n as usize),
                                // Ctrl-B n / p — next / previous live window.
                                Some(Control::NextWindow) => {
                                    if let Some(idx) = inner.next_live(inner.active + 1) {
                                        inner.set_active(idx);
                                    }
                                }
                                Some(Control::PrevWindow) => {
                                    let n = inner.windows.len();
                                    if n > 0 {
                                        if let Some(idx) = inner.prev_live((inner.active + n - 1) % n) {
                                            inner.set_active(idx);
                                        }
                                    }
                                }
                                // Ctrl-B l — last (previously-active) window.
                                Some(Control::LastWindow) => {
                                    let la = inner.last_active;
                                    inner.set_active(la);
                                }
                                // Ctrl-B [ / ] — page the scrollback view.
                                Some(Control::ScrollUp) => {
                                    if inner.started {
                                        let page = (inner.rows.saturating_sub(1)).max(1) as usize;
                                        let maxoff = inner.active_history_len();
                                        inner.scroll = (inner.scroll + page).min(maxoff);
                                        snapshot = inner.seal_scrollback();
                                    }
                                }
                                Some(Control::ScrollDown) => {
                                    if inner.started && inner.scroll > 0 {
                                        let page = (inner.rows.saturating_sub(1)).max(1) as usize;
                                        inner.scroll = inner.scroll.saturating_sub(page);
                                        snapshot = if inner.scroll == 0 {
                                            inner.seal_active_snapshot() // back to live
                                        } else {
                                            inner.seal_scrollback()
                                        };
                                    }
                                }
                                _ => {}
                            }
                        }
                        // A keystroke while scrolled returns to the live screen.
                        if env.msg_type == MsgType::Input && inner.scroll > 0 {
                            inner.scroll = 0;
                            inner.need_snapshot = true;
                        }
                        // Lazy spawn of window 0 on the first datagram.
                        if !inner.started {
                            match spawn_window(&name, &state, &mut inner, &udp, &reg) {
                                Some(idx) => {
                                    inner.active = idx;
                                    inner.started = true;
                                }
                                None => {
                                    // window 0 failed → end the session (so the
                                    // client doesn't hang). See `spawn_for`.
                                    if let Some(addr) = inner.last_addr {
                                        let w = seal(
                                            &inner.key,
                                            inner.tx_seq,
                                            CONTROL,
                                            &Control::Bye.encode(),
                                        );
                                        inner.tx_seq = inner.tx_seq.wrapping_add(1);
                                        let _ = udp.send_to(&w, addr);
                                    }
                                    state.dead.store(true, Ordering::SeqCst);
                                }
                            }
                        }
                        // Snapshot the active window when requested (reattach,
                        // redraw, window switch/new, return-to-live). Any live
                        // snapshot is at the bottom, so clear the scroll offset.
                        if inner.need_snapshot && inner.started {
                            inner.need_snapshot = false;
                            inner.scroll = 0;
                            snapshot = inner.seal_active_snapshot();
                        }
                        let pty = inner.active_pty();
                        Some((env.msg_type, pty, env.payload))
                    }
                }
            }
        };

        if let Some((addr, wire)) = snapshot {
            let _ = udp.send_to(&wire, addr);
        }
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

/// Resolve a [`Target::SessionJail`] to its concrete spawn target.
///
/// Three cases, in order:
/// 1. `$STOA_SESSION_JAIL` names a jail → route the session through the
///    broker into that jail (broker-routed, correct on a jailed stoad).
/// 2. stoad is itself **jailed** but no session jail is configured → ERROR.
///    A jailed stoad must never `forkpty` a session shell in its OWN jail
///    (`atrium-stoad`, the wrong jail) — refuse instead of doing the wrong
///    thing. The operator sets `STOA_SESSION_JAIL`, or the client uses
///    `--jail`.
/// 3. stoad is **not** jailed (dev/macOS) → `Target::SessionJail`, i.e.
///    `DirectSpawner` `forkpty`s as the current user, no jail.
///
/// NOTE: one configured jail serves all sessions for now — operator-scoped,
/// not yet per-user. The seam generalizes: when the session model wires
/// per-user session jails, it sets this per session (stoa.md §4.5, §17).
fn resolve_session_target() -> std::io::Result<Target> {
    if let Ok(j) = std::env::var("STOA_SESSION_JAIL") {
        if !j.is_empty() {
            return Ok(Target::Jail(j));
        }
    }
    if stoad_is_jailed() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "session target on a jailed stoad: set STOA_SESSION_JAIL or attach with --jail \
             (a jailed stoad will not run a session shell in its own jail)",
        ));
    }
    Ok(Target::SessionJail)
}

/// Is this stoad process itself running inside a jail? (`security.jail.jailed`
/// = 1 in a jail.) On non-FreeBSD (the macOS dev host) we're never jailed.
fn stoad_is_jailed() -> bool {
    #[cfg(target_os = "freebsd")]
    {
        let mut jailed: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>();
        let name = c"security.jail.jailed";
        // SAFETY: sysctlbyname reads the int into `jailed`/`len`.
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                &mut jailed as *mut _ as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        rc == 0 && jailed != 0
    }
    #[cfg(not(target_os = "freebsd"))]
    {
        false
    }
}

/// Spawn the shell for a session target: `Jail(id)` via the portcullisd
/// `BrokerSpawner` (FreeBSD); `SessionJail` via [`resolve_session_target`]
/// (DirectSpawner when unjailed, broker when a session jail is configured,
/// error on a jailed stoad with none).
fn spawn_for(target: &Target, cols: u16, rows: u16) -> std::io::Result<PtyShell> {
    let resolved = match target {
        Target::SessionJail => resolve_session_target()?,
        Target::Jail(_) => target.clone(),
    };
    let spec = SpawnSpec {
        target: resolved.clone(),
        cmd: Vec::new(),
        cols,
        rows,
        cwd: None,
        env: Vec::new(),
    };
    match resolved {
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

/// Spawn one window (shell + pty + grid mirror) and start its reader thread.
/// Appends to `inner.windows`; returns the new window's index, or `None` on
/// spawn failure. Caller holds `inner` and decides the failure policy
/// (window 0 → end the session; a Ctrl-B-c window → ignore).
fn spawn_window(
    name: &str,
    state: &Arc<SessionState>,
    inner: &mut Inner,
    udp: &UdpSocket,
    reg: &Reg,
) -> Option<usize> {
    match spawn_for(&parse_target(&state.target), inner.cols, inner.rows) {
        Ok(shell) => {
            let fd = shell.master_fd();
            let idx = inner.windows.len();
            inner.windows.push(Window {
                pty_fd: fd,
                shell: Some(shell),
                term: stoa_term::Terminal::new(inner.cols, inner.rows),
                alive: true,
            });
            let rstate = state.clone();
            let rudp = udp.try_clone().expect("clone udp");
            let rname = name.to_string();
            let rreg = reg.clone();
            thread::spawn(move || reader(rname, idx, fd, rstate, rudp, rreg));
            eprintln!("stoad: session {name:?} window {idx} spawned on port {}", state.port);
            Some(idx)
        }
        Err(e) => {
            eprintln!("stoad: session {name:?} window spawn failed: {e}");
            None
        }
    }
}

/// What a window's reader does after its shell exits.
enum CloseOutcome {
    /// No live windows left → session ends; carries an optional Bye datagram.
    EndSession(Option<(SocketAddr, Vec<u8>)>),
    /// The active window closed → switched to another; carries its snapshot.
    Switched(Option<(SocketAddr, Vec<u8>)>),
    /// A background window closed → nothing to send.
    Nothing,
}

/// Per-window pty reader: feed this window's mirror; if it's the active
/// window, stream its output to the client. On shell exit, close the window.
fn reader(name: String, win_idx: usize, pty_fd: i32, state: Arc<SessionState>, udp: UdpSocket, reg: Reg) {
    let mut buf = [0u8; 8192];
    loop {
        // SAFETY: read this window's pty master into our buffer.
        let n = unsafe { libc::read(pty_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            // The window's shell exited. Decide the outcome under `inner`
            // (released before touching `reg`, to keep the reg→inner order).
            let outcome = {
                let mut inner = state.inner.lock().unwrap();
                if let Some(w) = inner.windows.get_mut(win_idx) {
                    w.alive = false;
                    if let Some(sh) = w.shell.take() {
                        let _ = sh.wait();
                    }
                }
                if !inner.windows.iter().any(|w| w.alive) {
                    let bye = inner.last_addr.map(|a| {
                        let wire = seal(&inner.key, inner.tx_seq, CONTROL, &Control::Bye.encode());
                        inner.tx_seq = inner.tx_seq.wrapping_add(1);
                        (a, wire)
                    });
                    CloseOutcome::EndSession(bye)
                } else if inner.active == win_idx {
                    if let Some(idx) = inner.next_live(0) {
                        inner.active = idx;
                    }
                    CloseOutcome::Switched(inner.seal_active_snapshot())
                } else {
                    CloseOutcome::Nothing
                }
            };
            match outcome {
                CloseOutcome::EndSession(bye) => {
                    state.dead.store(true, Ordering::SeqCst);
                    reg.lock().unwrap().remove(&name);
                    if let Some((addr, wire)) = bye {
                        let _ = udp.send_to(&wire, addr);
                    }
                    eprintln!("stoad: session {name:?} ended (last window closed)");
                }
                CloseOutcome::Switched(snap) => {
                    if let Some((addr, wire)) = snap {
                        let _ = udp.send_to(&wire, addr);
                    }
                    eprintln!("stoad: session {name:?} window {win_idx} closed; switched active");
                }
                CloseOutcome::Nothing => {
                    eprintln!("stoad: session {name:?} window {win_idx} closed");
                }
            }
            return;
        }
        let bytes = &buf[..n as usize];
        let out = {
            let mut inner = state.inner.lock().unwrap();
            // Mirror every byte into THIS window's grid (even when it's not
            // active), so a switch can paint its current screen.
            if let Some(w) = inner.windows.get_mut(win_idx) {
                w.term.feed(bytes);
            }
            // Only the active window streams to the client, and only when not
            // scrolled back (while viewing history, output is held — it still
            // lands in the mirror/history and shows on return-to-live).
            if inner.active == win_idx && inner.scroll == 0 {
                inner.last_addr.map(|a| {
                    let w = seal(&inner.key, inner.tx_seq, OUTPUT, bytes);
                    inner.tx_seq = inner.tx_seq.wrapping_add(1);
                    (a, w)
                })
            } else {
                None
            }
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
