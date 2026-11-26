//! Data Analysis… (docs/spec/officina.md §16.161), as Excel's Analysis
//! ToolPak: Descriptive Statistics, Histogram, Rank and Percentile, Moving
//! Average, Exponential Smoothing, Correlation, Covariance, Sampling and
//! Random Number Generation, typed in one bar. Each writes its table on a
//! new sheet, or at `to CELL` when every cell there is empty, as one undo
//! step.

use officina_grid::addr::{a1, col_name, CellAddr, Rect, MAX_COLS, MAX_ROWS};
use officina_grid::book::Input;
use officina_grid::ops::GridOp;
use officina_grid::value::ErrorKind;
use officina_grid::{Value, Workbook};
use pergola::widgets::CellRange;

use crate::doc::Doc;
use crate::whatif::{parse_area, MAX_AREA_CELLS};

/// The most columns Correlation and Covariance compare.
const MAX_SERIES: u32 = 256;
/// The most cells a tool writes.
const MAX_OUT: u64 = 1_000_000;
/// The most bins a histogram has.
const MAX_BINS: usize = 10_000;

/// The tools the bar knows, as their first word.
pub const TOOLS: &str = "descriptive, histogram, rank, moving average, smoothing, correlation, covariance, sampling, random";

/// The bar's text to start with: descriptive statistics of the selection,
/// with labels when its first row is text over numbers.
pub fn start(d: &Doc, sel: CellRange) -> String {
    let area = Rect { sheet: d.sheet, r0: sel.r0, c0: sel.c0, r1: sel.r1, c1: sel.c1 };
    let text_top = sel.r1 > sel.r0 && matches!(d.wb.get(CellAddr::new(d.sheet, sel.r0, sel.c0)), Value::Str(_));
    format!("descriptive {}{}", range_text(&area), if text_top { ", labels" } else { "" })
}

fn range_text(r: &Rect) -> String {
    if r.r0 == r.r1 && r.c0 == r.c1 { a1(r.r0, r.c0) } else { format!("{}:{}", a1(r.r0, r.c0), a1(r.r1, r.c1)) }
}

/// `Sheet1!$A$2:$A$9`, for a formula on any sheet.
fn abs_ref(wb: &Workbook, r: &Rect) -> String {
    let cell = |row: u32, c: u32| format!("${}${}", col_name(c), row as u64 + 1);
    let cells = if r.r0 == r.r1 && r.c0 == r.c1 { cell(r.r0, r.c0) } else { format!("{}:{}", cell(r.r0, r.c0), cell(r.r1, r.c1)) };
    format!("{}!{cells}", crate::formulas::sheet_ref(wb, r.sheet))
}

/// One series of the input: its label and its cells (below the label).
struct Series {
    label: String,
    area: Rect,
}

fn series(wb: &Workbook, area: &Rect, labels: bool) -> Result<Vec<Series>, String> {
    if area.cells() > MAX_AREA_CELLS {
        return Err(format!("Analyse at most {MAX_AREA_CELLS} cells at once."));
    }
    if labels && area.r0 == area.r1 {
        return Err("With labels the range needs a row of data under them.".into());
    }
    let r0 = if labels { area.r0 + 1 } else { area.r0 };
    Ok((area.c0..=area.c1)
        .enumerate()
        .map(|(i, c)| {
            let label = if labels { wb.formatted(CellAddr::new(area.sheet, area.r0, c)).text } else { String::new() };
            let label = if label.trim().is_empty() { format!("Column {}", i + 1) } else { label };
            Series { label, area: Rect { sheet: area.sheet, r0, c0: c, r1: area.r1, c1: c } }
        })
        .collect())
}

/// The numbers of `r`, in order (text, blanks and errors left out).
fn numbers(wb: &Workbook, r: &Rect) -> Vec<f64> {
    let mut v = Vec::new();
    for row in r.r0..=r.r1 {
        for c in r.c0..=r.c1 {
            if let Value::Num(x) = wb.get(CellAddr::new(r.sheet, row, c)) {
                v.push(x);
            }
        }
    }
    v
}

/// What the tool writes: rows of inputs from its top-left, and number
/// formats for some of its cells (row, col offsets from there).
struct Table {
    name: &'static str,
    rows: Vec<Vec<Input>>,
    formats: Vec<(u32, u32, u32, u32, &'static str)>,
    say: String,
}

fn text(s: &str) -> Input {
    Input::Value(Value::str(s))
}

fn num(x: f64) -> Input {
    Input::Value(Value::Num(x))
}

fn na() -> Input {
    Input::Value(Value::Err(ErrorKind::NA))
}

/// The options after the range: words, and `word value` pairs.
struct Options {
    labels: bool,
    rest: Vec<String>,
    to: Option<String>,
    seed: Option<u64>,
}

fn options(parts: &[&str]) -> Result<Options, String> {
    let mut o = Options { labels: false, rest: Vec::new(), to: None, seed: None };
    for p in parts {
        let p = p.trim();
        let low = p.to_lowercase();
        if p.is_empty() {
            continue;
        } else if low == "labels" || low == "labels in first row" {
            o.labels = true;
        } else if p.get(..3).is_some_and(|w| w.eq_ignore_ascii_case("to ")) {
            o.to = Some(p[3..].trim().to_string());
        } else if let Some(s) = low.strip_prefix("seed ") {
            o.seed = Some(s.trim().parse().map_err(|_| format!("{p:?}: the seed is a whole number, such as seed 42."))?);
        } else {
            o.rest.push(low);
        }
    }
    Ok(o)
}

/// The value after `word` in the options, if one says it.
fn option_num(o: &Options, word: &str) -> Result<Option<f64>, String> {
    for r in &o.rest {
        if let Some(v) = r.strip_prefix(word) {
            let v = v.trim().trim_end_matches('%');
            let x: f64 = v.parse().map_err(|_| format!("{r:?}: type {word} and a number."))?;
            return Ok(Some(if r.ends_with('%') { x / 100.0 } else { x }));
        }
    }
    Ok(None)
}

fn unknown(o: &Options, known: &[&str]) -> Result<(), String> {
    match o.rest.iter().find(|r| !known.iter().any(|k| r.starts_with(k))) {
        Some(r) => Err(format!("{r:?} isn't an option here; it takes {}.", known.join(", "))),
        None => Ok(()),
    }
}

/// Runs the bar: `TOOL RANGE, options…, to CELL`.
pub fn run(d: &mut Doc, typed: &str) -> Result<String, String> {
    let t = typed.trim();
    let low = t.to_lowercase();
    let (tool, rest) = ["moving average", "descriptive statistics", "descriptive", "histogram", "rank and percentile", "rank", "exponential smoothing", "smoothing", "correlation", "covariance", "sampling", "random number generation", "random"]
        .iter()
        .find(|k| low.starts_with(**k) && low.get(k.len()..).is_some_and(|r| r.chars().next().is_none_or(char::is_whitespace)))
        .map(|k| (*k, t.get(k.len()..).unwrap_or("").trim()))
        .ok_or_else(|| format!("Start with a tool: {TOOLS}."))?;
    let parts: Vec<&str> = rest.split(',').collect();
    let table = if tool.starts_with("random") {
        random(&options(&parts[1..])?, parts[0])?
    } else {
        let area = parse_area(&d.wb, d.sheet, parts[0]).map_err(|e| if parts[0].trim().is_empty() { "Type the input range after the tool, such as descriptive A1:B20.".into() } else { e })?;
        let o = options(&parts[1..])?;
        let wb = &d.wb;
        match tool {
            "descriptive" | "descriptive statistics" => descriptive(wb, &area, &o)?,
            "histogram" => histogram(wb, d.sheet, &area, &o)?,
            "rank" | "rank and percentile" => rank(wb, &area, &o)?,
            "moving average" => moving_average(wb, &area, &o)?,
            "smoothing" | "exponential smoothing" => smoothing(wb, &area, &o)?,
            "correlation" => matrix(wb, &area, &o, true)?,
            "covariance" => matrix(wb, &area, &o, false)?,
            _ => sampling(wb, &area, &o)?,
        }
    };
    let o = options(&parts[1..])?;
    write(d, table, o.to.as_deref())
}

/// Formulas that name the output cell above them say `{UP}`; once the
/// table's place is known it becomes that cell.
fn placed(rows: Vec<Vec<Input>>, r0: u32, c0: u32) -> Vec<Vec<Input>> {
    rows.into_iter()
        .enumerate()
        .map(|(r, row)| {
            row.into_iter()
                .enumerate()
                .map(|(c, input)| match input {
                    Input::Formula(f) if f.contains("{UP}") && r > 0 => Input::Formula(f.replace("{UP}", &a1(r0 + r as u32 - 1, c0 + c as u32))),
                    other => other,
                })
                .collect()
        })
        .collect()
}

fn write(d: &mut Doc, t: Table, to: Option<&str>) -> Result<String, String> {
    let height = t.rows.len() as u32;
    let width = t.rows.iter().map(Vec::len).max().unwrap_or(0) as u32;
    if height == 0 || width == 0 {
        return Err("There is nothing to write.".into());
    }
    if u64::from(height) * u64::from(width) > MAX_OUT {
        return Err(format!("The result would be over {MAX_OUT} cells; analyse less at once."));
    }
    let style_ops = |sheet: u16, r0: u32, c0: u32| -> Vec<GridOp> {
        t.formats.iter().map(|&(a, b, c, e, f)| GridOp::style(Rect { sheet, r0: r0 + a, c0: c0 + b, r1: r0 + c, c1: c0 + e }, officina_grid::style::Style { num_fmt: f.into(), ..Default::default() })).collect()
    };
    match to {
        Some(cell) => {
            let at = parse_area(&d.wb, d.sheet, cell)?;
            if at.r0 + height > MAX_ROWS || at.c0 + width > MAX_COLS {
                return Err("The result doesn't fit there: it runs off the sheet.".into());
            }
            let out = Rect { sheet: at.sheet, r0: at.r0, c0: at.c0, r1: at.r0 + height - 1, c1: at.c0 + width - 1 };
            for r in out.r0..=out.r1 {
                for c in out.c0..=out.c1 {
                    if d.wb.input(CellAddr::new(out.sheet, r, c)) != Input::Empty {
                        return Err(format!("{} isn't empty: the result needs {} free. Pick another place, or leave out to for a new sheet.", a1(r, c), range_text(&out)));
                    }
                }
            }
            let mut ops = vec![GridOp::block(out.sheet, out.r0, out.c0, placed(t.rows, out.r0, out.c0))];
            ops.extend(style_ops(out.sheet, out.r0, out.c0));
            d.status.clear();
            if !d.run_action(t.name, ops) {
                return Err(std::mem::take(&mut d.status));
            }
            Ok(format!("{} at {}!{}.", t.say, d.wb.sheet_name(out.sheet), range_text(&out)))
        }
        None => {
            let name = if d.wb.sheet_id(t.name).is_none() { t.name.to_string() } else { crate::data::copy_name(&d.wb, t.name) };
            let count = d.wb.sheet_count() as u16;
            d.status.clear();
            if !d.run_action(t.name, vec![GridOp::add_sheet(name.clone())]) || d.wb.sheet_count() as u16 == count {
                return Err(if d.status.is_empty() { "The sheet couldn't be added.".into() } else { std::mem::take(&mut d.status) });
            }
            let mut ops = vec![GridOp::block(count, 0, 0, placed(t.rows, 0, 0))];
            ops.extend(style_ops(count, 0, 0));
            d.extend(ops);
            Ok(format!("{} on the new sheet “{name}”.", t.say))
        }
    }
}

/// Descriptive Statistics: per column, Excel's thirteen measures and,
/// with `confidence 95%`, the confidence level of the mean. Formulas, so
/// they follow the data.
fn descriptive(wb: &Workbook, area: &Rect, o: &Options) -> Result<Table, String> {
    unknown(o, &["confidence", "summary"])?;
    let confidence = option_num(o, "confidence")?;
    if let Some(c) = confidence {
        if !(c > 0.0 && c < 1.0) {
            return Err("The confidence is between 0% and 100%, such as confidence 95%.".into());
        }
    }
    let all = series(wb, area, o.labels)?;
    if all.len() as u32 > MAX_SERIES {
        return Err(format!("Describe at most {MAX_SERIES} columns at once."));
    }
    let measures: &[(&str, &str)] = &[
        ("Mean", "AVERAGE(@)"),
        ("Standard Error", "STDEV.S(@)/SQRT(COUNT(@))"),
        ("Median", "MEDIAN(@)"),
        ("Mode", "MODE.SNGL(@)"),
        ("Standard Deviation", "STDEV.S(@)"),
        ("Sample Variance", "VAR.S(@)"),
        ("Kurtosis", "KURT(@)"),
        ("Skewness", "SKEW(@)"),
        ("Range", "MAX(@)-MIN(@)"),
        ("Minimum", "MIN(@)"),
        ("Maximum", "MAX(@)"),
        ("Sum", "SUM(@)"),
        ("Count", "COUNT(@)"),
    ];
    let mut rows: Vec<Vec<Input>> = vec![Vec::new(); measures.len() + 2 + usize::from(confidence.is_some())];
    for s in &all {
        let r = abs_ref(wb, &s.area);
        rows[0].extend([text(&s.label), Input::Empty]);
        rows[1].extend([Input::Empty, Input::Empty]);
        for (i, (name, f)) in measures.iter().enumerate() {
            rows[i + 2].extend([text(name), Input::Formula(format!("={}", f.replace('@', &r)))]);
        }
        if let Some(c) = confidence {
            let pct = (c * 1000.0).round() / 10.0;
            rows[measures.len() + 2].extend([text(&format!("Confidence Level({pct:.1}%)")), Input::Formula(format!("=CONFIDENCE.T({},STDEV.S({r}),COUNT({r}))", ((1.0 - c) * 1e12).round() / 1e12))]);
        }
    }
    let n = all.len();
    Ok(Table { name: "Descriptive Statistics", rows, formats: Vec::new(), say: format!("Described {n} {}", if n == 1 { "column" } else { "columns" }) })
}

/// Histogram: counts of the data at or under each bin and over the one
/// before it, with More for the rest. Without `bins RANGE`, √n equal bins
/// from the smallest to the largest value. `cumulative` adds the running
/// percentage, `pareto` the bins again sorted by count.
fn histogram(wb: &Workbook, here: u16, area: &Rect, o: &Options) -> Result<Table, String> {
    let mut bins_at = None;
    let mut cumulative = false;
    let mut pareto = false;
    for r in &o.rest {
        if let Some(b) = r.strip_prefix("bins ") {
            bins_at = Some(parse_area(wb, here, b)?);
        } else if r == "cumulative" || r == "cumulative percentage" {
            cumulative = true;
        } else if r == "pareto" || r == "sorted" {
            pareto = true;
        } else {
            return Err(format!("{r:?} isn't an option here; it takes bins RANGE, cumulative, pareto, labels, to CELL."));
        }
    }
    let data_area = if o.labels { Rect { r0: area.r0 + 1, ..*area } } else { *area };
    if o.labels && area.r0 == area.r1 {
        return Err("With labels the range needs a row of data under them.".into());
    }
    if data_area.cells() > MAX_AREA_CELLS {
        return Err(format!("Analyse at most {MAX_AREA_CELLS} cells at once."));
    }
    let data = numbers(wb, &data_area);
    if data.is_empty() {
        return Err("The input range has no numbers.".into());
    }
    let mut edges = match bins_at {
        Some(b) => {
            if b.cells() > MAX_BINS as u64 {
                return Err(format!("Use at most {MAX_BINS} bins."));
            }
            let b = if o.labels && b.r1 > b.r0 && !matches!(wb.get(CellAddr::new(b.sheet, b.r0, b.c0)), Value::Num(_)) { Rect { r0: b.r0 + 1, ..b } } else { b };
            let e = numbers(wb, &b);
            if e.is_empty() {
                return Err("The bin range has no numbers.".into());
            }
            e
        }
        None => {
            let lo = data.iter().copied().fold(f64::INFINITY, f64::min);
            let hi = data.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            let k = ((data.len() as f64).sqrt().ceil() as usize).clamp(1, MAX_BINS);
            if hi == lo { vec![lo] } else { (1..=k).map(|i| lo + (hi - lo) * i as f64 / k as f64).collect() }
        }
    };
    edges.sort_by(f64::total_cmp);
    edges.dedup();
    let mut counts = vec![0u64; edges.len() + 1];
    for &x in &data {
        let i = edges.partition_point(|&e| e < x);
        counts[i] += 1;
    }
    let total = data.len() as f64;
    let mut rows = vec![{
        let mut h = vec![text("Bin"), text("Frequency")];
        if cumulative { h.push(text("Cumulative %")); }
        if pareto {
            h.extend([text("Bin"), text("Frequency")]);
            if cumulative { h.push(text("Cumulative %")); }
        }
        h
    }];
    let label = |i: usize| if i < edges.len() { num(edges[i]) } else { text("More") };
    let mut order: Vec<usize> = (0..counts.len()).collect();
    order.sort_by(|&a, &b| counts[b].cmp(&counts[a]).then(a.cmp(&b)));
    let (mut run, mut run_sorted) = (0u64, 0u64);
    for i in 0..counts.len() {
        run += counts[i];
        let mut row = vec![label(i), num(counts[i] as f64)];
        if cumulative { row.push(num(run as f64 / total)); }
        if pareto {
            let j = order[i];
            run_sorted += counts[j];
            row.extend([label(j), num(counts[j] as f64)]);
            if cumulative { row.push(num(run_sorted as f64 / total)); }
        }
        rows.push(row);
    }
    let last = counts.len() as u32;
    let mut formats = Vec::new();
    if cumulative {
        formats.push((1, 2, last, 2, "0.00%"));
        if pareto { formats.push((1, 5, last, 5, "0.00%")); }
    }
    Ok(Table { name: "Histogram", rows, formats, say: format!("Counted {} values in {} bins and More", data.len(), edges.len()) })
}

/// Rank and Percentile: per column, each value's place in the data (Point),
/// the value, its rank from the largest (ties share one) and its percent
/// rank as PERCENTRANK.INC gives it, largest first.
fn rank(wb: &Workbook, area: &Rect, o: &Options) -> Result<Table, String> {
    unknown(o, &[])?;
    let all = series(wb, area, o.labels)?;
    if all.len() as u32 > MAX_SERIES {
        return Err(format!("Rank at most {MAX_SERIES} columns at once."));
    }
    let mut rows: Vec<Vec<Input>> = vec![Vec::new()];
    let mut formats = Vec::new();
    for (k, s) in all.iter().enumerate() {
        let v = numbers(wb, &s.area);
        rows[0].extend([text("Point"), text(&s.label), text("Rank"), text("Percent")]);
        let mut sorted: Vec<(usize, f64)> = v.iter().copied().enumerate().collect();
        sorted.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        let n = v.len();
        for (i, &(point, x)) in sorted.iter().enumerate() {
            // Sorted largest first, so both counts are binary searches.
            let rank = 1 + sorted.partition_point(|p| p.1 > x);
            let below = n - sorted.partition_point(|p| p.1 >= x);
            let pct = if n <= 1 { 1.0 } else { ((below as f64 / (n - 1) as f64) * 1000.0 + 1e-9).floor() / 1000.0 };
            if rows.len() <= i + 1 { rows.push(Vec::new()); }
            let row = &mut rows[i + 1];
            row.resize(k * 4, Input::Empty);
            row.extend([num(point as f64 + 1.0), num(x), num(rank as f64), num(pct)]);
        }
        if n > 0 { formats.push((1, k as u32 * 4 + 3, n as u32, k as u32 * 4 + 3, "0.00%")); }
    }
    let n = all.len();
    Ok(Table { name: "Rank and Percentile", rows, formats, say: format!("Ranked {n} {}", if n == 1 { "column" } else { "columns" }) })
}

/// The one column (or row) a forecast tool reads, without its label.
fn one_series(wb: &Workbook, area: &Rect, o: &Options, tool: &str) -> Result<(Option<String>, Vec<Rect>), String> {
    let by_rows = area.r0 == area.r1 && area.c1 > area.c0;
    if !by_rows && area.c1 != area.c0 {
        return Err(format!("{tool} reads one column or one row."));
    }
    let len = if by_rows { area.c1 - area.c0 + 1 } else { area.r1 - area.r0 + 1 };
    if len > MAX_ROWS.min(1_000_000) {
        return Err("The input is too long.".into());
    }
    let skip = u32::from(o.labels);
    if len <= skip {
        return Err("With labels the range needs data after them.".into());
    }
    let label = o.labels.then(|| wb.formatted(CellAddr::new(area.sheet, area.r0, area.c0)).text);
    let cells = (skip..len).map(|i| if by_rows { Rect::cell(CellAddr::new(area.sheet, area.r0, area.c0 + i)) } else { Rect::cell(CellAddr::new(area.sheet, area.r0 + i, area.c0)) }).collect();
    Ok((label, cells))
}

/// Moving Average: `interval N` (3 when not said). Each output is the
/// AVERAGE of that point and the N−1 before it; the first N−1 are #N/A.
fn moving_average(wb: &Workbook, area: &Rect, o: &Options) -> Result<Table, String> {
    unknown(o, &["interval"])?;
    let n = option_num(o, "interval")?.unwrap_or(3.0);
    if n.fract() != 0.0 || !(2.0..=10_000.0).contains(&n) {
        return Err("The interval is a whole number from 2 to 10,000.".into());
    }
    let n = n as usize;
    let (label, cells) = one_series(wb, area, o, "Moving Average")?;
    let mut rows = vec![vec![text(&label.map_or("Moving Average".into(), |l| format!("Moving Average ({l})")))]];
    for i in 0..cells.len() {
        rows.push(vec![if i + 1 < n {
            na()
        } else {
            let span = Rect { sheet: cells[0].sheet, r0: cells[i + 1 - n].r0, c0: cells[i + 1 - n].c0, r1: cells[i].r0, c1: cells[i].c0 };
            Input::Formula(format!("=AVERAGE({})", abs_ref(wb, &span)))
        }]);
    }
    Ok(Table { name: "Moving Average", rows, formats: Vec::new(), say: format!("Averaged {} points over {n}", cells.len()) })
}

/// Exponential Smoothing: `damping D` (0.3 when not said). The first
/// output is #N/A, the second the first value, then (1−D)·value + D·the
/// smoothed one before, as formulas down the output column.
fn smoothing(wb: &Workbook, area: &Rect, o: &Options) -> Result<Table, String> {
    unknown(o, &["damping"])?;
    let damp = option_num(o, "damping")?.unwrap_or(0.3);
    if !(0.0..=1.0).contains(&damp) {
        return Err("The damping factor is from 0 to 1, such as damping 0.3.".into());
    }
    let (label, cells) = one_series(wb, area, o, "Exponential Smoothing")?;
    let alpha = ((1.0 - damp) * 1e12).round() / 1e12;
    let mut rows = vec![vec![text(&label.map_or("Smoothed".into(), |l| format!("Smoothed ({l})")))]];
    for i in 0..cells.len() {
        rows.push(vec![match i {
            0 => na(),
            1 => Input::Formula(format!("={}", abs_ref(wb, &cells[0]))),
            // The smoothed value just above, on the output's own sheet.
            _ => Input::Formula(format!("={alpha}*{}+{damp}*{{UP}}", abs_ref(wb, &cells[i - 1]))),
        }]);
    }
    Ok(Table { name: "Exponential Smoothing", rows, formats: Vec::new(), say: format!("Smoothed {} points with damping {damp}", cells.len()) })
}

/// Correlation (CORREL) or Covariance (COVARIANCE.P, as the ToolPak's):
/// the lower triangle of the matrix of every pair of columns.
fn matrix(wb: &Workbook, area: &Rect, o: &Options, correl: bool) -> Result<Table, String> {
    unknown(o, &[])?;
    let all = series(wb, area, o.labels)?;
    if all.len() < 2 && correl {
        return Err("Correlation compares two or more columns.".into());
    }
    if all.len() as u32 > MAX_SERIES {
        return Err(format!("Compare at most {MAX_SERIES} columns at once."));
    }
    let f = if correl { "CORREL" } else { "COVARIANCE.P" };
    let mut rows = vec![std::iter::once(Input::Empty).chain(all.iter().map(|s| text(&s.label))).collect::<Vec<_>>()];
    for (i, a) in all.iter().enumerate() {
        let mut row = vec![text(&a.label)];
        for b in all.iter().take(i + 1) {
            row.push(if correl && std::ptr::eq(a, b) { num(1.0) } else { Input::Formula(format!("={f}({},{})", abs_ref(wb, &a.area), abs_ref(wb, &b.area))) });
        }
        rows.push(row);
    }
    let n = all.len();
    Ok(Table { name: if correl { "Correlation" } else { "Covariance" }, rows, formats: Vec::new(), say: format!("Compared {n} columns") })
}

/// A small, seeded generator (SplitMix64), so a seed gives the same draw.
struct Rng(u64);

impl Rng {
    fn new(seed: Option<u64>) -> Rng {
        Rng(seed.unwrap_or_else(|| std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0x9e37_79b9, |d| d.as_nanos() as u64)))
    }
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    /// Uniform in [0, 1).
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn below(&mut self, n: usize) -> usize {
        (self.unit() * n as f64) as usize % n.max(1)
    }
    fn normal(&mut self) -> f64 {
        let u = 1.0 - self.unit();
        let v = self.unit();
        (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * v).cos()
    }
}

/// Sampling: `periodic N` takes every Nth value; `random N` draws N with
/// replacement (the ToolPak's way). One column out.
fn sampling(wb: &Workbook, area: &Rect, o: &Options) -> Result<Table, String> {
    if area.cells() > MAX_AREA_CELLS {
        return Err(format!("Analyse at most {MAX_AREA_CELLS} cells at once."));
    }
    let data_area = if o.labels && area.r1 > area.r0 { Rect { r0: area.r0 + 1, ..*area } } else { *area };
    let mut v = Vec::new();
    for c in data_area.c0..=data_area.c1 {
        v.extend(numbers(wb, &Rect { c0: c, c1: c, ..data_area }));
    }
    if v.is_empty() {
        return Err("The input range has no numbers.".into());
    }
    let how = "Say periodic N (every Nth value) or random N (N values drawn).";
    let mut out = None;
    for r in &o.rest {
        let (kind, n) = r.split_once(' ').ok_or(how)?;
        let n: usize = n.trim().parse().map_err(|_| how.to_string())?;
        if n == 0 || n > MAX_OUT as usize {
            return Err(format!("Sample 1 to {MAX_OUT}."));
        }
        out = Some(match kind {
            "periodic" => {
                if n > v.len() {
                    return Err(format!("The period is at most the {} values there are.", v.len()));
                }
                v.iter().skip(n - 1).step_by(n).copied().collect::<Vec<_>>()
            }
            "random" => {
                let mut rng = Rng::new(o.seed);
                (0..n).map(|_| v[rng.below(v.len())]).collect()
            }
            _ => return Err(how.into()),
        });
    }
    let out = out.ok_or(how)?;
    let k = out.len();
    let rows = std::iter::once(vec![text("Sample")]).chain(out.into_iter().map(|x| vec![num(x)])).collect();
    Ok(Table { name: "Sampling", rows, formats: Vec::new(), say: format!("Sampled {k} {}", if k == 1 { "value" } else { "values" }) })
}

/// Random Number Generation: `random ROWSxCOLUMNS, DISTRIBUTION…, seed N`
/// with uniform A B, normal MEAN SD, bernoulli P or poisson LAMBDA.
fn random(o: &Options, size: &str) -> Result<Table, String> {
    let size = size.trim().to_lowercase();
    let (rows_n, cols_n) = if size.is_empty() {
        (10, 1)
    } else {
        let (r, c) = size.split_once('x').unwrap_or((&size, "1"));
        let parse = |s: &str| s.trim().parse::<u32>().ok().filter(|&n| n >= 1);
        (parse(r).ok_or("Say how many: random 10x2 for 10 rows of 2 columns.")?, parse(c).ok_or("Say how many: random 10x2 for 10 rows of 2 columns.")?)
    };
    if u64::from(rows_n) * u64::from(cols_n) > MAX_OUT || rows_n >= MAX_ROWS || cols_n > MAX_COLS {
        return Err(format!("Generate at most {MAX_OUT} numbers at once."));
    }
    let mut dist = ("uniform", 0.0, 1.0);
    for r in &o.rest {
        let w: Vec<&str> = r.split_whitespace().collect();
        let p = |i: usize| -> Result<f64, String> { w.get(i).and_then(|x| x.parse().ok()).ok_or(format!("{r:?}: uniform FROM TO, normal MEAN SD, bernoulli P or poisson LAMBDA.")) };
        dist = match w.first().copied() {
            Some("uniform") => ("uniform", p(1)?, p(2)?),
            Some("normal") => ("normal", p(1)?, p(2)?),
            Some("bernoulli") => ("bernoulli", p(1)?, 0.0),
            Some("poisson") => ("poisson", p(1)?, 0.0),
            _ => return Err(format!("{r:?}: uniform FROM TO, normal MEAN SD, bernoulli P or poisson LAMBDA.")),
        };
    }
    match dist {
        ("uniform", a, b) if !(a.is_finite() && b.is_finite() && a <= b) => return Err("Uniform needs FROM no larger than TO.".into()),
        ("normal", _, sd) if !(sd.is_finite() && sd >= 0.0) => return Err("The standard deviation is 0 or more.".into()),
        ("bernoulli", p, _) if !(0.0..=1.0).contains(&p) => return Err("Bernoulli's P is from 0 to 1.".into()),
        ("poisson", l, _) if !(l > 0.0 && l <= 1000.0) => return Err("Poisson's lambda is over 0 and at most 1,000.".into()),
        _ => {}
    }
    let mut rng = Rng::new(o.seed);
    let mut draw = || match dist {
        ("uniform", a, b) => a + (b - a) * rng.unit(),
        ("normal", m, sd) => m + sd * rng.normal(),
        ("bernoulli", p, _) => f64::from(u8::from(rng.unit() < p)),
        (_, l, _) => {
            // Knuth's method; lambda is bounded above, so it ends.
            let limit = (-l).exp();
            let (mut k, mut prod) = (0.0, rng.unit());
            while prod > limit {
                k += 1.0;
                prod *= rng.unit();
            }
            k
        }
    };
    let rows = (0..rows_n).map(|_| (0..cols_n).map(|_| num(draw())).collect()).collect();
    Ok(Table { name: "Random Numbers", rows, formats: Vec::new(), say: format!("Generated {} {} numbers", u64::from(rows_n) * u64::from(cols_n), dist.0) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> Doc {
        Doc::from_csv("a.csv", b"Sales,Cost\n2,1\n4,3\n4,2\n7,8\n9,6\n").unwrap().0
    }

    fn v(d: &Doc, s: u16, r: u32, c: u32) -> Value {
        d.wb.get(CellAddr::new(s, r, c))
    }

    #[test]
    fn descriptive_statistics_on_a_new_sheet_follow_the_data() {
        let mut d = doc();
        assert_eq!(start(&d, CellRange { r0: 0, c0: 0, r1: 5, c1: 1 }), "descriptive A1:B6, labels");
        let say = run(&mut d, "descriptive A1:B6, labels, confidence 95%").unwrap();
        assert_eq!(say, "Described 2 columns on the new sheet “Descriptive Statistics”.");
        d.wb.recalc();
        assert_eq!(d.wb.sheet_count(), 2);
        assert_eq!(v(&d, 1, 0, 0), Value::str("Sales"));
        assert_eq!(v(&d, 1, 0, 2), Value::str("Cost"));
        assert_eq!(v(&d, 1, 2, 0), Value::str("Mean"));
        assert_eq!(v(&d, 1, 2, 1), Value::Num(5.2));
        assert_eq!(v(&d, 1, 4, 1), Value::Num(4.0), "median");
        assert_eq!(v(&d, 1, 5, 1), Value::Num(4.0), "mode");
        assert_eq!(v(&d, 1, 14, 1), Value::Num(5.0), "count");
        assert_eq!(v(&d, 1, 15, 0), Value::str("Confidence Level(95.0%)"));
        assert!(matches!(v(&d, 1, 15, 1), Value::Num(x) if (x - 3.4455).abs() < 1e-3));
        // Live: a changed input changes the mean.
        d.type_into(1, 0, "7");
        d.wb.recalc();
        assert_eq!(v(&d, 1, 2, 1), Value::Num(6.2));
        // One undo step takes the sheet away (and the edit before it).
        assert!(d.undo());
        assert!(d.undo());
        assert_eq!(d.wb.sheet_count(), 1);
    }

    #[test]
    fn histogram_rank_and_matrices_in_place() {
        let mut d = doc();
        d.type_into(0, 3, "Bins");
        d.type_into(1, 3, "3");
        d.type_into(2, 3, "6");
        let say = run(&mut d, "histogram A1:A6, labels, bins D1:D3, cumulative, pareto, to F1").unwrap();
        assert_eq!(say, "Counted 5 values in 2 bins and More at Sheet1!F1:K4.");
        // 2 | 4,4 | 7,9: at or under 3, under 6, More.
        let col = |c| (1..4).map(|r| v(&d, 0, r, c)).collect::<Vec<_>>();
        assert_eq!(col(5), vec![Value::Num(3.0), Value::Num(6.0), Value::str("More")]);
        assert_eq!(col(6), vec![Value::Num(1.0), Value::Num(2.0), Value::Num(2.0)]);
        assert_eq!(col(7), vec![Value::Num(0.2), Value::Num(0.6), Value::Num(1.0)]);
        assert_eq!(col(8), vec![Value::Num(6.0), Value::str("More"), Value::Num(3.0)], "pareto: by count, ties in bin order");
        assert_eq!(d.wb.style(CellAddr::new(0, 1, 7)).num_fmt.as_ref(), "0.00%");
        // It won't write over cells.
        assert!(run(&mut d, "rank A1:A6, labels, to F1").unwrap_err().contains("isn't empty"));
        run(&mut d, "rank A1:A6, labels, to M1").unwrap();
        // Largest first; the two 4s share rank 3; percent as PERCENTRANK.INC.
        let row = |r| (12..16).map(|c| v(&d, 0, r, c)).collect::<Vec<_>>();
        assert_eq!(row(0), vec![Value::str("Point"), Value::str("Sales"), Value::str("Rank"), Value::str("Percent")]);
        assert_eq!(row(1), vec![Value::Num(5.0), Value::Num(9.0), Value::Num(1.0), Value::Num(1.0)]);
        assert_eq!(row(3), vec![Value::Num(2.0), Value::Num(4.0), Value::Num(3.0), Value::Num(0.25)]);
        assert_eq!(row(4), vec![Value::Num(3.0), Value::Num(4.0), Value::Num(3.0), Value::Num(0.25)]);
        run(&mut d, "correlation A1:B6, labels, to Q1").unwrap();
        run(&mut d, "covariance A1:B6, labels, to Q5").unwrap();
        d.wb.recalc();
        assert_eq!(v(&d, 0, 1, 17), Value::Num(1.0));
        assert!(matches!(v(&d, 0, 2, 17), Value::Num(x) if (x - 0.86525).abs() < 1e-4), "{:?}", v(&d, 0, 2, 17));
        assert_eq!(v(&d, 0, 1, 18), Value::Empty, "lower triangle only");
        assert!(matches!(v(&d, 0, 5, 17), Value::Num(x) if (x - 6.16).abs() < 1e-9), "{:?}", v(&d, 0, 5, 17));
    }

    #[test]
    fn moving_average_and_smoothing_write_formulas() {
        let mut d = doc();
        run(&mut d, "moving average A1:A6, labels, interval 2, to D1").unwrap();
        run(&mut d, "smoothing A2:A6, damping 0.5, to E1").unwrap();
        d.wb.recalc();
        assert_eq!(v(&d, 0, 0, 3), Value::str("Moving Average (Sales)"));
        assert_eq!(v(&d, 0, 1, 3), Value::Err(ErrorKind::NA));
        assert_eq!(v(&d, 0, 2, 3), Value::Num(3.0));
        assert_eq!(v(&d, 0, 5, 3), Value::Num(8.0));
        // 2, 4, 4, 7, 9 smoothed at 0.5: #N/A, 2, 3, 3.5, 5.25.
        let col: Vec<Value> = (1..6).map(|r| v(&d, 0, r, 4)).collect();
        assert_eq!(col, vec![Value::Err(ErrorKind::NA), Value::Num(2.0), Value::Num(3.0), Value::Num(3.5), Value::Num(5.25)]);
        assert!(run(&mut d, "moving average A1:B6, to H1").unwrap_err().contains("one column"));
        assert!(run(&mut d, "smoothing A2:A6, damping 2").is_err());
    }

    #[test]
    fn sampling_and_random_numbers_repeat_with_a_seed() {
        let mut d = doc();
        run(&mut d, "sampling A1:A6, labels, periodic 2, to D1").unwrap();
        assert_eq!((v(&d, 0, 1, 3), v(&d, 0, 2, 3), v(&d, 0, 3, 3)), (Value::Num(4.0), Value::Num(7.0), Value::Empty));
        run(&mut d, "sampling A2:A6, random 50, seed 7, to E1").unwrap();
        let drawn: Vec<Value> = (1..51).map(|r| v(&d, 0, r, 4)).collect();
        assert!(drawn.iter().all(|x| matches!(x, Value::Num(n) if [2.0, 4.0, 7.0, 9.0].contains(n))));
        run(&mut d, "random 200x2, normal 10 2, seed 3, to G1").unwrap();
        let a: Vec<Value> = (0..200).map(|r| v(&d, 0, r, 6)).collect();
        let mean = a.iter().map(|x| if let Value::Num(n) = x { *n } else { f64::NAN }).sum::<f64>() / 200.0;
        assert!((mean - 10.0).abs() < 0.5, "{mean}");
        run(&mut d, "random 200x2, normal 10 2, seed 3, to J1").unwrap();
        assert_eq!(v(&d, 0, 17, 9), v(&d, 0, 17, 6), "the same seed draws the same numbers");
        run(&mut d, "random 100, bernoulli 0.5, seed 1, to M1").unwrap();
        assert!((0..100).all(|r| matches!(v(&d, 0, r, 12), Value::Num(n) if n == 0.0 || n == 1.0)));
        assert!(run(&mut d, "random 0x2").is_err());
        assert!(run(&mut d, "random 10, uniform 5 1").is_err());
        assert!(run(&mut d, "anova A1:B6").unwrap_err().starts_with("Start with a tool"));
    }
}
