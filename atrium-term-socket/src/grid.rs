//! Terminal cell grid + minimal vte::Perform implementation.
//!
//! Phase 1: just `print` (printable chars) + LF/CR + BS. ANSI colors,
//! cursor save/restore, scroll regions, and the rest of CSI/OSC land
//! in later turns. Defaults: 80×24, white-on-black, cursor at (0,0).

#[derive(Clone, Copy)]
pub struct Cell {
    pub ch: char,
}

impl Default for Cell {
    fn default() -> Self { Self { ch: ' ' } }
}

pub struct Grid {
    pub cols: u16,
    pub rows: u16,
    cells: Vec<Cell>,
    pub cursor_col: u16,
    pub cursor_row: u16,
    pub dirty: bool,
}

impl Grid {
    pub fn new(cols: u16, rows: u16) -> Self {
        Self {
            cols, rows,
            cells: vec![Cell::default(); cols as usize * rows as usize],
            cursor_col: 0,
            cursor_row: 0,
            dirty: true,
        }
    }

    pub fn cells(&self) -> &[Cell] { &self.cells }

    /// Reshape the grid to (cols, rows). Existing content is
    /// preserved where it falls within the new dims; everything
    /// outside is dropped (truncate-style — no reflow). The cursor
    /// is clamped into the new bounds.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        if cols == self.cols && rows == self.rows { return; }
        let mut new_cells = vec![Cell::default(); cols as usize * rows as usize];
        let copy_cols = cols.min(self.cols) as usize;
        let copy_rows = rows.min(self.rows) as usize;
        for r in 0..copy_rows {
            let src = r * self.cols as usize;
            let dst = r * cols as usize;
            new_cells[dst..dst + copy_cols]
                .copy_from_slice(&self.cells[src..src + copy_cols]);
        }
        self.cells = new_cells;
        self.cols = cols;
        self.rows = rows;
        self.cursor_col = self.cursor_col.min(cols.saturating_sub(1));
        self.cursor_row = self.cursor_row.min(rows.saturating_sub(1));
        self.dirty = true;
    }

    fn idx(&self, col: u16, row: u16) -> usize {
        row as usize * self.cols as usize + col as usize
    }

    fn write_char(&mut self, c: char) {
        if self.cursor_col >= self.cols {
            self.newline();
        }
        let i = self.idx(self.cursor_col, self.cursor_row);
        self.cells[i] = Cell { ch: c };
        self.cursor_col += 1;
        self.dirty = true;
    }

    fn newline(&mut self) {
        self.cursor_col = 0;
        self.cursor_row += 1;
        if self.cursor_row >= self.rows {
            // scroll up by one
            let cw = self.cols as usize;
            self.cells.copy_within(cw.., 0);
            for c in &mut self.cells[(self.rows as usize - 1) * cw..] {
                *c = Cell::default();
            }
            self.cursor_row = self.rows - 1;
        }
        self.dirty = true;
    }

    fn carriage_return(&mut self) {
        self.cursor_col = 0;
        self.dirty = true;
    }

    fn backspace(&mut self) {
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
            self.dirty = true;
        }
    }
}

impl vte::Perform for Grid {
    fn print(&mut self, c: char) {
        if c.is_control() { return; }
        self.write_char(c);
    }
    fn execute(&mut self, byte: u8) {
        match byte {
            b'\n' => self.newline(),
            b'\r' => self.carriage_return(),
            0x08  => self.backspace(),   // BS
            0x09  => {                    // TAB → next 8-col stop
                let next = ((self.cursor_col / 8) + 1) * 8;
                while self.cursor_col < next.min(self.cols) {
                    self.write_char(' ');
                }
            }
            _ => {}
        }
    }
    // Phase-1 stubs — silently swallow CSI/OSC/etc.; later turns
    // wire ANSI color, cursor positioning, etc. into the grid.
    fn hook(&mut self, _: &vte::Params, _: &[u8], _: bool, _: char) {}
    fn put(&mut self, _: u8) {}
    fn unhook(&mut self) {}
    fn osc_dispatch(&mut self, _: &[&[u8]], _: bool) {}
    fn csi_dispatch(&mut self, _: &vte::Params, _: &[u8], _: bool, _: char) {}
    fn esc_dispatch(&mut self, _: &[u8], _: bool, _: u8) {}
}
