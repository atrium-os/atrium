//! Copying and pasting cells within a workbook: values, formulas and styles.
//!
//! A copy keeps formulas as expressions, so pasting moves their relative
//! references with them (`=A1+1` copied from B1 to B5 reads `=A5+1`). A cut
//! moves the cells when pasted, as Excel does: the source is emptied in the
//! same step, and references to the moved cells (from anywhere, the moved
//! formulas too) follow them, while references to cells left behind stay.

use std::sync::Arc;

use crate::addr::{CellAddr, Rect, MAX_COLS, MAX_ROWS};
use crate::book::Workbook;
use crate::formula::ast::{CellRef, Coord, Expr, RangeKind, RangeRef};
use crate::ops::{Fill, GridOp, Region, StyleRegion};
use crate::style::Style;
use crate::value::Value;

/// One copied cell's contents.
#[derive(Debug, Clone)]
enum Content {
    Value(Value),
    /// Copied: moves with the cell; its value when copied.
    Expr(Arc<Expr>, Value),
    /// Cut: its references follow what moves with it (and stay on what
    /// doesn't), written for the cell's place in the cut area.
    Moved(Arc<Expr>),
}

/// What a paste brings (Paste Special).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum What {
    /// Values, formulas and styles.
    All,
    /// Values, formulas as their values when copied; no styles.
    Values,
    /// Values and formulas; no styles.
    Formulas,
    /// Styles only.
    Formats,
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
                    (false, Some(g)) => Content::Expr(wb.group(g).expr.clone(), wb.value(rect.sheet, r, f.col)),
                    (true, Some(g)) => Content::Moved(wb.group(g).expr.clone()),
                    _ => continue,
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
        self.paste_as(wb, sheet, row, col, What::All, false)
    }

    /// As [`paste`](Self::paste), bringing only `what`, and `transpose`d
    /// (rows become columns). A cut pastes only whole and as it is. A
    /// transposed formula comes as its value: turning its references is
    /// not done yet.
    pub fn paste_as(&self, wb: &Workbook, sheet: u16, row: u32, col: u32, what: What, transpose: bool) -> Result<(Vec<GridOp>, Rect), String> {
        if self.cut && (what != What::All || transpose) {
            return Err("Cut cells paste only as they are; copy them for Paste Special.".into());
        }
        let (rows, cols) = if transpose { (self.cols(), self.rows()) } else { (self.rows(), self.cols()) };
        let at = |dr: u32, dc: u32| if transpose { (row + dc, col + dr) } else { (row + dr, col + dc) };
        if row as u64 + rows as u64 > MAX_ROWS as u64 || col as u64 + cols as u64 > MAX_COLS as u64 {
            return Err("That doesn't fit on the sheet there.".into());
        }
        if (sheet as usize) >= wb.sheet_count() || (self.from.sheet as usize) >= wb.sheet_count() {
            return Err("The sheet it came from is gone.".into());
        }
        let to = Rect { sheet, r0: row, c0: col, r1: row + rows - 1, c1: col + cols - 1 };
        if wb.merges_in(&to).next().is_some() {
            return Err("Pasting over merged cells isn't done yet; unmerge them first.".into());
        }
        let mut values = Vec::new();
        let mut exprs = Vec::new();
        let mut fills = Vec::new();
        for (dr, dc, content) in &self.cells {
            let (r, c) = at(*dr, *dc);
            match content {
                Content::Value(v) => values.push((r, c, v.clone())),
                Content::Expr(_, v) if what == What::Values || transpose => {
                    if !v.is_empty() {
                        values.push((r, c, v.clone()))
                    }
                }
                Content::Expr(e, _) => exprs.push((r, c, e.clone())),
                Content::Moved(e) => {
                    let m = self.mover(sheet, row, col);
                    let e = m.expr(e, self.from.r0 + dr, self.from.c0 + dc, self.from.sheet, r, c, sheet);
                    fills.push(Fill { col: c, r0: r, r1: r, src: crate::formula::to_a1(&e, r, c, &|id| wb.sheet_name(id)) });
                }
            }
        }
        // Copied formulas sharing a relative form stay one fill a column.
        crate::shift::fills_of(wb, exprs, &mut fills);
        values.sort_by_key(|v| (v.1, v.0));
        let spans = if transpose {
            // Column runs become row runs: one span a cell.
            let mut cells: Vec<(u32, u32, Style)> = Vec::new();
            for (c, a, b, st) in &self.styles {
                for r in *a..=*b {
                    let (rr, cc) = at(r, *c);
                    cells.push((cc, rr, st.clone()));
                }
            }
            cells.sort_by_key(|x| (x.0, x.1));
            let mut spans: Vec<(u32, u32, u32, Style)> = Vec::new();
            for (c, r, st) in cells {
                match spans.last_mut() {
                    Some(l) if l.0 == c && l.2 + 1 == r && l.3 == st => l.2 = r,
                    _ => spans.push((c, r, r, st)),
                }
            }
            spans
        } else {
            self.styles.iter().map(|(c, a, b, st)| (col + c, row + a, row + b, st.clone())).collect()
        };
        let mut ops = Vec::new();
        if self.cut {
            ops.push(GridOp::clear(self.from));
            ops.push(GridOp::Restyle(StyleRegion { rect: self.from, spans: Vec::new() }));
            // Formulas elsewhere that read the moved cells follow them.
            let m = self.mover(sheet, row, col);
            for (_, g) in wb.groups() {
                if !reads(&g.expr, g.sheet, self.from) {
                    continue;
                }
                let mut cells = Vec::new();
                for r in g.r0..=g.r1 {
                    let here = CellAddr::new(g.sheet, r, g.col);
                    if self.from.contains(here) || to.contains(here) {
                        continue;
                    }
                    let e = m.expr(&g.expr, r, g.col, g.sheet, r, g.col, g.sheet);
                    if e != *g.expr {
                        cells.push((r, g.col, Arc::new(e)));
                    }
                }
                let mut moved = Vec::new();
                crate::shift::fills_of(wb, cells, &mut moved);
                for f in moved {
                    ops.push(GridOp::Replace(Region { rect: Rect { sheet: g.sheet, r0: f.r0, c0: f.col, r1: f.r1, c1: f.col }, values: Vec::new(), fills: vec![f] }));
                }
            }
        }
        if what != What::Formats {
            ops.push(GridOp::Replace(Region { rect: to, values, fills }));
        }
        if matches!(what, What::All | What::Formats) {
            ops.push(GridOp::Restyle(StyleRegion { rect: to, spans }));
        }
        Ok((ops, to))
    }
}

impl Clip {
    fn mover(&self, sheet: u16, row: u32, col: u32) -> Mover {
        Mover { from: self.from, sheet, dr: row as i64 - self.from.r0 as i64, dc: col as i64 - self.from.c0 as i64 }
    }
}

/// Whether `e`, a formula on `own`, reads any cell of `area`.
fn reads(e: &Expr, own: u16, area: Rect) -> bool {
    let mut hit = false;
    e.walk(&mut |x| match x {
        Expr::Ref(r) | Expr::Spill(r) => hit |= r.sheet.unwrap_or(own) == area.sheet,
        Expr::Range(r) => hit |= r.sheet.unwrap_or(own) == area.sheet,
        _ => {}
    });
    hit
}

/// References into `from` moved by (`dr`, `dc`) onto `sheet`, as a cut and
/// paste moves them: a cell inside, or a range wholly inside.
struct Mover {
    from: Rect,
    sheet: u16,
    dr: i64,
    dc: i64,
}

impl Mover {
    /// `e`, written for (`or`, `oc`) on `own`, rewritten for (`nr`, `nc`)
    /// on `new_own`.
    #[allow(clippy::too_many_arguments)]
    fn expr(&self, e: &Expr, or: u32, oc: u32, own: u16, nr: u32, nc: u32, new_own: u16) -> Expr {
        let enc = |c: Coord, t: u32, at: u32| match c {
            Coord::Abs(_) => Coord::Abs(t),
            Coord::Rel(_) => Coord::Rel((t as i64 - at as i64) as i32),
        };
        // The sheet a reference names: kept bare when it stays the formula's own.
        let name = |old: Option<u16>, target: u16| if old.is_none() && target == new_own { None } else { Some(target) };
        let inside = |t: u16, r: u32, c: u32| t == self.from.sheet && self.from.contains(CellAddr::new(t, r, c));
        let cell = |r: &CellRef| -> CellRef {
            let t = r.sheet.unwrap_or(own);
            let (Some(row), Some(col)) = (r.row.resolve(or, MAX_ROWS), r.col.resolve(oc, MAX_COLS)) else { return *r };
            let (row, col, t) = if inside(t, row, col) { ((row as i64 + self.dr) as u32, (col as i64 + self.dc) as u32, self.sheet) } else { (row, col, t) };
            CellRef { sheet: name(r.sheet, t), row: enc(r.row, row, nr), col: enc(r.col, col, nc) }
        };
        let range = |r: &RangeRef| -> RangeRef {
            let t = r.sheet.unwrap_or(own);
            let ends = (r.r0.resolve(or, MAX_ROWS), r.c0.resolve(oc, MAX_COLS), r.r1.resolve(or, MAX_ROWS), r.c1.resolve(oc, MAX_COLS));
            let (Some(r0), Some(c0), Some(r1), Some(c1)) = ends else { return *r };
            let whole = r.kind == RangeKind::Cells && inside(t, r0.min(r1), c0.min(c1)) && inside(t, r0.max(r1), c0.max(c1));
            let (r0, c0, r1, c1, t) = if whole {
                let (mr, mc) = (|v: u32| (v as i64 + self.dr) as u32, |v: u32| (v as i64 + self.dc) as u32);
                (mr(r0), mc(c0), mr(r1), mc(c1), self.sheet)
            } else {
                (r0, c0, r1, c1, t)
            };
            RangeRef { sheet: name(r.sheet, t), r0: enc(r.r0, r0, nr), c0: enc(r.c0, c0, nc), r1: enc(r.r1, r1, nr), c1: enc(r.c1, c1, nc), kind: r.kind }
        };
        self.walk(e, &cell, &range)
    }

    fn walk(&self, e: &Expr, cell: &dyn Fn(&CellRef) -> CellRef, range: &dyn Fn(&RangeRef) -> RangeRef) -> Expr {
        let b = |x: &Expr| Box::new(self.walk(x, cell, range));
        let all = |xs: &[Expr]| xs.iter().map(|a| self.walk(a, cell, range)).collect::<Vec<_>>();
        match e {
            Expr::Ref(r) => Expr::Ref(cell(r)),
            Expr::Spill(r) => Expr::Spill(cell(r)),
            Expr::Range(r) => Expr::Range(range(r)),
            Expr::Neg(x) => Expr::Neg(b(x)),
            Expr::Plus(x) => Expr::Plus(b(x)),
            Expr::Pct(x) => Expr::Pct(b(x)),
            Expr::Paren(x) => Expr::Paren(b(x)),
            Expr::At(x) => Expr::At(b(x)),
            Expr::Bin(op, x, y) => Expr::Bin(*op, b(x), b(y)),
            Expr::Call(f, args) => Expr::Call(f.clone(), all(args)),
            Expr::Invoke(g, args) => Expr::Invoke(b(g), all(args)),
            Expr::Array(rows) => Expr::Array(rows.iter().map(|r| all(r)).collect()),
            other => other.clone(),
        }
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
    fn a_cut_moves_the_cells_and_what_they_read_stays() {
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
        assert_eq!((shown(&wb, 0, 0, 0), shown(&wb, 0, 0, 1), shown(&wb, 0, 0, 2)), ("".into(), "2".into(), "=B1*10".into()));
    }

    #[test]
    fn paste_special_brings_values_formulas_formats_or_turns_them() {
        let mut wb = wb();
        set(&mut wb, 0, 0, "2");
        set(&mut wb, 0, 1, "=A1*10");
        set(&mut wb, 1, 0, "x");
        GridOp::restyle(&wb, rect(0, 0, 0, 1), |st| Style { num_fmt: Arc::from("0.00"), ..st.clone() }).apply(&mut wb);
        wb.recalc();
        let clip = Clip::take(&wb, rect(0, 0, 1, 1), false);
        let fmt = |wb: &Workbook, r, c| wb.style(CellAddr::new(0, r, c)).num_fmt.to_string();
        // Values: the formula's result, no format.
        let (ops, _) = clip.paste_as(&wb, 0, 5, 0, What::Values, false).unwrap();
        run(&mut wb, ops);
        assert_eq!((shown(&wb, 0, 5, 1), fmt(&wb, 5, 1)), ("20".into(), "General".into()));
        // Formulas: moved, no format.
        let (ops, _) = clip.paste_as(&wb, 0, 8, 0, What::Formulas, false).unwrap();
        run(&mut wb, ops);
        assert_eq!((shown(&wb, 0, 8, 1), fmt(&wb, 8, 1)), ("=A9*10".into(), "General".into()));
        // Formats: the cells keep what they hold.
        let (ops, _) = clip.paste_as(&wb, 0, 8, 0, What::Formats, false).unwrap();
        run(&mut wb, ops);
        assert_eq!((shown(&wb, 0, 8, 1), fmt(&wb, 8, 1)), ("=A9*10".into(), "0.00".into()));
        // Transposed: 2 rows × 2 columns turned; B1 lands in A12 as its value.
        let (ops, to) = clip.paste_as(&wb, 0, 10, 0, What::All, true).unwrap();
        assert_eq!(to, rect(10, 0, 11, 1));
        run(&mut wb, ops);
        assert_eq!((shown(&wb, 0, 10, 0), shown(&wb, 0, 10, 1), shown(&wb, 0, 11, 0)), ("2".into(), "x".into(), "20".into()));
        assert_eq!((fmt(&wb, 11, 0), fmt(&wb, 10, 1)), ("0.00".into(), "General".into()));
        // A cut pastes only whole.
        assert!(Clip::take(&wb, rect(0, 0, 0, 0), true).paste_as(&wb, 0, 20, 0, What::Values, false).is_err());
    }

    #[test]
    fn references_to_moved_cells_follow_them() {
        let mut wb = wb();
        set(&mut wb, 0, 0, "1");
        set(&mut wb, 1, 0, "2");
        set(&mut wb, 9, 9, "5");
        // Elsewhere: a cell, a range wholly inside, one partly inside, absolute.
        set(&mut wb, 0, 4, "=A1+$A$2");
        set(&mut wb, 1, 4, "=SUM(A1:A2)");
        set(&mut wb, 2, 4, "=SUM(A1:A3)");
        GridOp::set(CellAddr::new(1, 0, 0), Input::typed("=Sheet1!A2*2")).apply(&mut wb);
        // A moved formula reading something left behind keeps reading it.
        set(&mut wb, 2, 0, "=J10");
        let clip = Clip::take(&wb, rect(0, 0, 2, 0), true);
        let (ops, _) = clip.paste(&wb, 0, 5, 2).unwrap();
        let undo = run(&mut wb, ops);
        assert_eq!(shown(&wb, 0, 0, 4), "=C6+$C$7");
        assert_eq!(shown(&wb, 0, 1, 4), "=SUM(C6:C7)");
        assert_eq!(shown(&wb, 0, 2, 4), "=SUM(C6:C8)");
        assert_eq!(shown(&wb, 1, 0, 0), "=Sheet1!C7*2");
        assert_eq!(shown(&wb, 0, 7, 2), "=J10");
        undo.apply(&mut wb);
        assert_eq!(shown(&wb, 0, 0, 4), "=A1+$A$2");
        assert_eq!(shown(&wb, 1, 0, 0), "=Sheet1!A2*2");
        // Moved to another sheet: what stays is named by its sheet.
        let clip = Clip::take(&wb, rect(1, 0, 2, 0), true);
        let (ops, _) = clip.paste(&wb, 1, 3, 3).unwrap();
        run(&mut wb, ops);
        assert_eq!(shown(&wb, 1, 4, 3), "=Sheet1!J10");
        assert_eq!(shown(&wb, 1, 0, 0), "=Sheet2!D4*2");
        assert_eq!(shown(&wb, 0, 2, 4), "=SUM(A1:A3)");
        assert_eq!(shown(&wb, 0, 1, 4), "=SUM(A1:A2)");
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
