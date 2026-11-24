//! Copying and pasting cells within a workbook: values, formulas and styles.
//!
//! A copy keeps formulas as expressions, so pasting moves their relative
//! references with them (`=A1+1` copied from B1 to B5 reads `=A5+1`). A cut
//! moves the cells when pasted, as Excel does: the source is emptied in the
//! same step, and references to the moved cells (from anywhere, the moved
//! formulas too) follow them, while references to cells left behind stay.

mod system;
pub use system::MAX_SYSTEM_COPY_CELLS;

use std::sync::Arc;

use crate::addr::{CellAddr, Rect, MAX_COLS, MAX_ROWS};
use crate::book::Workbook;
use crate::formula::ast::{BinOp, CellRef, Coord, Expr, RangeKind, RangeRef};
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
    Moved(Arc<Expr>, Value),
}

/// The most copies one paste into a bigger area makes.
pub const MAX_TILES: u64 = 10_000;
/// The most cells one paste into a bigger area fills.
pub const MAX_TILED_CELLS: u64 = 4_000_000;

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
    /// Comments only.
    Comments,
}

/// Paste Special's arithmetic: each pasted number combined with the
/// number already in its cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arith {
    Add,
    Sub,
    Mul,
    Div,
}

/// How Paste Special pastes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Special {
    pub what: What,
    pub transpose: bool,
    /// Combine with what is there (numbers only; formulas there gain the
    /// operation, text there stays). Blank copied cells leave theirs alone.
    pub arith: Option<Arith>,
    /// Blank copied cells leave what is under them.
    pub skip_blanks: bool,
}

impl Special {
    pub fn of(what: What) -> Special {
        Special { what, transpose: false, arith: None, skip_blanks: false }
    }
}

/// Cells taken by a copy or a cut, by their place in the copied area.
#[derive(Debug, Clone)]
pub struct Clip {
    /// Where they were taken from.
    pub from: Rect,
    pub cut: bool,
    cells: Vec<(u32, u32, Content)>,
    styles: Vec<(u32, u32, u32, Style)>,
    /// Comments and links on the cells, by place in the area.
    notes: Vec<(u32, u32, crate::comments::Comment)>,
    links: Vec<(u32, u32, crate::links::Link)>,
}

impl Clip {
    /// Takes `rect` (cut: to move it). Only the used part is read, so a
    /// whole column costs what it holds.
    pub fn take(wb: &Workbook, rect: Rect, cut: bool) -> Clip {
        let (er, ec) = wb.extent(rect.sheet);
        let mut used = Rect { r1: rect.r1.min(er.saturating_sub(1)).max(rect.r0), c1: rect.c1.min(ec.saturating_sub(1)).max(rect.c0), ..rect };
        let formats = wb.style_snapshot(&rect);
        for (col, _, last, _) in &formats.spans {
            used.c1 = used.c1.max(*col);
            used.r1 = used.r1.max(*last);
        }
        let notes = wb.comments(rect.sheet).range((rect.r0, 0)..=(rect.r1, u32::MAX))
            .filter(|&(&(r, c), _)| {
                if c < rect.c0 || c > rect.c1 { return false; }
                used.r1 = used.r1.max(r);
                used.c1 = used.c1.max(c);
                true
            })
            .map(|(&(r, c), k)| (r - rect.r0, c - rect.c0, k.clone()))
            .collect();
        let snap = wb.snapshot(&used);
        let mut cells = Vec::new();
        for (r, c, v) in snap.values {
            if !wb.hides_value(CellAddr::new(rect.sheet, r, c)) {
                cells.push((r - rect.r0, c - rect.c0, Content::Value(v)));
            }
        }
        for f in &snap.fills {
            for r in f.r0..=f.r1 {
                let address = CellAddr::new(rect.sheet, r, f.col);
                if wb.hides_value(address) {
                    continue;
                }
                if !cut && wb.hides_formula(address) {
                    cells.push((r - rect.r0, f.col - rect.c0, Content::Value(wb.get(address))));
                    continue;
                }
                let content = match (cut, wb.group_at(rect.sheet, r, f.col)) {
                    (false, Some(g)) => Content::Expr(wb.group(g).expr.clone(), wb.value(rect.sheet, r, f.col)),
                    (true, Some(g)) => Content::Moved(wb.group(g).expr.clone(), wb.value(rect.sheet, r, f.col)),
                    _ => continue,
                };
                cells.push((r - rect.r0, f.col - rect.c0, content));
            }
        }
        let styles = formats.spans.into_iter().map(|(c, a, b, st)| (c - rect.c0, a - rect.r0, b - rect.r0, st)).collect();
        let inside = |&(r, c): &(u32, u32)| used.contains(CellAddr::new(rect.sheet, r, c));
        let links = wb.links(rect.sheet).iter().filter(|(k, _)| inside(k)).map(|(&(r, c), l)| (r - rect.r0, c - rect.c0, l.clone())).collect();
        Clip { from: used, cut, cells, styles, notes, links }
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

    /// Pasted into `area`: when it is a whole number of copies of the clip
    /// in each direction (more than one), the clip repeated to fill it, as
    /// Excel does; otherwise once at its top-left. A cut pastes once.
    pub fn paste_into(&self, wb: &Workbook, area: Rect) -> Result<(Vec<GridOp>, Rect), String> {
        self.paste_into_as(wb, area, What::All)
    }

    /// As [`paste_into`](Self::paste_into), bringing only `what`.
    pub fn paste_into_as(&self, wb: &Workbook, area: Rect, what: What) -> Result<(Vec<GridOp>, Rect), String> {
        let (h, w) = (area.rows(), area.cols());
        let (ch, cw) = (self.rows() as u64, self.cols() as u64);
        let (nr, nc) = (h / ch, w / cw);
        if self.cut || h % ch != 0 || w % cw != 0 || nr * nc <= 1 {
            return self.paste_as(wb, area.sheet, area.r0, area.c0, what, false);
        }
        if nr * nc > MAX_TILES || area.cells() > MAX_TILED_CELLS {
            return Err("That's too big an area to fill with copies; select a smaller one.".into());
        }
        let mut ops = Vec::new();
        for i in 0..nr {
            for j in 0..nc {
                let (o, _) = self.paste_as(wb, area.sheet, area.r0 + (i * ch) as u32, area.c0 + (j * cw) as u32, what, false)?;
                ops.extend(o);
            }
        }
        Ok((ops, area))
    }

    /// As [`paste`](Self::paste), bringing only `what`, and `transpose`d
    /// (rows become columns: references into the copied cells turn with
    /// them). A cut pastes only whole and as it is.
    pub fn paste_as(&self, wb: &Workbook, sheet: u16, row: u32, col: u32, what: What, transpose: bool) -> Result<(Vec<GridOp>, Rect), String> {
        self.paste_special(wb, sheet, row, col, Special { transpose, ..Special::of(what) })
    }

    /// As [`paste_as`](Self::paste_as), with arithmetic and skipping blanks.
    pub fn paste_special(&self, wb: &Workbook, sheet: u16, row: u32, col: u32, sp: Special) -> Result<(Vec<GridOp>, Rect), String> {
        let Special { what, transpose, arith, skip_blanks } = sp;
        if arith.is_some() && matches!(what, What::Formats | What::Comments) {
            return Err("Formats can't be added or multiplied.".into());
        }
        if self.cut && (what != What::All || transpose || arith.is_some() || skip_blanks) {
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
                Content::Expr(_, v) if what == What::Values => {
                    if !v.is_empty() {
                        values.push((r, c, v.clone()))
                    }
                }
                // Turned: references into the copied cells turn with them;
                // others move as a copy moves them.
                Content::Expr(e, _) if transpose => {
                    let m = Mover { transpose: true, keep_outside: true, ..self.mover(sheet, row, col) };
                    let e = m.expr(e, self.from.r0 + dr, self.from.c0 + dc, self.from.sheet, r, c, sheet);
                    exprs.push((r, c, Arc::new(e)));
                }
                Content::Expr(e, _) => exprs.push((r, c, e.clone())),
                Content::Moved(e, _) => {
                    let m = self.mover(sheet, row, col);
                    let e = m.expr(e, self.from.r0 + dr, self.from.c0 + dc, self.from.sheet, r, c, sheet);
                    fills.push(Fill { col: c, r0: r, r1: r, src: crate::formula::to_a1(&e, r, c, &|id| wb.sheet_name(id)) });
                }
            }
        }
        // Copied formulas as they land, for arithmetic with them.
        let pasted: std::collections::HashMap<(u32, u32), Arc<Expr>> = if arith.is_some() { exprs.iter().map(|(r, c, e)| ((*r, *c), e.clone())).collect() } else { Default::default() };
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
                if !reads(&g.expr, g.sheet, self.from) && !reads(&g.expr, g.sheet, to) {
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
        if let (Some(op), true) = (arith, !matches!(what, What::Formats | What::Comments)) {
            // Each copied number with the cell it lands on; a copied formula
            // (pasting all or formulas) is combined with it as a formula, as
            // Excel does: =(what was there)+(the copied formula).
            let bop = match op {
                Arith::Add => BinOp::Add,
                Arith::Sub => BinOp::Sub,
                Arith::Mul => BinOp::Mul,
                Arith::Div => BinOp::Div,
            };
            let mut cells = Vec::new();
            for (dr, dc, content) in &self.cells {
                let (r, c) = at(*dr, *dc);
                let right = match content {
                    Content::Expr(..) if what != What::Values => match pasted.get(&(r, c)) {
                        Some(e) => Expr::Paren(Box::new((**e).clone())),
                        None => continue,
                    },
                    Content::Value(Value::Num(n)) | Content::Expr(_, Value::Num(n)) => Expr::Num(*n),
                    _ => continue,
                };
                let left = match wb.group_at(sheet, r, c) {
                    Some(g) => Some(Expr::Paren(Box::new((*wb.group(g).expr).clone()))),
                    None => None,
                };
                let t = match (&left, wb.value(sheet, r, c)) {
                    (Some(_), _) => 0.0,
                    (None, Value::Empty) => 0.0,
                    (None, Value::Num(t)) => t,
                    _ => continue,
                };
                if left.is_some() || !matches!(right, Expr::Num(_)) {
                    let e = Expr::Bin(bop, Box::new(left.unwrap_or(Expr::Num(t))), Box::new(right));
                    let src = crate::formula::to_a1(&e, r, c, &|id| wb.sheet_name(id));
                    cells.push((r, c, None, Some(Fill { col: c, r0: r, r1: r, src })));
                    continue;
                }
                let Expr::Num(n) = right else { continue };
                let v = match op {
                    Arith::Add => Value::Num(t + n),
                    Arith::Sub => Value::Num(t - n),
                    Arith::Mul => Value::Num(t * n),
                    Arith::Div if n == 0.0 => Value::Err(crate::value::ErrorKind::Div0),
                    Arith::Div => Value::Num(t / n),
                };
                cells.push((r, c, Some(v), None));
            }
            ops.extend(by_runs(sheet, cells));
        } else if !matches!(what, What::Formats | What::Comments) && skip_blanks {
            let mut cells: Vec<_> = values.into_iter().map(|(r, c, v)| (r, c, Some(v), None)).collect();
            for f in fills {
                cells.push((f.r0, f.col, None, Some(f)));
            }
            ops.extend(by_runs(sheet, cells));
        } else if !matches!(what, What::Formats | What::Comments) {
            ops.push(GridOp::Replace(Region { rect: to, values, fills }));
        }
        if matches!(what, What::All | What::Formats) {
            ops.push(GridOp::Restyle(StyleRegion { rect: to, spans }));
        }
        // Comments and links come with the cells (a cut takes them away
        // from where they were first).
        if matches!(what, What::All | What::Comments) {
            let fsh = self.from.sheet;
            let place = |dr: u32, dc: u32| at(dr, dc);
            if !self.notes.is_empty() {
                let mut set: Vec<(u32, u32, Option<crate::comments::Comment>)> = Vec::new();
                if self.cut {
                    let gone: Vec<_> = self.notes.iter().map(|(dr, dc, _)| (self.from.r0 + dr, self.from.c0 + dc, None)).collect();
                    ops.push(GridOp::Comments { sheet: fsh, set: gone });
                }
                for (dr, dc, k) in &self.notes {
                    let (r, c) = place(*dr, *dc);
                    set.push((r, c, Some(k.clone())));
                }
                ops.push(GridOp::Comments { sheet, set });
            }
            if !self.links.is_empty() && what == What::All {
                if self.cut {
                    let gone: Vec<_> = self.links.iter().map(|(dr, dc, _)| (self.from.r0 + dr, self.from.c0 + dc, None)).collect();
                    ops.push(GridOp::Links { sheet: fsh, set: gone });
                }
                let set = self.links.iter().map(|(dr, dc, l)| { let (r, c) = place(*dr, *dc); (r, c, Some(l.clone())) }).collect();
                ops.push(GridOp::Links { sheet, set });
            }
        }
        if self.cut {
            ops.extend(self.follow(wb, sheet, row, col));
        }
        Ok((ops, to))
    }
}

impl Clip {
    /// What else points at cells a cut moves, moved with them: defined
    /// names, conditional formats and validation rules (their areas wholly
    /// inside, and their formulas), and charts' data.
    fn follow(&self, wb: &Workbook, sheet: u16, row: u32, col: u32) -> Vec<GridOp> {
        let m = self.mover(sheet, row, col);
        let mut ops = Vec::new();
        // An area wholly inside the cut, where it goes.
        let area = |r: Rect| -> Option<Rect> {
            (r.sheet == self.from.sheet && self.from.contains(CellAddr::new(r.sheet, r.r0, r.c0)) && self.from.contains(CellAddr::new(r.sheet, r.r1, r.c1))).then(|| {
                let (dr, dc) = (row as i64 - self.from.r0 as i64, col as i64 - self.from.c0 as i64);
                Rect { sheet, r0: (r.r0 as i64 + dr) as u32, c0: (r.c0 as i64 + dc) as u32, r1: (r.r1 as i64 + dr) as u32, c1: (r.c1 as i64 + dc) as u32 }
            })
        };
        // A list of areas: each wholly inside moves; onto another sheet,
        // only when all of them do.
        let areas = |own: u16, rs: &[Rect]| -> (u16, Vec<Rect>) {
            let all = rs.iter().all(|r| area(Rect { sheet: own, ..*r }).is_some());
            if own != sheet && !all {
                return (own, rs.to_vec());
            }
            let to = if all { sheet } else { own };
            (to, rs.iter().map(|r| area(Rect { sheet: own, ..*r }).map_or(*r, |a| Rect { sheet: to, ..a })).collect())
        };
        let defs: Vec<_> = wb.names().iter().cloned().collect();
        let moved: Vec<_> = defs.iter().map(|d| {
            let own = d.scope.unwrap_or(0);
            crate::names::NameDef { expr: Arc::new(m.expr(&d.expr, 0, 0, own, 0, 0, own)), ..d.clone() }
        }).collect();
        if moved.iter().zip(&defs).any(|(a, b)| a.expr != b.expr) {
            ops.push(GridOp::names(wb, &moved));
        }
        // A list of areas some of which the cut takes part of (or takes some
        // whole and leaves others): (the areas left, the parts taken, where
        // they were). Empty parts taken when it doesn't, or takes them all.
        let split = |own: u16, rs: &[Rect]| -> (Vec<Rect>, Vec<Rect>) {
            if own != self.from.sheet {
                return (Vec::new(), Vec::new());
            }
            let (mut stay, mut taken) = (Vec::new(), Vec::new());
            for r in rs {
                let r = Rect { sheet: own, ..*r };
                match meet(r, self.from) {
                    Some(i) => {
                        taken.push(i);
                        stay.extend(minus(r, i));
                    }
                    None => stay.push(r),
                }
            }
            if taken.is_empty() || stay.is_empty() {
                return (Vec::new(), Vec::new());
            }
            (stay, taken)
        };
        let (dr, dc) = (row as i64 - self.from.r0 as i64, col as i64 - self.from.c0 as i64);
        let there = |r: Rect| Rect { sheet, r0: (r.r0 as i64 + dr) as u32, c0: (r.c0 as i64 + dc) as u32, r1: (r.r1 as i64 + dr) as u32, c1: (r.c1 as i64 + dc) as u32 };
        let cfs: Vec<_> = wb.cond_formats().iter().flat_map(|f| {
            // Partly cut: the cells taken keep the rule where they go (a
            // copy of it, after it), and the rest keep it here, as Excel
            // splits a rule when some of its cells move.
            let (stay, taken) = split(f.sheet, &f.ranges);
            if !taken.is_empty() {
                let ((or, oc), own) = (f.origin(), f.sheet);
                let mut here = f.clone();
                here.ranges = stay;
                for r in &mut here.rules {
                    r.kind = r.kind.map_exprs(&|e| m.expr(e, or, oc, own, or, oc, own));
                }
                let (a, b) = (taken[0].r0, taken[0].c0);
                let mut gone = f.clone();
                gone.sheet = sheet;
                gone.ranges = taken.into_iter().map(there).collect();
                let (na, nb) = (gone.ranges[0].r0, gone.ranges[0].c0);
                for r in &mut gone.rules {
                    r.kind = r.kind.map_exprs(&|e| m.expr(e, a, b, own, na, nb, sheet));
                }
                return vec![here, gone];
            }
            let mut f = f.clone();
            let ((or, oc), own) = (f.origin(), f.sheet);
            let (to, ranges) = areas(f.sheet, &f.ranges);
            f.sheet = to;
            f.ranges = ranges;
            let (nr, nc) = f.origin();
            for r in &mut f.rules {
                r.kind = r.kind.map_exprs(&|e| m.expr(e, or, oc, own, nr, nc, to));
            }
            vec![f]
        }).collect();
        if cfs != wb.cond_formats() {
            ops.push(GridOp::cond_formats(wb, &cfs));
        }
        let vs: Vec<_> = wb.validations().iter().flat_map(|v| {
            // Partly cut: split as conditional formats are.
            let (stay, taken) = split(v.sheet, &v.ranges);
            if !taken.is_empty() {
                let ((or, oc), own) = (v.origin(), v.sheet);
                let mut here = v.clone();
                here.ranges = stay;
                let here = here.map_exprs(&|e| m.expr(e, or, oc, own, or, oc, own));
                let (a, b) = (taken[0].r0, taken[0].c0);
                let mut gone = v.clone();
                gone.sheet = sheet;
                gone.ranges = taken.into_iter().map(there).collect();
                let (na, nb) = (gone.ranges[0].r0, gone.ranges[0].c0);
                let gone = gone.map_exprs(&|e| m.expr(e, a, b, own, na, nb, sheet));
                return vec![here, gone];
            }
            let mut v = v.clone();
            let ((or, oc), own) = (v.origin(), v.sheet);
            let (to, ranges) = areas(v.sheet, &v.ranges);
            v.sheet = to;
            v.ranges = ranges;
            let (nr, nc) = v.origin();
            vec![v.map_exprs(&|e| m.expr(e, or, oc, own, nr, nc, to))]
        }).collect();
        if vs != wb.validations() {
            ops.push(GridOp::validations(wb, &vs));
        }
        let charts: Vec<_> = wb.charts().iter().map(|c| {
            let mut c = c.clone();
            for x in &mut c.series {
                x.vals = area(x.vals).unwrap_or(x.vals);
                x.cats = x.cats.map(|r| area(r).unwrap_or(r));
                if let Some(crate::charts::ChartText::Cell(a)) = x.name.clone() {
                    if let Some(t) = area(Rect { sheet: a.sheet, r0: a.row, c0: a.col, r1: a.row, c1: a.col }) {
                        x.name = Some(crate::charts::ChartText::Cell(CellAddr::new(t.sheet, t.r0, t.c0)));
                    }
                }
            }
            c
        }).collect();
        if charts != wb.charts() {
            ops.push(GridOp::Charts(charts));
        }
        ops
    }

    fn mover(&self, sheet: u16, row: u32, col: u32) -> Mover {
        let (dr, dc) = (row as i64 - self.from.r0 as i64, col as i64 - self.from.c0 as i64);
        let dead = self.cut.then(|| Rect { sheet, r0: row, c0: col, r1: row + self.rows() - 1, c1: col + self.cols() - 1 });
        Mover { from: self.from, sheet, dr, dc, transpose: false, keep_outside: false, dead }
    }
}

/// Replace edits for just these cells (a value or a fill starting at each),
/// one a run of touching cells down a column, leaving the cells between.
pub(crate) fn by_runs(sheet: u16, mut cells: Vec<(u32, u32, Option<Value>, Option<Fill>)>) -> Vec<GridOp> {
    cells.sort_by_key(|x| (x.1, x.0));
    let mut ops: Vec<GridOp> = Vec::new();
    for (r, c, v, f) in cells {
        let r1 = f.as_ref().map_or(r, |f| f.r1);
        let joins = matches!(ops.last(), Some(GridOp::Replace(g)) if g.rect.c0 == c && g.rect.r1 + 1 == r);
        if !joins {
            ops.push(GridOp::Replace(Region { rect: Rect { sheet, r0: r, c0: c, r1, c1: c }, values: Vec::new(), fills: Vec::new() }));
        }
        let Some(GridOp::Replace(g)) = ops.last_mut() else { unreachable!() };
        g.rect.r1 = r1;
        if let Some(v) = v {
            g.values.push((r, c, v));
        }
        if let Some(f) = f {
            g.fills.push(f);
        }
    }
    ops
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
    /// Rows become columns: a cell `k` rows down `from` lands `k` columns
    /// right of where `from`'s corner goes.
    transpose: bool,
    /// References outside `from` keep their relative form (a copy), rather
    /// than the cells they read (a move).
    keep_outside: bool,
    /// Cells a move writes over: references to them (a cell, or a range
    /// wholly inside) become `#REF!`, as Excel's do.
    dead: Option<Rect>,
}

/// The edits that turn references to the cells of `dead` (a cell inside,
/// or a range wholly inside) into `#REF!` in formulas outside it, as Excel
/// does when cells are deleted. Other references stay as they are.
pub fn kill_refs(wb: &Workbook, dead: Rect) -> Vec<GridOp> {
    // Nothing moves: `from` is on no sheet, so every reference is outside it.
    let nowhere = Rect { sheet: u16::MAX, r0: 0, c0: 0, r1: 0, c1: 0 };
    let m = Mover { from: nowhere, sheet: dead.sheet, dr: 0, dc: 0, transpose: false, keep_outside: true, dead: Some(dead) };
    let mut ops = Vec::new();
    for (_, g) in wb.groups() {
        if !reads(&g.expr, g.sheet, dead) {
            continue;
        }
        let mut cells = Vec::new();
        for r in g.r0..=g.r1 {
            if dead.contains(CellAddr::new(g.sheet, r, g.col)) {
                continue;
            }
            let e = m.expr(&g.expr, r, g.col, g.sheet, r, g.col, g.sheet);
            if e != *g.expr {
                cells.push((r, g.col, Arc::new(e)));
            }
        }
        let mut fills = Vec::new();
        crate::shift::fills_of(wb, cells, &mut fills);
        for f in fills {
            ops.push(GridOp::Replace(Region { rect: Rect { sheet: g.sheet, r0: f.r0, c0: f.col, r1: f.r1, c1: f.col }, values: Vec::new(), fills: vec![f] }));
        }
    }
    ops
}

/// Where `a` and `b` (on one sheet) overlap.
fn meet(a: Rect, b: Rect) -> Option<Rect> {
    let r = Rect { sheet: a.sheet, r0: a.r0.max(b.r0), c0: a.c0.max(b.c0), r1: a.r1.min(b.r1), c1: a.c1.min(b.c1) };
    (a.sheet == b.sheet && r.r0 <= r.r1 && r.c0 <= r.c1).then_some(r)
}

/// `r` without `i` (inside it): up to four areas, rows above and below
/// whole, then left and right of `i`.
fn minus(r: Rect, i: Rect) -> Vec<Rect> {
    let mut v = Vec::new();
    if i.r0 > r.r0 {
        v.push(Rect { r1: i.r0 - 1, ..r });
    }
    if i.r1 < r.r1 {
        v.push(Rect { r0: i.r1 + 1, ..r });
    }
    if i.c0 > r.c0 {
        v.push(Rect { r0: i.r0, r1: i.r1, c1: i.c0 - 1, ..r });
    }
    if i.c1 < r.c1 {
        v.push(Rect { r0: i.r0, r1: i.r1, c0: i.c1 + 1, ..r });
    }
    v
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
        // Where a cell inside `from` goes.
        let to = |r: u32, c: u32| -> (u32, u32) {
            let (r0, c0) = ((self.from.r0 as i64 + self.dr) as u32, (self.from.c0 as i64 + self.dc) as u32);
            if self.transpose { (r0 + (c - self.from.c0), c0 + (r - self.from.r0)) } else { (r0 + (r - self.from.r0), c0 + (c - self.from.c0)) }
        };
        let dead = |t: u16, r: u32, c: u32| self.dead.is_some_and(|d| d.sheet == t && d.contains(CellAddr::new(t, r, c))) && !inside(t, r, c);
        let cell = |r: &CellRef| -> Option<CellRef> {
            let t = r.sheet.unwrap_or(own);
            let (Some(row), Some(col)) = (r.row.resolve(or, MAX_ROWS), r.col.resolve(oc, MAX_COLS)) else { return Some(*r) };
            if dead(t, row, col) {
                return None;
            }
            if self.keep_outside && !inside(t, row, col) {
                return Some(*r);
            }
            let (row, col, t) = if inside(t, row, col) { let (a, b) = to(row, col); (a, b, self.sheet) } else { (row, col, t) };
            Some(CellRef { sheet: name(r.sheet, t), row: enc(r.row, row, nr), col: enc(r.col, col, nc) })
        };
        let range = |r: &RangeRef| -> Option<RangeRef> {
            let t = r.sheet.unwrap_or(own);
            let ends = (r.r0.resolve(or, MAX_ROWS), r.c0.resolve(oc, MAX_COLS), r.r1.resolve(or, MAX_ROWS), r.c1.resolve(oc, MAX_COLS));
            let (Some(r0), Some(c0), Some(r1), Some(c1)) = ends else { return Some(*r) };
            let whole = r.kind == RangeKind::Cells && inside(t, r0.min(r1), c0.min(c1)) && inside(t, r0.max(r1), c0.max(c1));
            // Wholly written over, none of it moving: gone.
            if let Some(d) = self.dead.filter(|d| r.kind == RangeKind::Cells && d.sheet == t) {
                let span = Rect { sheet: t, r0: r0.min(r1), c0: c0.min(c1), r1: r0.max(r1), c1: c0.max(c1) };
                let held = d.contains(CellAddr::new(t, span.r0, span.c0)) && d.contains(CellAddr::new(t, span.r1, span.c1));
                if held && !(t == self.from.sheet && span.intersects(&self.from)) {
                    return None;
                }
            }
            if self.keep_outside && !whole {
                return Some(*r);
            }
            let (r0, c0, r1, c1, t) = if whole {
                let ((a0, b0), (a1, b1)) = (to(r0, c0), to(r1, c1));
                (a0, b0, a1, b1, self.sheet)
            } else {
                (r0, c0, r1, c1, t)
            };
            Some(RangeRef { sheet: name(r.sheet, t), r0: enc(r.r0, r0, nr), c0: enc(r.c0, c0, nc), r1: enc(r.r1, r1, nr), c1: enc(r.c1, c1, nc), kind: r.kind })
        };
        self.walk(e, &cell, &range)
    }

    fn walk(&self, e: &Expr, cell: &dyn Fn(&CellRef) -> Option<CellRef>, range: &dyn Fn(&RangeRef) -> Option<RangeRef>) -> Expr {
        let b = |x: &Expr| Box::new(self.walk(x, cell, range));
        let all = |xs: &[Expr]| xs.iter().map(|a| self.walk(a, cell, range)).collect::<Vec<_>>();
        match e {
            Expr::Ref(r) => cell(r).map_or(Expr::Err(crate::value::ErrorKind::Ref), Expr::Ref),
            Expr::Spill(r) => cell(r).map_or(Expr::Err(crate::value::ErrorKind::Ref), Expr::Spill),
            Expr::Range(r) => range(r).map_or(Expr::Err(crate::value::ErrorKind::Ref), Expr::Range),
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
        // Transposed: 2 rows × 2 columns turned; B1 lands in A12, reading A1 where it went.
        let (ops, to) = clip.paste_as(&wb, 0, 10, 0, What::All, true).unwrap();
        assert_eq!(to, rect(10, 0, 11, 1));
        run(&mut wb, ops);
        assert_eq!((shown(&wb, 0, 10, 0), shown(&wb, 0, 10, 1), shown(&wb, 0, 11, 0)), ("2".into(), "x".into(), "=A11*10".into()));
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
        // Formulas reading cells the move writes over: #REF!, as in Excel
        // (a range only partly over stays).
        set(&mut wb, 6, 2, "9");
        set(&mut wb, 3, 4, "=C7+1");
        set(&mut wb, 4, 4, "=SUM(C6:C7)");
        set(&mut wb, 5, 4, "=SUM(C7:C12)");
        let (ops, _) = clip.paste(&wb, 0, 5, 2).unwrap();
        let undo2 = run(&mut wb, ops);
        assert_eq!(shown(&wb, 0, 3, 4), "=#REF!+1");
        assert_eq!(shown(&wb, 0, 4, 4), "=SUM(#REF!)");
        assert_eq!(shown(&wb, 0, 5, 4), "=SUM(C7:C12)");
        undo2.apply(&mut wb);
        assert_eq!(shown(&wb, 0, 3, 4), "=C7+1");
        for r in 3..6 {
            set(&mut wb, r, 4, "");
        }
        set(&mut wb, 6, 2, "");
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
    fn a_transposed_formula_reads_the_turned_cells() {
        let mut wb = wb();
        // A1:C1 = 1, 2, =A1+B1*$E$9+F1.
        set(&mut wb, 0, 0, "1");
        set(&mut wb, 0, 1, "2");
        set(&mut wb, 0, 2, "=A1+B1*$E$9+F1+SUM(A1:B1)");
        let clip = Clip::take(&wb, rect(0, 0, 0, 2), false);
        let (ops, to) = clip.paste_as(&wb, 0, 4, 0, What::All, true).unwrap();
        assert_eq!(to, rect(4, 0, 6, 0));
        run(&mut wb, ops);
        // A5:A7: the cells it read turned with it; $E$9 stays; F1 (outside) moves as a copy.
        assert_eq!(shown(&wb, 0, 6, 0), "=A5+A6*$E$9+D7+SUM(A5:A6)");
    }

    #[test]
    fn paste_special_adds_multiplies_and_skips_blanks() {
        let mut wb = wb();
        // Copied: A1:A3 = 10, (blank), =A1/2.
        set(&mut wb, 0, 0, "10");
        set(&mut wb, 2, 0, "=A1/2");
        // Under it at C1:C3: 1, 7, text; C4 untouched.
        set(&mut wb, 0, 2, "1");
        set(&mut wb, 1, 2, "7");
        set(&mut wb, 2, 2, "x");
        wb.recalc();
        let clip = Clip::take(&wb, rect(0, 0, 2, 0), false);
        let sp = |arith| Special { arith, ..Special::of(What::Values) };
        let (ops, _) = clip.paste_special(&wb, 0, 0, 2, sp(Some(Arith::Add))).unwrap();
        let undo = run(&mut wb, ops);
        assert_eq!((shown(&wb, 0, 0, 2), shown(&wb, 0, 1, 2), shown(&wb, 0, 2, 2)), ("11".into(), "7".into(), "x".into()));
        undo.apply(&mut wb);
        // Onto a formula: it gains the operation. Onto blank: 0 times.
        set(&mut wb, 0, 3, "=C2*2");
        let (ops, _) = clip.paste_special(&wb, 0, 0, 3, sp(Some(Arith::Mul))).unwrap();
        run(&mut wb, ops);
        assert_eq!((shown(&wb, 0, 0, 3), shown(&wb, 0, 2, 3)), ("=(C2*2)*10".into(), "0".into()));
        // Formulas pasted with arithmetic combine as formulas, moved as a
        // copy moves them: onto 5, onto a formula, onto blank.
        let one = Clip::take(&wb, rect(2, 0, 2, 0), false);
        let fx = |arith| Special { arith, ..Special::of(What::Formulas) };
        set(&mut wb, 2, 5, "5");
        set(&mut wb, 2, 6, "=F3*2");
        for (c, want) in [(5, "=5+(F1/2)"), (6, "=(F3*2)+(G1/2)"), (7, "=0+(H1/2)")] {
            let (ops, _) = one.paste_special(&wb, 0, 2, c, fx(Some(Arith::Add))).unwrap();
            run(&mut wb, ops);
            assert_eq!(shown(&wb, 0, 2, c), want);
        }
        // Divided by zero: #DIV/0!.
        set(&mut wb, 0, 4, "0");
        let clip0 = Clip::take(&wb, rect(0, 4, 0, 4), false);
        let (ops, _) = clip0.paste_special(&wb, 0, 0, 2, sp(Some(Arith::Div))).unwrap();
        run(&mut wb, ops);
        assert_eq!(shown(&wb, 0, 0, 2), "#DIV/0!");
        // Skip blanks: B2 under the blank keeps its 5.
        set(&mut wb, 1, 1, "5");
        let (ops, _) = clip.paste_special(&wb, 0, 0, 1, Special { skip_blanks: true, ..Special::of(What::All) }).unwrap();
        run(&mut wb, ops);
        assert_eq!((shown(&wb, 0, 0, 1), shown(&wb, 0, 1, 1), shown(&wb, 0, 2, 1)), ("10".into(), "5".into(), "=B1/2".into()));
        // A cut does neither.
        let cut = Clip::take(&wb, rect(0, 0, 0, 0), true);
        assert!(cut.paste_special(&wb, 0, 9, 9, sp(Some(Arith::Add))).is_err());
    }

    #[test]
    fn a_move_brings_comments_links_names_and_rules() {
        use crate::condfmt::CmpOp;
        use crate::validation::{Allow, Validation};
        let mut wb = wb();
        set(&mut wb, 0, 0, "1");
        set(&mut wb, 1, 0, "2");
        wb.set_comments(0, vec![(0, 0, Some(crate::comments::Comment::new("me", "look")))]);
        wb.set_links(0, vec![(1, 0, crate::links::Link::new("https://example.org"))]);
        wb.define_name("Two", None, "=Sheet1!$A$1:$A$2").unwrap();
        let f = wb.validation_formula("A1>0", 0, (0, 0)).unwrap();
        wb.add_validation(Validation::new(0, vec![rect(0, 0, 1, 0)], Allow::Custom, CmpOp::Between, Some(f), None)).unwrap();
        let ch = crate::charts::Chart {
            kind: crate::charts::ChartKind::Column,
            grouping: crate::charts::Grouping::Standard,
            title: None,
            series: vec![crate::charts::Series { name: None, cats: None, vals: rect(0, 0, 1, 0), kind: None }],
            legend: None,
            x_title: None,
            y_title: None,
            anchor: crate::charts::Anchor { sheet: 0, from: (5, 5, 0.0, 0.0), to: (15, 9, 0.0, 0.0) },
        };
        GridOp::Charts(vec![ch]).apply(&mut wb);
        // A copy brings the comment and link, and leaves the rest.
        let copy = Clip::take(&wb, rect(0, 0, 1, 0), false);
        let (ops, _) = copy.paste(&wb, 0, 0, 3).unwrap();
        run(&mut wb, ops);
        assert_eq!(wb.comment(0, 0, 3).map(|k| k.text.to_string()).as_deref(), Some("look"));
        assert!(wb.link(0, 1, 3).is_some() && wb.comment(0, 0, 0).is_some());
        // Paste Comments: only them.
        let (ops, _) = copy.paste_as(&wb, 0, 0, 7, What::Comments, false).unwrap();
        run(&mut wb, ops);
        assert!(wb.comment(0, 0, 7).is_some() && wb.link(0, 1, 7).is_none() && shown(&wb, 0, 0, 7).is_empty());
        // A cut moves all of them to C6.
        let cut = Clip::take(&wb, rect(0, 0, 1, 0), true);
        let (ops, _) = cut.paste(&wb, 0, 5, 2).unwrap();
        let undo = run(&mut wb, ops);
        assert!(wb.comment(0, 0, 0).is_none() && wb.link(0, 1, 0).is_none());
        assert!(wb.comment(0, 5, 2).is_some() && wb.link(0, 6, 2).is_some());
        let name = wb.names().iter().find(|d| &*d.name == "Two").cloned().unwrap();
        assert_eq!(wb.name_text(&name), "=Sheet1!$C$6:$C$7");
        assert_eq!(wb.validations()[0].ranges, vec![rect(5, 2, 6, 2)]);
        assert_eq!(wb.charts()[0].series[0].vals, rect(5, 2, 6, 2));
        let moved = wb.validations()[0].clone();
        let back = crate::formula::to_a1(moved.f1.as_ref().unwrap(), 5, 2, &|id| wb.sheet_name(id));
        assert_eq!(back, "=C6>0");
        undo.apply(&mut wb);
        assert!(wb.comment(0, 0, 0).is_some() && wb.comment(0, 5, 2).is_none());
        assert_eq!(wb.validations()[0].ranges, vec![rect(0, 0, 1, 0)]);
    }

    #[test]
    fn a_cut_of_part_of_a_rule_splits_it() {
        use crate::condfmt::CmpOp;
        use crate::validation::{Allow, Validation};
        let mut wb = wb();
        for r in 0..6 {
            set(&mut wb, r, 0, &r.to_string());
        }
        // A1:A6 must be above B's cell beside it; A3:A4 move to D10.
        let f = wb.validation_formula("A1>B1", 0, (0, 0)).unwrap();
        wb.add_validation(Validation::new(0, vec![rect(0, 0, 5, 0)], Allow::Custom, CmpOp::Between, Some(f), None)).unwrap();
        let cut = Clip::take(&wb, rect(2, 0, 3, 0), true);
        let (ops, _) = cut.paste(&wb, 0, 9, 3).unwrap();
        let undo = run(&mut wb, ops);
        let vs = wb.validations().to_vec();
        assert_eq!(vs.len(), 2);
        assert_eq!(vs[0].ranges, vec![rect(0, 0, 1, 0), rect(4, 0, 5, 0)]);
        assert_eq!(vs[1].ranges, vec![rect(9, 3, 10, 3)]);
        let at = |v: &Validation, r, c| crate::formula::to_a1(v.f1.as_ref().unwrap(), r, c, &|id| wb.sheet_name(id));
        // Left behind: unchanged. Moved: reads its own cell, and B3 where it was.
        assert_eq!(at(&vs[0], 0, 0), "=A1>B1");
        assert_eq!(at(&vs[1], 9, 3), "=D10>B3");
        assert_eq!(at(&vs[1], 10, 3), "=D11>B4");
        // Undo: one rule again.
        undo.apply(&mut wb);
        assert_eq!(wb.validations().len(), 1);
        assert_eq!(wb.validations()[0].ranges, vec![rect(0, 0, 5, 0)]);
    }

    #[test]
    fn pasting_into_a_bigger_area_repeats_the_copy() {
        let mut wb = wb();
        set(&mut wb, 0, 0, "1");
        set(&mut wb, 0, 1, "=A1*2");
        let clip = Clip::take(&wb, rect(0, 0, 0, 1), false);
        // Three rows by four columns: two copies across, three down.
        let (ops, to) = clip.paste_into(&wb, rect(5, 0, 7, 3)).unwrap();
        assert_eq!(to, rect(5, 0, 7, 3));
        run(&mut wb, ops);
        assert_eq!((shown(&wb, 0, 7, 2), shown(&wb, 0, 7, 3)), ("1".into(), "=C8*2".into()));
        // Not a whole number of copies: once.
        let (_, to) = clip.paste_into(&wb, rect(10, 0, 10, 2)).unwrap();
        assert_eq!(to, rect(10, 0, 10, 1));
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
