//! Auto-filter: a header row over a range, conditions per column, and the rows that
//! fail them hidden. Hidden rows are per sheet ranges (a filter's, or hidden by
//! hand); the view's row sizes treat them as zero high ([`Sheet::view_row_sizes`]).
//!
//! Setting or changing a filter is one undoable op ([`GridOp::Filter`]) that swaps the
//! sheet's filter and hidden rows.

use crate::addr::{CellAddr, Rect, MAX_COLS, MAX_ROWS};
use officina_core::wire::{Reader, WireError, Writer};
use crate::book::{Sheet, Workbook};
use crate::column::Sizes;
use crate::eval::funcs::text_key;
use crate::value::Value;

mod appearance;
pub use appearance::{AppearanceFilter, ColorFilter, IconFilter};
mod calendar;
pub use calendar::{valid_calendar_type, CalendarSelection, DateBounds, DateGroup, DynamicDate, GROUP_FIELDS, MAX_SELECTIONS};
mod text;
pub use text::{wildcard_regex, Test, TextOp, TextTest, MAX_TESTS};

/// What a filtered column keeps.
#[derive(Debug, Clone, PartialEq)]
pub enum Cond {
    /// Cells showing one of these texts (as formatted; compared ignoring case). An
    /// empty string keeps empty cells.
    Values(Vec<String>),
    /// Numbers compared with a constant: (operator, value), `op` one of
    /// `= <> < <= > >=`; two joined by and (`true`) or or.
    Compare(Vec<(String, f64)>, bool),
    Rank(Rank),
    Calendar(CalendarSelection),
    DynamicDate(DynamicDate),
    Appearance(AppearanceFilter),
    Expression(Box<FilterExpr>),
    /// Custom numeric and text tests joined by AND (true) or OR.
    Custom(Vec<Test>, bool),
}

/// Boolean filter conditions with columns relative to the filter's left edge.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterExpr {
    Column(u32, Cond),
    All(Vec<Self>),
    Any(Vec<Self>),
}

pub const MAX_FILTER_DEPTH: usize = 16;
pub const MAX_FILTER_NODES: usize = 1024;

impl FilterExpr {
    pub fn fits(&self, width: u32) -> bool {
        match self {
            Self::Column(c, _) => *c < width,
            Self::All(children) | Self::Any(children) => children.iter().all(|child| child.fits(width)),
        }
    }

    pub fn uses_column(&self, col: u32) -> bool {
        match self {
            Self::Column(c, _) => *c == col,
            Self::All(children) | Self::Any(children) => children.iter().any(|child| child.uses_column(col)),
        }
    }

    pub(crate) fn remap(self, map: &impl Fn(u32) -> Option<u32>) -> Option<Self> {
        let (all, children) = match self {
            Self::Column(c, cond) => return Some(Self::Column(map(c)?, cond)),
            Self::All(children) => (true, children),
            Self::Any(children) => (false, children),
        };
        let children: Vec<_> = children.into_iter().filter_map(|child| child.remap(map)).collect();
        if children.is_empty() { return None; }
        Some(if all { Self::All(children) } else { Self::Any(children) })
    }

    fn encode(&self, w: &mut Writer) {
        match self {
            Self::Column(c, cond) => {
                w.u8(0);
                w.var(u64::from(*c));
                cond.encode(w);
            }
            Self::All(children) | Self::Any(children) => {
                w.u8(if matches!(self, Self::All(_)) { 1 } else { 2 });
                w.var(children.len() as u64);
                for child in children { child.encode(w); }
            }
        }
    }

    fn decode(r: &mut Reader, depth: usize, remaining: &mut usize) -> Result<Self, WireError> {
        if depth > MAX_FILTER_DEPTH || *remaining == 0 {
            return Err(WireError::Invalid("filter expression limit"));
        }
        *remaining -= 1;
        match r.u8()? {
            0 => {
                let c = r.len("filter expression column", u64::from(MAX_COLS - 1))? as u32;
                Ok(Self::Column(c, Cond::decode(r, false)?))
            }
            tag @ (1 | 2) => {
                let n = r.count("filter branches", *remaining as u64, 3)?;
                if n == 0 { return Err(WireError::Invalid("empty filter expression")); }
                let children = (0..n).map(|_| Self::decode(r, depth + 1, remaining)).collect::<Result<_, _>>()?;
                Ok(if tag == 1 { Self::All(children) } else { Self::Any(children) })
            }
            _ => Err(WireError::Invalid("filter expression kind")),
        }
    }

    fn prepare(&self, wb: &Workbook, rect: Rect, first: u32, col: u32) -> PreparedExpr<'_> {
        match self {
            Self::Column(offset, rule) => {
                let column = col + offset;
                PreparedExpr::Column(column, rule, rule.prepare(wb, rect, first, column))
            }
            Self::All(items) | Self::Any(items) => PreparedExpr::Group(matches!(self, Self::All(_)),
                items.iter().map(|item| item.prepare(wb, rect, first, col)).collect()),
        }
    }
}

enum PreparedExpr<'a> {
    Column(u32, &'a Cond, Prepared<'a>),
    Group(bool, Vec<Self>),
}

impl PreparedExpr<'_> {
    fn keeps(&self, wb: &Workbook, addr: CellAddr) -> bool {
        match self {
            Self::Column(c, rule, prepared) => rule.keeps(wb, CellAddr { col: *c, ..addr }, prepared),
            Self::Group(true, children) => children.iter().all(|item| item.keeps(wb, addr)),
            Self::Group(false, children) => children.iter().any(|item| item.keeps(wb, addr)),
        }
    }
}

/// Top or bottom numeric cells by count or percentage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rank {
    limit: u32,
    pub percent: bool,
    pub bottom: bool,
}

impl Rank {
    pub fn new(limit: u32, percent: bool, bottom: bool) -> Option<Self> {
        let max = if percent { 100 } else { MAX_ROWS };
        (1..=max).contains(&limit).then_some(Self { limit, percent, bottom })
    }

    pub fn parse(limit: &str, percent: bool, bottom: bool) -> Option<Self> {
        let limit: f64 = limit.parse().ok()?;
        if !limit.is_finite() || limit.fract() != 0.0 || limit > f64::from(MAX_ROWS) {
            return None;
        }
        Self::new(limit as u32, percent, bottom)
    }

    pub fn limit(self) -> u32 {
        self.limit
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AutoFilter {
    /// The range, its first row the header (with the filter buttons).
    pub rect: Rect,
    /// Conditions by column of the sheet (inside the range).
    pub conds: Vec<(u32, Cond)>,
}

/// A sheet's filter and hidden rows, as a [`GridOp::Filter`] carries them.
#[derive(Debug, Clone, PartialEq)]
pub struct FilterState {
    pub sheet: u16,
    pub filter: Option<AutoFilter>,
    pub hidden: Vec<(u32, u32)>,
}

impl FilterState {
    /// As journal or file bytes (after the sheet).
    pub fn encode(&self, w: &mut Writer) {
        w.u16(self.sheet);
        w.var(self.hidden.len() as u64);
        for &(a, b) in &self.hidden {
            w.var(a as u64);
            w.var((b - a) as u64);
        }
        match &self.filter {
            None => w.u8(0),
            Some(f) => {
                w.u8(1);
                for x in [f.rect.r0, f.rect.c0, f.rect.r1, f.rect.c1] {
                    w.var(x as u64);
                }
                w.var(f.conds.len() as u64);
                for (c, cond) in &f.conds {
                    w.var(*c as u64);
                    cond.encode(w);
                }
            }
        }
    }

    /// Reads [`FilterState::encode`]; bounded, never panics.
    pub fn decode(r: &mut Reader) -> Result<Self, WireError> {
        let sheet = r.u16()?;
        let n = r.count("hidden ranges", MAX_ROWS as u64, 2)?;
        let mut hidden = Vec::with_capacity(n);
        let mut next = 0u64;
        for _ in 0..n {
            let a = r.len("row", MAX_ROWS as u64 - 1)? as u64;
            let len = r.len("rows", MAX_ROWS as u64 - 1)? as u64;
            if a < next || a + len >= MAX_ROWS as u64 {
                return Err(WireError::Invalid("hidden rows out of order"));
            }
            hidden.push((a as u32, (a + len) as u32));
            next = a + len + 2;
        }
        let filter = match r.u8()? {
            0 => None,
            1 => {
                let row = |r: &mut Reader| r.len("row", MAX_ROWS as u64 - 1).map(|x| x as u32);
                let col = |r: &mut Reader| r.len("column", MAX_COLS as u64 - 1).map(|x| x as u32);
                let (r0, c0, r1, c1) = (row(r)?, col(r)?, row(r)?, col(r)?);
                if r0 > r1 || c0 > c1 {
                    return Err(WireError::Invalid("filter range"));
                }
                let k = r.count("filter columns", (c1 - c0 + 2) as u64, 2)?;
                let mut conds = Vec::with_capacity(k);
                for _ in 0..k {
                    let c = col(r)?;
                    if c < c0 || c > c1 {
                        return Err(WireError::Invalid("filter column outside its range"));
                    }
                    let cond = Cond::decode(r, true)?;
                    if let Cond::Expression(expr) = &cond {
                        if c != c0 || !expr.fits(c1 - c0 + 1) || conds.iter().any(|(_, prev)| matches!(prev, Cond::Expression(_))) {
                            return Err(WireError::Invalid("filter expression outside its range"));
                        }
                    }
                    conds.push((c, cond));
                }
                Some(AutoFilter { rect: Rect { sheet, r0, c0, r1, c1 }, conds })
            }
            _ => return Err(WireError::Invalid("filter flag")),
        };
        Ok(FilterState { sheet, filter, hidden })
    }
}

impl Cond {
    fn encode(&self, w: &mut Writer) {
        match self {
            Cond::Values(v) => {
                w.u8(0);
                w.var(v.len() as u64);
                for s in v {
                    w.str(s);
                }
            }
            Cond::Compare(cs, and) => {
                w.u8(if *and { 2 } else { 1 });
                w.var(cs.len() as u64);
                for (op, x) in cs {
                    w.str(op);
                    w.f64(*x);
                }
            }
            Cond::Custom(tests, and) => {
                w.u8(if *and { 5 } else { 4 });
                w.var(tests.len() as u64);
                for test in tests {
                    test.encode(w);
                }
            }
            Cond::Calendar(selection) => {
                w.u8(6);
                selection.encode(w);
            }
            Cond::DynamicDate(rule) => {
                w.u8(7);
                rule.encode(w);
            }
            Cond::Appearance(rule) => {
                w.u8(8);
                rule.encode(w);
            }
            Cond::Rank(rank) => {
                w.u8(3);
                w.u8(u8::from(rank.percent) | (u8::from(rank.bottom) << 1));
                w.var(u64::from(rank.limit()));
            }
            Cond::Expression(expr) => {
                w.u8(9);
                expr.encode(w);
            }
        }
    }

    fn decode(r: &mut Reader, expressions: bool) -> Result<Self, WireError> {
        Ok(match r.u8()? {
            0 => {
                let m = r.count("filter values", 1 << 20, 1)?;
                let mut v = Vec::with_capacity(m);
                for _ in 0..m {
                    v.push(r.str("filter value", crate::strings::MAX_TEXT as u64)?.to_string());
                }
                Cond::Values(v)
            }
            t @ (1 | 2) => {
                let m = r.count("filter tests", 2, 9)?;
                let mut cs = Vec::with_capacity(m);
                for _ in 0..m {
                    let op = r.str("operator", 2)?;
                    if !["=", "<>", "<", "<=", ">", ">="].contains(&op) {
                        return Err(WireError::Invalid("filter operator"));
                    }
                    let x = r.f64()?;
                    if !x.is_finite() {
                        return Err(WireError::Invalid("filter number"));
                    }
                    cs.push((op.to_string(), x));
                }
                Cond::Compare(cs, t == 2)
            }
            3 => {
                let flags = r.u8()?;
                if flags > 3 {
                    return Err(WireError::Invalid("rank filter flags"));
                }
                let limit = r.len("rank filter limit", u64::from(MAX_ROWS))? as u32;
                Cond::Rank(Rank::new(limit, flags & 1 != 0, flags & 2 != 0)
                    .ok_or(WireError::Invalid("rank filter limit"))?)
            }
            t @ (4 | 5) => {
                let n = r.count("custom tests", MAX_TESTS as u64, 4)?;
                if n == 0 {
                    return Err(WireError::Invalid("empty custom filter"));
                }
                let tests = (0..n).map(|_| Test::decode(r)).collect::<Result<_, _>>()?;
                Cond::Custom(tests, t == 5)
            }
            6 => Cond::Calendar(CalendarSelection::decode(r)?),
            7 => Cond::DynamicDate(DynamicDate::decode(r)?),
            8 => Cond::Appearance(AppearanceFilter::decode(r)?),
            9 if expressions => {
                let mut remaining = MAX_FILTER_NODES;
                Cond::Expression(Box::new(FilterExpr::decode(r, 0, &mut remaining)?))
            }
            _ => return Err(WireError::Invalid("filter kind")),
        })
    }

    pub fn is_expression(&self) -> bool { matches!(self, Self::Expression(_)) }

    fn prepare(&self, wb: &Workbook, rect: Rect, first: u32, col: u32) -> Prepared<'_> {
        match self {
            Self::Rank(rank) => Prepared::Rank(wb.rank_cutoff(rect, first, col, *rank)),
            Self::Custom(tests, _) => Prepared::Custom(tests.iter().map(Test::prepare).collect()),
            Self::Calendar(selection) => Prepared::Calendar(selection.prepare()),
            Self::Expression(expr) => Prepared::Expression(Box::new(expr.prepare(wb, rect, first, rect.c0))),
            _ => Prepared::Other,
        }
    }

    pub fn is_excel_date(&self) -> bool {
        matches!(self, Self::Calendar(_) | Self::DynamicDate(_))
    }

    /// Keep the original numeric wire kind for existing one- and two-test rules.
    pub fn custom(tests: Vec<Test>, and: bool) -> Self {
        if tests.len() <= 2 && tests.iter().all(|t| matches!(t, Test::Number(..))) {
            Self::Compare(tests.into_iter().filter_map(|t| match t {
                Test::Number(op, x) => Some((op, x)),
                Test::Text(_) => None,
            }).collect(), and)
        } else {
            Self::Custom(tests, and)
        }
    }

    fn keeps(&self, wb: &Workbook, addr: CellAddr, prepared: &Prepared<'_>) -> bool {
        match self {
            Cond::Values(vals) => {
                let k = text_key(&wb.formatted(addr).text);
                vals.iter().any(|x| text_key(x) == k)
            }
            Cond::Compare(cs, and) => {
                let Value::Num(x) = wb.get(addr) else { return false };
                let test = |(op, y): &(String, f64)| match op.as_str() {
                    "=" => x == *y,
                    "<>" => x != *y,
                    "<" => x < *y,
                    "<=" => x <= *y,
                    ">" => x > *y,
                    ">=" => x >= *y,
                    _ => true,
                };
                if *and {
                    cs.iter().all(test)
                } else {
                    cs.iter().any(test)
                }
            }
            Cond::Custom(_, and) => {
                let value = wb.get(addr);
                let shown = wb.formatted(addr).text;
                let test = |t: &text::Prepared<'_>| t.keeps(&value, &shown);
                let Prepared::Custom(tests) = prepared else { unreachable!() };
                if *and { tests.iter().all(test) } else { tests.iter().any(test) }
            }
            Cond::Calendar(_) => {
                let Prepared::Calendar(selection) = prepared else { unreachable!() };
                calendar_serial(wb, addr).is_some_and(|serial| selection.matches_date(wb.date_system(), serial))
                    || (!selection.values.is_empty() && selection.values.contains(&text_key(&wb.formatted(addr).text)))
            }
            Cond::Expression(_) => {
                let Prepared::Expression(expr) = prepared else { unreachable!() };
                expr.keeps(wb, addr)
            }
            Cond::Appearance(rule) => rule.keeps(wb, addr),
            Cond::DynamicDate(rule) => calendar_serial(wb, addr).is_some_and(|serial| rule.keeps(wb.date_system(), serial)),
            Cond::Rank(rank) => {
                let (Value::Num(x), Prepared::Rank(Some(y))) = (wb.get(addr), prepared) else { return false };
                x.is_finite() && if rank.bottom { x <= *y } else { x >= *y }
            }
        }
    }
}

fn calendar_serial(wb: &Workbook, addr: CellAddr) -> Option<f64> {
    let Value::Num(serial) = wb.get(addr) else { return None };
    wb.styles.format(wb.style_id(addr)).is_date().then_some(serial)
}

enum Prepared<'a> {
    Other,
    Rank(Option<f64>),
    Custom(Vec<text::Prepared<'a>>),
    Calendar(calendar::CalendarMatcher),
    Expression(Box<PreparedExpr<'a>>),
}

/// Merges sorted row ranges, joining touching ones.
pub fn merge(mut v: Vec<(u32, u32)>) -> Vec<(u32, u32)> {
    v.sort_unstable();
    let mut out: Vec<(u32, u32)> = Vec::with_capacity(v.len());
    for (a, b) in v {
        match out.last_mut() {
            Some(l) if a <= l.1.saturating_add(1) => l.1 = l.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

fn intersect_rows(a: &[(u32, u32)], b: &[(u32, u32)]) -> Vec<(u32, u32)> {
    a.iter().flat_map(|&(lo, hi)| {
        b[b.partition_point(|&(_, end)| end < lo)..].iter().take_while(move |&&(start, _)| start <= hi)
            .map(move |&(start, end)| (lo.max(start), hi.min(end)))
    }).collect()
}

fn subtract_rows(a: &[(u32, u32)], b: &[(u32, u32)]) -> Vec<(u32, u32)> {
    let mut rows = Vec::new();
    for &(mut lo, hi) in a {
        for &(start, end) in &b[b.partition_point(|&(_, end)| end < lo)..] {
            if start > hi { break; }
            if start > lo { rows.push((lo, start - 1)); }
            lo = lo.max(end + 1);
            if lo > hi { break; }
        }
        if lo <= hi { rows.push((lo, hi)); }
    }
    rows
}

fn contains_row(ranges: &[(u32, u32)], row: u32) -> bool {
    ranges.get(ranges.partition_point(|h| h.1 < row)).is_some_and(|h| h.0 <= row)
}

impl Sheet {
    /// Whether row `r` is hidden.
    pub fn row_hidden(&self, r: u32) -> bool {
        contains_row(&self.hidden, r) || contains_row(&self.table_hidden, r) || self.outline(crate::shift::Axis::Rows).hides(r)
    }

    pub fn table_hidden(&self) -> &[(u32, u32)] {
        &self.table_hidden
    }

    pub fn hidden_rows(&self) -> Vec<(u32, u32)> {
        merge(self.hidden.iter().chain(&self.table_hidden).copied().collect())
    }

    /// Row sizes as the view lays them out: hidden rows zero high. O(overrides +
    /// hidden ranges); the app keeps it until rows or sizes change.
    pub fn view_row_sizes(&self) -> Sizes {
        let outlined = self.outline(crate::shift::Axis::Rows).hidden();
        if self.hidden.is_empty() && self.table_hidden.is_empty() && outlined.is_empty() {
            return self.row_sizes.clone();
        }
        let mut over: Vec<(u32, f64)> = self.row_sizes.overrides().iter().copied().filter(|(r, _)| !self.row_hidden(*r)).collect();
        let rows = merge(self.hidden_rows().into_iter().chain(outlined.iter().copied()).collect());
        for (a, b) in rows {
            over.extend((a..=b).map(|r| (r, 0.0)));
        }
        Sizes::from_overrides(self.row_sizes.default, over)
    }
}

impl Workbook {
    pub fn filter_conditions(&self) -> impl Iterator<Item = &Cond> {
        self.sheets.iter().filter_map(|sh| sh.filter.as_ref()).map(|f| f.conds.as_slice())
            .chain(self.tables.iter().filter_map(|t| t.filter.as_ref()).map(|f| f.conds.as_slice()))
            .flatten().map(|(_, cond)| cond)
    }

    /// The sheet's filter and hidden rows now.
    pub fn filter_state(&self, sheet: u16) -> FilterState {
        let sh = self.sheet(sheet);
        FilterState { sheet, filter: sh.filter.clone(), hidden: sh.hidden.clone() }
    }

    /// Replaces the sheet's filter and hidden rows; returns what they were.
    pub fn replace_filter(&mut self, st: FilterState) -> FilterState {
        if st.sheet as usize >= self.sheet_count() {
            return st;
        }
        let old = self.filter_state(st.sheet);
        let sh = self.sheet_mut(st.sheet);
        sh.filter = st.filter;
        sh.hidden = merge(st.hidden);
        old
    }

    /// The cutoff over the numeric data rows before intersecting other column
    /// conditions. All values tied at the cutoff are kept.
    fn rank_cutoff(&self, rect: Rect, first: u32, col: u32, rank: Rank) -> Option<f64> {
        if first > rect.r1 {
            return None;
        }
        let mut numbers = Vec::new();
        let column = Rect { r0: first, c0: col, c1: col, ..rect };
        self.each(&column, &mut |_, _, value| {
            if let Value::Num(x) = value {
                if x.is_finite() {
                    numbers.push(x);
                }
            }
        });
        let n = numbers.len();
        if n == 0 {
            return None;
        }
        let limit = rank.limit() as usize;
        let count = if rank.percent { (n * limit).div_ceil(100) } else { limit.min(n) };
        let index = if rank.bottom { count - 1 } else { n - count };
        Some(*numbers.select_nth_unstable_by(index, f64::total_cmp).1)
    }

    /// The state `filter` leaves the sheet in: rows of its range (below the header)
    /// failing a condition hidden, rows hidden outside the range kept hidden.
    pub fn filtered(&self, sheet: u16, mut filter: Option<AutoFilter>) -> FilterState {
        let sh = self.sheet(sheet);
        // Rows hidden outside the old filter's range stay hidden by hand.
        let old = sh.filter.as_ref().map(|f| (f.rect.r0 + 1, f.rect.r1));
        let mut hidden: Vec<(u32, u32)> = Vec::new();
        for &(a, b) in &sh.hidden {
            match old {
                Some((x, y)) if a <= y && b >= x => {
                    if a < x {
                        hidden.push((a, x - 1));
                    }
                    if b > y {
                        hidden.push((y + 1, b));
                    }
                }
                _ => hidden.push((a, b)),
            }
        }
        if let Some(f) = &mut filter {
            hidden.extend(self.filter_rows(f, f.rect.r0 + 1, true));
        }
        FilterState { sheet, filter, hidden: merge(hidden) }
    }

    fn filter_rows(&self, f: &mut AutoFilter, first: u32, reapply: bool) -> Vec<(u32, u32)> {
        if f.conds.is_empty() || first > f.rect.r1 {
            return Vec::new();
        }
        let mut hidden = Vec::new();
        if reapply && f.conds.iter().any(|(_, cond)| matches!(cond, Cond::DynamicDate(_))) {
            let system = self.date_system();
            let today = system.now();
            for (_, cond) in &mut f.conds {
                if let Cond::DynamicDate(rule) = cond {
                    rule.cached = rule.bounds(system, today);
                }
            }
        }
        let prepared: Vec<_> = f.conds.iter().map(|(col, cond)| cond.prepare(self, f.rect, first, *col)).collect();
        let mut run: Option<(u32, u32)> = None;
        for r in first..=f.rect.r1 {
            let keep = f.conds.iter().zip(&prepared).all(|((c, cond), prepared)| {
                cond.keeps(self, CellAddr::new(f.rect.sheet, r, *c), prepared)
            });
            if !keep {
                run = match run {
                    Some((a, b)) if b + 1 == r => Some((a, r)),
                    Some(x) => {
                        hidden.push(x);
                        Some((r, r))
                    }
                    None => Some((r, r)),
                };
            }
        }
        hidden.extend(run);
        hidden
    }

    pub fn apply_table_filter(&self, table: &crate::tables::Table, mut filter: AutoFilter) -> crate::tables::TableFilter {
        let rect = filter.rect;
        let hidden = self.filter_rows(&mut filter, rect.r0 + u32::from(table.header), true);
        crate::tables::TableFilter {
            conds: filter.conds.into_iter().map(|(col, cond)| (col - rect.c0, cond)).collect(),
            hidden: hidden.into_iter().map(|(lo, hi)| (lo - rect.r0, hi - rect.r0)).collect(),
        }
    }

    pub(crate) fn rebuild_table_visibility(&mut self, sheet: u16) {
        self.sheets[sheet as usize].table_hidden = merge(self.tables.iter()
            .filter(|t| t.rect.sheet == sheet)
            .filter_map(|t| t.filter.as_ref().map(|f| (t.rect.r0, f)))
            .flat_map(|(base, f)| f.hidden.iter().map(move |(a, b)| (a + base, b + base)))
            .collect());
    }

    /// Foreign row visibility is sheet-wide. Attribute saved hidden rows to the
    /// table rules the cells currently fail, without changing the saved union.
    /// ODS may additionally supply the rows it explicitly marks as filtered.
    pub fn restore_table_visibility(&mut self, sheet: u16, filtered: Option<&[(u32, u32)]>) {
        let sh = self.sheet(sheet);
        let saved = sh.hidden.as_slice();
        if saved.is_empty() {
            return;
        }
        let owned: Vec<_> = self.tables.iter().enumerate().filter(|(_, t)| t.rect.sheet == sheet)
            .filter_map(|(i, t)| {
                let mut filter = t.auto_filter()?;
                if filter.conds.is_empty() {
                    return None;
                }
                let first = t.rect.r0 + u32::from(t.header);
                let rows = intersect_rows(saved, &self.filter_rows(&mut filter, first, false));
                Some((i, t.rect.r0, rows))
            }).collect();
        if owned.is_empty() {
            return;
        }
        let worksheet = sh.filter.clone().map_or_else(Vec::new, |mut f| {
            let first = f.rect.r0 + 1;
            self.filter_rows(&mut f, first, false)
        });
        let mut taken = Vec::new();
        for (i, base, rows) in owned {
            self.tables[i].filter.as_mut().unwrap().hidden = rows.iter().map(|(a, b)| (a - base, b - base)).collect();
            taken.extend(rows);
        }
        let mut remove = subtract_rows(&merge(taken), &worksheet);
        if let Some(filtered) = filtered {
            let manual = subtract_rows(&self.sheet(sheet).hidden, filtered);
            remove = subtract_rows(&remove, &manual);
        }
        self.sheets[sheet as usize].hidden = subtract_rows(&self.sheet(sheet).hidden, &remove);
        self.rebuild_table_visibility(sheet);
    }

    /// The values a filter column offers to pick from: distinct shown texts below the
    /// header, sorted, with how many rows show each (at most `max`).
    pub fn filter_values(&self, rect: &Rect, col: u32, max: usize) -> Vec<(String, u32)> {
        self.filter_values_from(rect, rect.r0 + 1, col, max)
    }

    pub fn filter_values_from(&self, rect: &Rect, first: u32, col: u32, max: usize) -> Vec<(String, u32)> {
        let mut m: std::collections::BTreeMap<String, (String, u32)> = std::collections::BTreeMap::new();
        for r in first..=rect.r1 {
            let t = self.formatted(CellAddr::new(rect.sheet, r, col)).text;
            let e = m.entry(text_key(&t)).or_insert((t, 0));
            e.1 += 1;
            if m.len() > max {
                break;
            }
        }
        m.into_values().take(max).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::GridOp;
    use officina_core::history::Op;

    #[test]
    fn rank_filter_reapplies_and_undo_keeps_the_rule() {
        let mut wb = Workbook::new();
        wb.add_sheet("Scores").unwrap();
        for (row, (group, score)) in (1..).zip([("away", 9.0), ("home", 8.0), ("home", 7.0)]) {
            wb.set_value(CellAddr::new(0, row, 0), Value::str(group));
            wb.set_value(CellAddr::new(0, row, 1), Value::Num(score));
        }
        let filter = AutoFilter {
            rect: Rect { sheet: 0, r0: 0, r1: 3, c0: 0, c1: 1 },
            conds: vec![
                (0, Cond::Values(vec!["home".into()])),
                (1, Cond::Rank(Rank::new(2, false, false).unwrap())),
            ],
        };
        wb.replace_filter(wb.filtered(0, Some(filter.clone())));
        assert_eq!(wb.sheet(0).hidden, [(1, 1), (3, 3)]);
        wb.set_value(CellAddr::new(0, 1, 1), Value::Num(1.0));

        let previous = wb.filter_state(0);
        let undo = GridOp::Filter(wb.filtered(0, Some(filter))).apply(&mut wb);
        assert_eq!(wb.sheet(0).hidden, [(1, 1)]);
        let reopened = crate::native::load(&crate::native::save_to_vec(&wb)).unwrap();
        assert_eq!(reopened.filter_state(0), wb.filter_state(0));

        GridOp::decode(&undo.encode()).unwrap().apply(&mut wb);
        assert_eq!(wb.filter_state(0), previous);
    }

    #[test]
    fn filter_hides_and_undo_shows() {
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        wb.set_input(CellAddr::new(0, 0, 0), "Fruit").unwrap();
        wb.set_input(CellAddr::new(0, 0, 1), "Count").unwrap();
        for (r, (f, n)) in [("apple", 3), ("Pear", 10), ("apple", 7), ("fig", 1), ("pear", 4)].iter().enumerate() {
            wb.set_input(CellAddr::new(0, r as u32 + 1, 0), f).unwrap();
            wb.set_value(CellAddr::new(0, r as u32 + 1, 1), Value::Num(*n as f64));
        }
        // A row hidden by hand, outside the range.
        wb.sheet_mut(0).hidden = vec![(20, 22)];
        let rect = Rect { sheet: 0, r0: 0, c0: 0, r1: 5, c1: 1 };
        assert_eq!(wb.filter_values(&rect, 0, 100), [("apple".to_string(), 2), ("fig".to_string(), 1), ("Pear".to_string(), 2)]);
        let f = AutoFilter { rect, conds: vec![(0, Cond::Values(vec!["pear".into(), "apple".into()])), (1, Cond::Compare(vec![(">".into(), 3.0)], true))] };
        let op = GridOp::Filter(wb.filtered(0, Some(f.clone())));
        let undo = op.apply(&mut wb);
        assert_eq!(wb.sheet(0).hidden, [(1, 1), (4, 4), (20, 22)]);
        assert_eq!(wb.sheet(0).filter.as_ref(), Some(&f));
        let v = wb.sheet(0).view_row_sizes();
        assert_eq!(v.offset(5) - v.offset(1), 2.0 * 20.0);
        assert!(wb.sheet(0).row_hidden(21) && !wb.sheet(0).row_hidden(2));
        // The native file keeps both.
        let back = crate::native::load(&crate::native::save_to_vec(&wb)).unwrap();
        assert_eq!(back.filter_state(0), wb.filter_state(0));
        // Clearing the filter keeps the hand-hidden rows.
        let st = wb.filtered(0, None);
        assert_eq!(st.hidden, [(20, 22)]);
        let bytes = undo.encode();
        GridOp::decode(&bytes).unwrap().apply(&mut wb);
        assert_eq!(wb.sheet(0).hidden, [(20, 22)]);
        assert!(wb.sheet(0).filter.is_none());
    }
}
