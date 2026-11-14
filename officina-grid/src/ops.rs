//! Undoable grid edits for `officina_core::history`.
//!
//! Every edit is "replace this rectangle's contents with a snapshot". Applying one
//! returns the snapshot of what it replaced, which is its inverse, so undo and redo are
//! the same operation. A snapshot keeps formulas as fills (one per group piece), not as
//! cells, so undoing an edit inside a million-row fill costs one fill, not a million.

use crate::addr::{CellAddr, Rect, MAX_COLS, MAX_ROWS};
use crate::book::{Input, Workbook};
use crate::column::Cell;
use crate::style::Style;
use crate::formula;
use crate::value::{ErrorKind, Value};
use officina_core::history::Op;
use officina_core::wire::{Reader, WireError, Writer};

/// A formula filled down rows `r0..=r1` of one column; `src` is written for row `r0`.
#[derive(Debug, Clone, PartialEq)]
pub struct Fill {
    pub col: u32,
    pub r0: u32,
    pub r1: u32,
    pub src: String,
}

/// The contents of a rectangle: constants by cell, formulas by fill. Cells of the
/// rectangle not named are empty.
#[derive(Debug, Clone, PartialEq)]
pub struct Region {
    pub rect: Rect,
    pub values: Vec<(u32, u32, Value)>,
    pub fills: Vec<Fill>,
}

/// The styles of a rectangle: spans of one column with a style other than the
/// default, by value (ids are per workbook). Cells not covered have the default.
#[derive(Debug, Clone, PartialEq)]
pub struct StyleRegion {
    pub rect: Rect,
    pub spans: Vec<(u32, u32, u32, Style)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GridOp {
    Replace(Region),
    Restyle(StyleRegion),
    /// A sheet's filter and hidden rows, swapped in whole.
    Filter(crate::filter::FilterState),
    /// Several edits as one (a sort moves values and styles together). Not nested.
    Batch(Vec<GridOp>),
    /// The workbook's structured tables, swapped in whole (tables that don't fit
    /// are left out).
    Tables(Vec<crate::tables::Table>),
    /// The workbook's charts, swapped in whole.
    Charts(Vec<crate::charts::Chart>),
}

impl GridOp {
    /// Sets one cell.
    pub fn set(a: CellAddr, input: Input) -> Self {
        Self::block(a.sheet, a.row, a.col, vec![vec![input]])
    }

    /// Sets a block of cells from `rows[r][c]` at (`r0`, `c0`), as a paste does.
    pub fn block(sheet: u16, r0: u32, c0: u32, rows: Vec<Vec<Input>>) -> Self {
        let h = rows.len().max(1) as u32;
        let w = rows.iter().map(Vec::len).max().unwrap_or(1).max(1) as u32;
        let mut values = Vec::new();
        let mut fills = Vec::new();
        for (i, row) in rows.into_iter().enumerate() {
            for (j, input) in row.into_iter().enumerate() {
                let (r, c) = (r0 + i as u32, c0 + j as u32);
                match input {
                    Input::Empty | Input::Value(Value::Empty) => {}
                    Input::Value(v) => values.push((r, c, v)),
                    Input::Formula(src) => fills.push(Fill { col: c, r0: r, r1: r, src }),
                }
            }
        }
        GridOp::Replace(Region { rect: Rect { sheet, r0, c0, r1: r0 + h - 1, c1: c0 + w - 1 }, values, fills })
    }

    /// Fills `src` (written for row `r0`) down `r0..=r1` of a column.
    pub fn fill(sheet: u16, col: u32, r0: u32, r1: u32, src: &str) -> Self {
        GridOp::Replace(Region {
            rect: Rect { sheet, r0, c0: col, r1, c1: col },
            values: Vec::new(),
            fills: vec![Fill { col, r0, r1, src: src.to_string() }],
        })
    }

    /// Empties a rectangle.
    pub fn clear(rect: Rect) -> Self {
        GridOp::Replace(Region { rect, values: Vec::new(), fills: Vec::new() })
    }

    /// Sets `style` on every cell of `rect`.
    pub fn style(rect: Rect, style: Style) -> Self {
        let spans = if style == Style::default() { Vec::new() } else { (rect.c0..=rect.c1).map(|c| (c, rect.r0, rect.r1, style.clone())).collect() };
        GridOp::Restyle(StyleRegion { rect, spans })
    }

    /// Changes the style of every cell of `rect` with `f` (bold on, a new number
    /// format, ...), keeping the rest of each cell's style.
    pub fn restyle(wb: &Workbook, rect: Rect, f: impl Fn(&Style) -> Style) -> Self {
        let mut spans = Vec::new();
        for c in rect.c0..=rect.c1 {
            let runs = wb.sheets.get(rect.sheet as usize).and_then(|s| s.styles.get(c as usize));
            let parts = match runs {
                Some(r) => r.spans(rect.r0, rect.r1),
                None => vec![(rect.r0, rect.r1, 0)],
            };
            for (a, b, id) in parts {
                let st = f(wb.styles.get(id));
                if st != Style::default() {
                    spans.push((c, a, b, st));
                }
            }
        }
        GridOp::Restyle(StyleRegion { rect, spans })
    }

    /// The op as journal bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        let r = match self {
            GridOp::Filter(f) => {
                w.u8(4);
                f.encode(&mut w);
                return w.into_bytes();
            }
            GridOp::Tables(ts) => {
                w.u8(5);
                w.var(ts.len() as u64);
                for t in ts {
                    t.encode(&mut w);
                }
                return w.into_bytes();
            }
            GridOp::Charts(cs) => {
                w.u8(6);
                w.var(cs.len() as u64);
                for c in cs {
                    c.encode(&mut w);
                }
                return w.into_bytes();
            }
            GridOp::Batch(ops) => {
                w.u8(3);
                w.var(ops.len() as u64);
                for op in ops {
                    w.bytes(&op.encode());
                }
                return w.into_bytes();
            }
            GridOp::Replace(r) => r,
            GridOp::Restyle(s) => {
                w.u8(2);
                w.u16(s.rect.sheet);
                for x in [s.rect.r0, s.rect.c0, s.rect.r1, s.rect.c1] {
                    w.var(x as u64);
                }
                w.var(s.spans.len() as u64);
                for (c, a, b, st) in &s.spans {
                    w.var(*c as u64);
                    w.var(*a as u64);
                    w.var(*b as u64);
                    st.encode(&mut w);
                }
                return w.into_bytes();
            }
        };
        w.u8(1);
        w.u16(r.rect.sheet);
        w.var(r.rect.r0 as u64);
        w.var(r.rect.c0 as u64);
        w.var(r.rect.r1 as u64);
        w.var(r.rect.c1 as u64);
        w.var(r.values.len() as u64);
        for (row, col, v) in &r.values {
            w.var(*row as u64);
            w.var(*col as u64);
            write_value(&mut w, v);
        }
        w.var(r.fills.len() as u64);
        for f in &r.fills {
            w.var(f.col as u64);
            w.var(f.r0 as u64);
            w.var(f.r1 as u64);
            w.str(&f.src);
        }
        w.into_bytes()
    }

    /// Reads an op written by [`encode`](Self::encode). Bounds everything; never panics.
    pub fn decode(b: &[u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(b);
        let kind = r.u8()?;
        if kind == 5 || kind == 6 {
            let op = if kind == 5 {
                let n = r.count("tables", crate::tables::MAX_TABLES as u64, 8)?;
                let mut v = Vec::with_capacity(n);
                for _ in 0..n {
                    v.push(crate::tables::Table::decode(&mut r)?);
                }
                GridOp::Tables(v)
            } else {
                let n = r.count("charts", crate::charts::MAX_CHARTS as u64, 30)?;
                let mut v = Vec::with_capacity(n);
                for _ in 0..n {
                    v.push(crate::charts::Chart::decode(&mut r)?);
                }
                GridOp::Charts(v)
            };
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(op);
        }
        if kind == 4 {
            let f = crate::filter::FilterState::decode(&mut r)?;
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(GridOp::Filter(f));
        }
        if kind == 3 {
            let n = r.count("ops", 1 << 16, 1)?;
            let mut ops = Vec::with_capacity(n);
            for _ in 0..n {
                let b = r.bytes("op", b.len() as u64)?;
                if b.first() == Some(&3) {
                    return Err(WireError::Invalid("nested batch"));
                }
                ops.push(GridOp::decode(b)?);
            }
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(GridOp::Batch(ops));
        }
        if kind != 1 && kind != 2 {
            return Err(WireError::Invalid("op kind"));
        }
        let sheet = r.u16()?;
        let row = |r: &mut Reader| -> Result<u32, WireError> { r.len("row", MAX_ROWS as u64 - 1).map(|x| x as u32) };
        let col = |r: &mut Reader| -> Result<u32, WireError> { r.len("column", MAX_COLS as u64 - 1).map(|x| x as u32) };
        let (r0, c0, r1, c1) = (row(&mut r)?, col(&mut r)?, row(&mut r)?, col(&mut r)?);
        if r0 > r1 || c0 > c1 {
            return Err(WireError::Invalid("rectangle"));
        }
        let rect = Rect { sheet, r0, c0, r1, c1 };
        if kind == 2 {
            let n = r.count("style spans", u32::MAX as u64, 8)?;
            let mut spans = Vec::with_capacity(n);
            for _ in 0..n {
                let (x, a, b) = (col(&mut r)?, row(&mut r)?, row(&mut r)?);
                if a > b || x < c0 || x > c1 || a < r0 || b > r1 {
                    return Err(WireError::Invalid("style span outside rectangle"));
                }
                spans.push((x, a, b, Style::decode(&mut r)?));
            }
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(GridOp::Restyle(StyleRegion { rect, spans }));
        }
        let n = r.count("values", u32::MAX as u64, 3)?;
        let mut values = Vec::with_capacity(n);
        for _ in 0..n {
            let (y, x) = (row(&mut r)?, col(&mut r)?);
            if !rect.contains(CellAddr::new(sheet, y, x)) {
                return Err(WireError::Invalid("value outside rectangle"));
            }
            values.push((y, x, read_value(&mut r)?));
        }
        let n = r.count("fills", u32::MAX as u64, 4)?;
        let mut fills = Vec::with_capacity(n);
        for _ in 0..n {
            let (x, a, b) = (col(&mut r)?, row(&mut r)?, row(&mut r)?);
            if a > b || x < c0 || x > c1 || a < r0 || b > r1 {
                return Err(WireError::Invalid("fill outside rectangle"));
            }
            let src = r.str("formula", formula::MAX_STORED_FORMULA_LEN as u64)?.to_string();
            fills.push(Fill { col: x, r0: a, r1: b, src });
        }
        if !r.is_empty() {
            return Err(WireError::Invalid("trailing bytes"));
        }
        Ok(GridOp::Replace(Region { rect, values, fills }))
    }
}

impl Op<Workbook> for GridOp {
    fn apply(self, wb: &mut Workbook) -> Self {
        match self {
            GridOp::Replace(region) => GridOp::Replace(wb.replace(region)),
            GridOp::Restyle(region) => GridOp::Restyle(wb.replace_styles(region)),
            GridOp::Filter(f) => GridOp::Filter(wb.replace_filter(f)),
            GridOp::Tables(ts) => GridOp::Tables(wb.replace_tables(ts)),
            GridOp::Charts(cs) => GridOp::Charts(wb.replace_charts(cs)),
            GridOp::Batch(ops) => {
                // The inverse undoes them in reverse order.
                let mut inv: Vec<GridOp> = ops.into_iter().map(|op| op.apply(wb)).collect();
                inv.reverse();
                GridOp::Batch(inv)
            }
        }
    }
}

impl Workbook {
    /// The styles of `rect` now, as a [`StyleRegion`].
    pub fn style_snapshot(&self, rect: &Rect) -> StyleRegion {
        let mut spans = Vec::new();
        if let Some(sh) = self.sheets.get(rect.sheet as usize) {
            let last = rect.c1.min(sh.styles.len() as u32);
            for c in rect.c0..=last {
                let Some(runs) = sh.styles.get(c as usize) else { continue };
                for (a, b, id) in runs.spans(rect.r0, rect.r1) {
                    if id != 0 {
                        spans.push((c, a, b, self.styles.get(id).clone()));
                    }
                }
            }
        }
        StyleRegion { rect: *rect, spans }
    }

    /// Replaces the styles of `region.rect`; returns what they were.
    pub fn replace_styles(&mut self, region: StyleRegion) -> StyleRegion {
        let old = self.style_snapshot(&region.rect);
        self.set_style_id(&region.rect, 0);
        for (c, a, b, st) in region.spans {
            let id = self.styles.intern(st);
            self.set_style_id(&Rect { sheet: region.rect.sheet, r0: a, c0: c, r1: b, c1: c }, id);
        }
        old
    }

    /// What `rect` holds now, as a [`Region`].
    pub fn snapshot(&self, rect: &Rect) -> Region {
        let mut values = Vec::new();
        let mut fills = Vec::new();
        let Some(sheet) = self.sheets.get(rect.sheet as usize) else {
            return Region { rect: *rect, values, fills };
        };
        let last = rect.c1.min(sheet.cols.len().max(sheet.fidx.len()) as u32);
        for c in rect.c0..=last {
            // Fills first: they decide which stored cells are formula results.
            let start = fills.len();
            if let Some(f) = sheet.fidx.get(c as usize) {
                let mut hit: Vec<u32> = f.range(..=rect.r1).rev().take_while(|(_, &g)| self.group(g).r1 >= rect.r0).map(|(_, &g)| g).collect();
                hit.reverse();
                for g in hit {
                    let grp = self.group(g);
                    let (a, b) = (grp.r0.max(rect.r0), grp.r1.min(rect.r1));
                    let src = formula::to_a1(&grp.expr, a, c, &|s| self.sheet_name(s));
                    fills.push(Fill { col: c, r0: a, r1: b, src });
                }
            }
            let Some(column) = sheet.cols.get(c as usize) else { continue };
            let mine = &fills[start..];
            let mut k = 0;
            column.each(rect.r0, rect.r1, &mut |r, cell| {
                while k < mine.len() && mine[k].r1 < r {
                    k += 1;
                }
                if k < mine.len() && mine[k].r0 <= r {
                    return;
                }
                values.push((r, c, self.cell_value(cell)));
            });
        }
        Region { rect: *rect, values, fills }
    }

    /// Replaces `region.rect` with `region`; returns what was there.
    pub fn replace(&mut self, region: Region) -> Region {
        let rect = region.rect;
        let old = self.snapshot(&rect);
        if (rect.sheet as usize) >= self.sheets.len() {
            return old;
        }
        // Empty the rectangle.
        let last = rect.c1.min(self.sheets[rect.sheet as usize].cols.len().max(self.sheets[rect.sheet as usize].fidx.len()) as u32);
        for c in rect.c0..=last {
            self.clear_formulas_in(rect.sheet, c, rect.r0, rect.r1);
            let mut rows = Vec::new();
            if let Some(col) = self.sheets[rect.sheet as usize].cols.get(c as usize) {
                col.each(rect.r0, rect.r1, &mut |r, _| rows.push(r));
            }
            for r in rows {
                self.store_cell(rect.sheet, r, c, Cell::Empty);
            }
        }
        self.changed.push(rect);
        for (r, c, v) in &region.values {
            self.store(rect.sheet, *r, *c, v);
        }
        for f in &region.fills {
            if self.fill_formula(rect.sheet, f.col, f.r0, f.r1, &f.src).is_err() {
                // The text no longer parses (a sheet it named is gone): keep the cells
                // visibly broken rather than silently empty.
                for r in f.r0..=f.r1.min(f.r0 + 65_535) {
                    self.store(rect.sheet, r, f.col, &Value::Err(ErrorKind::Ref));
                }
            }
        }
        old
    }
}

// ---------- values on the wire (shared with the native format) ----------

pub(crate) fn write_value(w: &mut Writer, v: &Value) {
    match v {
        Value::Empty => w.u8(0),
        Value::Num(x) => {
            w.u8(1);
            w.f64(*x);
        }
        Value::Str(s) => {
            w.u8(2);
            w.str(s);
        }
        Value::Bool(b) => w.u8(if *b { 4 } else { 3 }),
        Value::Err(e) => {
            w.u8(5);
            w.u8(*e as u8);
        }
    }
}

pub(crate) fn read_value(r: &mut Reader) -> Result<Value, WireError> {
    Ok(match r.u8()? {
        0 => Value::Empty,
        1 => Value::num(r.f64()?),
        2 => Value::str(r.str("text", crate::strings::MAX_TEXT as u64)?),
        3 => Value::Bool(false),
        4 => Value::Bool(true),
        5 => Value::Err(ErrorKind::from_u8(r.u8()?).ok_or(WireError::Invalid("error value"))?),
        _ => return Err(WireError::Invalid("value tag")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use officina_core::history::History;

    fn a(s: &str) -> CellAddr {
        let (r, c) = crate::addr::parse_a1(s).unwrap();
        CellAddr::new(0, r, c)
    }

    #[test]
    fn styles_undo_and_encode() {
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        let mut h: History<GridOp> = History::new();
        wb.set_value(a("B2"), Value::Num(0.256));
        let all = Rect { sheet: 0, r0: 0, c0: 0, r1: 1_048_575, c1: 3 };
        let op = GridOp::restyle(&wb, all, |s| s.clone().with_num_fmt("0.0%"));
        h.run(&mut wb, "Percent", vec![op]);
        assert_eq!(wb.formatted(a("B2")).text, "25.6%");
        let bold = Rect { sheet: 0, r0: 1, c0: 1, r1: 2, c1: 1 };
        let op = GridOp::restyle(&wb, bold, |s| {
            let mut s = s.clone();
            s.font.bold = true;
            s
        });
        assert_eq!(GridOp::decode(&op.encode()).unwrap(), op);
        h.run(&mut wb, "Bold", vec![op]);
        assert!(wb.style(a("B2")).font.bold && wb.style(a("B3")).font.bold && !wb.style(a("B4")).font.bold);
        assert_eq!(&*wb.style(a("B2")).num_fmt, "0.0%");
        // Whole columns stay a few runs.
        assert_eq!(wb.sheet(0).styles[1].runs().len(), 4, "{:?}", wb.sheet(0).styles[1].runs());
        h.undo(&mut wb);
        assert!(!wb.style(a("B2")).font.bold);
        assert_eq!(wb.formatted(a("B2")).text, "25.6%");
        h.undo(&mut wb);
        assert_eq!(wb.formatted(a("B2")).text, "0.256");
        assert!(wb.sheet(0).styles.is_empty());
        h.redo(&mut wb);
        h.redo(&mut wb);
        assert!(wb.style(a("B3")).font.bold);
        assert!(GridOp::decode(&[2, 0, 0]).is_err());
    }

    #[test]
    fn undo_redo_inside_a_fill() {
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        let mut h: History<GridOp> = History::new();
        let n = 10_000u32;
        let vals: Vec<f64> = (0..n).map(|i| i as f64).collect();
        wb.set_numbers(0, 0, 0, &vals);
        h.run(&mut wb, "Fill", vec![GridOp::fill(0, 1, 0, n - 1, "=A1*2")]);
        wb.recalc();
        assert_eq!(wb.groups().count(), 1);
        assert_eq!(wb.get(a("B5000")), Value::Num(9998.0));

        h.run(&mut wb, "Typing", vec![GridOp::set(a("B5000"), Input::Value(Value::str("x")))]);
        wb.recalc();
        assert_eq!(wb.groups().count(), 2);
        assert_eq!(wb.get(a("B5000")), Value::str("x"));

        assert_eq!(h.undo(&mut wb).as_deref(), Some("Typing"));
        wb.recalc();
        assert_eq!(wb.get(a("B5000")), Value::Num(9998.0));
        assert_eq!(wb.groups().count(), 1, "the fill joins up again");
        assert_eq!(wb.formula_text(a("B5000")).as_deref(), Some("=A5000*2"));

        h.redo(&mut wb);
        wb.recalc();
        assert_eq!(wb.get(a("B5000")), Value::str("x"));

        h.undo(&mut wb);
        h.undo(&mut wb);
        wb.recalc();
        assert_eq!(wb.groups().count(), 0);
        assert_eq!(wb.get(a("B5000")), Value::Empty);
        assert_eq!(wb.get(a("A5000")), Value::Num(4999.0));
    }

    #[test]
    fn tables_and_charts_undo_and_encode() {
        use crate::charts::*;
        use crate::tables::Table;
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        for r in 0..5 {
            wb.set_input(CellAddr::new(0, r, 0), &format!("{r}")).unwrap();
        }
        wb.set_input(a("A1"), "Qty").unwrap();
        wb.set_input(a("C1"), "=SUM(T[Qty])").unwrap();
        wb.recalc();
        assert_eq!(wb.get(a("C1")), Value::Err(ErrorKind::Ref));
        let t = Table { name: "T".into(), rect: Rect { sheet: 0, r0: 0, c0: 0, r1: 4, c1: 0 }, header: true, totals: false, columns: vec!["Qty".into()], style: None, stripes: true };
        let ch = Chart {
            kind: ChartKind::Column,
            grouping: Grouping::Standard,
            title: None,
            series: vec![Series { name: None, cats: None, vals: Rect { sheet: 0, r0: 1, c0: 0, r1: 4, c1: 0 }, kind: None }],
            legend: None,
            x_title: None,
            y_title: None,
            anchor: Anchor { sheet: 0, from: (1, 3, 0.0, 0.0), to: (10, 8, 0.0, 0.0) },
        };
        let ops = vec![GridOp::Tables(vec![t.clone()]), GridOp::Charts(vec![ch.clone()])];
        for op in &ops {
            assert_eq!(&GridOp::decode(&op.encode()).unwrap(), op);
        }
        let mut h: History<GridOp> = History::new();
        h.run(&mut wb, "Format as Table", vec![ops[0].clone()]);
        h.run(&mut wb, "Insert Chart", vec![ops[1].clone()]);
        wb.recalc();
        assert_eq!(wb.get(a("C1")), Value::Num(10.0), "the formula found the new table");
        assert_eq!(wb.charts(), &[ch]);
        h.undo(&mut wb);
        assert!(wb.charts().is_empty());
        h.undo(&mut wb);
        wb.recalc();
        assert!(wb.tables().is_empty());
        assert_eq!(wb.get(a("C1")), Value::Err(ErrorKind::Ref));
        h.redo(&mut wb);
        wb.recalc();
        assert_eq!(wb.get(a("C1")), Value::Num(10.0));
        // A table that doesn't fit (a taken name) is left out, not applied half.
        let mut bad = t.clone();
        bad.name = "Qty Table".into();
        assert!(matches!(GridOp::Tables(vec![bad]).apply(&mut wb), GridOp::Tables(old) if old.len() == 1));
        assert!(wb.tables().is_empty());
    }

    #[test]
    fn block_paste_and_undo() {
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        wb.set_input(a("A1"), "old").unwrap();
        wb.set_input(a("C3"), "=1+1").unwrap();
        let before = wb.snapshot(&Rect { sheet: 0, r0: 0, c0: 0, r1: 9, c1: 9 });
        let mut h: History<GridOp> = History::new();
        let rows = vec![
            vec![Input::Value(Value::Num(1.0)), Input::Formula("=A1+1".into())],
            vec![Input::Empty, Input::Formula("=A2+1".into())],
        ];
        h.run(&mut wb, "Paste", vec![GridOp::block(0, 0, 0, rows)]);
        wb.recalc();
        assert_eq!(wb.get(a("B1")), Value::Num(2.0));
        assert_eq!(wb.get(a("B2")), Value::Num(1.0));
        assert_eq!(wb.groups().count(), 2, "B1:B2 is one fill, plus C3");
        h.undo(&mut wb);
        wb.recalc();
        assert_eq!(wb.snapshot(&Rect { sheet: 0, r0: 0, c0: 0, r1: 9, c1: 9 }), before);
    }

    #[test]
    fn journal_round_trip_and_garbage() {
        let op = GridOp::block(0, 3, 4, vec![vec![Input::Value(Value::str("é")), Input::Formula("=SUM(A1:A9)".into())], vec![Input::Value(Value::Err(ErrorKind::NA)), Input::Value(Value::Bool(true))]]);
        let b = op.encode();
        assert_eq!(GridOp::decode(&b).unwrap(), op);
        for i in 0..b.len() {
            let _ = GridOp::decode(&b[..i]);
            let mut c = b.clone();
            c[i] ^= 0x5a;
            let _ = GridOp::decode(&c);
        }
    }
}
