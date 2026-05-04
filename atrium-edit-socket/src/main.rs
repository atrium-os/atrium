//! atrium-edit-socket — port of atrium-edit to the FreeBSD-native
//! Atrium stack via fresco-socket-rs.
//!
//! Reuses atrium-edit's buffer + keymap modules unchanged. The
//! renderer is rewritten to emit per-glyph textured RenderItems
//! against the existing tiny_skia_backend (no per-vertex UV needed).
//!
//! Interactive: receives `Event::Key` from the server (today via
//! `atrium-keyboard` injecting over CMD_INJECT_KEY; tomorrow via
//! `/dev/usbhid` plumbed through frescod). Routes through
//! the existing keymap → buffer-mutation → re-render loop.

mod buffer;
mod glyph_cache;
mod keymap;
mod render;

use std::io;
use std::path::PathBuf;

use fresco_socket::{Connection, Event};

use crate::keymap::Action;

const FONT_PATH:    &str   = "/mnt/host/test-assets/DejaVuSansMono.ttf";
const FONT_SIZE_PX: f32    = 18.0;
const VIEWPORT_ROWS: usize = 24;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path: Option<PathBuf> = std::env::args().nth(1).map(PathBuf::from);
    let mut buf = match path.as_ref() {
        Some(p) => buffer::Buffer::open(p)?,
        None    => buffer::Buffer::empty(),
    };
    eprintln!("buffer: {} lines (path={:?})", buf.lines.len(), buf.path);

    let font = std::fs::read(FONT_PATH)?;

    let sock = std::env::var("FRESCOD_SOCK")
        .unwrap_or_else(|_| "/tmp/frescod.sock".to_string());
    let mut conn = Connection::connect(&sock)?;
    eprintln!("connected to {sock}");

    // Create our own window so the compositor's per-window FBO
    // pipeline can composite us alongside other apps. Without this
    // we'd render into the screen scene (window 0) and the last
    // SET_ROOT writer would win.
    const WIN_W: u32 = 720;
    const WIN_H: u32 = 540;
    let win = conn.create_window(WIN_W, WIN_H, Some("edit"))?;
    let _ = conn.window_set_pos(win as u16, 60.0, 60.0);
    conn.set_default_window(win as u16);
    eprintln!("window {win} created — {WIN_W}x{WIN_H}");

    let cache    = glyph_cache::GlyphCache::build(&mut conn, &font, FONT_SIZE_PX)?;
    let renderer = render::Renderer::new(&cache, &mut conn)?;
    let mut keymap = keymap::Keymap::new();

    renderer.render(&mut conn, &buf, VIEWPORT_ROWS, /*cursor_visible=*/true)?;
    eprintln!("rendered initial view; now waiting for events");

    let mut alive = true;
    while alive {
        let ev = conn.wait_event(None)?;
        let mut dirty = false;

        // Drain the burst — multiple keystrokes shouldn't trigger
        // multiple renders.
        let mut next = ev;
        loop {
            match next {
                Some(Event::CloseRequested { .. }) => alive = false,
                Some(Event::Key { hid_usage, pressed, modifiers, .. }) => {
                    keymap.shift = modifiers & 0x01 != 0;
                    keymap.ctrl  = modifiers & 0x02 != 0;
                    if let Some(action) = keymap.handle(hid_usage, pressed) {
                        if apply(&mut buf, action, &mut alive)? {
                            dirty = true;
                        }
                    }
                }
                Some(_) => {}
                None    => break,
            }
            if !alive { break; }
            next = conn.poll_event()?;
        }

        if dirty {
            buf.scroll_into_view(VIEWPORT_ROWS);
            renderer.render(&mut conn, &buf, VIEWPORT_ROWS, /*cursor_visible=*/true)?;
        }
    }
    eprintln!("exiting");
    Ok(())
}

fn apply(buf: &mut buffer::Buffer, a: Action, alive: &mut bool) -> io::Result<bool> {
    Ok(match a {
        Action::Insert(c)   => { buf.insert_char(c); true }
        Action::Newline     => { buf.newline(); true }
        Action::Backspace   => { buf.backspace(); true }
        Action::Tab         => { buf.insert_char('\t'); true }
        Action::Left        => { buf.move_left();  true }
        Action::Right       => { buf.move_right(); true }
        Action::Up          => { buf.move_up();    true }
        Action::Down        => { buf.move_down();  true }
        Action::Save        => match buf.save() {
            Ok(())  => true,
            Err(e)  => { buf.status = format!("[save error: {e}]"); true }
        },
        Action::Quit        => { *alive = false; false }
    })
}
