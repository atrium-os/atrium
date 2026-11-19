//! Inserting and deleting rows and columns (officina.md §16.69).
//!
//! [`shift_op`] builds the undoable edit from the workbook as it stands, out
//! of the ops that already exist: the sheet's cells and styles replaced with
//! themselves moved, every formula that refers to the sheet rewritten (a
//! reference into deleted cells becomes `#REF!`, a range loses the deleted
//! part and grows by rows inserted inside it), and the sheet's tables,
//! charts, pivot tables, conditional formats, filter and hidden rows moved
//! along. Its inverse is the inverse of each, so undo needs nothing new.
//!
//! Defined names and the formulas inside conditional-format rules follow as
//! cell formulas do, and row heights or column widths move with their rows or
//! columns.

use std::collections::HashSet;
use std::sync::Arc;

use crate::addr::{CellAddr, Rect, MAX_COLS, MAX_ROWS};
use crate::book::Workbook;
use crate::formula::{self, ast::{CellRef, Coord, Expr, RangeKind, RangeRef}};
use crate::ops::{Fill, GridOp, Region, StyleRegion};
use crate::value::ErrorKind;

/// Rows or columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Rows,
    Cols,
}

impl Axis {
    pub(crate) fn index(self) -> usize {
        match self { Self::Rows => 0, Self::Cols => 1 }
    }
}

/// `n` (> 0) inserted before `at`, or `-n` deleted from `at`, along one axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shift {
    pub axis: Axis,
    pub at: u32,
    pub n: i64,
}

impl Shift {
    fn max(&self) -> u32 {
        match self.axis {
            Axis::Rows => MAX_ROWS,
            Axis::Cols => MAX_COLS,
        }
    }

    /// Where coordinate `v` goes; `None` if it was deleted (or would fall
    /// off the sheet).
    pub fn map(&self, v: u32) -> Option<u32> {
        let (at, n) = (self.at as i64, self.n);
        let v = v as i64;
        let out = if n >= 0 {
            if v >= at { v + n } else { v }
        } else if v < at {
            v
        } else if v >= at - n {
            v + n
        } else {
            return None;
        };
        (out < self.max() as i64).then_some(out as u32)
    }

    /// Where the span `lo..=hi` goes: rows inserted inside it widen it,
    /// deleted ones narrow it; `None` if all of it was deleted.
    pub fn map_span(&self, lo: u32, hi: u32) -> Option<(u32, u32)> {
        if self.n >= 0 {
            let lo2 = self.map(lo)?;
            let hi2 = self.map(hi).unwrap_or(self.max() - 1);
            return Some((lo2, hi2));
        }
        let (at, k) = (self.at as i64, -self.n);
        let (lo, hi) = (lo as i64, hi as i64);
        let lo2 = if lo < at { lo } else if lo >= at + k { lo - k } else { at };
        let hi2 = if hi < at { hi } else if hi >= at + k { hi - k } else { at - 1 };
        (hi2 >= lo2 && lo2 >= 0).then_some((lo2 as u32, hi2 as u32))
    }

    /// `r` moved, along this axis.
    pub fn map_rect(&self, r: Rect) -> Option<Rect> {
        Some(match self.axis {
            Axis::Rows => {
                let (r0, r1) = self.map_span(r.r0, r.r1)?;
                Rect { r0, r1, ..r }
            }
            Axis::Cols => {
                let (c0, c1) = self.map_span(r.c0, r.c1)?;
                Rect { c0, c1, ..r }
            }
        })
    }

    /// Cell (`row`, `col`) moved.
    pub fn map_cell(&self, row: u32, col: u32) -> Option<(u32, u32)> {
        match self.axis {
            Axis::Rows => Some((self.map(row)?, col)),
            Axis::Cols => Some((row, self.map(col)?)),
        }
    }
}

/// Re-encodes `c` (a coordinate for a formula at `old`) for a formula at
/// `new`, its target moved by `m` when given; `None` when it falls off.
fn recode(c: Coord, old: u32, new: u32, max: u32, m: Option<&dyn Fn(u32) -> Option<u32>>) -> Result<Coord, ()> {
    let Some(t) = c.resolve(old, max) else { return Ok(c) };
    let t = match m {
        Some(f) => f(t).ok_or(())?,
        None => t,
    };
    Ok(match c {
        Coord::Abs(_) => Coord::Abs(t),
        Coord::Rel(_) => Coord::Rel((t as i64 - new as i64) as i32),
    })
}

/// What one formula becomes: written for (`or`, `oc`) on `own`, now at
/// (`nr`, `nc`), with sheet `target` shifted by `s`.
struct Adjust {
    own: u16,
    target: u16,
    s: Shift,
    or: u32,
    oc: u32,
    nr: u32,
    nc: u32,
}

impl Adjust {
    fn on_target(&self, sheet: Option<u16>) -> bool {
        sheet.unwrap_or(self.own) == self.target
    }

    fn cell(&self, r: &CellRef) -> Option<CellRef> {
        let hit = self.on_target(r.sheet);
        let m = |v: u32| self.s.map(v);
        let (rm, cm): (Option<&dyn Fn(u32) -> Option<u32>>, Option<&dyn Fn(u32) -> Option<u32>>) = match (hit, self.s.axis) {
            (true, Axis::Rows) => (Some(&m), None),
            (true, Axis::Cols) => (None, Some(&m)),
            _ => (None, None),
        };
        let row = recode(r.row, self.or, self.nr, MAX_ROWS, rm).ok()?;
        let col = recode(r.col, self.oc, self.nc, MAX_COLS, cm).ok()?;
        Some(CellRef { sheet: r.sheet, row, col })
    }

    fn range(&self, r: &RangeRef) -> Option<RangeRef> {
        let hit = self.on_target(r.sheet);
        // Whole columns don't move with rows, nor whole rows with columns.
        let along = hit && !matches!((self.s.axis, r.kind), (Axis::Rows, RangeKind::Cols) | (Axis::Cols, RangeKind::Rows));
        let span = |a: Coord, b: Coord, o: u32, n: u32, max: u32, moving: bool| -> Option<(Coord, Coord)> {
            if !moving {
                return Some((recode(a, o, n, max, None).ok()?, recode(b, o, n, max, None).ok()?));
            }
            let (Some(ta), Some(tb)) = (a.resolve(o, max), b.resolve(o, max)) else { return Some((a, b)) };
            let (lo, hi) = (ta.min(tb), ta.max(tb));
            let (lo2, hi2) = self.s.map_span(lo, hi)?;
            let (na, nb) = if ta <= tb { (lo2, hi2) } else { (hi2, lo2) };
            let enc = |c: Coord, t: u32| match c {
                Coord::Abs(_) => Coord::Abs(t),
                Coord::Rel(_) => Coord::Rel((t as i64 - n as i64) as i32),
            };
            Some((enc(a, na), enc(b, nb)))
        };
        let (r0, r1) = span(r.r0, r.r1, self.or, self.nr, MAX_ROWS, along && self.s.axis == Axis::Rows)?;
        let (c0, c1) = span(r.c0, r.c1, self.oc, self.nc, MAX_COLS, along && self.s.axis == Axis::Cols)?;
        Some(RangeRef { sheet: r.sheet, r0, c0, r1, c1, kind: r.kind })
    }

    fn expr(&self, e: &Expr) -> Expr {
        let b = |x: &Expr| Box::new(self.expr(x));
        match e {
            Expr::Ref(r) => self.cell(r).map_or(Expr::Err(ErrorKind::Ref), Expr::Ref),
            Expr::Spill(r) => self.cell(r).map_or(Expr::Err(ErrorKind::Ref), Expr::Spill),
            Expr::Range(r) => self.range(r).map_or(Expr::Err(ErrorKind::Ref), Expr::Range),
            Expr::Neg(x) => Expr::Neg(b(x)),
            Expr::Plus(x) => Expr::Plus(b(x)),
            Expr::Pct(x) => Expr::Pct(b(x)),
            Expr::Paren(x) => Expr::Paren(b(x)),
            Expr::At(x) => Expr::At(b(x)),
            Expr::Bin(op, x, y) => Expr::Bin(*op, b(x), b(y)),
            Expr::Call(f, args) => Expr::Call(f.clone(), args.iter().map(|a| self.expr(a)).collect()),
            Expr::Invoke(g, args) => Expr::Invoke(b(g), args.iter().map(|a| self.expr(a)).collect()),
            Expr::Array(rows) => Expr::Array(rows.iter().map(|r| r.iter().map(|a| self.expr(a)).collect()).collect()),
            other => other.clone(),
        }
    }
}

/// Whether `e` names sheet `target` (or, with `own == target`, has any
/// reference at all).
fn mentions(e: &Expr, own: u16, target: u16) -> bool {
    let mut hit = false;
    e.walk(&mut |x| match x {
        Expr::Ref(r) | Expr::Spill(r) => hit |= r.sheet.unwrap_or(own) == target,
        Expr::Range(r) => hit |= r.sheet.unwrap_or(own) == target,
        _ => {}
    });
    hit
}

/// Adds the fills of formula cells `cells` (old row, old col, new row, new
/// col, new expression), in order down each column, joining runs whose
/// relative expressions are the same.
pub(crate) fn fills_of(wb: &Workbook, mut cells: Vec<(u32, u32, Arc<Expr>)>, out: &mut Vec<Fill>) {
    cells.sort_by_key(|c| (c.1, c.0));
    let mut last: Option<(u32, u32, Arc<Expr>)> = None;
    for (r, c, e) in cells {
        if let (Some((lc, lr, le)), Some(f)) = (&last, out.last_mut()) {
            if *lc == c && *lr + 1 == r && **le == *e {
                f.r1 = r;
                last = Some((c, r, e));
                continue;
            }
        }
        out.push(Fill { col: c, r0: r, r1: r, src: formula::to_a1(&e, r, c, &|id| wb.sheet_name(id)) });
        last = Some((c, r, e));
    }
}

/// The edit that inserts (`n` > 0) or deletes (`n` < 0) rows or columns of
/// `sheet` at `at`; `None` if there is nothing to do or it would push cells
/// off the sheet.
pub fn shift_op(wb: &Workbook, sheet: u16, axis: Axis, at: u32, n: i64) -> Option<GridOp> {
    if n > i64::from(MAX_ROWS) { return None; }
    if n > 0 {
        let inserted = [crate::axis_ids::Run::inserted(n as u32)];
        shift_with_ids(wb, sheet, axis, at, n, Some(&inserted))
    } else {
        shift_with_ids(wb, sheet, axis, at, n, Some(&[]))
    }
}

pub(crate) fn shift_with_ids(
    wb: &Workbook, sheet: u16, axis: Axis, at: u32, n: i64,
    inserted: Option<&[crate::axis_ids::Run]>,
) -> Option<GridOp> {
    if n == 0 || sheet as usize >= wb.sheet_count() {
        return None;
    }
    let s = Shift { axis, at, n };
    let max = s.max();
    if at >= max || n.unsigned_abs() > u64::from(max - at) { return None; }

    let (rows, cols) = wb.extent(sheet);
    let grow = n.max(0) as u32;
    let (nrows, ncols) = match axis {
        Axis::Rows => (rows.checked_add(grow)?, cols),
        Axis::Cols => (rows, cols.checked_add(grow)?),
    };
    if (axis == Axis::Rows && nrows > MAX_ROWS) || (axis == Axis::Cols && ncols > MAX_COLS) {
        return None;
    }
    // Something on the far side of `at` to move (or delete).
    let moves = match axis {
        Axis::Rows => at < rows,
        Axis::Cols => at < cols,
    };
    let mut ops = Vec::new();

    // The sheet's cells: all of them replaced, moved.
    if moves && rows > 0 && cols > 0 {
        let old = Rect { sheet, r0: 0, c0: 0, r1: rows - 1, c1: cols - 1 };
        let rect = Rect { sheet, r0: 0, c0: 0, r1: nrows.max(rows) - 1, c1: ncols.max(cols) - 1 };
        let mut in_group: HashSet<(u32, u32)> = HashSet::new();
        let mut cells = Vec::new();
        for (_, g) in wb.groups() {
            if g.sheet != sheet {
                continue;
            }
            for r in g.r0..=g.r1 {
                in_group.insert((r, g.col));
                let Some((nr, nc)) = s.map_cell(r, g.col) else { continue };
                let a = Adjust { own: sheet, target: sheet, s, or: r, oc: g.col, nr, nc };
                cells.push((nr, nc, Arc::new(a.expr(&g.expr))));
            }
        }
        let spilled: Vec<Rect> = wb.spills().filter(|(a, _)| a.sheet == sheet).filter_map(|(a, _)| wb.spill_area(*a)).collect();
        let mut values = Vec::new();
        wb.each(&old, &mut |r, c, v| {
            if in_group.contains(&(r, c)) || spilled.iter().any(|a| a.contains(CellAddr::new(sheet, r, c)) && !(a.r0 == r && a.c0 == c)) {
                return;
            }
            if let Some((nr, nc)) = s.map_cell(r, c) {
                values.push((nr, nc, v));
            }
        });
        let mut fills = Vec::new();
        fills_of(wb, cells, &mut fills);
        ops.push(GridOp::Replace(Region { rect, values, fills }));
        // Styles move with their cells; inserted ones start plain.
        let snap = wb.style_snapshot(&old);
        let mut spans = Vec::new();
        for (c, r0, r1, st) in snap.spans {
            match axis {
                Axis::Cols => {
                    if let Some(c2) = s.map(c) {
                        spans.push((c2, r0, r1, st));
                    }
                }
                Axis::Rows if n > 0 && r0 < at && r1 >= at => {
                    spans.push((c, r0, at - 1, st.clone()));
                    spans.push((c, at + grow, r1 + grow, st));
                }
                Axis::Rows => {
                    if let Some((a, b)) = s.map_span(r0, r1) {
                        spans.push((c, a, b, st));
                    }
                }
            }
        }
        ops.push(GridOp::Restyle(StyleRegion { rect, spans }));
    }

    // Formulas on other sheets that refer to this one.
    for (_, g) in wb.groups() {
        if g.sheet == sheet || !mentions(&g.expr, g.sheet, sheet) {
            continue;
        }
        let mut cells = Vec::new();
        let mut changed = false;
        for r in g.r0..=g.r1 {
            let a = Adjust { own: g.sheet, target: sheet, s, or: r, oc: g.col, nr: r, nc: g.col };
            let e = a.expr(&g.expr);
            changed |= e != *g.expr;
            cells.push((r, g.col, Arc::new(e)));
        }
        if changed {
            let mut fills = Vec::new();
            fills_of(wb, cells, &mut fills);
            ops.push(GridOp::Replace(Region { rect: Rect { sheet: g.sheet, r0: g.r0, c0: g.col, r1: g.r1, c1: g.col }, values: Vec::new(), fills }));
        }
    }

    let on = |r: &Rect| r.sheet == sheet;
    // Tables: their rectangles; one whose header goes is dropped.
    let tables: Vec<_> = wb.tables().iter().filter_map(|t| {
        if !on(&t.rect) {
            return Some(t.clone());
        }
        let rect = s.map_rect(t.rect)?;
        if t.header && axis == Axis::Rows && s.map(t.rect.r0).is_none() {
            return None;
        }
        let mut t = t.clone();
        if axis == Axis::Cols {
            // Columns deleted take their names.
            t.columns = (t.rect.c0..=t.rect.c1).zip(t.columns.iter()).filter(|(c, _)| s.map(*c).is_some()).map(|(_, n)| n.clone()).collect();
            if t.columns.len() < (rect.c1 - rect.c0 + 1) as usize {
                // Columns inserted inside get new names.
                let mut k = 1;
                while t.columns.len() < (rect.c1 - rect.c0 + 1) as usize {
                    let name: Arc<str> = format!("Column{k}").into();
                    if !t.columns.iter().any(|c| c.eq_ignore_ascii_case(&name)) {
                        let i = (at.saturating_sub(t.rect.c0) as usize).min(t.columns.len());
                        t.columns.insert(i, name);
                    }
                    k += 1;
                }
            }
        }
        t.rect = rect;
        Some(t)
    }).collect();
    if tables != wb.tables() {
        ops.push(GridOp::Tables(tables));
    }

    // Charts: their data and where they sit.
    let charts: Vec<_> = wb.charts().iter().filter_map(|c| {
        let mut c = c.clone();
        let mut series = Vec::new();
        for mut x in c.series.clone() {
            if on(&x.vals) {
                x.vals = s.map_rect(x.vals)?;
            }
            if let Some(cats) = x.cats.filter(on) {
                x.cats = s.map_rect(cats);
            }
            if let Some(crate::charts::ChartText::Cell(a)) = x.name.clone() {
                if a.sheet == sheet {
                    x.name = s.map_cell(a.row, a.col).map(|(r, cc)| crate::charts::ChartText::Cell(CellAddr::new(sheet, r, cc)));
                }
            }
            series.push(x);
        }
        if series.is_empty() {
            return None;
        }
        c.series = series;
        if c.anchor.sheet == sheet {
            let pin = |p: (u32, u32, f32, f32)| {
                let (r, cc) = match axis {
                    Axis::Rows => (s.map_span(p.0, p.0).map_or(at.min(p.0), |x| x.0), p.1),
                    Axis::Cols => (p.0, s.map_span(p.1, p.1).map_or(at.min(p.1), |x| x.0)),
                };
                (r, cc, p.2, p.3)
            };
            c.anchor.from = pin(c.anchor.from);
            c.anchor.to = pin(c.anchor.to);
        }
        Some(c)
    }).collect();
    if charts != wb.charts() {
        ops.push(GridOp::Charts(charts));
    }

    // Pivot tables: their sources and outputs.
    let pivots = wb.pivot_state();
    let moved: Vec<_> = pivots.iter().filter_map(|(p, area)| {
        let mut p = p.clone();
        if on(&p.source) {
            p.source = s.map_rect(p.source)?;
        }
        let mut area = *area;
        if p.dest.sheet == sheet {
            let (r, c) = s.map_cell(p.dest.row, p.dest.col)?;
            p.dest = CellAddr::new(sheet, r, c);
            area = area.and_then(|a| s.map_rect(a));
        }
        Some((p, area))
    }).collect();
    if moved != pivots {
        ops.push(GridOp::Pivots(moved));
    }

    // Conditional formats: their ranges, and their rules' formulas (written for
    // the first range's top-left, which may move differently from what they read).
    let cfs: Vec<_> = wb.cond_formats().iter().filter_map(|f| {
        let mut f = f.clone();
        let (or, oc) = f.origin();
        if f.sheet == sheet {
            f.ranges = f.ranges.iter().filter_map(|r| s.map_rect(*r)).collect();
            if f.ranges.is_empty() {
                return None;
            }
        }
        let (nr, nc) = f.origin();
        let a = Adjust { own: f.sheet, target: sheet, s, or, oc, nr, nc };
        for r in &mut f.rules {
            r.kind = r.kind.map_exprs(&|e| a.expr(e));
        }
        Some(f)
    }).collect();
    if cfs != wb.cond_formats() {
        ops.push(GridOp::cond_formats(wb, &cfs));
    }

    // Validation rules, as conditional formats.
    let vs: Vec<_> = wb.validations().iter().filter_map(|v| {
        let mut v = v.clone();
        let (or, oc) = v.origin();
        if v.sheet == sheet {
            v.ranges = v.ranges.iter().filter_map(|r| s.map_rect(*r)).collect();
            if v.ranges.is_empty() {
                return None;
            }
        }
        let (nr, nc) = v.origin();
        let a = Adjust { own: v.sheet, target: sheet, s, or, oc, nr, nc };
        Some(v.map_exprs(&|e| a.expr(e)))
    }).collect();
    if vs != wb.validations() {
        ops.push(GridOp::validations(wb, &vs));
    }

    // Comments and links: with their cells; those on deleted cells go.
    let set = crate::links::moved(wb.comments(sheet), |r, c| s.map_cell(r, c));
    if !set.is_empty() {
        ops.push(GridOp::Comments { sheet, set });
    }
    let set = crate::links::moved(wb.links(sheet), |r, c| s.map_cell(r, c));
    if !set.is_empty() {
        ops.push(GridOp::Links { sheet, set });
    }

    // Merged cells: they move, grow by what's inserted inside and shrink by
    // what's deleted; one left a single cell is no longer merged.
    let merges: Vec<Rect> = wb.merges(sheet).iter().filter_map(|r| s.map_rect(*r)).filter(|r| r.cells() > 1).collect();
    if merges != wb.merges(sheet) {
        ops.push(GridOp::Merges { sheet, list: merges });
    }

    // Hidden columns' remembered widths go with their columns.
    if axis == Axis::Cols {
        let hw = &wb.sheet(sheet).hidden_widths;
        let moved: Vec<(u32, f64)> = hw.iter().filter_map(|&(c, w)| s.map(c).map(|c| (c, w))).collect();
        if moved != *hw {
            ops.push(GridOp::HiddenWidths { sheet, list: moved });
        }
    }

    // The filter and hidden rows.
    let st = wb.filter_state(sheet);
    let mut st2 = st.clone();
    if let Some(f) = st2.filter.as_mut() {
        match s.map_rect(f.rect) {
            Some(r) => {
                f.rect = r;
                if axis == Axis::Cols {
                    f.conds = f.conds.iter().filter_map(|(c, k)| s.map(*c).map(|c| (c, k.clone()))).collect();
                }
            }
            None => st2.filter = None,
        }
    }
    if axis == Axis::Rows {
        st2.hidden = st.hidden.iter().filter_map(|&(a, b)| match (n > 0 && a < at && b >= at, s.map_span(a, b)) {
            // Rows inserted inside a hidden run show.
            (true, _) => None,
            (false, m) => m,
        }).chain(st.hidden.iter().filter(|&&(a, b)| n > 0 && a < at && b >= at).flat_map(|&(a, b)| [(a, at - 1), (at + grow, b + grow)])).collect();
        st2.hidden.sort();
    }
    if st2 != st {
        ops.push(GridOp::Filter(st2));
    }

    // Defined names.
    let defs: Vec<_> = wb.names().iter().cloned().collect();
    let moved: Vec<_> = defs
        .iter()
        .map(|d| {
            let a = Adjust { own: d.scope.unwrap_or(0), target: sheet, s, or: 0, oc: 0, nr: 0, nc: 0 };
            crate::names::NameDef { expr: Arc::new(a.expr(&d.expr)), ..d.clone() }
        })
        .collect();
    if moved.iter().zip(&defs).any(|(a, b)| a.expr != b.expr) {
        ops.push(GridOp::names(wb, &moved));
    }

    // Row heights or column widths.
    let sh = wb.sheet(sheet);
    let sizes = match axis {
        Axis::Rows => &sh.row_sizes,
        Axis::Cols => &sh.col_sizes,
    };
    let over: Vec<(u32, f64)> = sizes.overrides().iter().filter_map(|&(i, v)| s.map(i).map(|j| (j, v))).collect();
    if over != sizes.overrides() {
        ops.push(GridOp::Sizes { sheet, rows: axis == Axis::Rows, over });
    }

    if let Some(inserted) = inserted {
        if inserted.iter().map(|r| i64::from(r.len)).sum::<i64>() != n.max(0) { return None; }
        let ids = wb.sheet(sheet).axis_ids[axis.index()].splice(at, (-n).max(0) as u32, inserted)?;
        ops.push(GridOp::AxisIdentities { sheet, axis, ids });
    }
    if ops.is_empty() {
        return None;
    }
    Some(if ops.len() == 1 { ops.pop().unwrap() } else { GridOp::Batch(ops) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::book::Input;
    use crate::value::Value;
    use officina_core::history::Op;

    fn book(rows: &[&[&str]]) -> Workbook {
        let mut wb = Workbook::new();
        wb.add_sheet("Sheet1").unwrap();
        wb.add_sheet("Other").unwrap();
        for (r, row) in rows.iter().enumerate() {
            for (c, s) in row.iter().enumerate() {
                if !s.is_empty() {
                    wb.set_input(CellAddr::new(0, r as u32, c as u32), s).unwrap();
                }
            }
        }
        wb.recalc();
        wb
    }

    fn input(wb: &Workbook, s: u16, r: u32, c: u32) -> String {
        match wb.input(CellAddr::new(s, r, c)) {
            Input::Empty => String::new(),
            Input::Value(v) => v.display(),
            Input::Formula(f) => f,
        }
    }

    fn grid(wb: &Workbook, s: u16) -> Vec<Vec<String>> {
        let (n, c) = wb.extent(s);
        let mut v: Vec<Vec<String>> = (0..n).map(|r| (0..c).map(|k| input(wb, s, r, k)).collect()).collect();
        while v.last().is_some_and(|r| r.iter().all(String::is_empty)) {
            v.pop();
        }
        for r in &mut v {
            while r.last().is_some_and(String::is_empty) {
                r.pop();
            }
        }
        v
    }

    fn apply(wb: &mut Workbook, op: GridOp) -> GridOp {
        let inv = op.apply(wb);
        wb.recalc();
        inv
    }

    #[test]
    fn inserting_rows_moves_cells_and_references() {
        let mut wb = book(&[&["1", "=A1*2"], &["2", "=A2*2"], &["3", "=SUM(A1:A3)"], &["=A3+$A$2"]]);
        wb.set_input(CellAddr::new(1, 0, 0), "=Sheet1!A3+SUM(Sheet1!A1:A3)").unwrap();
        wb.recalc();
        let before = (grid(&wb, 0), grid(&wb, 1));
        // Two rows before row 2 (index 1).
        let op = shift_op(&wb, 0, Axis::Rows, 1, 2).unwrap();
        let inv = apply(&mut wb, op);
        assert_eq!(grid(&wb, 0), vec![
            vec!["1".to_string(), "=A1*2".into()],
            vec![],
            vec![],
            vec!["2".into(), "=A4*2".into()],
            vec!["3".into(), "=SUM(A1:A5)".into()],
            vec!["=A5+$A$4".into()],
        ]);
        assert_eq!(input(&wb, 1, 0, 0), "=Sheet1!A5+SUM(Sheet1!A1:A5)");
        assert_eq!(wb.value(0, 4, 1), Value::Num(6.0));
        assert_eq!(wb.value(1, 0, 0), Value::Num(9.0));
        apply(&mut wb, inv);
        assert_eq!((grid(&wb, 0), grid(&wb, 1)), before);
    }

    #[test]
    fn deleting_rows_narrows_ranges_and_breaks_references() {
        let mut wb = book(&[&["1"], &["2"], &["3"], &["4"], &["=SUM(A1:A4)", "=A2", "=A4"]]);
        let before = grid(&wb, 0);
        let op = shift_op(&wb, 0, Axis::Rows, 1, -2).unwrap();
        let inv = apply(&mut wb, op);
        assert_eq!(grid(&wb, 0), vec![vec!["1".to_string()], vec!["4".into()], vec!["=SUM(A1:A2)".into(), "=#REF!".into(), "=A2".into()]]);
        assert_eq!(wb.value(0, 2, 0), Value::Num(5.0));
        apply(&mut wb, inv);
        assert_eq!(grid(&wb, 0), before);
    }

    #[test]
    fn columns_and_the_sheets_parts_move() {
        let mut wb = book(&[&["Name", "Qty", "Price"], &["pens", "3", "2"], &["ink", "4", "5"], &["", "=SUM(B2:B3)", "=B4*C3"]]);
        let t = crate::tables::Table { name: "T".into(), rect: Rect { sheet: 0, r0: 0, c0: 0, r1: 2, c1: 2 }, header: true, totals: false, columns: vec!["Name".into(), "Qty".into(), "Price".into()], style: None, stripes: true };
        GridOp::Tables(vec![t]).apply(&mut wb);
        let before = grid(&wb, 0);
        // A column before Qty.
        let op = shift_op(&wb, 0, Axis::Cols, 1, 1).unwrap();
        let inv = apply(&mut wb, op);
        assert_eq!(input(&wb, 0, 3, 2), "=SUM(C2:C3)");
        assert_eq!(input(&wb, 0, 3, 3), "=C4*D3");
        let t = &wb.tables()[0];
        assert_eq!((t.rect.c0, t.rect.c1, t.columns.len()), (0, 3, 4));
        assert_eq!(&*t.columns[1], "Column1");
        apply(&mut wb, inv);
        assert_eq!(grid(&wb, 0), before);
        assert_eq!(wb.tables()[0].rect.c1, 2);
        // Deleting the table's header row drops it.
        let op = shift_op(&wb, 0, Axis::Rows, 0, -1).unwrap();
        let inv = apply(&mut wb, op);
        assert!(wb.tables().is_empty());
        apply(&mut wb, inv);
        assert_eq!(wb.tables().len(), 1);
    }

    #[test]
    fn conditional_format_formulas_follow_what_they_read() {
        use crate::condfmt::{CmpOp, CondFormat, Rule, RuleKind};
        let mut wb = book(&[&["1", "2"], &["3", "4"], &["5", "6"], &["7", "8"]]);
        if wb.sheet_count() < 2 {
            wb.add_sheet("Other").unwrap();
        }
        let rule = |src: &str, origin| Rule { kind: RuleKind::Expression(wb.cond_formula(src, 0, origin).unwrap()), dxf: Default::default(), stop: false };
        // On sheet 0 over B3:B4, reading $A$1 and the cell to its left.
        let a = CondFormat { sheet: 0, ranges: vec![Rect { sheet: 0, r0: 2, c0: 1, r1: 3, c1: 1 }], rules: vec![rule("A3>$A$1", (2, 1))] };
        // On the other sheet, reading sheet 0's A2 and A4 (Sheet1 named by the book).
        let name = wb.sheet_name(0).to_string();
        let b_rule = Rule { kind: RuleKind::CellIs { op: CmpOp::Greater, a: wb.cond_formula(&format!("{name}!$A$2"), 1, (0, 0)).unwrap(), b: None }, dxf: Default::default(), stop: false };
        let b = CondFormat { sheet: 1, ranges: vec![Rect { sheet: 1, r0: 0, c0: 0, r1: 1, c1: 0 }], rules: vec![b_rule] };
        GridOp::cond_formats(&wb, &[a, b]).apply(&mut wb);
        let text = |wb: &Workbook, i: usize| {
            let f = &wb.cond_formats()[i];
            match &f.rules[0].kind {
                RuleKind::Expression(e) | RuleKind::CellIs { a: e, .. } => wb.cond_formula_text(e, f.origin()),
                _ => unreachable!(),
            }
        };
        // A row inserted at row 2 (between A1 and A3).
        let op = shift_op(&wb, 0, Axis::Rows, 1, 1).unwrap();
        let inv = apply(&mut wb, op);
        assert_eq!(wb.cond_formats()[0].ranges[0].r0, 3);
        assert_eq!(text(&wb, 0), "A4>$A$1");
        assert_eq!(text(&wb, 1), format!("{name}!$A$3"));
        apply(&mut wb, inv);
        assert_eq!(text(&wb, 0), "A3>$A$1");
        assert_eq!(text(&wb, 1), format!("{name}!$A$2"));
        // Deleting row 1 breaks the absolute reference; a column before A moves both.
        let op = shift_op(&wb, 0, Axis::Rows, 0, -1).unwrap();
        let inv = apply(&mut wb, op);
        assert_eq!(text(&wb, 0), "A2>#REF!");
        apply(&mut wb, inv);
        let op = shift_op(&wb, 0, Axis::Cols, 0, 1).unwrap();
        apply(&mut wb, op);
        assert_eq!(text(&wb, 0), "B3>$B$1");
        assert_eq!(text(&wb, 1), format!("{name}!$B$2"));
    }

    #[test]
    fn a_fill_through_the_insert_is_split_and_still_relative() {
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        let vals: Vec<f64> = (0..1000).map(|r| r as f64).collect();
        wb.set_numbers(0, 0, 0, &vals);
        wb.fill_formula(0, 1, 0, 999, "=A1+1").unwrap();
        wb.recalc();
        let op = shift_op(&wb, 0, Axis::Rows, 500, 10).unwrap();
        let inv = apply(&mut wb, op);
        assert_eq!(input(&wb, 0, 499, 1), "=A500+1");
        assert_eq!(input(&wb, 0, 510, 1), "=A511+1");
        assert_eq!(wb.value(0, 1009, 1), Value::Num(1000.0));
        assert!(wb.value(0, 505, 1).is_empty());
        let groups = wb.groups().filter(|(_, g)| g.sheet == 0).count();
        assert_eq!(groups, 2, "two pieces of one fill");
        apply(&mut wb, inv);
        assert_eq!(wb.value(0, 999, 1), Value::Num(1000.0));
    }

    #[test]
    fn random_inserts_and_deletes_undo_exactly() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut rnd = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        for _ in 0..200 {
            let mut wb = Workbook::new();
            wb.add_sheet("S").unwrap();
            for r in 0..8u32 {
                for c in 0..4u32 {
                    let s = match rnd(4) {
                        0 => String::new(),
                        1 => format!("={}{}+1", ["A", "B", "C", "D"][rnd(4) as usize], 1 + rnd(8)),
                        2 => format!("=SUM(A{}:B{})", 1 + rnd(4), 5 + rnd(4)),
                        _ => format!("{}", rnd(50)),
                    };
                    if !s.is_empty() && !(s.starts_with('=') && s.contains(&format!("{}{}", ["A", "B", "C", "D"][c as usize], r + 1))) {
                        let _ = wb.set_input(CellAddr::new(0, r, c), &s);
                    }
                }
            }
            wb.recalc();
            let before = grid(&wb, 0);
            let axis = if rnd(2) == 0 { Axis::Rows } else { Axis::Cols };
            let n = if rnd(2) == 0 { 1 + rnd(3) as i64 } else { -(1 + rnd(3) as i64) };
            if let Some(op) = shift_op(&wb, 0, axis, rnd(9) as u32, n) {
                let inv = apply(&mut wb, op);
                apply(&mut wb, inv);
                assert_eq!(grid(&wb, 0), before, "{axis:?} {n}");
            }
        }
    }

    #[test]
    fn a_big_sheet_inserts_a_row_in_time() {
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        let n = 200_000u32;
        let vals: Vec<f64> = (0..n).map(|r| r as f64).collect();
        wb.set_numbers(0, 0, 0, &vals);
        wb.fill_formula(0, 1, 0, n - 1, "=A1*2").unwrap();
        wb.set_input(CellAddr::new(0, 0, 2), "=SUM(B1:B200000)").unwrap();
        wb.recalc();
        let t = std::time::Instant::now();
        let op = shift_op(&wb, 0, Axis::Rows, 10, 1).unwrap();
        let built = t.elapsed();
        let inv = op.apply(&mut wb);
        let applied = t.elapsed();
        wb.recalc();
        assert_eq!(input(&wb, 0, 0, 2), "=SUM(B1:B200001)");
        assert_eq!(wb.value(0, 200_000, 1), Value::Num(2.0 * 199_999.0));
        inv.apply(&mut wb);
        eprintln!("insert a row in 200,000: built {built:?}, applied {applied:?}");
        assert!(applied.as_secs_f64() < 5.0);
    }

    #[test]
    fn names_and_sizes_move_too() {
        let mut wb = book(&[&["1"], &["2"], &["3"], &["=SUM(Total)"]]);
        wb.define_name("Total", None, "=Sheet1!$A$1:$A$3").unwrap();
        wb.define_name("Last", Some(0), "=$A$3").unwrap();
        wb.sheet_mut(0).row_sizes.set(2, 40.0);
        wb.sheet_mut(0).col_sizes.set(1, 120.0);
        wb.recalc();
        let op = shift_op(&wb, 0, Axis::Rows, 1, 2).unwrap();
        let inv = apply(&mut wb, op);
        let text = |wb: &Workbook, n: &str| wb.name_text(wb.names().get(0, n).unwrap());
        assert_eq!(text(&wb, "Total"), "=Sheet1!$A$1:$A$5");
        assert_eq!(text(&wb, "Last"), "=$A$5");
        assert_eq!(wb.value(0, 5, 0), Value::Num(6.0));
        assert_eq!(wb.sheet(0).row_sizes.size(4), 40.0);
        assert_eq!(wb.sheet(0).row_sizes.size(2), wb.sheet(0).row_sizes.default);
        // The op survives the journal.
        let back = GridOp::decode(&inv.encode()).unwrap();
        apply(&mut wb, back);
        assert_eq!(text(&wb, "Total"), "=Sheet1!$A$1:$A$3");
        assert_eq!(wb.sheet(0).row_sizes.size(2), 40.0);
        // Deleting column B takes its width; deleting the named rows breaks the name.
        let op = shift_op(&wb, 0, Axis::Cols, 1, -1).unwrap();
        apply(&mut wb, op);
        assert_eq!(wb.sheet(0).col_sizes.size(1), wb.sheet(0).col_sizes.default);
        let op = shift_op(&wb, 0, Axis::Rows, 0, -3).unwrap();
        apply(&mut wb, op);
        assert_eq!(text(&wb, "Total"), "=#REF!");
    }
}
