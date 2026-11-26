//! PivotTable Tools… (docs/spec/officina.md §16.164), as Excel's PivotTable
//! Analyze tab, typed in one line for the pivot at the cursor: Show Values As,
//! calculated fields, grouping, slicers and timelines and their selections,
//! and a PivotChart. Each line is one undo step. The slicers and timelines are
//! drawn on the sheet by [`crate::source`]; a click on one of their buttons is
//! [`press`].

use officina_grid::charts::{Anchor, Chart, ChartKind, Grouping as ChartGrouping, LegendPos};
use officina_grid::ops::GridOp;
use officina_grid::pivot::{
    in_layout, pivot_chart_series, slicer_name, BaseItem, CalculatedField, DataField, DatePart, GroupField, GroupKind, Pivot, PivotField, ReferenceCalc, ShowAs, Slicer, MAX_GROUPS, MAX_SLICERS,
};
use officina_grid::addr::MAX_COLS;
use officina_grid::{CellAddr, Value, Workbook};

use crate::doc::Doc;

/// Periods a timeline shows, at most (the latest ones).
pub const MAX_PERIODS: usize = 400;
/// Buttons a slicer shows, at most.
pub const MAX_BUTTONS: usize = 200;

fn pivot_here(d: &Doc, cursor: (u32, u32)) -> Option<usize> {
    crate::pivotpane::pivot_here(&d.wb, d.sheet, cursor.0, cursor.1)
        .or_else(|| d.wb.pivots().iter().position(|p| p.slicers.iter().any(|s| s.anchor.sheet == d.sheet && (s.anchor.from.0..=s.anchor.to.0).contains(&cursor.0) && (s.anchor.from.1..=s.anchor.to.1).contains(&cursor.1))))
}

/// Whether PivotTable Tools has a pivot to work on at `cursor`.
pub fn can_use(d: &Doc, cursor: (u32, u32)) -> bool {
    pivot_here(d, cursor).is_some()
}

/// The bar's starting text: the pivot's slicers and how its values show.
pub fn start(d: &Doc, cursor: (u32, u32)) -> String {
    let Some(i) = pivot_here(d, cursor) else { return String::new() };
    let p = &d.wb.pivots()[i];
    match p.data.first() {
        Some(v) => format!("show {} as {}", d.wb.data_caption(p, v), show_text(&d.wb, p, &v.show_as)),
        None => String::new(),
    }
}

fn show_text(wb: &Workbook, p: &Pivot, s: &ShowAs) -> String {
    let name = |f: u32| wb.pivot_field_name(p, f);
    let item = |i: &BaseItem| match i {
        BaseItem::Previous => "previous".to_string(),
        BaseItem::Next => "next".to_string(),
        BaseItem::Named(v) => v.display(),
    };
    match s {
        ShowAs::Normal => "normal".into(),
        ShowAs::PercentOfRow => "% of row".into(),
        ShowAs::PercentOfColumn => "% of column".into(),
        ShowAs::PercentOfTotal => "% of total".into(),
        ShowAs::Index => "index".into(),
        ShowAs::PercentOfParentRow => "% of parent row".into(),
        ShowAs::PercentOfParentColumn => "% of parent column".into(),
        ShowAs::PercentOfParent(f) => format!("% of parent {}", name(*f)),
        ShowAs::RunningTotal(f) => format!("running total in {}", name(*f)),
        ShowAs::PercentOfRunningTotal(f) => format!("% running total in {}", name(*f)),
        ShowAs::Rank { field, descending } => format!("rank {} by {}", if *descending { "largest" } else { "smallest" }, name(*field)),
        ShowAs::Reference { calc, field, item: it } => {
            let what = match calc {
                ReferenceCalc::Difference => "difference from",
                ReferenceCalc::Percent => "% of",
                ReferenceCalc::PercentDifference => "% difference from",
            };
            format!("{what} {} {}", name(*field), item(it))
        }
    }
}

/// Every field of the pivot that a name can mean: source columns, groups,
/// calculated fields.
fn field_ids(p: &Pivot) -> Vec<u32> {
    let width = p.source.c1 - p.source.c0 + 1;
    (0..width).chain(p.groups.iter().map(|g| g.field).filter(|&f| f >= width)).chain(p.calculations.iter().map(|c| c.field)).collect()
}

fn field(wb: &Workbook, p: &Pivot, name: &str) -> Result<u32, String> {
    let name = name.trim().trim_matches('"');
    field_ids(p).into_iter().find(|&f| wb.pivot_field_name(p, f).eq_ignore_ascii_case(name)).ok_or_else(|| format!("The pivot has no field “{name}”."))
}

fn value_field(wb: &Workbook, p: &Pivot, name: &str) -> Result<usize, String> {
    let name = name.trim().trim_matches('"');
    p.data.iter().position(|d| wb.data_caption(p, d).eq_ignore_ascii_case(name))
        .or_else(|| p.data.iter().position(|d| wb.pivot_field_name(p, d.field).eq_ignore_ascii_case(name)))
        .ok_or_else(|| format!("The pivot has no value field “{name}” (it has {}).", p.data.iter().map(|d| wb.data_caption(p, d)).collect::<Vec<_>>().join(", ")))
}

/// `previous`, `next`, or an item of `f`.
fn base_item(wb: &Workbook, p: &Pivot, f: u32, text: &str) -> Result<BaseItem, String> {
    match text.trim().to_ascii_lowercase().as_str() {
        "previous" | "(previous)" => return Ok(BaseItem::Previous),
        "next" | "(next)" => return Ok(BaseItem::Next),
        _ => {}
    }
    wb.pivot_items(p, f).into_iter().find(|v| v.display().eq_ignore_ascii_case(text.trim())).map(BaseItem::Named)
        .ok_or_else(|| format!("{} has no item “{}”.", wb.pivot_field_name(p, f), text.trim()))
}

/// Reads a Show Values As kind, with the field names it needs.
fn show_as(wb: &Workbook, p: &Pivot, text: &str) -> Result<ShowAs, String> {
    // `percent` as `%`, `%` its own word; case kept for names (lowercasing
    // keeps byte offsets, so a prefix's length is the same in both).
    let mut t = String::new();
    let lower = text.to_ascii_lowercase();
    let mut k = 0;
    while k < text.len() {
        if lower[k..].starts_with("percent") {
            t.push_str(" % ");
            k += 7;
        } else if text[k..].starts_with('%') {
            t.push_str(" % ");
            k += 1;
        } else {
            let c = text[k..].chars().next().unwrap_or(' ');
            t.push(c);
            k += c.len_utf8();
        }
    }
    let t = t.split_whitespace().collect::<Vec<_>>().join(" ");
    let l = t.to_ascii_lowercase();
    let fixed = match l.as_str() {
        "normal" | "no calculation" | "none" => Some(ShowAs::Normal),
        "% of total" | "% of grand total" => Some(ShowAs::PercentOfTotal),
        "% of row" | "% of row total" => Some(ShowAs::PercentOfRow),
        "% of column" | "% of column total" => Some(ShowAs::PercentOfColumn),
        "% of parent row" | "% of parent row total" => Some(ShowAs::PercentOfParentRow),
        "% of parent column" | "% of parent column total" => Some(ShowAs::PercentOfParentColumn),
        "index" => Some(ShowAs::Index),
        _ => None,
    };
    if let Some(s) = fixed {
        return Ok(s);
    }
    let after = |prefix: &str| l.starts_with(prefix).then(|| &t[prefix.len()..]);
    if let Some(f) = after("% running total in ") {
        return Ok(ShowAs::PercentOfRunningTotal(field(wb, p, f)?));
    }
    if let Some(f) = after("running total in ") {
        return Ok(ShowAs::RunningTotal(field(wb, p, f)?));
    }
    if let Some(f) = after("% of parent ") {
        return Ok(ShowAs::PercentOfParent(field(wb, p, f)?));
    }
    for (prefix, descending) in [("rank largest to smallest by ", true), ("rank smallest to largest by ", false), ("rank largest by ", true), ("rank smallest by ", false), ("rank descending by ", true), ("rank ascending by ", false)] {
        if let Some(f) = after(prefix) {
            return Ok(ShowAs::Rank { field: field(wb, p, f)?, descending });
        }
    }
    for (prefix, calc) in [("% difference from ", ReferenceCalc::PercentDifference), ("difference from ", ReferenceCalc::Difference), ("% of ", ReferenceCalc::Percent)] {
        let Some(rest) = after(prefix) else { continue };
        // FIELD ITEM: the longest field name that leads the rest.
        let mut best: Option<(u32, usize)> = None;
        for f in field_ids(p) {
            let n = wb.pivot_field_name(p, f);
            if rest.len() > n.len() && rest.is_char_boundary(n.len()) && rest[..n.len()].eq_ignore_ascii_case(&n) && rest.as_bytes()[n.len()] == b' ' && best.is_none_or(|b| n.len() > b.1) {
                best = Some((f, n.len()));
            }
        }
        let (f, n) = best.ok_or_else(|| format!("Type the base field and item after “{}”, such as {prefix}Region previous.", prefix.trim()))?;
        let item = base_item(wb, p, f, &rest[n + 1..])?;
        return Ok(ShowAs::Reference { calc, field: f, item });
    }
    Err(format!("“{}” isn't a Show Values As kind: normal, % of total, % of row, % of column, % of parent row, % of parent column, % of parent FIELD, index, difference from FIELD ITEM, % of FIELD ITEM, % difference from FIELD ITEM, running total in FIELD, % running total in FIELD, rank smallest by FIELD, rank largest by FIELD.", text.trim()))
}

const PARTS: [(DatePart, &str, &[&str]); 7] = [
    (DatePart::Years, "Years", &["years", "year"]),
    (DatePart::Quarters, "Quarters", &["quarters", "quarter"]),
    (DatePart::Months, "Months", &["months", "month"]),
    (DatePart::Days, "Days", &["days", "day"]),
    (DatePart::Hours, "Hours", &["hours", "hour"]),
    (DatePart::Minutes, "Minutes", &["minutes", "minute"]),
    (DatePart::Seconds, "Seconds", &["seconds", "second"]),
];

fn part_of(word: &str) -> Option<DatePart> {
    let w = word.trim().to_ascii_lowercase();
    PARTS.iter().find(|(_, _, ws)| ws.contains(&w.as_str())).map(|(p, _, _)| *p)
}

fn part_name(part: DatePart) -> &'static str {
    PARTS.iter().find(|(p, _, _)| *p == part).map_or("Days", |(_, n, _)| n)
}

fn free_field(p: &Pivot) -> Result<u32, String> {
    (MAX_COLS..MAX_COLS + MAX_GROUPS as u32)
        .find(|f| !p.groups.iter().any(|g| g.field == *f) && !p.calculations.iter().any(|c| c.field == *f))
        .ok_or_else(|| format!("A pivot has at most {MAX_GROUPS} grouped and calculated fields."))
}

fn numbers_in(wb: &Workbook, p: &Pivot, f: u32) -> bool {
    f <= p.source.c1 - p.source.c0 && wb.pivot_source_items(p, f).iter().any(|v| matches!(v, Value::Num(_)))
}

/// `2024`, `2024-03` or `2024-03-31`: the serial of its first day, and of the
/// first day after it.
fn date_span(wb: &Workbook, text: &str) -> Result<(f64, f64), String> {
    let ds = wb.date_system();
    let parts: Vec<i64> = text.trim().split(['-', '/']).map(|x| x.trim().parse::<i64>()).collect::<Result<_, _>>().map_err(|_| format!("“{}” isn't a date (2024, 2024-03 or 2024-03-31).", text.trim()))?;
    let bad = || format!("“{}” isn't a date (2024, 2024-03 or 2024-03-31).", text.trim());
    let (from, to) = match parts.as_slice() {
        [y] => (ds.date(*y as f64, 1.0, 1.0), ds.date(*y as f64 + 1.0, 1.0, 1.0)),
        [y, m] if (1..=12).contains(m) => (ds.date(*y as f64, *m as f64, 1.0), ds.date(*y as f64, *m as f64 + 1.0, 1.0)),
        [y, m, dd] if (1..=12).contains(m) && (1..=31).contains(dd) => {
            let a = ds.date(*y as f64, *m as f64, *dd as f64);
            (a, a.map(|a| a + 1.0))
        }
        _ => return Err(bad()),
    };
    match (from, to) {
        (Some(a), Some(b)) if a < b => Ok((a, b)),
        _ => Err(bad()),
    }
}

/// Where a new slicer or timeline goes: right of the pivot, past the others.
fn new_anchor(wb: &Workbook, p: &Pivot, i: usize, timeline: bool) -> Anchor {
    let area = wb.pivot_shape(i).and_then(|s| s.area);
    let (r, c) = area.map_or((p.dest.row, p.dest.col + 2), |a| (a.r0, a.c1 + 2));
    let right = p.slicers.iter().filter(|s| s.anchor.sheet == p.dest.sheet).map(|s| s.anchor.to.1 + 2).max().unwrap_or(c).max(c);
    let (h, w) = if timeline { (5, 6) } else { (9, 2) };
    let c0 = right.min(MAX_COLS - 1 - w);
    let r0 = r.min(officina_grid::addr::MAX_ROWS - 1 - h);
    Anchor { sheet: p.dest.sheet, from: (r0, c0, 0.0, 0.0), to: (r0 + h, c0 + w, 0.0, 0.0) }
}

fn at_anchor(d: &Doc, text: &str, timeline: bool) -> Result<Anchor, String> {
    let r = crate::whatif::parse_area(&d.wb, d.sheet, text)?;
    let (h, w) = if timeline { (5, 6) } else { (9, 2) };
    let (r1, c1) = if r.r0 == r.r1 && r.c0 == r.c1 { (r.r0 + h, r.c0 + w) } else { (r.r1, r.c1) };
    if r1 >= officina_grid::addr::MAX_ROWS || c1 >= MAX_COLS {
        return Err("That's too near the sheet's edge.".into());
    }
    Ok(Anchor { sheet: r.sheet, from: (r.r0, r.c0, 0.0, 0.0), to: (r1, c1, 0.0, 0.0) })
}

/// PivotTable Tools…: what was typed, on the pivot at the cursor. One step.
pub fn run(d: &mut Doc, cursor: (u32, u32), typed: &str) -> Result<String, String> {
    let i = pivot_here(d, cursor).ok_or("Put the cursor in a pivot table (or on its slicer) first.")?;
    let before = d.wb.pivots()[i].clone();
    let mut p = before.clone();
    let mut chart: Option<ChartKind> = None;
    let mut said = Vec::new();
    for part in typed.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let low = part.to_ascii_lowercase();
        let words: Vec<&str> = part.split_whitespace().collect();
        let after = |n: usize| words[n.min(words.len())..].join(" ");
        if low.starts_with("show ") {
            let (what, how) = part[5..].split_once(" as ").ok_or("Type show VALUE FIELD as KIND, such as show Sum of Sales as % of total.")?;
            let v = value_field(&d.wb, &p, what)?;
            let s = show_as(&d.wb, &p, how)?;
            if let Some(b) = s.base_field() {
                if !p.rows.iter().chain(&p.cols).any(|f| f.field == b) {
                    return Err(format!("{} needs {} in Rows or Columns.", how.trim(), d.wb.pivot_field_name(&p, b)));
                }
            }
            p.data[v].show_as = s;
            said.push("values shown");
        } else if low.starts_with("calculated ") {
            let (name, formula) = after(1).split_once('=').map(|(a, b)| (a.trim().to_string(), b.trim().to_string())).ok_or("Type calculated NAME = FORMULA, such as calculated Profit = Sales - Cost.")?;
            if name.is_empty() || formula.is_empty() {
                return Err("Type calculated NAME = FORMULA, such as calculated Profit = Sales - Cost.".into());
            }
            if field_ids(&p).into_iter().any(|f| d.wb.pivot_field_name(&p, f).eq_ignore_ascii_case(&name)) {
                return Err(format!("The pivot already has a field “{name}”."));
            }
            let f = free_field(&p)?;
            p.calculations.push(CalculatedField { field: f, name: name.as_str().into(), formula: format!("={formula}").into() });
            p.data.push(DataField { field: f, agg: officina_grid::pivot::Agg::Sum, name: None, show_as: ShowAs::Normal });
            said.push("calculated field added");
        } else if low.starts_with("remove calculated ") {
            let name = after(2);
            let k = p.calculations.iter().position(|c| c.name.eq_ignore_ascii_case(name.trim())).ok_or_else(|| format!("The pivot has no calculated field “{}”.", name.trim()))?;
            let f = p.calculations.remove(k).field;
            p.data.retain(|x| x.field != f);
            said.push("calculated field removed");
        } else if low.starts_with("ungroup ") {
            let f = field(&d.wb, &p, &after(1))?;
            let gone: Vec<u32> = p.groups.iter().filter(|g| g.source == f || g.field == f).map(|g| g.field).collect();
            if gone.is_empty() {
                return Err(format!("{} isn't grouped.", d.wb.pivot_field_name(&p, f)));
            }
            p.groups.retain(|g| !gone.contains(&g.field));
            let width = p.source.c1 - p.source.c0 + 1;
            for axis in [&mut p.rows, &mut p.cols, &mut p.pages] {
                axis.retain(|x| x.field < width || !gone.contains(&x.field));
                for x in axis.iter_mut().filter(|x| gone.contains(&x.field)) {
                    x.hidden.clear();
                }
            }
            p.slicers.retain(|s| s.field < width || !gone.contains(&s.field));
            p.filters.retain(|x| x.field < width || !gone.contains(&x.field));
            said.push("ungrouped");
        } else if low.starts_with("group ") {
            group(d, &mut p, part)?;
            said.push("grouped");
        } else if low.starts_with("slicer ") || low.starts_with("timeline ") {
            let timeline = low.starts_with("timeline ");
            let mut rest = after(1);
            let mut at = None;
            if let Some(k) = rest.to_ascii_lowercase().rfind(" at ") {
                at = Some(at_anchor(d, &rest[k + 4..], timeline)?);
                rest.truncate(k);
            }
            let mut level = DatePart::Months;
            if timeline {
                if let Some(k) = rest.to_ascii_lowercase().rfind(" by ") {
                    level = part_of(&rest[k + 4..]).filter(|p| Slicer::timeline_part_ok(*p)).ok_or("A timeline shows years, quarters, months or days.")?;
                    rest.truncate(k);
                }
            }
            let f = field(&d.wb, &p, &rest)?;
            if timeline && !numbers_in(&d.wb, &p, f) {
                return Err(format!("A timeline needs a column of dates; {} has none.", d.wb.pivot_field_name(&p, f)));
            }
            if p.calculations.iter().any(|c| c.field == f) {
                return Err("A slicer filters by a field's items; a calculated field has none.".into());
            }
            if p.slicers.len() >= MAX_SLICERS {
                return Err(format!("A pivot has at most {MAX_SLICERS} slicers and timelines."));
            }
            let anchor = match at { Some(a) => a, None => new_anchor(&d.wb, &p, i, timeline) };
            let name = slicer_name(&p, &d.wb.pivot_field_name(&p, f));
            p.slicers.push(Slicer { name: name.as_str().into(), field: f, anchor, timeline: timeline.then_some(level) });
            said.push(if timeline { "timeline added" } else { "slicer added" });
        } else if low.starts_with("remove slicer ") || low.starts_with("remove timeline ") {
            let name = after(2);
            let k = p.slicers.iter().position(|s| s.name.eq_ignore_ascii_case(name.trim())).ok_or_else(|| format!("The pivot has no slicer or timeline “{}”.", name.trim()))?;
            p.slicers.remove(k);
            said.push("slicer removed");
        } else if low.starts_with("select ") {
            let (name, items) = after(1).split_once(':').map(|(a, b)| (a.to_string(), b.to_string())).ok_or("Type select FIELD: ITEM, ITEM (or all).")?;
            let f = field(&d.wb, &p, &name)?;
            let all = d.wb.pivot_items(&p, f);
            let hidden = if items.trim().eq_ignore_ascii_case("all") {
                Vec::new()
            } else {
                let want: Vec<&str> = items.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
                for w in &want {
                    if !all.iter().any(|v| v.display().eq_ignore_ascii_case(w)) {
                        return Err(format!("{} has no item “{w}”.", d.wb.pivot_field_name(&p, f)));
                    }
                }
                if want.is_empty() {
                    return Err("Name at least one item to show, or all.".into());
                }
                all.into_iter().filter(|v| !want.iter().any(|w| v.display().eq_ignore_ascii_case(w))).collect()
            };
            p.set_hidden(f, hidden);
            said.push("filtered");
        } else if low.starts_with("dates ") {
            let rest = after(1);
            let lr = rest.to_ascii_lowercase();
            if let Some(name) = lr.strip_suffix(" all").map(|_| &rest[..rest.len() - 4]) {
                let f = field(&d.wb, &p, name)?;
                p.set_range(f, None);
            } else {
                let k = lr.find(" from ").ok_or("Type dates FIELD from DATE to DATE (2024, 2024-03 or 2024-03-31), or dates FIELD all.")?;
                let f = field(&d.wb, &p, &rest[..k])?;
                let span = &rest[k + 6..];
                let (a, b) = span.split_once(" to ").ok_or("Type dates FIELD from DATE to DATE.")?;
                let (from, _) = date_span(&d.wb, a)?;
                let (_, to) = date_span(&d.wb, b)?;
                if from >= to {
                    return Err("The range ends before it starts.".into());
                }
                if f > p.source.c1 - p.source.c0 {
                    return Err("A date range filters a column of the source.".into());
                }
                p.set_range(f, Some((from, to)));
            }
            said.push("dates set");
        } else if low == "chart" || low.starts_with("chart ") {
            let k = after(1);
            chart = Some(if k.is_empty() {
                ChartKind::Column
            } else {
                match k.to_ascii_lowercase().as_str() {
                    "column" => ChartKind::Column,
                    "bar" => ChartKind::Bar,
                    "line" => ChartKind::Line,
                    "area" => ChartKind::Area,
                    "pie" => ChartKind::Pie,
                    "doughnut" => ChartKind::Doughnut,
                    "radar" => ChartKind::Radar,
                    _ => return Err("A PivotChart is a column, bar, line, area, pie, doughnut or radar chart.".into()),
                }
            });
        } else {
            return Err(format!("“{part}” isn't a PivotTable Tools command (show, calculated, group, ungroup, slicer, timeline, select, dates, chart, remove …)."));
        }
    }
    d.status.clear();
    if p != before {
        let op = d.wb.pivot_op(Some(i), Some(p)).map_err(|e| format!("Couldn't change the pivot table: {e}."))?;
        if !d.run_action("PivotTable Tools", vec![op]) {
            return Err(std::mem::take(&mut d.status));
        }
    }
    if let Some(kind) = chart {
        let p = d.wb.pivots()[i].clone();
        let shape = d.wb.pivot_shape(i).cloned().ok_or("The pivot table is empty.")?;
        let mut series = pivot_chart_series(&shape, p.compact);
        if series.is_empty() {
            return Err("The pivot has no values to chart.".into());
        }
        if kind.round() {
            series.truncate(1);
        }
        let area = shape.area.ok_or("The pivot table is empty.")?;
        let r0 = area.r0.max(1) - 1;
        let anchor = Anchor { sheet: area.sheet, from: (r0 + 1, area.c1 + 2, 0.0, 0.0), to: (r0 + 16, area.c1 + 9, 0.0, 0.0) };
        let anchor = if p.slicers.iter().any(|s| s.anchor.sheet == area.sheet && s.anchor.from.1 <= anchor.to.1 && anchor.from.1 <= s.anchor.to.1 && s.anchor.from.0 <= anchor.to.0 && anchor.from.0 <= s.anchor.to.0) {
            Anchor { from: (area.r1 + 2, area.c0, 0.0, 0.0), to: (area.r1 + 17, area.c0 + 7, 0.0, 0.0), ..anchor }
        } else {
            anchor
        };
        let c = Chart { kind, grouping: ChartGrouping::Standard, title: Some(officina_grid::charts::ChartText::Lit(p.name.clone())), series, legend: Some(LegendPos::Right), x_title: None, y_title: None, anchor, alt: None };
        let mut charts = d.wb.charts().to_vec();
        charts.push(c);
        let ops = vec![GridOp::Charts(charts)];
        if p == before {
            if !d.run_action("Insert PivotChart", ops) {
                return Err(std::mem::take(&mut d.status));
            }
        } else {
            d.extend(ops);
        }
        said.push("PivotChart added");
    }
    if said.is_empty() {
        return Ok("Nothing to change.".into());
    }
    Ok(format!("{}: {}.", d.wb.pivots()[i].name, said.join(", ")))
}

/// `group FIELD by months, quarters, years` (dates: the finest part in place,
/// the coarser ones as new fields outside it), `group FIELD by 7 days`,
/// `group FIELD from A to B by STEP` or `group FIELD by STEP` (numbers).
fn group(d: &Doc, p: &mut Pivot, part: &str) -> Result<(), String> {
    let rest = part["group ".len()..].trim();
    let lr = rest.to_ascii_lowercase();
    let (head, spec) = match (lr.find(" from "), lr.rfind(" by ")) {
        (Some(k), _) => (&rest[..k], &rest[k..]),
        (None, Some(k)) => (&rest[..k], &rest[k..]),
        _ => return Err("Type group FIELD by months, quarters, years, or group FIELD from 0 to 100 by 10.".into()),
    };
    let f = field(&d.wb, p, head)?;
    let width = p.source.c1 - p.source.c0 + 1;
    if f >= width {
        return Err("Group a column of the source (ungroup this field first).".into());
    }
    if !numbers_in(&d.wb, p, f) {
        return Err(format!("{} has no numbers or dates to group.", d.wb.pivot_field_name(p, f)));
    }
    if p.groups.iter().any(|g| g.source == f || g.field == f) {
        return Err(format!("{} is grouped already; ungroup it first.", d.wb.pivot_field_name(p, f)));
    }
    let sl = spec.to_ascii_lowercase();
    let by = sl.rfind(" by ").map(|k| spec[k + 4..].trim().to_string()).ok_or("Say what to group by.")?;
    let range = sl.find(" from ").map(|k| {
        let end = sl.rfind(" by ").unwrap_or(sl.len());
        spec[k + 6..end].to_string()
    });
    let num = |s: &str| s.trim().parse::<f64>().ok().filter(|v| v.is_finite()).ok_or_else(|| format!("“{}” isn't a number.", s.trim()));
    let (start, end) = match &range {
        Some(r) => {
            let (a, b) = r.split_once(" to ").ok_or("Type from A to B.")?;
            let (a, b) = (num(a)?, num(b)?);
            if a >= b {
                return Err("The range ends before it starts.".into());
            }
            (Some(a), Some(b))
        }
        None => (None, None),
    };
    let name: std::sync::Arc<str> = d.wb.pivot_field_name(p, f).as_str().into();
    // Date parts?
    let parts: Vec<&str> = by.split([',', ' ']).map(str::trim).filter(|w| !w.is_empty() && *w != "and").collect();
    if parts.iter().all(|w| part_of(w).is_some()) {
        let mut ps: Vec<DatePart> = parts.iter().filter_map(|w| part_of(w)).collect();
        ps.sort_by_key(|x| PARTS.iter().position(|(q, _, _)| q == x));
        ps.dedup();
        let finest = *ps.last().ok_or("Say what to group by.")?;
        p.groups.push(GroupField { field: f, source: f, name: name.clone(), kind: GroupKind::DateParts { start, end, part: finest } });
        let mut outer = Vec::new();
        for part in &ps[..ps.len() - 1] {
            let id = free_field(p)?;
            let label = part_name(*part);
            if field_ids(p).into_iter().any(|g| d.wb.pivot_field_name(p, g).eq_ignore_ascii_case(label)) {
                return Err(format!("The pivot already has a field “{label}”."));
            }
            p.groups.push(GroupField { field: id, source: f, name: label.into(), kind: GroupKind::DateParts { start, end, part: *part } });
            outer.push(id);
        }
        // The coarser parts go just outside the field, where it is.
        for axis in [&mut p.rows, &mut p.cols] {
            if let Some(k) = axis.iter().position(|x| x.field == f) {
                axis[k].hidden.clear();
                for (j, id) in outer.iter().enumerate() {
                    axis.insert(k + j, PivotField::new(*id));
                }
                break;
            }
        }
        if let Some(x) = p.pages.iter_mut().find(|x| x.field == f) {
            x.hidden.clear();
        }
        return Ok(());
    }
    // A step: `7 days` for dates, a number otherwise.
    let byl = by.to_ascii_lowercase();
    let (step, dates) = match byl.strip_suffix(" days").or_else(|| byl.strip_suffix(" day")) {
        Some(n) => (num(n)?, true),
        None => (num(&by)?, false),
    };
    if step <= 0.0 {
        return Err("The step is more than 0.".into());
    }
    p.groups.push(GroupField { field: f, source: f, name, kind: GroupKind::Intervals { start, end, step, dates } });
    for x in p.rows.iter_mut().chain(&mut p.cols).chain(&mut p.pages).filter(|x| x.field == f) {
        x.hidden.clear();
    }
    Ok(())
}

/// What a slicer or timeline button does.
#[derive(Debug, Clone, PartialEq)]
pub enum Press {
    /// Show only this item (by its index in the field's items).
    Item(usize),
    /// Show only these dates.
    Period(f64, f64),
    /// Clear the filter.
    Clear,
}

/// One button: its label, whether it is selected, and what a press does.
#[derive(Debug, Clone, PartialEq)]
pub struct Button {
    pub label: String,
    pub on: bool,
    pub press: Press,
}

/// The buttons of slicer `s` of `p`: each item (shown or not), or each period
/// of a timeline from the first date to the last (the latest `MAX_PERIODS`).
pub fn buttons(wb: &Workbook, p: &Pivot, s: &Slicer) -> Vec<Button> {
    match s.timeline {
        None => {
            let hidden = p.hidden_of(s.field);
            let key = |v: &Value| v.display().to_lowercase();
            wb.pivot_items(p, s.field).into_iter().take(MAX_BUTTONS).enumerate().map(|(i, v)| Button {
                on: !hidden.iter().any(|h| key(h) == key(&v)),
                label: if v.is_empty() { "(blank)".to_string() } else { v.display() },
                press: Press::Item(i),
            }).collect()
        }
        Some(level) => {
            let ds = wb.date_system();
            let src = p.source;
            let col = src.c0 + s.field;
            let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
            for r in src.r0 + 1..=src.r1 {
                if let Value::Num(x) = wb.get(CellAddr::new(src.sheet, r, col)) {
                    if ds.ymd(x).is_some() {
                        lo = lo.min(x);
                        hi = hi.max(x);
                    }
                }
            }
            if lo > hi {
                return Vec::new();
            }
            let range = p.range_of(s.field);
            let months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
            let (Some((y0, m0, d0)), Some((y1, m1, d1))) = (ds.ymd(lo), ds.ymd(hi)) else { return Vec::new() };
            let mut out = Vec::new();
            let mut push = |label: String, a: Option<f64>, b: Option<f64>| {
                if let (Some(a), Some(b)) = (a, b) {
                    let on = range.is_none_or(|(x, y)| a < y && x < b);
                    out.push(Button { label, on, press: Press::Period(a, b) });
                }
            };
            match level {
                DatePart::Years => {
                    for y in y0..=y1.min(y0 + MAX_PERIODS as i64 * 4) {
                        push(y.to_string(), ds.date(y as f64, 1.0, 1.0), ds.date(y as f64 + 1.0, 1.0, 1.0));
                    }
                }
                DatePart::Quarters => {
                    let (q0, q1) = (y0 * 4 + (m0 - 1) / 3, y1 * 4 + (m1 - 1) / 3);
                    for q in q0.max(q1 - MAX_PERIODS as i64 * 4)..=q1 {
                        let (y, k) = (q.div_euclid(4), q.rem_euclid(4));
                        push(format!("{y} Q{}", k + 1), ds.date(y as f64, (k * 3 + 1) as f64, 1.0), ds.date(y as f64, (k * 3 + 4) as f64, 1.0));
                    }
                }
                DatePart::Days => {
                    let first = (hi.floor() - MAX_PERIODS as f64 * 4.0).max(lo.floor());
                    let mut x = first;
                    while x <= hi.floor() {
                        if let Some((y, m, dd)) = ds.ymd(x) {
                            push(format!("{dd} {} {y}", months[(m - 1) as usize]), Some(x), Some(x + 1.0));
                        }
                        x += 1.0;
                    }
                    let _ = (d0, d1);
                }
                _ => {
                    let (a, b) = (y0 * 12 + m0 - 1, y1 * 12 + m1 - 1);
                    for k in a.max(b - MAX_PERIODS as i64 * 4)..=b {
                        let (y, m) = (k.div_euclid(12), k.rem_euclid(12));
                        push(format!("{} {y}", months[m as usize]), ds.date(y as f64, (m + 1) as f64, 1.0), ds.date(y as f64, (m + 2) as f64, 1.0));
                    }
                }
            }
            let n = out.len();
            out.split_off(n.saturating_sub(MAX_PERIODS))
        }
    }
}

/// A press on button `b` of slicer `k` of pivot `i`: show only that item or
/// period, or clear the filter. One step.
pub fn press(d: &mut Doc, i: usize, k: usize, b: &Press) -> Result<(), String> {
    let mut p = d.wb.pivots().get(i).ok_or("That pivot table is gone.")?.clone();
    let s = p.slicers.get(k).ok_or("That slicer is gone.")?.clone();
    match b {
        Press::Clear => {
            if s.timeline.is_some() {
                p.set_range(s.field, None);
            } else {
                p.set_hidden(s.field, Vec::new());
            }
        }
        Press::Item(j) => {
            let items = d.wb.pivot_items(&p, s.field);
            let Some(keep) = items.get(*j).cloned() else { return Ok(()) };
            let hidden: Vec<Value> = items.into_iter().filter(|v| !officina_grid::pivot::same_item(v, &keep)).collect();
            p.set_hidden(s.field, hidden);
        }
        Press::Period(a, b) => p.set_range(s.field, Some((*a, *b))),
    }
    if p == d.wb.pivots()[i] {
        return Ok(());
    }
    let label = if s.timeline.is_some() { "Timeline" } else { "Slicer" };
    let op = d.wb.pivot_op(Some(i), Some(p)).map_err(|e| format!("Couldn't filter the pivot table: {e}."))?;
    d.status.clear();
    if !d.run_action(label, vec![op]) {
        return Err(std::mem::take(&mut d.status));
    }
    Ok(())
}

/// Whether slicer `s` of `p` filters anything now.
pub fn filtering(p: &Pivot, s: &Slicer) -> bool {
    if s.timeline.is_some() { p.range_of(s.field).is_some() } else { !p.hidden_of(s.field).is_empty() }
}

/// For the bar's hint and the menu: a field in the layout or a slicer's.
pub fn layout_field(p: &Pivot, f: u32) -> bool {
    in_layout(p, f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use officina_grid::pivot::Agg;
    use officina_grid::Rect;

    /// Date, Region, Sales, Cost over 2024: one row a month.
    fn doc() -> Doc {
        let mut d = Doc::new("t");
        let ds = d.wb.date_system();
        for (c, h) in ["Date", "Region", "Sales", "Cost"].iter().enumerate() {
            d.wb.set_value(CellAddr::new(0, 0, c as u32), Value::str(h));
        }
        for m in 1..=12u32 {
            let date = ds.date(2024.0, f64::from(m), 15.0).unwrap();
            d.wb.set_value(CellAddr::new(0, m, 0), Value::Num(date));
            d.wb.set_value(CellAddr::new(0, m, 1), Value::str(if m % 2 == 0 { "East" } else { "West" }));
            d.wb.set_value(CellAddr::new(0, m, 2), Value::Num(f64::from(m * 10)));
            d.wb.set_value(CellAddr::new(0, m, 3), Value::Num(f64::from(m * 4)));
        }
        let mut p = Pivot::new("Sales", Rect { sheet: 0, r0: 0, c0: 0, r1: 12, c1: 3 }, CellAddr::new(0, 0, 6));
        p.rows = vec![PivotField::new(1)];
        p.data = vec![DataField { field: 2, agg: Agg::Sum, name: None, show_as: ShowAs::Normal }];
        let op = d.wb.pivot_op(None, Some(p)).unwrap();
        d.run("Insert Pivot Table", vec![op]);
        d
    }

    fn text(d: &Doc, r: u32, c: u32) -> String {
        d.wb.get(CellAddr::new(0, r, c)).display()
    }

    #[test]
    fn show_values_as_and_calculated_fields() {
        let mut d = doc();
        let at = (1, 6);
        // East 20+40+…+120 = 420, West 360, total 780.
        assert_eq!((text(&d, 1, 6), text(&d, 1, 7)), ("East".into(), "420".into()));
        run(&mut d, at, "show Sum of Sales as % of total").unwrap();
        // A fraction, shown as a percentage by its format.
        assert!(text(&d, 1, 7).starts_with("0.538"), "{}", text(&d, 1, 7));
        assert_eq!(start(&d, at), "show Sum of Sales as % of total");
        run(&mut d, at, "show Sales as difference from Region previous").unwrap();
        assert_eq!(start(&d, at), "show Sum of Sales as difference from Region previous");
        assert!(run(&mut d, at, "show Sales as % of Region Nowhere").unwrap_err().contains("no item"));
        run(&mut d, at, "show Sales as rank largest by Region").unwrap();
        assert_eq!(text(&d, 1, 7), "1");
        run(&mut d, at, "show Sales as normal; calculated Margin = Sales - Cost").unwrap();
        // Margin: East 420 - 168 = 252.
        assert_eq!(text(&d, 2, 8), "252");
        assert_eq!(text(&d, 1, 8), "Sum of Margin");
        assert!(run(&mut d, at, "calculated Margin = Sales").unwrap_err().contains("already has"));
        // One undo takes a line back whole.
        d.undo();
        assert!(d.wb.pivots()[0].calculations.is_empty());
        assert_eq!(d.wb.pivots()[0].data[0].show_as, ShowAs::Rank { field: 1, descending: true });
    }

    #[test]
    fn grouping_dates_and_numbers() {
        let mut d = doc();
        let at = (1, 6);
        // Dates by quarters and years: Years outside Quarters, on the rows.
        let mut p = d.wb.pivots()[0].clone();
        p.rows = vec![PivotField::new(0)];
        let op = d.wb.pivot_op(Some(0), Some(p)).unwrap();
        d.run("t", vec![op]);
        run(&mut d, at, "group Date by quarters, years").unwrap();
        let p = &d.wb.pivots()[0];
        assert_eq!(p.rows.len(), 2);
        assert_eq!(d.wb.pivot_field_name(p, p.rows[0].field), "Years");
        assert_eq!((text(&d, 1, 6), text(&d, 1, 7), text(&d, 1, 8)), ("2024".into(), "Q1".into(), "60".into()));
        assert!(run(&mut d, at, "group Date by months").unwrap_err().contains("grouped already"));
        run(&mut d, at, "ungroup Date").unwrap();
        assert!(d.wb.pivots()[0].groups.is_empty() && d.wb.pivots()[0].rows.len() == 1);
        // Numbers: Sales in steps of 50 from 0 to 150.
        let mut p = d.wb.pivots()[0].clone();
        p.rows = vec![PivotField::new(2)];
        p.data = vec![DataField { field: 3, agg: Agg::Sum, name: None, show_as: ShowAs::Normal }];
        let op = d.wb.pivot_op(Some(0), Some(p)).unwrap();
        d.run("t", vec![op]);
        run(&mut d, at, "group Sales from 0 to 150 by 50").unwrap();
        // 10..40 (4+8+12+16 = 40) fall in 0-49.
        assert_eq!((text(&d, 1, 6), text(&d, 1, 7)), ("0-49".into(), "40".into()));
        assert!(run(&mut d, at, "group Region by 5").unwrap_err().contains("no numbers"));
    }

    #[test]
    fn slicers_timelines_and_a_pivot_chart() {
        let mut d = doc();
        let at = (1, 6);
        run(&mut d, at, "slicer Region; timeline Date by quarters").unwrap();
        let p = d.wb.pivots()[0].clone();
        assert_eq!(p.slicers.len(), 2);
        assert_eq!((&*p.slicers[0].name, p.slicers[1].timeline), ("Region", Some(DatePart::Quarters)));
        // The slicer's buttons; a press shows only that item.
        let b = buttons(&d.wb, &p, &p.slicers[0]);
        assert_eq!(b.iter().map(|b| (b.label.as_str(), b.on)).collect::<Vec<_>>(), [("East", true), ("West", true)]);
        press(&mut d, 0, 0, &b[1].press).unwrap();
        assert_eq!(text(&d, 1, 6), "West");
        assert_eq!(text(&d, 2, 6), "Grand Total");
        let p = d.wb.pivots()[0].clone();
        assert!(filtering(&p, &p.slicers[0]));
        // The timeline: four quarters; Q2 picked keeps April to June.
        let t = buttons(&d.wb, &p, &p.slicers[1]);
        assert_eq!(t.iter().map(|b| b.label.as_str()).collect::<Vec<_>>(), ["2024 Q1", "2024 Q2", "2024 Q3", "2024 Q4"]);
        press(&mut d, 0, 1, &t[1].press).unwrap();
        // West in Q2: May, 50.
        assert_eq!(text(&d, 1, 7), "50");
        press(&mut d, 0, 0, &Press::Clear).unwrap();
        // Typed: both regions, and dates from March to May.
        run(&mut d, at, "select Region: East, west; dates Date from 2024-03 to 2024-05").unwrap();
        assert_eq!((text(&d, 1, 7), text(&d, 2, 7)), ("40".into(), "80".into()));
        assert!(run(&mut d, at, "select Region: North").unwrap_err().contains("no item"));
        assert!(run(&mut d, at, "timeline Region").unwrap_err().contains("column of dates"));
        // A PivotChart beside it, from the pivot's cells; it follows a refresh.
        run(&mut d, at, "chart").unwrap();
        let c = d.wb.charts()[0].clone();
        assert_eq!(c.series[0].vals, Rect { sheet: 0, r0: 1, c0: 7, r1: 2, c1: 7 });
        run(&mut d, at, "select Region: East").unwrap();
        assert_eq!(d.wb.charts()[0].series[0].vals, Rect { sheet: 0, r0: 1, c0: 7, r1: 1, c1: 7 });
        run(&mut d, at, "remove slicer Region").unwrap();
        assert_eq!(d.wb.pivots()[0].slicers.len(), 1);
        assert!(run(&mut d, at, "sparkle").unwrap_err().contains("isn't a PivotTable Tools command"));
    }
}
