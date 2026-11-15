//! atrium-term-socket — terminal emulator on the FreeBSD-native
//! Atrium stack, ported to fresco-client (M3d).
//!
//! Output goes through Stoa's terminal emulator (`stoa-term`) into a cell
//! grid, drawn as a cursor RECT and one text run per row. Key events
//! translate to bytes via the keymap. kqueue multiplexes the compositor
//! socket and every session's descriptor.
//!
//! **Consoles** (docs/spec/homes.md §4): run by Portcullis as the system
//! terminal, it starts no shell of its own. Each console portcullisd opens
//! (a shell in an app's jail) arrives on the document channel and gets a
//! window. Two kinds:
//!
//! - a **Stoa session** (`application/x-atrium-stoa`): a connected datagram
//!   socket to stoad, which holds the shells. Closing the window detaches;
//!   the shells keep running, and Terminal on the app reattaches. The
//!   screen comes as Stoa's grid diffs. Ctrl+Shift+T asks portcullisd for
//!   another shell in the session (a tab: a Stoa window), Ctrl+PageUp and
//!   Ctrl+PageDown go between them;
//! - a bare **pty** (`application/x-atrium-console`), when stoad isn't
//!   running: closing the window ends the shell.
//!
//! Ctrl+Shift+N asks for a new console window in the same app. Every ask
//! carries the key press that made it, which portcullisd spends.
//!
//! **On its own** (homes.md §5.3, W7): opened from the Applications grid,
//! with no console on its way, it asks for the **empty** console, a shell
//! that reaches nothing. A strip at the top names where each window's shell
//! is; a click on it (or Ctrl+Shift+O) opens the switcher: Empty and the
//! person's workspaces. Choosing one is a press that moves this window to
//! a shell there (portcullisd starts or reattaches it).

mod keymap;
mod pty;
mod render;

use std::ffi::OsStr;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixDatagram;
use std::ptr;
use std::time::Duration;

use fresco_client::{Connection, Event};
use fresco_protocol::WindowHints;
use stoa::Control;
use stoa_proto::{Envelope, MsgType, ReplayWindow};

const FONT_NAME:    &str = "system-mono";
/// In points, like everything this lays out (resolution-independence.md §3).
const FONT_SIZE_PX: f32  = 18.0;
const COLS: u16 = 80;
const ROWS: u16 = 24;
const WIN_W: u32 = 880;
const WIN_H: u32 = 540;

const CONSOLE_MIME: &str = "application/x-atrium-console";
const STOA_MIME: &str = "application/x-atrium-stoa";
const HID_T: u16 = 0x17;
const HID_N: u16 = 0x11;
const HID_PAGE_UP: u16 = 0x4B;
const HID_PAGE_DOWN: u16 = 0x4E;
const HID_O: u16 = 0x12;
const HID_ENTER: u16 = 0x28;
const HID_ESC: u16 = 0x29;
const HID_DOWN: u16 = 0x51;
const HID_UP: u16 = 0x52;
/// How long a terminal opened on its own waits for a console before it
/// asks for the empty one.
const ALONE_AFTER: Duration = Duration::from_millis(1500);

/// Where a window's screen comes from.
enum Io {
    Pty { shell: pty::Shell, term: stoa_term::Terminal },
    Stoa { sock: UnixDatagram, key: Vec<u8>, tx: u32, rx: ReplayWindow, session: String, grid: stoa_term::Grid,
           /// The last sequence number taken: a gap means a lost diff.
           last: Option<u32> },
}

impl Io {
    fn fd(&self) -> i32 {
        match self {
            Io::Pty { shell, .. } => shell.master.as_raw_fd(),
            Io::Stoa { sock, .. } => sock.as_raw_fd(),
        }
    }

    fn grid(&self) -> &stoa_term::Grid {
        match self {
            Io::Pty { term, .. } => term.grid(),
            Io::Stoa { grid, .. } => grid,
        }
    }

    fn send(&mut self, t: MsgType, payload: &[u8]) {
        match self {
            Io::Pty { shell, .. } => {
                if t == MsgType::Input {
                    let _ = shell.write(payload);
                }
            }
            Io::Stoa { sock, key, tx, .. } => {
                let _ = sock.send(&stoa::seal(key, *tx, t, payload));
                *tx = tx.wrapping_add(1);
            }
        }
    }

    fn input(&mut self, b: &[u8]) {
        self.send(MsgType::Input, b);
    }

    fn control(&mut self, c: Control) {
        if let Io::Stoa { .. } = self {
            self.send(MsgType::Control, &c.encode());
        }
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        match self {
            Io::Pty { shell, term } => {
                term.resize(cols, rows);
                let _ = shell.resize(cols, rows);
            }
            Io::Stoa { .. } => self.control(Control::Resize { cols, rows }),
        }
    }

    /// Take what is waiting. `Ok(true)`: the screen changed; `Err`: it's over.
    fn read(&mut self, buf: &mut [u8]) -> Result<bool, ()> {
        match self {
            Io::Pty { shell, term } => {
                let mut changed = false;
                loop {
                    match shell.read(buf) {
                        Ok(0) => return Ok(changed),
                        Ok(n) => {
                            term.feed(&buf[..n]);
                            changed = true;
                        }
                        Err(_) => return Err(()),
                    }
                }
            }
            Io::Stoa { sock, key, rx, grid, last, .. } => {
                let mut changed = false;
                let mut lost = false;
                loop {
                    let n = match sock.recv(buf) {
                        Ok(n) => n,
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(_) => return Err(()),
                    };
                    let Ok(env) = Envelope::decode(key, &buf[..n]) else { continue }; // probes, strays
                    if !rx.accept(env.seq) {
                        continue;
                    }
                    // Diffs build on each other: after a lost one the grid
                    // is wrong until a full repaint, so ask for one.
                    if last.is_some_and(|l| env.seq != l.wrapping_add(1)) {
                        lost = true;
                    }
                    *last = Some(env.seq);
                    match env.msg_type {
                        MsgType::StateDiff => {
                            if let Some(d) = stoa_term::StateDiff::decode(&env.payload) {
                                d.apply(grid);
                                changed = true;
                            }
                        }
                        MsgType::Control => {
                            if matches!(Control::decode(&env.payload), Some(Control::Bye)) {
                                return Err(());
                            }
                        }
                        _ => {}
                    }
                }
                if lost {
                    self.control(Control::SyncMode);
                }
                Ok(changed)
            }
        }
    }
}

struct Session {
    win:      u32,
    title:    String,
    /// The app this is a console of (more may be asked for), if any.
    app:      Option<String>,
    io:       Io,
    keymap:   keymap::Keymap,
    renderer: render::Renderer,
    dirty:    bool,
    /// What the trace last printed.
    traced:   String,
    /// The strip and the switcher's state; the switcher's targets.
    strip:    render::Strip,
    targets:  Vec<String>,
}

/// What the strip calls a console target.
fn place_name(app: Option<&str>) -> String {
    match app {
        None => "Terminal".into(),
        Some("empty") => "Empty (reaches nothing)".into(),
        Some(a) => match a.strip_prefix("workspace/") {
            Some(w) => format!("Workspace: {w}"),
            None => format!("App: {}", a.rsplit('.').next().unwrap_or(a)),
        },
    }
}

fn portcullisd() -> Result<std::os::unix::net::UnixStream, String> {
    use portcullis_ipc::{round_trip, Request, Response, PROTO_VERSION};
    let path = portcullis_ipc::resolve_socket_path().ok_or("portcullisd is not reachable")?;
    let mut s = std::os::unix::net::UnixStream::connect(path).map_err(|e| e.to_string())?;
    let _ = s.set_read_timeout(Some(Duration::from_secs(60)));
    match round_trip(&mut s, &Request::Hello { version: PROTO_VERSION }) {
        Ok(Response::Hello { .. }) => Ok(s),
        other => Err(format!("handshake: {other:?}")),
    }
}

/// The person's workspaces, for the switcher.
fn workspaces() -> Vec<String> {
    use portcullis_ipc::{round_trip, Request, Response};
    let Ok(mut s) = portcullisd() else { return Vec::new() };
    match round_trip(&mut s, &Request::Workspaces) {
        Ok(Response::WorkspaceList { workspaces, .. }) => workspaces.into_iter().map(|w| w.name).collect(),
        other => {
            eprintln!("atrium-term-socket: the workspaces: {other:?}");
            Vec::new()
        }
    }
}

impl Session {
    fn open_switcher(&mut self) {
        let mut items = vec!["Empty".to_string()];
        let mut targets = vec!["empty".to_string()];
        for w in workspaces() {
            items.push(format!("Workspace: {w}"));
            targets.push(format!("workspace/{w}"));
        }
        let sel = targets.iter().position(|t| Some(t.as_str()) == self.app.as_deref()).unwrap_or(0);
        self.strip.menu = Some((items, sel));
        self.targets = targets;
        self.dirty = true;
    }

    fn close_switcher(&mut self) {
        self.strip.menu = None;
        self.dirty = true;
    }
}

struct Font { id: u32, size: f32, cell_w: f32, line_h: f32, ascent: f32 }

fn open_window(conn: &mut Connection, font: &Font, title: &str, mut io: Io, app: Option<String>, at: (i32, i32)) -> io::Result<Session> {
    let win = conn.window_create(WIN_W, WIN_H, title, WindowHints {
        initial_position: Some(at),
        ..Default::default()
    })?;
    let mut renderer = render::Renderer::new(font.id, font.size, font.cell_w, font.line_h, font.ascent);
    renderer.k = conn.window_scale(win);
    eprintln!("atrium-term-socket: window {win} “{title}”");
    // Stoa: grid diffs, at our size; the first datagram also tells stoad
    // where we are, and it answers with the whole screen.
    if app.is_some() {
        renderer.top = render::STRIP_H;
    }
    io.control(Control::SyncMode);
    io.resize(COLS, ROWS);
    let strip = render::Strip { place: place_name(app.as_deref()), hint: "switch: click or Ctrl+Shift+O".into(), menu: None };
    Ok(Session { win, title: title.into(), app, io, keymap: keymap::Keymap::new(), renderer, dirty: true, traced: String::new(),
                 strip, targets: Vec::new() })
}

fn draw(conn: &mut Connection, s: &mut Session, trace: bool) -> io::Result<()> {
    conn.set_default_window(s.win as u16);
    s.renderer.render(conn, s.io.grid(), &s.strip)?;
    s.dirty = false;
    if trace {
        let g = s.io.grid();
        let rows: Vec<String> = (0..g.rows())
            .map(|r| g.row(r).iter().map(|c| if c.c == '\0' { ' ' } else { c.c }).collect::<String>().trim_end().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        let t = rows.join(" | ");
        let t = match &s.strip.menu {
            Some((items, sel)) => format!("[{}] menu={}@{sel} {t}", s.strip.place, items.join(",")),
            None => format!("[{}] {t}", s.strip.place),
        };
        if t != s.traced {
            eprintln!("atrium-term: screen “{}”: {t}", s.title);
            s.traced = t;
        }
    }
    Ok(())
}

/// Ask portcullisd for a console of `app`: a tab in Stoa session `tab_of`,
/// a new window, or (neither) one this window switches to. `activation` is
/// the press.
fn ask(app: &str, tab_of: Option<String>, new_window: bool, activation: [u8; 16]) -> Result<(), String> {
    use portcullis_ipc::{round_trip, Request, Response};
    let mut s = portcullisd()?;
    match round_trip(&mut s, &Request::Console { app_id: app.into(), activation, tab_of, new_window }) {
        Ok(Response::ConsoleOpened { .. }) => Ok(()),
        Ok(Response::Error { message }) => Err(message),
        other => Err(format!("{other:?}")),
    }
}

/// `type; k=v; k=v` → the value of `k`.
fn param<'a>(mime: &'a str, k: &str) -> Option<&'a str> {
    mime.split(';').skip(1).filter_map(|p| p.trim().split_once('=')).find(|(a, _)| *a == k).map(|(_, v)| v)
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // $FRESCOD_SOCK (the old name), else $FRESCO_SOCKET, the jail's, or dev's.
    let sock = std::env::var("FRESCOD_SOCK")
        .unwrap_or_else(|_| fresco_client::default_socket_path().to_string_lossy().into_owned());
    let mut conn = Connection::connect(&sock)?;
    eprintln!("atrium-term-socket: connected to {sock}");
    // The screens' text on stderr, for gates: $ATRIUM_TERM_TRACE, or a
    // `.trace` file shipped in the bundle (jail(8) clears the environment).
    let trace = std::env::var_os("ATRIUM_TERM_TRACE").is_some() || std::path::Path::new("/.trace").exists();

    let f = conn.font_open(FONT_NAME)?;
    if f.font_id == 0 {
        return Err(format!("server could not open '{FONT_NAME}'").into());
    }
    let upe = f.units_per_em as f32;
    let ascent = f.ascent_units as f32 * FONT_SIZE_PX / upe;
    let descent = -(f.descent_units as f32) * FONT_SIZE_PX / upe;
    let cell_w = if f.mono_advance_units > 0 { f.mono_advance_units as f32 * FONT_SIZE_PX / upe } else { FONT_SIZE_PX * 0.6 };
    let font = Font { id: f.font_id, size: FONT_SIZE_PX, cell_w, line_h: ascent + descent + 2.0, ascent };

    // Under Portcullis, consoles come on the document channel; outside it,
    // one window with a shell of our own, as before.
    let mut channel = atrium_open::Channel::connect()?;
    let mut sessions: Vec<Session> = Vec::new();
    if channel.is_none() {
        let shell = pty::Shell::spawn(OsStr::new("/bin/sh"), &["-i"], COLS, ROWS)?;
        eprintln!("spawned /bin/sh pid={}", shell.pid);
        let io = Io::Pty { shell, term: stoa_term::Terminal::new(COLS, ROWS) };
        sessions.push(open_window(&mut conn, &font, "term", io, None, (200, 200))?);
    } else {
        eprintln!("atrium-term-socket: waiting for consoles");
    }
    let mut ever = !sessions.is_empty();
    // On its own (no console on its way): the empty console, once.
    let alone_at = channel.is_some().then(|| std::time::Instant::now() + ALONE_AFTER);
    let mut asked_alone = false;
    // A switch asked for: (window, target); its console replaces that
    // window's shell when it comes.
    let mut pending: Option<(u32, String)> = None;
    let short = libc::timespec { tv_sec: 0, tv_nsec: 200_000_000 };

    let kq = unsafe { libc::kqueue() };
    if kq < 0 { return Err(io::Error::last_os_error().into()); }
    let sock_fd = conn.as_raw_fd();
    register_read(kq, sock_fd)?;
    let chan_fd = channel.as_ref().map(|c| c.as_raw_fd());
    if let Some(fd) = chan_fd {
        register_read(kq, fd)?;
    }
    for s in &sessions {
        register_read(kq, s.io.fd())?;
    }
    for s in sessions.iter_mut() {
        draw(&mut conn, s, trace)?;
    }

    let mut events: [MaybeUninit<libc::kevent>; 16] =
        unsafe { MaybeUninit::uninit().assume_init() };
    let mut buf = vec![0u8; 65536];
    // With nothing open after the last console closed, wait this long for
    // another before quitting (one asked for may be on its way).
    let idle = libc::timespec { tv_sec: 5, tv_nsec: 0 };

    loop {
        let waiting_alone = !ever && !asked_alone && alone_at.is_some();
        let timeout = if waiting_alone { &short as *const _ } else if ever && sessions.is_empty() { &idle as *const _ } else { ptr::null() };
        let n = unsafe {
            libc::kevent(kq, ptr::null(), 0,
                events.as_mut_ptr() as *mut libc::kevent, events.len() as libc::c_int, timeout)
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted { continue; }
            return Err(e.into());
        }
        if waiting_alone && alone_at.is_some_and(|t| std::time::Instant::now() >= t) {
            asked_alone = true;
            // Nothing to reach, so no press is needed.
            match ask("empty", None, false, [0; 16]) {
                Ok(()) => eprintln!("atrium-term-socket: on its own: the empty console"),
                Err(e) => {
                    // Nothing to show and nothing coming: don't linger unseen.
                    eprintln!("atrium-term-socket: the empty console: {e}");
                    if sessions.is_empty() {
                        break;
                    }
                }
            }
            continue;
        }
        if n == 0 && sessions.is_empty() && !waiting_alone {
            break;
        }

        let mut closed: Vec<u32> = Vec::new();
        for i in 0..n as usize {
            let ev = unsafe { events[i].assume_init() };
            let fd = ev.ident as i32;
            if Some(fd) == chan_fd {
                let ch = channel.as_mut().unwrap();
                match ch.recv(Some(Duration::ZERO)) {
                    Ok(Some(o)) => {
                        let app = param(&o.mime, "app").map(str::to_string);
                        let io = if o.mime.starts_with(STOA_MIME) {
                            let (Some(session), Some(key)) = (param(&o.mime, "session"), param(&o.mime, "key").and_then(stoa::from_hex)) else {
                                eprintln!("atrium-term-socket: “{}”: a Stoa console without its session or key", o.name);
                                continue;
                            };
                            let sock = UnixDatagram::from(std::os::fd::OwnedFd::from(o.file));
                            sock.set_nonblocking(true)?;
                            stoa::link::bufs(&sock);
                            Io::Stoa { sock, key, tx: 0, rx: ReplayWindow::new(), session: session.into(), grid: stoa_term::Grid::new(COLS, ROWS), last: None }
                        } else if o.mime.starts_with(CONSOLE_MIME) {
                            let shell = pty::Shell::from_master(std::os::fd::OwnedFd::from(o.file));
                            Io::Pty { shell, term: stoa_term::Terminal::new(COLS, ROWS) }
                        } else {
                            eprintln!("atrium-term-socket: “{}” ({}) is not a console; ignored", o.name, o.mime);
                            continue;
                        };
                        // The console a switch asked for: this window's shell now.
                        if let Some((w, _)) = pending.take_if(|(_, t)| Some(t.as_str()) == app.as_deref()) {
                            if let Some(s) = sessions.iter_mut().find(|s| s.win == w) {
                                let (cols, rows) = (s.io.grid().cols(), s.io.grid().rows());
                                let _ = unregister(kq, s.io.fd());
                                // The old one detaches (Stoa) or ends (a bare pty).
                                let mut io = io;
                                if let Io::Stoa { grid, .. } = &mut io {
                                    grid.resize(cols, rows);
                                }
                                io.control(Control::SyncMode);
                                io.resize(cols, rows);
                                s.io = io;
                                register_read(kq, s.io.fd())?;
                                eprintln!("atrium-term-socket: window {} now “{}”", s.win, o.name);
                                s.title = o.name.clone();
                                s.app = app;
                                s.strip.place = place_name(s.app.as_deref());
                                s.close_switcher();
                                draw(&mut conn, s, trace)?;
                                continue;
                            }
                        }
                        let k = sessions.len() as i32;
                        let s = open_window(&mut conn, &font, &o.name, io, app, (200 + 32 * k, 160 + 32 * k))?;
                        register_read(kq, s.io.fd())?;
                        sessions.push(s);
                        ever = true;
                        let s = sessions.last_mut().unwrap();
                        draw(&mut conn, s, trace)?;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        eprintln!("atrium-term-socket: the document channel: {e}");
                        let _ = unregister(kq, fd);
                        channel = None;
                    }
                }
            } else if fd == sock_fd {
                while let Some(input) = conn.poll_event()? {
                    let Some(s) = sessions.iter_mut().find(|s| match &input {
                        Event::Key { window_id, .. } | Event::Resized { window_id, .. } | Event::CloseRequested { window_id }
                        | Event::PointerButton { window_id, .. } => *window_id == s.win,
                        _ => false,
                    }) else { continue };
                    // The switcher: a press on a row (or Enter) moves this
                    // window there.
                    let mut choose: Option<(usize, [u8; 16])> = None;
                    // Modifiers always reach the keymap, or a Ctrl let go
                    // while the switcher had the keys would stay held.
                    if let Event::Key { hid_usage: h @ 0xe0..=0xe7, pressed, .. } = &input {
                        if s.strip.menu.is_some() {
                            let _ = s.keymap.handle(*h, *pressed);
                            continue;
                        }
                    }
                    let mut taken = false;
                    match &input {
                        Event::PointerButton { pressed: true, x, y, activation, .. } if s.app.is_some() => {
                            let hit = s.strip.menu.as_ref().and_then(|(items, _)| (0..items.len()).find(|&i| {
                                let (rx, ry, rw, rh) = s.renderer.menu_row(i);
                                *x >= rx && *x < rx + rw && *y >= ry && *y < ry + rh
                            }));
                            if let Some(i) = hit {
                                choose = Some((i, *activation));
                            } else if *y < s.renderer.top && s.strip.menu.is_none() {
                                s.open_switcher();
                            } else if s.strip.menu.is_some() {
                                s.close_switcher();
                            }
                        }
                        Event::Key { hid_usage: HID_O, pressed: true, modifiers, repeat: false, .. }
                            if modifiers & 0x11 != 0 && modifiers & 0x22 != 0 && s.app.is_some() =>
                        {
                            if s.strip.menu.is_some() { s.close_switcher() } else { s.open_switcher() }
                            taken = true;
                        }
                        Event::Key { hid_usage, pressed: true, activation, repeat, .. } if s.strip.menu.is_some() => {
                            let n = s.strip.menu.as_ref().map(|m| m.0.len()).unwrap_or(0);
                            let sel = s.strip.menu.as_ref().map(|m| m.1).unwrap_or(0);
                            match *hid_usage {
                                HID_UP => { if let Some(m) = &mut s.strip.menu { m.1 = sel.saturating_sub(1) } s.dirty = true }
                                HID_DOWN => { if let Some(m) = &mut s.strip.menu { m.1 = (sel + 1).min(n.saturating_sub(1)) } s.dirty = true }
                                HID_ENTER if !*repeat => choose = Some((sel, *activation)),
                                HID_ESC => s.close_switcher(),
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                    if let Some((i, activation)) = choose {
                        let target = s.targets.get(i).cloned().unwrap_or_default();
                        s.close_switcher();
                        if target.is_empty() || Some(target.as_str()) == s.app.as_deref() {
                            continue;
                        }
                        pending = Some((s.win, target.clone()));
                        match ask(&target, None, false, activation) {
                            Ok(()) => eprintln!("atrium-term-socket: window {} switching to {target}", s.win),
                            Err(e) => {
                                eprintln!("atrium-term-socket: switch to {target}: {e}");
                                pending = None;
                                s.strip.hint = format!("not switched: {e}");
                                s.dirty = true;
                            }
                        }
                        continue;
                    }
                    if taken || (s.strip.menu.is_some() && matches!(input, Event::Key { .. } | Event::PointerButton { .. })) {
                        continue;
                    }
                    match input {
                        Event::Key { hid_usage, pressed: true, modifiers, activation, repeat: false, .. }
                            if modifiers & 0x11 != 0 && modifiers & 0x22 != 0 && (hid_usage == HID_T || hid_usage == HID_N) && s.app.is_some() =>
                        {
                            // Ctrl+Shift+T: a tab (Stoa only; a window otherwise);
                            // Ctrl+Shift+N: a window.
                            let app = s.app.clone().unwrap();
                            let tab_of = match &s.io {
                                Io::Stoa { session, .. } if hid_usage == HID_T => Some(session.clone()),
                                _ => None,
                            };
                            if let Err(e) = ask(&app, tab_of, true, activation) {
                                eprintln!("atrium-term-socket: more of {app}: {e}");
                            }
                        }
                        Event::Key { hid_usage: h @ (HID_PAGE_UP | HID_PAGE_DOWN), pressed: true, modifiers, .. }
                            if modifiers & 0x11 != 0 && matches!(s.io, Io::Stoa { .. }) =>
                        {
                            s.io.control(if h == HID_PAGE_DOWN { Control::NextWindow } else { Control::PrevWindow });
                        }
                        Event::Key { hid_usage, pressed, .. } => {
                            if let Some(bytes) = s.keymap.handle(hid_usage, pressed) {
                                s.io.input(&bytes);
                            }
                        }
                        // The WM's size, in points: as many cells as fit,
                        // and the shell told (SIGWINCH); a new scale only
                        // changes how it is drawn.
                        Event::Resized { width, height, scale, .. } => {
                            if scale > 0.0 {
                                s.renderer.k = scale;
                            }
                            let (cols, rows) = s.renderer.grid_for(width, height);
                            let g = s.io.grid();
                            if (cols, rows) != (g.cols(), g.rows()) {
                                if let Io::Stoa { grid, .. } = &mut s.io {
                                    grid.resize(cols, rows);
                                }
                                s.io.resize(cols, rows);
                            }
                            s.dirty = true;
                        }
                        // Closing the window: a Stoa session detaches (its
                        // shells run on); a bare pty's shell gets SIGHUP.
                        Event::CloseRequested { window_id } => closed.push(window_id),
                        _ => {}
                    }
                }
            } else if let Some(s) = sessions.iter_mut().find(|s| s.io.fd() == fd) {
                match s.io.read(&mut buf) {
                    Ok(true) => s.dirty = true,
                    Ok(false) => {}
                    Err(()) => closed.push(s.win),
                }
                if matches!(s.io, Io::Pty { .. }) && (ev.flags as u32) & (libc::EV_EOF as u32) != 0 {
                    closed.push(s.win);
                }
            }
        }

        for s in sessions.iter_mut().filter(|s| s.dirty) {
            draw(&mut conn, s, trace)?;
        }
        closed.sort();
        closed.dedup();
        for w in closed {
            if let Some(i) = sessions.iter().position(|s| s.win == w) {
                let s = sessions.remove(i);
                let _ = unregister(kq, s.io.fd());
                eprintln!("atrium-term-socket: “{}” closed", s.title);
                let _ = conn.window_destroy(s.win);
            }
        }
        // Outside Portcullis, the one shell ending ends the terminal.
        if sessions.is_empty() && channel.is_none() {
            break;
        }
    }

    let _ = unsafe { libc::close(kq) };
    Ok(())
}

fn unregister(kq: i32, fd: i32) -> io::Result<()> {
    let chg = libc::kevent {
        ident:  fd as _,
        filter: libc::EVFILT_READ,
        flags:  libc::EV_DELETE,
        fflags: 0,
        data:   0,
        udata:  ptr::null_mut(),
        ext:    [0; 4],
    };
    let r = unsafe { libc::kevent(kq, &chg as *const _, 1, ptr::null_mut(), 0, ptr::null()) };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

fn register_read(kq: i32, fd: i32) -> io::Result<()> {
    let chg = libc::kevent {
        ident:  fd as _,
        filter: libc::EVFILT_READ,
        flags:  libc::EV_ADD,
        fflags: 0,
        data:   0,
        udata:  ptr::null_mut(),
        ext:    [0; 4],
    };
    let r = unsafe {
        libc::kevent(kq, &chg as *const _, 1, ptr::null_mut(), 0, ptr::null())
    };
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}
