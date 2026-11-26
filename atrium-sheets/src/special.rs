//! Go To Special, cell protection toggles, outline detail and the Forecast
//! Sheet (docs/spec/officina.md §16.148).
//!
//! Go To Special finds the cells of one kind in the selection (or, for one
//! cell selected, the sheet's used area) and marks them on the grid; the
//! cursor goes to the first and `next` steps on. Kinds that make one
//! rectangle (last cell, current region, current array) select it.

use officina_grid::addr::{a1, CellAddr, Rect, MAX_COLS, MAX_ROWS};
use officina_grid::book::Input;
use officina_grid::ops::GridOp;
use officina_grid::outline::Outline;
use officina_grid::shift::Axis;
use officina_grid::Value;
use pergola::widgets::CellRange;

use crate::doc::Doc;

/// The most areas Go To Special marks.
pub const MAX_FOUND: usize = 10_000;
/// The most cells it looks at.
pub const MAX_SCAN: u64 = 4_000_000;

/// What Go To Special did: where the cursor goes, the selection (a corner
/// pair) when it selects one area, and what to say.
#[derive(Debug, Clone, PartialEq)]
pub struct Went {
    pub cursor: (u32, u32),
    pub select: Option<CellRange>,
    pub say: String,
}

fn rect_of(s: u16, sel: CellRange) -> Rect {
    Rect { sheet: s, r0: sel.r0, c0: sel.c0, r1: sel.r1, c1: sel.c1 }
}

fn overlap(a: &Rect, b: &Rect) -> Option<Rect> {
    let r = Rect { sheet: a.sheet, r0: a.r0.max(b.r0), c0: a.c0.max(b.c0), r1: a.r1.min(b.r1), c1: a.c1.min(b.c1) };
    (a.sheet == b.sheet && r.r0 <= r.r1 && r.c0 <= r.c1).then_some(r)
}

/// Joins cells found column by column into runs down each column.
fn runs(s: u16, mut cells: Vec<(u32, u32)>, max: usize) -> (Vec<Rect>, bool) {
    cells.sort_unstable_by_key(|&(r, c)| (c, r));
    let mut out: Vec<Rect> = Vec::new();
    let mut over = false;
    for (r, c) in cells {
        if let Some(last) = out.last_mut().filter(|l| l.c0 == c && l.r1 + 1 == r) {
            last.r1 = r;
        } else if out.len() >= max {
            over = true;
            break;
        } else {
            out.push(Rect { sheet: s, r0: r, c0: c, r1: r, c1: c });
        }
    }
    (out, over)
}

/// The kinds Go To Special knows, for the bar's hint.
pub const KINDS: &str = "blanks, constants, formulas (numbers, text, logicals, errors), comments, validation, conditional, row differences, column differences, precedents, dependents, visible, last cell, current region, current array; next";

/// Runs Go To Special for `typed`.
pub fn go_to_special(d: &mut Doc, sel: CellRange, cursor: (u32, u32), typed: &str) -> Result<Went, String> {
    let t = typed.trim().to_lowercase();
    let s = d.sheet;
    let (rows, cols) = d.wb.extent(s);
    let one = sel.r0 == sel.r1 && sel.c0 == sel.c1;
    if t == "next" {
        let found = d.found.clone();
        if found.is_empty() {
            return Err("Nothing is found yet: type what to go to.".into());
        }
        let inside = |r: &Rect| r.r0 <= cursor.0 && cursor.0 <= r.r1 && r.c0 <= cursor.1 && cursor.1 <= r.c1;
        let i = found.iter().position(|r| (r.r0, r.c0) > cursor && !inside(r)).unwrap_or(0);
        let r = found[i];
        return Ok(Went { cursor: (r.r0, r.c0), select: Some(CellRange { r0: r.r0, c0: r.c0, r1: r.r1, c1: r.c1 }), say: format!("Area {} of {}.", i + 1, found.len()) });
    }
    let select = |r: Rect, what: &str| Went { cursor: (r.r0, r.c0), select: Some(CellRange { r0: r.r0, c0: r.c0, r1: r.r1, c1: r.c1 }), say: format!("{what}: {}.", range(&r)) };
    match t.as_str() {
        "last cell" | "last" => {
            if rows == 0 || cols == 0 {
                return Err("The sheet is empty.".into());
            }
            // The last row and column that hold anything.
            let mut last = (0u32, 0u32);
            d.wb.each(&Rect { sheet: s, r0: 0, c0: 0, r1: rows - 1, c1: cols - 1 }, &mut |r, c, _| last = (last.0.max(r), last.1.max(c)));
            d.found.clear();
            return Ok(select(Rect::cell(CellAddr::new(s, last.0, last.1)), "The last cell"));
        }
        "current region" | "region" => {
            d.found.clear();
            return Ok(select(crate::data::current_region(&d.wb, s, cursor.0, cursor.1), "The current region"));
        }
        "current array" | "array" => {
            let at = CellAddr::new(s, cursor.0, cursor.1);
            let origin = d.wb.spilled_from(at).unwrap_or(at);
            let area = d.wb.spill_area(origin).ok_or("The cursor isn't in an array formula's cells.")?;
            d.found.clear();
            return Ok(select(area, "The array"));
        }
        _ => {}
    }
    let scope = if one {
        if rows == 0 || cols == 0 {
            return Err("The sheet is empty.".into());
        }
        Rect { sheet: s, r0: 0, c0: 0, r1: rows - 1, c1: cols - 1 }
    } else {
        rect_of(s, sel)
    };
    // Blanks and visible cells look at every cell; the rest at those with something in them.
    let mut cells: Vec<(u32, u32)> = Vec::new();
    let mut limit_hit = false;
    let mut add = |cells: &mut Vec<(u32, u32)>, r: u32, c: u32| {
        if cells.len() < MAX_SCAN as usize {
            cells.push((r, c));
        } else {
            limit_hit = true;
        }
    };
    let words: Vec<&str> = t.split(|c: char| c == ',' || c.is_whitespace()).filter(|w| !w.is_empty()).collect();
    let Some(&kind) = words.first() else {
        return Err(format!("Type what to go to: {KINDS}."));
    };
    let types: Vec<&str> = words[1..].to_vec();
    for w in &types {
        if !matches!(*w, "numbers" | "text" | "logicals" | "errors" | "differences") {
            return Err(format!("{w:?}: after constants or formulas type numbers, text, logicals or errors."));
        }
    }
    let wants = |v: &Value| types.is_empty() || types.iter().any(|w| match *w {
        "numbers" => matches!(v, Value::Num(_)),
        "text" => matches!(v, Value::Str(_)),
        "logicals" => matches!(v, Value::Bool(_)),
        "errors" => matches!(v, Value::Err(_)),
        _ => false,
    });
    let formula = |r: u32, c: u32| d.wb.group_at(s, r, c).is_some() || d.wb.spilled_from(CellAddr::new(s, r, c)).is_some();
    let what;
    match kind {
        "blanks" | "blank" => {
            what = "blank cells";
            if scope.cells() > MAX_SCAN {
                return Err(format!("Select fewer than {MAX_SCAN} cells to look for blanks."));
            }
            let mut full = std::collections::HashSet::new();
            d.wb.each(&scope, &mut |r, c, _| {
                full.insert((r, c));
            });
            for c in scope.c0..=scope.c1 {
                for r in scope.r0..=scope.r1 {
                    if !full.contains(&(r, c)) && !formula(r, c) {
                        add(&mut cells, r, c);
                    }
                }
            }
        }
        "constants" | "constant" | "formulas" | "formula" => {
            let want_formula = kind.starts_with("formula");
            what = if want_formula { "formulas" } else { "constants" };
            let mut seen = Vec::new();
            d.wb.each(&scope, &mut |r, c, v| seen.push((r, c, v)));
            for (r, c, v) in seen {
                if formula(r, c) == want_formula && wants(&v) {
                    add(&mut cells, r, c);
                }
            }
        }
        "comments" | "notes" => {
            what = "comments";
            for &(r, c) in d.wb.comments(s).keys() {
                if r >= scope.r0 && r <= scope.r1 && c >= scope.c0 && c <= scope.c1 {
                    add(&mut cells, r, c);
                }
            }
        }
        "validation" | "conditional" => {
            let areas: Vec<Rect> = if kind == "validation" {
                what = "cells with validation";
                d.wb.validations().iter().filter(|v| v.sheet == s).flat_map(|v| v.ranges.clone()).collect()
            } else {
                what = "cells with conditional formats";
                d.wb.cond_formats().iter().filter(|f| f.sheet == s).flat_map(|f| f.ranges.clone()).collect()
            };
            let scope = if one { Rect { sheet: s, r0: 0, c0: 0, r1: MAX_ROWS - 1, c1: MAX_COLS - 1 } } else { scope };
            let mut out: Vec<Rect> = areas.iter().filter_map(|a| overlap(a, &scope)).collect();
            out.sort_by_key(|r| (r.c0, r.r0));
            out.dedup();
            out.truncate(MAX_FOUND);
            return mark(d, out, false, what);
        }
        "row" | "column" | "rows" | "columns" if types == ["differences"] => {
            // Each row (column) of the selection against the cursor's column (row) in it.
            let rows_mode = kind.starts_with("row");
            what = if rows_mode { "row differences" } else { "column differences" };
            if one {
                return Err("Select the rows or columns to compare first.".into());
            }
            if scope.cells() > MAX_SCAN {
                return Err(format!("Select fewer than {MAX_SCAN} cells to compare."));
            }
            let text = |r: u32, c: u32| match d.wb.formula_text(CellAddr::new(s, r, c)) {
                Some(_) => d.wb.group_at(s, r, c).map(|g| format!("={:?}", d.wb.group(g).expr)).unwrap_or_default(),
                None => format!("{:?}", d.wb.get(CellAddr::new(s, r, c))),
            };
            // Formulas compare by their shape (relative references the same), values by value.
            for r in scope.r0..=scope.r1 {
                for c in scope.c0..=scope.c1 {
                    let base = if rows_mode { (r, cursor.1) } else { (cursor.0, c) };
                    if (r, c) != base && text(r, c) != text(base.0, base.1) {
                        add(&mut cells, r, c);
                    }
                }
            }
        }
        "precedents" | "dependents" => {
            what = if kind == "precedents" { "precedents" } else { "dependents" };
            if kind == "precedents" {
                let areas: Vec<Rect> = crate::data::precedents(&d.wb, s, cursor.0, cursor.1).into_iter().filter(|r| r.sheet == s).collect();
                if areas.is_empty() {
                    return Err("The cursor's cell reads no cells on this sheet.".into());
                }
                return mark(d, areas, false, what);
            }
            for a in crate::data::dependents(&d.wb, s, cursor.0, cursor.1, crate::data::MAX_DEPENDENTS) {
                if a.sheet == s {
                    add(&mut cells, a.row, a.col);
                }
            }
        }
        "visible" => {
            what = "visible cells";
            if one {
                return Err("Select the cells to take the visible ones of.".into());
            }
            let sheet = d.wb.sheet(s);
            let (rs, cs) = (sheet.view_row_sizes(), sheet.view_col_sizes());
            let vis_rows: Vec<u32> = (scope.r0..=scope.r1).filter(|&r| rs.size(r) > 0.0).collect();
            let vis_cols: Vec<u32> = (scope.c0..=scope.c1).filter(|&c| cs.size(c) > 0.0).collect();
            // Whole runs of visible rows in each run of visible columns.
            let spans = |v: &[u32]| {
                let mut out: Vec<(u32, u32)> = Vec::new();
                for &x in v {
                    match out.last_mut() {
                        Some(l) if l.1 + 1 == x => l.1 = x,
                        _ => out.push((x, x)),
                    }
                }
                out
            };
            let mut out = Vec::new();
            for (c0, c1) in spans(&vis_cols) {
                for &(r0, r1) in &spans(&vis_rows) {
                    if out.len() < MAX_FOUND {
                        out.push(Rect { sheet: s, r0, c0, r1, c1 });
                    }
                }
            }
            return mark(d, out, false, what);
        }
        _ => return Err(format!("{kind:?}: type one of {KINDS}.")),
    }
    let (areas, over) = runs(s, cells, MAX_FOUND);
    mark(d, areas, over || limit_hit, what)
}

fn range(r: &Rect) -> String {
    if r.r0 == r.r1 && r.c0 == r.c1 { a1(r.r0, r.c0) } else { format!("{}:{}", a1(r.r0, r.c0), a1(r.r1, r.c1)) }
}

fn mark(d: &mut Doc, mut areas: Vec<Rect>, over: bool, what: &str) -> Result<Went, String> {
    if areas.is_empty() {
        d.found.clear();
        return Err(format!("No {what} found."));
    }
    // Reading order: by row, then column.
    areas.sort_by_key(|r| (r.r0, r.c0));
    let cells: u64 = areas.iter().map(|r| r.cells()).sum();
    let first = areas[0];
    d.found = areas;
    let more = if over { format!(" (the first {MAX_FOUND} areas)") } else { String::new() };
    let n = d.found.len();
    Ok(Went {
        cursor: (first.r0, first.c0),
        select: Some(CellRange { r0: first.r0, c0: first.c0, r1: first.r1, c1: first.c1 }),
        say: format!("{cells} {what} in {n} {}{more}, marked. Type next to go on.", if n == 1 { "area" } else { "areas" }),
    })
}

// ---------- Protection toggles ----------

/// Format Cells ▸ Protection: Locked (`lock`) or Hidden (formulas not shown
/// while the sheet is protected) toggled on the selection, from the first
/// cell's state. Refused while the sheet is protected, as Excel greys them.
pub fn toggle_protection(d: &mut Doc, sel: CellRange, lock: bool) -> Result<String, String> {
    let s = d.sheet;
    if d.wb.sheet_protected(s) {
        return Err("Unprotect the sheet to change which cells are locked or hidden.".into());
    }
    let r = rect_of(s, sel);
    let first = d.wb.style(CellAddr::new(s, sel.r0, sel.c0)).clone();
    let op = if lock {
        let unlock = !first.unlocked;
        GridOp::restyle(&d.wb, r, |st| officina_grid::style::Style { unlocked: unlock, ..st.clone() })
    } else {
        let hide = !first.hide_formula;
        GridOp::restyle(&d.wb, r, |st| officina_grid::style::Style { hide_formula: hide, ..st.clone() })
    };
    d.status.clear();
    if !d.run_validated(if lock { "Lock Cells" } else { "Hide Formulas" }, vec![op]) {
        return Err(if d.status.is_empty() { "The change was refused.".into() } else { d.status.clone() });
    }
    Ok(match (lock, first.unlocked, first.hide_formula) {
        (true, true, _) => "Cells locked: protecting the sheet keeps them from changing.".into(),
        (true, false, _) => "Cells unlocked: they can be edited while the sheet is protected.".into(),
        (false, _, false) => "Formulas hidden: they won't show in the formula bar while the sheet is protected.".into(),
        (false, _, true) => "Formulas shown again.".into(),
    })
}

// ---------- Outline detail ----------

/// The innermost group around the selection on either axis (rows first).
fn group_around(d: &Doc, sel: CellRange) -> Option<(Axis, usize)> {
    for axis in [Axis::Rows, Axis::Cols] {
        let (a, b) = if axis == Axis::Rows { (sel.r0, sel.r1) } else { (sel.c0, sel.c1) };
        let groups = d.wb.sheet(d.sheet).outline(axis).groups();
        // A group just collapsed has its summary row (column) right after it.
        let inside = |g: &officina_grid::outline::Group| (g.start <= a && b <= g.end) || (g.end + 1 == a && a == b);
        if let Some((i, _)) = groups.iter().enumerate().filter(|(_, g)| inside(g)).min_by_key(|(_, g)| g.end - g.start) {
            return Some((axis, i));
        }
    }
    None
}

/// Show Detail (`show`) or Hide Detail: the group around the selection (or
/// its summary row or column) expanded or collapsed.
pub fn detail(d: &mut Doc, sel: CellRange, show: bool) -> Result<String, String> {
    let (axis, i) = group_around(d, sel).ok_or("Select cells in an outline group, or its summary row or column.")?;
    let s = d.sheet;
    let mut groups = d.wb.sheet(s).outline(axis).groups().to_vec();
    if groups[i].collapsed != show {
        return Err(if show { "That group's detail is showing already.".into() } else { "That group's detail is hidden already.".into() });
    }
    groups[i].collapsed = !show;
    let outline = Outline::new(groups, axis).ok_or("The outline couldn't be changed.")?;
    d.status.clear();
    if !d.run_validated(if show { "Show Detail" } else { "Hide Detail" }, vec![GridOp::Outline { sheet: s, axis, outline }]) {
        return Err(if d.status.is_empty() { "The change was refused.".into() } else { d.status.clone() });
    }
    Ok(if show { "Detail shown.".into() } else { "Detail hidden.".into() })
}

/// Clear Outline: every row and column group on the sheet taken away, the
/// rows and columns they hid shown. One step.
pub fn clear_outline(d: &mut Doc) -> Result<String, String> {
    let s = d.sheet;
    let n: usize = [Axis::Rows, Axis::Cols].iter().map(|&a| d.wb.sheet(s).outline(a).groups().len()).sum();
    if n == 0 {
        return Err("The sheet has no outline.".into());
    }
    let ops = [Axis::Rows, Axis::Cols].into_iter().filter(|&a| !d.wb.sheet(s).outline(a).groups().is_empty()).map(|axis| GridOp::Outline { sheet: s, axis, outline: Outline::default() }).collect();
    d.status.clear();
    if !d.run_validated("Clear Outline", ops) {
        return Err(if d.status.is_empty() { "The change was refused.".into() } else { d.status.clone() });
    }
    Ok(format!("Outline cleared: {n} {} taken away.", if n == 1 { "group" } else { "groups" }))
}

// ---------- Forecast Sheet ----------

/// The most steps a Forecast Sheet looks ahead.
pub const MAX_AHEAD: u32 = 10_000;

/// The timeline and values the Forecast Sheet reads: the selection's two
/// columns (or the region around the cursor), a header row if the first
/// row isn't numbers.
fn forecast_source(d: &Doc, sel: CellRange, cursor: (u32, u32)) -> Result<(Rect, bool), String> {
    let s = d.sheet;
    let r = if sel.r0 == sel.r1 && sel.c0 == sel.c1 { crate::data::current_region(&d.wb, s, cursor.0, cursor.1) } else { rect_of(s, sel) };
    if r.c1 != r.c0 + 1 {
        return Err("Select two columns: the dates or times, then the values.".into());
    }
    let num = |row: u32, c: u32| matches!(d.wb.get(CellAddr::new(s, row, c)), Value::Num(_));
    let header = !num(r.r0, r.c0);
    let first = if header { r.r0 + 1 } else { r.r0 };
    if r.r1 < first + 2 {
        return Err("A forecast needs at least three dates and values.".into());
    }
    if r.r1 - first + 1 > officina_grid::eval::ets::MAX_POINTS as u32 {
        return Err("The timeline is too long to forecast.".into());
    }
    Ok((Rect { r0: first, ..r }, header))
}

/// The bar's starting text: a third of the history ahead.
pub fn forecast_start(d: &Doc, sel: CellRange, cursor: (u32, u32)) -> String {
    let ahead = forecast_source(d, sel, cursor).map_or(12, |(r, _)| ((r.r1 - r.r0 + 1) / 3).max(1));
    format!("{ahead} ahead, 95%, season auto")
}

/// Forecast Sheet (`12 ahead, 95%, season 4`): a new sheet with the
/// history, then each step ahead's FORECAST.ETS and its lower and upper
/// confidence bounds, and a line chart of all four. One undo step.
pub fn forecast_sheet(d: &mut Doc, sel: CellRange, cursor: (u32, u32), typed: &str) -> Result<String, String> {
    let (src, header) = forecast_source(d, sel, cursor)?;
    let s = d.sheet;
    let mut ahead = None;
    let mut confidence = 0.95;
    let mut season: Option<u32> = None;
    let parts: Vec<String> = typed.split(',').map(|p| p.trim().to_lowercase()).filter(|p| !p.is_empty()).collect();
    for p in &parts {
        if let Some(n) = p.strip_suffix("ahead").map(str::trim) {
            ahead = Some(n.parse::<u32>().map_err(|_| format!("{p:?}: type how many steps ahead, such as 12 ahead."))?);
        } else if let Some(x) = p.strip_suffix('%') {
            let x: f64 = x.trim().parse().map_err(|_| format!("{p:?}: type the confidence as a percentage, such as 95%."))?;
            confidence = x / 100.0;
        } else if let Some(x) = p.strip_prefix("season") {
            let x = x.trim();
            season = match x {
                "auto" | "" => None,
                "none" | "off" => Some(0),
                n => Some(n.parse::<u32>().ok().filter(|&n| n >= 2 && n <= officina_grid::eval::ets::MAX_SEASON as u32).ok_or(format!("{p:?}: type season auto, none, or a length from 2 to 8760."))?),
            };
        } else if let Ok(n) = p.parse::<u32>() {
            ahead = Some(n);
        } else {
            return Err(format!("{p:?}: type the steps ahead, a confidence like 95%, and season auto, none or a length."));
        }
    }
    let ahead = ahead.unwrap_or(((src.r1 - src.r0 + 1) / 3).max(1));
    if ahead == 0 || ahead > MAX_AHEAD {
        return Err(format!("Forecast 1 to {MAX_AHEAD} steps ahead."));
    }
    if !(confidence > 0.0 && confidence < 1.0) {
        return Err("The confidence is between 0% and 100%.".into());
    }
    // The timeline's step, as FORECAST.ETS works it out.
    let vals = |c: u32| (src.r0..=src.r1).map(|r| d.wb.get(CellAddr::new(s, r, c))).collect::<Vec<_>>();
    let (times, ys) = (vals(src.c0), vals(src.c1));
    let series = officina_grid::eval::ets::series(&ys, &times, true, 1).map_err(|_| "The dates or times need a steady step (a few may be missing) and numbers beside them.".to_string())?;
    let last = times.iter().filter_map(|v| if let Value::Num(x) = v { Some(*x) } else { None }).fold(f64::MIN, f64::max);
    let n = src.r1 - src.r0 + 1;
    let text = |t: &str| Input::Value(Value::str(t));
    let head = |c: u32, fallback: &str| if header { match d.wb.get(CellAddr::new(s, src.r0 - 1, c)) { Value::Empty => fallback.to_string(), _ => d.wb.formatted(CellAddr::new(s, src.r0 - 1, c)).text } } else { fallback.to_string() };
    let (tname, vname) = (head(src.c0, "Timeline"), head(src.c1, "Values"));
    let mut rows: Vec<Vec<Input>> = vec![vec![text(&tname), text(&vname), text(&format!("Forecast({vname})")), text(&format!("Lower Confidence Bound({vname})")), text(&format!("Upper Confidence Bound({vname})"))]];
    for (i, (t, y)) in times.iter().zip(&ys).enumerate() {
        let mut row = vec![Input::Value(t.clone()), Input::Value(y.clone())];
        // The last known point starts the forecast lines, so they join.
        if i as u32 == n - 1 {
            row.extend([2, 3, 4].map(|_| Input::Formula(format!("=B{}", i + 2))));
        }
        rows.push(row);
    }
    let (hist_t, hist_v) = (format!("$A$2:$A${}", n + 1), format!("$B$2:$B${}", n + 1));
    let season_arg = season.map_or(1, |x| x);
    let conf = format!("{confidence}");
    for k in 1..=ahead {
        let r = n + 1 + k; // 1-based sheet row
        let t = last + series.step * k as f64;
        rows.push(vec![
            Input::Value(Value::Num(t)),
            Input::Empty,
            Input::Formula(format!("=FORECAST.ETS(A{r},{hist_v},{hist_t},{season_arg},1)")),
            Input::Formula(format!("=C{r}-FORECAST.ETS.CONFINT(A{r},{hist_v},{hist_t},{conf},{season_arg},1)")),
            Input::Formula(format!("=C{r}+FORECAST.ETS.CONFINT(A{r},{hist_v},{hist_t},{conf},{season_arg},1)")),
        ]);
    }
    let base = format!("Forecast {vname}");
    let base: String = base.chars().filter(|c| !"[]:*?/\\'".contains(*c)).take(24).collect();
    let name = if d.wb.sheet_id(&base).is_none() { base } else { crate::data::copy_name(&d.wb, &base) };
    let count = d.wb.sheet_count() as u16;
    if !d.run_action("Forecast Sheet", vec![GridOp::add_sheet(name.clone())]) || d.wb.sheet_count() as u16 == count {
        return Err(if d.status.is_empty() { "The forecast sheet couldn't be added.".into() } else { d.status.clone() });
    }
    let total = n + ahead;
    let mut ops = vec![GridOp::block(count, 0, 0, rows)];
    // The timeline keeps its number format (dates stay dates).
    let tfmt = d.wb.style(CellAddr::new(s, src.r0, src.c0)).num_fmt.clone();
    ops.push(GridOp::style(Rect { sheet: count, r0: 1, c0: 0, r1: total, c1: 0 }, officina_grid::style::Style { num_fmt: tfmt, ..Default::default() }));
    let vfmt = d.wb.style(CellAddr::new(s, src.r0, src.c1)).num_fmt.clone();
    ops.push(GridOp::style(Rect { sheet: count, r0: 1, c0: 1, r1: total, c1: 4 }, officina_grid::style::Style { num_fmt: vfmt, ..Default::default() }));
    let col = |c: u32| Rect { sheet: count, r0: 1, c0: c, r1: total, c1: c };
    let mut charts = d.wb.charts().to_vec();
    if charts.len() < officina_grid::charts::MAX_CHARTS {
        use officina_grid::charts::{Anchor, Chart, ChartKind, ChartText, Grouping, LegendPos, Series};
        let series = (1..=4).map(|c| Series { name: Some(ChartText::Cell(CellAddr::new(count, 0, c))), cats: Some(col(0)), vals: col(c), kind: None }).collect();
        charts.push(Chart { kind: ChartKind::Line, grouping: Grouping::Standard, title: None, series, legend: Some(LegendPos::Bottom), x_title: None, y_title: None, anchor: Anchor { sheet: count, from: (1, 6, 0.0, 0.0), to: (19, 14, 0.0, 0.0) }, alt: None });
        ops.push(GridOp::Charts(charts));
    }
    d.extend(ops);
    Ok(format!("“{name}” added: {n} points and {ahead} ahead at {}% confidence.", (confidence * 100.0).round()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> Doc {
        let mut d = Doc::from_csv("g.csv", b"Name,Qty,Total\nA,1,\nB,,\nC,3,\n").unwrap().0;
        d.type_into(1, 2, "=B2*2");
        d.type_into(3, 2, "=B4*2");
        d.wb.recalc();
        d
    }

    fn sel(r0: u32, c0: u32, r1: u32, c1: u32) -> CellRange {
        CellRange { r0, c0, r1, c1 }
    }

    #[test]
    fn go_to_special_finds_marks_and_steps() {
        let mut d = doc();
        let w = go_to_special(&mut d, sel(0, 0, 0, 0), (0, 0), "formulas").unwrap();
        assert_eq!((w.cursor, w.say.as_str()), ((1, 2), "2 formulas in 2 areas, marked. Type next to go on."));
        assert_eq!(go_to_special(&mut d, sel(0, 0, 0, 0), (1, 2), "next").unwrap().cursor, (3, 2));
        assert_eq!(go_to_special(&mut d, sel(0, 0, 0, 0), (3, 2), "next").unwrap().cursor, (1, 2), "wraps");
        // Blanks: B3 and C3 (C2 and C4 hold formulas).
        let w = go_to_special(&mut d, sel(0, 0, 0, 0), (0, 0), "blanks").unwrap();
        assert_eq!(d.found, vec![Rect::cell(CellAddr::new(0, 2, 1)), Rect::cell(CellAddr::new(0, 2, 2))]);
        assert_eq!(w.cursor, (2, 1));
        let w = go_to_special(&mut d, sel(0, 0, 3, 1), (0, 0), "constants numbers").unwrap();
        assert!(w.say.starts_with("2 constants"), "{}", w.say);
        assert!(go_to_special(&mut d, sel(0, 0, 0, 0), (0, 0), "constants logicals").is_err());
        assert!(d.found.is_empty());
        assert_eq!(go_to_special(&mut d, sel(0, 0, 0, 0), (0, 0), "last cell").unwrap().cursor, (3, 2));
        let w = go_to_special(&mut d, sel(0, 0, 0, 0), (1, 1), "current region").unwrap();
        assert_eq!(w.select, Some(sel(0, 0, 3, 2)));
        // Dependents of B2: C2.
        assert_eq!(go_to_special(&mut d, sel(0, 0, 0, 0), (1, 1), "dependents").unwrap().cursor, (1, 2));
        assert_eq!(go_to_special(&mut d, sel(0, 0, 0, 0), (1, 2), "precedents").unwrap().cursor, (1, 1));
        // Row differences against column B: in row 2, A2 "A" and C2 differ from B2.
        let w = go_to_special(&mut d, sel(1, 0, 1, 2), (1, 1), "row differences").unwrap();
        assert!(w.say.starts_with("2 row differences"), "{}", w.say);
        // Comments and validation, and the marks going with an edit.
        assert!(go_to_special(&mut d, sel(0, 0, 0, 0), (0, 0), "comments").is_err());
        go_to_special(&mut d, sel(0, 0, 0, 0), (0, 0), "formulas").unwrap();
        d.type_into(5, 5, "x");
        assert!(d.found.is_empty());
        assert!(go_to_special(&mut d, sel(0, 0, 0, 0), (0, 0), "pictures").unwrap_err().contains("type one of"));
    }

    #[test]
    fn visible_cells_leave_hidden_rows_out() {
        let mut d = doc();
        crate::data::run(&mut d, sel(2, 0, 2, 0), (2, 0), crate::data::DataCmd::HideRows);
        assert!(d.wb.sheet(0).row_hidden(2));
        go_to_special(&mut d, sel(0, 0, 3, 1), (0, 0), "visible").unwrap();
        assert_eq!(d.found, vec![Rect { sheet: 0, r0: 0, c0: 0, r1: 1, c1: 1 }, Rect { sheet: 0, r0: 3, c0: 0, r1: 3, c1: 1 }]);
    }

    #[test]
    fn lock_and_hide_toggle_and_wait_for_unprotecting() {
        let mut d = doc();
        assert!(toggle_protection(&mut d, sel(1, 1, 3, 1), true).unwrap().starts_with("Cells unlocked"));
        assert!(d.wb.style(CellAddr::new(0, 3, 1)).unlocked);
        assert!(toggle_protection(&mut d, sel(1, 1, 3, 1), true).unwrap().starts_with("Cells locked"));
        assert!(toggle_protection(&mut d, sel(1, 2, 3, 2), false).unwrap().starts_with("Formulas hidden"));
        assert!(d.wb.style(CellAddr::new(0, 1, 2)).hide_formula);
        assert!(d.undo());
        assert!(!d.wb.style(CellAddr::new(0, 1, 2)).hide_formula);
        d.wb.sheet_mut(0).protection = Some(officina_grid::protection::SheetProtection::new(officina_grid::protection::ProtectionFormat::Excel));
        assert!(toggle_protection(&mut d, sel(1, 2, 3, 2), false).unwrap_err().starts_with("Unprotect"));
        assert!(!d.wb.style(CellAddr::new(0, 1, 2)).hide_formula);
    }

    #[test]
    fn detail_shows_hides_and_the_outline_clears() {
        let mut d = doc();
        let outline = Outline::new(vec![officina_grid::outline::Group { start: 1, end: 2, collapsed: false }], Axis::Rows).unwrap();
        d.run("Group", vec![GridOp::Outline { sheet: 0, axis: Axis::Rows, outline }]);
        assert_eq!(detail(&mut d, sel(1, 0, 1, 0), false).unwrap(), "Detail hidden.");
        assert!(d.wb.sheet(0).row_hidden(2));
        assert!(detail(&mut d, sel(1, 0, 1, 0), false).unwrap_err().contains("hidden already"));
        // From the summary row under the collapsed group.
        assert_eq!(detail(&mut d, sel(3, 0, 3, 0), true).unwrap(), "Detail shown.");
        assert!(!d.wb.sheet(0).row_hidden(2));
        assert!(detail(&mut d, sel(0, 3, 0, 3), true).is_err());
        assert_eq!(clear_outline(&mut d).unwrap(), "Outline cleared: 1 group taken away.");
        assert!(d.wb.sheet(0).outline(Axis::Rows).groups().is_empty());
        assert!(clear_outline(&mut d).is_err());
        assert!(d.undo());
        assert_eq!(d.wb.sheet(0).outline(Axis::Rows).groups().len(), 1);
    }

    #[test]
    fn the_forecast_sheet_extends_the_history() {
        let mut csv = String::from("Month,Sales\n");
        for i in 0..12 {
            csv.push_str(&format!("{},{}\n", i + 1, 100 + 10 * i));
        }
        let mut d = Doc::from_csv("f.csv", csv.as_bytes()).unwrap().0;
        assert_eq!(forecast_start(&d, sel(0, 0, 0, 0), (1, 0)), "4 ahead, 95%, season auto");
        let m = forecast_sheet(&mut d, sel(0, 0, 0, 0), (1, 0), "3 ahead, 90%, season none").unwrap();
        assert_eq!(m, "“Forecast Sales” added: 12 points and 3 ahead at 90% confidence.");
        assert_eq!(d.sheet, 0, "the window moves to the new sheet");
        d.wb.recalc();
        let num = |r: u32, c: u32| match d.wb.get(CellAddr::new(1, r, c)) { Value::Num(x) => x, v => panic!("{v:?}") };
        assert_eq!(d.wb.formatted(CellAddr::new(1, 0, 2)).text, "Forecast(Sales)");
        assert_eq!(num(12, 2), 210.0, "the lines start at the last point");
        assert_eq!(num(15, 0), 15.0);
        assert!((num(15, 2) - 240.0).abs() < 1e-6, "{}", num(15, 2));
        assert!(num(15, 3) <= num(15, 2) && num(15, 4) >= num(15, 2));
        assert_eq!(d.wb.charts().len(), 1);
        assert_eq!(d.wb.charts()[0].series.len(), 4);
        assert!(d.undo());
        assert_eq!((d.wb.sheet_count(), d.wb.charts().len()), (1, 0));
        assert!(forecast_sheet(&mut d, sel(0, 0, 0, 0), (1, 0), "0 ahead").is_err());
        assert!(forecast_sheet(&mut d, sel(0, 0, 0, 0), (1, 0), "season 1").unwrap_err().contains("season auto"));
        assert!(forecast_sheet(&mut d, sel(0, 0, 3, 0), (1, 0), "").unwrap_err().contains("two columns"));
    }
}
