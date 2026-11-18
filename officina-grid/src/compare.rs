//! Comparing two workbooks: what would turn ours into theirs, as a list of
//! changes a person can look through and take one at a time (officina.md
//! §16.68).
//!
//! Sheets are matched by name. Within a sheet, rows are matched by their
//! contents (formulas compared in their relative form, so a row that moved
//! keeps matching), with a longest-common-subsequence over row hashes between
//! the common start and end; rows left over between two matches are paired in
//! order and compared cell by cell, and what is left after that are rows
//! added or removed. Styles are not compared.

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use crate::addr::{CellAddr, Rect};
use crate::book::{Input, Workbook};
use crate::deps::GroupId;
use crate::formula;
use crate::ops::{Fill, GridOp, Region};
use crate::value::Value;

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
    /// `n` rows of theirs, from their row `theirs_row`, that ours doesn't
    /// have; they would go before our row `at`.
    RowsAdded { sheet: u16, at: u32, n: u32, theirs_row: u32 },
    /// `n` rows of ours from `row` that theirs doesn't have.
    RowsRemoved { sheet: u16, row: u32, n: u32 },
    /// A sheet only theirs has.
    SheetAdded { name: String },
    /// A sheet only ours has.
    SheetRemoved { sheet: u16 },
}

impl Change {
    /// Our sheet it is on, if any.
    pub fn sheet(&self) -> Option<u16> {
        match *self {
            Change::Cell { sheet, .. } | Change::RowsAdded { sheet, .. } | Change::RowsRemoved { sheet, .. } | Change::SheetRemoved { sheet } => Some(sheet),
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
            Change::SheetAdded { .. } => None,
        }
    }

    /// Whether taking it takes the whole sheet (rows added or removed).
    pub fn whole_sheet(&self) -> bool {
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
            Change::RowsAdded { at, n, theirs_row, .. } => {
                let first = theirs_row_text(theirs, ours, self, *theirs_row);
                format!("{} added before row {}{}", rows(*n), at + 1, first.map(|t| format!(" ({t})")).unwrap_or_default())
            }
            Change::RowsRemoved { row, n, .. } => format!("{} removed from row {}", rows(*n), row + 1),
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
            out.push(Change::SheetAdded { name });
        }
    }
    (out, more)
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
        let (n, cols) = wb.extent(sheet);
        let mut rows: HashMap<u32, Vec<(u32, Key)>> = HashMap::new();
        if n == 0 || cols == 0 {
            return Rows { rows, n };
        }
        // Spilled cells belong to their formula.
        let spills: Vec<Rect> = wb.spills().filter(|(a, _)| a.sheet == sheet).filter_map(|(a, _)| wb.spill_area(*a)).collect();
        let spilled = |r: u32, c: u32| spills.iter().any(|a| a.contains(CellAddr::new(sheet, r, c)) && !(a.r0 == r && a.c0 == c));
        let rect = Rect { sheet, r0: 0, c0: 0, r1: n - 1, c1: cols - 1 };
        let mut formulas: HashMap<(u32, u32), Key> = HashMap::new();
        for (g, grp) in wb.groups() {
            if grp.sheet != sheet {
                continue;
            }
            let text: Arc<str> = formula::to_a1(&grp.expr, KEY_ROW, grp.col, &|id| wb.sheet_name(id)).into();
            for r in grp.r0..=grp.r1 {
                formulas.insert((r, grp.col), Key { text: text.clone(), group: Some(g) });
            }
        }
        wb.each(&rect, &mut |r, c, v| {
            if formulas.contains_key(&(r, c)) || spilled(r, c) {
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
    let (ra, rb) = (Rows::of(ours, s), Rows::of(theirs, t));
    let ha: Vec<u64> = (0..ra.n).map(|r| ra.hash(r)).collect();
    let hb: Vec<u64> = (0..rb.n).map(|r| rb.hash(r)).collect();
    let mut pairs = match_rows(&ha, &hb);
    pairs.push((ra.n, rb.n));
    let (mut i, mut j) = (0u32, 0u32);
    for (mi, mj) in pairs {
        // The gap before this match: rows paired by likeness, the rest added
        // or removed.
        for step in align_gap(&ra, i..mi, &rb, j..mj) {
            match step {
                Step::Pair(x, y) => cells(ours, &ra, s, x, theirs, &rb, y, out),
                Step::Add(at, y, n) => out.push(Change::RowsAdded { sheet: s, at, n, theirs_row: y }),
                Step::Remove(x, n) => out.push(Change::RowsRemoved { sheet: s, row: x, n }),
            }
            if out.len() >= MAX_CHANGES {
                return true;
            }
        }
        i = mi + 1;
        j = mj + 1;
    }
    false
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

/// The edit that takes `ch` from theirs: a cell takes their cell; rows
/// added or removed take their whole sheet (cells and styles), so that
/// references stay as they wrote them. Sheets added or removed can't be
/// taken (None), nor anything that no longer applies.
pub fn take(ours: &Workbook, theirs: &Workbook, ch: &Change) -> Option<GridOp> {
    match ch {
        Change::Cell { sheet, row, col, theirs: input, .. } => {
            if (*sheet as usize) >= ours.sheet_count() {
                return None;
            }
            Some(GridOp::set(CellAddr::new(*sheet, *row, *col), input.clone()))
        }
        Change::RowsAdded { sheet, .. } | Change::RowsRemoved { sheet, .. } => take_sheet(ours, theirs, *sheet),
        Change::SheetAdded { .. } | Change::SheetRemoved { .. } => None,
    }
}

/// Our sheet `s` made the same as theirs of its name: cells, formulas and
/// styles over both sheets' used area.
pub fn take_sheet(ours: &Workbook, theirs: &Workbook, s: u16) -> Option<GridOp> {
    if s as usize >= ours.sheet_count() {
        return None;
    }
    let t = theirs.sheet_id(&ours.sheet_name(s))?;
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
    let mut styles = theirs.style_snapshot(&theirs_rect);
    styles.rect.sheet = s;
    Some(GridOp::Batch(vec![GridOp::Replace(Region { rect, values, fills }), GridOp::Restyle(styles)]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use officina_core::history::Op;

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
        let op = take(&w, &b, &c).unwrap();
        let inv = op.apply(&mut w);
        w.recalc();
        assert_eq!(w.value(0, 0, 1), Value::Num(10.0));
        inv.apply(&mut w);
        w.recalc();
        assert_eq!(texts(&w), texts(&a));
    }

    #[test]
    fn taking_rows_takes_the_sheet_and_undoes() {
        let rows: &[&[&str]] = &[&["Name", "Qty"], &["Pens", "3"], &["Ink", "=B2+1"]];
        let a = book(rows);
        let b = book(&[&["Name", "Qty"], &["Clips", "9"], &["Pens", "3"], &["Ink", "=B3+1"]]);
        let (ch, _) = compare(&a, &b);
        let added = ch.iter().find(|c| c.whole_sheet()).unwrap();
        let mut w = book(rows);
        let inv = take(&w, &b, added).unwrap().apply(&mut w);
        w.recalc();
        assert_eq!(texts(&w), texts(&b));
        assert_eq!(w.value(0, 3, 1), Value::Num(4.0));
        assert_eq!(compare(&w, &b).0, vec![]);
        inv.apply(&mut w);
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
            if let Some(c) = ch.iter().find(|c| c.whole_sheet()) {
                take(&w, &wb, c).unwrap().apply(&mut w);
            } else {
                for c in &ch {
                    take(&w, &wb, c).unwrap().apply(&mut w);
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
