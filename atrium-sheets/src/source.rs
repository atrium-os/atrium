//! A sheet as a [`GridSource`]: what `GridView` asks for, read from the document.

use std::cell::RefCell;
use std::rc::Rc;

use officina_grid::addr::{CellAddr, MAX_COLS, MAX_ROWS};
use officina_grid::Value;
use pergola::theme::palette;
use pergola::widgets::{CellAlign, CellView, GridSource};

use crate::doc::Doc;

/// Rows the grid always offers, used or not (Excel shows all of them; so do we).
pub const MIN_ROWS: u32 = 1 << 20;
/// Columns the grid always offers.
pub const MIN_COLS: u32 = 16384;

/// The sheet on screen.
pub struct SheetSource {
    pub doc: Rc<RefCell<Doc>>,
}

impl SheetSource {
    pub fn new(doc: Rc<RefCell<Doc>>) -> Self {
        Self { doc }
    }
    fn used(&self) -> (u32, u32) {
        let d = self.doc.borrow();
        d.wb.extent(d.sheet)
    }
    fn filled(&self, r: u32, c: u32) -> bool {
        let d = self.doc.borrow();
        !d.wb.value(d.sheet, r, c).is_empty()
    }
}

/// How a value shows in a cell (General format).
pub fn view_of(v: &Value) -> Option<CellView> {
    Some(match v {
        Value::Empty => return None,
        Value::Num(_) => CellView::number(v.display()),
        Value::Str(s) => CellView::text(s.to_string()),
        Value::Bool(_) => CellView { align: CellAlign::Center, ..CellView::text(v.display()) },
        Value::Err(_) => CellView { align: CellAlign::Center, color: Some(palette::danger_500()), ..CellView::text(v.display()) },
    })
}

impl GridSource for SheetSource {
    fn rows(&self) -> u32 {
        let (r, _) = self.used();
        MIN_ROWS.max(r.saturating_add(1000)).min(MAX_ROWS)
    }
    fn cols(&self) -> u32 {
        let (_, c) = self.used();
        MIN_COLS.max(c.saturating_add(26)).min(MAX_COLS)
    }
    fn row_top(&self, r: u32) -> f64 {
        let d = self.doc.borrow();
        d.wb.sheet(d.sheet).row_sizes.offset(r)
    }
    fn row_at(&self, y: f64) -> u32 {
        let d = self.doc.borrow();
        d.wb.sheet(d.sheet).row_sizes.index_at(y.max(0.0)).min(self.rows() - 1)
    }
    fn col_left(&self, c: u32) -> f64 {
        let d = self.doc.borrow();
        d.wb.sheet(d.sheet).col_sizes.offset(c)
    }
    fn col_at(&self, x: f64) -> u32 {
        let d = self.doc.borrow();
        d.wb.sheet(d.sheet).col_sizes.index_at(x.max(0.0)).min(self.cols() - 1)
    }
    fn cell(&self, r: u32, c: u32) -> Option<CellView> {
        let d = self.doc.borrow();
        view_of(&d.wb.value(d.sheet, r, c))
    }
    fn edit_text(&self, r: u32, c: u32) -> String {
        let d = self.doc.borrow();
        let a = CellAddr::new(d.sheet, r, c);
        match d.wb.formula_text(a) {
            Some(f) => f,
            None => d.wb.get(a).display(),
        }
    }
    /// Excel's Ctrl + arrow: from a filled cell with a filled neighbour, to the end of
    /// the run; otherwise to the next filled cell, or the edge.
    fn data_edge(&self, r: u32, c: u32, dr: i32, dc: i32) -> (u32, u32) {
        let (used_r, used_c) = self.used();
        let (rows, cols) = (self.rows() as i64, self.cols() as i64);
        let step = |r: i64, c: i64| (r + dr.signum() as i64, c + dc.signum() as i64);
        let inside = |r: i64, c: i64| r >= 0 && c >= 0 && r < rows && c < cols;
        let (mut r, mut c) = (r as i64, c as i64);
        let (nr, nc) = step(r, c);
        if !inside(nr, nc) {
            return (r as u32, c as u32);
        }
        let here = self.filled(r as u32, c as u32);
        if here && self.filled(nr as u32, nc as u32) {
            // To the end of this run.
            loop {
                let (nr, nc) = step(r, c);
                if !inside(nr, nc) || !self.filled(nr as u32, nc as u32) {
                    return (r as u32, c as u32);
                }
                (r, c) = (nr, nc);
            }
        }
        // To the next filled cell; past the used area there is none.
        loop {
            let (nr, nc) = step(r, c);
            if !inside(nr, nc) {
                return (r as u32, c as u32);
            }
            (r, c) = (nr, nc);
            if (dr > 0 && r >= used_r as i64) || (dc > 0 && c >= used_c as i64) {
                let r = if dr > 0 { rows - 1 } else { r };
                let c = if dc > 0 { cols - 1 } else { c };
                return (r as u32, c as u32);
            }
            if self.filled(r as u32, c as u32) {
                return (r as u32, c as u32);
            }
        }
    }
    fn last_used(&self) -> (u32, u32) {
        let (r, c) = self.used();
        (r.saturating_sub(1), c.saturating_sub(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn src(cells: &[(u32, u32, &str)]) -> SheetSource {
        let mut d = Doc::new("t");
        for &(r, c, s) in cells {
            d.type_into(r, c, s);
        }
        SheetSource::new(Rc::new(RefCell::new(d)))
    }

    #[test]
    fn ctrl_arrows_follow_the_data() {
        let s = src(&[(0, 0, "1"), (1, 0, "2"), (2, 0, "3"), (10, 0, "x")]);
        assert_eq!(s.data_edge(0, 0, 1, 0), (2, 0), "to the end of the run");
        assert_eq!(s.data_edge(2, 0, 1, 0), (10, 0), "to the next filled cell");
        assert_eq!(s.data_edge(10, 0, 1, 0), (s.rows() - 1, 0), "past the data, the edge");
        assert_eq!(s.data_edge(10, 0, -1, 0), (2, 0));
        assert_eq!(s.data_edge(0, 0, 0, 1), (0, s.cols() - 1));
        assert_eq!(s.last_used(), (10, 0));
    }

    #[test]
    fn cells_show_their_values_and_edit_their_formulas() {
        let s = src(&[(0, 0, "2"), (0, 1, "=A1*3"), (0, 2, "=1/0"), (0, 3, "TRUE")]);
        assert_eq!(s.cell(0, 1).unwrap().text, "6");
        assert!(s.cell(0, 1).unwrap().numeric);
        assert_eq!(s.edit_text(0, 1), "=A1*3");
        assert_eq!(s.cell(0, 2).unwrap().text, "#DIV/0!");
        assert_eq!(s.cell(0, 3).unwrap().align, CellAlign::Center);
        assert!(s.cell(5, 5).is_none());
        assert_eq!(s.rows(), MIN_ROWS);
    }
}
