//! Cell comments (Excel's notes, ODF's `office:annotation`): a few lines of
//! plain text and who wrote them, on a cell. They stay when the cell is
//! cleared or edited, as in Excel, and move with inserted and deleted rows
//! and columns ([`crate::shift`]). [`crate::ops::GridOp::Comments`] changes
//! them, one undo step.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::addr::{MAX_COLS, MAX_ROWS};
use crate::book::Workbook;
use officina_core::wire::{Reader, WireError, Writer};

/// The longest comment text kept, in bytes (Excel's limit is 32,767 characters).
pub const MAX_TEXT: usize = 32 << 10;
/// The longest author name kept, in bytes.
pub const MAX_AUTHOR: usize = 255;
/// The most comments a sheet keeps.
pub const MAX_COMMENTS: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comment {
    /// Who wrote it; may be empty.
    pub author: Arc<str>,
    /// Its text; lines split by `\n`.
    pub text: Arc<str>,
}

impl Comment {
    pub fn new(author: &str, text: &str) -> Comment {
        Comment { author: cut(author, MAX_AUTHOR).into(), text: cut(text, MAX_TEXT).into() }
    }
}

/// `s` cut to at most `max` bytes, on a character boundary.
fn cut(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut n = max;
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

/// A sheet's comments by (row, column).
pub type Comments = BTreeMap<(u32, u32), Comment>;

/// Writes changes: count, then each cell and its comment or none.
pub fn encode(w: &mut Writer, set: &[(u32, u32, Option<Comment>)]) {
    w.var(set.len() as u64);
    for (r, c, k) in set {
        w.var(*r as u64);
        w.var(*c as u64);
        match k {
            Some(k) => {
                w.u8(1);
                w.str(&k.author);
                w.str(&k.text);
            }
            None => w.u8(0),
        }
    }
}

/// Reads what [`encode`] wrote.
pub fn decode(r: &mut Reader) -> Result<Vec<(u32, u32, Option<Comment>)>, WireError> {
    let n = r.count("comments", MAX_COMMENTS as u64 * 2, 3)?;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        let (row, col) = (r.var()?, r.var()?);
        if row >= MAX_ROWS as u64 || col >= MAX_COLS as u64 {
            return Err(WireError::Invalid("comment cell"));
        }
        let k = match r.u8()? {
            0 => None,
            1 => {
                let author = r.str("comment author", MAX_AUTHOR as u64)?;
                let text = r.str("comment", MAX_TEXT as u64)?;
                Some(Comment { author: author.into(), text: text.into() })
            }
            _ => return Err(WireError::Invalid("comment")),
        };
        v.push((row as u32, col as u32, k));
    }
    Ok(v)
}

impl Workbook {
    /// The sheet's comments, by (row, column).
    pub fn comments(&self, sheet: u16) -> &Comments {
        static NONE: Comments = BTreeMap::new();
        self.sheets.get(sheet as usize).map_or(&NONE, |s| &s.comments)
    }

    /// The comment on a cell.
    pub fn comment(&self, sheet: u16, row: u32, col: u32) -> Option<&Comment> {
        self.comments(sheet).get(&(row, col))
    }

    /// Sets (or, `None`, takes away) comments; what was there before, in the
    /// same order, so applying it undoes this. Past [`MAX_COMMENTS`] new ones
    /// are left out.
    pub fn set_comments(&mut self, sheet: u16, set: Vec<(u32, u32, Option<Comment>)>) -> Vec<(u32, u32, Option<Comment>)> {
        let Some(sh) = self.sheets.get_mut(sheet as usize) else { return Vec::new() };
        let mut old = Vec::with_capacity(set.len());
        for (r, c, k) in set {
            let before = match k {
                Some(k) if sh.comments.len() < MAX_COMMENTS || sh.comments.contains_key(&(r, c)) => sh.comments.insert((r, c), k),
                Some(_) => continue,
                None => sh.comments.remove(&(r, c)),
            };
            old.push((r, c, before));
        }
        old.reverse();
        old
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr::CellAddr;
    use crate::native;
    use crate::ops::GridOp;
    use crate::shift::{shift_op, Axis};
    use crate::Value;
    use officina_core::history::Op;

    #[test]
    fn comments_undo_round_trip_survive_clearing_and_follow_rows() {
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        wb.set_value(CellAddr::new(0, 2, 1), Value::Num(5.0));
        let op = GridOp::Comments { sheet: 0, set: vec![(2, 1, Some(Comment::new("Ana", "Check this\nagainst March"))), (0, 0, Some(Comment::new("", "Title")))] };
        assert_eq!(GridOp::decode(&op.encode()).unwrap(), op);
        let inv = op.apply(&mut wb);
        assert_eq!(wb.comment(0, 2, 1).map(|k| &*k.author), Some("Ana"));
        // Kept in the native file.
        let back = native::load(&native::save_to_vec(&wb)).unwrap();
        assert_eq!(back.comments(0), wb.comments(0));
        // Clearing the cell keeps its comment.
        let clear = GridOp::clear(crate::addr::Rect { sheet: 0, r0: 2, c0: 1, r1: 2, c1: 1 }).apply(&mut wb);
        assert!(wb.comment(0, 2, 1).is_some());
        clear.apply(&mut wb);
        // A row above moves it down; deleting its row takes it away; undo brings both back.
        let ins = shift_op(&wb, 0, Axis::Rows, 1, 1).unwrap().apply(&mut wb);
        assert!(wb.comment(0, 3, 1).is_some() && wb.comment(0, 2, 1).is_none());
        assert!(wb.comment(0, 0, 0).is_some());
        ins.apply(&mut wb);
        let del = shift_op(&wb, 0, Axis::Rows, 2, -1).unwrap().apply(&mut wb);
        assert!(wb.comment(0, 2, 1).is_none() && wb.comments(0).len() == 1);
        del.apply(&mut wb);
        assert_eq!(wb.comment(0, 2, 1).map(|k| &*k.text), Some("Check this\nagainst March"));
        inv.apply(&mut wb);
        assert!(wb.comments(0).is_empty());
    }

    #[test]
    fn long_text_is_cut_on_a_character() {
        let k = Comment::new("x", &"é".repeat(MAX_TEXT));
        assert!(k.text.len() <= MAX_TEXT && k.text.len() % 2 == 0);
    }
}
