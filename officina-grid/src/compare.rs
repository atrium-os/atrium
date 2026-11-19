//! Comparing two workbooks: what would turn ours into theirs, as a list of
//! changes a person can look through and take one at a time (officina.md
//! §16.68).
//!
//! Sheets are matched by name. Within a sheet, rows are matched by their
//! contents (formulas compared in their relative form, so a row that moved
//! keeps matching), with a longest-common-subsequence over row hashes between
//! the common start and end; rows left over between two matches are paired in
//! order and compared cell by cell, and what is left after that are rows
//! added or removed. Static styles follow matched rows in two-way review.

use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, HashMap};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use crate::addr::{CellAddr, Rect, MAX_COLS, MAX_ROWS};
use crate::book::{Input, Workbook};
use crate::deps::GroupId;
use crate::formula;
use crate::ops::{Fill, GridOp, Region};
use crate::value::Value;

mod parts;
mod rules;
mod tables;
pub use parts::{ChartChange, NameChange};
pub use rules::RuleChange;
pub use tables::{PivotChange, TableChange};

/// The row formulas are printed for when compared, so that relative
/// references read the same wherever the row is.
const KEY_ROW: u32 = 1 << 20;
/// Above this many row pairs between the common start and end, rows are
/// matched by position instead of by contents.
const LCS_MAX: usize = 4_000_000;
/// At most this many changes are listed.
pub const MAX_CHANGES: usize = 100_000;

/// One difference between ours and theirs.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// A cell that differs: what ours holds and what theirs does (a
    /// formula written for our cell).
    Cell { sheet: u16, row: u32, col: u32, ours: Input, theirs: Input },
    /// A static style change over a contiguous run in one column.
    Style { rect: Rect, ours: Box<crate::style::Style>, theirs: Box<crate::style::Style> },
    Chart(ChartChange),
    Name(NameChange),
    Table(TableChange),
    Pivot(PivotChange),
    ConditionalFormats(RuleChange<crate::condfmt::CondFormat>),
    Validations(RuleChange<crate::validation::Validation>),
    /// `n` rows of theirs, from their row `theirs_row`, that ours doesn't
    /// have; they would go before our row `at`.
    RowsAdded { sheet: u16, at: u32, n: u32, theirs_row: u32 },
    /// `n` rows of ours from `row` that theirs doesn't have.
    RowsRemoved { sheet: u16, row: u32, n: u32 },
    /// A sheet only theirs has.
    SheetAdded { name: String },
    /// A sheet only ours has.
    SheetRemoved { sheet: u16 },
    StructuralConflict { sheet: u16, axis: crate::shift::Axis, at: u32, reason: &'static str },
}

impl Change {
    /// Matches the same review entry after taking other changes, including
    /// charts whose collection indices shifted.
    pub fn same_review_item(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Chart(a), Self::Chart(b)) => a.same_review_item(b),
            _ => self == other,
        }
    }


    /// Our sheet it is on, if any.
    pub fn sheet(&self) -> Option<u16> {
        match *self {
            Change::Cell { sheet, .. } | Change::RowsAdded { sheet, .. } | Change::RowsRemoved { sheet, .. } | Change::SheetRemoved { sheet } | Change::StructuralConflict { sheet, .. } => Some(sheet),
            Change::Style { rect, .. } => Some(rect.sheet),
            Change::Chart(ref chart) => chart.sheet,
            Change::Name(ref name) => name.sheet,
            Change::Table(ref table) => table.at().map(|a| a.sheet),
            Change::Pivot(ref pivot) => pivot.at().map(|a| a.sheet),
            Change::ConditionalFormats(ref rules) => Some(rules.sheet),
            Change::Validations(ref rules) => Some(rules.sheet),
            Change::SheetAdded { .. } => None,
        }
    }

    /// Where to show it: our row and column.
    pub fn at(&self) -> Option<(u16, u32, u32)> {
        match *self {
            Change::Cell { sheet, row, col, .. } => Some((sheet, row, col)),
            Change::RowsAdded { sheet, at, .. } => Some((sheet, at, 0)),
            Change::RowsRemoved { sheet, row, .. } => Some((sheet, row, 0)),
            Change::SheetRemoved { sheet } => Some((sheet, 0, 0)),
            Change::StructuralConflict { sheet, axis, at, .. } => Some(match axis {
                crate::shift::Axis::Rows => (sheet, at, 0),
                crate::shift::Axis::Cols => (sheet, 0, at),
            }),
            Change::Style { rect, .. } => Some((rect.sheet, rect.r0, rect.c0)),
            Change::Chart(ref chart) => chart.anchor(),
            Change::Name(ref name) => name.sheet.map(|sheet| (sheet, 0, 0)),
            Change::Table(ref table) => table.at().map(|a| (a.sheet, a.row, a.col)),
            Change::Pivot(ref pivot) => pivot.at().map(|a| (a.sheet, a.row, a.col)),
            Change::ConditionalFormats(ref rules) => rules.anchor(),
            Change::Validations(ref rules) => rules.anchor(),
            Change::SheetAdded { .. } => None,
        }
    }

    /// Whether this entry is applied through a workbook object collection.
    pub fn is_object(&self) -> bool {
        matches!(self, Self::Chart(_) | Self::Name(_) | Self::Table(_) | Self::Pivot(_) | Self::ConditionalFormats(_) | Self::Validations(_))
    }

    /// Whether it is rows added or removed (taken as rows inserted or
    /// deleted; Take All takes such a sheet whole).
    pub fn is_rows(&self) -> bool {
        matches!(self, Change::RowsAdded { .. } | Change::RowsRemoved { .. })
    }

    /// The change in words: "B4: 12 → 15", "2 rows added before row 7".
    pub fn describe(&self, ours: &Workbook, theirs: &Workbook) -> String {
        let shown = |i: &Input| match i {
            Input::Empty => "empty".to_string(),
            Input::Value(v) => v.display(),
            Input::Formula(f) => f.clone(),
        };
        let rows = |n: u32| if n == 1 { "1 row".to_string() } else { format!("{n} rows") };
        match self {
            Change::Cell { row, col, ours: a, theirs: b, .. } => {
                let at = format!("{}{}", crate::addr::col_name(*col), row + 1);
                match (a, b) {
                    (Input::Empty, b) => format!("{at}: added {}", shown(b)),
                    (a, Input::Empty) => format!("{at}: {} cleared", shown(a)),
                    (a, b) => format!("{at}: {} → {}", shown(a), shown(b)),
                }
            }
            Change::Style { rect, .. } => {
                let first = crate::addr::a1(rect.r0, rect.c0);
                if rect.r0 == rect.r1 { format!("Style {first} changed") }
                else { format!("Style {first}:{} changed", crate::addr::a1(rect.r1, rect.c1)) }
            }
            Change::Chart(chart) => chart.describe(),
            Change::Name(name) => name.describe(),
            Change::Table(table) => table.describe(),
            Change::Pivot(pivot) => pivot.describe(),
            Change::ConditionalFormats(rules) => rules.describe("Conditional formatting"),
            Change::Validations(rules) => rules.describe("Data validation"),
            Change::RowsAdded { at, n, theirs_row, .. } => {
                let first = theirs_row_text(theirs, ours, self, *theirs_row);
                format!("{} added before row {}{}", rows(*n), at + 1, first.map(|t| format!(" ({t})")).unwrap_or_default())
            }
            Change::RowsRemoved { row, n, .. } => format!("{} removed from row {}", rows(*n), row + 1),
            Change::StructuralConflict { axis, at, reason, .. } => match axis {
                crate::shift::Axis::Rows => format!("Row {}: {reason}", at + 1),
                crate::shift::Axis::Cols => format!("Column {}: {reason}", crate::addr::col_name(*at)),
            },
            Change::SheetAdded { name } => format!("Sheet “{name}” added"),
            Change::SheetRemoved { sheet } => format!("Sheet “{}” removed", ours.sheet_name(*sheet)),
        }
    }
}

/// The first few cells of their row, for a description.
fn theirs_row_text(theirs: &Workbook, ours: &Workbook, ch: &Change, row: u32) -> Option<String> {
    let Change::RowsAdded { sheet, .. } = ch else { return None };
    let t = theirs.sheet_id(&ours.sheet_name(*sheet))?;
    let cols = theirs.extent(t).1;
    let shown: Vec<String> = (0..cols).map(|c| theirs.value(t, row, c)).filter(|v| !v.is_empty()).take(3).map(|v| v.display()).collect();
    (!shown.is_empty()).then(|| shown.join(", "))
}

/// What turns `ours` into `theirs`, at most [`MAX_CHANGES`]; the flag says
/// whether more were left out.
pub fn compare(ours: &Workbook, theirs: &Workbook) -> (Vec<Change>, bool) {
    let mut out = Vec::new();
    let mut more = false;
    for s in 0..ours.sheet_count() as u16 {
        match theirs.sheet_id(&ours.sheet_name(s)) {
            Some(t) => more |= compare_sheet(ours, s, theirs, t, &mut out),
            None => out.push(Change::SheetRemoved { sheet: s }),
        }
        if out.len() >= MAX_CHANGES {
            out.truncate(MAX_CHANGES);
            return (out, true);
        }
    }
    for t in 0..theirs.sheet_count() as u16 {
        let name = theirs.sheet_name(t);
        if ours.sheet_id(&name).is_none() {
            if out.len() == MAX_CHANGES { return (out, true); }
            out.push(Change::SheetAdded { name });
        }
    }
    for change in parts::chart_matches(ours, theirs).chain(parts::name_matches(ours, theirs)).chain(rules::compare(ours, theirs)).chain(tables::compare(ours, theirs)) {
        if out.len() == MAX_CHANGES { return (out, true); }
        out.push(change);
    }
    (out, more)
}

/// Sparse, positional cell inputs for tracking edits and merging copies
/// of the same sheet layout. Formula results and spill followers are excluded.
#[derive(Debug, Clone, PartialEq)]
pub struct CellSnapshot {
    pub(crate) sheets: BTreeMap<String, BTreeMap<(u32, u32), Input>>,
    pub(crate) axes: BTreeMap<String, [crate::axis_ids::AxisIds; 2]>,
}

pub struct MergePlan {
    pub incoming: Vec<Change>,
    pub conflicts: Vec<Change>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum MergeError {
    SheetLayout,
    TooManyChanges,
    StructuralAlignment,
}

impl CellSnapshot {
    pub fn capture(wb: &Workbook) -> Self {
        let sheets = (0..wb.sheet_count() as u16).map(|sheet| {
            let inputs = Rows::of(wb, sheet).rows.into_iter().flat_map(|(row, cells)| {
                cells.into_iter().map(move |(col, _)| ((row, col), wb.input(CellAddr::new(sheet, row, col))))
            }).collect();
            (wb.sheet_name(sheet), inputs)
        }).collect();
        let axes = (0..wb.sheet_count() as u16).map(|sheet| {
            (wb.sheet_name(sheet), wb.sheet(sheet).axis_ids.clone())
        }).collect();
        Self { sheets, axes }
    }

    /// Compare by address, with the base deciding which side changed.
    /// Refuse a truncated plan before callers apply any of it.
    pub fn plan(&self, ours: &Workbook, theirs: &Self) -> Result<MergePlan, MergeError> {
        let current = Self::capture(ours);
        self.plan_with(ours, &current, theirs)
    }

    pub fn changes_since(&self, wb: &Workbook) -> Result<Vec<Change>, MergeError> {
        let current = Self::capture(wb);
        let mut changes = current.plan_with(wb, &current, self)?.incoming;
        for change in &mut changes {
            if let Change::Cell { ours, theirs, .. } = change { std::mem::swap(ours, theirs); }
        }
        Ok(changes)
    }

    pub(crate) fn plan_with(&self, wb: &Workbook, current: &Self, theirs: &Self) -> Result<MergePlan, MergeError> {
        if self.sheets.keys().ne(current.sheets.keys()) || self.sheets.keys().ne(theirs.sheets.keys()) {
            return Err(MergeError::SheetLayout);
        }
        let mut plan = MergePlan { incoming: Vec::new(), conflicts: Vec::new() };
        for (name, base) in &self.sheets {
            let sheet = wb.sheet_id(name).expect("matching sheet");
            let local = &current.sheets[name];
            let other = &theirs.sheets[name];
            // Only incoming edits can require either an apply or a decision.
            for (&(row, col), value) in other.iter().chain(base.iter().filter(|(key, _)| !other.contains_key(key))) {
                let key = (row, col);
                let before = base.get(&key).unwrap_or(&Input::Empty);
                let ours = local.get(&key).unwrap_or(&Input::Empty);
                let theirs = if other.contains_key(&key) { value } else { &Input::Empty };
                if theirs == before || ours == theirs { continue; }
                if plan.incoming.len() + plan.conflicts.len() == MAX_CHANGES {
                    return Err(MergeError::TooManyChanges);
                }
                let change = Change::Cell { sheet, row, col, ours: ours.clone(), theirs: theirs.clone() };
                if ours == before { plan.incoming.push(change); } else { plan.conflicts.push(change); }
            }
        }
        Ok(plan)
    }
}

/// A cell as compared: its text (value, or formula in relative form) and
/// the formula group it came from.
#[derive(Debug, Clone)]
struct Key {
    text: Arc<str>,
    group: Option<GroupId>,
}

/// A sheet's cells by row: (column, key), columns in order.
struct Rows {
    rows: HashMap<u32, Vec<(u32, Key)>>,
    n: u32,
}

impl Rows {
    fn of(wb: &Workbook, sheet: u16) -> Rows {
        Self::outside(wb, sheet, &[])
    }

    fn outside(wb: &Workbook, sheet: u16, excluded: &[Rect]) -> Rows {
        let (n, cols) = wb.extent(sheet);
        let mut rows: HashMap<u32, Vec<(u32, Key)>> = HashMap::new();
        if n == 0 || cols == 0 {
            return Rows { rows, n };
        }
        // Spilled cells belong to their formula.
        let spills: Vec<Rect> = wb.spills().filter(|(a, _)| a.sheet == sheet).filter_map(|(a, _)| wb.spill_area(*a)).collect();
        let spilled = |r: u32, c: u32| spills.iter().any(|a| a.contains(CellAddr::new(sheet, r, c)) && !(a.r0 == r && a.c0 == c));
        let rect = Rect { sheet, r0: 0, c0: 0, r1: n - 1, c1: cols - 1 };
        let excluded = |r, c| excluded.iter().any(|area| area.contains(CellAddr::new(sheet, r, c)));
        let mut formulas: HashMap<(u32, u32), Key> = HashMap::new();
        for (g, grp) in wb.groups() {
            if grp.sheet != sheet {
                continue;
            }
            let text: Arc<str> = formula::to_a1(&grp.expr, KEY_ROW, grp.col, &|id| wb.sheet_name(id)).into();
            for r in grp.r0..=grp.r1 {
                if excluded(r, grp.col) { continue; }
                formulas.insert((r, grp.col), Key { text: text.clone(), group: Some(g) });
            }
        }
        wb.each(&rect, &mut |r, c, v| {
            if formulas.contains_key(&(r, c)) || spilled(r, c) || excluded(r, c) {
                return;
            }
            rows.entry(r).or_default().push((c, Key { text: value_key(&v).into(), group: None }));
        });
        for ((r, c), k) in formulas {
            rows.entry(r).or_default().push((c, k));
        }
        for v in rows.values_mut() {
            v.sort_by_key(|x| x.0);
        }
        // Empty rows at the end don't count.
        let n = rows.keys().max().map_or(0, |r| r + 1);
        Rows { rows, n }
    }

    fn hash(&self, r: u32) -> u64 {
        let mut h = DefaultHasher::new();
        if let Some(v) = self.rows.get(&r) {
            for (c, k) in v {
                c.hash(&mut h);
                k.text.hash(&mut h);
            }
        }
        h.finish()
    }

    fn cells(&self, r: u32) -> &[(u32, Key)] {
        self.rows.get(&r).map_or(&[], |v| v.as_slice())
    }
}

fn value_key(v: &Value) -> String {
    format!("{v:?}")
}

/// Pairs of matched rows (ours, theirs), in order: common start and end,
/// and a longest common subsequence of row hashes between them when small
/// enough.
fn match_rows(a: &[u64], b: &[u64]) -> Vec<(u32, u32)> {
    let mut p = 0;
    while p < a.len() && p < b.len() && a[p] == b[p] {
        p += 1;
    }
    let mut s = 0;
    while s < a.len() - p && s < b.len() - p && a[a.len() - 1 - s] == b[b.len() - 1 - s] {
        s += 1;
    }
    let mut out: Vec<(u32, u32)> = (0..p).map(|i| (i as u32, i as u32)).collect();
    let (ma, mb) = (&a[p..a.len() - s], &b[p..b.len() - s]);
    if !ma.is_empty() && !mb.is_empty() && ma.len() * mb.len() <= LCS_MAX {
        // Lengths of the common subsequences of the suffixes.
        let w = mb.len() + 1;
        let mut t = vec![0u32; (ma.len() + 1) * w];
        for i in (0..ma.len()).rev() {
            for j in (0..mb.len()).rev() {
                t[i * w + j] = if ma[i] == mb[j] { t[(i + 1) * w + j + 1] + 1 } else { t[(i + 1) * w + j].max(t[i * w + j + 1]) };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < ma.len() && j < mb.len() {
            if ma[i] == mb[j] {
                out.push(((p + i) as u32, (p + j) as u32));
                i += 1;
                j += 1;
            } else if t[(i + 1) * w + j] >= t[i * w + j + 1] {
                i += 1;
            } else {
                j += 1;
            }
        }
    }
    out.extend((0..s).map(|k| ((a.len() - s + k) as u32, (b.len() - s + k) as u32)));
    out
}

/// Adds sheet `s`'s changes (theirs `t`); true if some were left out.
fn compare_sheet(ours: &Workbook, s: u16, theirs: &Workbook, t: u16, out: &mut Vec<Change>) -> bool {
    let ra = Rows::outside(ours, s, &tables::pivot_areas(ours, s));
    let rb = Rows::outside(theirs, t, &tables::pivot_areas(theirs, t));
    let ha: Vec<u64> = (0..ra.n).map(|r| ra.hash(r)).collect();
    let hb: Vec<u64> = (0..rb.n).map(|r| rb.hash(r)).collect();
    let mut pairs = match_rows(&ha, &hb);
    pairs.push((ra.n, rb.n));
    let compare_styles = !ours.sheet(s).styles.is_empty() || !theirs.sheet(t).styles.is_empty();
    let mut style_pairs = Vec::new();
    let (mut i, mut j) = (0u32, 0u32);
    for (mi, mj) in pairs {
        // The gap before this match: rows paired by likeness, the rest added
        // or removed.
        for step in align_gap(&ra, i..mi, &rb, j..mj) {
            match step {
                Step::Pair(x, y) => {
                    cells(ours, &ra, s, x, theirs, &rb, y, out);
                    if compare_styles { parts::pair(&mut style_pairs, x, y, 1); }
                }
                Step::Add(at, y, n) => out.push(Change::RowsAdded { sheet: s, at, n, theirs_row: y }),
                Step::Remove(x, n) => out.push(Change::RowsRemoved { sheet: s, row: x, n }),
            }
            if out.len() >= MAX_CHANGES {
                return true;
            }
        }
        if compare_styles && mi < ra.n && mj < rb.n {
            parts::pair(&mut style_pairs, mi, mj, 1);
        }
        i = mi + 1;
        j = mj + 1;
    }
    if !compare_styles { return false; }
    parts::pair(&mut style_pairs, ra.n, rb.n, MAX_ROWS - ra.n.max(rb.n));
    parts::styles(ours, s, theirs, t, &style_pairs, out)
}

/// How a gap between matched rows lines up.
enum Step {
    /// Our row and theirs, compared cell by cell.
    Pair(u32, u32),
    /// Before our row, their rows from the second, this many.
    Add(u32, u32, u32),
    /// Our rows from the first, this many.
    Remove(u32, u32),
}

/// Above this many row pairs a gap is lined up in order.
const GAP_MAX: usize = 10_000;

/// Cells two rows have the same.
fn likeness(a: &[(u32, Key)], b: &[(u32, Key)]) -> u32 {
    let (mut x, mut y, mut n) = (0, 0, 0);
    while x < a.len() && y < b.len() {
        match a[x].0.cmp(&b[y].0) {
            std::cmp::Ordering::Less => x += 1,
            std::cmp::Ordering::Greater => y += 1,
            std::cmp::Ordering::Equal => {
                n += u32::from(a[x].1.text == b[y].1.text);
                x += 1;
                y += 1;
            }
        }
    }
    n
}

/// Lines up our rows `a` with their rows `b`: rows sharing cells paired so
/// the most cells line up, the others added or removed (runs together).
fn align_gap(ra: &Rows, a: std::ops::Range<u32>, rb: &Rows, b: std::ops::Range<u32>) -> Vec<Step> {
    let (la, lb) = (a.len(), b.len());
    // Which pairs: (our offset, their offset).
    let mut pairs: Vec<(usize, usize)> = Vec::new();
    if la > 0 && lb > 0 && la * lb <= GAP_MAX {
        let w = lb + 1;
        let sc = |i: usize, j: usize| likeness(ra.cells(a.start + i as u32), rb.cells(b.start + j as u32));
        let mut t = vec![0u32; (la + 1) * w];
        for i in (0..la).rev() {
            for j in (0..lb).rev() {
                let k = sc(i, j);
                let take = if k > 0 { t[(i + 1) * w + j + 1] + k } else { 0 };
                t[i * w + j] = take.max(t[(i + 1) * w + j]).max(t[i * w + j + 1]);
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < la && j < lb {
            let k = sc(i, j);
            if k > 0 && t[i * w + j] == t[(i + 1) * w + j + 1] + k {
                pairs.push((i, j));
                i += 1;
                j += 1;
            } else if t[i * w + j] == t[(i + 1) * w + j] {
                i += 1;
            } else {
                j += 1;
            }
        }
        // Rows with nothing alike: still paired in order when both sides
        // have them between the same pairs (an edited row, not a swap).
        let mut all = Vec::new();
        let (mut pi, mut pj) = (0, 0);
        for &(x, y) in pairs.iter().chain(std::iter::once(&(la, lb))) {
            let k = (x - pi).min(y - pj);
            all.extend((0..k).map(|d| (pi + d, pj + d)));
            if x < la {
                all.push((x, y));
            }
            pi = x + 1;
            pj = y + 1;
        }
        pairs = all;
    } else {
        pairs = (0..la.min(lb)).map(|d| (d, d)).collect();
    }
    let mut out = Vec::new();
    let (mut pi, mut pj) = (0usize, 0usize);
    for &(x, y) in pairs.iter().chain(std::iter::once(&(la, lb))) {
        if y > pj {
            out.push(Step::Add(a.start + pi as u32, b.start + pj as u32, (y - pj) as u32));
        }
        if x > pi {
            out.push(Step::Remove(a.start + pi as u32, (x - pi) as u32));
        }
        if x < la {
            out.push(Step::Pair(a.start + x as u32, b.start + y as u32));
        }
        pi = x + 1;
        pj = y + 1;
    }
    out
}

/// Changes between our row `i` and their row `j`.
#[allow(clippy::too_many_arguments)]
fn cells(ours: &Workbook, ra: &Rows, s: u16, i: u32, theirs: &Workbook, rb: &Rows, j: u32, out: &mut Vec<Change>) {
    let (a, b) = (ra.cells(i), rb.cells(j));
    let (mut x, mut y) = (0, 0);
    while x < a.len() || y < b.len() {
        let ca = a.get(x).map_or(u32::MAX, |e| e.0);
        let cb = b.get(y).map_or(u32::MAX, |e| e.0);
        let col = ca.min(cb);
        let ka = (ca == col).then(|| &a[x].1);
        let kb = (cb == col).then(|| &b[y].1);
        if ca == col {
            x += 1;
        }
        if cb == col {
            y += 1;
        }
        if ka.map(|k| &k.text) == kb.map(|k| &k.text) {
            continue;
        }
        let mine = if ka.is_some() { ours.input(CellAddr::new(s, i, col)) } else { Input::Empty };
        let other = match kb {
            None => Input::Empty,
            Some(Key { group: Some(g), .. }) => Input::Formula(formula::to_a1(&theirs.group(*g).expr, i, col, &|id| theirs.sheet_name(id))),
            Some(_) => match theirs.input(CellAddr::new(theirs_sheet(theirs, ours, s), j, col)) {
                Input::Formula(_) => Input::Empty,
                v => v,
            },
        };
        out.push(Change::Cell { sheet: s, row: i, col, ours: mine, theirs: other });
    }
}

fn theirs_sheet(theirs: &Workbook, ours: &Workbook, s: u16) -> u16 {
    theirs.sheet_id(&ours.sheet_name(s)).unwrap_or(0)
}

/// The edits that take `ch` from theirs, in order: a cell takes their
/// cell; rows added are inserted (references to the rows below follow)
/// and given their cells; rows removed are deleted. Sheets added or
/// removed can't be taken (None).
pub fn take(ours: &Workbook, theirs: &Workbook, ch: &Change) -> Option<Vec<GridOp>> {
    use crate::shift::{shift_op, Axis};
    match ch {
        Change::Cell { sheet, row, col, theirs: input, .. } => {
            if (*sheet as usize) >= ours.sheet_count() {
                return None;
            }
            let addr = CellAddr::new(*sheet, *row, *col);
            if ours.pivot_at(addr).is_some() { return None; }
            Some(vec![GridOp::set(addr, input.clone())])
        }
        Change::Style { rect, ours: before, theirs: after } => {
            if rect.sheet as usize >= ours.sheet_count() { return None; }
            if ours.style_snapshot(rect).spans != parts::style_spans(*rect, before) { return None; }
            Some(vec![GridOp::style(*rect, (**after).clone())])
        }
        Change::Chart(_) | Change::Name(_) | Change::Table(_) | Change::Pivot(_) | Change::ConditionalFormats(_) | Change::Validations(_) => {
            let (ops, n) = take_objects(ours, theirs, std::slice::from_ref(ch));
            (n != 0).then_some(ops)
        }
        Change::RowsAdded { sheet, at, n, theirs_row } => {
            let t = theirs.sheet_id(&ours.sheet_name(*sheet))?;
            let mut ops = vec![shift_op(ours, *sheet, Axis::Rows, *at, *n as i64)?];
            let cols = theirs.extent(t).1;
            let spills: Vec<Rect> = theirs.spills().filter(|(a, _)| a.sheet == t).filter_map(|(a, _)| theirs.spill_area(*a)).collect();
            let pivot_areas = tables::pivot_areas(theirs, t);
            let rows: Vec<Vec<Input>> = (0..*n)
                .map(|k| {
                    let (j, r) = (theirs_row + k, at + k);
                    (0..cols)
                        .map(|c| match theirs.group_at(t, j, c) {
                            _ if pivot_areas.iter().any(|area| area.contains(CellAddr::new(t, j, c))) => Input::Empty,
                            Some(g) => Input::Formula(formula::to_a1(&theirs.group(g).expr, r, c, &|id| theirs.sheet_name(id))),
                            None if spills.iter().any(|a| a.contains(CellAddr::new(t, j, c)) && !(a.r0 == j && a.c0 == c)) => Input::Empty,
                            None => match theirs.value(t, j, c) {
                                Value::Empty => Input::Empty,
                                v => Input::Value(v),
                            },
                        })
                        .collect()
                })
                .collect();
            if rows.iter().flatten().any(|i| *i != Input::Empty) {
                ops.push(GridOp::block(*sheet, *at, 0, rows));
            }
            let source = Rect { sheet: t, r0: *theirs_row, r1: *theirs_row + *n - 1, c0: 0, c1: 0 };
            let target = Rect { sheet: *sheet, r0: *at, r1: *at + *n - 1, ..source };
            if let Some(op) = parts::row_styles(theirs, source, target) { ops.push(op); }
            Some(ops)
        }
        Change::RowsRemoved { sheet, row, n } => shift_op(ours, *sheet, Axis::Rows, *row, -(*n as i64)).map(|op| vec![op]),
        Change::SheetAdded { .. } | Change::SheetRemoved { .. } | Change::StructuralConflict { .. } => None,
    }
}

/// Builds a combined update for workbook object changes in a review selection.
pub fn take_objects(ours: &Workbook, theirs: &Workbook, changes: &[Change]) -> (Vec<GridOp>, usize) {
    let (mut ops, count) = parts::take_objects(ours, theirs, changes);
    let rule_count = rules::take(ours, theirs, changes, &mut ops);
    let table_count = tables::take(ours, theirs, changes, &mut ops);
    (ops, count + rule_count + table_count)
}

/// Our sheet `s` made the same as theirs of its name: cells, formulas and
/// styles over both sheets' used area.
pub fn take_sheet(ours: &Workbook, theirs: &Workbook, s: u16) -> Option<GridOp> {
    if s as usize >= ours.sheet_count() {
        return None;
    }
    let t = theirs.sheet_id(&ours.sheet_name(s))?;
    if ours.pivots().iter().any(|p| p.dest.sheet == s)
        || theirs.pivots().iter().any(|p| p.dest.sheet == t) {
        return None;
    }
    let ((na, ca), (nb, cb)) = (ours.extent(s), theirs.extent(t));
    let (n, c) = (na.max(nb), ca.max(cb));
    if n == 0 || c == 0 {
        return None;
    }
    let rect = Rect { sheet: s, r0: 0, c0: 0, r1: n - 1, c1: c - 1 };
    let theirs_rect = Rect { sheet: t, ..rect };
    let mut fills = Vec::new();
    let mut in_group: HashMap<(u32, u32), ()> = HashMap::new();
    for (_, g) in theirs.groups() {
        if g.sheet != t {
            continue;
        }
        fills.push(Fill { col: g.col, r0: g.r0, r1: g.r1, src: formula::to_a1(&g.expr, g.r0, g.col, &|id| theirs.sheet_name(id)) });
        for r in g.r0..=g.r1 {
            in_group.insert((r, g.col), ());
        }
    }
    let spills: Vec<Rect> = theirs.spills().filter(|(a, _)| a.sheet == t).filter_map(|(a, _)| theirs.spill_area(*a)).collect();
    let mut values = Vec::new();
    theirs.each(&theirs_rect, &mut |r, c, v| {
        if in_group.contains_key(&(r, c)) || spills.iter().any(|a| a.contains(CellAddr::new(t, r, c))) {
            return;
        }
        values.push((r, c, v));
    });
    let mut styles = theirs.style_snapshot(&Rect { r1: MAX_ROWS - 1, c1: MAX_COLS - 1, ..theirs_rect });
    styles.rect.sheet = s;
    Some(GridOp::Batch(vec![GridOp::Replace(Region { rect, values, fills }), GridOp::Restyle(styles)]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use officina_core::history::Op;

    #[test]
    fn table_and_pivot_review_leaves_occupied_destinations_pending() {
        use crate::pivot::{Agg, DataField, Pivot, PivotField};
        use crate::tables::Table;

        let mut ours = book(&[&["Region", "Amount"], &["East", "3"]]);
        let mut theirs = book(&[&["Region", "Amount"], &["East", "3"]]);
        let table = |name: &str| Table {
            name: name.into(), rect: Rect { sheet: 0, r0: 0, r1: 1, c0: 1, c1: 1 },
            header: true, totals: false, columns: vec!["Amount".into()], style: None, stripes: false,
        };
        ours.add_table(table("KeepTable")).unwrap();
        theirs.add_table(table("NewTable")).unwrap();
        let occupied = CellAddr::new(0, 0, 4);
        ours.set_value(occupied, Value::str("Local note"));
        let mut pivot = Pivot::new("Summary", Rect { sheet: 0, r0: 0, r1: 1, c0: 0, c1: 1 }, occupied);
        pivot.rows.push(PivotField::new(0));
        pivot.data.push(DataField { field: 1, agg: Agg::Sum, name: None });
        theirs.add_pivot(pivot).unwrap();

        let (changes, _) = compare(&ours, &theirs);
        let table = changes.iter().find(|ch| matches!(ch, Change::Table(t) if t.describe() == "Table NewTable added")).unwrap();
        let pivot = changes.iter().find(|ch| matches!(ch, Change::Pivot(_))).unwrap();
        assert!(take(&ours, &theirs, table).is_none());
        assert!(take(&ours, &theirs, pivot).is_none());
        assert_eq!(ours.get(occupied), Value::str("Local note"));
        assert!(ours.table("KeepTable").is_some());
    }

    #[test]
    fn workbook_rule_review_remaps_sheet_ids_and_refuses_missing_references() {
        use crate::condfmt::CmpOp;
        use crate::validation::{Allow, Validation};

        let mut ours = Workbook::new();
        let mut theirs = Workbook::new();
        for wb in [&mut ours, &mut theirs] {
            wb.add_sheet("Entry").unwrap();
            wb.add_sheet("Options").unwrap();
        }
        assert!(theirs.move_sheet(1, 0));
        let rule = |wb: &Workbook, sheet, text| Validation::new(
            sheet, vec![Rect::cell(CellAddr::new(sheet, 3, 1))], Allow::List, CmpOp::Equal,
            Some(wb.validation_formula(text, sheet, (3, 1)).unwrap()), None,
        );
        ours.add_validation(rule(&ours, 0, "=Options!$A$1:$A$3")).unwrap();
        theirs.add_validation(rule(&theirs, 1, "=Options!$A$1:$A$3")).unwrap();
        assert!(compare(&ours, &theirs).0.is_empty());

        theirs.add_sheet("IncomingOnly").unwrap();
        theirs.replace_validations(vec![rule(&theirs, 1, "=IncomingOnly!$A$1:$A$3")]);
        let (changes, _) = compare(&ours, &theirs);
        let change = changes.iter().find(|ch| matches!(ch, Change::Validations(_))).unwrap();
        assert!(take(&ours, &theirs, change).is_none());
        assert_eq!(ours.validations(), &[rule(&ours, 0, "=Options!$A$1:$A$3")]);
    }

    #[test]
    fn styles_follow_matched_rows_and_take_new_row_styles() {
        use crate::style::Style;

        let mut ours = book(&[&["Name"], &["one"], &["two"]]);
        let mut theirs = book(&[&["Name"], &["one"], &["added"], &["two"]]);
        let style = Style { fill: Some(0x112233), ..Style::default() };
        let inserted = Style { fill: Some(0xaabbcc), ..Style::default() };
        theirs.set_style(&Rect::cell(CellAddr::new(0, 3, 1)), style.clone());
        theirs.set_style(&Rect::cell(CellAddr::new(0, 2, 5)), inserted.clone());
        let (changes, more) = compare(&ours, &theirs);
        assert!(!more);
        assert_eq!(changes.len(), 2);
        let aligned = changes.iter().find(|c| matches!(c, Change::Style { .. })).unwrap();
        assert_eq!(aligned.at(), Some((0, 2, 1)));
        take(&ours, &theirs, aligned).unwrap().into_iter().for_each(|op| { op.apply(&mut ours); });
        let (changes, _) = compare(&ours, &theirs);
        assert!(matches!(changes.as_slice(), [Change::RowsAdded { .. }]));
        take(&ours, &theirs, &changes[0]).unwrap().into_iter().for_each(|op| { op.apply(&mut ours); });
        assert_eq!(ours.style(CellAddr::new(0, 3, 1)), &style);
        assert_eq!(ours.style(CellAddr::new(0, 2, 5)), &inserted);
        assert!(compare(&ours, &theirs).0.is_empty());
    }

    fn book(rows: &[&[&str]]) -> Workbook {
        let mut wb = Workbook::new();
        wb.add_sheet("Sheet1").unwrap();
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

    fn texts(wb: &Workbook) -> Vec<Vec<String>> {
        let (n, c) = wb.extent(0);
        let mut v: Vec<Vec<String>> = (0..n).map(|r| (0..c).map(|k| match wb.input(CellAddr::new(0, r, k)) {
            Input::Empty => String::new(),
            Input::Value(v) => v.display(),
            Input::Formula(f) => f,
        }).collect()).collect();
        while v.last().is_some_and(|r| r.iter().all(String::is_empty)) {
            v.pop();
        }
        v
    }

    #[test]
    fn the_same_book_has_no_changes() {
        let rows: &[&[&str]] = &[&["Name", "Qty"], &["Pens", "3"], &["Ink", "=B2*2"]];
        assert_eq!(compare(&book(rows), &book(rows)).0, vec![]);
    }

    #[test]
    fn cells_rows_and_sheets() {
        let a = book(&[&["Name", "Qty", "Total"], &["Pens", "3", "=B2*2"], &["Ink", "4", "=B3*2"], &["Paper", "5", "=B4*2"]]);
        // Theirs: a row added after the header, Ink's quantity changed,
        // Paper removed, and a sheet more.
        let mut b = book(&[&["Name", "Qty", "Total"], &["Clips", "9", "=B2*2"], &["Pens", "3", "=B3*2"], &["Ink", "7", "=B4*2"]]);
        b.add_sheet("Notes").unwrap();
        let (ch, more) = compare(&a, &b);
        assert!(!more);
        assert_eq!(ch[0], Change::RowsAdded { sheet: 0, at: 1, n: 1, theirs_row: 1 }, "{ch:?}");
        // Pens moved down a row but its formula, relative, still matches.
        assert!(!ch.iter().any(|c| matches!(c, Change::Cell { row: 1, .. })), "{ch:?}");
        assert!(ch.contains(&Change::Cell { sheet: 0, row: 2, col: 1, ours: Input::Value(Value::Num(4.0)), theirs: Input::Value(Value::Num(7.0)) }), "{ch:?}");
        assert!(ch.contains(&Change::RowsRemoved { sheet: 0, row: 3, n: 1 }), "{ch:?}");
        assert!(ch.contains(&Change::SheetAdded { name: "Notes".into() }));
        let words: Vec<String> = ch.iter().map(|c| c.describe(&a, &b)).collect();
        assert!(words.contains(&"B3: 4 → 7".to_string()), "{words:?}");
        assert!(words.contains(&"1 row added before row 2 (Clips, 9, 18)".to_string()), "{words:?}");
        assert!(words.contains(&"1 row removed from row 4".to_string()), "{words:?}");
        assert!(words.contains(&"Sheet “Notes” added".to_string()), "{words:?}");
    }

    #[test]
    fn a_formula_cell_is_taken_for_our_row() {
        let rows: &[&[&str]] = &[&["1", "=A1+1"], &["2", "=A2+1"]];
        let a = book(rows);
        let b = book(&[&["X"], &["1", "=A2*10"], &["2", "=A3+1"]]);
        let (ch, _) = compare(&a, &b);
        // Row 1 of ours matched their row 2: its formula differs.
        let c = ch.iter().find(|c| matches!(c, Change::Cell { .. })).unwrap().clone();
        assert_eq!(c, Change::Cell { sheet: 0, row: 0, col: 1, ours: Input::Formula("=A1+1".into()), theirs: Input::Formula("=A1*10".into()) });
        let mut w = book(rows);
        let ops = take(&w, &b, &c).unwrap();
        let inv: Vec<GridOp> = ops.into_iter().map(|op| op.apply(&mut w)).collect();
        w.recalc();
        assert_eq!(w.value(0, 0, 1), Value::Num(10.0));
        for op in inv.into_iter().rev() {
            op.apply(&mut w);
        }
        w.recalc();
        assert_eq!(texts(&w), texts(&a));
    }

    #[test]
    fn taking_rows_takes_the_sheet_and_undoes() {
        let rows: &[&[&str]] = &[&["Name", "Qty"], &["Pens", "3"], &["Ink", "=B2+1"]];
        let a = book(rows);
        let b = book(&[&["Name", "Qty"], &["Clips", "9"], &["Pens", "3"], &["Ink", "=B3+1"]]);
        let (ch, _) = compare(&a, &b);
        let added = ch.iter().find(|c| c.is_rows()).unwrap();
        let mut w = book(rows);
        let inv: Vec<GridOp> = take(&w, &b, added).unwrap().into_iter().map(|op| op.apply(&mut w)).collect();
        w.recalc();
        assert_eq!(texts(&w), texts(&b));
        assert_eq!(w.value(0, 3, 1), Value::Num(4.0));
        assert_eq!(compare(&w, &b).0, vec![]);
        for op in inv.into_iter().rev() {
            op.apply(&mut w);
        }
        w.recalc();
        assert_eq!(texts(&w), texts(&a));
    }

    #[test]
    fn random_books_compare_to_their_changes() {
        // Taking every change (cells one by one, or the sheet) gives theirs.
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut rnd = |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        for _ in 0..300 {
            let rows = 1 + rnd(12) as usize;
            let mut a: Vec<Vec<String>> = (0..rows).map(|r| (0..3).map(|c| if rnd(4) == 0 { String::new() } else { format!("{}", r * 10 + c) }).collect()).collect();
            let mut b = a.clone();
            for _ in 0..rnd(4) {
                match rnd(3) {
                    0 if !b.is_empty() => {
                        let k = rnd(b.len() as u64) as usize;
                        b.remove(k);
                    }
                    1 => {
                        let k = rnd(b.len() as u64 + 1) as usize;
                        b.insert(k, vec![format!("n{}", rnd(100)), String::new(), "=1+1".into()]);
                    }
                    _ if !b.is_empty() => {
                        let k = rnd(b.len() as u64) as usize;
                        b[k][rnd(3) as usize] = format!("x{}", rnd(100));
                    }
                    _ => {}
                }
            }
            if rnd(2) == 0 {
                a.push(vec!["end".into()]);
                b.push(vec!["end".into()]);
            }
            let make = |v: &Vec<Vec<String>>| {
                let strs: Vec<Vec<&str>> = v.iter().map(|r| r.iter().map(|s| s.as_str()).collect()).collect();
                let refs: Vec<&[&str]> = strs.iter().map(|r| r.as_slice()).collect();
                book(&refs)
            };
            let (wa, wb) = (make(&a), make(&b));
            let (ch, _) = compare(&wa, &wb);
            let mut w = make(&a);
            if rnd(2) == 0 && ch.iter().any(|c| c.is_rows()) {
                take_sheet(&w, &wb, 0).unwrap().apply(&mut w);
            } else {
                // One at a time, comparing again after each.
                let mut left = ch.clone();
                let mut rounds = 0;
                while let Some(c) = left.first().cloned() {
                    for op in take(&w, &wb, &c).unwrap() {
                        op.apply(&mut w);
                    }
                    w.recalc();
                    left = compare(&w, &wb).0;
                    rounds += 1;
                    assert!(rounds < 100, "{a:?} -> {b:?}");
                }
            }
            w.recalc();
            assert_eq!(compare(&w, &wb).0, vec![], "{a:?} -> {b:?}: {ch:?}");
        }
    }

    #[test]
    fn a_big_sheet_compares_fast() {
        let make = |extra: bool| {
            let mut wb = Workbook::new();
            wb.add_sheet("Sheet1").unwrap();
            let n = 200_000u32;
            let vals: Vec<f64> = (0..n).map(|r| r as f64).collect();
            wb.set_numbers(0, 0, 0, &vals);
            wb.fill_formula(0, 1, 0, n - 1, "=A1*2").unwrap();
            if extra {
                wb.set_input(CellAddr::new(0, 100_000, 0), "7").unwrap();
                wb.set_input(CellAddr::new(0, 150_000, 2), "note").unwrap();
            }
            wb.recalc();
            wb
        };
        let (a, b) = (make(false), make(true));
        let t = std::time::Instant::now();
        let (ch, _) = compare(&a, &b);
        let took = t.elapsed();
        assert_eq!(ch.len(), 2, "{ch:?}");
        assert!(took.as_secs_f64() < 5.0, "{took:?}");
        eprintln!("200,000 rows compared in {took:?}");
    }
}
