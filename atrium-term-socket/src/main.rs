//! atrium-term-socket — terminal emulator on the FreeBSD-native
//! Atrium stack, ported to fresco-client (M3d).
//!
//! Forks /bin/sh over a pty, feeds output through a VTE parser into a
//! cell grid, renders the grid as cursor (RECT) + per-cell TEXTURE
//! nodes. Server-pushed EV_INPUT_KEY events translate to ASCII bytes
//! via the unchanged keymap and write to the pty.
//!
//! kqueue multiplexes pty master fd + compositor socket fd: one
//! kevent() wakes us on either pty output OR a server input event.
//!
//! **Consoles** (docs/spec/homes.md §4): run by Portcullis as the system
//! terminal, it starts no shell of its own. Each console portcullisd opens
//! (a shell in an app's jail) arrives on the document channel as its pty's
//! master side, and gets a window. Ctrl+Shift+N asks portcullisd for
//! another console in the same app, with that key press as the proof.

mod grid;
mod keymap;
mod pty;
mod render;

use std::ffi::OsStr;
use std::io;
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::ptr;
use std::time::Duration;

use fresco_client::{Connection, Event};
use fresco_protocol::WindowHints;

const FONT_NAME:    &str = "system-mono";
/// In points, like everything this lays out (resolution-independence.md §3).
const FONT_SIZE_PX: f32  = 18.0;
const COLS: u16 = 80;
const ROWS: u16 = 24;
const WIN_W: u32 = 880;
const WIN_H: u32 = 540;

struct Session {
    win:      u32,
    title:    String,
    /// The app this is a console of (more may be asked for), if any.
    app:      Option<String>,
    shell:    pty::Shell,
    grid:     grid::Grid,
    parser:   vte::Parser,
    keymap:   keymap::Keymap,
    renderer: render::Renderer,
    /// What the trace last printed.
    traced:   String,
}

struct Font { id: u32, size: f32, cell_w: f32, line_h: f32, ascent: f32 }

const CONSOLE_MIME: &str = "application/x-atrium-console";
const HID_N: u16 = 0x11;

fn open_window(conn: &mut Connection, font: &Font, title: &str, shell: pty::Shell, app: Option<String>, at: (i32, i32)) -> io::Result<Session> {
    let win = conn.window_create(WIN_W, WIN_H, title, WindowHints {
        initial_position: Some(at),
        ..Default::default()
    })?;
    let mut renderer = render::Renderer::new(font.id, font.size, font.cell_w, font.line_h, font.ascent);
    renderer.k = conn.window_scale(win);
    eprintln!("atrium-term-socket: window {win} “{title}”");
    let s = Session { win, title: title.into(), app, shell, grid: grid::Grid::new(COLS, ROWS), parser: vte::Parser::new(),
                      keymap: keymap::Keymap::new(), renderer, traced: String::new() };
    let _ = s.shell.resize(COLS, ROWS);
    Ok(s)
}

fn draw(conn: &mut Connection, s: &mut Session, trace: bool) -> io::Result<()> {
    conn.set_default_window(s.win as u16);
    s.renderer.render(conn, &s.grid)?;
    s.grid.dirty = false;
    if trace {
        let cells = s.grid.cells();
        let rows: Vec<String> = (0..s.grid.rows as usize)
            .map(|r| cells[r * s.grid.cols as usize..(r + 1) * s.grid.cols as usize].iter().map(|c| c.ch).collect::<String>().trim_end().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        let t = rows.join(" | ");
        if t != s.traced {
            eprintln!("atrium-term: screen “{}”: {t}", s.title);
            s.traced = t;
        }
    }
    Ok(())
}

/// Ask portcullisd for another console in `app`, `activation` the press.
fn ask_more(app: &str, activation: [u8; 16]) -> Result<(), String> {
    use portcullis_ipc::{round_trip, Request, Response, PROTO_VERSION};
    let path = portcullis_ipc::resolve_socket_path().ok_or("portcullisd is not reachable")?;
    let mut s = std::os::unix::net::UnixStream::connect(path).map_err(|e| e.to_string())?;
    match round_trip(&mut s, &Request::Hello { version: PROTO_VERSION }) {
        Ok(Response::Hello { .. }) => {}
        other => return Err(format!("handshake: {other:?}")),
    }
    match round_trip(&mut s, &Request::Console { app_id: app.into(), activation }) {
        Ok(Response::ConsoleOpened { .. }) => Ok(()),
        Ok(Response::Error { message }) => Err(message),
        other => Err(format!("{other:?}")),
    }
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
        sessions.push(open_window(&mut conn, &font, "term", shell, None, (200, 200))?);
    } else {
        eprintln!("atrium-term-socket: waiting for consoles");
    }
    let mut ever = !sessions.is_empty();

    let kq = unsafe { libc::kqueue() };
    if kq < 0 { return Err(io::Error::last_os_error().into()); }
    let sock_fd = conn.as_raw_fd();
    register_read(kq, sock_fd)?;
    let chan_fd = channel.as_ref().map(|c| c.as_raw_fd());
    if let Some(fd) = chan_fd {
        register_read(kq, fd)?;
    }
    for s in &sessions {
        register_read(kq, s.shell.master.as_raw_fd())?;
    }
    for s in sessions.iter_mut() {
        draw(&mut conn, s, trace)?;
    }

    let mut events: [MaybeUninit<libc::kevent>; 16] =
        unsafe { MaybeUninit::uninit().assume_init() };
    let mut pty_buf = [0u8; 4096];
    // With nothing open after the last console closed, wait this long for
    // another before quitting (one asked for may be on its way).
    let idle = libc::timespec { tv_sec: 5, tv_nsec: 0 };

    loop {
        let timeout = if ever && sessions.is_empty() { &idle as *const _ } else { ptr::null() };
        let n = unsafe {
            libc::kevent(kq, ptr::null(), 0,
                events.as_mut_ptr() as *mut libc::kevent, events.len() as libc::c_int, timeout)
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted { continue; }
            return Err(e.into());
        }
        if n == 0 && sessions.is_empty() {
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
                        let Some(rest) = o.mime.strip_prefix(CONSOLE_MIME) else {
                            eprintln!("atrium-term-socket: “{}” ({}) is not a console; ignored", o.name, o.mime);
                            continue;
                        };
                        let app = rest.trim_start_matches(';').trim().strip_prefix("app=").map(str::to_string);
                        let shell = pty::Shell::from_master(std::os::fd::OwnedFd::from(o.file));
                        let k = sessions.len() as i32;
                        let s = open_window(&mut conn, &font, &o.name, shell, app, (200 + 32 * k, 160 + 32 * k))?;
                        register_read(kq, s.shell.master.as_raw_fd())?;
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
                        Event::Key { window_id, .. } | Event::Resized { window_id, .. } | Event::CloseRequested { window_id } => *window_id == s.win,
                        _ => false,
                    }) else { continue };
                    match input {
                        // Ctrl+Shift+N: another console in this app.
                        Event::Key { hid_usage: HID_N, pressed: true, modifiers, activation, repeat: false, .. }
                            if modifiers & 0x11 != 0 && modifiers & 0x22 != 0 && s.app.is_some() =>
                        {
                            let app = s.app.clone().unwrap();
                            if let Err(e) = ask_more(&app, activation) {
                                eprintln!("atrium-term-socket: another console in {app}: {e}");
                            }
                        }
                        Event::Key { hid_usage, pressed, .. } => {
                            if let Some(bytes) = s.keymap.handle(hid_usage, pressed) {
                                let _ = s.shell.write(&bytes);
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
                            if (cols, rows) != (s.grid.cols, s.grid.rows) {
                                s.grid.resize(cols, rows);
                                let _ = s.shell.resize(cols, rows);
                            }
                            s.grid.dirty = true;
                        }
                        // Closing the window ends its console (the shell
                        // gets SIGHUP when the pty's master closes).
                        Event::CloseRequested { window_id } => closed.push(window_id),
                        _ => {}
                    }
                }
            } else if let Some(s) = sessions.iter_mut().find(|s| s.shell.master.as_raw_fd() == fd) {
                loop {
                    match s.shell.read(&mut pty_buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            for &b in &pty_buf[..n] {
                                s.parser.advance(&mut s.grid, b);
                            }
                            s.grid.dirty = true;
                        }
                        Err(_) => { closed.push(s.win); break; }
                    }
                }
                if (ev.flags as u32) & (libc::EV_EOF as u32) != 0 {
                    closed.push(s.win);
                }
            }
        }

        for s in sessions.iter_mut().filter(|s| s.grid.dirty) {
            draw(&mut conn, s, trace)?;
        }
        closed.sort();
        closed.dedup();
        for w in closed {
            if let Some(i) = sessions.iter().position(|s| s.win == w) {
                let s = sessions.remove(i);
                let _ = unregister(kq, s.shell.master.as_raw_fd());
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
