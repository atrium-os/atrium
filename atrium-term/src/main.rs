//! atrium-term — terminal emulator on Fresco (multi-window).
//!
//! Each instance creates its own decorated window, spawns a /bin/sh
//! pty, and renders the cell grid into the window's FBO. Input
//! events are filtered by target_window so two instances side by
//! side don't cross-talk. Close button (or shell exit) cleans up.
//!
//! Pure Rust + libfresco + libc. No GTK/Pango/Cairo/freetype/evdev.

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

use fresco_rs::{Connection, Event};

const FONT_PATH: &str = "/mnt/host/test-assets/DejaVuSansMono.ttf";
const FONT_SIZE_PX: f32 = 18.0;
const COLS: u16 = 80;
const ROWS: u16 = 24;
const WIN_W: u32 = 900;
const WIN_H: u32 = 540;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let font = std::fs::read(FONT_PATH)
        .map_err(|e| format!("read {FONT_PATH}: {e}"))?;
    let cache = glyph_cache::GlyphCache::build(&font, FONT_SIZE_PX)?;
    eprintln!("glyph cache: {}x{} atlas, cell={:.1}x{:.1}px",
        cache.atlas_w, cache.atlas_h, cache.cell_w, cache.line_height);

    let conn = Connection::open()?;
    eprintln!("connected — slot={}", conn.client_slot());

    // ── Create our window ────────────────────────────────────────
    let win = conn.create_window(WIN_W, WIN_H, Some("term"))?;
    let off = conn.client_slot() as f32 * 60.0;
    conn.window_set_pos(win, 60.0 + off, 60.0 + off)?;
    conn.set_default_window(win);
    eprintln!("window {win} created — {WIN_W}x{WIN_H}");

    let shell = pty::Shell::spawn(OsStr::new("/bin/sh"), &["-i"], COLS, ROWS)?;
    eprintln!("spawned /bin/sh, pid={}", shell.pid);

    let mut grid     = grid::Grid::new(COLS, ROWS);
    let mut parser   = vte::Parser::new();
    let mut keymap   = keymap::Keymap::new();
    let mut renderer = render::Renderer::new(&cache, /*slot*/ 1, WIN_W, WIN_H);

    // First render — empty grid, sets up the slot graph and camera so
    // subsequent re-renders are mesh-only.
    renderer.render(&conn, &grid)?;

    // Drain any stale events queued before we attached.
    while let Some(_) = conn.poll_event() {}

    // ── kqueue event loop ────────────────────────────────────────
    let kq = unsafe { libc::kqueue() };
    if kq < 0 { return Err(io::Error::last_os_error().into()); }
    let pty_fd    = shell.master.as_raw_fd();
    let fresco_fd = conn.as_raw_fd();
    register_read(kq, pty_fd)?;
    register_read(kq, fresco_fd)?;

    let mut events: [MaybeUninit<libc::kevent>; 16] = unsafe { MaybeUninit::uninit().assume_init() };
    let mut buf = [0u8; 4096];
    let mut alive = true;

    while alive {
        let n = unsafe {
            libc::kevent(kq,
                ptr::null(), 0,
                events.as_mut_ptr() as *mut libc::kevent, events.len() as libc::c_int,
                ptr::null())
        };
        if n < 0 {
            return Err(io::Error::last_os_error().into());
        }
        for i in 0..n as usize {
            let ev = unsafe { events[i].assume_init() };
            let fd = ev.ident as i32;
            if fd == pty_fd {
                // Drain pty output until EAGAIN.
                loop {
                    match shell.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            for &b in &buf[..n] {
                                parser.advance(&mut grid, b);
                            }
                        }
                        Err(_) => { alive = false; break; }
                    }
                }
                if (ev.flags as u32) & (libc::EV_EOF as u32) != 0 {
                    alive = false;
                }
            } else if fd == fresco_fd {
                while let Some(input) = conn.poll_event() {
                    match input {
                        Event::Key { keysym, pressed, target_window } => {
                            // Per-slot input rings already isolate by
                            // owner, but additional filter by our
                            // window_id keeps us robust if focus
                            // strays to (0 = screen).
                            if target_window == 0 || target_window == win as u32 {
                                if let Some(bytes) = keymap.handle(keysym, pressed) {
                                    let _ = shell.write(&bytes);
                                }
                            }
                        }
                        Event::CloseRequested { window_id } if window_id == win as u32 => {
                            eprintln!("close requested — exiting");
                            alive = false;
                        }
                        Event::WindowResized { window_id, width, height } if window_id == win as u32 => {
                            // Recompute cols/rows from the new pixel
                            // dims and the (fixed) cell size; resize
                            // the grid + pty so the shell's SIGWINCH
                            // and curses apps (vi, less) re-layout.
                            let new_cols = ((width as f32 / cache.cell_w).floor() as u16).max(8);
                            let new_rows = ((height as f32 / cache.line_height).floor() as u16).max(2);
                            eprintln!("resized to {width}x{height}px → {new_cols}x{new_rows} cells");
                            grid.resize(new_cols, new_rows);
                            let _ = shell.resize(new_cols, new_rows);
                            renderer.set_view_size(width, height);
                            grid.dirty = true;
                        }
                        _ => {}
                    }
                }
            }
        }

        if grid.dirty {
            renderer.render(&conn, &grid)?;
            grid.dirty = false;
        }
    }

    eprintln!("shell exited — sleeping briefly so the last frame is visible");
    std::thread::sleep(Duration::from_secs(2));
    let _ = conn.destroy_window(win);
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
