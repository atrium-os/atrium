//! atrium-find — two-pane file browser on Fresco.
//!
//! Vertical app #3. Patterns exposed beyond term/edit:
//!   - directory listing (readdir + sort)
//!   - selection state + scroll
//!   - two-pane layout (single mesh, multiple regions)
//!   - file preview (text content of currently-selected file)
//!
//! Up/Down navigate, Enter descends, Backspace/Left goes up,
//! Esc/Ctrl+Q quits. Vi-style hjkl also work.

mod dir;
mod glyph_cache;
mod keymap;
mod render;

use std::io;
use std::mem::MaybeUninit;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::ptr;

use fresco_rs::{Connection, Event};

const FONT_PATH: &str = "/mnt/host/test-assets/DejaVuSansMono.ttf";
const FONT_SIZE_PX: f32 = 18.0;
const PREVIEW_LINES: usize = 64;
const PREVIEW_BYTES: usize = 8192;

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
        // Read up to PREVIEW_BYTES, abort if it looks binary.
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

    fn move_sel(&mut self, delta: isize, visible_rows: usize) {
        if self.entries.is_empty() { return; }
        let n = self.entries.len() as isize;
        let s = (self.selected as isize + delta).clamp(0, n - 1) as usize;
        self.selected = s;
        // scroll
        let top = self.scroll_top;
        if s < top { self.scroll_top = s; }
        else if s >= top + visible_rows { self.scroll_top = s + 1 - visible_rows; }
        self.refresh_preview();
    }

    fn enter(&mut self, visible_rows: usize) {
        let Some(ent) = self.entries.get(self.selected).cloned() else { return };
        if !ent.is_dir { return; }
        let new_path = dir::join(&self.cwd, &ent.name);
        if let Ok(new_entries) = dir::read(&new_path) {
            self.cwd = new_path;
            self.entries = new_entries;
            self.selected = 0;
            self.scroll_top = 0;
            self.refresh_preview();
            let _ = visible_rows;
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
    eprintln!("cwd: {} ({} entries)", state.cwd.display(), state.entries.len());

    let font = std::fs::read(FONT_PATH)?;
    let cache = glyph_cache::GlyphCache::build(&font, FONT_SIZE_PX)?;

    let conn = Connection::open()?;
    let disp = conn.display();
    let visible_rows = (disp.height as f32 / cache.line_height) as usize;
    let list_visible = visible_rows.saturating_sub(3);  // header + footer + 1 gap

    let mut keymap   = keymap::Keymap::new();
    let mut renderer = render::Renderer::new(&cache, /*slot*/ 1);

    // Drain stale events.
    while conn.poll_event().is_some() {}

    renderer.render(&conn, &state.cwd.display().to_string(), &state.entries,
                    state.selected, state.scroll_top, &state.preview, visible_rows)?;

    let kq = unsafe { libc::kqueue() };
    if kq < 0 { return Err(io::Error::last_os_error().into()); }
    let chg = libc::kevent {
        ident: conn.as_raw_fd() as _,
        filter: libc::EVFILT_READ, flags: libc::EV_ADD,
        fflags: 0, data: 0, udata: ptr::null_mut(), ext: [0; 4],
    };
    unsafe { libc::kevent(kq, &chg, 1, ptr::null_mut(), 0, ptr::null()); }

    let mut events: [MaybeUninit<libc::kevent>; 16] =
        unsafe { MaybeUninit::uninit().assume_init() };
    let mut alive = true;

    while alive {
        let n = unsafe {
            libc::kevent(kq, ptr::null(), 0,
                events.as_mut_ptr() as *mut libc::kevent, events.len() as i32,
                ptr::null())
        };
        if n < 0 { return Err(io::Error::last_os_error().into()); }

        let mut dirty = false;
        while let Some(input) = conn.poll_event() {
            if let Event::Key { keysym, pressed } = input {
                if let Some(action) = keymap.handle(keysym, pressed) {
                    use keymap::Action::*;
                    match action {
                        Up         => { state.move_sel(-1, list_visible); dirty = true; }
                        Down       => { state.move_sel( 1, list_visible); dirty = true; }
                        PageUp     => { state.move_sel(-(list_visible as isize), list_visible); dirty = true; }
                        PageDown   => { state.move_sel( list_visible as isize,   list_visible); dirty = true; }
                        Enter      => { state.enter(list_visible); dirty = true; }
                        ParentDir  => { state.parent(); dirty = true; }
                        Quit       => { alive = false; }
                    }
                }
            }
        }
        if dirty {
            renderer.render(&conn, &state.cwd.display().to_string(), &state.entries,
                            state.selected, state.scroll_top, &state.preview, visible_rows)?;
        }
    }
    eprintln!("exiting");
    let _ = unsafe { libc::close(kq) };
    Ok(())
}
