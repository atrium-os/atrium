//! Fill ▸ Series… and Fill ▸ Justify (docs/spec/officina.md §16.142).
//!
//! Series… fills the selection from the first cell of each column (or row)
//! with a typed rule: `linear 2`, `growth 3`, `date month 1`, `date weekday`,
//! `trend`, with `stop 100` and `rows` or `columns` where wanted. Justify
//! joins the text down the selection's first column and breaks it again to
//! the selection's width.

use officina_grid::addr::{a1, CellAddr};
use officina_grid::book::Input;
use officina_grid::ops::GridOp;
use officina_grid::Value;
use pergola::widgets::CellRange;

use crate::doc::Doc;

/// The most cells Series… fills.
pub const MAX_SERIES_CELLS: u64 = 1_000_000;
/// About how wide one character of the default font is, for Justify.
pub const CHAR_PX: f64 = 7.0;
/// The most rows Justify reads and writes.
pub const MAX_JUSTIFY_ROWS: u32 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Unit {
    Day,
    Weekday,
    Month,
    Year,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Linear,
    Growth,
    Date(Unit),
    /// A straight line (Linear) or curve (Growth) fitted to the values
    /// already there, continued.
    Trend { growth: bool },
}

#[derive(Debug, Clone, PartialEq)]
struct Rule {
    kind: Kind,
    step: f64,
    stop: Option<f64>,
    /// Down each column (true) or across each row.
    down: Option<bool>,
}

fn parse(typed: &str) -> Result<Rule, String> {
    let mut rule = Rule { kind: Kind::Linear, step: 1.0, stop: None, down: None };
    let words: Vec<String> = typed.split([' ', ',']).filter(|w| !w.is_empty()).map(str::to_lowercase).collect();
    let mut i = 0;
    let mut stepped = false;
    let mut trend = false;
    let num = |w: Option<&String>| -> Result<f64, String> {
        let w = w.ok_or("A number is missing.")?;
        w.parse::<f64>().ok().filter(|x| x.is_finite()).ok_or_else(|| format!("{w:?} isn't a number."))
    };
    while i < words.len() {
        let w = words[i].as_str();
        match w {
            "linear" => rule.kind = Kind::Linear,
            "growth" => rule.kind = Kind::Growth,
            "trend" => trend = true,
            "date" | "dates" => rule.kind = Kind::Date(Unit::Day),
            "day" | "days" => rule.kind = Kind::Date(Unit::Day),
            "weekday" | "weekdays" => rule.kind = Kind::Date(Unit::Weekday),
            "month" | "months" => rule.kind = Kind::Date(Unit::Month),
            "year" | "years" => rule.kind = Kind::Date(Unit::Year),
            "rows" | "across" => rule.down = Some(false),
            "columns" | "down" => rule.down = Some(true),
            "step" => {
                i += 1;
                rule.step = num(words.get(i))?;
                stepped = true;
            }
            "stop" => {
                i += 1;
                rule.stop = Some(num(words.get(i))?);
            }
            _ => match w.parse::<f64>() {
                Ok(x) if x.is_finite() && !stepped => {
                    rule.step = x;
                    stepped = true;
                }
                _ => return Err(format!("{w:?}: type linear, growth, date (day, weekday, month, year) or trend, a step, and stop with a number.")),
            },
        }
        i += 1;
    }
    if trend {
        rule.kind = match rule.kind {
            Kind::Linear => Kind::Trend { growth: false },
            Kind::Growth => Kind::Trend { growth: true },
            _ => return Err("A trend is linear or growth, not of dates.".into()),
        };
    }
    if matches!(rule.kind, Kind::Date(_)) && rule.step.fract() != 0.0 {
        return Err("A date step is a whole number of days, months or years.".into());
    }
    Ok(rule)
}

/// The `k`th value after `start` under `rule` (k from 1), `None` past the
/// date range.
fn next(d: &Doc, rule: &Rule, start: f64, k: u32) -> Option<f64> {
    let sys = d.wb.date_system();
    let k = f64::from(k);
    match rule.kind {
        Kind::Linear => Some(start + rule.step * k),
        Kind::Growth => Some(start * rule.step.powf(k)),
        Kind::Date(Unit::Day) => Some(start + rule.step * k),
        Kind::Date(Unit::Weekday) => {
            // Each step moves `step` working days, Monday to Friday.
            let mut day = start.floor() as i64;
            let mut left = (rule.step * k).abs() as i64;
            let dir = if rule.step < 0.0 { -1 } else { 1 };
            while left > 0 {
                day += dir;
                if !matches!(sys.weekday(day), 0 | 6) {
                    left -= 1;
                }
            }
            Some(day as f64)
        }
        Kind::Date(u) => {
            let (y, m, dd) = sys.ymd(start)?;
            let months = rule.step as i64 * k as i64 * if u == Unit::Year { 12 } else { 1 };
            let total = y * 12 + (m - 1) + months;
            let (ty, tm) = (total.div_euclid(12), total.rem_euclid(12) + 1);
            // The same day of the month, or the month's last when it is shorter.
            let last = sys.date(ty as f64, tm as f64 + 1.0, 0.0).and_then(|s| sys.ymd(s)).map(|x| x.2)?;
            sys.date(ty as f64, tm as f64, dd.min(last) as f64).map(|s| s + start.fract())
        }
        Kind::Trend { .. } => None,
    }
}

/// Least squares over (1..=n, ys): the value at x for a line, or for an
/// exponential curve fitted to the logarithms.
fn fit(ys: &[f64], growth: bool) -> Option<impl Fn(f64) -> f64> {
    let pts: Vec<(f64, f64)> = ys.iter().enumerate().map(|(i, &y)| (i as f64 + 1.0, if growth { y.ln() } else { y })).collect();
    if pts.iter().any(|p| !p.1.is_finite()) || pts.is_empty() {
        return None;
    }
    let n = pts.len() as f64;
    let (sx, sy) = pts.iter().fold((0.0, 0.0), |a, p| (a.0 + p.0, a.1 + p.1));
    let (mx, my) = (sx / n, sy / n);
    let sxx: f64 = pts.iter().map(|p| (p.0 - mx).powi(2)).sum();
    let sxy: f64 = pts.iter().map(|p| (p.0 - mx) * (p.1 - my)).sum();
    let b = if sxx == 0.0 { 0.0 } else { sxy / sxx };
    let a = my - b * mx;
    Some(move |x: f64| if growth { (a + b * x).exp() } else { a + b * x })
}

/// Series…: each column (or row) of the selection filled from its first cell.
pub fn series(d: &mut Doc, sel: CellRange, typed: &str) -> Result<String, String> {
    let rule = parse(typed)?;
    let s = d.sheet;
    let (nr, nc) = (sel.r1 - sel.r0 + 1, sel.c1 - sel.c0 + 1);
    if u64::from(nr) * u64::from(nc) > MAX_SERIES_CELLS {
        return Err(format!("Series fills at most {MAX_SERIES_CELLS} cells."));
    }
    let down = rule.down.unwrap_or(nr >= nc);
    let (lines, len) = if down { (nc, nr) } else { (nr, nc) };
    if len < 2 {
        return Err("Select the first cell and the cells to fill after it.".into());
    }
    let at = |line: u32, k: u32| if down { (sel.r0 + k, sel.c0 + line) } else { (sel.r0 + line, sel.c0 + k) };
    let mut ops = Vec::new();
    let mut filled = 0u64;
    for line in 0..lines {
        let (r, c) = at(line, 0);
        let Value::Num(start) = d.wb.get(CellAddr::new(s, r, c)) else { continue };
        let mut vals: Vec<Input> = Vec::with_capacity(len as usize);
        let mut first = 1;
        if let Kind::Trend { growth } = rule.kind {
            // The numbers at the start of the line are the data the trend fits.
            let known: Vec<f64> = (0..len).map_while(|k| {
                let (r, c) = at(line, k);
                match d.wb.get(CellAddr::new(s, r, c)) { Value::Num(x) => Some(x), _ => None }
            }).collect();
            let f = fit(&known, growth).ok_or("Growth trends need positive numbers.")?;
            first = 0;
            for k in 0..len {
                vals.push(Input::Value(Value::Num(f(f64::from(k) + 1.0))));
            }
        } else {
            for k in 1..len {
                let Some(v) = next(d, &rule, start, k).filter(|v| v.is_finite()) else { break };
                if rule.stop.is_some_and(|stop| if rule.step >= 0.0 || matches!(rule.kind, Kind::Growth) && rule.step >= 1.0 { v > stop } else { v < stop }) {
                    break;
                }
                vals.push(Input::Value(Value::Num(v)));
            }
        }
        filled += vals.len() as u64;
        let (r, c) = at(line, first);
        let n = vals.len();
        if n == 0 {
            continue;
        }
        ops.push(if down { GridOp::block(s, r, c, vals.into_iter().map(|v| vec![v]).collect()) } else { GridOp::block(s, r, c, vec![vals]) });
    }
    if ops.is_empty() {
        return Err(format!("Put a number (or date) in {} to start the series from.", a1(sel.r0, sel.c0)));
    }
    d.run("Series", ops);
    if crate::sheet_protection::refused(&d.status) {
        return Err(d.status.clone());
    }
    Ok(format!("{filled} {} filled {}.", if filled == 1 { "cell" } else { "cells" }, if down { "down" } else { "across" }))
}

/// Fill ▸ Justify: the text in the selection's first column joined and
/// broken into lines that fit the selection's width, written down that
/// column from its top; the rows left over are emptied.
pub fn justify(d: &mut Doc, sel: CellRange) -> Result<String, String> {
    let s = d.sheet;
    let rows = sel.r1 - sel.r0 + 1;
    if rows > MAX_JUSTIFY_ROWS {
        return Err(format!("Justify works on at most {MAX_JUSTIFY_ROWS} rows."));
    }
    let mut words: Vec<String> = Vec::new();
    for r in sel.r0..=sel.r1 {
        let a = CellAddr::new(s, r, sel.c0);
        if d.wb.formula_text(a).is_some() {
            return Err(format!("{} holds a formula; Justify works on text.", a1(r, sel.c0)));
        }
        match d.wb.get(a) {
            Value::Empty => {}
            Value::Str(t) => words.extend(t.split_whitespace().map(str::to_string)),
            _ => return Err(format!("{} holds a number; Justify works on text.", a1(r, sel.c0))),
        }
    }
    if words.is_empty() {
        return Err("There is no text in the selection's first column.".into());
    }
    let px: f64 = (sel.c0..=sel.c1).map(|c| d.wb.sheet(s).col_sizes.size(c)).sum();
    let width = ((px / CHAR_PX).floor() as usize).max(1);
    let mut lines: Vec<String> = Vec::new();
    for w in words {
        match lines.last_mut() {
            Some(l) if l.chars().count() + 1 + w.chars().count() <= width => {
                l.push(' ');
                l.push_str(&w);
            }
            _ => lines.push(w),
        }
    }
    if lines.len() as u32 > rows {
        return Err(format!("The text needs {} rows at this width; select that many, or wider columns.", lines.len()));
    }
    let n = lines.len();
    let mut col: Vec<Vec<Input>> = lines.into_iter().map(|l| vec![Input::Value(Value::str(&l))]).collect();
    col.resize_with(rows as usize, || vec![Input::Empty]);
    d.run("Justify", vec![GridOp::block(s, sel.r0, sel.c0, col)]);
    if crate::sheet_protection::refused(&d.status) {
        return Err(d.status.clone());
    }
    Ok(format!("Text justified into {n} {}.", if n == 1 { "line" } else { "lines" }))
}

/// The most cells Fill Across Worksheets copies to each sheet.
pub const MAX_ACROSS_CELLS: u64 = 1_000_000;

/// Fill ▸ Across Worksheets: the selection copied to the same place on
/// other sheets: `all` (or empty) for every other sheet, or sheet names
/// separated by commas, then `contents` or `formats` for only those
/// (both by default), as one undo step.
pub fn fill_across(d: &mut Doc, sel: CellRange, typed: &str) -> Result<String, String> {
    use officina_grid::clip::{Clip, What};
    let s = d.sheet;
    let rect = officina_grid::addr::Rect { sheet: s, r0: sel.r0, c0: sel.c0, r1: sel.r1, c1: sel.c1 };
    if rect.cells() > MAX_ACROSS_CELLS {
        return Err(format!("Fill Across Worksheets copies at most {MAX_ACROSS_CELLS} cells."));
    }
    let mut what = What::All;
    let mut names: Vec<String> = Vec::new();
    for part in typed.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let mut part = part.to_string();
        for (word, w) in [("contents", What::Formulas), ("formats", What::Formats)] {
            let cut = part.len().checked_sub(word.len()).filter(|&at| part.is_char_boundary(at));
            if let Some(at) = cut.filter(|&at| part[at..].eq_ignore_ascii_case(word) && (at == 0 || part[..at].ends_with(' '))) {
                what = w;
                part = part[..at].trim().to_string();
            }
        }
        if !part.is_empty() {
            names.push(part);
        }
    }
    let all = names.is_empty() || (names.len() == 1 && names[0].eq_ignore_ascii_case("all"));
    let targets: Vec<u16> = if all {
        (0..d.wb.sheet_count() as u16).filter(|&t| t != s).collect()
    } else {
        let mut v = Vec::new();
        for n in &names {
            let t = d.wb.sheet_id(n.trim_matches('\'')).ok_or_else(|| format!("There is no sheet called {n:?}."))?;
            if t == s {
                return Err(format!("{n:?} is the sheet being copied from."));
            }
            if !v.contains(&t) {
                v.push(t);
            }
        }
        v
    };
    if targets.is_empty() {
        return Err("There is no other sheet to fill.".into());
    }
    let clip = Clip::take(&d.wb, rect, false);
    let mut ops = Vec::new();
    for &t in &targets {
        let (o, _) = clip.paste_as(&d.wb, t, sel.r0, sel.c0, what, false)?;
        ops.extend(o);
    }
    d.run("Fill Across Worksheets", ops);
    if crate::sheet_protection::refused(&d.status) {
        return Err(d.status.clone());
    }
    let n = targets.len();
    Ok(format!("{} copied to {n} other {}{}.", if rect.cells() == 1 { a1(sel.r0, sel.c0) } else { format!("{}:{}", a1(sel.r0, sel.c0), a1(sel.r1, sel.c1)) },
        if n == 1 { "sheet" } else { "sheets" },
        match what { What::Formulas => ", contents only", What::Formats => ", formats only", _ => "" }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nums(d: &Doc, cells: &[(u32, u32)]) -> Vec<f64> {
        cells.iter().map(|&(r, c)| match d.wb.get(CellAddr::new(0, r, c)) { Value::Num(x) => x, v => panic!("{} is {v:?}", a1(r, c)) }).collect()
    }

    fn col(r0: u32, r1: u32, c: u32) -> CellRange {
        CellRange { r0, c0: c, r1, c1: c }
    }

    #[test]
    fn series_fills_linear_growth_and_stops() {
        let mut d = Doc::from_csv("s.csv", b"1,2,5\n").unwrap().0;
        d.wb.recalc();
        series(&mut d, col(0, 4, 0), "linear 2").unwrap();
        assert_eq!(nums(&d, &[(0, 0), (1, 0), (2, 0), (3, 0), (4, 0)]), [1.0, 3.0, 5.0, 7.0, 9.0]);
        let m = series(&mut d, col(0, 9, 1), "growth 3 stop 100").unwrap();
        assert_eq!(m, "3 cells filled down.");
        assert_eq!(nums(&d, &[(0, 1), (1, 1), (2, 1), (3, 1)]), [2.0, 6.0, 18.0, 54.0]);
        assert_eq!(d.wb.get(CellAddr::new(0, 4, 1)), Value::Empty, "162 is past the stop");
        // Across a row, a negative step.
        series(&mut d, CellRange { r0: 0, c0: 2, r1: 0, c1: 5 }, "-1.5").unwrap();
        assert_eq!(nums(&d, &[(0, 3), (0, 4), (0, 5)]), [3.5, 2.0, 0.5]);
        assert!(d.undo());
        assert_eq!(d.wb.get(CellAddr::new(0, 0, 3)), Value::Empty);
        assert!(series(&mut d, col(0, 3, 0), "sideways").unwrap_err().contains("type linear"));
        assert!(series(&mut d, col(0, 0, 0), "linear").unwrap_err().contains("Select the first cell"));
        assert!(series(&mut d, col(5, 8, 0), "linear").unwrap_err().contains("to start the series"));
        assert!(series(&mut d, col(0, 3, 0), "date month 1.5").unwrap_err().contains("whole number"));
    }

    #[test]
    fn series_of_dates_by_month_weekday_and_year_and_a_trend() {
        // 2024-01-31 (45322) by month: Feb 29, Mar 31, Apr 30.
        let mut d = Doc::from_csv("d.csv", b"45322,45324,1\n,,3\n,,4\n").unwrap().0;
        d.wb.recalc();
        series(&mut d, col(0, 3, 0), "date month").unwrap();
        assert_eq!(nums(&d, &[(1, 0), (2, 0), (3, 0)]), [45351.0, 45382.0, 45412.0]);
        // Friday 2024-02-02 by weekday: Mon 5th, Tue 6th.
        series(&mut d, col(0, 2, 1), "date weekday").unwrap();
        assert_eq!(nums(&d, &[(1, 1), (2, 1)]), [45327.0, 45328.0]);
        series(&mut d, col(0, 1, 0), "date year 4").unwrap();
        assert_eq!(nums(&d, &[(1, 0)]), [46783.0], "2028-01-31");
        // Trend through 1, 3, 4: the fitted line 1.5x - 0.333…, continued.
        series(&mut d, col(0, 4, 2), "linear trend").unwrap();
        let got = nums(&d, &[(0, 2), (1, 2), (2, 2), (3, 2), (4, 2)]);
        for (g, w) in got.iter().zip([7.0 / 6.0, 8.0 / 3.0, 25.0 / 6.0, 17.0 / 3.0, 43.0 / 6.0]) {
            assert!((g - w).abs() < 1e-9, "{got:?}");
        }
    }

    #[test]
    fn justify_rebreaks_text_to_the_selection_width() {
        let mut d = Doc::from_csv("j.csv", b"the quick brown\nfox jumps over\nthe lazy dog\n,\n,\n").unwrap().0;
        d.wb.recalc();
        // Default columns are 64 px: nine characters a line.
        let m = justify(&mut d, CellRange { r0: 0, c0: 0, r1: 5, c1: 0 }).unwrap();
        assert_eq!(m, "Text justified into 5 lines.");
        let line = |d: &Doc, r| d.wb.formatted(CellAddr::new(0, r, 0)).text;
        assert_eq!((0..6).map(|r| line(&d, r)).collect::<Vec<_>>(), ["the quick", "brown fox", "jumps", "over the", "lazy dog", ""]);
        // Two columns wide: eighteen characters.
        justify(&mut d, CellRange { r0: 0, c0: 0, r1: 5, c1: 1 }).unwrap();
        assert_eq!((0..4).map(|r| line(&d, r)).collect::<Vec<_>>(), ["the quick brown", "fox jumps over the", "lazy dog", ""]);
        assert!(justify(&mut d, CellRange { r0: 0, c0: 0, r1: 1, c1: 0 }).unwrap_err().contains("needs"));
        d.type_into(5, 0, "7");
        d.wb.recalc();
        assert!(justify(&mut d, CellRange { r0: 0, c0: 0, r1: 5, c1: 0 }).unwrap_err().contains("holds a number"));
    }
}

#[cfg(test)]
mod across_tests {
    use crate::doc::Doc;
    use officina_grid::addr::{CellAddr, Rect};
    use officina_grid::style::Style;
    use pergola::widgets::CellRange;

    #[test]
    fn fill_across_worksheets_copies_contents_and_formats_to_other_sheets() {
        let mut d = Doc::from_csv("f.csv", b"Total,5\n").unwrap().0;
        d.type_into(0, 2, "=B1*2");
        d.wb.set_style(&Rect { sheet: 0, r0: 0, c0: 0, r1: 0, c1: 0 }, Style { fill: Some(0xFFC7CE), ..Style::default() });
        d.wb.add_sheet("Two").unwrap();
        d.wb.add_sheet("Three").unwrap();
        d.wb.recalc();
        let sel = CellRange { r0: 0, c0: 0, r1: 0, c1: 2 };
        let m = super::fill_across(&mut d, sel, "all").unwrap();
        assert_eq!(m, "A1:C1 copied to 2 other sheets.");
        for t in [1, 2] {
            assert_eq!(d.wb.formatted(CellAddr::new(t, 0, 0)).text, "Total");
            assert_eq!(d.wb.formula_text(CellAddr::new(t, 0, 2)).as_deref(), Some("=B1*2"));
            assert_eq!(d.wb.value(t, 0, 2), officina_grid::Value::Num(10.0));
            assert_eq!(d.wb.style(CellAddr::new(t, 0, 0)).fill, Some(0xFFC7CE));
        }
        assert!(d.undo());
        assert_eq!(d.wb.value(1, 0, 0), officina_grid::Value::Empty);
        // Formats only, to one sheet.
        super::fill_across(&mut d, sel, "Three formats").unwrap();
        assert_eq!(d.wb.value(2, 0, 0), officina_grid::Value::Empty);
        assert_eq!(d.wb.style(CellAddr::new(2, 0, 0)).fill, Some(0xFFC7CE));
        assert_eq!(d.wb.style(CellAddr::new(1, 0, 0)).fill, None);
        assert!(super::fill_across(&mut d, sel, "Nope").unwrap_err().contains("no sheet"));
        assert!(super::fill_across(&mut d, sel, "Sheet1").unwrap_err().contains("being copied from"));
    }
}

#[cfg(test)]
mod color_sort_tests {
    use crate::doc::Doc;
    use officina_grid::addr::{CellAddr, Rect};
    use officina_grid::style::Style;
    use pergola::widgets::CellRange;

    #[test]
    fn custom_sort_puts_a_colour_first_or_last_and_keeps_the_rest_in_order() {
        let mut d = Doc::from_csv("c.csv", b"Name,Qty\na,1\nb,2\nc,3\nd,4\n").unwrap().0;
        d.wb.recalc();
        let red = Style { fill: Some(0xFFC7CE), ..Style::default() };
        for r in [2, 4] {
            d.wb.set_style(&Rect { sheet: 0, r0: r, c0: 0, r1: r, c1: 0 }, red.clone());
        }
        let mut font = Style::default();
        font.font.color = Some(0x00B050);
        d.wb.set_style(&Rect { sheet: 0, r0: 3, c0: 1, r1: 3, c1: 1 }, font);
        let names = |d: &Doc| (1..5).map(|r| d.wb.formatted(CellAddr::new(0, r, 0)).text).collect::<Vec<_>>();
        let sel = CellRange { r0: 1, c0: 0, r1: 1, c1: 0 };
        let m = crate::data::custom_sort(&mut d, sel, (1, 0), "Name fill FFC7CE").unwrap();
        assert!(m.contains("A fill #FFC7CE first"), "{m}");
        assert_eq!(names(&d), ["b", "d", "a", "c"]);
        // The colour went with its cells.
        assert_eq!(d.wb.style(CellAddr::new(0, 1, 0)).fill, Some(0xFFC7CE));
        crate::data::custom_sort(&mut d, sel, (1, 0), "Name fill ffc7ce bottom, Name desc").unwrap();
        assert_eq!(names(&d), ["c", "a", "d", "b"]);
        // Font colour, then values.
        crate::data::custom_sort(&mut d, sel, (1, 0), "Qty font 00B050, Name").unwrap();
        assert_eq!(names(&d), ["c", "a", "b", "d"]);
        assert!(crate::data::custom_sort(&mut d, sel, (1, 0), "Name fill pink").unwrap_err().contains("isn't a colour"));
        assert!(d.undo());
        assert_eq!(names(&d), ["c", "a", "d", "b"]);
    }
}
