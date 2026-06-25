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

/// One pane: a shell on a pty + a server-side grid mirror + the region it
/// occupies in the window. A window has 1 pane normally; Ctrl-B % / " split
/// it into 2 (v1).
struct Pane {
    pty_fd: i32,
    shell: Option<PtyShell>,
    /// Mirror of this pane's screen — fed every byte; rendered (alone or
    /// composited) on (re)attach, switch, and split.
    term: stoa_term::Terminal,
    alive: bool,
    top: u16,
    left: u16,
    prows: u16,
    pcols: u16,
}

/// How a 2-pane window is divided.
#[derive(Clone, Copy)]
enum Divider {
    Vertical(u16),   // a │ column at this index
    Horizontal(u16), // a ─ row at this index
}

/// One window of a session: 1..2 panes + the active pane. A session has
/// 1..N windows; only the *active* window's output reaches the client.
/// Closed windows become tombstones (`alive = false`) so live window indices
/// stay stable (Ctrl-B `<n>` selects by index).
struct Window {
    panes: Vec<Pane>,
    active_pane: usize,
    alive: bool,
    divider: Option<Divider>,
}

impl Window {
    fn active(&self) -> Option<&Pane> {
        self.panes.get(self.active_pane).filter(|p| p.alive)
    }
    fn live_panes(&self) -> usize {
        self.panes.iter().filter(|p| p.alive).count()
    }
    /// Render the window for the client: a single pane is a plain snapshot;
    /// 2 panes are composited with the divider + the active pane's cursor.
    fn render(&self, cols: u16, rows: u16) -> Vec<u8> {
        if self.live_panes() <= 1 || self.divider.is_none() {
            return match self.active() {
                Some(p) => stoa_term::render_snapshot(p.term.grid()),
                None => Vec::new(),
            };
        }
        let views: Vec<stoa_term::PaneView> = self
            .panes
            .iter()
            .filter(|p| p.alive)
            .map(|p| stoa_term::PaneView { top: p.top, left: p.left, grid: p.term.grid() })
            .collect();
        let (vdivs, hdivs): (Vec<u16>, Vec<u16>) = match self.divider {
            Some(Divider::Vertical(c)) => (vec![c], vec![]),
            Some(Divider::Horizontal(r)) => (vec![], vec![r]),
            None => (vec![], vec![]),
        };
        let cursor = self
            .active()
            .map(|p| {
                let (cr, cc) = p.term.grid().cursor();
                (p.top + cr, p.left + cc)
            })
            .unwrap_or((0, 0));
        stoa_term::render_composite(cols, rows, &views, &vdivs, &hdivs, cursor)
    }

    /// Recompute pane regions for a `cols`×`rows` window and resize each
    /// pane's pty + mirror. Re-derives the split halves; falls back to a
    /// single full-window pane when there isn't an active split.
    fn relayout(&mut self, cols: u16, rows: u16) {
        let live: Vec<usize> = self
            .panes
            .iter()
            .enumerate()
            .filter(|(_, p)| p.alive)
            .map(|(i, _)| i)
            .collect();
        match (self.divider, live.as_slice()) {
            (Some(Divider::Vertical(_)), &[a0, b0]) => {
                let leftw = cols.saturating_sub(1) / 2;
                let rightw = cols.saturating_sub(leftw + 1);
                let (a, b) = if self.panes[a0].left <= self.panes[b0].left { (a0, b0) } else { (b0, a0) };
                set_pane_region(&mut self.panes[a], 0, 0, rows, leftw);
                set_pane_region(&mut self.panes[b], 0, leftw + 1, rows, rightw);
                self.divider = Some(Divider::Vertical(leftw));
            }
            (Some(Divider::Horizontal(_)), &[a0, b0]) => {
                let toph = rows.saturating_sub(1) / 2;
                let both = rows.saturating_sub(toph + 1);
                let (a, b) = if self.panes[a0].top <= self.panes[b0].top { (a0, b0) } else { (b0, a0) };
                set_pane_region(&mut self.panes[a], 0, 0, toph, cols);
                set_pane_region(&mut self.panes[b], toph + 1, 0, both, cols);
                self.divider = Some(Divider::Horizontal(toph));
            }
            _ => {
                if let Some(&i) = live.first() {
                    set_pane_region(&mut self.panes[i], 0, 0, rows, cols);
                }
                self.divider = None;
            }
        }
    }
}

/// Set a pane's region + resize its pty (TIOCSWINSZ → SIGWINCH inside) + its
/// grid mirror.
fn set_pane_region(p: &mut Pane, top: u16, left: u16, prows: u16, pcols: u16) {
    p.top = top;
    p.left = left;
    p.prows = prows;
    p.pcols = pcols;
    if let Some(sh) = &p.shell {
        let _ = sh.resize(pcols.max(1), prows.max(1));
    }
    p.term.resize(pcols.max(1), prows.max(1));
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
    /// The active window's active pane's pty fd (where input goes).
    fn active_pty(&self) -> Option<i32> {
        self.windows
            .get(self.active)
            .filter(|w| w.alive)
            .and_then(|w| w.active())
            .map(|p| p.pty_fd)
    }
    /// Render the active window (single-pane snapshot or composite) + seal it
    /// as an OUTPUT datagram. Bytes computed first (dropping the window
    /// borrow) so `tx_seq` can be bumped after.
    fn seal_active_snapshot(&mut self) -> Option<(SocketAddr, Vec<u8>)> {
        let addr = self.last_addr?;
        let (cols, rows) = (self.cols, self.rows);
        let bytes = {
            let w = self.windows.get(self.active).filter(|w| w.alive)?;
            w.render(cols, rows)
        };
        let wire = seal(&self.key, self.tx_seq, OUTPUT, &bytes);
        self.tx_seq = self.tx_seq.wrapping_add(1);
        Some((addr, wire))
    }
    /// Render the active pane's scrollback at the current `scroll` offset.
    /// (Scrollback follows the active pane; other panes aren't shown.)
    fn seal_scrollback(&mut self) -> Option<(SocketAddr, Vec<u8>)> {
        let addr = self.last_addr?;
        let off = self.scroll;
        let bytes = {
            let p = self.windows.get(self.active).filter(|w| w.alive)?.active()?;
            stoa_term::render_scrollback(p.term.grid(), off)
        };
        let wire = seal(&self.key, self.tx_seq, OUTPUT, &bytes);
        self.tx_seq = self.tx_seq.wrapping_add(1);
        Some((addr, wire))
    }
    /// History length of the active pane (max scroll offset).
    fn active_history_len(&self) -> usize {
        self.windows
            .get(self.active)
            .and_then(|w| w.active())
            .map(|p| p.term.grid().history().len())
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
                .and_then(|w| w.active())
                .map(|p| p.term.title().replace(['\t', '\n'], " "))
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
                for p in w.panes.iter().filter(|p| p.alive) {
                    if let Some(sh) = &p.shell {
                        let _ = sh.kill(libc::SIGHUP);
                    }
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
                                        w.relayout(cols, rows);
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
                                // Ctrl-B % / " — split the active window.
                                Some(Control::SplitVertical) => {
                                    if inner.started {
                                        split_active(&name, &state, &mut inner, &udp, &reg, true);
                                    }
                                }
                                Some(Control::SplitHorizontal) => {
                                    if inner.started {
                                        split_active(&name, &state, &mut inner, &udp, &reg, false);
                                    }
                                }
                                // Ctrl-B o — switch pane.
                                Some(Control::PaneSwitch) => pane_switch(&mut inner),
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
    let (cols, rows) = (inner.cols, inner.rows);
    match spawn_for(&parse_target(&state.target), cols, rows) {
        Ok(shell) => {
            let fd = shell.master_fd();
            let widx = inner.windows.len();
            inner.windows.push(Window {
                panes: vec![Pane {
                    pty_fd: fd,
                    shell: Some(shell),
                    term: stoa_term::Terminal::new(cols, rows),
                    alive: true,
                    top: 0,
                    left: 0,
                    prows: rows,
                    pcols: cols,
                }],
                active_pane: 0,
                alive: true,
                divider: None,
            });
            spawn_reader(name, widx, 0, fd, state, udp, reg);
            eprintln!("stoad: session {name:?} window {widx} spawned on port {}", state.port);
            Some(widx)
        }
        Err(e) => {
            eprintln!("stoad: session {name:?} window spawn failed: {e}");
            None
        }
    }
}

/// Spawn a new pane into an existing window at `(top,left,prows,pcols)` and
/// start its reader. Returns the new pane index. Caller holds `inner`.
fn spawn_pane(
    name: &str,
    state: &Arc<SessionState>,
    inner: &mut Inner,
    win_idx: usize,
    region: (u16, u16, u16, u16),
    udp: &UdpSocket,
    reg: &Reg,
) -> Option<usize> {
    let (top, left, prows, pcols) = region;
    match spawn_for(&parse_target(&state.target), pcols, prows) {
        Ok(shell) => {
            let fd = shell.master_fd();
            let w = inner.windows.get_mut(win_idx)?;
            let pidx = w.panes.len();
            w.panes.push(Pane {
                pty_fd: fd,
                shell: Some(shell),
                term: stoa_term::Terminal::new(pcols, prows),
                alive: true,
                top,
                left,
                prows,
                pcols,
            });
            spawn_reader(name, win_idx, pidx, fd, state, udp, reg);
            Some(pidx)
        }
        Err(e) => {
            eprintln!("stoad: session {name:?} pane spawn failed: {e}");
            None
        }
    }
}

/// Split the active window into two panes (v1: only a single-pane window).
/// The active pane keeps the first half; a new pane fills the second.
fn split_active(
    name: &str,
    state: &Arc<SessionState>,
    inner: &mut Inner,
    udp: &UdpSocket,
    reg: &Reg,
    vertical: bool,
) {
    let widx = inner.active;
    let (cols, rows) = (inner.cols, inner.rows);
    match inner.windows.get(widx) {
        Some(w) if w.live_panes() == 1 && w.divider.is_none() => {}
        _ => return, // already split, or nothing to split
    }
    let apane = inner.windows[widx].active_pane;
    let (a_region, b_region, divider) = if vertical {
        let leftw = cols.saturating_sub(1) / 2;
        let rightw = cols.saturating_sub(leftw + 1);
        if leftw < 2 || rightw < 2 {
            return; // too narrow to split
        }
        ((0, 0, rows, leftw), (0, leftw + 1, rows, rightw), Divider::Vertical(leftw))
    } else {
        let toph = rows.saturating_sub(1) / 2;
        let both = rows.saturating_sub(toph + 1);
        if toph < 2 || both < 2 {
            return; // too short to split
        }
        ((0, 0, toph, cols), (toph + 1, 0, both, cols), Divider::Horizontal(toph))
    };
    // Shrink the existing pane to the first half, set the divider.
    let (at, al, ar, ac) = a_region;
    set_pane_region(&mut inner.windows[widx].panes[apane], at, al, ar, ac);
    inner.windows[widx].divider = Some(divider);
    // Spawn the new pane in the second half and focus it.
    if let Some(pidx) = spawn_pane(name, state, inner, widx, b_region, udp, reg) {
        inner.windows[widx].active_pane = pidx;
        inner.need_snapshot = true;
        eprintln!("stoad: session {name:?} window {widx} split ({} panes)", inner.windows[widx].live_panes());
    }
}

/// Switch to the next live pane in the active window.
fn pane_switch(inner: &mut Inner) {
    let widx = inner.active;
    let changed = if let Some(w) = inner.windows.get_mut(widx) {
        let n = w.panes.len();
        match (1..=n).map(|k| (w.active_pane + k) % n).find(|&i| w.panes[i].alive) {
            Some(next) if next != w.active_pane => {
                w.active_pane = next;
                true
            }
            _ => false,
        }
    } else {
        false
    };
    if changed {
        inner.need_snapshot = true;
    }
}

/// Start a reader thread for window `win_idx`'s pane `pane_idx`.
fn spawn_reader(
    name: &str,
    win_idx: usize,
    pane_idx: usize,
    fd: i32,
    state: &Arc<SessionState>,
    udp: &UdpSocket,
    reg: &Reg,
) {
    let rstate = state.clone();
    let rudp = udp.try_clone().expect("clone udp");
    let rname = name.to_string();
    let rreg = reg.clone();
    thread::spawn(move || reader(rname, win_idx, pane_idx, fd, rstate, rudp, rreg));
}

/// What a pane's reader does after its shell exits.
enum CloseOutcome {
    /// No live windows left → session ends; carries an optional Bye datagram.
    EndSession(Option<(SocketAddr, Vec<u8>)>),
    /// The active window's view changed (active window closed → switched, or a
    /// pane closed → un-split) → repaint; carries the snapshot.
    Repaint(Option<(SocketAddr, Vec<u8>)>),
    /// A background change → nothing to send.
    Nothing,
}

/// Per-pane pty reader: feed this pane's mirror; if its window is active,
/// stream (single pane) or re-composite (multi-pane) to the client. On shell
/// exit, close the pane (un-split if a sibling survives, else close window).
fn reader(name: String, win_idx: usize, pane_idx: usize, pty_fd: i32, state: Arc<SessionState>, udp: UdpSocket, reg: Reg) {
    let mut buf = [0u8; 8192];
    loop {
        // SAFETY: read this pane's pty master into our buffer.
        let n = unsafe { libc::read(pty_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n <= 0 {
            // Shell exited. Decide the outcome under `inner` (released before
            // touching `reg`, to keep the reg→inner lock order).
            let outcome = {
                let mut inner = state.inner.lock().unwrap();
                let (cols, rows) = (inner.cols, inner.rows);
                let mut window_died = false;
                if let Some(w) = inner.windows.get_mut(win_idx) {
                    if let Some(p) = w.panes.get_mut(pane_idx) {
                        p.alive = false;
                        if let Some(sh) = p.shell.take() {
                            let _ = sh.wait();
                        }
                    }
                    let live = w.live_panes();
                    if live == 0 {
                        w.alive = false;
                        window_died = true;
                    } else if w.divider.is_some() {
                        // A pane of a split closed → the survivor takes the
                        // whole window (un-split).
                        if let Some(surv) = w.panes.iter().position(|p| p.alive) {
                            w.divider = None;
                            w.active_pane = surv;
                            let p = &mut w.panes[surv];
                            p.top = 0;
                            p.left = 0;
                            p.prows = rows;
                            p.pcols = cols;
                            if let Some(sh) = &p.shell {
                                let _ = sh.resize(cols, rows);
                            }
                            p.term.resize(cols, rows);
                        }
                    }
                }
                if window_died {
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
                        CloseOutcome::Repaint(inner.seal_active_snapshot())
                    } else {
                        CloseOutcome::Nothing
                    }
                } else if inner.active == win_idx {
                    // pane closed, window un-split → repaint the survivor
                    CloseOutcome::Repaint(inner.seal_active_snapshot())
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
                CloseOutcome::Repaint(snap) => {
                    if let Some((addr, wire)) = snap {
                        let _ = udp.send_to(&wire, addr);
                    }
                    eprintln!("stoad: session {name:?} window {win_idx} pane {pane_idx} closed");
                }
                CloseOutcome::Nothing => {}
            }
            return;
        }
        let bytes = &buf[..n as usize];
        let out = {
            let mut inner = state.inner.lock().unwrap();
            let (cols, rows) = (inner.cols, inner.rows);
            // Mirror every byte into THIS pane's grid (even when not active),
            // so a switch/split can paint its current screen.
            if let Some(w) = inner.windows.get_mut(win_idx) {
                if let Some(p) = w.panes.get_mut(pane_idx) {
                    p.term.feed(bytes);
                }
            }
            // Stream only when this pane's window is active and not scrolled.
            if inner.active == win_idx && inner.scroll == 0 {
                let multi = inner.windows.get(win_idx).map_or(false, |w| w.live_panes() > 1);
                if multi {
                    // Re-composite the whole window (both panes are visible).
                    let bytes = inner.windows[win_idx].render(cols, rows);
                    inner.last_addr.map(|a| {
                        let w = seal(&inner.key, inner.tx_seq, OUTPUT, &bytes);
                        inner.tx_seq = inner.tx_seq.wrapping_add(1);
                        (a, w)
                    })
                } else {
                    // Single pane: stream raw (cheap, low-latency).
                    inner.last_addr.map(|a| {
                        let w = seal(&inner.key, inner.tx_seq, OUTPUT, bytes);
                        inner.tx_seq = inner.tx_seq.wrapping_add(1);
                        (a, w)
                    })
                }
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
