//! Format Chart… (docs/spec/officina.md §16.163), as Excel's Change Chart
//! Type and the Format Data Series pane: the chart's type, and each series'
//! data labels, trendline, error bars, axis, bubble sizes, waterfall totals
//! and histogram bins, typed in one line and set as one undo step.
//!
//! `type waterfall; series 2: secondary, trend linear equation r2; labels`:
//! parts split on `;`, each optionally led by `series N:` (else it is for
//! every series), its settings split on `,`.

use officina_grid::charts::{Bins, Chart, ChartKind, ErrorBars, SeriesLook, TrendKind, Trendline, MAX_TOTALS};
use officina_grid::ops::GridOp;
use officina_grid::Workbook;

use crate::analysis::range_text;
use crate::doc::Doc;

/// The chart Format Chart works on at `cursor`: one over the cell, else one
/// drawn from it.
fn chart_here(d: &Doc, cursor: (u32, u32)) -> Option<usize> {
    crate::data::chart_at(&d.wb, d.sheet, cursor.0, cursor.1)
}

/// Whether there is a chart for Format Chart at `cursor`.
pub fn can_format(d: &Doc, cursor: (u32, u32)) -> bool {
    chart_here(d, cursor).is_some()
}

/// The words for each kind, the first the one written back.
const KINDS: [(ChartKind, &[&str]); 13] = [
    (ChartKind::Column, &["column", "columns"]),
    (ChartKind::Bar, &["bar", "bars"]),
    (ChartKind::Line, &["line", "lines"]),
    (ChartKind::Area, &["area"]),
    (ChartKind::Scatter, &["scatter", "xy"]),
    (ChartKind::Pie, &["pie"]),
    (ChartKind::Doughnut, &["doughnut", "donut"]),
    (ChartKind::Radar, &["radar", "spider"]),
    (ChartKind::Bubble, &["bubble"]),
    (ChartKind::Stock, &["stock", "hlc", "ohlc"]),
    (ChartKind::Waterfall, &["waterfall", "bridge"]),
    (ChartKind::Histogram, &["histogram"]),
    (ChartKind::BoxWhisker, &["box", "boxwhisker", "box-and-whisker"]),
];

fn kind_word(k: ChartKind) -> &'static str {
    KINDS.iter().find(|(x, _)| *x == k).map_or("column", |(_, w)| w[0])
}

fn num(s: &str, what: &str) -> Result<f64, String> {
    s.trim().trim_end_matches('%').parse::<f64>().ok().filter(|v| v.is_finite()).ok_or_else(|| format!("“{s}” isn't a number for {what}."))
}

/// The settings of one series as Format Chart reads them.
fn look_text(wb: &Workbook, here: u16, l: &SeriesLook) -> Vec<String> {
    let mut v = Vec::new();
    if l.labels {
        v.push("labels".to_string());
    }
    if l.secondary {
        v.push("secondary".into());
    }
    if let Some(t) = &l.trend {
        let mut s = match t.kind {
            TrendKind::Linear => "trend linear".to_string(),
            TrendKind::Exponential => "trend exponential".into(),
            TrendKind::Logarithmic => "trend log".into(),
            TrendKind::Polynomial(n) => format!("trend poly {n}"),
            TrendKind::Power => "trend power".into(),
            TrendKind::MovingAverage(n) => format!("trend moving {n}"),
        };
        if t.equation {
            s.push_str(" equation");
        }
        if t.r_squared {
            s.push_str(" r2");
        }
        if t.forward > 0.0 {
            s.push_str(&format!(" forward {}", t.forward));
        }
        if t.backward > 0.0 {
            s.push_str(&format!(" backward {}", t.backward));
        }
        v.push(s);
    }
    match l.errors {
        Some(ErrorBars::Fixed(x)) => v.push(format!("errors fixed {x}")),
        Some(ErrorBars::Percent(x)) => v.push(format!("errors percent {x}")),
        Some(ErrorBars::StdDev(x)) => v.push(format!("errors stddev {x}")),
        Some(ErrorBars::StdErr) => v.push("errors stderr".into()),
        None => {}
    }
    if let Some(r) = &l.sizes {
        let sheet = if r.sheet == here { String::new() } else { format!("{}!", crate::formulas::sheet_ref(wb, r.sheet)) };
        v.push(format!("sizes {sheet}{}", range_text(r)));
    }
    if !l.totals.is_empty() {
        let t: Vec<String> = l.totals.iter().map(|t| (u64::from(*t) + 1).to_string()).collect();
        v.push(format!("totals {}", t.join(" ")));
    }
    match l.bins {
        Some(Bins::Count(n)) => v.push(format!("bins {n}")),
        Some(Bins::Width(w)) => v.push(format!("bin width {w}")),
        None => {}
    }
    v
}

/// The bar's starting text: the chart's settings now.
pub fn start(d: &Doc, cursor: (u32, u32)) -> String {
    let Some(i) = chart_here(d, cursor) else { return "type column".into() };
    let c = &d.wb.charts()[i];
    let mut parts = vec![format!("type {}", kind_word(c.kind))];
    for (j, s) in c.series.iter().enumerate() {
        let mut v = look_text(&d.wb, d.sheet, &s.look);
        if let Some(k) = s.kind.filter(|k| *k != c.kind) {
            v.insert(0, kind_word(k).to_string());
        }
        if !v.is_empty() {
            parts.push(format!("series {}: {}", j + 1, v.join(", ")));
        }
    }
    parts.join("; ")
}

/// One setting applied to one series' look (and its own kind, for a combo).
fn apply(wb: &Workbook, here: u16, chart: ChartKind, kind: &mut Option<ChartKind>, l: &mut SeriesLook, setting: &str) -> Result<(), String> {
    let series_kind = kind.unwrap_or(chart);
    let low = setting.trim().to_ascii_lowercase();
    let words: Vec<&str> = low.split_whitespace().collect();
    match words.as_slice() {
        ["labels"] | ["data", "labels"] => l.labels = true,
        ["no", "labels"] => l.labels = false,
        ["secondary"] | ["secondary", "axis"] => l.secondary = true,
        ["primary"] | ["primary", "axis"] => l.secondary = false,
        ["no", "trend"] | ["no", "trendline"] => l.trend = None,
        ["no", "errors"] | ["no", "error", "bars"] => l.errors = None,
        ["no", "sizes"] => l.sizes = None,
        ["no", "totals"] => l.totals.clear(),
        ["bins", "auto"] | ["no", "bins"] => l.bins = None,
        ["trend" | "trendline", rest @ ..] => {
            let (kind, mut rest) = match rest {
                ["linear", r @ ..] => (TrendKind::Linear, r),
                ["exponential" | "exp", r @ ..] => (TrendKind::Exponential, r),
                ["log" | "logarithmic", r @ ..] => (TrendKind::Logarithmic, r),
                ["power", r @ ..] => (TrendKind::Power, r),
                ["poly" | "polynomial", n, r @ ..] => {
                    let n = num(n, "the polynomial's order")?;
                    if !(2.0..=6.0).contains(&n) || n.fract() != 0.0 {
                        return Err("A polynomial trendline's order is 2 to 6.".into());
                    }
                    (TrendKind::Polynomial(n as u8), r)
                }
                ["moving" | "average" | "moving-average", n, r @ ..] => {
                    let n = num(n, "the moving average's period")?;
                    if !(2.0..=255.0).contains(&n) || n.fract() != 0.0 {
                        return Err("A moving average's period is 2 to 255.".into());
                    }
                    (TrendKind::MovingAverage(n as u8), r)
                }
                _ => return Err("A trendline is linear, exponential, log, poly N, power or moving N.".into()),
            };
            let mut t = Trendline { kind, equation: false, r_squared: false, forward: 0.0, backward: 0.0 };
            while let Some((w, r)) = rest.split_first() {
                match (*w, r) {
                    ("equation", r) => (t.equation, rest) = (true, r),
                    ("r2" | "r²" | "rsquared", r) => (t.r_squared, rest) = (true, r),
                    ("forward" | "backward", [n, r @ ..]) => {
                        let v = num(n, "a trendline's periods")?;
                        if !(0.0..=1e6).contains(&v) {
                            return Err("A trendline extends 0 to 1,000,000 periods.".into());
                        }
                        if *w == "forward" { t.forward = v } else { t.backward = v }
                        rest = r;
                    }
                    _ => return Err(format!("“{w}” isn't a trendline option (equation, r2, forward N, backward N).")),
                }
            }
            if matches!(kind, TrendKind::MovingAverage(_)) && (t.equation || t.r_squared || t.forward > 0.0 || t.backward > 0.0) {
                return Err("A moving average has no equation and doesn't extend.".into());
            }
            if !matches!(series_kind, ChartKind::Column | ChartKind::Bar | ChartKind::Line | ChartKind::Area | ChartKind::Scatter | ChartKind::Bubble) {
                return Err("Trendlines go on column, bar, line, area, scatter and bubble series.".into());
            }
            l.trend = Some(t);
        }
        ["errors" | "error", rest @ ..] => {
            let rest = if rest.first() == Some(&"bars") { &rest[1..] } else { rest };
            let amount = |n: &str, what| -> Result<f64, String> {
                let v = num(n, what)?;
                if v < 0.0 { Err("Error amounts aren't negative.".into()) } else { Ok(v) }
            };
            l.errors = Some(match rest {
                ["fixed", n] => ErrorBars::Fixed(amount(n, "fixed error bars")?),
                ["percent" | "percentage", n] => ErrorBars::Percent(amount(n, "percent error bars")?),
                ["stddev" | "sd", n] => ErrorBars::StdDev(amount(n, "standard deviations")?),
                ["stddev" | "sd"] => ErrorBars::StdDev(1.0),
                ["stderr" | "se"] => ErrorBars::StdErr,
                _ => return Err("Error bars are fixed N, percent N, stddev N or stderr.".into()),
            });
        }
        ["sizes", _, ..] => {
            let at = setting.trim()[5..].trim();
            let r = crate::whatif::parse_area(wb, here, at)?;
            if r.r0 != r.r1 && r.c0 != r.c1 {
                return Err("Bubble sizes are one row or column.".into());
            }
            l.sizes = Some(r);
        }
        ["totals", rest @ ..] if !rest.is_empty() => {
            let mut t = Vec::new();
            for w in rest.iter().flat_map(|w| w.split(',')).filter(|w| !w.is_empty()) {
                let n: u32 = w.parse().ok().filter(|n| (1..=officina_grid::addr::MAX_ROWS).contains(n)).ok_or_else(|| format!("“{w}” isn't a point number (1 for the first)."))?;
                t.push(n - 1);
            }
            t.sort_unstable();
            t.dedup();
            if t.len() > MAX_TOTALS {
                return Err(format!("A waterfall marks at most {MAX_TOTALS} totals."));
            }
            l.totals = t;
        }
        ["bins", n] => {
            let n = num(n, "the bin count")?;
            if !(1.0..=10_000.0).contains(&n) || n.fract() != 0.0 {
                return Err("A histogram has 1 to 10,000 bins.".into());
            }
            l.bins = Some(Bins::Count(n as u32));
        }
        ["bin", "width", n] => {
            let w = num(n, "the bin width")?;
            if w <= 0.0 {
                return Err("A bin width is more than 0.".into());
            }
            l.bins = Some(Bins::Width(w));
        }
        [k] if KINDS.iter().any(|(_, ws)| ws.contains(k)) => {
            let k = KINDS.iter().find(|(_, ws)| ws.contains(k)).map(|(k, _)| *k).unwrap_or(chart);
            if k != chart && (k.whole() || chart.whole()) {
                return Err("Only column, bar, line, area, scatter and bubble series mix in one chart.".into());
            }
            *kind = (k != chart).then_some(k);
        }
        _ => return Err(format!("“{}” isn't a Format Chart setting.", setting.trim())),
    }
    Ok(())
}

/// Format Chart…: applies what was typed to the chart at the cursor. One step.
pub fn run(d: &mut Doc, cursor: (u32, u32), typed: &str) -> Result<String, String> {
    let i = chart_here(d, cursor).ok_or("Put the cursor on a chart, or on its data, to format it.")?;
    let mut c: Chart = d.wb.charts()[i].clone();
    let before = c.clone();
    for part in typed.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let low = part.to_ascii_lowercase();
        if let Some(k) = low.strip_prefix("type ").map(str::trim) {
            let k = KINDS.iter().find(|(_, ws)| ws.contains(&k)).map(|(k, _)| *k).ok_or_else(|| format!("“{k}” isn't a chart type (column, bar, line, area, scatter, pie, doughnut, radar, bubble, stock, waterfall, histogram, box)."))?;
            if c.kind != k {
                c.kind = k;
                for s in &mut c.series {
                    if s.kind.is_some_and(|x| x == k || x.whole() || k.whole()) {
                        s.kind = None;
                    }
                }
                if k.whole() {
                    c.grouping = officina_grid::charts::Grouping::Standard;
                }
            }
            continue;
        }
        // `series N:` scopes the settings to one series.
        let (which, settings) = match low.strip_prefix("series ").and_then(|r| r.split_once(':')) {
            Some((n, _)) => {
                let n: usize = n.trim().parse().ok().filter(|n| (1..=c.series.len()).contains(n)).ok_or_else(|| format!("The chart has series 1 to {}.", c.series.len()))?;
                (Some(n - 1), &part[part.find(':').map_or(0, |p| p + 1)..])
            }
            None => (None, part),
        };
        // `totals 3, 7`: the bare numbers after it are more points.
        let mut list: Vec<String> = Vec::new();
        for setting in settings.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match list.last_mut() {
                Some(last) if last.to_ascii_lowercase().starts_with("totals") && setting.bytes().all(|b| b.is_ascii_digit()) => {
                    last.push(' ');
                    last.push_str(setting);
                }
                _ => list.push(setting.to_string()),
            }
        }
        for setting in &list {
            for (j, s) in c.series.iter_mut().enumerate() {
                if which.is_some_and(|w| w != j) {
                    continue;
                }
                let mut kind = s.kind;
                apply(&d.wb, d.sheet, c.kind, &mut kind, &mut s.look, setting)?;
                s.kind = kind;
            }
        }
    }
    if c.kind == ChartKind::Stock && !(3..=4).contains(&c.series.len()) {
        return Err("A stock chart has three series (high, low, close) or four (open, high, low, close).".into());
    }
    if c.kind.round() && c.series.iter().any(|s| s.look.secondary) {
        return Err("A pie or doughnut has no secondary axis.".into());
    }
    if c == before {
        return Ok("The chart is already so.".into());
    }
    let mut charts = d.wb.charts().to_vec();
    charts[i] = c;
    d.status.clear();
    if !d.run_action("Format Chart", vec![GridOp::Charts(charts)]) {
        return Err(std::mem::take(&mut d.status));
    }
    Ok("The chart is formatted.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use officina_grid::addr::{CellAddr, Rect};
    use officina_grid::charts::{Anchor, ChartText, Grouping, Series};
    use officina_grid::Value;

    fn doc() -> Doc {
        let mut d = Doc::new("t");
        for r in 0..10u32 {
            d.wb.set_value(CellAddr::new(0, r, 0), Value::Num(f64::from(r) + 1.0));
            d.wb.set_value(CellAddr::new(0, r, 1), Value::Num(f64::from(r * r)));
            d.wb.set_value(CellAddr::new(0, r, 2), Value::Num(f64::from(10 - r)));
        }
        let col = |c: u32| Rect { sheet: 0, r0: 0, c0: c, r1: 9, c1: c };
        let ser = |c: u32| Series { name: Some(ChartText::Lit(format!("S{c}").into())), cats: None, vals: col(c), kind: None, look: Default::default() };
        d.wb.add_chart(Chart {
            kind: ChartKind::Column, grouping: Grouping::Standard, title: None, series: vec![ser(0), ser(1), ser(2)],
            legend: None, x_title: None, y_title: None, anchor: Anchor { sheet: 0, from: (0, 5, 0.0, 0.0), to: (10, 10, 0.0, 0.0) }, alt: None,
        }).unwrap();
        d
    }

    #[test]
    fn format_chart_sets_each_extra_and_reads_back() {
        let mut d = doc();
        let at = (2, 6);
        run(&mut d, at, "series 2: line, secondary, trend poly 3 equation r2 forward 2; series 1: labels, errors percent 5").unwrap();
        let c = &d.wb.charts()[0];
        assert_eq!(c.series[1].kind, Some(ChartKind::Line));
        assert!(c.series[1].look.secondary && c.series[0].look.labels && !c.series[2].look.labels);
        assert_eq!(c.series[1].look.trend, Some(Trendline { kind: TrendKind::Polynomial(3), equation: true, r_squared: true, forward: 2.0, backward: 0.0 }));
        assert_eq!(c.series[0].look.errors, Some(ErrorBars::Percent(5.0)));
        // The bar's text says what is set, and typing it again changes nothing.
        let now = start(&d, at);
        assert_eq!(now, "type column; series 1: labels, errors percent 5; series 2: line, secondary, trend poly 3 equation r2 forward 2");
        assert_eq!(run(&mut d, at, &now).unwrap(), "The chart is already so.");
        // One undo step takes it all back.
        d.undo();
        assert!(d.wb.charts()[0].series.iter().all(|s| s.look == SeriesLook::default() && s.kind.is_none()));
    }

    #[test]
    fn format_chart_changes_type_and_kind_specific_settings() {
        let mut d = doc();
        let at = (0, 0); // a cell of the chart's data
        run(&mut d, at, "type waterfall; series 1: totals 1, 10, labels").unwrap();
        let c = &d.wb.charts()[0];
        assert_eq!(c.kind, ChartKind::Waterfall);
        assert_eq!(c.series[0].look.totals, vec![0, 9]);
        run(&mut d, at, "type histogram; bins 4").unwrap();
        assert!(d.wb.charts()[0].series.iter().all(|s| s.look.bins == Some(Bins::Count(4))));
        run(&mut d, at, "type bubble; series 1: sizes C1:C10").unwrap();
        assert_eq!(d.wb.charts()[0].series[0].look.sizes, Some(Rect { sheet: 0, r0: 0, c0: 2, r1: 9, c1: 2 }));
        run(&mut d, at, "type stock").unwrap();
        assert_eq!(d.wb.charts()[0].kind, ChartKind::Stock);
        // Refusals change nothing and say why.
        let before = d.wb.charts().to_vec();
        for (typed, says) in [
            ("type pie; secondary", "no secondary axis"),
            ("trend poly 9", "order is 2 to 6"),
            ("series 4: labels", "series 1 to 3"),
            ("errors sideways", "Error bars are"),
            ("type sideways", "isn't a chart type"),
            ("type column; series 1: radar", "mix in one chart"),
            ("sparkle", "isn't a Format Chart setting"),
            ("type box; trend linear", "Trendlines go on"),
        ] {
            let e = run(&mut d, at, typed).unwrap_err();
            assert!(e.contains(says), "{typed}: {e}");
            assert_eq!(d.wb.charts(), &before[..], "{typed}");
        }
        assert!(run(&mut d, (40, 40), "labels").unwrap_err().contains("Put the cursor on a chart"));
    }
}
