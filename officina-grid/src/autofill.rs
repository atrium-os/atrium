//! Filling an area from a selection, as a spreadsheet's fill handle does.
//!
//! Each row (filling across) or column (filling down) of the selection is read
//! as a series where it is one: numbers (two or more, on their best-fit line),
//! dates (by days, or by months when the day of the month stays), text ending
//! in a number ("Item 1", "Q3"), and day and month names. Anything else is
//! repeated in turn, formulas moved as a copy moves them. Styles repeat too.
//! A copy ([`Mode::Copy`], Ctrl+D and Ctrl+R) repeats without series.

use std::sync::Arc;

use crate::addr::{CellAddr, Rect};
use crate::book::Workbook;
use crate::eval::dates::{date_serial, ymd};
use crate::formula::ast::Expr;
use crate::numfmt::NumFmt;
use crate::ops::{GridOp, Region, StyleRegion};
use crate::style::Style;
use crate::value::Value;

/// The most new cells one fill makes.
pub const MAX_FILL_CELLS: u64 = 4_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Series where the selection makes one (the fill handle).
    Series,
    /// The selection repeated as it is.
    Copy,
    /// The handle let go with Ctrl held, as Excel flips it: one number
    /// counts up (or down) by one; anything else is copied.
    Flip,
    /// Fill Series chosen after a fill: a series as `Series` makes, and
    /// one number counts by one too.
    Count,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dir {
    Down,
    Up,
    Right,
    Left,
}

/// One source cell as the fill reads it.
#[derive(Clone)]
enum Src {
    Value(Value),
    Formula(Arc<Expr>),
}

/// A line's series: the value at step `x` (0 is the first source cell).
enum Series {
    Repeat,
    Linear { mean_x: f64, mean_y: f64, slope: f64 },
    Months { first: (i64, i64, i64), step: f64, time: f64 },
    Days { first: f64, step: f64 },
    Numbered { head: String, tail: String, width: usize, first: f64, step: f64 },
    Named { list: std::sync::Arc<[String]>, case: Case, first: i64, step: i64 },
}

#[derive(Clone, Copy)]
enum Case {
    Title,
    Upper,
    Lower,
}

const DAYS: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const DAYS3: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
const MONTHS3: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const LISTS: [&[&str]; 4] = [&DAYS, &DAYS3, &MONTHS, &MONTHS3];

/// The person's own lists (Excel's custom lists: regions, sizes, terms),
/// tried before the day and month names. Set by the app from its settings.
static CUSTOM: std::sync::RwLock<Vec<std::sync::Arc<[String]>>> = std::sync::RwLock::new(Vec::new());
/// Lists, and entries in a list, at most.
pub const MAX_LISTS: usize = 64;
pub const MAX_ENTRIES: usize = 256;

/// Set the custom lists fills follow: each of 2 to [`MAX_ENTRIES`] entries,
/// blank ones and repeats dropped; at most [`MAX_LISTS`].
pub fn set_custom_lists(lists: Vec<Vec<String>>) {
    let clean: Vec<std::sync::Arc<[String]>> = lists
        .into_iter()
        .filter_map(|l| {
            let mut seen: Vec<String> = Vec::new();
            for e in l {
                let e = e.trim().to_string();
                if !e.is_empty() && e.chars().count() <= 255 && !seen.iter().any(|s| s.eq_ignore_ascii_case(&e)) && seen.len() < MAX_ENTRIES {
                    seen.push(e);
                }
            }
            (seen.len() >= 2).then(|| seen.into())
        })
        .take(MAX_LISTS)
        .collect();
    *CUSTOM.write().unwrap_or_else(|e| e.into_inner()) = clean;
}

/// The custom lists now set.
pub fn custom_lists() -> Vec<Vec<String>> {
    CUSTOM.read().unwrap_or_else(|e| e.into_inner()).iter().map(|l| l.to_vec()).collect()
}

/// The edit that fills `to` from `src`, which `to` holds and extends one way
/// (down, up, right or left). Only the cells of `to` outside `src` change.
pub fn fill(wb: &Workbook, src: Rect, to: Rect, mode: Mode) -> Result<GridOp, String> {
    let dir = if to.c0 == src.c0 && to.c1 == src.c1 && to.r0 == src.r0 && to.r1 > src.r1 {
        Dir::Down
    } else if to.c0 == src.c0 && to.c1 == src.c1 && to.r1 == src.r1 && to.r0 < src.r0 {
        Dir::Up
    } else if to.r0 == src.r0 && to.r1 == src.r1 && to.c0 == src.c0 && to.c1 > src.c1 {
        Dir::Right
    } else if to.r0 == src.r0 && to.r1 == src.r1 && to.c1 == src.c1 && to.c0 < src.c0 {
        Dir::Left
    } else {
        return Err("Fill reaches past the selection down, up, right or left.".into());
    };
    let new = match dir {
        Dir::Down => Rect { r0: src.r1 + 1, ..to },
        Dir::Up => Rect { r1: src.r0 - 1, ..to },
        Dir::Right => Rect { c0: src.c1 + 1, ..to },
        Dir::Left => Rect { c1: src.c0 - 1, ..to },
    };
    if new.cells() > MAX_FILL_CELLS {
        return Err("That's too many cells to fill at once.".into());
    }
    if wb.merges_in(&to).next().is_some() {
        return Err("Fill doesn't reach merged cells yet; unmerge them first.".into());
    }
    let s = src.sheet;
    let down = matches!(dir, Dir::Down | Dir::Up);
    // Lines across the fill: columns when filling down or up.
    let (lines, len) = if down { (src.c0..=src.c1, src.rows() as u32) } else { (src.r0..=src.r1, src.cols() as u32) };
    let mut values = Vec::new();
    let mut cells: Vec<(u32, u32, Arc<Expr>)> = Vec::new();
    let mut styles: Vec<(u32, u32, Style)> = Vec::new();
    for line in lines {
        // Source cells in order along the line, from the side the fill leaves.
        let at = |k: u32| -> (u32, u32) {
            match dir {
                Dir::Down => (src.r0 + k, line),
                Dir::Up => (src.r1 - k, line),
                Dir::Right => (line, src.c0 + k),
                Dir::Left => (line, src.c1 - k),
            }
        };
        let read: Vec<(Src, Style)> = (0..len)
            .map(|k| {
                let (r, c) = at(k);
                let st = wb.style(CellAddr::new(s, r, c)).clone();
                let v = match wb.group_at(s, r, c) {
                    Some(g) => Src::Formula(wb.group(g).expr.clone()),
                    None => Src::Value(wb.value(s, r, c)),
                };
                (v, st)
            })
            .collect();
        let back = matches!(dir, Dir::Up | Dir::Left);
        let series = match mode {
            Mode::Series => series_of(&read, back),
            Mode::Copy => Series::Repeat,
            Mode::Count => match &read[..] {
                [(Src::Value(Value::Num(n)), _)] => Series::Days { first: *n, step: if back { -1.0 } else { 1.0 } },
                _ => series_of(&read, back),
            },
            Mode::Flip => match &read[..] {
                [(Src::Value(Value::Num(n)), _)] => Series::Days { first: *n, step: if back { -1.0 } else { 1.0 } },
                _ => Series::Repeat,
            },
        };
        let count = if down { new.rows() } else { new.cols() } as u32;
        for i in 0..count {
            // Step along the line from the source's far end.
            let x = len + i;
            let (r, c) = match dir {
                Dir::Down => (src.r1 + 1 + i, line),
                Dir::Up => (src.r0 - 1 - i, line),
                Dir::Right => (line, src.c1 + 1 + i),
                Dir::Left => (line, src.c0 - 1 - i),
            };
            let (from, st) = &read[(x % len) as usize];
            styles.push((r, c, st.clone()));
            let v = match (&series, from) {
                (Series::Repeat, Src::Formula(e)) => {
                    cells.push((r, c, e.clone()));
                    continue;
                }
                (Series::Repeat, Src::Value(v)) => v.clone(),
                (s, _) => at_step(s, x as f64),
            };
            if !v.is_empty() {
                values.push((r, c, v));
            }
        }
    }
    let mut fills = Vec::new();
    // `fills_of` keeps runs that share a relative formula as one fill.
    crate::shift::fills_of(wb, cells, &mut fills);
    values.sort_by_key(|v| (v.1, v.0));
    styles.sort_by_key(|v| (v.1, v.0));
    let mut spans: Vec<(u32, u32, u32, Style)> = Vec::new();
    for (r, c, st) in styles {
        if st == Style::default() {
            continue;
        }
        match spans.last_mut() {
            Some(l) if l.0 == c && l.2 + 1 == r && l.3 == st => l.2 = r,
            _ => spans.push((c, r, r, st)),
        }
    }
    Ok(GridOp::Batch(vec![GridOp::Replace(Region { rect: new, values, fills }), GridOp::Restyle(StyleRegion { rect: new, spans })]))
}

/// Where a double-click on the fill handle fills to: the selection grown
/// down beside the data next to it (the column left of it, or else right),
/// to that data's last row, stopping above anything already in the
/// selection's first column. `None`: nothing beside it to follow.
pub fn to_data_end(wb: &Workbook, sel: Rect) -> Option<Rect> {
    let below = sel.r1.checked_add(1)?;
    let filled = |c: u32| !wb.value(sel.sheet, below, c).is_empty();
    let side = [sel.c0.checked_sub(1), sel.c1.checked_add(1)].into_iter().flatten().find(|&c| filled(c))?;
    let end = run_end(wb, sel.sheet, side, below, true);
    // Stop above what the first column already holds.
    let mine = run_end(wb, sel.sheet, sel.c0, below, false);
    let end = end.min(mine);
    (end >= below).then_some(Rect { r1: end, ..sel })
}

/// The last row of the run from `from` down column `c` whose cells are all
/// filled (`filled`) or all empty. Read a window at a time, so a short run
/// in a long column stops early.
fn run_end(wb: &Workbook, sheet: u16, c: u32, from: u32, filled: bool) -> u32 {
    let last = crate::addr::MAX_ROWS - 1;
    let mut next = from;
    let mut win = 1024u32;
    while next <= last {
        let r1 = next.saturating_add(win - 1).min(last);
        let mut stop = None;
        let mut want = next;
        wb.each(&Rect { sheet, r0: next, c0: c, r1, c1: c }, &mut |row, _, v| {
            if stop.is_some() || v.is_empty() {
                return;
            }
            if filled {
                if row != want {
                    stop = Some(want);
                }
                want = row + 1;
            } else {
                stop = Some(row);
            }
        });
        if let Some(s) = stop {
            return s.wrapping_sub(1);
        }
        if filled && want <= r1 {
            return want.wrapping_sub(1);
        }
        if !filled && r1 >= wb.extent(sheet).0 {
            return last;
        }
        next = r1 + 1;
        win = win.saturating_mul(4);
    }
    last
}

/// A value of a series at step `x`.
fn at_step(s: &Series, x: f64) -> Value {
    match s {
        Series::Repeat => Value::Empty,
        Series::Linear { mean_x, mean_y, slope } => Value::Num(round15(mean_y + slope * (x - mean_x))),
        Series::Days { first, step } => Value::Num(round15(first + step * x)),
        Series::Months { first: (y, m, d), step, time } => {
            let months = (step * x).round();
            match date_serial(*y as f64, *m as f64 + months, 1.0) {
                // The day kept where the month has it, else the month's last.
                Some(start) => {
                    let (yy, mm, _) = ymd(start).unwrap_or((*y, *m, 1));
                    let last = date_serial(yy as f64, mm as f64 + 1.0, 0.0).and_then(ymd).map_or(28, |t| t.2);
                    date_serial(yy as f64, mm as f64, (*d).min(last) as f64).map_or(Value::Err(crate::value::ErrorKind::Num), |v| Value::Num(v + time))
                }
                None => Value::Err(crate::value::ErrorKind::Num),
            }
        }
        Series::Numbered { head, tail, width, first, step } => {
            let n = (first + step * x).round().abs() as u64;
            Value::str(&format!("{head}{n:0width$}{tail}"))
        }
        Series::Named { list, case, first, step } => {
            let i = (first + step * x as i64).rem_euclid(list.len() as i64) as usize;
            let w = &list[i];
            Value::str(&match case {
                Case::Title => w.to_string(),
                Case::Upper => w.to_uppercase(),
                Case::Lower => w.to_lowercase(),
            })
        }
    }
}

/// Excel shows and stores series values to 15 significant digits, so 0.1
/// steps don't drift into 0.30000000000000004.
fn round15(v: f64) -> f64 {
    if v == 0.0 || !v.is_finite() {
        return v;
    }
    let mag = v.abs().log10().floor() as i32;
    let scale = 10f64.powi(14 - mag);
    if !scale.is_finite() || scale == 0.0 {
        return v;
    }
    (v * scale).round() / scale
}

/// The series a line's source cells make, or [`Series::Repeat`]. The cells
/// come in the fill's direction, so a series of two or more carries on as it
/// goes; one cell counts up, or down when filling up or left (`back`).
fn series_of(read: &[(Src, Style)], back: bool) -> Series {
    let one = if back { -1 } else { 1 };
    let vals: Option<Vec<&Value>> = read.iter().map(|(s, _)| if let Src::Value(v) = s { Some(v) } else { None }).collect();
    let Some(vals) = vals else { return Series::Repeat };
    let n = vals.len();
    // Numbers: dates by their format, others on their best-fit line.
    if vals.iter().all(|v| matches!(v, Value::Num(_))) {
        let ys: Vec<f64> = vals.iter().map(|v| if let Value::Num(x) = v { *x } else { 0.0 }).collect();
        let dates = read.iter().all(|(_, st)| NumFmt::parse(&st.num_fmt).is_date());
        if dates {
            if n == 1 {
                return Series::Days { first: ys[0], step: one as f64 };
            }
            if let Some(s) = months_of(&ys) {
                return s;
            }
        }
        if n == 1 {
            return Series::Repeat;
        }
        let mean_x = (n - 1) as f64 / 2.0;
        let mean_y = ys.iter().sum::<f64>() / n as f64;
        let (mut sxy, mut sxx) = (0.0, 0.0);
        for (i, y) in ys.iter().enumerate() {
            let dx = i as f64 - mean_x;
            sxy += dx * (y - mean_y);
            sxx += dx * dx;
        }
        return Series::Linear { mean_x, mean_y, slope: sxy / sxx };
    }
    let texts: Option<Vec<&str>> = vals.iter().map(|v| if let Value::Str(s) = v { Some(&**s) } else { None }).collect();
    let Some(texts) = texts else { return Series::Repeat };
    if let Some(s) = named_of(&texts, one) {
        return s;
    }
    numbered_of(&texts, one).unwrap_or(Series::Repeat)
}

/// Dates on the same day of the month, a whole number of months apart.
fn months_of(ys: &[f64]) -> Option<Series> {
    let parts: Option<Vec<(i64, i64, i64)>> = ys.iter().map(|&y| ymd(y)).collect();
    let parts = parts?;
    let first = parts[0];
    let time = ys[0] - ys[0].floor();
    if parts.iter().zip(ys).any(|(p, y)| p.2 != first.2 || y - y.floor() != time) {
        return None;
    }
    let idx: Vec<i64> = parts.iter().map(|p| p.0 * 12 + p.1).collect();
    let step = idx[1] - idx[0];
    if step == 0 || idx.windows(2).any(|w| w[1] - w[0] != step) {
        return None;
    }
    Some(Series::Months { first, step: step as f64, time })
}

/// Day or month names, all from one list, a fixed number apart.
fn named_of(texts: &[&str], one: i64) -> Option<Series> {
    let custom = CUSTOM.read().unwrap_or_else(|e| e.into_inner()).clone();
    let builtin = LISTS.iter().map(|l| l.iter().map(|s| s.to_string()).collect::<Vec<_>>().into());
    for list in custom.into_iter().chain(builtin) {
        let list: std::sync::Arc<[String]> = list;
        let idx: Option<Vec<i64>> = texts.iter().map(|t| list.iter().position(|w| w.eq_ignore_ascii_case(t)).map(|i| i as i64)).collect();
        let Some(idx) = idx else { continue };
        let len = list.len() as i64;
        let step = if idx.len() == 1 { one.rem_euclid(len) } else { (idx[1] - idx[0]).rem_euclid(len) };
        if idx.windows(2).any(|w| (w[1] - w[0]).rem_euclid(len) != step) {
            return None;
        }
        let t = texts[0];
        let case = if t.len() > 1 && t.chars().all(|c| c.is_ascii_uppercase()) {
            Case::Upper
        } else if t.chars().all(|c| c.is_ascii_lowercase()) {
            Case::Lower
        } else {
            Case::Title
        };
        return Some(Series::Named { list, case, first: idx[0], step });
    }
    None
}

/// Text whose last run of digits counts: "Item 1", "Q3", "Room 007".
fn numbered_of(texts: &[&str], one: i64) -> Option<Series> {
    let mut parts = Vec::new();
    for t in texts {
        let end = t.rfind(|c: char| c.is_ascii_digit())? + 1;
        let start = t[..end].rfind(|c: char| !c.is_ascii_digit()).map_or(0, |i| i + 1);
        let digits = &t[start..end];
        if digits.len() > 15 {
            return None;
        }
        parts.push((&t[..start], digits, &t[end..]));
    }
    let (head, d0, tail) = parts[0];
    if parts.iter().any(|p| p.0 != head || p.2 != tail) {
        return None;
    }
    let nums: Vec<f64> = parts.iter().map(|p| p.1.parse::<u64>().unwrap_or(0) as f64).collect();
    let width = if d0.starts_with('0') && d0.len() > 1 { d0.len() } else { 0 };
    let step = if nums.len() == 1 { one as f64 } else { nums[1] - nums[0] };
    if nums.windows(2).any(|w| w[1] - w[0] != step) {
        return None;
    }
    Some(Series::Numbered { head: head.to_string(), tail: tail.to_string(), width, first: nums[0], step })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::book::Input;
    use officina_core::history::Op;

    fn wb() -> Workbook {
        let mut wb = Workbook::new();
        wb.add_sheet("Sheet1").unwrap();
        wb
    }

    fn set(wb: &mut Workbook, r: u32, c: u32, s: &str) {
        GridOp::set(CellAddr::new(0, r, c), Input::typed(s)).apply(wb);
    }

    fn rect(r0: u32, c0: u32, r1: u32, c1: u32) -> Rect {
        Rect { sheet: 0, r0, c0, r1, c1 }
    }

    fn shown(wb: &Workbook, r: u32, c: u32) -> String {
        wb.formula_text(CellAddr::new(0, r, c)).unwrap_or_else(|| wb.value(0, r, c).display())
    }

    #[test]
    fn series_follow_numbers_names_and_numbered_text() {
        let mut wb = wb();
        for (c, a, b) in [(0, "1", "3"), (1, "Item 1", ""), (2, "Mon", ""), (3, "0.1", "0.2"), (4, "Q4", "Q3"), (5, "x", "7"), (6, "5", ""), (7, "JAN", "MAR"), (8, "Room 008", "")] {
            set(&mut wb, 0, c, a);
            set(&mut wb, 1, c, b);
        }
        // Two-cell sources in A, D, E, F, H; one cell in the rest (row 2 left as is).
        for (c, two) in [(0, true), (1, false), (2, false), (3, true), (4, true), (5, true), (6, false), (7, true), (8, false)] {
            let src = if two { rect(0, c, 1, c) } else { rect(0, c, 0, c) };
            let op = fill(&wb, src, rect(0, c, 4, c), Mode::Series).unwrap();
            op.apply(&mut wb);
        }
        let col = |wb: &Workbook, c| (2..=4).map(|r| shown(wb, r, c)).collect::<Vec<_>>();
        assert_eq!(col(&wb, 0), ["5", "7", "9"]);
        assert_eq!(col(&wb, 1), ["Item 3", "Item 4", "Item 5"]);
        assert_eq!(shown(&wb, 1, 1), "Item 2");
        assert_eq!(col(&wb, 2), ["Wed", "Thu", "Fri"]);
        assert_eq!(col(&wb, 3), ["0.3", "0.4", "0.5"]);
        assert_eq!(col(&wb, 4), ["Q2", "Q1", "Q0"]);
        // Mixed: repeated in turn.
        assert_eq!(col(&wb, 5), ["x", "7", "x"]);
        // One number: copied, as Excel's handle does.
        assert_eq!(col(&wb, 6), ["5", "5", "5"]);
        assert_eq!(col(&wb, 7), ["MAY", "JUL", "SEP"]);
        assert_eq!(shown(&wb, 4, 8), "Room 012");
    }

    #[test]
    fn dates_step_by_days_or_months_and_formulas_move() {
        let mut wb = wb();
        for (r, c, d) in [(0, 0, (2026, 1, 31)), (0, 1, (2026, 1, 15)), (1, 1, (2026, 2, 15))] {
            let v = date_serial(d.0 as f64, d.1 as f64, d.2 as f64).unwrap();
            GridOp::set(CellAddr::new(0, r, c), Input::Value(Value::Num(v))).apply(&mut wb);
        }
        GridOp::restyle(&wb, rect(0, 0, 1, 1), |st| Style { num_fmt: Arc::from("yyyy-mm-dd"), ..st.clone() }).apply(&mut wb);
        set(&mut wb, 0, 2, "=A1+1");
        set(&mut wb, 0, 3, "=$A$1*2");
        let date = |wb: &Workbook, r, c| ymd(match wb.value(0, r, c) { Value::Num(n) => n, v => panic!("{v:?}") }).unwrap();
        fill(&wb, rect(0, 0, 0, 0), rect(0, 0, 2, 0), Mode::Series).unwrap().apply(&mut wb);
        assert_eq!(date(&wb, 2, 0), (2026, 2, 2));
        fill(&wb, rect(0, 1, 1, 1), rect(0, 1, 3, 1), Mode::Series).unwrap().apply(&mut wb);
        assert_eq!(date(&wb, 3, 1), (2026, 4, 15));
        let op = fill(&wb, rect(0, 2, 0, 3), rect(0, 2, 3, 3), Mode::Series).unwrap();
        let undo = op.apply(&mut wb);
        assert_eq!(shown(&wb, 3, 2), "=A4+1");
        assert_eq!(shown(&wb, 3, 3), "=$A$1*2");
        // The three moved formulas are one fill per column.
        assert_eq!(wb.group_at(0, 1, 2), wb.group_at(0, 3, 2));
        undo.apply(&mut wb);
        assert_eq!(shown(&wb, 3, 2), "");
        // Across: =A1+1 in C1 to the right reads B1, C1...
        fill(&wb, rect(0, 2, 0, 2), rect(0, 2, 0, 4), Mode::Series).unwrap().apply(&mut wb);
        assert_eq!(shown(&wb, 0, 4), "=C1+1");
    }

    #[test]
    fn custom_lists_fill_in_turn() {
        let mut wb = wb();
        set_custom_lists(vec![vec!["Small".into(), " Medium ".into(), "".into(), "Large".into(), "small".into()], vec!["lonely".into()]]);
        assert_eq!(custom_lists(), vec![vec!["Small".to_string(), "Medium".into(), "Large".into()]]);
        set(&mut wb, 0, 0, "Medium");
        fill(&wb, rect(0, 0, 0, 0), rect(0, 0, 3, 0), Mode::Series).unwrap().apply(&mut wb);
        assert_eq!((1..=3).map(|r| shown(&wb, r, 0)).collect::<Vec<_>>(), ["Large", "Small", "Medium"]);
        set_custom_lists(Vec::new());
        assert!(custom_lists().is_empty());
    }

    #[test]
    fn filling_up_left_and_copying() {
        let mut wb = wb();
        set(&mut wb, 5, 0, "10");
        set(&mut wb, 6, 0, "20");
        fill(&wb, rect(5, 0, 6, 0), rect(3, 0, 6, 0), Mode::Series).unwrap().apply(&mut wb);
        assert_eq!((shown(&wb, 4, 0), shown(&wb, 3, 0)), ("0".into(), "-10".into()));
        set(&mut wb, 0, 5, "Item 5");
        fill(&wb, rect(0, 5, 0, 5), rect(0, 3, 0, 5), Mode::Series).unwrap().apply(&mut wb);
        assert_eq!((shown(&wb, 0, 4), shown(&wb, 0, 3)), ("Item 4".into(), "Item 3".into()));
        fill(&wb, rect(5, 0, 6, 0), rect(5, 0, 8, 0), Mode::Copy).unwrap().apply(&mut wb);
        assert_eq!((shown(&wb, 7, 0), shown(&wb, 8, 0)), ("10".into(), "20".into()));
        assert!(fill(&wb, rect(5, 0, 6, 0), rect(5, 0, 6, 1), Mode::Copy).is_ok());
        assert!(fill(&wb, rect(5, 0, 6, 0), rect(4, 0, 7, 0), Mode::Copy).is_err());
    }

    #[test]
    fn a_double_click_fills_beside_the_data() {
        let mut wb = wb();
        // A1:A6 data; B1 the formula to fill down; B5 already holds something.
        for r in 0..6 {
            set(&mut wb, r, 0, &format!("{r}"));
        }
        set(&mut wb, 0, 1, "=A1*2");
        set(&mut wb, 4, 1, "stop");
        assert_eq!(to_data_end(&wb, rect(0, 1, 0, 1)), Some(rect(0, 1, 3, 1)));
        set(&mut wb, 4, 1, "");
        assert_eq!(to_data_end(&wb, rect(0, 1, 0, 1)), Some(rect(0, 1, 5, 1)));
        // Data on the right when the left is empty; none: nothing.
        assert_eq!(to_data_end(&wb, rect(0, 4, 0, 4)), None);
        set(&mut wb, 0, 3, "x");
        set(&mut wb, 1, 6, "y");
        set(&mut wb, 2, 6, "y");
        assert_eq!(to_data_end(&wb, rect(0, 3, 0, 5)), Some(rect(0, 3, 2, 5)));
        // A long column is read a window at a time.
        GridOp::fill(0, 8, 0, 99_999, "=1").apply(&mut wb);
        wb.recalc();
        set(&mut wb, 0, 9, "1");
        assert_eq!(to_data_end(&wb, rect(0, 9, 0, 9)), Some(rect(0, 9, 99_999, 9)));
    }

    #[test]
    fn ctrl_flips_series_and_copy() {
        let mut wb = wb();
        set(&mut wb, 0, 0, "5");
        set(&mut wb, 0, 1, "1");
        set(&mut wb, 1, 1, "3");
        fill(&wb, rect(0, 0, 0, 0), rect(0, 0, 2, 0), Mode::Flip).unwrap().apply(&mut wb);
        assert_eq!((shown(&wb, 1, 0), shown(&wb, 2, 0)), ("6".into(), "7".into()));
        fill(&wb, rect(0, 1, 1, 1), rect(0, 1, 3, 1), Mode::Flip).unwrap().apply(&mut wb);
        assert_eq!((shown(&wb, 2, 1), shown(&wb, 3, 1)), ("1".into(), "3".into()));
        // Count: one number counts; a series stays one.
        fill(&wb, rect(0, 0, 0, 0), rect(0, 0, 4, 0), Mode::Count).unwrap().apply(&mut wb);
        assert_eq!(shown(&wb, 4, 0), "9");
        fill(&wb, rect(0, 1, 1, 1), rect(0, 1, 3, 1), Mode::Count).unwrap().apply(&mut wb);
        assert_eq!(shown(&wb, 3, 1), "7");
    }

    #[test]
    fn styles_repeat_with_the_cells() {
        let mut wb = wb();
        set(&mut wb, 0, 0, "1");
        GridOp::restyle(&wb, rect(0, 0, 0, 0), |st| Style { h_align: crate::style::HAlign::Right, ..st.clone() }).apply(&mut wb);
        fill(&wb, rect(0, 0, 1, 0), rect(0, 0, 5, 0), Mode::Copy).unwrap().apply(&mut wb);
        let bold = |r| wb.style(CellAddr::new(0, r, 0)).h_align == crate::style::HAlign::Right;
        assert_eq!((2..=5).map(bold).collect::<Vec<_>>(), [true, false, true, false]);
    }
}
