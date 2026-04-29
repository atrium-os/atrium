//! atrium-term-socket — terminal emulator on the FreeBSD-native
//! Atrium stack via fresco-socket-rs.
//!
//! Forks `/bin/sh` over a pty (`pty.rs`), feeds its output through a
//! VTE parser into a cell grid (`grid.rs`), and renders the grid into
//! the compositor's scene through fresco-socket-rs. Keystrokes from
//! the compositor's native input pipeline (Event::Key, HID Usage) get
//! translated into ASCII bytes (`keymap.rs`) and written to the pty.
//!
//! kqueue multiplexes the pty master fd with the compositor socket fd
//! — one `kevent()` wakes us on either pty output OR a server event.

mod glyph_cache;
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

use fresco_socket::{Connection, Event};

const FONT_PATH:    &str = "/mnt/host/test-assets/DejaVuSansMono.ttf";
const FONT_SIZE_PX: f32  = 18.0;
const COLS: u16 = 80;
const ROWS: u16 = 24;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let font = std::fs::read(FONT_PATH)
        .map_err(|e| format!("read {FONT_PATH}: {e}"))?;

    let sock = std::env::var("ATRIUM_COMPOSITOR_SOCK")
        .unwrap_or_else(|_| "/tmp/atrium-compositor.sock".to_string());
    let mut conn = Connection::connect(&sock)?;
    eprintln!("atrium-term-socket: connected to {sock}");

    // Per-window FBO: create our window, stagger position so we
    // don't overlap the editor when launched together, mark as the
    // default route for subsequent SET_ROOT/SLOT_*/FRAME_* commands.
    const WIN_W: u32 = 880;
    const WIN_H: u32 = 540;
    let win = conn.create_window(WIN_W, WIN_H, Some("term"))?;
    let _ = conn.window_set_pos(win as u16, 200.0, 200.0);
    conn.set_default_window(win as u16);
    eprintln!("atrium-term-socket: window {win} created — {WIN_W}x{WIN_H}");

    let cache = glyph_cache::GlyphCache::build(&mut conn, &font, FONT_SIZE_PX)?;
    let renderer = render::Renderer::new(&cache, &mut conn)?;

    let shell = pty::Shell::spawn(OsStr::new("/bin/sh"), &["-i"], COLS, ROWS)?;
    eprintln!("spawned /bin/sh pid={}", shell.pid);

    let mut grid     = grid::Grid::new(COLS, ROWS);
    let mut parser   = vte::Parser::new();
    let mut keymap   = keymap::Keymap::new();

    renderer.render(&mut conn, &grid)?;

    // ── kqueue: pty fd + compositor socket fd ────────────────────
    let kq = unsafe { libc::kqueue() };
    if kq < 0 { return Err(io::Error::last_os_error().into()); }
    let pty_fd  = shell.master.as_raw_fd();
    let sock_fd = conn.as_raw_fd();
    register_read(kq, pty_fd)?;
    register_read(kq, sock_fd)?;

    let mut events: [MaybeUninit<libc::kevent>; 16] =
        unsafe { MaybeUninit::uninit().assume_init() };
    let mut pty_buf = [0u8; 4096];
    let mut alive = true;

    while alive {
        let n = unsafe {
            libc::kevent(kq,
                ptr::null(), 0,
                events.as_mut_ptr() as *mut libc::kevent, events.len() as libc::c_int,
                ptr::null())
        };
        if n < 0 { return Err(io::Error::last_os_error().into()); }

        let mut dirty = false;
        for i in 0..n as usize {
            let ev = unsafe { events[i].assume_init() };
            let fd = ev.ident as i32;
            if fd == pty_fd {
                loop {
                    match shell.read(&mut pty_buf) {
                        Ok(0)  => break,
                        Ok(n)  => {
                            for &b in &pty_buf[..n] {
                                parser.advance(&mut grid, b);
                            }
                            dirty = true;
                        }
                        Err(_) => { alive = false; break; }
                    }
                }
                if (ev.flags as u32) & (libc::EV_EOF as u32) != 0 {
                    alive = false;
                }
            } else if fd == sock_fd {
                while let Some(input) = conn.poll_event()? {
                    match input {
                        Event::Key { hid_usage, pressed, .. } => {
                            if let Some(bytes) = keymap.handle(hid_usage, pressed) {
                                let _ = shell.write(&bytes);
                            }
                        }
                        Event::CloseRequested { .. } => {
                            eprintln!("close requested — exiting");
                            alive = false;
                        }
                        _ => {}
                    }
                }
            }
        }

        if dirty || grid.dirty {
            renderer.render(&mut conn, &grid)?;
            grid.dirty = false;
        }
    }

    eprintln!("atrium-term-socket: shell exited; sleeping 1s so the last frame is visible");
    std::thread::sleep(Duration::from_secs(1));
    let _ = unsafe { libc::close(kq) };
    Ok(())
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
