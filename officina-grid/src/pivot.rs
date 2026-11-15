//! Pivot tables (docs/spec/officina.md §8.4, §16.32): a summary of a source range
//! by the items of some of its columns, written into the sheet as values when it is
//! refreshed, as Excel does.
//!
//! The layout is Excel's tabular form: each row field has a column of its own, its
//! items shown on the first row of their group, a "<item> Total" row after each group
//! of an outer field, and a "Grand Total" row; column fields the same way across,
//! with a "Values" field among them when there is more than one value field. Page
//! (filter) fields sit above the table. Refreshing is one scan of the source: each
//! row adds into every subtotal it belongs to.

use crate::addr::{CellAddr, Rect, MAX_COLS, MAX_ROWS};
use crate::book::Workbook;
use crate::value::{cmp_text, ErrorKind, Value};
use officina_core::wire::{Reader, WireError, Writer};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Arc;

pub const MAX_PIVOTS: usize = 256;
/// Fields on one axis, and value fields.
pub const MAX_FIELDS: usize = 16;
/// Output cells a refresh may write.
pub const MAX_OUTPUT: u64 = 1 << 24;
/// Distinct items a field on an axis may have.
pub const MAX_ITEMS: usize = 1 << 20;

/// How a value field sums up its cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agg {
    Sum,
    /// Cells that aren't empty (Excel's "Count").
    Count,
    Average,
    Max,
    Min,
    Product,
    /// Numbers only.
    CountNums,
    StdDev,
    StdDevP,
    Var,
    VarP,
}

impl Agg {
    pub const ALL: [Agg; 11] = [Agg::Sum, Agg::Count, Agg::Average, Agg::Max, Agg::Min, Agg::Product, Agg::CountNums, Agg::StdDev, Agg::StdDevP, Agg::Var, Agg::VarP];

    /// The word Excel puts before a value field's name ("Sum of Units").
    pub fn label(self) -> &'static str {
        match self {
            Agg::Sum => "Sum",
            Agg::Count => "Count",
            Agg::Average => "Average",
            Agg::Max => "Max",
            Agg::Min => "Min",
            Agg::Product => "Product",
            Agg::CountNums => "Count",
            Agg::StdDev => "StdDev",
            Agg::StdDevP => "StdDevp",
            Agg::Var => "Var",
            Agg::VarP => "Varp",
        }
    }
}

/// A source column on an axis, with the items left out.
#[derive(Debug, Clone, PartialEq)]
pub struct PivotField {
    /// Column of the source, from its first column.
    pub field: u32,
    /// Items not shown (and not counted), as their values.
    pub hidden: Vec<Value>,
}

impl PivotField {
    pub fn new(field: u32) -> Self {
        PivotField { field, hidden: Vec::new() }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DataField {
    pub field: u32,
    pub agg: Agg,
    /// Its caption; "Sum of <field>" when `None`.
    pub name: Option<Arc<str>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Pivot {
    pub name: Arc<str>,
    /// The source with its header row (field names) first.
    pub source: Rect,
    /// Top-left of the output (page fields included).
    pub dest: CellAddr,
    pub rows: Vec<PivotField>,
    pub cols: Vec<PivotField>,
    pub pages: Vec<PivotField>,
    pub data: Vec<DataField>,
    /// The Grand Total row at the bottom.
    pub grand_row: bool,
    /// The Grand Total column at the right.
    pub grand_col: bool,
    /// Subtotals for outer row and column fields.
    pub subtotals: bool,
}

impl Pivot {
    pub fn new(name: &str, source: Rect, dest: CellAddr) -> Self {
        Pivot { name: Arc::from(name), source, dest, rows: Vec::new(), cols: Vec::new(), pages: Vec::new(), data: Vec::new(), grand_row: true, grand_col: true, subtotals: true }
    }
}

/// What part of a pivot a cell is, for drawing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// A page field and its selection.
    Page,
    /// Field names and column item labels.
    Header,
    /// A row item label.
    Label,
    Data,
    /// A subtotal or grand total row or column.
    Total,
}

/// Where a refresh put a pivot's parts.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Shape {
    /// The whole output, page fields included; `None` when nothing was written.
    pub area: Option<Rect>,
    /// Rows of page fields (and the gap after them) at the top.
    pub page_rows: u32,
    /// Header rows of the table under them.
    pub header_rows: u32,
    /// Label columns at the left.
    pub label_cols: u32,
    /// Total rows and columns, from the table's first row and column.
    pub total_rows: Vec<u32>,
    pub total_cols: Vec<u32>,
}

impl Shape {
    /// The table without the page fields (Excel's pivot "location").
    pub fn table(&self) -> Option<Rect> {
        self.area.map(|a| Rect { r0: a.r0 + self.page_rows, ..a })
    }

    pub fn role(&self, a: CellAddr) -> Option<Role> {
        let area = self.area?;
        if !area.contains(a) {
            return None;
        }
        let (r, c) = (a.row - area.r0, a.col - area.c0);
        if r < self.page_rows {
            return Some(Role::Page);
        }
        let r = r - self.page_rows;
        if r < self.header_rows {
            return Some(Role::Header);
        }
        let r = r - self.header_rows;
        if self.total_rows.contains(&r) || self.total_cols.contains(&c) {
            return Some(Role::Total);
        }
        Some(if c < self.label_cols { Role::Label } else { Role::Data })
    }
}

/// An item's identity: text without regard to case, as Excel groups items.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Num(u64),
    Text(String),
    Bool(bool),
    Err(u8),
    Blank,
}

fn key(v: &Value) -> Key {
    match v {
        Value::Num(x) => Key::Num(if *x == 0.0 { 0 } else { x.to_bits() }),
        Value::Str(s) if s.is_empty() => Key::Blank,
        Value::Str(s) => Key::Text(s.to_lowercase()),
        Value::Bool(b) => Key::Bool(*b),
        Value::Err(e) => Key::Err(*e as u8),
        Value::Empty => Key::Blank,
    }
}

/// Whether two values are the same pivot item (text in any case).
pub fn same_item(a: &Value, b: &Value) -> bool {
    key(a) == key(b)
}

/// Excel's item order: numbers, text (any case), FALSE before TRUE, errors, blank.
pub fn item_order(a: &Value, b: &Value) -> Ordering {
    fn rank(v: &Value) -> u8 {
        match v {
            Value::Num(_) => 0,
            Value::Str(s) if !s.is_empty() => 1,
            Value::Bool(_) => 2,
            Value::Err(_) => 3,
            _ => 4,
        }
    }
    match (a, b) {
        (Value::Num(x), Value::Num(y)) => x.total_cmp(y),
        (Value::Str(x), Value::Str(y)) if !x.is_empty() && !y.is_empty() => cmp_text(x, y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (Value::Err(x), Value::Err(y)) => (*x as u8).cmp(&(*y as u8)),
        _ => rank(a).cmp(&rank(b)),
    }
}

/// How an item shows as a label.
fn label(v: &Value) -> Value {
    match v {
        Value::Empty => Value::str("(blank)"),
        Value::Str(s) if s.is_empty() => Value::str("(blank)"),
        v => v.clone(),
    }
}

fn label_text(v: &Value) -> String {
    match label(v) {
        Value::Str(s) => s.to_string(),
        v => v.display(),
    }
}

/// One field's items in order, and each source row's item.
struct Axis {
    items: Vec<Value>,
}

#[derive(Clone, Default)]
struct Acc {
    n: f64,
    count: f64,
    sum: f64,
    sq: f64,
    prod: f64,
    max: f64,
    min: f64,
    err: Option<ErrorKind>,
}

impl Acc {
    fn add(&mut self, v: &Value) {
        match v {
            Value::Num(x) => {
                if self.n == 0.0 {
                    self.max = *x;
                    self.min = *x;
                    self.prod = 1.0;
                }
                self.n += 1.0;
                self.count += 1.0;
                self.sum += x;
                self.sq += x * x;
                self.prod *= x;
                self.max = self.max.max(*x);
                self.min = self.min.min(*x);
            }
            Value::Err(e) => {
                self.count += 1.0;
                self.err.get_or_insert(*e);
            }
            Value::Empty => {}
            _ => self.count += 1.0,
        }
    }

    fn merge(&mut self, o: &Acc) {
        if o.n > 0.0 {
            if self.n == 0.0 {
                self.max = o.max;
                self.min = o.min;
                self.prod = 1.0;
            }
            self.max = self.max.max(o.max);
            self.min = self.min.min(o.min);
            self.prod *= o.prod;
        }
        self.n += o.n;
        self.count += o.count;
        self.sum += o.sum;
        self.sq += o.sq;
        if self.err.is_none() {
            self.err = o.err;
        }
    }

    fn result(&self, agg: Agg) -> Value {
        if let (Some(e), false) = (self.err, matches!(agg, Agg::Count)) {
            return Value::Err(e);
        }
        let var = |pop: bool| {
            let d = if pop { self.n } else { self.n - 1.0 };
            if d <= 0.0 {
                return None;
            }
            Some(((self.sq - self.sum * self.sum / self.n) / d).max(0.0))
        };
        let num = |x: Option<f64>| x.map_or(Value::Err(ErrorKind::Div0), Value::Num);
        match agg {
            Agg::Sum => Value::Num(self.sum),
            Agg::Count => Value::Num(self.count),
            Agg::CountNums => Value::Num(self.n),
            Agg::Average => num((self.n > 0.0).then(|| self.sum / self.n)),
            Agg::Max => Value::Num(if self.n > 0.0 { self.max } else { 0.0 }),
            Agg::Min => Value::Num(if self.n > 0.0 { self.min } else { 0.0 }),
            Agg::Product => Value::Num(if self.n > 0.0 { self.prod } else { 0.0 }),
            Agg::StdDev => num(var(false).map(f64::sqrt)),
            Agg::StdDevP => num(var(true).map(f64::sqrt)),
            Agg::Var => num(var(false)),
            Agg::VarP => num(var(true)),
        }
    }
}

/// A column header of the output: the column items (a prefix of the column
/// fields' items, `None` past the real fields) and the value field it shows.
#[derive(Clone, Debug)]
struct ColHead {
    path: Vec<u32>,
    data: usize,
    /// A subtotal (of `path`) or the grand total (empty `path`).
    total: bool,
}

/// The computed output: cells relative to the area's top-left, and its shape.
pub struct Output {
    pub cells: Vec<(u32, u32, Value)>,
    pub rows: u32,
    pub cols: u32,
    pub shape: Shape,
}

impl Workbook {
    pub fn pivots(&self) -> &[Pivot] {
        &self.pivots
    }

    /// The shape of pivot `i`'s last refresh.
    pub fn pivot_shape(&self, i: usize) -> Option<&Shape> {
        self.pivot_shapes.get(i)
    }

    /// The pivot that owns a cell, and the cell's part of it.
    pub fn pivot_at(&self, a: CellAddr) -> Option<(usize, Role)> {
        self.pivot_shapes.iter().enumerate().find_map(|(i, s)| (self.pivots[i].dest.sheet == a.sheet).then(|| s.role(a)).flatten().map(|r| (i, r)))
    }

    /// A source column's name: its header cell's text.
    pub fn pivot_field_name(&self, p: &Pivot, field: u32) -> String {
        let v = self.get(CellAddr::new(p.source.sheet, p.source.r0, p.source.c0 + field));
        match v {
            Value::Str(s) => s.to_string(),
            Value::Empty => format!("Column{}", field + 1),
            v => v.display(),
        }
    }

    /// A value field's caption.
    pub fn data_caption(&self, p: &Pivot, d: &DataField) -> String {
        match &d.name {
            Some(n) => n.to_string(),
            None => format!("{} of {}", d.agg.label(), self.pivot_field_name(p, d.field)),
        }
    }

    fn check_pivot(&self, p: &Pivot) -> Result<(), &'static str> {
        let s = &p.source;
        if s.sheet as usize >= self.sheet_count() || p.dest.sheet as usize >= self.sheet_count() {
            return Err("no such sheet");
        }
        if s.r0 > s.r1 || s.c0 > s.c1 || s.r1 >= MAX_ROWS || s.c1 >= MAX_COLS || p.dest.row >= MAX_ROWS || p.dest.col >= MAX_COLS {
            return Err("a range outside the sheet");
        }
        let width = s.c1 - s.c0 + 1;
        let all = p.rows.iter().chain(&p.cols).chain(&p.pages);
        if p.rows.len() > MAX_FIELDS || p.cols.len() > MAX_FIELDS || p.pages.len() > MAX_FIELDS || p.data.len() > MAX_FIELDS {
            return Err("too many fields");
        }
        let mut seen = Vec::new();
        for f in all {
            if f.field >= width {
                return Err("a field outside the source");
            }
            if seen.contains(&f.field) {
                return Err("a field on two axes");
            }
            seen.push(f.field);
        }
        if p.data.iter().any(|d| d.field >= width) {
            return Err("a field outside the source");
        }
        if p.name.is_empty() || p.name.len() > 255 {
            return Err("a pivot table needs a name");
        }
        Ok(())
    }

    /// Adds a pivot table and refreshes it.
    pub fn add_pivot(&mut self, p: Pivot) -> Result<usize, &'static str> {
        if self.pivots.len() >= MAX_PIVOTS {
            return Err("too many pivot tables");
        }
        self.check_pivot(&p)?;
        if self.pivots.iter().any(|q| q.name.eq_ignore_ascii_case(&p.name)) {
            return Err("a pivot table of that name exists");
        }
        self.pivots.push(p);
        self.pivot_shapes.push(Shape::default());
        let i = self.pivots.len() - 1;
        if let Err(e) = self.refresh_pivot(i) {
            self.pivots.pop();
            self.pivot_shapes.pop();
            return Err(e);
        }
        Ok(i)
    }

    /// Adds a pivot whose output is already in the cells (a file's): its shape is
    /// worked out, nothing is written. `area` is where the file says the output is
    /// (page fields included), when that isn't where ours would be (another
    /// program's layout): it is kept until the next refresh.
    pub fn add_pivot_as_is(&mut self, p: Pivot, area: Option<Rect>) -> Result<usize, &'static str> {
        if self.pivots.len() >= MAX_PIVOTS {
            return Err("too many pivot tables");
        }
        self.check_pivot(&p)?;
        let mut shape = self.pivot_output(&p)?.shape;
        if let Some(a) = area.filter(|a| Some(*a) != shape.area) {
            if a.sheet != p.dest.sheet || a.r0 > a.r1 || a.c0 > a.c1 || a.r1 >= MAX_ROWS || a.c1 >= MAX_COLS {
                return Err("a range outside the sheet");
            }
            shape.area = Some(a);
            shape.total_rows.clear();
            shape.total_cols.clear();
        }
        self.pivots.push(p);
        self.pivot_shapes.push(shape);
        Ok(self.pivots.len() - 1)
    }

    /// The pivots with where their output is, as [`GridOp::Pivots`] holds them.
    ///
    /// [`GridOp::Pivots`]: crate::ops::GridOp::Pivots
    pub fn pivot_state(&self) -> Vec<(Pivot, Option<Rect>)> {
        self.pivots.iter().cloned().zip(self.pivot_shapes.iter().map(|s| s.area)).collect()
    }

    /// Swaps in pivots (each checked as [`add_pivot_as_is`] does; their cells are
    /// not touched) and returns the old ones.
    ///
    /// [`add_pivot_as_is`]: Workbook::add_pivot_as_is
    pub fn replace_pivots(&mut self, v: Vec<(Pivot, Option<Rect>)>) -> Vec<(Pivot, Option<Rect>)> {
        let old = self.pivot_state();
        self.pivots.clear();
        self.pivot_shapes.clear();
        for (p, area) in v {
            let _ = self.add_pivot_as_is(p, area);
        }
        old
    }

    /// The undoable edit that adds (`i` = `None`), changes or refreshes (`p` is the
    /// pivot's new definition) or removes (`p` = `None`) a pivot, cells and all.
    /// Refused as [`refresh_pivot`] refuses.
    ///
    /// [`refresh_pivot`]: Workbook::refresh_pivot
    pub fn pivot_op(&self, i: Option<usize>, p: Option<Pivot>) -> Result<crate::ops::GridOp, &'static str> {
        use crate::ops::{GridOp, Region};
        let mut state = self.pivot_state();
        let old = i.and_then(|i| self.pivot_shapes.get(i)).and_then(|s| s.area);
        let mut ops = Vec::new();
        if let Some(o) = old {
            ops.push(GridOp::Replace(Region { rect: o, values: Vec::new(), fills: Vec::new() }));
        }
        match p {
            Some(p) => {
                self.check_pivot(&p)?;
                if self.pivots.iter().enumerate().any(|(j, q)| Some(j) != i && q.name.eq_ignore_ascii_case(&p.name)) {
                    return Err("a pivot table of that name exists");
                }
                let out = self.pivot_output(&p)?;
                let new = out.shape.area.ok_or("the pivot table would be empty")?;
                for (j, s) in self.pivot_shapes.iter().enumerate() {
                    if Some(j) != i && s.area.is_some_and(|a| a.sheet == new.sheet && a.r0 <= new.r1 && new.r0 <= a.r1 && a.c0 <= new.c1 && new.c0 <= a.c1) {
                        return Err("it would overlap another pivot table");
                    }
                }
                let src = p.source;
                if src.sheet == new.sheet && src.r0 <= new.r1 && new.r0 <= src.r1 && src.c0 <= new.c1 && new.c0 <= src.c1 {
                    return Err("it would cover its own source");
                }
                for r in new.r0..=new.r1 {
                    for c in new.c0..=new.c1 {
                        let at = CellAddr::new(new.sheet, r, c);
                        if !old.is_some_and(|o| o.contains(at)) && (!self.get(at).is_empty() || self.formula_text(at).is_some()) {
                            return Err("there are cells in the way");
                        }
                    }
                }
                ops.push(GridOp::Replace(Region { rect: new, values: out.cells.into_iter().map(|(r, c, v)| (new.r0 + r, new.c0 + c, v)).collect(), fills: Vec::new() }));
                match i {
                    Some(i) => state[i] = (p, Some(new)),
                    None => state.push((p, Some(new))),
                }
            }
            None => {
                if let Some(i) = i {
                    state.remove(i);
                }
            }
        }
        if state.len() > MAX_PIVOTS {
            return Err("too many pivot tables");
        }
        ops.push(GridOp::Pivots(state));
        Ok(GridOp::Batch(ops))
    }

    /// A name for a new pivot: PivotTable1, PivotTable2…
    pub fn new_pivot_name(&self) -> String {
        (1..).map(|i| format!("PivotTable{i}")).find(|n| !self.pivots.iter().any(|p| p.name.eq_ignore_ascii_case(n))).unwrap()
    }

    /// Changes pivot `i`'s definition and refreshes it; on failure nothing changes.
    pub fn set_pivot(&mut self, i: usize, p: Pivot) -> Result<(), &'static str> {
        self.check_pivot(&p)?;
        let old = std::mem::replace(&mut self.pivots[i], p);
        if let Err(e) = self.refresh_pivot(i) {
            self.pivots[i] = old;
            return Err(e);
        }
        Ok(())
    }

    /// Removes pivot `i` and clears its output.
    pub fn remove_pivot(&mut self, i: usize) -> Option<Pivot> {
        if i >= self.pivots.len() {
            return None;
        }
        let shape = self.pivot_shapes.remove(i);
        if let Some(a) = shape.area {
            self.clear_values(a);
        }
        Some(self.pivots.remove(i))
    }

    fn clear_values(&mut self, a: Rect) {
        for r in a.r0..=a.r1 {
            for c in a.c0..=a.c1 {
                let at = CellAddr::new(a.sheet, r, c);
                if !self.get(at).is_empty() || self.formula_text(at).is_some() {
                    self.set_value(at, Value::Empty);
                }
            }
        }
    }

    /// Recomputes pivot `i` from its source and writes it into the sheet. Refused
    /// (nothing written) when the output would cover cells that hold something not
    /// its own, or another pivot.
    pub fn refresh_pivot(&mut self, i: usize) -> Result<(), &'static str> {
        let p = self.pivots[i].clone();
        let out = self.pivot_output(&p)?;
        let old = self.pivot_shapes[i].area;
        if let Some(new) = out.shape.area {
            for (j, s) in self.pivot_shapes.iter().enumerate() {
                if j != i && s.area.is_some_and(|a| a.sheet == new.sheet && a.r0 <= new.r1 && new.r0 <= a.r1 && a.c0 <= new.c1 && new.c0 <= a.c1) {
                    return Err("it would overlap another pivot table");
                }
            }
            let src = p.source;
            if src.sheet == new.sheet && src.r0 <= new.r1 && new.r0 <= src.r1 && src.c0 <= new.c1 && new.c0 <= src.c1 {
                return Err("it would cover its own source");
            }
            for r in new.r0..=new.r1 {
                for c in new.c0..=new.c1 {
                    let at = CellAddr::new(new.sheet, r, c);
                    if old.is_some_and(|o| o.contains(at)) {
                        continue;
                    }
                    if !self.get(at).is_empty() || self.formula_text(at).is_some() {
                        return Err("there are cells in the way");
                    }
                }
            }
        }
        if let Some(o) = old {
            self.clear_values(o);
        }
        if let Some(a) = out.shape.area {
            for (r, c, v) in out.cells {
                self.set_value(CellAddr::new(a.sheet, a.r0 + r, a.c0 + c), v);
            }
        }
        self.pivot_shapes[i] = out.shape;
        Ok(())
    }

    /// Refreshes every pivot; the names of those that couldn't be, with why.
    pub fn refresh_pivots(&mut self) -> Vec<(Arc<str>, &'static str)> {
        let mut bad = Vec::new();
        for i in 0..self.pivots.len() {
            if let Err(e) = self.refresh_pivot(i) {
                bad.push((self.pivots[i].name.clone(), e));
            }
        }
        if !bad.is_empty() || !self.pivots.is_empty() {
            self.recalc();
        }
        bad
    }

    /// The distinct items of a source column, in Excel's order.
    pub fn pivot_items(&self, p: &Pivot, field: u32) -> Vec<Value> {
        self.axis(p, field).map(|a| a.items).unwrap_or_default()
    }

    fn axis(&self, p: &Pivot, field: u32) -> Result<Axis, &'static str> {
        let s = &p.source;
        let mut seen: HashMap<Key, ()> = HashMap::new();
        let mut items = Vec::new();
        for r in s.r0 + 1..=s.r1 {
            let v = self.get(CellAddr::new(s.sheet, r, s.c0 + field));
            if seen.insert(key(&v), ()).is_none() {
                items.push(if matches!(&v, Value::Str(x) if x.is_empty()) { Value::Empty } else { v });
                if items.len() > MAX_ITEMS {
                    return Err("a field with too many items");
                }
            }
        }
        items.sort_by(item_order);
        Ok(Axis { items })
    }

    /// The pivot's output, computed, not written.
    pub fn pivot_output(&self, p: &Pivot) -> Result<Output, &'static str> {
        let s = &p.source;
        let fields: Vec<&PivotField> = p.rows.iter().chain(&p.cols).chain(&p.pages).collect();
        let mut axes: HashMap<u32, (Axis, HashMap<Key, u32>)> = HashMap::new();
        for f in &fields {
            let ax = self.axis(p, f.field)?;
            let hidden: Vec<Key> = f.hidden.iter().map(key).collect();
            let kept: Vec<Value> = ax.items.into_iter().filter(|v| !hidden.contains(&key(v))).collect();
            let idx = kept.iter().enumerate().map(|(i, v)| (key(v), i as u32)).collect();
            axes.insert(f.field, (Axis { items: kept }, idx));
        }
        let nd = p.data.len().max(1);
        let (k, m) = (p.rows.len(), p.cols.len());
        // Sums by leaf (full row path, full column path), one lookup a row; then
        // each leaf adds into every (row prefix, column prefix) it belongs to.
        type Path = Vec<u32>;
        let mut leaves: HashMap<Vec<u32>, Vec<Acc>> = HashMap::new();
        let mut keybuf: Vec<u32> = Vec::with_capacity(k + m);
        'rows: for r in s.r0 + 1..=s.r1 {
            let at = |f: u32| self.get(CellAddr::new(s.sheet, r, s.c0 + f));
            keybuf.clear();
            for f in &fields {
                let (_, idx) = &axes[&f.field];
                let Some(&i) = idx.get(&key(&at(f.field))) else { continue 'rows };
                keybuf.push(i);
            }
            keybuf.truncate(k + m);
            let e = match leaves.get_mut(&keybuf[..]) {
                Some(e) => e,
                None => leaves.entry(keybuf.clone()).or_insert_with(|| vec![Acc::default(); nd]),
            };
            for (x, d) in e.iter_mut().zip(&p.data) {
                x.add(&at(d.field));
            }
        }
        let mut acc: HashMap<(Path, Path), Vec<Acc>> = HashMap::new();
        let mut row_paths: HashMap<Path, ()> = HashMap::new();
        let mut col_paths: HashMap<Path, ()> = HashMap::new();
        for (leaf, sums) in &leaves {
            let (rp, cp) = leaf.split_at(k);
            for a in 0..=k {
                for b in 0..=m {
                    let e = acc.entry((rp[..a].to_vec(), cp[..b].to_vec())).or_insert_with(|| vec![Acc::default(); nd]);
                    for (x, y) in e.iter_mut().zip(sums) {
                        x.merge(y);
                    }
                }
            }
            row_paths.insert(rp.to_vec(), ());
            col_paths.insert(cp.to_vec(), ());
        }
        let mut row_paths: Vec<Path> = row_paths.into_keys().collect();
        row_paths.sort();
        let mut col_paths: Vec<Path> = col_paths.into_keys().collect();
        col_paths.sort();
        let items = |field: u32, i: u32| -> Value { label(&axes[&field].0.items[i as usize]) };

        // Rows of the body: (path, is a subtotal of that prefix).
        let mut body: Vec<(Path, bool)> = Vec::new();
        if k == 0 {
            body.push((Vec::new(), false));
        } else {
            for (n, path) in row_paths.iter().enumerate() {
                body.push((path.clone(), false));
                if p.subtotals {
                    // Subtotals for the outer fields whose group ends here.
                    let next = row_paths.get(n + 1);
                    for depth in (1..k).rev() {
                        if next.is_none_or(|q| q[..depth] != path[..depth]) {
                            body.push((path[..depth].to_vec(), true));
                        }
                    }
                }
            }
        }
        // Column heads: column items (with the value fields innermost when there
        // are several).
        let mut heads: Vec<ColHead> = Vec::new();
        if m == 0 {
            for d in 0..nd {
                heads.push(ColHead { path: Vec::new(), data: d, total: false });
            }
        } else {
            for (n, path) in col_paths.iter().enumerate() {
                for d in 0..nd {
                    heads.push(ColHead { path: path.clone(), data: d, total: false });
                }
                if p.subtotals {
                    let next = col_paths.get(n + 1);
                    for depth in (1..m).rev() {
                        if next.is_none_or(|q| q[..depth] != path[..depth]) {
                            for d in 0..nd {
                                heads.push(ColHead { path: path[..depth].to_vec(), data: d, total: true });
                            }
                        }
                    }
                }
            }
            if p.grand_col {
                for d in 0..nd {
                    heads.push(ColHead { path: Vec::new(), data: d, total: true });
                }
            }
        }
        let multi = p.data.len() > 1;
        // A header row for the column fields (and "Values") when there are any.
        let col_dims = m + multi as usize;
        let header_rows = if col_dims == 0 { 1 } else { 1 + col_dims } as u32;
        let label_cols = k.max(1) as u32;
        let page_rows = if p.pages.is_empty() { 0 } else { p.pages.len() as u32 + 1 };
        let rows_total = page_rows as u64 + header_rows as u64 + body.len() as u64 + (p.grand_row && k > 0) as u64;
        let cols_total = label_cols as u64 + heads.len() as u64;
        if rows_total * cols_total > MAX_OUTPUT {
            return Err("the pivot table would be too large");
        }
        if p.dest.row as u64 + rows_total > MAX_ROWS as u64 || p.dest.col as u64 + cols_total > MAX_COLS as u64 {
            return Err("the pivot table would run off the sheet");
        }
        let mut cells: Vec<(u32, u32, Value)> = Vec::new();
        let mut put = |r: u32, c: u32, v: Value| {
            if !v.is_empty() {
                cells.push((r, c, v));
            }
        };
        // Page fields: name and selection.
        for (i, f) in p.pages.iter().enumerate() {
            put(i as u32, 0, Value::str(&self.pivot_field_name(p, f.field)));
            let shown = &axes[&f.field].0.items;
            let sel = if f.hidden.is_empty() {
                Value::str("(All)")
            } else if shown.len() == 1 {
                label(&shown[0])
            } else {
                Value::str("(Multiple Items)")
            };
            put(i as u32, 1, sel);
        }
        let top = page_rows;
        let caption = |d: usize| -> String { p.data.get(d).map_or_else(|| "Count".to_string(), |x| self.data_caption(p, x)) };
        // Headers.
        if col_dims == 0 {
            for (j, f) in p.rows.iter().enumerate() {
                put(top, j as u32, Value::str(&self.pivot_field_name(p, f.field)));
            }
            if p.data.len() == 1 {
                put(top, label_cols, Value::str(&caption(0)));
            }
        } else {
            if !multi {
                put(top, 0, Value::str(&caption(0)));
            }
            for (j, f) in p.cols.iter().enumerate() {
                put(top, label_cols + j as u32, Value::str(&self.pivot_field_name(p, f.field)));
            }
            if multi {
                put(top, label_cols + m as u32, Value::str("Values"));
            }
            let last = top + header_rows - 1;
            for (j, f) in p.rows.iter().enumerate() {
                put(last, j as u32, Value::str(&self.pivot_field_name(p, f.field)));
            }
            let mut prev: Option<&ColHead> = None;
            for (x, h) in heads.iter().enumerate() {
                let c = label_cols + x as u32;
                if h.total {
                    // "<item> Total" (or "Grand Total") on the row of its depth.
                    let depth = h.path.len();
                    let base = if depth == 0 { "Grand Total".to_string() } else { format!("{} Total", label_text(&items(p.cols[depth - 1].field, h.path[depth - 1]))) };
                    let text = if multi { format!("{} {}", if depth == 0 { "Total".to_string() } else { base.clone() }, caption(h.data)) } else { base };
                    put(top + 1 + depth.saturating_sub(1) as u32, c, Value::str(&text));
                    prev = Some(h);
                    continue;
                }
                for (lvl, &it) in h.path.iter().enumerate() {
                    let new = prev.is_none_or(|q| q.total || q.path.len() <= lvl || q.path[..=lvl] != h.path[..=lvl]);
                    if new {
                        put(top + 1 + lvl as u32, c, items(p.cols[lvl].field, it));
                    }
                }
                if multi {
                    put(top + 1 + m as u32, c, Value::str(&caption(h.data)));
                }
                prev = Some(h);
            }
        }
        // Body.
        let first = top + header_rows;
        let mut total_rows = Vec::new();
        let mut prev: Option<&Path> = None;
        let value = |rp: &[u32], h: &ColHead| -> Value {
            match acc.get(&(rp.to_vec(), h.path.clone())) {
                Some(a) => match p.data.get(h.data) {
                    Some(d) => a[h.data].result(d.agg),
                    None => a[0].result(Agg::Count),
                },
                None => Value::Empty,
            }
        };
        let mut row = first;
        for (path, sub) in &body {
            if *sub {
                let d = path.len();
                put(row, d as u32 - 1, Value::str(&format!("{} Total", label_text(&items(p.rows[d - 1].field, path[d - 1])))));
                total_rows.push(row - first);
            } else {
                for (lvl, &it) in path.iter().enumerate() {
                    let new = prev.is_none_or(|q| q[..=lvl] != path[..=lvl]);
                    if new {
                        put(row, lvl as u32, items(p.rows[lvl].field, it));
                    }
                }
                if k == 0 && col_dims > 0 {
                    put(row, 0, Value::str(if multi { "" } else { "Total" }));
                }
                prev = Some(path);
            }
            for (x, h) in heads.iter().enumerate() {
                put(row, label_cols + x as u32, value(path, h));
            }
            row += 1;
        }
        if p.grand_row && k > 0 {
            put(row, 0, Value::str("Grand Total"));
            total_rows.push(row - first);
            for (x, h) in heads.iter().enumerate() {
                put(row, label_cols + x as u32, value(&[], h));
            }
        }
        let total_cols = heads.iter().enumerate().filter(|(_, h)| h.total).map(|(x, _)| label_cols + x as u32).collect();
        // Shape rows are counted from the table's top (under the page fields).
        let shape = Shape {
            area: Some(Rect { sheet: p.dest.sheet, r0: p.dest.row, c0: p.dest.col, r1: p.dest.row + rows_total as u32 - 1, c1: p.dest.col + cols_total as u32 - 1 }),
            page_rows,
            header_rows,
            label_cols,
            total_rows,
            total_cols,
        };
        Ok(Output { cells, rows: rows_total as u32, cols: cols_total as u32, shape })
    }
}

// ---------------------------------------------------------------------------
// Native file.

fn put_value(w: &mut Writer, v: &Value) {
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
        Value::Bool(b) => w.u8(3 + *b as u8),
        Value::Err(e) => {
            w.u8(5);
            w.u8(*e as u8);
        }
    }
}

fn get_value(r: &mut Reader) -> Result<Value, WireError> {
    Ok(match r.u8()? {
        0 => Value::Empty,
        1 => Value::Num(r.f64()?),
        2 => Value::str(r.str("pivot item", 32_767)?),
        3 => Value::Bool(false),
        4 => Value::Bool(true),
        5 => Value::Err(ErrorKind::from_u8(r.u8()?).ok_or(WireError::Invalid("pivot item"))?),
        _ => return Err(WireError::Invalid("pivot item")),
    })
}

fn put_rect(w: &mut Writer, r: &Rect) {
    w.u16(r.sheet);
    for v in [r.r0, r.c0, r.r1, r.c1] {
        w.var(v as u64);
    }
}

fn get_rect(r: &mut Reader) -> Result<Rect, WireError> {
    let sheet = r.u16()?;
    let mut v = [0u32; 4];
    for (i, x) in v.iter_mut().enumerate() {
        let y = r.var()?;
        if y >= if i % 2 == 0 { MAX_ROWS } else { MAX_COLS } as u64 {
            return Err(WireError::Invalid("pivot range"));
        }
        *x = y as u32;
    }
    Ok(Rect { sheet, r0: v[0], c0: v[1], r1: v[2], c1: v[3] })
}

impl Pivot {
    /// The definition and where its output is (`area`, the shape's).
    pub fn encode(&self, w: &mut Writer, area: Option<Rect>) {
        w.str(&self.name);
        put_rect(w, &self.source);
        w.u16(self.dest.sheet);
        w.var(self.dest.row as u64);
        w.var(self.dest.col as u64);
        w.u8(self.grand_row as u8 | (self.grand_col as u8) << 1 | (self.subtotals as u8) << 2);
        for axis in [&self.rows, &self.cols, &self.pages] {
            w.var(axis.len() as u64);
            for f in axis {
                w.var(f.field as u64);
                w.var(f.hidden.len() as u64);
                for v in &f.hidden {
                    put_value(w, v);
                }
            }
        }
        w.var(self.data.len() as u64);
        for d in &self.data {
            w.var(d.field as u64);
            w.u8(Agg::ALL.iter().position(|a| *a == d.agg).unwrap_or(0) as u8);
            w.str(d.name.as_deref().unwrap_or(""));
        }
        match area {
            Some(a) => {
                w.u8(1);
                put_rect(w, &a);
            }
            None => w.u8(0),
        }
    }

    pub fn decode(r: &mut Reader) -> Result<(Pivot, Option<Rect>), WireError> {
        let name = Arc::from(r.str("pivot name", 255)?);
        let source = get_rect(r)?;
        let sheet = r.u16()?;
        let (row, col) = (r.var()?, r.var()?);
        if row >= MAX_ROWS as u64 || col >= MAX_COLS as u64 {
            return Err(WireError::Invalid("pivot place"));
        }
        let flags = r.u8()?;
        let mut axes = Vec::new();
        for _ in 0..3 {
            let n = r.count("pivot fields", MAX_FIELDS as u64, 2)?;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                let field = r.var()?.min(MAX_COLS as u64) as u32;
                let h = r.count("hidden items", MAX_ITEMS as u64, 1)?;
                let mut hidden = Vec::with_capacity(h.min(1024));
                for _ in 0..h {
                    hidden.push(get_value(r)?);
                }
                v.push(PivotField { field, hidden });
            }
            axes.push(v);
        }
        let n = r.count("value fields", MAX_FIELDS as u64, 3)?;
        let mut data = Vec::with_capacity(n);
        for _ in 0..n {
            let field = r.var()?.min(MAX_COLS as u64) as u32;
            let agg = *Agg::ALL.get(r.u8()? as usize).ok_or(WireError::Invalid("pivot function"))?;
            let nm = r.str("value field name", 255)?;
            data.push(DataField { field, agg, name: (!nm.is_empty()).then(|| Arc::from(nm)) });
        }
        let area = match r.u8()? {
            0 => None,
            _ => Some(get_rect(r)?),
        };
        let pages = axes.pop().unwrap();
        let cols = axes.pop().unwrap();
        let rows = axes.pop().unwrap();
        Ok((Pivot { name, source, dest: CellAddr::new(sheet, row as u32, col as u32), rows, cols, pages, data, grand_row: flags & 1 != 0, grand_col: flags & 2 != 0, subtotals: flags & 4 != 0 }, area))
    }
}
