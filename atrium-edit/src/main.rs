//! atrium-edit — minimal text editor on Fresco.
//!
//! Multi-window port: each instance creates its own decorated window,
//! renders text into that window's FBO, and exits when the user
//! clicks the close button (or hits Ctrl+Q / Esc). Run multiple
//! copies side by side — each owns a distinct client slot and
//! renders independently.
//!
//! Pure Rust + libfresco + libc. No editor frameworks.

mod buffer;
mod glyph_cache;
mod keymap;
mod render;

use std::io;
use std::path::PathBuf;

use fresco_rs::{Connection, Event};

const FONT_PATH:    &str = "/mnt/host/test-assets/DejaVuSansMono.ttf";
const FONT_SIZE_PX: f32 = 18.0;
const VIEWPORT_ROWS: usize = 20;
const WIN_W: u32 = 700;
const WIN_H: u32 = 460;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path: Option<PathBuf> = std::env::args().nth(1).map(PathBuf::from);
    let mut buf = match path.as_ref() {
        Some(p) => buffer::Buffer::open(p)?,
        None    => buffer::Buffer::empty(),
    };
    eprintln!("buffer: {} lines (path={:?})", buf.lines.len(), buf.path);

    let font = std::fs::read(FONT_PATH)?;
    let cache = glyph_cache::GlyphCache::build(&font, FONT_SIZE_PX)?;

    let conn = Connection::open()?;
    eprintln!("connected — slot={}", conn.client_slot());

    // ── Create our window ────────────────────────────────────────
    let title = path.as_ref()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .map(|s| format!("edit: {s}"))
        .unwrap_or_else(|| "edit".to_string());
    let win = conn.create_window(WIN_W, WIN_H, Some(&title))?;
    // Stagger window position so multiple instances don't fully
    // overlap. Use the slot index (0..3) for 60 px diagonal offsets.
    let off = conn.client_slot() as f32 * 60.0;
    conn.window_set_pos(win, 80.0 + off, 80.0 + off)?;
    conn.set_default_window(win);

    eprintln!("window {win} created — {WIN_W}x{WIN_H} title={title:?}");

    let mut keymap   = keymap::Keymap::new();
    let mut renderer = render::Renderer::new(&cache, /*slot*/ 1, WIN_W, WIN_H);
    renderer.render(&conn, &buf, VIEWPORT_ROWS)?;

    // Drain stale events the host queued before we attached.
    let mut drained = 0;
    while let Some(_) = conn.poll_event() { drained += 1; }
    if drained > 0 { eprintln!("drained {drained} stale events"); }

    let mut alive = true;
    while alive {
        // Block until something happens — input event for our
        // window, close request, focus change, or our own redraw.
        let ev = conn.wait_event(-1)?;
        let mut dirty = false;

        // Drain everything that's queued in one pass before
        // re-rendering, so a burst of keystrokes only repaints once.
        let mut next = ev;
        loop {
            match next {
                Some(Event::Key { keysym, pressed, target_window }) => {
                    if target_window == 0 || target_window == win as u32 {
                        if let Some(action) = keymap.handle(keysym, pressed) {
                            if apply(&mut buf, action, &mut alive)? {
                                dirty = true;
                            }
                        }
                    }
                }
                Some(Event::CloseRequested { window_id }) if window_id == win as u32 => {
                    eprintln!("close requested");
                    alive = false;
                }
                Some(Event::WindowResized { window_id, width, height }) if window_id == win as u32 => {
                    eprintln!("window resized to {width}x{height}");
                    renderer.set_view_size(width, height);
                    dirty = true;
                }
                Some(_) => {}
                None    => break,
            }
            if !alive { break; }
            next = conn.poll_event();
        }

        if dirty {
            buf.scroll_into_view(VIEWPORT_ROWS);
            renderer.render(&conn, &buf, VIEWPORT_ROWS)?;
        }
    }

    let _ = conn.destroy_window(win);
    eprintln!("exiting");
    Ok(())
}

fn apply(buf: &mut buffer::Buffer, a: keymap::Action, alive: &mut bool)
    -> io::Result<bool>
{
    use keymap::Action::*;
    Ok(match a {
        Insert(c)  => { buf.insert_char(c); true }
        Newline    => { buf.newline(); true }
        Backspace  => { buf.backspace(); true }
        Tab        => { buf.insert_char('\t'); true }
        Left       => { buf.move_left();  true }
        Right      => { buf.move_right(); true }
        Up         => { buf.move_up();    true }
        Down       => { buf.move_down();  true }
        Save       => match buf.save() {
            Ok(())  => true,
            Err(e)  => { buf.status = format!("[save error: {e}]"); true }
        },
        Quit       => { *alive = false; false }
    })
}
