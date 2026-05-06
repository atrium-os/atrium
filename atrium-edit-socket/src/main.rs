//! atrium-edit-socket — minimal text editor on the Atrium FreeBSD
//! stack, on the M6.3 server-side text path.
//!
//! Server owns the font + atlas + shaper. Per-frame the editor sends
//! one OP_TEXT_RUN_INSTALL per visible line + one for the status bar
//! and a single OP_SCENE_NODE_SET (RECT) for the cursor. No font file,
//! no rustybuzz, no swash.

mod buffer;
mod keymap;
mod render;

use std::io;
use std::path::PathBuf;

use fresco_client::{Connection, Event};
use fresco_protocol::WindowHints;

use crate::keymap::Action;

const FONT_NAME:    &str   = "system-mono";
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

    let sock = std::env::var("FRESCOD_SOCK")
        .unwrap_or_else(|_| "/tmp/frescod.sock".to_string());
    let mut conn = Connection::connect(&sock)?;
    eprintln!("connected to {sock}");

    let win = conn.window_create(WIN_W, WIN_H, "edit", WindowHints {
        initial_position: Some((60, 60)),
        ..Default::default()
    })?;
    eprintln!("window {win} created — {WIN_W}x{WIN_H}");

    let font = conn.font_open(FONT_NAME)?;
    if font.font_id == 0 {
        return Err(format!("server could not open '{FONT_NAME}'").into());
    }
    let upe = font.units_per_em as f32;
    let ascent_px  = font.ascent_units  as f32 * FONT_SIZE_PX / upe;
    let descent_px = -(font.descent_units as f32) * FONT_SIZE_PX / upe;
    let cell_w     = if font.mono_advance_units > 0 {
        font.mono_advance_units as f32 * FONT_SIZE_PX / upe
    } else {
        FONT_SIZE_PX * 0.6 /* heuristic fallback for proportional */
    };
    let line_h = ascent_px + descent_px + 2.0;

    eprintln!(
        "font: {} (id={}) cell_w={:.1} line_h={:.1} baseline={:.1}",
        FONT_NAME, font.font_id, cell_w, line_h, ascent_px,
    );

    let renderer = render::Renderer::new(font.font_id, FONT_SIZE_PX,
                                         cell_w, line_h, ascent_px);
    let mut keymap = keymap::Keymap::new();

    renderer.render(&mut conn, &buf, VIEWPORT_ROWS, /*cursor_visible=*/true)?;
    eprintln!("rendered initial view; now waiting for events");

    let mut alive = true;
    while alive {
        let ev = conn.wait_event(None)?;
        let mut dirty = false;

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
