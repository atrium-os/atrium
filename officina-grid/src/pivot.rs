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

mod calculate;
mod display;
mod group;
mod order;
mod slicer;

pub use calculate::CalculatedField;
pub use display::{BaseItem, ReferenceCalc, ShowAs};
pub use group::{date_part_labels, group_bounds, DatePart, GroupField, GroupKind, ItemGroup};
use display::{Display, Slot};
use group::Grouping;
pub use order::{AxisSort, ScopeItems, MAX_SCOPE_ITEMS};
pub use slicer::{in_layout, slicer_name, PivotFilter, Slicer, MAX_SLICERS};
use order::{Order, SortKey};

pub const MAX_GROUPS: usize = 64;
pub const MAX_GROUP_MEMBERS: usize = 65_536;

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
    Auto,
}

impl Agg {
    pub const ALL: [Agg; 12] = [Agg::Sum, Agg::Count, Agg::Average, Agg::Max, Agg::Min, Agg::Product, Agg::CountNums, Agg::StdDev, Agg::StdDevP, Agg::Var, Agg::VarP, Agg::Auto];

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
            Agg::Auto => "Automatic",
        }
    }
}

/// Per-field subtotal placement. Default follows the pivot-wide setting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Subtotal {
    #[default]
    Default,
    Off,
    Top,
    Bottom,
}

impl Subtotal {
    const ALL: [Self; 4] = [Self::Default, Self::Off, Self::Top, Self::Bottom];
}

/// A source column on an axis, with the items left out.
#[derive(Debug, Clone, PartialEq)]
pub struct PivotField {
    /// Column of the source, from its first column.
    pub field: u32,
    /// Items not shown (and not counted), as their values.
    pub hidden: Vec<Value>,
    pub sort: AxisSort,
    pub outline: bool,
    pub subtotal: Subtotal,
}

impl PivotField {
    pub fn new(field: u32) -> Self {
        PivotField { field, hidden: Vec::new(), sort: AxisSort::Default, outline: false, subtotal: Subtotal::Default }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct DataField {
    pub field: u32,
    pub agg: Agg,
    /// Its caption; "Sum of <field>" when `None`.
    pub name: Option<Arc<str>>,
    pub show_as: ShowAs,
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
    pub groups: Vec<GroupField>,
    pub calculations: Vec<CalculatedField>,
    /// The Grand Total row at the bottom.
    pub grand_row: bool,
    /// The Grand Total column at the right.
    pub grand_col: bool,
    /// Subtotals for outer row and column fields.
    pub subtotals: bool,
    /// Excel's compact form: the row fields share one column, inner items
    /// indented under their outer item, whose row holds its subtotal (else the
    /// tabular form, a column per row field and subtotal rows under each group).
    pub compact: bool,
    /// The implicit Values field is the innermost row field.
    pub values_on_rows: bool,
    /// Filters that don't show in the cells: slicers' fields out of the
    /// layout, and timelines' date ranges.
    pub filters: Vec<PivotFilter>,
    pub slicers: Vec<Slicer>,
}

impl Pivot {
    pub fn subtotal_at(&self, field: &PivotField, row: bool) -> Subtotal {
        if !self.subtotals { return Subtotal::Off; }
        match field.subtotal {
            Subtotal::Default if row && (self.compact || field.outline) => Subtotal::Top,
            Subtotal::Default => Subtotal::Bottom,
            Subtotal::Top if !row => Subtotal::Bottom,
            subtotal => subtotal,
        }
    }

    /// Validate group dependencies before resolving their members.
    pub fn check_groups(&self) -> Result<(), &'static str> {
        let width = self.source.c1.checked_sub(self.source.c0).ok_or("an invalid pivot source")? + 1;
        if self.groups.len() > MAX_GROUPS {
            return Err("too many grouped fields");
        }
        let mut grouped = Vec::new();
        for group in &self.groups {
            if !group.valid(width) || grouped.contains(&group.field) {
                return Err("an invalid grouped field");
            }
            grouped.push(group.field);
        }
        for group in &self.groups {
            let mut source = group.source;
            for _ in 0..self.groups.len() {
                if source < width { break; }
                source = self.groups.iter().find(|group| group.field == source)
                    .ok_or("a grouped field has no source")?.source;
            }
            if source >= width {
                return Err("a pivot group depends on itself");
            }
        }
        Ok(())
    }

    pub fn new(name: &str, source: Rect, dest: CellAddr) -> Self {
        Pivot { name: Arc::from(name), source, dest, rows: Vec::new(), cols: Vec::new(), pages: Vec::new(), data: Vec::new(), groups: Vec::new(), calculations: Vec::new(), grand_row: true, grand_col: true, subtotals: true, compact: false, values_on_rows: false, filters: Vec::new(), slicers: Vec::new() }
    }

    /// Swap value fields without changing the measure referenced by a sort.
    pub fn swap_data_fields(&mut self, a: usize, b: usize) {
        self.data.swap(a, b);
        for field in self.rows.iter_mut().chain(&mut self.cols) {
            if let Some(data) = field.sort.scoped_measure_mut() {
                if *data as usize == a {
                    *data = b as u32;
                } else if *data as usize == b {
                    *data = a as u32;
                }
            }
        }
    }

    /// Removing a measure also removes sorts that used it.
    pub fn remove_data_field(&mut self, index: usize) {
        self.data.remove(index);
        for field in self.rows.iter_mut().chain(&mut self.cols) {
            if let Some(data) = field.sort.scoped_measure_mut() {
                match (*data as usize).cmp(&index) {
                    Ordering::Equal => field.sort = AxisSort::Default,
                    Ordering::Greater => *data -= 1,
                    Ordering::Less => {}
                }
            }
        }
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
    /// Compact form: body rows whose label is indented, with its level (1 for
    /// the second row field, …), and whether the row is an outer item's (a group).
    pub indents: Vec<(u32, u8, bool)>,
}

impl Shape {
    /// The table without the page fields (Excel's pivot "location").
    pub fn table(&self) -> Option<Rect> {
        self.area.map(|a| Rect { r0: a.r0 + self.page_rows, ..a })
    }

    /// A compact pivot's label cell: how far it is indented and whether it is an
    /// outer item's row; `(0, false)` elsewhere.
    pub fn indent(&self, a: CellAddr) -> (u8, bool) {
        let Some(area) = self.area else { return (0, false) };
        if !area.contains(a) || a.col != area.c0 || self.indents.is_empty() {
            return (0, false);
        }
        let Some(r) = (a.row - area.r0).checked_sub(self.page_rows + self.header_rows) else { return (0, false) };
        match self.indents.binary_search_by_key(&r, |x| x.0) {
            Ok(i) => (self.indents[i].1, self.indents[i].2),
            Err(_) => (0, false),
        }
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

/// A PivotChart's series for a pivot's output: one per value column (the
/// grand total column left out), named by its header, over the body rows up to
/// the last that isn't a total (subtotal rows inside stay points), with the
/// innermost row labels as categories.
pub fn pivot_chart_series(shape: &Shape, compact: bool) -> Vec<crate::charts::Series> {
    use crate::charts::{ChartText, Series, MAX_SERIES};
    let Some(a) = shape.area else { return Vec::new() };
    let body = a.r0 + shape.page_rows + shape.header_rows;
    if body > a.r1 {
        return Vec::new();
    }
    let Some(last) = (body..=a.r1).rev().find(|r| !shape.total_rows.contains(&(r - body))) else { return Vec::new() };
    let label = if compact || shape.label_cols == 0 { a.c0 } else { a.c0 + shape.label_cols - 1 };
    let cats = (shape.label_cols > 0).then_some(Rect { sheet: a.sheet, r0: body, c0: label, r1: last, c1: label });
    let head = body.checked_sub(1).filter(|h| *h >= a.r0 + shape.page_rows);
    (a.c0 + shape.label_cols..=a.c1)
        .filter(|c| !shape.total_cols.contains(&(c - a.c0)))
        .take(MAX_SERIES)
        .map(|c| Series {
            name: head.map(|h| ChartText::Cell(CellAddr::new(a.sheet, h, c))),
            cats,
            vals: Rect { sheet: a.sheet, r0: body, c0: c, r1: last, c1: c },
            kind: None,
            look: Default::default(),
        })
        .collect()
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
pub fn item_key(value: &Value) -> impl Eq + std::hash::Hash + use<> {
    key(value)
}

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

fn order_manual(items: &mut [Value], prior: &[Value]) {
    let mut position = HashMap::new();
    for (i, item) in prior.iter().enumerate() {
        position.entry(key(item)).or_insert(i);
    }
    items.sort_by_cached_key(|item| position.get(&key(item)).copied().unwrap_or(usize::MAX));
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
            Agg::Auto => unreachable!("automatic aggregates resolve per source field"),
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum RowKind { Detail, Heading, Top, Bottom, Grand }

/// The computed output: cells relative to the area's top-left, and its shape.
pub struct Output {
    pub cells: Vec<(u32, u32, Value)>,
    pub rows: u32,
    pub cols: u32,
    percent_ranges: Vec<Rect>,
    percent_by_row: bool,
    pub shape: Shape,
}

impl Output {
    fn percent_styles(&self, wb: &Workbook) -> Vec<crate::ops::GridOp> {
        use crate::ops::{GridOp, StyleRegion};
        use crate::style::Style;

        let percent: Arc<str> = Arc::from("0.00%");
        let style_percent = |style: &Style| {
            let mut style = style.clone();
            if style.num_fmt.eq_ignore_ascii_case("General") {
                style.num_fmt = percent.clone();
            }
            style
        };
        if !self.percent_by_row {
            return self.percent_ranges.iter().map(|rect| GridOp::restyle(wb, *rect, style_percent)).collect();
        }

        let (Some(&first), Some(last)) = (self.percent_ranges.first(), self.percent_ranges.last()) else {
            return Vec::new();
        };
        // Use one journal operation even when the Values axis has many measure rows.
        let rect = Rect { r1: last.r1, ..first };
        let sheet = wb.sheet(rect.sheet);
        let mut spans = Vec::new();
        for col in rect.c0..=rect.c1 {
            let parts = sheet.styles.get(col as usize).map_or_else(
                || vec![(rect.r0, rect.r1, 0)],
                |runs| runs.spans(rect.r0, rect.r1),
            );
            for (start, end, id) in parts {
                let original = wb.styles.get(id);
                if !original.num_fmt.eq_ignore_ascii_case("General") {
                    spans.push((col, start, end, original.clone()));
                    continue;
                }
                let first = self.percent_ranges.partition_point(|range| range.r0 < start);
                let last = self.percent_ranges.partition_point(|range| range.r0 <= end);
                let mut next = start;
                let styled = style_percent(original);
                for range in &self.percent_ranges[first..last] {
                    if id != 0 && next < range.r0 {
                        spans.push((col, next, range.r0 - 1, original.clone()));
                    }
                    spans.push((col, range.r0, range.r0, styled.clone()));
                    next = range.r0 + 1;
                }
                if id != 0 && next <= end {
                    spans.push((col, next, end, original.clone()));
                }
            }
        }
        vec![GridOp::Restyle(StyleRegion { rect, spans })]
    }
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

    /// A source column's name, or a grouped field's own name.
    pub fn pivot_field_name(&self, p: &Pivot, field: u32) -> String {
        if field >= MAX_COLS {
            return p.groups.iter().find(|group| group.field == field).map(|group| &group.name)
                .or_else(|| p.calculations.iter().find(|calc| calc.field == field).map(|calc| &calc.name))
                .map_or_else(String::new, |name| name.to_string());
        }
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

    /// Resolves a sort value field by its saved caption or unambiguous source name.
    pub fn pivot_sort_value(&self, p: &Pivot, caption: &str) -> Option<(usize, Agg)> {
        if let Some((i, field)) = p.data.iter().enumerate()
            .find(|(_, field)| self.data_caption(p, field).eq_ignore_ascii_case(caption))
        {
            return Some((i, field.agg));
        }
        let mut matches = p.data.iter().enumerate().filter(|(_, field)| {
            let name = self.pivot_field_name(p, field.field);
            name.eq_ignore_ascii_case(caption)
                || (field.name.is_none() && format!("{} - {name}", field.agg.label()).eq_ignore_ascii_case(caption))
        });
        let (i, field) = matches.next()?;
        matches.next().is_none().then_some((i, field.agg))
    }

    pub fn check_pivot_calculations(&self, p: &Pivot) -> Result<(), &'static str> {
        if p.groups.len() + p.calculations.len() > MAX_GROUPS {
            return Err("too many derived pivot fields");
        }
        let mut ids = std::collections::HashSet::new();
        for field in &p.calculations {
            if !ids.insert(field.field) || p.groups.iter().any(|group| group.field == field.field)
                || !(MAX_COLS..MAX_COLS + MAX_GROUPS as u32).contains(&field.field)
                || field.name.is_empty() || field.name.len() > 255
            {
                return Err("an invalid calculated pivot field");
            }
        }
        calculate::Plan::new(self, p).map(|_| ())
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
        p.check_groups()?;
        let grouped: Vec<_> = p.groups.iter().map(|group| group.field).collect();
        let mut seen = Vec::new();
        for f in all {
            if f.field >= width && !grouped.contains(&f.field) {
                return Err("a field outside the source");
            }
            if !f.sort.valid() {
                return Err("invalid pivot sort settings");
            }
            if seen.contains(&f.field) {
                return Err("a field on two axes");
            }
            seen.push(f.field);
        }
        self.check_pivot_calculations(p)?;
        p.check_filters(width, self.sheet_count())?;
        if p.data.iter().any(|d| d.field >= width
            && !(d.agg == Agg::Sum && p.calculations.iter().any(|field| field.field == d.field)))
        {
            return Err("an invalid pivot value field");
        }
        if p.data.iter().filter_map(|d| d.show_as.base_field()).any(|field| {
            !p.rows.iter().chain(&p.cols).any(|f| f.field == field)
        }) {
            return Err("a display calculation needs its base field in Rows or Columns");
        }
        if p.pages.iter().any(|field| field.sort.scoped_measure().is_some()) {
            return Err("a pivot value sort needs a row or column field");
        }
        for (axis, other) in [(&p.rows, &p.cols), (&p.cols, &p.rows)] {
            for field in axis {
                let sort = &field.sort;
                if let Some(index) = sort.scoped_measure() {
                    let Some(measure) = p.data.get(index as usize).filter(|_| sort.matches_axis(other)) else {
                        return Err("a pivot value sort needs its measure and selected members on the opposite axis");
                    };
                    if matches!(sort, AxisSort::Selection { .. }) && p.calculations.iter().any(|field| field.field == measure.field) {
                        return Err("a multi-item or positional sort needs an ordinary measure");
                    }
                    if sort.uses_positions() && other.iter().any(|field| field.sort.uses_values()) {
                        return Err("a positional sort needs a label or manual order on the opposite axis");
                    }
                }
            }
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
            shape.indents.clear();
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
        let mut state = self.pivot_state();
        let old = i.and_then(|i| state.get(i)).and_then(|s| s.1);
        let mut ops = self.plan_pivot(&mut state, i, p)?;
        // PivotCharts: charts drawn wholly from the old output follow the new one.
        if let (Some(old), Some(i)) = (old, i) {
            if let Some((p, Some(_))) = state.get(i) {
                let follows = |c: &crate::charts::Chart| !c.series.is_empty() && c.series.iter().all(|s| old.contains_rect(&s.vals) && s.cats.is_none_or(|r| old.contains_rect(&r)));
                if self.charts().iter().any(follows) {
                    if let Ok(out) = self.pivot_output(p) {
                        let fresh = pivot_chart_series(&out.shape, p.compact);
                        if !fresh.is_empty() {
                            let charts = self.charts().iter().map(|c| {
                                if !follows(c) {
                                    return c.clone();
                                }
                                let mut c = c.clone();
                                c.series = fresh.iter().enumerate().map(|(k, s)| {
                                    let was = c.series.get(k).or(c.series.last());
                                    crate::charts::Series { look: was.map(|w| w.look.clone()).unwrap_or_default(), kind: was.and_then(|w| w.kind), ..s.clone() }
                                }).collect();
                                c
                            }).collect();
                            ops.push(crate::ops::GridOp::Charts(charts));
                        }
                    }
                }
            }
        }
        ops.push(crate::ops::GridOp::Pivots(state));
        Ok(crate::ops::GridOp::Batch(ops))
    }

    /// Plans output edits against current cells and updates a candidate collection.
    pub(crate) fn plan_pivot(
        &self, state: &mut Vec<(Pivot, Option<Rect>)>, i: Option<usize>, p: Option<Pivot>,
    ) -> Result<Vec<crate::ops::GridOp>, &'static str> {
        use crate::ops::{GridOp, Region};
        if i.is_some_and(|i| i >= state.len()) {
            return Err("no such pivot table");
        }
        let old = i.and_then(|i| state[i].1);
        let mut ops = Vec::new();
        if let Some(o) = old {
            ops.push(GridOp::Replace(Region { rect: o, values: Vec::new(), fills: Vec::new() }));
        }
        match p {
            Some(p) => {
                self.check_pivot(&p)?;
                if state.iter().enumerate().any(|(j, (q, _))| Some(j) != i && q.name.eq_ignore_ascii_case(&p.name)) {
                    return Err("a pivot table of that name exists");
                }
                self.check_pivot_refresh(&p)?;
                let out = self.pivot_output(&p)?;
                let new = out.shape.area.ok_or("the pivot table would be empty")?;
                for (j, (_, area)) in state.iter().enumerate() {
                    if Some(j) != i && area.is_some_and(|a| a.intersects(&new)) {
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
                ops.extend(out.percent_styles(self));
                ops.push(GridOp::Replace(Region { rect: new, values: out.cells.into_iter().map(|(r, c, v)| (new.r0 + r, new.c0 + c, v)).collect(), fills: Vec::new() }));
                match i {
                    Some(i) => state[i] = (p, Some(new)),
                    None if state.len() < MAX_PIVOTS => state.push((p, Some(new))),
                    None => return Err("too many pivot tables"),
                }
            }
            None => {
                if let Some(i) = i {
                    state.remove(i);
                }
            }
        }
        Ok(ops)
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
        self.check_pivot_refresh(&p)?;
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
        let styles = out.percent_styles(self);
        for style in styles {
            use officina_core::history::Op;
            style.apply(self);
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

    /// Items of a source or grouped column in field order before aggregate sorting.
    pub fn pivot_items(&self, p: &Pivot, field: u32) -> Vec<Value> {
        if p.check_groups().is_err() { return Vec::new(); }
        let grouping = p.groups.iter().find(|group| group.field == field)
            .map(|group| Grouping::new(self, p, group));
        let sort = p.rows.iter().chain(&p.cols).chain(&p.pages).find(|f| f.field == field)
            .map_or(&AxisSort::Default, |f| &f.sort);
        self.axis(p, field, grouping.as_ref(), sort).map(|a| a.items).unwrap_or_default()
    }

    /// A date-part or interval grouped field's every possible item in order,
    /// with the start and end its grouping uses (see `Grouping::domain`).
    pub fn pivot_group_domain(&self, p: &Pivot, field: u32) -> Option<(f64, f64, Vec<Value>)> {
        p.check_groups().ok()?;
        let group = p.groups.iter().find(|group| group.field == field)?;
        Grouping::new(self, p, group).domain(self.date_system())
    }

    /// Ungrouped cache items for a physical source column.
    pub fn pivot_source_items(&self, p: &Pivot, field: u32) -> Vec<Value> {
        self.axis(p, field, None, &AxisSort::Default).map(|axis| axis.items).unwrap_or_default()
    }

    /// Items to serialize, including saved manual positions for absent members.
    pub fn pivot_export_items(&self, p: &Pivot, field: &PivotField) -> Vec<Value> {
        let mut items = self.pivot_items(p, field.field);
        if let AxisSort::Manual(prior) = &field.sort {
            order::add_missing(&mut items, prior);
            order_manual(&mut items, prior);
        }
        items
    }

    fn pivot_item(&self, p: &Pivot, row: u32, field: u32, grouping: Option<&Grouping>) -> (Value, Option<f64>) {
        grouping.map_or_else(
            || (self.get(CellAddr::new(p.source.sheet, row, p.source.c0 + field)), None),
            |group| group.item(self, p, row),
        )
    }

    fn axis(&self, p: &Pivot, field: u32, grouping: Option<&Grouping>, sort: &AxisSort) -> Result<Axis, &'static str> {
        let s = &p.source;
        let mut seen: HashMap<Key, ()> = HashMap::new();
        let mut items = Vec::new();
        for r in s.r0 + 1..=s.r1 {
            let (v, order) = self.pivot_item(p, r, field, grouping);
            if seen.insert(key(&v), ()).is_none() {
                let item = if matches!(&v, Value::Str(x) if x.is_empty()) { Value::Empty } else { v };
                items.push((item, order));
                if items.len() > MAX_ITEMS {
                    return Err("a field with too many items");
                }
            }
        }
        if *sort != AxisSort::Source {
            items.sort_by(|(a, ak), (b, bk)| {
                let order = match (ak, bk) {
                    (Some(a), Some(b)) => a.total_cmp(b),
                    (Some(_), None) => Ordering::Less,
                    (None, Some(_)) => Ordering::Greater,
                    _ => item_order(a, b),
                };
                if matches!(sort, AxisSort::Name { descending: true }) { order.reverse() } else { order }
            });
        }
        let mut items: Vec<_> = items.into_iter().map(|(item, _)| item).collect();
        if let AxisSort::Manual(prior) = sort {
            order_manual(&mut items, prior);
        }
        Ok(Axis { items })
    }

    /// Calc's automatic aggregate uses Count when the source contains nonnumeric values.
    pub fn pivot_auto_agg(&self, p: &Pivot, field: u32) -> Agg {
        if p.source.r0 == p.source.r1 { return Agg::Sum; }
        let col = p.source.c0 + field;
        let range = Rect { r0: p.source.r0 + 1, c0: col, c1: col, ..p.source };
        let mut nonnumeric = false;
        self.each(&range, &mut |_, _, v| nonnumeric |= !matches!(v, Value::Num(_) | Value::Empty));
        if nonnumeric { Agg::Count } else { Agg::Sum }
    }

    fn check_pivot_refresh(&self, p: &Pivot) -> Result<(), &'static str> {
        let s = &p.source;
        for group in &p.groups {
            if matches!(group.kind, GroupKind::NumericBins { .. }) {
                let col = s.c0 + group.source;
                if group.source > s.c1 - s.c0 || (s.r0 + 1..=s.r1).any(|row| {
                    !matches!(self.get(CellAddr::new(s.sheet, row, col)), Value::Num(n) if n.is_finite())
                }) {
                    return Err("numeric pivot bins require a numeric source column");
                }
            }
        }
        Ok(())
    }

    /// The pivot's output, computed, not written.
    pub fn pivot_output(&self, p: &Pivot) -> Result<Output, &'static str> {
        p.check_groups()?;
        let resolved = p.data.iter().any(|field| field.agg == Agg::Auto).then(|| {
            let mut pivot = p.clone();
            for field in &mut pivot.data {
                if field.agg == Agg::Auto {
                    // An explicit caption and the saved sort-by-measure caption remain addressable.
                    if field.name.is_none() { field.name = Some(Arc::from(self.data_caption(p, field))); }
                    field.agg = self.pivot_auto_agg(p, field.field);
                }
            }
            pivot
        });
        let p = resolved.as_ref().unwrap_or(p);
        let s = &p.source;
        // Layout fields, then the filters' fields that aren't in it: a row
        // counts only when each of its items is kept.
        let filter_fields: Vec<PivotField> = p.filter_only().map(|f| PivotField { hidden: f.hidden.clone(), ..PivotField::new(f.field) }).collect();
        let fields: Vec<&PivotField> = p.rows.iter().chain(&p.cols).chain(&p.pages).chain(&filter_fields).collect();
        let ranges: Vec<(u32, f64, f64)> = p.filters.iter().filter_map(|f| f.between.map(|(a, b)| (f.field, a, b))).collect();
        let groups: HashMap<_, _> = p.groups.iter()
            .filter(|group| fields.iter().any(|f| f.field == group.field))
            .map(|group| (group.field, Grouping::new(self, p, group))).collect();
        let mut axes: HashMap<u32, (Axis, HashMap<Key, u32>)> = HashMap::new();
        for f in &fields {
            let ax = self.axis(p, f.field, groups.get(&f.field), &f.sort)?;
            let mut hidden: Vec<Key> = f.hidden.iter().map(key).collect();
            // A layout field that also has a filter (one moved into the
            // layout after a slicer filtered it): both hide.
            if in_layout(p, f.field) {
                if let Some(extra) = p.filters.iter().find(|x| x.field == f.field) {
                    hidden.extend(extra.hidden.iter().map(key));
                }
            }
            let kept: Vec<Value> = ax.items.into_iter().filter(|v| !hidden.contains(&key(v))).collect();
            let idx = kept.iter().enumerate().map(|(i, v)| (key(v), i as u32)).collect();
            axes.insert(f.field, (Axis { items: kept }, idx));
        }
        let nd = p.data.len().max(1);
        let calc = calculate::Plan::new(self, p)?;
        let accumulators = nd + calc.source.len();
        let (k, m) = (p.rows.len(), p.cols.len());
        // Sums by leaf (full row path, full column path), one lookup a row; then
        // each leaf adds into every (row prefix, column prefix) it belongs to.
        type Path = Vec<u32>;
        let mut leaves: HashMap<Vec<u32>, Vec<Acc>> = HashMap::new();
        let mut keybuf: Vec<u32> = Vec::with_capacity(k + m);
        'rows: for r in s.r0 + 1..=s.r1 {
            let at = |f: u32| self.get(CellAddr::new(s.sheet, r, s.c0 + f));
            keybuf.clear();
            for &(f, a, b) in &ranges {
                match at(f) {
                    Value::Num(x) if x >= a && x < b => {}
                    _ => continue 'rows,
                }
            }
            for f in &fields {
                let (_, idx) = &axes[&f.field];
                let (item, _) = self.pivot_item(p, r, f.field, groups.get(&f.field));
                let Some(&i) = idx.get(&key(&item)) else { continue 'rows };
                keybuf.push(i);
            }
            keybuf.truncate(k + m);
            let e = match leaves.get_mut(&keybuf[..]) {
                Some(e) => e,
                None => leaves.entry(keybuf.clone()).or_insert_with(|| vec![Acc::default(); accumulators]),
            };
            for (x, d) in e.iter_mut().zip(&p.data) {
                if d.field < MAX_COLS {
                    x.add(&at(d.field));
                }
            }
            for (acc, field) in e[nd..].iter_mut().zip(&calc.source) {
                acc.add(&at(*field));
            }
        }
        let mut acc: HashMap<(Path, Path), Vec<Acc>> = HashMap::new();
        let mut row_paths: HashMap<Path, ()> = HashMap::new();
        let mut col_paths: HashMap<Path, ()> = HashMap::new();
        for (leaf, sums) in &leaves {
            let (rp, cp) = leaf.split_at(k);
            for a in 0..=k {
                for b in 0..=m {
                    let e = acc.entry((rp[..a].to_vec(), cp[..b].to_vec())).or_insert_with(|| vec![Acc::default(); accumulators]);
                    for (x, y) in e.iter_mut().zip(sums) {
                        x.merge(y);
                    }
                }
            }
            row_paths.insert(rp.to_vec(), ());
            col_paths.insert(cp.to_vec(), ());
        }
        if !p.calculations.is_empty() {
            for totals in acc.values_mut() {
                calc.apply(p, totals);
            }
        }
        let mut row_paths: Vec<Path> = row_paths.into_keys().collect();
        row_paths.sort();
        let mut col_paths: Vec<Path> = col_paths.into_keys().collect();
        col_paths.sort();
        let resolve = |sort: &AxisSort, column: bool| -> Option<SortKey> {
            let (data, agg, descending, opposite) = match sort {
                AxisSort::Data { caption, descending } => {
                    let (data, agg) = self.pivot_sort_value(p, caption)?;
                    (data, agg, *descending, vec![Vec::new()])
                }
                AxisSort::Scoped { data, descending, members } => {
                    let data = *data as usize;
                    let agg = p.data.get(data)?.agg;
                    let opposite = members.iter().map(|(field, item)| {
                        axes.get(field)?.1.get(&key(item)).copied()
                    }).collect::<Option<Vec<_>>>()?;
                    (data, agg, *descending, vec![opposite])
                }
                AxisSort::Selection { data, descending, fields } => {
                    let data = *data as usize;
                    let agg = p.data.get(data)?.agg;
                    let (paths, other) = if column { (&row_paths, &p.rows) } else { (&col_paths, &p.cols) };
                    if !sort.matches_axis(other) { return None; }
                    let prefixes = order::selected_paths(paths, fields, |field, item| {
                        axes.get(&field)?.1.get(&key(item)).copied()
                    });
                    (data, agg, *descending, prefixes)
                }
                _ => return None,
            };
            Some(SortKey { data, agg, descending, opposite })
        };
        let row_order = Order::new(&p.rows, false, &row_paths, &acc, resolve);
        let col_order = Order::new(&p.cols, true, &col_paths, &acc, resolve);
        row_order.sort(&mut row_paths);
        col_order.sort(&mut col_paths);
        let items = |field: u32, i: u32| -> Value { label(&axes[&field].0.items[i as usize]) };

        let mut body = Vec::new();
        let compact = p.compact && k > 0;
        let values_on_rows = p.values_on_rows && p.data.len() > 1;
        let vertical_labels = values_on_rows && k > 0;
        if k == 0 {
            body.push((Vec::new(), RowKind::Detail));
        }
        for (n, path) in row_paths.iter().enumerate().filter(|_| k > 0) {
            let prev = n.checked_sub(1).and_then(|index| row_paths.get(index));
            for depth in 1..k {
                if prev.is_some_and(|q| q[..depth] == path[..depth]) { continue; }
                let field = &p.rows[depth - 1];
                let subtotal = p.subtotal_at(field, true);
                if subtotal == Subtotal::Top {
                    body.push((path[..depth].to_vec(), RowKind::Top));
                } else if compact || field.outline {
                    body.push((path[..depth].to_vec(), RowKind::Heading));
                }
            }
            body.push((path.clone(), RowKind::Detail));
            let next = row_paths.get(n + 1);
            for depth in (1..k).rev() {
                if p.subtotal_at(&p.rows[depth - 1], true) == Subtotal::Bottom
                    && next.is_none_or(|q| q[..depth] != path[..depth])
                {
                    body.push((path[..depth].to_vec(), RowKind::Bottom));
                }
            }
        }
        if p.grand_row && k > 0 { body.push((Vec::new(), RowKind::Grand)); }
        let copies = if values_on_rows { 1 } else { nd };
        // Column heads: column items (with the value fields innermost when there
        // are several).
        let mut heads: Vec<ColHead> = Vec::new();
        if m == 0 {
            for d in 0..copies {
                heads.push(ColHead { path: Vec::new(), data: d, total: false });
            }
        } else {
            for (n, path) in col_paths.iter().enumerate() {
                for d in 0..copies {
                    heads.push(ColHead { path: path.clone(), data: d, total: false });
                }
                let next = col_paths.get(n + 1);
                for depth in (1..m).rev() {
                    if p.subtotal_at(&p.cols[depth - 1], false) != Subtotal::Off
                        && next.is_none_or(|q| q[..depth] != path[..depth])
                    {
                        for d in 0..copies {
                            heads.push(ColHead { path: path[..depth].to_vec(), data: d, total: true });
                        }
                    }
                }
            }
            if p.grand_col {
                for d in 0..copies {
                    heads.push(ColHead { path: Vec::new(), data: d, total: true });
                }
            }
        }
        let multi = p.data.len() > 1 && !values_on_rows;
        // A header row for the column fields (and "Values") when there are any.
        let col_dims = m + multi as usize;
        let header_rows = if col_dims == 0 { 1 } else { 1 + col_dims } as u32;
        let label_cols = (if compact { 1 } else { k.max(1) as u32 }) + u32::from(vertical_labels);
        let page_rows = if p.pages.is_empty() { 0 } else { p.pages.len() as u32 + 1 };
        let body_rows: u64 = body.iter().map(|(_, kind)| {
            if values_on_rows && *kind != RowKind::Heading { nd as u64 } else { 1 }
        }).sum();
        let rows_total = page_rows as u64 + header_rows as u64 + body_rows;
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
        if compact {
            let last = top + header_rows - 1;
            if col_dims == 0 {
                if p.data.len() == 1 {
                    put(top, label_cols, Value::str(&caption(0)));
                }
            } else {
                if !multi && !values_on_rows {
                    put(top, 0, Value::str(&caption(0)));
                }
                put(top, label_cols, Value::str("Column Labels"));
            }
            put(last, 0, Value::str("Row Labels"));
        }
        if compact && col_dims == 0 {
        } else if col_dims == 0 {
            for (j, f) in p.rows.iter().enumerate() {
                put(top, j as u32, Value::str(&self.pivot_field_name(p, f.field)));
            }
            if p.data.len() == 1 {
                put(top, label_cols, Value::str(&caption(0)));
            }
        } else {
            if !compact {
                if !multi && !values_on_rows {
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
        if values_on_rows {
            put(top + header_rows - 1, label_cols - 1, Value::str("Values"));
        }
        // Body.
        let first = top + header_rows;
        let mut total_rows = Vec::new();
        let mut prev: Option<&Path> = None;
        let displays: Vec<_> = p.data.iter().enumerate().map(|(i, data)| {
            let field = data.show_as.base_field();
            let slot = field.and_then(|field| {
                p.rows.iter().position(|f| f.field == field).map(|level| Slot { column: false, level })
                    .or_else(|| p.cols.iter().position(|f| f.field == field).map(|level| Slot { column: true, level }))
            });
            let items = field.and_then(|field| axes.get(&field)).map(|(_, items)| items);
            let order = if slot.is_some_and(|slot| slot.column) { &col_order } else { &row_order };
            Display::new(&data.show_as, slot, items, &acc, i, data.agg, order)
        }).collect();
        let value = |rp: &[u32], h: &ColHead, data: usize| -> Value {
            let agg = p.data.get(data).map_or(Agg::Count, |d| d.agg);
            let raw = |row: &[u32], col: &[u32]| acc.get(&(row.to_vec(), col.to_vec()))
                .map_or(Value::Empty, |a| a[data].result(agg));
            match displays.get(data) {
                Some(display) => display.value(rp, &h.path, raw),
                None => raw(rp, &h.path),
            }
        };
        let mut row = first;
        let mut indents = Vec::new();
        let mut percent_bands = Vec::new();
        for (path, kind) in &body {
            let depth = path.len();
            match kind {
                RowKind::Grand => put(row, 0, Value::str("Grand Total")),
                RowKind::Heading | RowKind::Top | RowKind::Bottom => {
                    let col = if compact { 0 } else { depth as u32 - 1 };
                    let item = items(p.rows[depth - 1].field, path[depth - 1]);
                    let shown = if *kind == RowKind::Bottom {
                        Value::str(&format!("{} Total", label_text(&item)))
                    } else { item };
                    put(row, col, shown);
                    if compact { indents.push((row - first, (depth - 1) as u8, true)); }
                }
                RowKind::Detail if compact => {
                    put(row, 0, items(p.rows[depth - 1].field, path[depth - 1]));
                    if depth > 1 { indents.push((row - first, (depth - 1) as u8, false)); }
                }
                RowKind::Detail => {
                    for (lvl, &it) in path.iter().enumerate() {
                        let field = &p.rows[lvl];
                        let has_heading = lvl + 1 < k && (field.outline || p.subtotal_at(field, true) == Subtotal::Top);
                        let new = prev.is_none_or(|q| q[..=lvl] != path[..=lvl]);
                        if !has_heading && new { put(row, lvl as u32, items(field.field, it)); }
                    }
                    if k == 0 && col_dims > 0 && !multi && !values_on_rows {
                        put(row, 0, Value::str("Total"));
                    }
                    prev = Some(path);
                }
            }
            if *kind == RowKind::Heading {
                row += 1;
                continue;
            }
            let total = matches!(kind, RowKind::Top | RowKind::Bottom | RowKind::Grand);
            if values_on_rows {
                let col = label_cols - 1;
                for d in 0..nd {
                    put(row, col, Value::str(&caption(d)));
                    for (x, h) in heads.iter().enumerate() {
                        put(row, label_cols + x as u32, value(path, h, d));
                    }
                    if total { total_rows.push(row - first); }
                    if p.data[d].show_as.is_percent() { percent_bands.push(row); }
                    row += 1;
                }
            } else {
                if total && (!compact || *kind != RowKind::Top) { total_rows.push(row - first); }
                for (x, h) in heads.iter().enumerate() {
                    put(row, label_cols + x as u32, value(path, h, h.data));
                }
                row += 1;
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
            indents,
        };
        let mut percent_ranges = Vec::new();
        if let Some(area) = shape.area.filter(|area| area.r0 + first <= area.r1) {
            if values_on_rows {
                for row in percent_bands {
                    let r = area.r0 + row;
                    percent_ranges.push(Rect { r0: r, r1: r, c0: area.c0 + label_cols, ..area });
                }
            } else {
                for (col, head) in heads.iter().enumerate() {
                    if p.data.get(head.data).is_some_and(|data| data.show_as.is_percent()) {
                        let c = area.c0 + label_cols + col as u32;
                        percent_ranges.push(Rect { r0: area.r0 + first, c0: c, c1: c, ..area });
                    }
                }
            }
        }
        Ok(Output { cells, rows: rows_total as u32, cols: cols_total as u32, shape, percent_ranges, percent_by_row: values_on_rows })
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

fn get_field(r: &mut Reader) -> Result<u32, WireError> {
    r.len("pivot field", u64::from(MAX_COLS) + MAX_GROUPS as u64 - 1).map(|field| field as u32)
}

impl Pivot {
    /// The definition and where its output is (`area`, the shape's).
    pub fn encode(&self, w: &mut Writer, area: Option<Rect>) {
        w.str(&self.name);
        put_rect(w, &self.source);
        w.u16(self.dest.sheet);
        w.var(self.dest.row as u64);
        w.var(self.dest.col as u64);
        let display = self.data.iter().any(|d| d.show_as != ShowAs::Normal);
        let grouped = !self.groups.is_empty();
        let sorted = self.rows.iter().chain(&self.cols).chain(&self.pages).any(|f| f.sort != AxisSort::Default);
        w.u8(self.grand_row as u8 | (self.grand_col as u8) << 1 | (self.subtotals as u8) << 2
            | (self.compact as u8) << 3 | (display as u8) << 4 | (grouped as u8) << 5 | (sorted as u8) << 6 | ((!self.calculations.is_empty()) as u8) << 7);
        for axis in [&self.rows, &self.cols, &self.pages] {
            w.var(axis.len() as u64);
            for f in axis {
                w.var(f.field as u64);
                w.var(f.hidden.len() as u64);
                for v in &f.hidden {
                    put_value(w, v);
                }
                if sorted {
                    f.sort.encode(w);
                }
            }
        }
        w.var(self.data.len() as u64);
        for d in &self.data {
            w.var(d.field as u64);
            w.u8(Agg::ALL.iter().position(|a| *a == d.agg).unwrap_or(0) as u8);
            w.str(d.name.as_deref().unwrap_or(""));
            if display {
                d.show_as.encode(w);
            }
        }
        if grouped {
            w.var(self.groups.len() as u64);
            for group in &self.groups {
                group.encode(w);
            }
        }
        if !self.calculations.is_empty() {
            w.var(self.calculations.len() as u64);
            for field in &self.calculations {
                w.var(u64::from(field.field));
                w.str(&field.name);
                w.str(&field.formula);
            }
        }
        let layout = self.values_on_rows || self.rows.iter().chain(&self.cols)
            .any(|field| field.outline || field.subtotal != Subtotal::Default);
        let extras = self.has_extras();
        w.u8(u8::from(area.is_some()) | (u8::from(layout) << 1) | (u8::from(extras) << 2));
        if let Some(area) = area { put_rect(w, &area); }
        if layout {
            w.u8(u8::from(self.values_on_rows));
            for field in self.rows.iter().chain(&self.cols) {
                w.u8(field.subtotal as u8 | (u8::from(field.outline) << 2));
            }
        }
        if extras {
            self.encode_extras(w, put_value);
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
                let field = get_field(r)?;
                let h = r.count("hidden items", MAX_ITEMS as u64, 1)?;
                let mut hidden = Vec::with_capacity(h.min(1024));
                for _ in 0..h {
                    hidden.push(get_value(r)?);
                }
                let sort = if flags & 64 == 0 { AxisSort::Default } else { AxisSort::decode(r)? };
                v.push(PivotField { field, hidden, sort, ..PivotField::new(field) });
            }
            axes.push(v);
        }
        let n = r.count("value fields", MAX_FIELDS as u64, 3)?;
        let mut data = Vec::with_capacity(n);
        for _ in 0..n {
            let field = get_field(r)?;
            let agg = *Agg::ALL.get(r.u8()? as usize).ok_or(WireError::Invalid("pivot function"))?;
            let nm = r.str("value field name", 255)?;
            let name = (!nm.is_empty()).then(|| Arc::from(nm));
            let show_as = if flags & 16 == 0 { ShowAs::Normal } else { ShowAs::decode(r)? };
            data.push(DataField { field, agg, name, show_as });
        }
        let mut groups = Vec::new();
        if flags & 32 != 0 {
            let n = r.count("pivot group fields", MAX_GROUPS as u64, 4)?;
            for _ in 0..n {
                groups.push(GroupField::decode(r)?);
            }
        }
        let mut calculations = Vec::new();
        if flags & 128 != 0 {
            let n = r.count("pivot calculated fields", MAX_GROUPS as u64, 3)?;
            for _ in 0..n {
                calculations.push(CalculatedField {
                    field: get_field(r)?,
                    name: Arc::from(r.str("pivot calculated field name", 255)?),
                    formula: Arc::from(r.str("pivot calculated formula", 1024)?),
                });
            }
        }
        let tag = r.u8()?;
        if tag > 7 { return Err(WireError::Invalid("pivot layout flags")); }
        let area = if tag & 1 != 0 { Some(get_rect(r)?) } else { None };
        let pages = axes.pop().unwrap();
        let mut cols = axes.pop().unwrap();
        let mut rows = axes.pop().unwrap();
        let values_on_rows = if tag & 2 != 0 {
            let values = match r.u8()? {
                0 => false, 1 => true,
                _ => return Err(WireError::Invalid("pivot values axis")),
            };
            for field in rows.iter_mut().chain(&mut cols) {
                let flags = r.u8()?;
                if flags > 7 { return Err(WireError::Invalid("pivot field layout")); }
                field.subtotal = Subtotal::ALL[(flags & 3) as usize];
                field.outline = flags & 4 != 0;
            }
            values
        } else { false };
        let (filters, slicers) = if tag & 4 != 0 { Pivot::decode_extras(r, get_field, get_value)? } else { (Vec::new(), Vec::new()) };
        Ok((Pivot { name, source, dest: CellAddr::new(sheet, row as u32, col as u32), rows, cols, pages, data, groups, calculations, grand_row: flags & 1 != 0, grand_col: flags & 2 != 0, subtotals: flags & 4 != 0, compact: flags & 8 != 0, values_on_rows, filters, slicers }, area))
    }
}
