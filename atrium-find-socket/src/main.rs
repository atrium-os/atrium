//! atrium-find-socket — two-pane file browser on the Atrium FreeBSD-
//! native stack via fresco-socket-rs. Up/Down navigate, Enter
//! descends into directories, Backspace/Left goes up to parent,
//! Esc/Ctrl+Q quits. Vi-style hjkl also work.

mod dir;
mod glyph_cache;
mod keymap;
mod render;

use std::io;
use std::path::PathBuf;

use fresco_socket::{Connection, Event};

const FONT_PATH:    &str = "/mnt/host/test-assets/DejaVuSansMono.ttf";
const FONT_SIZE_PX: f32  = 18.0;
const PREVIEW_LINES: usize = 64;
const PREVIEW_BYTES: usize = 8192;
const WIN_W: u32 = 1000;
const WIN_H: u32 = 600;

struct State {
    cwd:        PathBuf,
    entries:    Vec<dir::Entry>,
    selected:   usize,
    scroll_top: usize,
    preview:    Vec<String>,
}

impl State {
    fn open(path: PathBuf) -> io::Result<Self> {
        let entries = dir::read(&path)?;
        let mut s = State { cwd: path, entries, selected: 0, scroll_top: 0, preview: vec![] };
        s.refresh_preview();
        Ok(s)
    }

    fn refresh_preview(&mut self) {
        let Some(ent) = self.entries.get(self.selected) else { self.preview.clear(); return };
        let path = dir::join(&self.cwd, &ent.name);
        if ent.is_dir {
            self.preview = vec![format!("(directory: {})", path.display())];
            return;
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => { self.preview = vec![format!("(error: {e})")]; return; }
        };
        let take = bytes.len().min(PREVIEW_BYTES);
        let head = &bytes[..take];
        if head.iter().any(|&b| b == 0) {
            self.preview = vec![format!("(binary, {} bytes)", bytes.len())];
            return;
        }
        let text = String::from_utf8_lossy(head);
        self.preview = text.lines().take(PREVIEW_LINES).map(String::from).collect();
        if self.preview.is_empty() { self.preview = vec!["(empty)".into()]; }
    }

    fn move_sel(&mut self, delta: isize, visible: usize) {
        if self.entries.is_empty() { return; }
        let n = self.entries.len() as isize;
        let s = (self.selected as isize + delta).clamp(0, n - 1) as usize;
        self.selected = s;
        let top = self.scroll_top;
        if s < top { self.scroll_top = s; }
        else if s >= top + visible { self.scroll_top = s + 1 - visible; }
        self.refresh_preview();
    }

    fn enter(&mut self) {
        let Some(ent) = self.entries.get(self.selected).cloned() else { return };
        if !ent.is_dir { return; }
        let new_path = dir::join(&self.cwd, &ent.name);
        if let Ok(es) = dir::read(&new_path) {
            self.cwd = new_path;
            self.entries = es;
            self.selected = 0;
            self.scroll_top = 0;
            self.refresh_preview();
        }
    }

    fn parent(&mut self) {
        if let Some(parent) = self.cwd.parent() {
            let parent = parent.to_path_buf();
            if let Ok(es) = dir::read(&parent) {
                self.cwd = parent;
                self.entries = es;
                self.selected = 0;
                self.scroll_top = 0;
                self.refresh_preview();
            }
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let start: PathBuf = std::env::args().nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| "/".into()));

    let mut state = State::open(start)?;
    eprintln!("atrium-find-socket: cwd={} ({} entries)", state.cwd.display(), state.entries.len());

    let font = std::fs::read(FONT_PATH)?;

    let sock = std::env::var("ATRIUM_COMPOSITOR_SOCK")
        .unwrap_or_else(|_| "/tmp/atrium-compositor.sock".to_string());
    let mut conn = Connection::connect(&sock)?;
    eprintln!("atrium-find-socket: connected to {sock}");

    let win = conn.create_window(WIN_W, WIN_H, Some("find"))?;
    let _ = conn.window_set_pos(win as u16, 100.0, 100.0);
    conn.set_default_window(win as u16);
    eprintln!("atrium-find-socket: window {win} created — {WIN_W}x{WIN_H}");

    let cache = glyph_cache::GlyphCache::build(&mut conn, &font, FONT_SIZE_PX)?;
    let renderer = render::Renderer::new(&cache, &mut conn, WIN_W, WIN_H)?;
    let mut keymap = keymap::Keymap::new();

    // Visible rows minus header(1) + gap(1) + footer(1) = list area.
    let visible_rows = ((WIN_H as f32 - 24.0) / cache.line_height).floor() as usize;
    let list_visible = visible_rows.saturating_sub(3);

    renderer.render(&mut conn, &state.cwd.display().to_string(),
                    &state.entries, state.selected, state.scroll_top, &state.preview)?;

    let mut alive = true;
    while alive {
        let ev = conn.wait_event(None)?;
        let mut dirty = false;
        let mut next = ev;
        loop {
            match next {
                Some(Event::CloseRequested { .. }) => alive = false,
                Some(Event::Key { hid_usage, pressed, .. }) => {
                    if let Some(action) = keymap.handle(hid_usage, pressed) {
                        use keymap::Action::*;
                        match action {
                            Up        => { state.move_sel(-1, list_visible); dirty = true; }
                            Down      => { state.move_sel( 1, list_visible); dirty = true; }
                            PageUp    => { state.move_sel(-(list_visible as isize), list_visible); dirty = true; }
                            PageDown  => { state.move_sel( list_visible as isize,   list_visible); dirty = true; }
                            Enter     => { state.enter(); dirty = true; }
                            ParentDir => { state.parent(); dirty = true; }
                            Quit      => { alive = false; }
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
            renderer.render(&mut conn, &state.cwd.display().to_string(),
                            &state.entries, state.selected, state.scroll_top, &state.preview)?;
        }
    }

    eprintln!("atrium-find-socket: exiting");
    Ok(())
}
