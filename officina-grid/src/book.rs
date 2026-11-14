//! The workbook: sheets of column-chunked cells, the string pool, formula groups and the
//! dependency index.

use crate::addr::{CellAddr, Rect, MAX_COLS, MAX_ROWS};
use crate::column::{Cell, Column, Sizes};
use crate::deps::{footprints, DepIndex, Footprint, GroupId, RowSet};
use crate::eval::funcs::{self, text_key};
use crate::eval::parse_number;
use crate::fast::Fast;
use crate::formula::{self, Expr, Func, ParseError};
use crate::numfmt::Formatted;
use crate::strings::StringPool;
use crate::style::{Style, StyleId, StyleRuns, StyleTable};
use crate::value::{ErrorKind, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex};

/// The most sheets in a workbook.
pub const MAX_SHEETS: usize = 4096;
/// The longest sheet name, in bytes.
pub const MAX_SHEET_NAME: usize = 255;

#[derive(Debug)]
pub struct Sheet {
    pub name: String,
    pub(crate) cols: Vec<Column>,
    pub row_sizes: Sizes,
    pub col_sizes: Sizes,
    /// Per column: formula groups by first row.
    pub(crate) fidx: Vec<BTreeMap<u32, GroupId>>,
    /// One past the last row ever written (an upper bound; never shrinks).
    pub(crate) rows_used: u32,
    /// Per column: its cells' styles.
    pub styles: Vec<StyleRuns>,
}

impl Sheet {
    fn new(name: &str) -> Self {
        Sheet { name: name.to_string(), cols: Vec::new(), row_sizes: Sizes::new(20.0), col_sizes: Sizes::new(64.0), fidx: Vec::new(), rows_used: 0, styles: Vec::new() }
    }
    pub fn column(&self, c: u32) -> Option<&Column> {
        self.cols.get(c as usize)
    }
    /// The style id of a cell.
    pub fn style_id(&self, r: u32, c: u32) -> StyleId {
        self.styles.get(c as usize).map_or(0, |s| s.get(r))
    }
}

/// A formula filled down one column: one AST for rows `r0..=r1`.
#[derive(Debug, Clone)]
pub struct Group {
    pub sheet: u16,
    pub col: u32,
    pub r0: u32,
    pub r1: u32,
    pub expr: Arc<Expr>,
    pub volatile: bool,
    pub fps: Vec<Footprint>,
    pub(crate) fast: Option<Arc<Fast>>,
    pub(crate) dep_ids: Vec<u32>,
}

/// What a user typed into a cell, for undo and for the native format.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    Empty,
    Value(Value),
    /// Formula text in A1 form for its cell, with the `=`.
    Formula(String),
}

impl Input {
    /// What typing `s` into a cell means: `=...` is a formula, else a number, TRUE or
    /// FALSE, an error value, or text (empty: nothing).
    pub fn typed(s: &str) -> Input {
        if s.starts_with('=') && s.len() > 1 {
            return Input::Formula(s.to_string());
        }
        if s.is_empty() {
            Input::Empty
        } else if let Some(x) = parse_number(s) {
            Input::Value(Value::Num(x))
        } else if s.eq_ignore_ascii_case("TRUE") {
            Input::Value(Value::Bool(true))
        } else if s.eq_ignore_ascii_case("FALSE") {
            Input::Value(Value::Bool(false))
        } else if let Some(e) = ErrorKind::from_text(s) {
            Input::Value(Value::Err(e))
        } else {
            Input::Value(Value::str(s))
        }
    }
}

/// Exact-match index of one column range, for MATCH / VLOOKUP / XLOOKUP.
#[derive(Debug, Default)]
pub struct LookupIndex {
    map: HashMap<Key, u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Num(u64),
    Text(String),
    Bool(bool),
}

fn key(v: &Value) -> Option<Key> {
    match v {
        Value::Num(x) => Some(Key::Num(if *x == 0.0 { 0 } else { x.to_bits() })),
        Value::Str(s) => Some(Key::Text(text_key(s))),
        Value::Bool(b) => Some(Key::Bool(*b)),
        _ => None,
    }
}

impl LookupIndex {
    pub fn get(&self, v: &Value) -> Option<u32> {
        self.map.get(&key(v)?).copied()
    }
}

type LookupKey = (u16, u32, u32, u32);

pub struct Workbook {
    pub(crate) sheets: Vec<Sheet>,
    pub strings: StringPool,
    pub styles: StyleTable,
    pub(crate) groups: Vec<Option<Group>>,
    free_groups: Vec<GroupId>,
    pub(crate) deps: DepIndex,
    /// Value writes since the last recalculation.
    pub(crate) changed: Vec<Rect>,
    /// Groups (and their rows) to evaluate at the next recalculation.
    pub(crate) pending: HashMap<GroupId, RowSet>,
    pub(crate) volatile: HashSet<GroupId>,
    lookup: Mutex<HashMap<LookupKey, (u64, Arc<LookupIndex>)>>,
    /// Threads recalculation may use.
    pub threads: usize,
}

impl Default for Workbook {
    fn default() -> Self {
        Self::new()
    }
}

impl Workbook {
    pub fn new() -> Self {
        Workbook {
            sheets: Vec::new(),
            strings: StringPool::new(),
            styles: StyleTable::new(),
            groups: Vec::new(),
            free_groups: Vec::new(),
            deps: DepIndex::default(),
            changed: Vec::new(),
            pending: HashMap::new(),
            volatile: HashSet::new(),
            lookup: Mutex::new(HashMap::new()),
            threads: std::thread::available_parallelism().map_or(1, |n| n.get()),
        }
    }

    // ---------- sheets ----------

    pub fn add_sheet(&mut self, name: &str) -> Result<u16, &'static str> {
        if self.sheets.len() >= MAX_SHEETS {
            return Err("too many sheets");
        }
        if name.is_empty() || name.len() > MAX_SHEET_NAME || name.contains(['[', ']', ':', '*', '?', '/', '\\']) {
            return Err("bad sheet name");
        }
        if self.sheet_id(name).is_some() {
            return Err("a sheet with that name exists");
        }
        self.sheets.push(Sheet::new(name));
        Ok((self.sheets.len() - 1) as u16)
    }
    pub fn sheet_count(&self) -> usize {
        self.sheets.len()
    }
    pub fn sheet(&self, s: u16) -> &Sheet {
        &self.sheets[s as usize]
    }
    pub fn sheet_mut(&mut self, s: u16) -> &mut Sheet {
        &mut self.sheets[s as usize]
    }
    pub fn sheet_id(&self, name: &str) -> Option<u16> {
        self.sheets.iter().position(|s| s.name.eq_ignore_ascii_case(name)).map(|i| i as u16)
    }
    pub fn sheet_name(&self, s: u16) -> String {
        self.sheets.get(s as usize).map_or_else(|| "#REF".into(), |s| s.name.clone())
    }

    // ---------- reading ----------

    #[inline]
    pub fn cell(&self, sheet: u16, row: u32, col: u32) -> Cell {
        match self.sheets.get(sheet as usize).and_then(|s| s.cols.get(col as usize)) {
            Some(c) => c.get(row),
            None => Cell::Empty,
        }
    }

    #[inline]
    pub fn cell_value(&self, c: Cell) -> Value {
        match c {
            Cell::Empty => Value::Empty,
            Cell::Num(x) => Value::Num(x),
            Cell::Str(id) => Value::Str(self.strings.get(id).clone()),
            Cell::Bool(b) => Value::Bool(b),
            Cell::Err(e) => Value::Err(e),
        }
    }

    pub fn value(&self, sheet: u16, row: u32, col: u32) -> Value {
        self.cell_value(self.cell(sheet, row, col))
    }

    pub fn get(&self, a: CellAddr) -> Value {
        self.value(a.sheet, a.row, a.col)
    }

    /// (rows, cols) that may hold anything.
    pub fn extent(&self, sheet: u16) -> (u32, u32) {
        match self.sheets.get(sheet as usize) {
            Some(s) => (s.rows_used, s.cols.len() as u32),
            None => (0, 0),
        }
    }

    /// Calls `f(row, col, value)` for each non-empty cell in `r`, column by column (fast:
    /// empty chunks are skipped unseen).
    pub fn each(&self, r: &Rect, f: &mut dyn FnMut(u32, u32, Value)) {
        let Some(s) = self.sheets.get(r.sheet as usize) else { return };
        let c1 = r.c1.min(s.cols.len().saturating_sub(1) as u32);
        if s.cols.is_empty() || r.c0 > c1 {
            return;
        }
        for c in r.c0..=c1 {
            s.cols[c as usize].each(r.r0, r.r1, &mut |row, cell| f(row, c, self.cell_value(cell)));
        }
    }

    /// As [`each`](Self::each) with stored cells, not values: no text is touched (the
    /// numeric aggregates' path).
    pub fn each_cell(&self, r: &Rect, f: &mut dyn FnMut(u32, u32, Cell)) {
        let Some(s) = self.sheets.get(r.sheet as usize) else { return };
        let c1 = r.c1.min(s.cols.len().saturating_sub(1) as u32);
        if s.cols.is_empty() || r.c0 > c1 {
            return;
        }
        for c in r.c0..=c1 {
            s.cols[c as usize].each(r.r0, r.r1, &mut |row, cell| f(row, c, cell));
        }
    }

    /// [`Column::each_num_run`] over a rectangle.
    pub fn each_num_run(&self, r: &Rect, nums: &mut dyn FnMut(&[f64]), other: &mut dyn FnMut(u32, u32, Cell)) {
        let Some(s) = self.sheets.get(r.sheet as usize) else { return };
        let c1 = r.c1.min(s.cols.len().saturating_sub(1) as u32);
        if s.cols.is_empty() || r.c0 > c1 {
            return;
        }
        for c in r.c0..=c1 {
            s.cols[c as usize].each_num_run(r.r0, r.r1, nums, &mut |row, cell| other(row, c, cell));
        }
    }

    /// As [`each`](Self::each) but in reading order (row by row), for functions where order
    /// matters (CONCAT, TEXTJOIN).
    pub fn each_rowmajor(&self, r: &Rect, f: &mut dyn FnMut(u32, u32, Value)) {
        if r.c0 == r.c1 {
            return self.each(r, f);
        }
        let mut cells = Vec::new();
        self.each(r, &mut |row, col, v| cells.push((row, col, v)));
        cells.sort_by_key(|&(row, col, _)| (row, col));
        for (row, col, v) in cells {
            f(row, col, v);
        }
    }

    // ---------- writing values ----------

    pub(crate) fn col_mut(&mut self, sheet: u16, col: u32) -> &mut Column {
        let s = &mut self.sheets[sheet as usize];
        if col as usize >= s.cols.len() {
            s.cols.resize_with(col as usize + 1, Column::default);
            s.fidx.resize_with(col as usize + 1, BTreeMap::new);
        }
        &mut s.cols[col as usize]
    }

    /// Stores a value without touching formulas or dependencies (results, loading).
    pub(crate) fn store(&mut self, sheet: u16, row: u32, col: u32, v: &Value) {
        let c = match v {
            Value::Empty => Cell::Empty,
            Value::Num(x) => Cell::Num(*x),
            Value::Str(s) => Cell::Str(self.strings.intern_arc(s)),
            Value::Bool(b) => Cell::Bool(*b),
            Value::Err(e) => Cell::Err(*e),
        };
        self.store_cell(sheet, row, col, c);
    }

    pub(crate) fn store_cell(&mut self, sheet: u16, row: u32, col: u32, c: Cell) {
        if !c.is_empty() {
            let s = &mut self.sheets[sheet as usize];
            s.rows_used = s.rows_used.max(row + 1);
        }
        self.col_mut(sheet, col).set(row, c);
    }

    /// Sets a cell to a value, replacing any formula there.
    pub fn set_value(&mut self, a: CellAddr, v: Value) {
        assert!(a.row < MAX_ROWS && a.col < MAX_COLS);
        self.clear_formula(a.sheet, a.row, a.col);
        self.store(a.sheet, a.row, a.col, &v);
        self.changed.push(Rect::cell(a));
    }

    /// Writes a column of numbers starting at `row` (bulk load; replaces formulas there).
    pub fn set_numbers(&mut self, sheet: u16, col: u32, row: u32, vals: &[f64]) {
        if vals.is_empty() {
            return;
        }
        let r1 = row + vals.len() as u32 - 1;
        self.clear_formulas_in(sheet, col, row, r1);
        {
            let s = &mut self.sheets[sheet as usize];
            s.rows_used = s.rows_used.max(r1 + 1);
        }
        let c = self.col_mut(sheet, col);
        for (i, x) in vals.iter().enumerate() {
            c.set(row + i as u32, Cell::Num(*x));
        }
        self.changed.push(Rect { sheet, r0: row, r1, c0: col, c1: col });
    }

    /// Sets a cell from what a user typed: `=...` is a formula, else a number, TRUE or
    /// FALSE, an error value, or text.
    pub fn set_input(&mut self, a: CellAddr, s: &str) -> Result<(), ParseError> {
        match Input::typed(s) {
            Input::Formula(f) => self.set_formula(a, &f),
            i => self.set(a, &i),
        }
    }

    pub fn set(&mut self, a: CellAddr, input: &Input) -> Result<(), ParseError> {
        match input {
            Input::Empty => {
                self.set_value(a, Value::Empty);
                Ok(())
            }
            Input::Value(v) => {
                self.set_value(a, v.clone());
                Ok(())
            }
            Input::Formula(f) => self.set_formula(a, f),
        }
    }

    /// What the cell holds as input (its formula, else its value).
    pub fn input(&self, a: CellAddr) -> Input {
        if let Some(f) = self.formula_text(a) {
            return Input::Formula(f);
        }
        match self.get(a) {
            Value::Empty => Input::Empty,
            v => Input::Value(v),
        }
    }

    // ---------- formulas ----------

    pub fn group(&self, g: GroupId) -> &Group {
        self.groups[g as usize].as_ref().unwrap()
    }

    pub fn groups(&self) -> impl Iterator<Item = (GroupId, &Group)> {
        self.groups.iter().enumerate().filter_map(|(i, g)| g.as_ref().map(|g| (i as GroupId, g)))
    }

    pub fn group_at(&self, sheet: u16, row: u32, col: u32) -> Option<GroupId> {
        let f = self.sheets.get(sheet as usize)?.fidx.get(col as usize)?;
        let (_, &g) = f.range(..=row).next_back()?;
        (self.group(g).r1 >= row).then_some(g)
    }

    pub fn formula_text(&self, a: CellAddr) -> Option<String> {
        let g = self.group_at(a.sheet, a.row, a.col)?;
        Some(formula::to_a1(&self.group(g).expr, a.row, a.col, &|s| self.sheet_name(s)))
    }

    pub fn parse(&self, src: &str, sheet: u16, row: u32, col: u32) -> Result<Expr, ParseError> {
        let _ = sheet;
        let e = formula::parse(src, row, col, &|n: &str| self.sheet_id(n))?;
        check_depth(&e)?;
        Ok(e)
    }

    /// Parses formula text read back from a file.
    pub(crate) fn parse_stored(&self, src: &str, row: u32, col: u32) -> Result<Expr, ParseError> {
        let e = formula::parse_stored(src, row, col, &|n: &str| self.sheet_id(n))?;
        check_depth(&e)?;
        Ok(e)
    }

    /// Puts a formula in one cell. Joins the group above (or below) when the formula is
    /// the same fill, so typing down a column keeps one group.
    pub fn set_formula(&mut self, a: CellAddr, src: &str) -> Result<(), ParseError> {
        self.fill_formula(a.sheet, a.col, a.row, a.row, src).map(|_| ())
    }

    /// Fills `src` (as written for row `r0`) down `r0..=r1` of a column as one group,
    /// joined with the group just above or below when that is the same fill.
    pub fn fill_formula(&mut self, sheet: u16, col: u32, r0: u32, r1: u32, src: &str) -> Result<GroupId, ParseError> {
        assert!(r0 <= r1 && r1 < MAX_ROWS && col < MAX_COLS);
        let e = Arc::new(self.parse(src, sheet, r0, col)?);
        self.clear_formulas_in(sheet, col, r0, r1);
        self.changed.push(Rect { sheet, r0, r1, c0: col, c1: col });
        let above = if r0 > 0 { self.group_at(sheet, r0 - 1, col) } else { None };
        let below = if r1 + 1 < MAX_ROWS { self.group_at(sheet, r1 + 1, col) } else { None };
        let (mut a, mut b, mut expr) = (r0, r1, e.clone());
        let mut carried = Vec::new();
        if let Some(g) = above {
            if *self.group(g).expr == *e {
                expr = self.group(g).expr.clone();
                a = self.group(g).r0;
                carried.extend(self.remove_group(g, true));
            }
        }
        if let Some(g) = below {
            if *self.group(g).expr == *e {
                b = self.group(g).r1;
                carried.extend(self.remove_group(g, true));
            }
        }
        let id = self.add_group(sheet, col, a, b, expr, Some((r0, r1)));
        for p in carried {
            for (x, y) in p.0 {
                self.pending.entry(id).or_default().add(x, y);
            }
        }
        Ok(id)
    }

    pub(crate) fn add_group(&mut self, sheet: u16, col: u32, r0: u32, r1: u32, expr: Arc<Expr>, dirty: Option<(u32, u32)>) -> GroupId {
        let mut volatile = false;
        expr.walk(&mut |n| {
            if let Expr::Call(Func::Known(id), _) = n {
                volatile |= funcs::FUNCS[*id as usize].volatile;
            }
        });
        let fps = footprints(&expr, sheet, col, r0, r1);
        let fast = Fast::compile(&expr, sheet, col).map(Arc::new);
        let g = Group { sheet, col, r0, r1, expr, volatile, fps, fast, dep_ids: Vec::new() };
        let id = match self.free_groups.pop() {
            Some(id) => {
                self.groups[id as usize] = Some(g);
                id
            }
            None => {
                self.groups.push(Some(g));
                (self.groups.len() - 1) as GroupId
            }
        };
        let fps = self.group(id).fps.clone();
        let mut ids = Vec::with_capacity(fps.len());
        for fp in fps {
            if let Some(rect) = fp.rect(r0, r1) {
                ids.push(self.deps.add(id, fp, rect));
            }
        }
        self.groups[id as usize].as_mut().unwrap().dep_ids = ids;
        self.col_mut(sheet, col);
        self.sheets[sheet as usize].fidx[col as usize].insert(r0, id);
        {
            let s = &mut self.sheets[sheet as usize];
            s.rows_used = s.rows_used.max(r1 + 1);
        }
        if volatile {
            self.volatile.insert(id);
        }
        if let Some((a, b)) = dirty {
            self.pending.entry(id).or_default().add(a, b);
        }
        id
    }

    /// Takes a group out. With `keep_pending` its pending rows move to whatever replaces
    /// it (the caller re-adds groups covering them).
    fn remove_group(&mut self, g: GroupId, keep_pending: bool) -> Option<RowSet> {
        let grp = self.groups[g as usize].take().unwrap();
        for id in &grp.dep_ids {
            self.deps.remove(*id);
        }
        self.sheets[grp.sheet as usize].fidx[grp.col as usize].remove(&grp.r0);
        self.volatile.remove(&g);
        self.free_groups.push(g);
        let p = self.pending.remove(&g);
        if keep_pending {
            p
        } else {
            None
        }
    }

    /// Removes the formula at one cell (splitting its group), leaving the stored value.
    fn clear_formula(&mut self, sheet: u16, row: u32, col: u32) {
        self.clear_formulas_in(sheet, col, row, row);
    }

    /// Removes formulas from rows `a..=b` of a column, splitting groups that cross the
    /// edges. Pending rows of the remaining parts stay pending.
    pub(crate) fn clear_formulas_in(&mut self, sheet: u16, col: u32, a: u32, b: u32) {
        let Some(f) = self.sheets.get(sheet as usize).and_then(|s| s.fidx.get(col as usize)) else { return };
        let mut hit: Vec<GroupId> = f.range(..=b).rev().take_while(|(_, &g)| self.group(g).r1 >= a).map(|(_, &g)| g).collect();
        // `take_while` above stops at the first group ending before `a`; groups are
        // disjoint and sorted, so that is all of them.
        hit.reverse();
        for g in hit {
            let grp = self.group(g).clone();
            let pend = self.remove_group(g, true);
            let re = |wb: &mut Workbook, lo: u32, hi: u32| {
                let id = wb.add_group(sheet, col, lo, hi, grp.expr.clone(), None);
                if let Some(p) = &pend {
                    for (x, y) in p.within(lo, hi) {
                        wb.pending.entry(id).or_default().add(x, y);
                    }
                }
            };
            if grp.r0 < a {
                re(self, grp.r0, a - 1);
            }
            if grp.r1 > b {
                re(self, b + 1, grp.r1);
            }
        }
    }

    // ---------- lookups ----------

    /// The exact-match index of rows `r0..r0+len` of a column, built once per version of
    /// the column.
    pub fn lookup_index(&self, sheet: u16, col: u32, r0: u32, len: u32) -> Arc<LookupIndex> {
        let gen = self.sheets[sheet as usize].cols.get(col as usize).map_or(0, |c| c.generation);
        let k = (sheet, col, r0, len);
        if let Some((g, idx)) = self.lookup.lock().unwrap().get(&k) {
            if *g == gen {
                return idx.clone();
            }
        }
        let mut map = HashMap::new();
        if len > 0 {
            let r = Rect { sheet, r0, r1: r0 + len - 1, c0: col, c1: col };
            self.each(&r, &mut |row, _, v| {
                if let Some(kk) = key(&v) {
                    map.entry(kk).or_insert(row - r0);
                }
            });
        }
        let idx = Arc::new(LookupIndex { map });
        let mut l = self.lookup.lock().unwrap();
        if l.len() > 256 {
            l.clear();
        }
        l.insert(k, (gen, idx.clone()));
        idx
    }

    /// Bytes held by cell storage (not counting strings and formulas).
    pub fn storage_bytes(&self) -> usize {
        self.sheets.iter().flat_map(|s| s.cols.iter()).map(|c| c.bytes()).sum()
    }
}

/// Refuses ASTs deeper than 256 nodes (a long `1+1+...` chain parses flat but would
/// recurse deeply in evaluation). Checked without recursion.
fn check_depth(e: &Expr) -> Result<(), ParseError> {
    let mut stack: Vec<(&Expr, usize)> = vec![(e, 1)];
    while let Some((n, d)) = stack.pop() {
        if d > 256 {
            return Err(ParseError { at: 0, msg: "formula nested too deeply".into() });
        }
        match n {
            Expr::Neg(a) | Expr::Plus(a) | Expr::Pct(a) | Expr::Paren(a) => stack.push((a, d + 1)),
            Expr::Bin(_, a, b) => {
                stack.push((a, d + 1));
                stack.push((b, d + 1));
            }
            Expr::Call(_, args) => args.iter().for_each(|a| stack.push((a, d + 1))),
            Expr::Array(rows) => rows.iter().flatten().for_each(|a| stack.push((a, d + 1))),
            _ => {}
        }
    }
    Ok(())
}

// ---------- styles ----------

impl Workbook {
    /// The style id of a cell.
    pub fn style_id(&self, a: CellAddr) -> StyleId {
        self.sheets.get(a.sheet as usize).map_or(0, |s| s.style_id(a.row, a.col))
    }

    /// The style of a cell.
    pub fn style(&self, a: CellAddr) -> &Style {
        self.styles.get(self.style_id(a))
    }

    /// Sets the style of every cell in `r`.
    pub fn set_style_id(&mut self, r: &Rect, id: StyleId) {
        let Some(sh) = self.sheets.get_mut(r.sheet as usize) else { return };
        if id == 0 && r.c0 as usize >= sh.styles.len() {
            return;
        }
        let need = (r.c1 as usize + 1).min(MAX_COLS as usize);
        if id != 0 && sh.styles.len() < need {
            sh.styles.resize_with(need, StyleRuns::default);
        }
        let last = (r.c1 as usize).min(sh.styles.len().saturating_sub(1));
        for c in r.c0 as usize..=last {
            sh.styles[c].set(r.r0, r.r1, id);
        }
        while sh.styles.last().is_some_and(|s| s.is_empty()) {
            sh.styles.pop();
        }
    }

    /// Interns `s` and sets it on every cell in `r`.
    pub fn set_style(&mut self, r: &Rect, s: Style) {
        let id = self.styles.intern(s);
        self.set_style_id(r, id);
    }

    /// How a cell shows: its value through its number format.
    pub fn formatted(&self, a: CellAddr) -> Formatted {
        let v = self.get(a);
        self.styles.format(self.style_id(a)).format(&v)
    }
}
