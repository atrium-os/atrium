//! The Sheets system clipboard payload contains only values and static cell
//! styles. Formulas, links, comments and move semantics stay in the source
//! document's local copy.

use officina_core::wire::{Reader, WireError, Writer};

use super::{Clip, Content};
use crate::addr::{Rect, MAX_COLS, MAX_ROWS};
use crate::ops::{read_value, write_value};
use crate::style::Style;
use crate::value::Value;

/// The largest selection Sheets exports as one clipboard item.
pub const MAX_SYSTEM_COPY_CELLS: u64 = 2_000_000;
const VERSION: u8 = 1;

impl Clip {
    /// A rectangular clipboard range from an external table. Missing cells
    /// on shorter rows are blank. Text values are never parsed as formulas.
    pub fn from_table(table: Vec<Vec<(Value, Style)>>) -> Option<Self> {
        let rows = table.len();
        let cols = table.iter().map(Vec::len).max().unwrap_or(0);
        if rows == 0 || cols == 0 || rows > MAX_ROWS as usize || cols > MAX_COLS as usize
            || rows as u64 * cols as u64 > MAX_SYSTEM_COPY_CELLS
        {
            return None;
        }
        let mut cells = Vec::new();
        let mut styles = Vec::new();
        let default = Style::default();
        for (row, values) in table.into_iter().enumerate() {
            for (col, (value, style)) in values.into_iter().enumerate() {
                let (row, col) = (row as u32, col as u32);
                if !value.is_empty() { cells.push((row, col, Content::Value(value))); }
                if style != default { styles.push((col, row, row, style)); }
            }
        }
        styles.sort_unstable_by_key(|&(col, row, _, _)| (col, row));
        let from = Rect { sheet: 0, r0: 0, c0: 0, r1: rows as u32 - 1, c1: cols as u32 - 1 };
        Some(Clip { from, cut: false, cells, styles, notes: Vec::new(), links: Vec::new() })
    }

    /// Encode a copy as values and styles, with a per-copy ID that can match
    /// the source document's local copy. Return none above the byte limit.
    pub fn encode_system(&self, id: &[u8; 16], max_bytes: usize) -> Option<Vec<u8>> {
        if self.from.cells() > MAX_SYSTEM_COPY_CELLS {
            return None;
        }
        let mut w = Writer::new();
        w.u8(VERSION);
        w.bytes(id);
        w.var(self.rows() as u64);
        w.var(self.cols() as u64);
        w.var(self.cells.len() as u64);
        for (row, col, content) in &self.cells {
            let value = match content {
                Content::Value(value) | Content::Expr(_, value) | Content::Moved(_, value) => value,
            };
            w.var(*row as u64);
            w.var(*col as u64);
            write_value(&mut w, value);
            if w.buf.len() > max_bytes { return None; }
        }
        w.var(self.styles.len() as u64);
        for (col, first, last, style) in &self.styles {
            w.var(*col as u64);
            w.var(*first as u64);
            w.var(*last as u64);
            style.encode(&mut w);
            if w.buf.len() > max_bytes { return None; }
        }
        (w.buf.len() <= max_bytes).then(|| w.into_bytes())
    }

    /// Decode a clipboard range without interpreting any strings as formulas.
    pub fn decode_system(bytes: &[u8], max_bytes: usize) -> Result<Self, WireError> {
        if bytes.len() > max_bytes {
            return Err(WireError::Invalid("clipboard size"));
        }
        let mut r = Reader::new(bytes);
        if r.u8()? != VERSION || r.bytes("clipboard copy ID", 16)?.len() != 16 {
            return Err(WireError::Invalid("clipboard header"));
        }
        let rows = r.len("clipboard rows", MAX_ROWS as u64)? as u32;
        let cols = r.len("clipboard columns", MAX_COLS as u64)? as u32;
        let area = rows as u64 * cols as u64;
        if rows == 0 || cols == 0 || area > MAX_SYSTEM_COPY_CELLS {
            return Err(WireError::Invalid("clipboard area"));
        }
        let n = r.count("clipboard values", area, 3)?;
        let mut cells = Vec::with_capacity(n);
        for _ in 0..n {
            let row = r.len("clipboard row", (rows - 1) as u64)? as u32;
            let col = r.len("clipboard column", (cols - 1) as u64)? as u32;
            cells.push((row, col, Content::Value(read_value(&mut r)?)));
        }
        cells.sort_unstable_by_key(|&(row, col, _)| (col, row));
        if cells.windows(2).any(|w| (w[0].0, w[0].1) == (w[1].0, w[1].1)) {
            return Err(WireError::Invalid("duplicate clipboard cell"));
        }
        let n = r.count("clipboard style runs", area, 16)?;
        let mut styles = Vec::with_capacity(n);
        for _ in 0..n {
            let col = r.len("clipboard style column", (cols - 1) as u64)? as u32;
            let first = r.len("clipboard style row", (rows - 1) as u64)? as u32;
            let last = r.len("clipboard style end", (rows - 1) as u64)? as u32;
            if first > last || styles.last().is_some_and(|&(c, _, end, _)| c > col || (c == col && end >= first)) {
                return Err(WireError::Invalid("overlapping or unordered clipboard styles"));
            }
            styles.push((col, first, last, Style::decode(&mut r)?));
        }
        if !r.is_empty() {
            return Err(WireError::Invalid("clipboard trailing data"));
        }
        // The values and formats are position-independent and never cut.
        let from = Rect { sheet: 0, r0: 0, c0: 0, r1: rows - 1, c1: cols - 1 };
        Ok(Clip { from, cut: false, cells, styles, notes: Vec::new(), links: Vec::new() })
    }
}
