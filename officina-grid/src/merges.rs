//! Merged cells: rectangles of a sheet shown as one cell, its top-left's
//! value and style across the whole area (Excel's `mergeCell`, ODF's
//! `table:number-columns-spanned`). The other cells keep whatever they hold,
//! as Excel and LibreOffice keep it, but are neither shown nor edited.
//!
//! A sheet's merges are disjoint, at least two cells each, sorted by top row
//! then left column. They move with inserted and deleted rows and columns
//! ([`crate::shift`]) and change through [`crate::ops::GridOp::Merges`], one
//! undo step.

use crate::addr::{CellAddr, Rect, MAX_COLS, MAX_ROWS};
use crate::book::Workbook;
use officina_core::wire::{Reader, WireError, Writer};

/// Writes a sheet's merges: count, then each area's corners.
pub fn encode(w: &mut Writer, list: &[Rect]) {
    w.var(list.len() as u64);
    for r in list {
        w.var(r.r0 as u64);
        w.var(r.c0 as u64);
        w.var(r.r1 as u64);
        w.var(r.c1 as u64);
    }
}

/// Reads what [`encode`] wrote, for `sheet`; areas outside the grid or of one
/// cell are refused.
pub fn decode(r: &mut Reader, sheet: u16) -> Result<Vec<Rect>, WireError> {
    let n = r.count("merged cells", MAX_MERGES as u64, 4)?;
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        let (r0, c0, r1, c1) = (r.var()?, r.var()?, r.var()?, r.var()?);
        if r0 > r1 || c0 > c1 || r1 >= MAX_ROWS as u64 || c1 >= MAX_COLS as u64 || (r0 == r1 && c0 == c1) {
            return Err(WireError::Invalid("merged cells"));
        }
        v.push(Rect { sheet, r0: r0 as u32, c0: c0 as u32, r1: r1 as u32, c1: c1 as u32 });
    }
    Ok(v)
}

/// The most merged areas a sheet keeps (Excel allows about this many too).
pub const MAX_MERGES: usize = 1 << 20;

/// `list` made a valid set of merges on `sheet`: inside the grid, more than
/// one cell, the earlier of two that overlap kept, sorted.
pub fn normalize(sheet: u16, list: impl IntoIterator<Item = Rect>) -> Vec<Rect> {
    let mut out: Vec<Rect> = Vec::new();
    for r in list {
        let r = Rect { sheet, r0: r.r0.min(r.r1), c0: r.c0.min(r.c1), r1: r.r0.max(r.r1), c1: r.c0.max(r.c1) };
        if r.r1 >= MAX_ROWS || r.c1 >= MAX_COLS || r.cells() < 2 || out.len() >= MAX_MERGES {
            continue;
        }
        out.push(r);
    }
    // Keep the first of overlapping ones (in the order given), sorted as they go.
    let mut by_row: Vec<Rect> = Vec::new();
    for r in out {
        // `by_row` is sorted by r0; any overlap starts at or above r.r1.
        let end = by_row.partition_point(|k| k.r0 <= r.r1);
        if by_row[..end].iter().any(|k| k.intersects(&r)) {
            continue;
        }
        let at = by_row.partition_point(|k| (k.r0, k.c0) < (r.r0, r.c0));
        by_row.insert(at, r);
    }
    by_row
}

impl Workbook {
    /// The sheet's merged areas, sorted by top row then left column.
    pub fn merges(&self, sheet: u16) -> &[Rect] {
        self.sheets.get(sheet as usize).map_or(&[], |s| &s.merges[..])
    }

    /// The merged area holding the cell, if any.
    pub fn merge_at(&self, a: CellAddr) -> Option<Rect> {
        let m = self.merges(a.sheet);
        let end = m.partition_point(|r| r.r0 <= a.row);
        m[..end].iter().rev().find(|r| r.contains(a)).copied()
    }

    /// The merged areas that meet `rect`.
    pub fn merges_in(&self, rect: &Rect) -> impl Iterator<Item = Rect> + '_ {
        let rect = *rect;
        let m = self.merges(rect.sheet);
        let end = m.partition_point(|r| r.r0 <= rect.r1);
        m[..end].iter().filter(move |r| r.intersects(&rect)).copied()
    }

    /// `rect` grown until no merged area crosses its edge (a selection that
    /// touches part of a merge takes all of it, as Excel's does).
    pub fn grow_to_merges(&self, mut rect: Rect) -> Rect {
        loop {
            let mut grown = rect;
            for m in self.merges_in(&rect) {
                grown.r0 = grown.r0.min(m.r0);
                grown.c0 = grown.c0.min(m.c0);
                grown.r1 = grown.r1.max(m.r1);
                grown.c1 = grown.c1.max(m.c1);
            }
            if grown == rect {
                return rect;
            }
            rect = grown;
        }
    }

    /// Replaces the sheet's merges with `list` (normalized); the old ones.
    pub fn set_merges(&mut self, sheet: u16, list: Vec<Rect>) -> Vec<Rect> {
        let list = normalize(sheet, list);
        match self.sheets.get_mut(sheet as usize) {
            Some(s) => std::mem::replace(&mut s.merges, list),
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(r0: u32, c0: u32, r1: u32, c1: u32) -> Rect {
        Rect { sheet: 0, r0, c0, r1, c1 }
    }

    #[test]
    fn overlapping_and_single_cells_are_dropped_and_the_rest_sorted() {
        let n = normalize(0, [r(5, 0, 6, 1), r(0, 0, 0, 0), r(1, 1, 3, 3), r(2, 2, 4, 4), r(0, 3, 0, 1)]);
        assert_eq!(n, vec![r(0, 1, 0, 3), r(1, 1, 3, 3), r(5, 0, 6, 1)]);
    }

    #[test]
    fn cells_find_their_area_and_selections_grow_to_whole_ones() {
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        wb.set_merges(0, vec![r(1, 1, 2, 3), r(4, 0, 4, 2), r(10, 5, 20, 5)]);
        assert_eq!(wb.merge_at(CellAddr::new(0, 2, 2)), Some(r(1, 1, 2, 3)));
        assert_eq!(wb.merge_at(CellAddr::new(0, 15, 5)), Some(r(10, 5, 20, 5)));
        assert_eq!(wb.merge_at(CellAddr::new(0, 3, 2)), None);
        // B3:B5 touches the first two; growing it takes both whole.
        assert_eq!(wb.grow_to_merges(r(2, 1, 4, 1)), r(1, 0, 4, 3));
        assert_eq!(wb.merges_in(&r(0, 0, 9, 9)).count(), 2);
    }
}

#[cfg(test)]
mod op_tests {
    use crate::addr::{CellAddr, Rect};
    use crate::book::Workbook;
    use crate::native;
    use crate::ops::GridOp;
    use crate::shift::{shift_op, Axis};
    use officina_core::history::Op;

    fn r(r0: u32, c0: u32, r1: u32, c1: u32) -> Rect {
        Rect { sheet: 0, r0, c0, r1, c1 }
    }

    #[test]
    fn merges_undo_round_trip_and_follow_rows_and_columns() {
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        let op = GridOp::Merges { sheet: 0, list: vec![r(1, 1, 2, 3), r(5, 0, 5, 1)] };
        assert_eq!(GridOp::decode(&op.encode()).unwrap(), op);
        assert!(GridOp::decode(&GridOp::Merges { sheet: 0, list: vec![r(1, 1, 1, 1)] }.encode()).is_err());
        let inv = op.apply(&mut wb);
        assert_eq!(wb.merges(0).len(), 2);
        // Kept in the native file.
        let back = native::load(&native::save_to_vec(&wb)).unwrap();
        assert_eq!(back.merges(0), wb.merges(0));
        // A row inserted inside the first grows it; the second moves down.
        let ins = shift_op(&wb, 0, Axis::Rows, 2, 1).unwrap().apply(&mut wb);
        assert_eq!(wb.merges(0), &[r(1, 1, 3, 3), r(6, 0, 6, 1)]);
        ins.apply(&mut wb);
        // Deleting column A leaves the second one cell: no longer merged.
        let del = shift_op(&wb, 0, Axis::Cols, 0, -1).unwrap().apply(&mut wb);
        assert_eq!(wb.merges(0), &[r(1, 0, 2, 2)]);
        del.apply(&mut wb);
        assert_eq!(wb.merges(0), &[r(1, 1, 2, 3), r(5, 0, 5, 1)]);
        inv.apply(&mut wb);
        assert!(wb.merges(0).is_empty());
        assert_eq!(wb.merge_at(CellAddr::new(0, 1, 1)), None);
    }
}
