//! Copying and pasting cells within a workbook: values, formulas and styles.
//!
//! A copy keeps formulas as expressions, so pasting moves their relative
//! references with them (`=A1+1` copied from B1 to B5 reads `=A5+1`). A cut
//! keeps the formula text as it is, as Excel does, and pasting it moves the
//! cells: the source is emptied in the same step.

use std::sync::Arc;

use crate::addr::{CellAddr, Rect, MAX_COLS, MAX_ROWS};
use crate::book::Workbook;
use crate::formula::ast::Expr;
use crate::ops::{Fill, GridOp, Region, StyleRegion};
use crate::style::Style;
use crate::value::Value;

/// One copied cell's contents.
#[derive(Debug, Clone)]
enum Content {
    Value(Value),
    /// Copied: moves with the cell.
    Expr(Arc<Expr>),
    /// Cut: the same text wherever it lands.
    Text(String),
}

/// Cells taken by a copy or a cut, by their place in the copied area.
#[derive(Debug, Clone)]
pub struct Clip {
    /// Where they were taken from.
    pub from: Rect,
    pub cut: bool,
    cells: Vec<(u32, u32, Content)>,
    styles: Vec<(u32, u32, u32, Style)>,
}

impl Clip {
    /// Takes `rect` (cut: to move it). Only the used part is read, so a
    /// whole column costs what it holds.
    pub fn take(wb: &Workbook, rect: Rect, cut: bool) -> Clip {
        let (er, ec) = wb.extent(rect.sheet);
        let used = Rect { r1: rect.r1.min(er.saturating_sub(1)).max(rect.r0), c1: rect.c1.min(ec.saturating_sub(1)).max(rect.c0), ..rect };
        let snap = wb.snapshot(&used);
        let mut cells = Vec::new();
        for (r, c, v) in snap.values {
            cells.push((r - rect.r0, c - rect.c0, Content::Value(v)));
        }
        for f in &snap.fills {
            for r in f.r0..=f.r1 {
                let content = match (cut, wb.group_at(rect.sheet, r, f.col)) {
                    (false, Some(g)) => Content::Expr(wb.group(g).expr.clone()),
                    _ => Content::Text(wb.formula_text(CellAddr::new(rect.sheet, r, f.col)).unwrap_or_default()),
                };
                cells.push((r - rect.r0, f.col - rect.c0, content));
            }
        }
        let styles = wb.style_snapshot(&used).spans.into_iter().map(|(c, a, b, st)| (c - rect.c0, a - rect.r0, b - rect.r0, st)).collect();
        Clip { from: used, cut, cells, styles }
    }

    pub fn rows(&self) -> u32 {
        self.from.r1 - self.from.r0 + 1
    }

    pub fn cols(&self) -> u32 {
        self.from.c1 - self.from.c0 + 1
    }

    /// The edit that pastes it with its top-left at (`row`, `col`) of
    /// `sheet` (a cut also empties where it came from), and the area pasted.
    pub fn paste(&self, wb: &Workbook, sheet: u16, row: u32, col: u32) -> Result<(Vec<GridOp>, Rect), String> {
        if row as u64 + self.rows() as u64 > MAX_ROWS as u64 || col as u64 + self.cols() as u64 > MAX_COLS as u64 {
            return Err("That doesn't fit on the sheet there.".into());
        }
        if (sheet as usize) >= wb.sheet_count() || (self.from.sheet as usize) >= wb.sheet_count() {
            return Err("The sheet it came from is gone.".into());
        }
        let to = Rect { sheet, r0: row, c0: col, r1: row + self.rows() - 1, c1: col + self.cols() - 1 };
        if wb.merges_in(&to).next().is_some() {
            return Err("Pasting over merged cells isn't done yet; unmerge them first.".into());
        }
        let mut values = Vec::new();
        let mut exprs = Vec::new();
        let mut fills = Vec::new();
        for (dr, dc, content) in &self.cells {
            let (r, c) = (row + dr, col + dc);
            match content {
                Content::Value(v) => values.push((r, c, v.clone())),
                Content::Expr(e) => exprs.push((r, c, e.clone())),
                Content::Text(t) => fills.push(Fill { col: c, r0: r, r1: r, src: t.clone() }),
            }
        }
        // Copied formulas sharing a relative form stay one fill a column.
        crate::shift::fills_of(wb, exprs, &mut fills);
        values.sort_by_key(|v| (v.1, v.0));
        let spans = self.styles.iter().map(|(c, a, b, st)| (col + c, row + a, row + b, st.clone())).collect();
        let mut ops = Vec::new();
        if self.cut {
            ops.push(GridOp::clear(self.from));
            ops.push(GridOp::Restyle(StyleRegion { rect: self.from, spans: Vec::new() }));
        }
        ops.push(GridOp::Replace(Region { rect: to, values, fills }));
        ops.push(GridOp::Restyle(StyleRegion { rect: to, spans }));
        Ok((ops, to))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::book::Input;
    use officina_core::history::Op;

    fn wb() -> Workbook {
        let mut wb = Workbook::new();
        wb.add_sheet("Sheet1").unwrap();
        wb.add_sheet("Sheet2").unwrap();
        wb
    }

    fn set(wb: &mut Workbook, r: u32, c: u32, s: &str) {
        GridOp::set(CellAddr::new(0, r, c), Input::typed(s)).apply(wb);
    }

    fn shown(wb: &Workbook, s: u16, r: u32, c: u32) -> String {
        wb.formula_text(CellAddr::new(s, r, c)).unwrap_or_else(|| wb.value(s, r, c).display())
    }

    fn rect(r0: u32, c0: u32, r1: u32, c1: u32) -> Rect {
        Rect { sheet: 0, r0, c0, r1, c1 }
    }

    fn run(wb: &mut Workbook, ops: Vec<GridOp>) -> GridOp {
        GridOp::Batch(ops).apply(wb)
    }

    #[test]
    fn a_copy_moves_relative_references_and_keeps_styles() {
        let mut wb = wb();
        set(&mut wb, 0, 0, "2");
        set(&mut wb, 0, 1, "=A1*10");
        set(&mut wb, 1, 1, "=$A$1+B1");
        GridOp::restyle(&wb, rect(0, 1, 0, 1), |st| Style { num_fmt: Arc::from("0.00"), ..st.clone() }).apply(&mut wb);
        let clip = Clip::take(&wb, rect(0, 0, 1, 1), false);
        let (ops, to) = clip.paste(&wb, 0, 5, 2).unwrap();
        assert_eq!(to, rect(5, 2, 6, 3));
        let undo = run(&mut wb, ops);
        assert_eq!(shown(&wb, 0, 5, 2), "2");
        assert_eq!(shown(&wb, 0, 5, 3), "=C6*10");
        assert_eq!(shown(&wb, 0, 6, 3), "=$A$1+D6");
        assert_eq!(&*wb.style(CellAddr::new(0, 5, 3)).num_fmt, "0.00");
        // The source stays.
        assert_eq!(shown(&wb, 0, 0, 1), "=A1*10");
        undo.apply(&mut wb);
        assert_eq!(shown(&wb, 0, 5, 3), "");
        // Onto another sheet: the references are its own cells.
        let (ops, _) = clip.paste(&wb, 1, 0, 0).unwrap();
        run(&mut wb, ops);
        assert_eq!(shown(&wb, 1, 0, 1), "=A1*10");
    }

    #[test]
    fn a_cut_moves_the_cells_and_keeps_formula_text() {
        let mut wb = wb();
        set(&mut wb, 0, 0, "2");
        set(&mut wb, 0, 1, "=A1*10");
        GridOp::restyle(&wb, rect(0, 1, 0, 1), |st| Style { num_fmt: Arc::from("0.00"), ..st.clone() }).apply(&mut wb);
        let clip = Clip::take(&wb, rect(0, 1, 0, 1), true);
        let (ops, _) = clip.paste(&wb, 0, 3, 3).unwrap();
        let undo = run(&mut wb, ops);
        assert_eq!(shown(&wb, 0, 3, 3), "=A1*10");
        assert_eq!(shown(&wb, 0, 0, 1), "");
        assert_eq!(&*wb.style(CellAddr::new(0, 0, 1)).num_fmt, "General");
        assert_eq!(&*wb.style(CellAddr::new(0, 3, 3)).num_fmt, "0.00");
        undo.apply(&mut wb);
        assert_eq!(shown(&wb, 0, 0, 1), "=A1*10");
        assert_eq!(shown(&wb, 0, 3, 3), "");
        // Moved onto itself shifted by one: what overlaps survives.
        let clip = Clip::take(&wb, rect(0, 0, 0, 1), true);
        let (ops, _) = clip.paste(&wb, 0, 0, 1).unwrap();
        run(&mut wb, ops);
        assert_eq!((shown(&wb, 0, 0, 0), shown(&wb, 0, 0, 1), shown(&wb, 0, 0, 2)), ("".into(), "2".into(), "=A1*10".into()));
    }

    #[test]
    fn a_whole_column_copies_only_what_it_holds() {
        let mut wb = wb();
        set(&mut wb, 2, 0, "x");
        let clip = Clip::take(&wb, rect(0, 0, MAX_ROWS - 1, 0), false);
        assert_eq!(clip.rows(), 3);
        assert!(clip.paste(&wb, 0, MAX_ROWS - 2, 0).is_err());
    }
}
