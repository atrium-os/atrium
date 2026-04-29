//! Editable text buffer.
//!
//! Phase 1 scope: lines as `Vec<String>`, cursor in (line, col) byte
//! coordinates, append/backspace/newline. Cursor movement, undo, and
//! O(1) insert in long lines come later (gap buffer or rope when
//! patterns demand it).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub struct Buffer {
    pub lines: Vec<String>,
    pub cursor_line: usize,
    pub cursor_col:  usize,
    pub scroll_top:  usize,
    pub modified:    bool,
    pub path:        Option<PathBuf>,
    pub status:      String,
}

impl Buffer {
    pub fn empty() -> Self {
        Self {
            lines: vec![String::new()],
            cursor_line: 0,
            cursor_col: 0,
            scroll_top: 0,
            modified: false,
            path: None,
            status: String::from("[new buffer]"),
        }
    }

    pub fn open<P: AsRef<Path>>(p: P) -> io::Result<Self> {
        let p = p.as_ref();
        let text = match fs::read_to_string(p) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        let mut lines: Vec<String> = text.split('\n').map(String::from).collect();
        if lines.is_empty() { lines.push(String::new()); }
        // last "line" after a trailing \n is an empty string we want to keep.
        let n = lines.len();
        Ok(Self {
            lines,
            cursor_line: n - 1,
            cursor_col:  0,
            scroll_top:  0,
            modified:    false,
            path:        Some(p.to_path_buf()),
            status:      format!("[loaded {}]", p.display()),
        })
    }

    pub fn save(&mut self) -> io::Result<()> {
        let path = match &self.path {
            Some(p) => p.clone(),
            None => return Err(io::Error::new(io::ErrorKind::Other, "no path")),
        };
        let body = self.lines.join("\n");
        fs::write(&path, body.as_bytes())?;
        self.modified = false;
        self.status = format!("[saved {}]", path.display());
        Ok(())
    }

    pub fn insert_char(&mut self, c: char) {
        let line = &mut self.lines[self.cursor_line];
        if self.cursor_col > line.len() { self.cursor_col = line.len(); }
        line.insert(self.cursor_col, c);
        self.cursor_col += c.len_utf8();
        self.modified = true;
    }

    pub fn newline(&mut self) {
        let line = &mut self.lines[self.cursor_line];
        let rest = line.split_off(self.cursor_col);
        self.lines.insert(self.cursor_line + 1, rest);
        self.cursor_line += 1;
        self.cursor_col = 0;
        self.modified = true;
    }

    pub fn backspace(&mut self) {
        if self.cursor_col > 0 {
            let line = &mut self.lines[self.cursor_line];
            // Drop the previous char, byte-correct.
            let mut new_col = self.cursor_col - 1;
            while new_col > 0 && !line.is_char_boundary(new_col) { new_col -= 1; }
            line.drain(new_col..self.cursor_col);
            self.cursor_col = new_col;
            self.modified = true;
        } else if self.cursor_line > 0 {
            // Join with previous line.
            let cur = self.lines.remove(self.cursor_line);
            self.cursor_line -= 1;
            let prev_len = self.lines[self.cursor_line].len();
            self.lines[self.cursor_line].push_str(&cur);
            self.cursor_col = prev_len;
            self.modified = true;
        }
    }

    pub fn move_left(&mut self) {
        if self.cursor_col > 0 {
            let line = &self.lines[self.cursor_line];
            let mut c = self.cursor_col - 1;
            while c > 0 && !line.is_char_boundary(c) { c -= 1; }
            self.cursor_col = c;
        } else if self.cursor_line > 0 {
            self.cursor_line -= 1;
            self.cursor_col = self.lines[self.cursor_line].len();
        }
    }
    pub fn move_right(&mut self) {
        let line = &self.lines[self.cursor_line];
        if self.cursor_col < line.len() {
            let mut c = self.cursor_col + 1;
            while c < line.len() && !line.is_char_boundary(c) { c += 1; }
            self.cursor_col = c;
        } else if self.cursor_line + 1 < self.lines.len() {
            self.cursor_line += 1;
            self.cursor_col = 0;
        }
    }
    pub fn move_up(&mut self) {
        if self.cursor_line > 0 {
            self.cursor_line -= 1;
            self.cursor_col = self.cursor_col.min(self.lines[self.cursor_line].len());
        }
    }
    pub fn move_down(&mut self) {
        if self.cursor_line + 1 < self.lines.len() {
            self.cursor_line += 1;
            self.cursor_col = self.cursor_col.min(self.lines[self.cursor_line].len());
        }
    }

    /// Adjust scroll so cursor is visible inside `visible_rows`.
    pub fn scroll_into_view(&mut self, visible_rows: usize) {
        if self.cursor_line < self.scroll_top {
            self.scroll_top = self.cursor_line;
        } else if self.cursor_line >= self.scroll_top + visible_rows.saturating_sub(1) {
            self.scroll_top = self.cursor_line + 1 - visible_rows.max(1);
        }
    }
}
