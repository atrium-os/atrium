//! atrium-edit-socket — minimal text editor on the Atrium FreeBSD
//! stack, ported to fresco-client (M3d).
//!
//! Reuses the original buffer + keymap modules unchanged. Renderer
//! emits per-glyph TEXTURE nodes against per-glyph slots and a single
//! RECT node for the cursor (no rotation needed → RECT not PATH).
//!
//! Interactive: server pushes EV_INPUT_KEY events (M3a — native
//! /dev/hidraw → DisplayEvent fan-out); we filter by window_id, run
//! through the existing keymap → buffer-mutation → re-render loop.

mod buffer;
use fresco_client::MonoAtlas;
mod keymap;
mod render;

use std::io;
use std::path::PathBuf;

use fresco_client::{Connection, Event};
use fresco_protocol::WindowHints;

use crate::keymap::Action;

const FONT_PATH:    &str   = "/mnt/host/test-assets/DejaVuSansMono.ttf";
const FONT_SIZE_PX: f32    = 18.0;
const VIEWPORT_ROWS: usize = 24;
const WIN_W: u32 = 720;
const WIN_H: u32 = 540;

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

    let win = conn.window_create(WIN_W, WIN_H, "edit", WindowHints {
        initial_position: Some((60, 60)),
        ..Default::default()
    })?;
    eprintln!("window {win} created — {WIN_W}x{WIN_H}");

    let cache    = MonoAtlas::build(&mut conn, &font, FONT_SIZE_PX, 100)?;
    let renderer = render::Renderer::new(&cache);
    let mut keymap = keymap::Keymap::new();

    renderer.render(&mut conn, &buf, VIEWPORT_ROWS, /*cursor_visible=*/true)?;
    eprintln!("rendered initial view; now waiting for events");

    let mut alive = true;
    while alive {
        let ev = conn.wait_event(None)?;
        let mut dirty = false;

        /* Drain a burst — multiple keystrokes shouldn't trigger
         * multiple renders. */
        let mut next = ev;
        loop {
            match next {
                Some(Event::CloseRequested { window_id }) if window_id == win => {
                    alive = false;
                }
                Some(Event::Key { hid_usage, pressed, modifiers, window_id })
                    if window_id == win =>
                {
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
