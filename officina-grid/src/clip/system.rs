//! The Sheets system clipboard payload contains values, static cell styles
//! and comments. Formulas, links and move semantics stay in the local copy.

use officina_core::wire::{Reader, WireError, Writer};

use super::{Clip, Content};
use crate::addr::{Rect, MAX_COLS, MAX_ROWS};
use crate::ops::{read_value, write_value};
use crate::style::Style;
use crate::value::Value;

/// The largest selection Sheets exports as one clipboard item.
pub const MAX_SYSTEM_COPY_CELLS: u64 = 2_000_000;
const VALUES_AND_STYLES: u8 = 1;
const WITH_COMMENTS: u8 = 2;

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
        Some(Clip { from, cut: false, cells, styles, notes: Vec::new(), links: Vec::new(), widths: Vec::new() })
    }

    /// Encode a copy as values, styles and comments, with a per-copy ID that can match
    /// the source document's local copy. Return none above the byte limit.
    pub fn encode_system(&self, id: &[u8; 16], max_bytes: usize) -> Option<Vec<u8>> {
        if self.from.cells() > MAX_SYSTEM_COPY_CELLS {
            return None;
        }
        let mut w = Writer::new();
        w.u8(if self.notes.is_empty() { VALUES_AND_STYLES } else { WITH_COMMENTS });
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
        if !self.notes.is_empty() {
            w.var(self.notes.len() as u64);
            for (row, col, comment) in &self.notes {
                if comment.estimated_bytes() > max_bytes.saturating_sub(w.buf.len()) as u64 {
                    return None;
                }
                crate::comments::encode_entry(&mut w, *row, *col, Some(comment));
            }
        }
        (w.buf.len() <= max_bytes).then(|| w.into_bytes())
    }

    /// Decode a clipboard range without interpreting any strings as formulas.
    pub fn decode_system(bytes: &[u8], max_bytes: usize) -> Result<Self, WireError> {
        if bytes.len() > max_bytes {
            return Err(WireError::Invalid("clipboard size"));
        }
        let mut r = Reader::new(bytes);
        let version = r.u8()?;
        if !matches!(version, VALUES_AND_STYLES | WITH_COMMENTS) || r.bytes("clipboard copy ID", 16)?.len() != 16 {
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
        let mut notes = Vec::new();
        if version == WITH_COMMENTS {
            for (row, col, comment) in crate::comments::decode_in(&mut r, rows, cols, area.min(crate::comments::MAX_COMMENTS as u64))? {
                let comment = comment.ok_or(WireError::Invalid("missing clipboard comment"))?;
                if notes.last().is_some_and(|&(r, c, _)| (r, c) >= (row, col)) {
                    return Err(WireError::Invalid("duplicate or unordered clipboard comments"));
                }
                notes.push((row, col, comment));
            }
        }
        if !r.is_empty() {
            return Err(WireError::Invalid("clipboard trailing data"));
        }
        // Clipboard contents are position-independent and never cut.
        let from = Rect { sheet: 0, r0: 0, c0: 0, r1: rows - 1, c1: cols - 1 };
        Ok(Clip { from, cut: false, cells, styles, notes, links: Vec::new(), widths: Vec::new() })
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::comments::Comment;

    #[test]
    fn clipboard_rejects_comments_outside_the_range_or_repeated_at_one_cell() {
        for notes in [
            vec![(1, 0, Comment::new("A", "out of range"))],
            vec![(0, 0, Comment::new("B", "one")), (0, 0, Comment::new("C", "two"))],
        ] {
            let mut clip = Clip::from_table(vec![vec![(Value::Empty, Style::default()); 2]]).unwrap();
            clip.notes = notes;
            let bytes = clip.encode_system(&[0; 16], 4096).unwrap();
            assert!(Clip::decode_system(&bytes, 4096).is_err());
        }
    }
}
