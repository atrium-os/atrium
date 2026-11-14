//! Structured tables (Excel's "tables", ListObjects): a named range with a header row,
//! named columns and an optional totals row, which formulas reach by name
//! (`Sales[Amount]`, `[@Qty]`, `Sales[[#Headers],[Region]]`).
//!
//! A structured reference is kept as written and resolved when evaluated, so a formula
//! follows its table when the table changes; its dependencies are those of the range it
//! resolves to (the formula's own row for `@`).

use std::sync::Arc;

use crate::addr::{Rect, MAX_COLS, MAX_ROWS};
use crate::book::Workbook;
use crate::formula::ast::{CellRef, Coord, Expr, RangeKind, RangeRef, TableArea, TableRef};
use crate::names::valid_name;
use officina_core::wire::{Reader, WireError, Writer};

/// Tables per workbook, at most.
pub const MAX_TABLES: usize = 4096;
/// Column-name length, as Excel.
pub const MAX_COL_NAME: usize = 255;

#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub name: Arc<str>,
    /// Header row (if any), data rows and totals row (if any).
    pub rect: Rect,
    pub header: bool,
    pub totals: bool,
    /// One name per column of `rect`, unique ignoring case.
    pub columns: Vec<Arc<str>>,
    /// The table style's name (`TableStyleMedium2`), kept for files.
    pub style: Option<Arc<str>>,
    /// Banded rows, kept for files.
    pub stripes: bool,
}

impl Table {
    /// The data rows, first and last (`None` for a table of only a header and totals).
    pub fn data_rows(&self) -> Option<(u32, u32)> {
        let a = self.rect.r0 + self.header as u32;
        let b = self.rect.r1.checked_sub(self.totals as u32)?;
        (a <= b).then_some((a, b))
    }

    /// The sheet column of the column named `name` (ignoring case).
    pub fn col(&self, name: &str) -> Option<u32> {
        let k = crate::eval::funcs::text_key(name);
        self.columns.iter().position(|c| crate::eval::funcs::text_key(c) == k).map(|i| self.rect.c0 + i as u32)
    }

    /// What `t` reaches in this table: a range on the table's sheet, its rows the
    /// formula's own for `@` (`None`: a column or area the table doesn't have).
    pub fn resolve(&self, t: &TableRef) -> Option<Expr> {
        let (c0, c1) = match &t.cols {
            None => (self.rect.c0, self.rect.c1),
            Some((a, b)) => {
                let (x, y) = (self.col(a)?, self.col(b)?);
                (x.min(y), x.max(y))
            }
        };
        let data = self.data_rows();
        let (r0, r1) = match t.area {
            TableArea::Data => data?,
            TableArea::All => (self.rect.r0, self.rect.r1),
            TableArea::Headers if self.header => (self.rect.r0, self.rect.r0),
            TableArea::Totals if self.totals => (self.rect.r1, self.rect.r1),
            TableArea::HeadersData => (self.rect.r0, data.map_or(self.rect.r0, |d| d.1)),
            TableArea::DataTotals => (data.map_or(self.rect.r1, |d| d.0), self.rect.r1),
            TableArea::ThisRow => {
                let sheet = Some(self.rect.sheet);
                return Some(if c0 == c1 {
                    Expr::Ref(CellRef { sheet, row: Coord::Rel(0), col: Coord::Abs(c0) })
                } else {
                    Expr::Range(RangeRef { sheet, r0: Coord::Rel(0), c0: Coord::Abs(c0), r1: Coord::Rel(0), c1: Coord::Abs(c1), kind: RangeKind::Cells })
                });
            }
            TableArea::Headers | TableArea::Totals => return None,
        };
        let sheet = Some(self.rect.sheet);
        Some(if r0 == r1 && c0 == c1 {
            Expr::Ref(CellRef { sheet, row: Coord::Abs(r0), col: Coord::Abs(c0) })
        } else {
            Expr::Range(RangeRef { sheet, r0: Coord::Abs(r0), c0: Coord::Abs(c0), r1: Coord::Abs(r1), c1: Coord::Abs(c1), kind: RangeKind::Cells })
        })
    }

    pub fn encode(&self, w: &mut Writer) {
        w.str(&self.name);
        w.u16(self.rect.sheet);
        w.var(self.rect.r0 as u64);
        w.var(self.rect.c0 as u64);
        w.var(self.rect.r1 as u64);
        w.var(self.rect.c1 as u64);
        w.u8(self.header as u8 | (self.totals as u8) << 1 | (self.stripes as u8) << 2);
        w.str(self.style.as_deref().unwrap_or(""));
        for c in &self.columns {
            w.str(c);
        }
    }

    /// Reads what [`Table::encode`] wrote; the caller checks it with
    /// [`Workbook::add_table`].
    pub fn decode(r: &mut Reader) -> Result<Table, WireError> {
        let name = Arc::from(r.str("table name", crate::names::MAX_NAME_LEN as u64)?);
        let sheet = r.u16()?;
        let mut v = [0u32; 4];
        for (x, max) in v.iter_mut().zip([MAX_ROWS, MAX_COLS, MAX_ROWS, MAX_COLS]) {
            let n = r.var()?;
            if n >= max as u64 {
                return Err(WireError::Invalid("table range"));
            }
            *x = n as u32;
        }
        let rect = Rect { sheet, r0: v[0], c0: v[1], r1: v[2], c1: v[3] };
        if rect.r0 > rect.r1 || rect.c0 > rect.c1 {
            return Err(WireError::Invalid("table range"));
        }
        let flags = r.u8()?;
        let style = r.str("table style", 255)?;
        let n = (rect.c1 - rect.c0) as usize + 1;
        if n > r.remaining() {
            return Err(WireError::Invalid("table columns"));
        }
        let mut columns = Vec::with_capacity(n);
        for _ in 0..n {
            columns.push(Arc::from(r.str("column name", MAX_COL_NAME as u64)?));
        }
        Ok(Table {
            name,
            rect,
            header: flags & 1 != 0,
            totals: flags & 2 != 0,
            stripes: flags & 4 != 0,
            style: (!style.is_empty()).then(|| Arc::from(style)),
            columns,
        })
    }
}

fn overlaps(a: &Rect, b: &Rect) -> bool {
    a.sheet == b.sheet && a.r0 <= b.r1 && b.r0 <= a.r1 && a.c0 <= b.c1 && b.c0 <= a.c1
}

impl Workbook {
    pub fn tables(&self) -> &[Table] {
        &self.tables
    }

    /// The table named `name` (ignoring case).
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.iter().find(|t| t.name.eq_ignore_ascii_case(name) || (!name.is_ascii() && t.name.to_uppercase() == name.to_uppercase()))
    }

    /// The table a cell is in.
    pub fn table_at(&self, sheet: u16, row: u32, col: u32) -> Option<&Table> {
        self.tables.iter().find(|t| t.rect.sheet == sheet && (t.rect.r0..=t.rect.r1).contains(&row) && (t.rect.c0..=t.rect.c1).contains(&col))
    }

    /// Adds a table. Refused: a bad or taken name (tables and defined names share
    /// one namespace), a range off the sheet or over another table, column names
    /// that don't match the width or repeat.
    pub fn add_table(&mut self, t: Table) -> Result<(), &'static str> {
        if self.tables.len() >= MAX_TABLES {
            return Err("too many tables");
        }
        if !valid_name(&t.name) || t.name.starts_with("_xlnm.") {
            return Err("not a valid table name");
        }
        if self.table(&t.name).is_some() || self.names.iter().any(|d| d.name.eq_ignore_ascii_case(&t.name)) {
            return Err("the name is taken");
        }
        let r = &t.rect;
        if r.sheet as usize >= self.sheet_count() || r.r0 > r.r1 || r.c0 > r.c1 || r.r1 >= MAX_ROWS || r.c1 >= MAX_COLS {
            return Err("the range is off the sheet");
        }
        if t.header as u32 + t.totals as u32 > r.r1 - r.r0 + 1 {
            return Err("no room for the header and totals rows");
        }
        if t.columns.len() as u64 != (r.c1 - r.c0) as u64 + 1 {
            return Err("one name per column");
        }
        let mut keys: Vec<String> = t.columns.iter().map(|c| crate::eval::funcs::text_key(c)).collect();
        if t.columns.iter().any(|c| c.is_empty() || c.len() > MAX_COL_NAME) {
            return Err("an empty or long column name");
        }
        keys.sort();
        if keys.windows(2).any(|w| w[0] == w[1]) {
            return Err("two columns with one name");
        }
        if self.tables.iter().any(|o| overlaps(&o.rect, r)) {
            return Err("over another table");
        }
        self.tables.push(t);
        self.rebind_names();
        Ok(())
    }

    /// Swaps in `ts` as the workbook's tables (each checked as [`add_table`] does;
    /// one that doesn't fit is left out) and returns the old ones.
    ///
    /// [`add_table`]: Workbook::add_table
    pub fn replace_tables(&mut self, ts: Vec<Table>) -> Vec<Table> {
        let old = std::mem::take(&mut self.tables);
        for t in ts {
            let _ = self.add_table(t);
        }
        self.rebind_names();
        old
    }

    pub fn remove_table(&mut self, name: &str) -> Option<Table> {
        let i = self.tables.iter().position(|t| t.name.eq_ignore_ascii_case(name))?;
        let t = self.tables.remove(i);
        self.rebind_names();
        Some(t)
    }

    /// What a structured reference in cell (`sheet`, `row`, `col`) reaches.
    pub fn resolve_table(&self, sheet: u16, row: u32, col: u32, t: &TableRef) -> Option<Expr> {
        let tb = if t.table.is_empty() { self.table_at(sheet, row, col)? } else { self.table(&t.table)? };
        tb.resolve(t)
    }

    /// `e` with its structured references (and names of tables) replaced by the ranges
    /// they reach from (`sheet`, `row`, `col`), for formats without tables; one that
    /// reaches nothing becomes `#REF!`.
    pub fn inline_tables(&self, e: &Expr, sheet: u16, row: u32, col: u32) -> Expr {
        let map = |x: &Expr| self.inline_tables(x, sheet, row, col);
        match e {
            Expr::Table(t) => self.resolve_table(sheet, row, col, t).unwrap_or(Expr::Err(crate::value::ErrorKind::Ref)),
            Expr::Name(n) if !n.is_empty() && self.names.get(sheet, n).is_none() => match self.table(n) {
                Some(t) => t.resolve(&TableRef { table: n.clone(), area: TableArea::Data, cols: None }).unwrap_or(Expr::Err(crate::value::ErrorKind::Ref)),
                None => e.clone(),
            },
            Expr::Neg(a) => Expr::Neg(Box::new(map(a))),
            Expr::Plus(a) => Expr::Plus(Box::new(map(a))),
            Expr::Pct(a) => Expr::Pct(Box::new(map(a))),
            Expr::Paren(a) => Expr::Paren(Box::new(map(a))),
            Expr::Bin(op, a, b) => Expr::Bin(*op, Box::new(map(a)), Box::new(map(b))),
            Expr::Call(f, args) => Expr::Call(f.clone(), args.iter().map(map).collect()),
            Expr::Array(rows) => Expr::Array(rows.iter().map(|r| r.iter().map(map).collect()).collect()),
            _ => e.clone(),
        }
    }

    /// Whether `e` uses a table (or a name, which may be a table's).
    pub fn uses_tables(e: &Expr) -> bool {
        let mut any = false;
        e.walk(&mut |n| any |= matches!(n, Expr::Table(_)) || matches!(n, Expr::Name(x) if !x.is_empty()));
        any
    }
}

/// `e` with references to table `name` written unqualified, as inside that table.
pub(crate) fn unqualify(e: &Expr, name: &str) -> Expr {
    let map = |x: &Expr| unqualify(x, name);
    match e {
        Expr::Table(t) if t.table.eq_ignore_ascii_case(name) && t.area == TableArea::ThisRow => Expr::Table(Arc::new(TableRef { table: Arc::from(""), ..(**t).clone() })),
        Expr::Neg(a) => Expr::Neg(Box::new(map(a))),
        Expr::Plus(a) => Expr::Plus(Box::new(map(a))),
        Expr::Pct(a) => Expr::Pct(Box::new(map(a))),
        Expr::Paren(a) => Expr::Paren(Box::new(map(a))),
        Expr::Bin(op, a, b) => Expr::Bin(*op, Box::new(map(a)), Box::new(map(b))),
        Expr::Call(f, args) => Expr::Call(f.clone(), args.iter().map(map).collect()),
        Expr::Array(rows) => Expr::Array(rows.iter().map(|r| r.iter().map(map).collect()).collect()),
        _ => e.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr::CellAddr;
    use crate::value::Value;

    fn book() -> Workbook {
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        // A1:C5 table "Sales": header, 3 data rows, totals.
        for (c, h) in ["Region", "Qty", "Price"].iter().enumerate() {
            wb.set_value(CellAddr::new(0, 0, c as u32), Value::str(h));
        }
        for r in 1..4u32 {
            wb.set_value(CellAddr::new(0, r, 0), Value::str(&format!("r{r}")));
            wb.set_value(CellAddr::new(0, r, 1), Value::Num(r as f64));
            wb.set_value(CellAddr::new(0, r, 2), Value::Num(10.0 * r as f64));
        }
        wb.add_table(Table {
            name: "Sales".into(),
            rect: Rect { sheet: 0, r0: 0, c0: 0, r1: 4, c1: 3 },
            header: true,
            totals: true,
            columns: vec!["Region".into(), "Qty".into(), "Price".into(), "Total".into()],
            style: Some("TableStyleMedium2".into()),
            stripes: true,
        })
        .unwrap();
        wb
    }

    #[test]
    fn structured_references() {
        let mut wb = book();
        // A calculated column, written as Excel shows it.
        wb.fill_formula(0, 3, 1, 3, "=[@Qty]*[@Price]").unwrap();
        wb.set_formula(CellAddr::new(0, 4, 3), "=SUM([Total])").unwrap();
        wb.set_formula(CellAddr::new(0, 4, 1), "=SUM(Sales[Qty])").unwrap();
        wb.set_formula(CellAddr::new(0, 7, 0), "=Sales[[#Headers],[Price]]").unwrap();
        wb.set_formula(CellAddr::new(0, 8, 0), "=ROWS(Sales[#All])").unwrap();
        wb.set_formula(CellAddr::new(0, 9, 0), "=SUM(Sales[[Qty]:[Price]])").unwrap();
        wb.set_formula(CellAddr::new(0, 10, 0), "=ROWS(Sales)").unwrap();
        wb.set_formula(CellAddr::new(0, 11, 0), "=Sales[Nope]").unwrap();
        wb.set_formula(CellAddr::new(0, 12, 0), "=Sales[@Qty]").unwrap();
        wb.recalc();
        assert_eq!(wb.get(CellAddr::new(0, 3, 3)), Value::Num(90.0));
        assert_eq!(wb.get(CellAddr::new(0, 4, 3)), Value::Num(10.0 + 40.0 + 90.0));
        assert_eq!(wb.get(CellAddr::new(0, 4, 1)), Value::Num(6.0));
        assert_eq!(wb.get(CellAddr::new(0, 7, 0)), Value::str("Price"));
        assert_eq!(wb.get(CellAddr::new(0, 8, 0)), Value::Num(5.0));
        assert_eq!(wb.get(CellAddr::new(0, 9, 0)), Value::Num(66.0));
        assert_eq!(wb.get(CellAddr::new(0, 10, 0)), Value::Num(3.0));
        assert_eq!(wb.get(CellAddr::new(0, 11, 0)), Value::Err(crate::value::ErrorKind::Ref));
        assert_eq!(wb.get(CellAddr::new(0, 12, 0)), Value::Err(crate::value::ErrorKind::Value));
        // Dependencies follow the table: a new quantity updates the column and totals.
        wb.set_value(CellAddr::new(0, 2, 1), Value::Num(5.0));
        wb.recalc();
        assert_eq!(wb.get(CellAddr::new(0, 2, 3)), Value::Num(100.0));
        assert_eq!(wb.get(CellAddr::new(0, 4, 3)), Value::Num(10.0 + 100.0 + 90.0));
        assert_eq!(wb.get(CellAddr::new(0, 9, 0)), Value::Num(69.0));
        // Text as typed, and in file form.
        assert_eq!(wb.formula_text(CellAddr::new(0, 1, 3)).unwrap(), "=[@Qty]*[@Price]");
        let g = wb.group_at(0, 1, 3).unwrap();
        let file = crate::formula::to_a1_file(&wb.group(g).expr, 1, 3, &|s| wb.sheet_name(s), &|r, c| wb.table_at(0, r, c).map(|t| t.name.clone()));
        assert_eq!(file, "=Sales[[#This Row],[Qty]]*Sales[[#This Row],[Price]]");
        assert_eq!(wb.formula_text(CellAddr::new(0, 7, 0)).unwrap(), "=Sales[[#Headers],[Price]]");
        assert_eq!(wb.formula_text(CellAddr::new(0, 9, 0)).unwrap(), "=SUM(Sales[[Qty]:[Price]])");
        // The file form parses back to the same values.
        let e = wb.parse(&file, 0, 1, 3).unwrap();
        assert_eq!(wb.inline_tables(&e, 0, 1, 3), wb.inline_tables(&wb.group(g).expr, 0, 1, 3));
        assert_eq!(crate::formula::to_a1(&wb.inline_tables(&e, 0, 2, 3), 2, 3, &|s| wb.sheet_name(s)), "=S!$B3*S!$C3");
    }

    #[test]
    fn table_rules_and_native_round_trip() {
        let mut wb = book();
        let mut t = wb.tables()[0].clone();
        assert_eq!(wb.add_table(t.clone()), Err("the name is taken"));
        t.name = "Other".into();
        assert_eq!(wb.add_table(t.clone()), Err("over another table"));
        t.rect = Rect { sheet: 0, r0: 10, c0: 0, r1: 12, c1: 3 };
        t.columns[1] = "region".into();
        assert_eq!(wb.add_table(t.clone()), Err("two columns with one name"));
        t.columns[1] = "B".into();
        for bad in ["A1", "Sales", "TRUE", "a b"] {
            t.name = bad.into();
            assert!(wb.add_table(t.clone()).is_err(), "{bad}");
        }
        t.name = "Other".into();
        wb.add_table(t).unwrap();
        wb.set_formula(CellAddr::new(0, 20, 0), "=SUM(Sales[Qty])+ROWS(Other)").unwrap();
        wb.recalc();
        assert_eq!(wb.get(CellAddr::new(0, 20, 0)), Value::Num(6.0 + 1.0));
        let mut bytes = Vec::new();
        crate::native::save(&wb, &mut bytes).unwrap();
        let back = crate::native::load(&bytes).unwrap();
        assert_eq!(back.tables(), wb.tables());
        assert_eq!(back.get(CellAddr::new(0, 20, 0)), Value::Num(7.0));
        assert_eq!(back.formula_text(CellAddr::new(0, 20, 0)), wb.formula_text(CellAddr::new(0, 20, 0)));
    }

    #[test]
    fn hostile_structured_references_parse_or_fail() {
        let wb = book();
        for s in ["=Sales[", "=Sales[[", "=[]]", "=Sales[[#Bogus]]", "=Sales[[#All],[#This Row]]", "=Sales[[a]:[b]:[c]]", "=A1[x]", "=Sales['", "=Sales[[a],[#All]]", "=[@[a]:[b]:[c]]"] {
            assert!(wb.parse(s, 0, 0, 0).is_err(), "{s}");
        }
        for s in ["=Sales[]", "=Sales[#Data]", "=[@]", "=Sales[[#Data],[#Totals],[Qty]]", "=Sales['#odd'[name']]", "=Sales[ [#Data] , [Qty] ]"] {
            let e = wb.parse(s, 0, 0, 0).unwrap_or_else(|e| panic!("{s}: {e}"));
            // What prints parses back to the same thing.
            let t = crate::formula::to_a1(&e, 0, 0, &|s| wb.sheet_name(s));
            assert_eq!(wb.parse(&t, 0, 0, 0).unwrap(), e, "{s} → {t}");
        }
    }
}
