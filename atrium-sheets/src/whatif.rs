//! The Data menu's typed-bar tools (docs/spec/officina.md §16.141):
//! Iterative Calculation, Subtotal, Advanced Filter, Consolidate and
//! What-If Data Table. Each reads one line typed in the bar under the grid
//! and runs as one undo step; a line it can't read changes nothing and
//! says why.

use std::collections::HashMap;

use officina_grid::addr::{a1, parse_a1, CellAddr, Rect, MAX_COLS, MAX_ROWS};
use officina_grid::book::{Input, Iteration};
use officina_grid::ops::GridOp;
use officina_grid::outline::{Group, Outline};
use officina_grid::shift::{shift_op, Axis};
use officina_grid::value::ErrorKind;
use officina_grid::{Value, Workbook};
use pergola::widgets::CellRange;

use crate::data::{columns_named, target, DataCmd};
use crate::doc::Doc;

/// What the bar shows for `kind`: its label, an example, the button and a hint.
pub fn bar(kind: DataCmd) -> (&'static str, &'static str, &'static str, &'static str) {
    match kind {
        DataCmd::RemoveDuplicatesBy => ("Compare", "Name, City", "Remove", "columns (letters or headers) that make a row a duplicate"),
        DataCmd::Iteration => ("Iterate", "100, 0.001", "Set", "most passes and largest change for circular formulas, or off"),
        DataCmd::Subtotal => ("Subtotal", "Region; Sum; Sales", "Add", "at each change in; function; columns to total (or remove)"),
        DataCmd::AdvancedFilter => ("Criteria", "G1:H2, J1, unique", "Filter", "criteria range; a cell to copy to (else in place); unique"),
        DataCmd::Consolidate => ("Consolidate", "Sum; A1:B5, Sheet2!A1:B5; top, left", "Combine", "function; ranges; labels in the top row, left column"),
        DataCmd::DataTable => ("Inputs", "B1, B2", "Fill", "row input cell, column input cell (one may be empty)"),
        DataCmd::EvaluateFormula => ("Evaluate", "", "Next", "each Next works out one more part, the leftmost innermost first"),
        DataCmd::EditRanges => ("Edit ranges", "add Inputs: B2:B9, D1", "Set", "add TITLE: cells, delete TITLE, or list; they can be edited while the sheet is protected"),
        DataCmd::DataForm => ("Record", "Qty = 5; Region = East", "Apply", "change fields; next, prev, delete; new Field = value; …; find Field = value, Qty > 10"),
        DataCmd::ProtectSheet => ("Password", "", "Protect", "optional; it can't be recovered if lost"),
        DataCmd::UnprotectSheet => ("Password", "", "Unprotect", "the password the sheet was protected with"),
        DataCmd::ProtectionOptions => ("Allow", "format cells, autofilter", "Set", "what a protected sheet still allows: format cells, columns or rows, insert hyperlinks, autofilter, objects, scenarios; or none"),
        DataCmd::GoToSpecial => ("Go to", "blanks", "Go", crate::special::KINDS),
        DataCmd::ForecastSheet => ("Forecast", "12 ahead, 95%, season auto", "Create", "steps ahead past the last date, the confidence, and season auto, none or a length"),
        DataCmd::QuickTotals => ("Totals", "sum below", "Add", "sum, average, count, percent or running; below or right"),
        DataCmd::CreateNames => ("Names from", "top, left", "Create", "where the labels are: top row, left column, bottom row, right column"),
        DataCmd::FillAcross => ("Fill to", "all", "Fill", "all, or sheet names; then contents or formats for only those"),
        DataCmd::Sparklines => ("Sparklines", "line B2:F9", "Draw", "line, column or winloss and the data; markers, color 00B050; or clear"),
        DataCmd::Series => ("Series", "linear 2, stop 100", "Fill", "linear, growth, date (day, weekday, month, year) or trend; the step; stop; rows or columns"),
        DataCmd::Scenarios => ("Scenario", "add Best: B1=5, B2=7", "Run", "add NAME: cells, show NAME, delete NAME, or summary and the result cells"),
        _ => ("Split at", "comma, -+, fixed 3, 8", "Split", "a name or characters (+ after: runs count as one), fixed 3, 8 for widths, or empty to find one"),
    }
}

/// The bar's starting text for `kind`.
pub fn start(d: &Doc, sel: CellRange, cursor: (u32, u32), kind: DataCmd) -> String {
    match kind {
        DataCmd::Iteration => match d.wb.iteration() {
            Some(it) => format!("{}, {}", it.count, it.change),
            None => format!("{}, {}", Iteration::DEFAULT.count, Iteration::DEFAULT.change),
        },
        DataCmd::Subtotal => subtotal_start(d, sel, cursor),
        DataCmd::Scenarios => crate::scenarios::start(d, sel),
        DataCmd::Series => "linear 1".into(),
        DataCmd::Sparklines => crate::sparkbar::start(d, sel),
        DataCmd::FillAcross => "all".into(),
        DataCmd::CreateNames => crate::formulas::create_start(d, sel),
        DataCmd::QuickTotals => "sum below".into(),
        DataCmd::GoToSpecial => "blanks".into(),
        DataCmd::ProtectionOptions => crate::lock::options_start(d),
        DataCmd::ForecastSheet => crate::special::forecast_start(d, sel, cursor),
        DataCmd::DataForm => form_start(d, cursor),
        DataCmd::EditRanges => crate::checks::ranges_start(d, sel),
        DataCmd::EvaluateFormula => officina_grid::eval::steps::evaluate_steps(&d.wb, CellAddr::new(d.sheet, cursor.0, cursor.1)).and_then(|s| s.into_iter().next()).unwrap_or_default(),
        _ => String::new(),
    }
}

/// Runs `kind` with what was typed in its bar.
pub fn run(d: &mut Doc, sel: CellRange, cursor: (u32, u32), kind: DataCmd, typed: &str) -> Result<String, String> {
    match kind {
        DataCmd::Iteration => iteration(d, typed),
        DataCmd::Subtotal => subtotal(d, sel, cursor, typed),
        DataCmd::AdvancedFilter => advanced_filter(d, sel, cursor, typed),
        DataCmd::Consolidate => consolidate(d, cursor, typed),
        DataCmd::DataTable => data_table(d, sel, typed),
        DataCmd::Scenarios => crate::scenarios::run(d, typed),
        DataCmd::Series => crate::series::series(d, sel, typed),
        DataCmd::Sparklines => crate::sparkbar::run(d, sel, typed),
        DataCmd::FillAcross => crate::series::fill_across(d, sel, typed),
        DataCmd::CreateNames => crate::formulas::create_names(d, sel, typed),
        DataCmd::QuickTotals => crate::dataform::quick_totals(d, sel, typed),
        DataCmd::ForecastSheet => crate::special::forecast_sheet(d, sel, cursor, typed),
        DataCmd::ProtectSheet => crate::lock::protect(d, typed),
        DataCmd::UnprotectSheet => crate::lock::unprotect(d, typed),
        DataCmd::ProtectionOptions => crate::lock::options(d, typed),
        DataCmd::EditRanges => crate::checks::edit_ranges(d, typed),
        DataCmd::EvaluateFormula => Err(evaluate_next(d, cursor, typed).1),
        _ => Err("Not a bar command.".into()),
    }
}

/// The Data Form bar's text for the record at the cursor: its fields as
/// `Field = value; …`, formulas left out since they can't be typed.
pub fn form_start(d: &Doc, cursor: (u32, u32)) -> String {
    let Ok(f) = crate::dataform::open(d, cursor) else { return String::new() };
    crate::dataform::fields(d, &f).into_iter().filter(|(_, _, formula)| !formula).map(|(n, v, _)| format!("{n} = {v}")).collect::<Vec<_>>().join("; ")
}

/// The Data Form's next state after `typed` at the cursor: the cursor's
/// new place, the bar's new text, and what to say.
pub fn form_next(d: &mut Doc, cursor: (u32, u32), typed: &str) -> Result<((u32, u32), String, String), String> {
    let f = crate::dataform::open(d, cursor)?;
    let (f, say) = crate::dataform::run(d, f, typed)?;
    let at = (f.area.r0 + 1 + f.record.min(f.count().saturating_sub(1)), cursor.1.clamp(f.area.c0, f.area.c1));
    Ok((at, form_start(d, at), say))
}

/// Evaluate Formula's next step after `shown`: (the step, what to say).
/// After the last it starts over from the formula.
pub fn evaluate_next(d: &Doc, cursor: (u32, u32), shown: &str) -> (String, String) {
    let at = CellAddr::new(d.sheet, cursor.0, cursor.1);
    let Some(steps) = officina_grid::eval::steps::evaluate_steps(&d.wb, at) else {
        return (String::new(), format!("{} has no formula to evaluate.", a1(cursor.0, cursor.1)));
    };
    let i = steps.iter().position(|s| s == shown.trim()).map_or(0, |i| (i + 1) % steps.len());
    let say = if i + 1 == steps.len() { format!("Step {} of {}: the value. Next starts over.", i + 1, steps.len()) } else { format!("Step {} of {}.", i + 1, steps.len()) };
    (steps[i].clone(), say)
}

fn refused(d: &Doc) -> Option<String> {
    (crate::sheet_protection::refused(&d.status)).then(|| d.status.clone())
}

/// Iterative Calculation: `count, change` turns it on (circular formulas
/// are calculated again up to `count` times until no number moves more
/// than `change`), `off` turns it off.
pub fn iteration(d: &mut Doc, typed: &str) -> Result<String, String> {
    let t = typed.trim();
    let want = if t.eq_ignore_ascii_case("off") || t.eq_ignore_ascii_case("no") {
        None
    } else {
        let (n, ch) = t.split_once(',').unwrap_or((t, ""));
        let count: u32 = n.trim().parse().map_err(|_| format!("{:?} isn't a number of passes: type 100, 0.001 or off.", n.trim()))?;
        let change = if ch.trim().is_empty() { Iteration::DEFAULT.change } else { ch.trim().parse().map_err(|_| format!("{:?} isn't a change: type 100, 0.001.", ch.trim()))? };
        let it = Iteration { count, change };
        if !it.valid() {
            return Err(format!("Passes go from 1 to {}, and the change is 0 or more.", Iteration::MAX_COUNT));
        }
        Some(it)
    };
    if d.wb.iteration() == want {
        return Ok("Iterative calculation is already set that way.".into());
    }
    d.run("Iterative Calculation", vec![GridOp::Iteration(want)]);
    if let Some(why) = refused(d) {
        return Err(why);
    }
    d.wb.recalc();
    Ok(match want {
        Some(it) => format!("Iterative calculation on: circular formulas run up to {} times, until nothing changes by more than {}.", it.count, it.change),
        None => "Iterative calculation off: circular formulas show #CIRC!.".into(),
    })
}

/// The most groups Subtotal makes.
pub const MAX_SUBTOTAL_GROUPS: usize = 1000;
/// The most rows Subtotal reads.
pub const MAX_SUBTOTAL_ROWS: u32 = 100_000;

/// The SUBTOTAL function number for a name the bar takes.
fn subtotal_code(name: &str) -> Option<(u8, &'static str)> {
    Some(match name.trim().to_lowercase().replace(['.', ' '], "").as_str() {
        "" | "sum" => (9, "Total"),
        "count" | "counta" => (3, "Count"),
        "average" | "avg" | "mean" => (1, "Average"),
        "max" => (4, "Max"),
        "min" => (5, "Min"),
        "product" => (6, "Product"),
        "countnumbers" | "countnums" => (2, "Count"),
        "stdev" | "stddev" => (7, "StdDev"),
        "stdevp" | "stddevp" => (8, "StdDevp"),
        "var" => (10, "Var"),
        "varp" => (11, "Varp"),
        _ => return None,
    })
}

fn header_text(wb: &Workbook, s: u16, r: u32, c: u32) -> String {
    wb.formatted(CellAddr::new(s, r, c)).text.trim().to_string()
}

fn subtotal_start(d: &Doc, sel: CellRange, cursor: (u32, u32)) -> String {
    let Some(t) = target(d, sel, cursor).filter(|t| t.header) else { return String::new() };
    let s = d.sheet;
    let key = cursor.1.clamp(t.rect.c0, t.rect.c1);
    let nums: Vec<String> = (t.rect.c0..=t.rect.c1)
        .filter(|&c| c != key && (t.rect.r0 + 1..=t.rect.r1.min(t.rect.r0 + 50)).any(|r| matches!(d.wb.get(CellAddr::new(s, r, c)), Value::Num(_))))
        .map(|c| header_text(&d.wb, s, t.rect.r0, c))
        .collect();
    format!("{}; Sum; {}", header_text(&d.wb, s, t.rect.r0, key), nums.join(", "))
}

/// Subtotal: at each change in the key column a row totals the columns
/// listed with SUBTOTAL, a Grand Total row ends the data, and an outline
/// groups each block. `remove` takes the subtotal rows out again.
pub fn subtotal(d: &mut Doc, sel: CellRange, cursor: (u32, u32), typed: &str) -> Result<String, String> {
    let s = d.sheet;
    let t = target(d, sel, cursor).ok_or("Put the cursor in your data (or select it) first.")?;
    if d.wb.table_at(s, t.rect.r0, t.rect.c0).is_some() {
        return Err("Subtotal works on plain cells: convert the table to a range first.".into());
    }
    if typed.trim().eq_ignore_ascii_case("remove") || typed.trim().eq_ignore_ascii_case("remove all") {
        return remove_subtotals(d, t.rect);
    }
    if !t.header {
        return Err("Subtotal needs a header row naming the columns.".into());
    }
    let rect = t.rect;
    let (r0, r1) = (rect.r0 + 1, rect.r1);
    if r0 > r1 {
        return Err("There is nothing under the header to total.".into());
    }
    if r1 - r0 >= MAX_SUBTOTAL_ROWS {
        return Err(format!("Subtotal reads at most {MAX_SUBTOTAL_ROWS} rows."));
    }
    if d.wb.sheet(s).hidden.iter().any(|&(a, b)| a <= r1 && b >= r0) {
        return Err("Show all rows first: some of the data is hidden.".into());
    }
    let mut parts = typed.split(';');
    let key_spec = parts.next().unwrap_or("").trim();
    let (code, word) = match parts.next() {
        Some(f) => subtotal_code(f).ok_or_else(|| format!("{:?} isn't a function: use Sum, Count, Average, Max, Min, Product, StdDev or Var.", f.trim()))?,
        None => (9, "Total"),
    };
    let cols_spec = parts.next().unwrap_or("").trim().to_string();
    if parts.next().is_some() {
        return Err("Type the column to group by; the function; the columns to total.".into());
    }
    if key_spec.is_empty() {
        return Err("Type the column to group by, such as: Region; Sum; Sales".into());
    }
    let key = *columns_named(d, &t, rect, key_spec)?.first().ok_or("Name one column to group by.")?;
    let cols: Vec<u32> = if cols_spec.is_empty() {
        (rect.c0..=rect.c1).filter(|&c| c != key && (r0..=r1).any(|r| matches!(d.wb.get(CellAddr::new(s, r, c)), Value::Num(_)))).collect()
    } else {
        columns_named(d, &t, rect, &cols_spec)?
    };
    if cols.is_empty() {
        return Err("No column of numbers to total: name the columns after the function.".into());
    }
    // Each block of equal keys, as (first row, last row).
    let id = |r: u32| match d.wb.get(CellAddr::new(s, r, key)) {
        Value::Str(x) => format!("s{}", x.to_lowercase()),
        v => format!("{v:?}"),
    };
    let mut groups: Vec<(u32, u32)> = Vec::new();
    let mut last = None;
    for r in r0..=r1 {
        let k = id(r);
        if last.as_ref() == Some(&k) {
            if let Some(g) = groups.last_mut() {
                g.1 = r;
            }
        } else {
            if groups.len() == MAX_SUBTOTAL_GROUPS {
                return Err(format!("Subtotal makes at most {MAX_SUBTOTAL_GROUPS} groups; sort by the column first."));
            }
            groups.push((r, r));
            last = Some(k);
        }
    }
    let n = groups.len() as u32;
    let (rows, _) = d.wb.extent(s);
    if rows.max(r1 + 1).saturating_add(n + 1) > MAX_ROWS {
        return Err("No room on the sheet for the total rows.".into());
    }
    let labels: Vec<String> = groups.iter().map(|g| header_text(&d.wb, s, g.0, key)).collect();
    // Rows go in from the bottom up, so the rows above keep their places.
    let grand = shift_op(&d.wb, s, Axis::Rows, r1 + 1, 1).ok_or("This would push data off the sheet.")?;
    if !d.run_action("Subtotal", vec![grand]) {
        return Err(refused(d).unwrap_or_else(|| "Subtotal was refused.".into()));
    }
    for &(_, e) in groups.iter().rev() {
        let op = shift_op(&d.wb, s, Axis::Rows, e + 1, 1).ok_or("This would push data off the sheet.")?;
        d.extend(vec![op]);
    }
    let mut sets = Vec::new();
    let label = |text: String| Input::Value(Value::str(&text));
    for (i, (&(a, e), name)) in groups.iter().zip(&labels).enumerate() {
        let (a, e, at) = (a + i as u32, e + i as u32, e + i as u32 + 1);
        sets.push(GridOp::set(CellAddr::new(s, at, key), label(format!("{name} {word}"))));
        for &c in &cols {
            sets.push(GridOp::set(CellAddr::new(s, at, c), Input::Formula(format!("=SUBTOTAL({code},{}:{})", a1(a, c), a1(e, c)))));
        }
    }
    let end = r1 + n;
    let g_row = end + 1;
    sets.push(GridOp::set(CellAddr::new(s, g_row, key), label(format!("Grand {word}"))));
    for &c in &cols {
        sets.push(GridOp::set(CellAddr::new(s, g_row, c), Input::Formula(format!("=SUBTOTAL({code},{}:{})", a1(r0, c), a1(end, c)))));
    }
    d.extend(sets);
    let mut outline: Vec<Group> = d.wb.sheet(s).outline(Axis::Rows).groups().to_vec();
    outline.push(Group { start: r0, end, collapsed: false });
    outline.extend(groups.iter().enumerate().map(|(i, &(a, e))| Group { start: a + i as u32, end: e + i as u32, collapsed: false }));
    let outlined = match Outline::new(outline, Axis::Rows) {
        Some(o) => {
            d.extend(vec![GridOp::Outline { sheet: s, axis: Axis::Rows, outline: o }]);
            ""
        }
        None => " (too many rows to outline)",
    };
    d.wb.recalc();
    Ok(format!("{n} subtotal {} added by {}{outlined}.", if n == 1 { "row" } else { "rows" }, header_text(&d.wb, s, rect.r0, key)))
}

/// Subtotal's Remove All: the rows of `rect` holding a SUBTOTAL formula go,
/// with the row outline groups inside it.
fn remove_subtotals(d: &mut Doc, rect: Rect) -> Result<String, String> {
    let s = d.sheet;
    if rect.r1 - rect.r0 >= MAX_SUBTOTAL_ROWS {
        return Err(format!("Subtotal reads at most {MAX_SUBTOTAL_ROWS} rows."));
    }
    let totals: Vec<u32> = (rect.r0..=rect.r1)
        .filter(|&r| (rect.c0..=rect.c1).any(|c| d.wb.formula_text(CellAddr::new(s, r, c)).is_some_and(|f| f.to_uppercase().contains("SUBTOTAL("))))
        .collect();
    if totals.is_empty() {
        return Err("No subtotal rows here to remove.".into());
    }
    if totals.len() > MAX_SUBTOTAL_GROUPS + 1 {
        return Err(format!("Subtotal removes at most {} rows at once.", MAX_SUBTOTAL_GROUPS + 1));
    }
    let groups: Vec<Group> = d.wb.sheet(s).outline(Axis::Rows).groups().iter().filter(|g| g.end < rect.r0 || g.start > rect.r1).copied().collect();
    let outline = Outline::new(groups, Axis::Rows).ok_or("The outline couldn't be kept.")?;
    if !d.run_action("Remove Subtotals", vec![GridOp::Outline { sheet: s, axis: Axis::Rows, outline }]) {
        return Err(refused(d).unwrap_or_else(|| "Remove Subtotals was refused.".into()));
    }
    for &r in totals.iter().rev() {
        if let Some(op) = shift_op(&d.wb, s, Axis::Rows, r, -1) {
            d.extend(vec![op]);
        }
    }
    d.wb.recalc();
    let n = totals.len();
    Ok(format!("{n} subtotal {} removed.", if n == 1 { "row" } else { "rows" }))
}

/// A range typed in a bar: `A1`, `A1:C9`, with `Sheet!` or `'My sheet'!`
/// before it for another sheet.
pub fn parse_area(wb: &Workbook, here: u16, typed: &str) -> Result<Rect, String> {
    let t = typed.trim();
    let bad = || format!("{t:?} isn't a cell or range, such as A1:C9 or Sheet2!A1:C9.");
    let (sheet, cells) = match t.rsplit_once('!') {
        Some((name, cells)) => {
            let name = name.trim().trim_matches('\'').replace("''", "'");
            let s = (0..wb.sheet_count() as u16).find(|&i| wb.sheet_name(i).eq_ignore_ascii_case(&name)).ok_or_else(|| format!("There is no sheet called {name:?}."))?;
            (s, cells)
        }
        None => (here, t),
    };
    let cells = cells.replace('$', "").to_ascii_uppercase();
    let (a, b) = cells.split_once(':').unwrap_or((&cells, &cells));
    let (ra, ca) = parse_a1(a.trim()).ok_or_else(bad)?;
    let (rb, cb) = parse_a1(b.trim()).ok_or_else(bad)?;
    Ok(Rect { sheet, r0: ra.min(rb), c0: ca.min(cb), r1: ra.max(rb), c1: ca.max(cb) })
}

fn values(wb: &Workbook, r: &Rect) -> Vec<Value> {
    let mut v = Vec::with_capacity(r.cells() as usize);
    for row in r.r0..=r.r1 {
        for c in r.c0..=r.c1 {
            v.push(wb.get(CellAddr::new(r.sheet, row, c)));
        }
    }
    v
}

/// The most cells Advanced Filter and Consolidate read.
pub const MAX_AREA_CELLS: u64 = 1_000_000;
/// The most criteria cells Advanced Filter reads.
pub const MAX_CRITERIA_CELLS: u64 = 4096;

/// Advanced Filter: `criteria[, copy to][, unique]`. The criteria range's
/// first row names fields of the data, each row below is one way a record
/// may match (every condition in it holding), as the D functions read it.
/// In place, the other records' rows are hidden; with a cell to copy to,
/// the header and the matching records' values are written there.
pub fn advanced_filter(d: &mut Doc, sel: CellRange, cursor: (u32, u32), typed: &str) -> Result<String, String> {
    let s = d.sheet;
    let t = target(d, sel, cursor).ok_or("Put the cursor in your data (or select it) first.")?;
    let rect = t.rect;
    if rect.cells() > MAX_AREA_CELLS {
        return Err(format!("Advanced Filter reads at most {MAX_AREA_CELLS} cells."));
    }
    let mut parts: Vec<&str> = typed.split(',').map(str::trim).filter(|p| !p.is_empty()).collect();
    let unique = parts.last().is_some_and(|p| p.eq_ignore_ascii_case("unique"));
    if unique {
        parts.pop();
    }
    let (crit, to) = match parts.as_slice() {
        [c] => (parse_area(&d.wb, s, c)?, None),
        [c, to] => (parse_area(&d.wb, s, c)?, Some(parse_area(&d.wb, s, to)?)),
        _ => return Err("Type the criteria range, then a cell to copy to if wanted: G1:H2, J1".into()),
    };
    if crit.cells() > MAX_CRITERIA_CELLS || crit.r0 == crit.r1 {
        return Err(format!("The criteria range is a row of field names and at least one row of conditions, at most {MAX_CRITERIA_CELLS} cells."));
    }
    if crit.sheet == s && crit.r0 <= rect.r1 && crit.r1 >= rect.r0 && crit.c0 <= rect.c1 && crit.c1 >= rect.c0 {
        return Err("The criteria range must be outside the data.".into());
    }
    let cols = (rect.c1 - rect.c0 + 1) as usize;
    let db = values(&d.wb, &rect);
    let picked = officina_grid::eval::database::picks(&db, cols, &values(&d.wb, &crit), (crit.c1 - crit.c0 + 1) as usize)
        .ok_or("Each criteria heading over a condition must name a column of the data.")?;
    let mut keep: Vec<u32> = picked.iter().enumerate().filter(|p| *p.1).map(|(i, _)| rect.r0 + 1 + i as u32).collect();
    if unique {
        let mut seen = std::collections::HashSet::new();
        keep.retain(|&r| seen.insert((rect.c0..=rect.c1).map(|c| format!("{:?}", d.wb.get(CellAddr::new(s, r, c)))).collect::<Vec<_>>()));
    }
    let n = keep.len();
    let records = |n: usize| if n == 1 { "1 record".to_string() } else { format!("{n} records") };
    match to {
        None => {
            let mut st = d.wb.filter_state(s);
            let (a, b) = (rect.r0 + 1, rect.r1);
            // Rows hidden outside the data stay; inside it, only the misses are.
            let mut hidden: Vec<(u32, u32)> = st.hidden.iter().flat_map(|&(x, y)| {
                let mut v = Vec::new();
                if x < a { v.push((x, y.min(a - 1))) }
                if y > b { v.push((x.max(b + 1), y)) }
                v
            }).collect();
            let mut k = keep.iter().peekable();
            for r in a..=b {
                if k.peek() == Some(&&r) {
                    k.next();
                } else {
                    hidden.push((r, r));
                }
            }
            st.hidden = officina_grid::filter::merge(hidden);
            d.run("Advanced Filter", vec![GridOp::Filter(st)]);
            if let Some(why) = refused(d) {
                return Err(why);
            }
            Ok(format!("{} of {} shown.", records(n), rect.r1 - rect.r0))
        }
        Some(to) => {
            let (h, w) = (n as u32 + 1, rect.c1 - rect.c0 + 1);
            if to.r0 + h > MAX_ROWS || to.c0 + w > MAX_COLS {
                return Err("The copy doesn't fit there on the sheet.".into());
            }
            if to.sheet == s && to.r0 <= rect.r1 && to.r0 + h > rect.r0 && to.c0 <= rect.c1 && to.c0 + w > rect.c0 {
                return Err("Copy to a place outside the data.".into());
            }
            let row = |r: u32| (rect.c0..=rect.c1).map(|c| match d.wb.get(CellAddr::new(s, r, c)) {
                Value::Empty => Input::Empty,
                v => Input::Value(v),
            }).collect::<Vec<_>>();
            let rows: Vec<Vec<Input>> = std::iter::once(rect.r0).chain(keep.iter().copied()).map(row).collect();
            let clear = Rect { sheet: to.sheet, r0: to.r0, c0: to.c0, r1: to.r0 + h - 1, c1: to.c0 + w - 1 };
            d.run("Advanced Filter", vec![GridOp::clear(clear), GridOp::block(to.sheet, to.r0, to.c0, rows)]);
            if let Some(why) = refused(d) {
                return Err(why);
            }
            Ok(format!("{} copied to {}.", records(n), a1(to.r0, to.c0)))
        }
    }
}

/// A Consolidate (and Data Table) result too big to write.
pub const MAX_RESULT_CELLS: usize = 100_000;
/// The most ranges Consolidate combines.
pub const MAX_RANGES: usize = 64;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Fold {
    Sum,
    Count,
    Average,
    Max,
    Min,
    Product,
    CountNums,
    StdDev,
    StdDevP,
    Var,
    VarP,
}

fn fold_named(name: &str) -> Option<Fold> {
    Some(match name.trim().to_lowercase().replace(['.', ' '], "").as_str() {
        "sum" => Fold::Sum,
        "count" | "counta" => Fold::Count,
        "average" | "avg" | "mean" => Fold::Average,
        "max" => Fold::Max,
        "min" => Fold::Min,
        "product" => Fold::Product,
        "countnumbers" | "countnums" => Fold::CountNums,
        "stdev" | "stddev" => Fold::StdDev,
        "stdevp" | "stddevp" => Fold::StdDevP,
        "var" => Fold::Var,
        "varp" => Fold::VarP,
        _ => return None,
    })
}

#[derive(Default, Clone)]
struct Acc {
    nums: Vec<f64>,
    any: usize,
}

impl Acc {
    fn add(&mut self, v: &Value) {
        match v {
            Value::Empty => {}
            Value::Num(x) => {
                self.nums.push(*x);
                self.any += 1;
            }
            _ => self.any += 1,
        }
    }

    fn fold(&self, f: Fold) -> Value {
        if self.any == 0 {
            return Value::Empty;
        }
        let n = self.nums.len() as f64;
        let mean = self.nums.iter().sum::<f64>() / n;
        let ss = || self.nums.iter().map(|x| (x - mean).powi(2)).sum::<f64>();
        let div0 = Value::Err(ErrorKind::Div0);
        match f {
            Fold::Sum => Value::Num(self.nums.iter().sum()),
            Fold::Count => Value::Num(self.any as f64),
            Fold::CountNums => Value::Num(n),
            Fold::Average if n == 0.0 => div0,
            Fold::Average => Value::Num(mean),
            Fold::Max => Value::Num(self.nums.iter().copied().fold(f64::NEG_INFINITY, f64::max).max(if n == 0.0 { 0.0 } else { f64::NEG_INFINITY })),
            Fold::Min => Value::Num(self.nums.iter().copied().fold(f64::INFINITY, f64::min).min(if n == 0.0 { 0.0 } else { f64::INFINITY })),
            Fold::Product if n == 0.0 => Value::Num(0.0),
            Fold::Product => Value::Num(self.nums.iter().product()),
            Fold::StdDev | Fold::Var if n < 2.0 => div0,
            Fold::StdDevP | Fold::VarP if n < 1.0 => div0,
            Fold::StdDev => Value::Num((ss() / (n - 1.0)).sqrt()),
            Fold::Var => Value::Num(ss() / (n - 1.0)),
            Fold::StdDevP => Value::Num((ss() / n).sqrt()),
            Fold::VarP => Value::Num(ss() / n),
        }
    }
}

/// Labels down one side of the result: positions, or names in the order
/// first met (matched without regard to case).
#[derive(Default)]
struct Keys {
    shown: Vec<Value>,
    at: HashMap<String, usize>,
}

impl Keys {
    fn of(&mut self, wb: &Workbook, a: CellAddr) -> usize {
        let text = wb.formatted(a).text.trim().to_string();
        let k = text.to_lowercase();
        if let Some(&i) = self.at.get(&k) {
            return i;
        }
        self.shown.push(if text.is_empty() { Value::Empty } else { Value::str(&text) });
        self.at.insert(k, self.shown.len() - 1);
        self.shown.len() - 1
    }
}

/// Consolidate: `function; range, range…[; top, left]`. The ranges'
/// values are combined cell by cell (by position), or, with labels, by the
/// names in their top row and/or left column, and the result is written
/// as values at the cursor.
pub fn consolidate(d: &mut Doc, cursor: (u32, u32), typed: &str) -> Result<String, String> {
    let s = d.sheet;
    let mut parts = typed.split(';');
    let f_name = parts.next().unwrap_or("").trim();
    let fold = fold_named(f_name).ok_or_else(|| format!("{f_name:?} isn't a function: use Sum, Count, Average, Max, Min, Product, StdDev or Var."))?;
    let areas_text = parts.next().unwrap_or("");
    let labels = parts.next().unwrap_or("").to_lowercase();
    if parts.next().is_some() {
        return Err("Type the function; the ranges; where the labels are.".into());
    }
    let (top, left) = (labels.contains("top") || labels.contains("both"), labels.contains("left") || labels.contains("both"));
    if !labels.trim().is_empty() && !top && !left && !labels.contains("none") {
        return Err("Labels are in the top row, the left column, or both: type top, left.".into());
    }
    let mut areas = Vec::new();
    let mut total = 0u64;
    for p in areas_text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        if areas.len() == MAX_RANGES {
            return Err(format!("Consolidate combines at most {MAX_RANGES} ranges."));
        }
        let r = parse_area(&d.wb, s, p)?;
        total += r.cells();
        if total > MAX_AREA_CELLS {
            return Err(format!("Consolidate reads at most {MAX_AREA_CELLS} cells."));
        }
        areas.push(r);
    }
    if areas.is_empty() {
        return Err("Type the ranges to combine, such as: Sum; A1:B5, Sheet2!A1:B5".into());
    }
    let (mut rows, mut cols) = (Keys::default(), Keys::default());
    let mut cells: HashMap<(usize, usize), Acc> = HashMap::new();
    let (mut nr, mut nc) = (0usize, 0usize);
    for a in &areas {
        let (dr, dc) = (u32::from(top), u32::from(left));
        if a.r0 + dr > a.r1 || a.c0 + dc > a.c1 {
            continue;
        }
        let col_at: Vec<usize> = (a.c0 + dc..=a.c1).map(|c| if top { cols.of(&d.wb, CellAddr::new(a.sheet, a.r0, c)) } else { (c - a.c0) as usize }).collect();
        for r in a.r0 + dr..=a.r1 {
            let i = if left { rows.of(&d.wb, CellAddr::new(a.sheet, r, a.c0)) } else { (r - a.r0 - dr) as usize };
            nr = nr.max(i + 1);
            for (k, c) in (a.c0 + dc..=a.c1).enumerate() {
                let j = col_at[k];
                nc = nc.max(j + 1);
                cells.entry((i, j)).or_default().add(&d.wb.get(CellAddr::new(a.sheet, r, c)));
            }
        }
    }
    let (h, w) = (nr + usize::from(top), nc + usize::from(left));
    if h == 0 || w == 0 {
        return Err("The ranges hold nothing under their labels.".into());
    }
    if h.saturating_mul(w) > MAX_RESULT_CELLS || cursor.0 as u64 + h as u64 > u64::from(MAX_ROWS) || cursor.1 as u64 + w as u64 > u64::from(MAX_COLS) {
        return Err("The result doesn't fit there.".into());
    }
    let out = Rect { sheet: s, r0: cursor.0, c0: cursor.1, r1: cursor.0 + h as u32 - 1, c1: cursor.1 + w as u32 - 1 };
    if areas.iter().any(|a| a.sheet == s && a.r0 <= out.r1 && a.r1 >= out.r0 && a.c0 <= out.c1 && a.c1 >= out.c0) {
        return Err("Put the cursor outside the ranges: the result would cover them.".into());
    }
    let put = |v: Value| if matches!(v, Value::Empty) { Input::Empty } else { Input::Value(v) };
    let mut grid = Vec::with_capacity(h);
    if top {
        let mut head = Vec::with_capacity(w);
        if left {
            head.push(Input::Empty);
        }
        head.extend((0..nc).map(|j| put(cols.shown.get(j).cloned().unwrap_or(Value::Empty))));
        grid.push(head);
    }
    for i in 0..nr {
        let mut row = Vec::with_capacity(w);
        if left {
            row.push(put(rows.shown.get(i).cloned().unwrap_or(Value::Empty)));
        }
        row.extend((0..nc).map(|j| put(cells.get(&(i, j)).map_or(Value::Empty, |a| a.fold(fold)))));
        grid.push(row);
    }
    d.run("Consolidate", vec![GridOp::clear(out), GridOp::block(s, out.r0, out.c0, grid)]);
    if let Some(why) = refused(d) {
        return Err(why);
    }
    Ok(format!("{} ranges combined into {}:{}.", areas.len(), a1(out.r0, out.c0), a1(out.r1, out.c1)))
}

/// The most cells a Data Table fills (each one a recalculation).
pub const MAX_TABLE_CELLS: u64 = 10_000;
/// The longest a Data Table runs.
pub const TABLE_TIME: std::time::Duration = std::time::Duration::from_secs(10);

/// What-If Data Table on the selection: `row input, column input`, either
/// one empty. With a column input, the selection's first column holds the
/// values to try and its first row the formulas (beside the corner); with
/// a row input, the other way round; with both, the corner holds the one
/// formula. Each result cell gets the formula's value with the input
/// cell(s) set to its row's and column's value. Excel keeps a TABLE()
/// array formula there; these are values, so run it again after a change.
pub fn data_table(d: &mut Doc, sel: CellRange, typed: &str) -> Result<String, String> {
    let s = d.sheet;
    let (row_in, col_in) = match typed.split_once(',') {
        Some((a, b)) => (a.trim(), b.trim()),
        None => ("", typed.trim()),
    };
    let cell = |t: &str| -> Result<Option<CellAddr>, String> {
        if t.is_empty() {
            return Ok(None);
        }
        let (r, c) = parse_a1(&t.replace('$', "").to_ascii_uppercase()).ok_or_else(|| format!("{t:?} isn't a cell on this sheet, such as B2."))?;
        Ok(Some(CellAddr::new(s, r, c)))
    };
    let (ri, ci) = (cell(row_in)?, cell(col_in)?);
    if ri.is_none() && ci.is_none() {
        return Err("Type the input cell: B1, B2 for a row and a column input, or , B2 for a column input only.".into());
    }
    if sel.r1 == sel.r0 || sel.c1 == sel.c0 {
        return Err("Select the table: the values to try along its edge and the formulas beside them.".into());
    }
    let inner = u64::from(sel.r1 - sel.r0) * u64::from(sel.c1 - sel.c0);
    if inner > MAX_TABLE_CELLS {
        return Err(format!("A data table fills at most {MAX_TABLE_CELLS} cells."));
    }
    let inside = |a: CellAddr| (sel.r0..=sel.r1).contains(&a.row) && (sel.c0..=sel.c1).contains(&a.col);
    for x in [ri, ci].into_iter().flatten() {
        if inside(x) {
            return Err(format!("The input cell {} must be outside the table.", a1(x.row, x.col)));
        }
        if d.wb.formula_text(x).is_some() {
            return Err(format!("{} holds a formula; an input cell holds a value.", a1(x.row, x.col)));
        }
    }
    let at = |r: u32, c: u32| CellAddr::new(s, r, c);
    let both = ri.is_some() && ci.is_some();
    if both && d.wb.formula_text(at(sel.r0, sel.c0)).is_none() {
        return Err(format!("Put the formula in the table's corner, {}.", a1(sel.r0, sel.c0)));
    }
    if d.wb.recalc_pending() {
        d.wb.recalc();
    }
    let olds: Vec<(CellAddr, Value)> = [ri, ci].into_iter().flatten().map(|x| (x, d.wb.get(x))).collect();
    let deadline = std::time::Instant::now() + TABLE_TIME;
    let mut grid: Vec<Vec<Input>> = Vec::new();
    let mut late = false;
    'rows: for r in sel.r0 + 1..=sel.r1 {
        let mut row = Vec::new();
        for c in sel.c0 + 1..=sel.c1 {
            if std::time::Instant::now() > deadline {
                late = true;
                break 'rows;
            }
            // Which formula, and which values go into the input cells.
            let (formula, sets) = match (ri, ci) {
                (Some(x), Some(y)) => (at(sel.r0, sel.c0), vec![(x, d.wb.get(at(sel.r0, c))), (y, d.wb.get(at(r, sel.c0)))]),
                (None, Some(y)) => (at(sel.r0, c), vec![(y, d.wb.get(at(r, sel.c0)))]),
                (Some(x), _) => (at(r, sel.c0), vec![(x, d.wb.get(at(sel.r0, c)))]),
                (None, None) => break 'rows,
            };
            if d.wb.formula_text(formula).is_none() {
                row.push(Input::Empty);
                continue;
            }
            for (x, v) in sets {
                d.wb.set_value(x, v);
            }
            d.wb.recalc();
            row.push(match d.wb.get(formula) {
                Value::Empty => Input::Empty,
                v => Input::Value(v),
            });
        }
        grid.push(row);
    }
    // The workbook as it was; the table goes in as one undo step.
    for (x, v) in olds {
        d.wb.set_value(x, v);
    }
    d.wb.recalc();
    if late {
        return Err(format!("The data table took longer than {} s; try a smaller one.", TABLE_TIME.as_secs()));
    }
    d.run("Data Table", vec![GridOp::block(s, sel.r0 + 1, sel.c0 + 1, grid)]);
    if let Some(why) = refused(d) {
        return Err(why);
    }
    Ok(format!("Data table filled: {inner} {} from {}{}{}.", if inner == 1 { "value" } else { "values" },
        ri.map_or(String::new(), |x| format!("row input {}", a1(x.row, x.col))),
        if both { " and " } else { "" },
        ci.map_or(String::new(), |x| format!("column input {}", a1(x.row, x.col)))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn num(d: &Doc, r: u32, c: u32) -> f64 {
        match d.wb.get(CellAddr::new(d.sheet, r, c)) {
            Value::Num(x) => x,
            v => panic!("{} is {v:?}", a1(r, c)),
        }
    }

    fn text(d: &Doc, r: u32, c: u32) -> String {
        d.wb.formatted(CellAddr::new(d.sheet, r, c)).text
    }

    fn one(r: u32, c: u32) -> CellRange {
        CellRange { r0: r, c0: c, r1: r, c1: c }
    }

    #[test]
    fn iteration_bar_turns_a_cycle_from_circ_to_a_value_and_undoes() {
        let mut d = Doc::from_csv("i.csv", b"1,2\n").unwrap().0;
        d.type_into(0, 0, "=B1/2+1");
        d.type_into(0, 1, "=A1");
        d.wb.recalc();
        assert_eq!(d.wb.get(CellAddr::new(0, 0, 0)), Value::Err(ErrorKind::Circ));
        assert_eq!(start(&d, one(0, 0), (0, 0), DataCmd::Iteration), "100, 0.001");
        assert!(iteration(&mut d, "0, 1").unwrap_err().contains("Passes go from 1"));
        assert!(iteration(&mut d, "many").unwrap_err().contains("isn't a number"));
        let m = iteration(&mut d, "200, 0.0001").unwrap();
        assert!(m.contains("on"), "{m}");
        assert!((num(&d, 0, 0) - 2.0).abs() < 0.001);
        assert_eq!(start(&d, one(0, 0), (0, 0), DataCmd::Iteration), "200, 0.0001");
        assert!(d.undo());
        d.wb.recalc();
        assert_eq!(d.wb.iteration(), None);
        assert_eq!(d.wb.get(CellAddr::new(0, 0, 0)), Value::Err(ErrorKind::Circ));
        iteration(&mut d, "50").unwrap();
        iteration(&mut d, "off").unwrap();
        assert_eq!(d.wb.iteration(), None);
    }

    const SALES: &[u8] = b"Region,Rep,Sales\nEast,Ann,10\nEast,Bo,20\nWest,Cy,5\nWest,Di,7\nWest,Ed,3\nNorth,Fa,100\n";

    #[test]
    fn subtotal_adds_a_total_at_each_change_with_an_outline_and_removes_them() {
        let mut d = Doc::from_csv("s.csv", SALES).unwrap().0;
        d.wb.recalc();
        assert_eq!(start(&d, one(1, 0), (1, 0), DataCmd::Subtotal), "Region; Sum; Sales");
        assert!(subtotal(&mut d, one(1, 0), (1, 0), "Region; Median; Sales").unwrap_err().contains("isn't a function"));
        assert!(subtotal(&mut d, one(1, 0), (1, 0), "Place; Sum; Sales").unwrap_err().contains("isn't a column"));
        let m = subtotal(&mut d, one(1, 0), (1, 0), "Region; Sum; Sales").unwrap();
        assert!(m.starts_with("3 subtotal rows"), "{m}");
        // East 1-2, total 3; West 4-6, total 7; North 8, total 9; grand 10.
        assert_eq!(text(&d, 3, 0), "East Total");
        assert_eq!(num(&d, 3, 2), 30.0);
        assert_eq!(text(&d, 7, 0), "West Total");
        assert_eq!(num(&d, 7, 2), 15.0);
        assert_eq!(text(&d, 9, 0), "North Total");
        assert_eq!(num(&d, 9, 2), 100.0);
        assert_eq!(text(&d, 10, 0), "Grand Total");
        assert_eq!(num(&d, 10, 2), 145.0, "the grand total skips the subtotals");
        assert_eq!(d.wb.formula_text(CellAddr::new(0, 7, 2)).as_deref(), Some("=SUBTOTAL(9,C5:C7)"));
        let groups = d.wb.sheet(0).outline(Axis::Rows).groups().to_vec();
        assert_eq!(groups.len(), 4, "{groups:?}");
        assert!(groups.iter().any(|g| (g.start, g.end) == (1, 9)) && groups.iter().any(|g| (g.start, g.end) == (4, 6)));
        // Remove All: the data as it was, outline gone.
        let m = subtotal(&mut d, one(1, 0), (1, 0), "remove").unwrap();
        assert!(m.starts_with("4 subtotal rows removed"), "{m}");
        assert_eq!(text(&d, 3, 0), "West");
        assert_eq!((text(&d, 6, 0).as_str(), text(&d, 7, 0).as_str()), ("North", ""));
        assert!(d.wb.sheet(0).outline(Axis::Rows).groups().is_empty());
        // Undo brings the totals back as one step each way.
        assert!(d.undo());
        d.wb.recalc();
        assert_eq!(text(&d, 10, 0), "Grand Total");
        assert!(d.undo());
        d.wb.recalc();
        assert_eq!((text(&d, 6, 0).as_str(), text(&d, 7, 0).as_str()), ("North", ""));
        assert!(subtotal(&mut d, one(1, 0), (1, 0), "remove").unwrap_err().contains("No subtotal rows"));
        // Average by rep count.
        subtotal(&mut d, one(1, 0), (1, 0), "Region; Average; C").unwrap();
        assert_eq!(text(&d, 3, 0), "East Average");
        assert_eq!(num(&d, 3, 2), 15.0);
    }

    #[test]
    fn advanced_filter_hides_rows_in_place_or_copies_matches_with_or_of_rows() {
        let mut d = Doc::from_csv("a.csv", b"Region,Rep,Sales\nEast,Ann,10\nEast,Bo,20\nWest,Cy,5\nWest,Di,7\nWest,Ed,3\nNorth,Fa,100\n,,\n,,\n").unwrap().0;
        // Criteria in E1:F3: East with Sales > 15, or any North.
        for (r, c, t) in [(0, 4, "Region"), (0, 5, "Sales"), (1, 4, "East"), (1, 5, ">15"), (2, 4, "North")] {
            d.type_into(r, c, t);
        }
        d.wb.recalc();
        let m = advanced_filter(&mut d, one(1, 0), (1, 0), "E1:F3").unwrap();
        assert_eq!(m, "2 records of 6 shown.");
        let sh = d.wb.sheet(0);
        assert!(sh.row_hidden(1) && !sh.row_hidden(2) && sh.row_hidden(3) && !sh.row_hidden(6) && !sh.row_hidden(0));
        // Copy to H1: the header and the two matches, values only.
        let m = advanced_filter(&mut d, one(1, 0), (1, 0), "$E$1:$F$3, H1").unwrap();
        assert!(m.starts_with("2 records copied to H1"), "{m}");
        assert_eq!(text(&d, 0, 7), "Region");
        assert_eq!(text(&d, 1, 8), "Bo");
        assert_eq!(num(&d, 2, 9), 100.0);
        // A heading that isn't a field, criteria over the data, a bad range.
        d.type_into(0, 5, "Price");
        d.wb.recalc();
        assert!(advanced_filter(&mut d, one(1, 0), (1, 0), "E1:F3").unwrap_err().contains("name a column"));
        assert!(advanced_filter(&mut d, one(1, 0), (1, 0), "A1:B3").unwrap_err().contains("outside the data"));
        assert!(advanced_filter(&mut d, one(1, 0), (1, 0), "nowhere").unwrap_err().contains("isn't a cell"));
    }

    #[test]
    fn advanced_filter_unique_drops_repeated_records() {
        let mut d = Doc::from_csv("u.csv", b"Fruit,Qty\npear,1\npear,1\nfig,2\n").unwrap().0;
        d.type_into(0, 3, "Fruit");
        d.wb.recalc();
        // A criteria row with no conditions picks every record.
        let m = advanced_filter(&mut d, one(1, 0), (1, 0), "D1:D2, F1, unique").unwrap();
        assert!(m.starts_with("2 records"), "{m}");
        assert_eq!(text(&d, 2, 5), "fig");
    }

    #[test]
    fn consolidate_by_position_and_by_labels_across_sheets() {
        let mut d = Doc::from_csv("c.csv", b"Item,Q1\npen,1\ncup,2\n").unwrap().0;
        d.type_into(0, 3, "Item");
        d.type_into(0, 4, "Q1");
        d.type_into(1, 3, "cup");
        d.type_into(1, 4, "10");
        d.type_into(2, 3, "ink");
        d.type_into(2, 4, "5");
        d.wb.recalc();
        // By labels: pen 1, cup 12, ink 5.
        let m = consolidate(&mut d, (5, 0), "Sum; A1:B3, D1:E3; top, left").unwrap();
        assert!(m.contains("2 ranges combined into A6:B9"), "{m}");
        assert_eq!(text(&d, 5, 1), "Q1");
        assert_eq!((text(&d, 6, 0).as_str(), num(&d, 6, 1)), ("pen", 1.0));
        assert_eq!((text(&d, 7, 0).as_str(), num(&d, 7, 1)), ("cup", 12.0));
        assert_eq!((text(&d, 8, 0).as_str(), num(&d, 8, 1)), ("ink", 5.0));
        // By position: B2:B3 and E2:E3 averaged cell by cell.
        consolidate(&mut d, (10, 0), "average; B2:B3, E2:E3").unwrap();
        assert_eq!(num(&d, 10, 0), 5.5);
        assert_eq!(num(&d, 11, 0), 3.5);
        assert!(consolidate(&mut d, (10, 0), "median; B2:B3").unwrap_err().contains("isn't a function"));
        assert!(consolidate(&mut d, (1, 1), "sum; B2:B3").unwrap_err().contains("outside the ranges"));
        assert!(consolidate(&mut d, (20, 0), "sum; Nope!A1:B2").unwrap_err().contains("no sheet"));
        assert!(consolidate(&mut d, (20, 0), "sum; A1:B2; sideways").unwrap_err().contains("top row"));
    }

    #[test]
    fn data_table_fills_one_and_two_input_tables_and_leaves_the_inputs() {
        // B1 rate, B2 = 100 * B1; the table D1:E4 tries rates 1..3 down D.
        let mut d = Doc::from_csv("t.csv", b"rate,0.5\n").unwrap().0;
        d.type_into(1, 1, "=100*B1");
        d.type_into(0, 4, "=B2");
        for (r, v) in [(1, "1"), (2, "2"), (3, "3")] {
            d.type_into(r, 3, v);
        }
        d.wb.recalc();
        let sel = CellRange { r0: 0, c0: 3, r1: 3, c1: 4 };
        let m = data_table(&mut d, sel, ", B1").unwrap();
        assert!(m.contains("3 values") && m.contains("column input B1"), "{m}");
        assert_eq!([num(&d, 1, 4), num(&d, 2, 4), num(&d, 3, 4)], [100.0, 200.0, 300.0]);
        assert_eq!(num(&d, 0, 1), 0.5, "the input cell is left as it was");
        assert_eq!(num(&d, 1, 1), 50.0);
        // Two inputs: G1 = A5*B5 in the corner, 1..2 across, 10..20 down.
        d.type_into(4, 0, "1");
        d.type_into(4, 1, "1");
        d.type_into(5, 6, "=A5*B5");
        d.type_into(5, 7, "1");
        d.type_into(5, 8, "2");
        d.type_into(6, 6, "10");
        d.type_into(7, 6, "20");
        d.wb.recalc();
        let sel = CellRange { r0: 5, c0: 6, r1: 7, c1: 8 };
        data_table(&mut d, sel, "A5, B5").unwrap();
        assert_eq!([num(&d, 6, 7), num(&d, 6, 8), num(&d, 7, 7), num(&d, 7, 8)], [10.0, 20.0, 20.0, 40.0]);
        assert!(d.undo());
        assert_eq!(d.wb.get(CellAddr::new(0, 7, 8)), Value::Empty);
        assert!(data_table(&mut d, sel, "").unwrap_err().contains("input cell"));
        assert!(data_table(&mut d, sel, "G7, B5").unwrap_err().contains("outside the table"));
        assert!(data_table(&mut d, sel, "B2, A5").unwrap_err().contains("holds a formula"));
        assert!(data_table(&mut d, one(0, 0), "B1").unwrap_err().contains("Select the table"));
    }
}
