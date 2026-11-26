//! A chart laid out in a box as plain shapes and text ([`Prim`]), which a UI turns
//! into its own drawing nodes.
//!
//! Bounded at any size: a line of a million points becomes at most two points per
//! pixel column (its minimum and maximum there, in order), a scatter at most one dot
//! per pixel, bars at most one per pixel column; text is measured by the caller.

use std::collections::HashSet;
use std::f32::consts::PI;

use officina_grid::charts::{Bins, Chart, ChartKind, ErrorBars, Grouping, LegendPos, SeriesData, TrendKind};

pub type Rgba = [f32; 4];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Start,
    Middle,
    End,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Prim {
    Rect { x: f32, y: f32, w: f32, h: f32, color: Rgba },
    /// A polyline, `width` px.
    Line { pts: Vec<(f32, f32)>, width: f32, color: Rgba },
    /// A filled polygon.
    Area { pts: Vec<(f32, f32)>, color: Rgba },
    /// A ring sector (a pie slice when `r_in` is 0), angles in radians clockwise
    /// from 12 o'clock.
    Wedge { cx: f32, cy: f32, r_in: f32, r_out: f32, a0: f32, a1: f32, color: Rgba },
    Dot { x: f32, y: f32, r: f32, color: Rgba },
    /// Text whose top-left (or top-middle, top-right) is at (x, y).
    Text { x: f32, y: f32, text: String, size: f32, color: Rgba, align: Align, bold: bool },
}

#[derive(Debug, Clone)]
pub struct Style {
    pub background: Rgba,
    pub text: Rgba,
    pub muted: Rgba,
    pub grid: Rgba,
    pub axis: Rgba,
    pub series: Vec<Rgba>,
}

impl Default for Style {
    fn default() -> Self {
        let c = |r: u8, g: u8, b: u8| [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0];
        Style {
            background: c(255, 255, 255),
            text: c(32, 33, 36),
            muted: c(95, 99, 104),
            grid: c(225, 227, 230),
            axis: c(150, 154, 160),
            // Excel's Office theme accents, in order.
            series: vec![c(68, 114, 196), c(237, 125, 49), c(165, 165, 165), c(255, 192, 0), c(91, 155, 213), c(112, 173, 71), c(38, 68, 120), c(158, 72, 14)],
        }
    }
}

const PAD: f32 = 8.0;
const TITLE: f32 = 14.0;
const LABEL: f32 = 10.0;

/// Measures text: (text, size px) → width px.
pub type Measure<'a> = &'a dyn Fn(&str, f32) -> f32;

/// `ch` (its `data` and `title` already read from the sheet) laid out in a box `w` ×
/// `h` px with its origin at (0, 0).
pub fn layout(ch: &Chart, data: &[SeriesData], title: Option<&str>, w: f32, h: f32, st: &Style, measure: Measure) -> Vec<Prim> {
    let mut out = vec![Prim::Rect { x: 0.0, y: 0.0, w, h, color: st.background }];
    if w < 2.0 * PAD || h < 2.0 * PAD {
        return out;
    }
    let color = |i: usize| st.series[i % st.series.len()];
    let mut area = (PAD, PAD, w - 2.0 * PAD, h - 2.0 * PAD);
    if let Some(t) = title.filter(|t| !t.is_empty()) {
        let t = fit(t, area.2, TITLE, measure);
        out.push(Prim::Text { x: w / 2.0, y: area.1, text: t, size: TITLE, color: st.text, align: Align::Middle, bold: true });
        area.1 += TITLE * 1.5;
        area.3 -= TITLE * 1.5;
    }
    let round = ch.kind.round();
    // A histogram draws its first series' bins as touching columns.
    let binned: Vec<SeriesData>;
    let data = if ch.kind == ChartKind::Histogram {
        binned = data.first().map(|d| {
            let bins = histogram_bins(d);
            SeriesData { cats: bins.iter().map(|b| b.0.clone()).collect(), vals: bins.iter().map(|b| Some(b.1)).collect(), kind: ChartKind::Histogram, xs: Vec::new(), sizes: Vec::new(), ..d.clone() }
        }).into_iter().collect();
        &binned[..]
    } else {
        data
    };
    // Legend entries: series, or a round chart's categories.
    let entries: Vec<(String, Rgba)> = if ch.kind == ChartKind::Waterfall {
        ["Increase", "Decrease", "Total"].iter().enumerate().map(|(i, t)| (t.to_string(), color(i))).collect()
    } else if round {
        data.first().map(|d| (0..d.vals.len().min(64)).map(|i| (d.cats.get(i).cloned().unwrap_or_else(|| (i + 1).to_string()), color(i))).collect()).unwrap_or_default()
    } else {
        data.iter().take(64).enumerate().map(|(i, d)| (d.name.clone(), color(i))).collect()
    };
    if let Some(pos) = ch.legend.filter(|_| !entries.is_empty()) {
        area = legend(&mut out, &entries, pos, area, st, measure);
    }
    if area.2 < 4.0 || area.3 < 4.0 {
        return out;
    }
    if round {
        pie(&mut out, ch, data.first(), area, st);
    } else if ch.kind == ChartKind::Radar {
        radar(&mut out, data, area, st, measure);
    } else {
        axes(&mut out, ch, data, area, st, measure);
    }
    out
}

/// `t` cut with "…" to fit `w` px.
fn fit(t: &str, w: f32, size: f32, measure: Measure) -> String {
    if measure(t, size) <= w {
        return t.to_string();
    }
    let mut s: String = t.chars().take(200).collect();
    while !s.is_empty() && measure(&format!("{s}…"), size) > w {
        s.pop();
    }
    format!("{s}…")
}

/// Draws the legend and returns the area left for the plot.
fn legend(out: &mut Vec<Prim>, entries: &[(String, Rgba)], pos: LegendPos, a: (f32, f32, f32, f32), st: &Style, measure: Measure) -> (f32, f32, f32, f32) {
    let sw = LABEL * 0.9;
    let line = LABEL * 1.6;
    match pos {
        LegendPos::Right | LegendPos::Left => {
            let tw = entries.iter().map(|(t, _)| measure(t, LABEL)).fold(0.0, f32::max).min(a.2 * 0.35);
            let lw = sw + 6.0 + tw;
            let n = ((a.3 / line) as usize).min(entries.len());
            let y0 = a.1 + (a.3 - n as f32 * line) / 2.0;
            let x0 = if pos == LegendPos::Right { a.0 + a.2 - lw } else { a.0 };
            for (i, (t, c)) in entries.iter().take(n).enumerate() {
                let y = y0 + i as f32 * line;
                out.push(Prim::Rect { x: x0, y: y + (line - sw) / 2.0, w: sw, h: sw, color: *c });
                out.push(Prim::Text { x: x0 + sw + 6.0, y: y + (line - LABEL * 1.3) / 2.0, text: fit(t, tw, LABEL, measure), size: LABEL, color: st.text, align: Align::Start, bold: false });
            }
            if pos == LegendPos::Right {
                (a.0, a.1, a.2 - lw - PAD, a.3)
            } else {
                (a.0 + lw + PAD, a.1, a.2 - lw - PAD, a.3)
            }
        }
        LegendPos::Top | LegendPos::Bottom => {
            let widths: Vec<f32> = entries.iter().map(|(t, _)| sw + 6.0 + measure(t, LABEL).min(a.2 * 0.5) + 12.0).collect();
            // As many entries as fit on one line.
            let mut n = 0;
            let mut total = 0.0;
            while n < widths.len() && total + widths[n] <= a.2 {
                total += widths[n];
                n += 1;
            }
            let y = if pos == LegendPos::Top { a.1 } else { a.1 + a.3 - line };
            let mut x = a.0 + (a.2 - total) / 2.0;
            for ((t, c), wd) in entries.iter().zip(&widths).take(n) {
                out.push(Prim::Rect { x, y: y + (line - sw) / 2.0, w: sw, h: sw, color: *c });
                out.push(Prim::Text { x: x + sw + 6.0, y: y + (line - LABEL * 1.3) / 2.0, text: fit(t, wd - sw - 18.0, LABEL, measure), size: LABEL, color: st.text, align: Align::Start, bold: false });
                x += wd;
            }
            if pos == LegendPos::Top {
                (a.0, a.1 + line + 4.0, a.2, a.3 - line - 4.0)
            } else {
                (a.0, a.1, a.2, a.3 - line - 4.0)
            }
        }
    }
}

fn pie(out: &mut Vec<Prim>, ch: &Chart, d: Option<&SeriesData>, a: (f32, f32, f32, f32), st: &Style) {
    let Some(d) = d else { return };
    let vals: Vec<f32> = d.vals.iter().map(|v| v.map_or(0.0, |x| x.abs() as f32)).collect();
    let total: f32 = vals.iter().sum();
    let r = a.2.min(a.3) / 2.0 * 0.95;
    let (cx, cy) = (a.0 + a.2 / 2.0, a.1 + a.3 / 2.0);
    let r_in = if ch.kind == ChartKind::Doughnut { r * 0.5 } else { 0.0 };
    if !(total > 0.0) || !total.is_finite() {
        out.push(Prim::Wedge { cx, cy, r_in, r_out: r, a0: 0.0, a1: 2.0 * PI, color: st.grid });
        return;
    }
    let mut a0 = 0.0f32;
    // Slices thinner than a tenth of a degree are merged into the next drawn one.
    let mut pending = 0.0f32;
    for (i, v) in vals.iter().enumerate() {
        let sweep = v / total * 2.0 * PI + pending;
        if sweep < 0.0017 && i + 1 < vals.len() {
            pending = sweep;
            continue;
        }
        pending = 0.0;
        if sweep > 0.0 {
            out.push(Prim::Wedge { cx, cy, r_in, r_out: r, a0, a1: a0 + sweep, color: st.series[i % st.series.len()] });
        }
        a0 += sweep;
    }
}

/// A round step near `span / n`: 1, 2 or 5 times a power of ten.
fn nice_step(span: f64, n: f64) -> f64 {
    let raw = (span / n).max(f64::MIN_POSITIVE);
    let p = 10f64.powf(raw.log10().floor());
    let m = raw / p;
    p * if m <= 1.0 {
        1.0
    } else if m <= 2.0 {
        2.0
    } else if m <= 5.0 {
        5.0
    } else {
        10.0
    }
}

/// Axis bounds and step for values in `lo..=hi`.
fn scale(lo: f64, hi: f64, include_zero: bool) -> (f64, f64, f64) {
    let (mut lo, mut hi) = if lo.is_finite() && hi.is_finite() && lo <= hi { (lo, hi) } else { (0.0, 1.0) };
    if include_zero {
        lo = lo.min(0.0);
        hi = hi.max(0.0);
    }
    if hi - lo < 1e-300 {
        let d = if hi == 0.0 { 1.0 } else { hi.abs() * 0.1 };
        lo -= d;
        hi += d;
    }
    let step = nice_step(hi - lo, 5.0);
    ((lo / step).floor() * step, (hi / step).ceil() * step, step)
}

/// An axis label: as few decimals as the step needs.
fn tick_text(v: f64, step: f64, percent: bool) -> String {
    let v = if percent { v * 100.0 } else { v };
    let step = if percent { step * 100.0 } else { step };
    if v.abs() >= 1e15 || (v != 0.0 && v.abs() < 1e-9) {
        return format!("{v:.1E}");
    }
    let dec = (-step.log10().floor()).clamp(0.0, 10.0) as usize;
    let mut s = format!("{:.*}", dec, if v.abs() < step * 1e-9 { 0.0 } else { v });
    if percent {
        s.push('%');
    }
    s
}

/// A trendline fitted to (x, y): its terms and R².
#[derive(Debug, Clone, PartialEq)]
pub struct Fit {
    pub kind: TrendKind,
    /// Linear `[a, b]` for a + bx; polynomial `[c0, c1, …]`; exponential
    /// and power `[a, b]` for a·eᵇˣ and a·xᵇ; logarithmic `[a, b]` for
    /// a + b·ln x. A moving average keeps the points it made.
    pub coef: Vec<f64>,
    pub r2: f64,
    pub moving: Vec<(f64, f64)>,
}

/// Solves `m·x = v` by Gaussian elimination with partial pivoting.
fn solve(mut m: Vec<Vec<f64>>, mut v: Vec<f64>) -> Option<Vec<f64>> {
    let n = v.len();
    for c in 0..n {
        let p = (c..n).max_by(|&a, &b| m[a][c].abs().total_cmp(&m[b][c].abs()))?;
        if m[p][c].abs() < 1e-300 {
            return None;
        }
        m.swap(c, p);
        v.swap(c, p);
        for r in c + 1..n {
            let f = m[r][c] / m[c][c];
            for k in c..n {
                m[r][k] -= f * m[c][k];
            }
            v[r] -= f * v[c];
        }
    }
    let mut x = vec![0.0; n];
    for r in (0..n).rev() {
        x[r] = (v[r] - (r + 1..n).map(|k| m[r][k] * x[k]).sum::<f64>()) / m[r][r];
    }
    x.iter().all(|v| v.is_finite()).then_some(x)
}

/// Least squares of y on powers 0..=order of x, with its R².
fn poly(xs: &[f64], ys: &[f64], order: usize) -> Option<(Vec<f64>, f64)> {
    let k = order + 1;
    if xs.len() < k {
        return None;
    }
    let mut m = vec![vec![0.0; k]; k];
    let mut v = vec![0.0; k];
    for (&x, &y) in xs.iter().zip(ys) {
        let p: Vec<f64> = (0..k).map(|i| x.powi(i as i32)).collect();
        for i in 0..k {
            v[i] += p[i] * y;
            for j in 0..k {
                m[i][j] += p[i] * p[j];
            }
        }
    }
    let c = solve(m, v)?;
    let mean = ys.iter().sum::<f64>() / ys.len() as f64;
    let at = |x: f64| c.iter().enumerate().map(|(i, c)| c * x.powi(i as i32)).sum::<f64>();
    let ss_res: f64 = xs.iter().zip(ys).map(|(&x, &y)| (y - at(x)).powi(2)).sum();
    let ss_tot: f64 = ys.iter().map(|y| (y - mean).powi(2)).sum();
    Some((c, if ss_tot > 0.0 { 1.0 - ss_res / ss_tot } else { 1.0 }))
}

/// Excel's trendline of `kind` through the points, `None` when it can't
/// be fitted (too few points, or x or y ≤ 0 where a log is needed).
pub fn trend_fit(kind: TrendKind, pts: &[(f64, f64)]) -> Option<Fit> {
    let xs: Vec<f64> = pts.iter().map(|p| p.0).collect();
    let ys: Vec<f64> = pts.iter().map(|p| p.1).collect();
    let pos = |v: &[f64]| v.iter().all(|&x| x > 0.0);
    let ln = |v: &[f64]| v.iter().map(|x| x.ln()).collect::<Vec<_>>();
    let (coef, r2) = match kind {
        TrendKind::Linear => poly(&xs, &ys, 1)?,
        TrendKind::Polynomial(o) => poly(&xs, &ys, o as usize)?,
        TrendKind::Exponential if pos(&ys) => {
            let (c, r2) = poly(&xs, &ln(&ys), 1)?;
            (vec![c[0].exp(), c[1]], r2)
        }
        TrendKind::Logarithmic if pos(&xs) => poly(&ln(&xs), &ys, 1)?,
        TrendKind::Power if pos(&xs) && pos(&ys) => {
            let (c, r2) = poly(&ln(&xs), &ln(&ys), 1)?;
            (vec![c[0].exp(), c[1]], r2)
        }
        TrendKind::MovingAverage(p) => {
            let p = p as usize;
            if pts.len() < p {
                return None;
            }
            let moving = (p - 1..pts.len()).map(|i| (xs[i], ys[i + 1 - p..=i].iter().sum::<f64>() / p as f64)).collect();
            return Some(Fit { kind, coef: Vec::new(), r2: f64::NAN, moving });
        }
        _ => return None,
    };
    Some(Fit { kind, coef, r2, moving: Vec::new() })
}

impl Fit {
    pub fn at(&self, x: f64) -> f64 {
        let c = &self.coef;
        match self.kind {
            TrendKind::Linear | TrendKind::Polynomial(_) => c.iter().enumerate().map(|(i, c)| c * x.powi(i as i32)).sum(),
            TrendKind::Exponential => c[0] * (c[1] * x).exp(),
            TrendKind::Logarithmic => c[0] + c[1] * x.ln(),
            TrendKind::Power => c[0] * x.powf(c[1]),
            TrendKind::MovingAverage(_) => f64::NAN,
        }
    }

    /// The equation as Excel shows it, such as `y = 2.3x + 0.5`.
    pub fn equation(&self) -> String {
        let g = |v: f64| {
            let s = format!("{:.4}", v);
            let s = s.trim_end_matches('0').trim_end_matches('.').to_string();
            if s == "-0" { "0".to_string() } else { s }
        };
        let term = |c: f64, first: bool| if first { g(c) } else if c < 0.0 { format!(" - {}", g(-c)) } else { format!(" + {}", g(c)) };
        let c = &self.coef;
        match self.kind {
            TrendKind::Linear | TrendKind::Polynomial(_) => {
                let mut s = String::from("y = ");
                let mut first = true;
                for i in (0..c.len()).rev() {
                    let x = match i { 0 => String::new(), 1 => "x".into(), _ => format!("x{}", sup(i)) };
                    s.push_str(&term(c[i], first));
                    s.push_str(&x);
                    first = false;
                }
                s
            }
            TrendKind::Exponential => format!("y = {}e{}", g(c[0]), sup_text(&format!("{}x", g(c[1])))),
            TrendKind::Logarithmic => format!("y = {}ln(x){}", g(c[1]), term(c[0], false)),
            TrendKind::Power => format!("y = {}x{}", g(c[0]), sup_text(&g(c[1]))),
            TrendKind::MovingAverage(_) => String::new(),
        }
    }
}

fn sup(n: usize) -> String {
    sup_text(&n.to_string())
}

fn sup_text(s: &str) -> String {
    s.chars().map(|c| match c {
        '0' => '⁰', '1' => '¹', '2' => '²', '3' => '³', '4' => '⁴', '5' => '⁵', '6' => '⁶', '7' => '⁷', '8' => '⁸', '9' => '⁹',
        '-' => '⁻', '.' => '·', 'x' => 'ˣ', c => c,
    }).collect()
}

/// A data label's text: up to four decimals, trailing zeros dropped.
fn label_text(v: f64) -> String {
    if v.abs() >= 1e15 || (v != 0.0 && v.abs() < 1e-4) {
        return format!("{v:.2E}");
    }
    let s = format!("{v:.4}");
    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// Quartiles (Excel's exclusive method), whiskers, mean and outliers.
#[derive(Debug, Clone, PartialEq)]
pub struct BoxStats {
    pub q1: f64,
    pub median: f64,
    pub q3: f64,
    pub low: f64,
    pub high: f64,
    pub mean: f64,
    pub outliers: Vec<f64>,
}

pub fn box_stats(vals: &[Option<f64>]) -> Option<BoxStats> {
    let mut v: Vec<f64> = vals.iter().flatten().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let n = v.len();
    // QUARTILE.EXC's rank p(n + 1), falling back to the inclusive one
    // when that lands outside the data.
    let q = |p: f64| {
        let r = p * (n as f64 + 1.0);
        let r = if r < 1.0 || r > n as f64 { 1.0 + p * (n as f64 - 1.0) } else { r };
        let i = (r.floor() as usize).clamp(1, n);
        let f = r - i as f64;
        v[i - 1] + f * (v[i.min(n - 1)] - v[i - 1])
    };
    let (q1, median, q3) = (q(0.25), q(0.5), q(0.75));
    let iqr = q3 - q1;
    let (lo_fence, hi_fence) = (q1 - 1.5 * iqr, q3 + 1.5 * iqr);
    let inside: Vec<f64> = v.iter().copied().filter(|x| (lo_fence..=hi_fence).contains(x)).collect();
    let low = inside.first().copied().unwrap_or(q1);
    let high = inside.last().copied().unwrap_or(q3);
    let mean = v.iter().sum::<f64>() / n as f64;
    let outliers = v.iter().copied().filter(|x| !(lo_fence..=hi_fence).contains(x)).take(1000).collect();
    Some(BoxStats { q1, median, q3, low, high, mean, outliers })
}

/// The half-width of a point's error bar, or for standard deviation the
/// bar's (low, high) around the series mean.
fn error_span(e: ErrorBars, v: f64, vals: &[Option<f64>]) -> (f64, f64) {
    let xs: Vec<f64> = vals.iter().flatten().copied().collect();
    let n = xs.len() as f64;
    let mean = if n > 0.0 { xs.iter().sum::<f64>() / n } else { 0.0 };
    let sd = if n > 1.0 { (xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt() } else { 0.0 };
    match e {
        ErrorBars::Fixed(d) => (v - d, v + d),
        ErrorBars::Percent(p) => (v - v.abs() * p / 100.0, v + v.abs() * p / 100.0),
        ErrorBars::StdDev(k) => (mean - k * sd, mean + k * sd),
        ErrorBars::StdErr => {
            let se = if n > 0.0 { sd / n.sqrt() } else { 0.0 };
            (v - se, v + se)
        }
    }
}

/// A waterfall's bars: (from, to) for each point, totals from 0.
fn waterfall(d: &SeriesData) -> Vec<Option<(f64, f64)>> {
    let mut run = 0.0;
    d.vals
        .iter()
        .enumerate()
        .map(|(i, v)| {
            let v = (*v)?;
            if d.look.totals.binary_search(&(i as u32)).is_ok() {
                run = v;
                Some((0.0, v))
            } else {
                let from = run;
                run += v;
                Some((from, run))
            }
        })
        .collect()
}

/// A histogram's bins of `d`'s values: (label, count), Excel's way (the
/// first bin closed, the rest open on the left; Scott's rule when auto).
pub fn histogram_bins(d: &SeriesData) -> Vec<(String, f64)> {
    let mut v: Vec<f64> = d.vals.iter().flatten().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return Vec::new();
    }
    v.sort_by(f64::total_cmp);
    let (lo, hi) = (v[0], v[v.len() - 1]);
    let n = v.len() as f64;
    let width = match d.look.bins {
        Some(Bins::Width(w)) => w,
        Some(Bins::Count(k)) => (hi - lo) / k as f64,
        None => {
            let mean = v.iter().sum::<f64>() / n;
            let sd = (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0).max(1.0)).sqrt();
            3.5 * sd / n.cbrt()
        }
    };
    let k = if hi > lo && width > 0.0 && width.is_finite() { (((hi - lo) / width).ceil() as usize).clamp(1, 10_000) } else { 1 };
    let width = if k == 1 && !(hi > lo) { 1.0 } else { width };
    let mut counts = vec![0.0; k];
    for &x in &v {
        let i = if x <= lo + width { 0 } else { (((x - lo) / width).ceil() as usize).saturating_sub(1).min(k - 1) };
        counts[i] += 1.0;
    }
    (0..k)
        .map(|i| {
            let (a, b) = (lo + width * i as f64, lo + width * (i + 1) as f64);
            (format!("{}{}, {}]", if i == 0 { '[' } else { '(' }, label_text(a), label_text(b)), counts[i])
        })
        .collect()
}

fn radar(out: &mut Vec<Prim>, data: &[SeriesData], a: (f32, f32, f32, f32), st: &Style, measure: Measure) {
    let n = data.iter().map(|d| d.vals.len()).max().unwrap_or(0).min(360);
    if n == 0 {
        return;
    }
    let cats: Vec<String> = (0..n).map(|i| data.iter().find_map(|d| d.cats.get(i).cloned()).unwrap_or_else(|| (i + 1).to_string())).collect();
    let cw = cats.iter().map(|t| measure(t, LABEL)).fold(0.0, f32::max).min(a.2 * 0.2);
    let r = ((a.2 - 2.0 * cw - 12.0).min(a.3 - 2.0 * LABEL * 1.6) / 2.0).max(4.0);
    let (cx, cy) = (a.0 + a.2 / 2.0, a.1 + a.3 / 2.0);
    let (mut lo, mut hi) = (0.0f64, f64::NEG_INFINITY);
    for v in data.iter().flat_map(|d| d.vals.iter()).flatten() {
        lo = lo.min(*v);
        hi = hi.max(*v);
    }
    let (v0, v1, step) = scale(lo, hi, true);
    let at = |i: usize, v: f64| {
        let t = 2.0 * PI * i as f32 / n as f32;
        let k = (((v - v0) / (v1 - v0)) as f32).clamp(0.0, 1.0) * r;
        (cx + k * t.sin(), cy - k * t.cos())
    };
    let ticks = (((v1 - v0) / step).round() as usize).clamp(1, 20);
    for k in 1..=ticks {
        let v = v0 + step * k as f64;
        let mut ring: Vec<(f32, f32)> = (0..n).map(|i| at(i, v)).collect();
        ring.push(ring[0]);
        out.push(Prim::Line { pts: ring, width: 1.0, color: st.grid });
        let (x, y) = at(0, v);
        out.push(Prim::Text { x: x + 3.0, y: y - LABEL * 0.65, text: tick_text(v, step, false), size: LABEL, color: st.muted, align: Align::Start, bold: false });
    }
    for (i, cat) in cats.iter().enumerate() {
        out.push(Prim::Line { pts: vec![(cx, cy), at(i, v1)], width: 1.0, color: st.grid });
        let t = 2.0 * PI * i as f32 / n as f32;
        let (x, y) = (cx + (r + 6.0) * t.sin(), cy - (r + 6.0) * t.cos() - LABEL * 0.65);
        let align = if t.sin() > 0.2 { Align::Start } else if t.sin() < -0.2 { Align::End } else { Align::Middle };
        out.push(Prim::Text { x, y, text: fit(cat, cw, LABEL, measure), size: LABEL, color: st.muted, align, bold: false });
    }
    for (si, d) in data.iter().enumerate() {
        let col = st.series[si % st.series.len()];
        let mut ring: Vec<(f32, f32)> = d.vals.iter().take(n).enumerate().filter_map(|(i, v)| v.map(|v| at(i, v))).collect();
        if ring.len() >= 2 {
            ring.push(ring[0]);
            out.push(Prim::Line { pts: ring.clone(), width: 2.0, color: col });
        }
        for &(x, y) in ring.iter().take(n) {
            out.push(Prim::Dot { x, y, r: 2.5, color: col });
        }
    }
}

fn axes(out: &mut Vec<Prim>, ch: &Chart, data: &[SeriesData], a: (f32, f32, f32, f32), st: &Style, measure: Measure) {
    let kind = ch.kind;
    let boxes = kind == ChartKind::BoxWhisker;
    let stock = kind == ChartKind::Stock;
    let water = kind == ChartKind::Waterfall;
    let horizontal = data.iter().any(|d| d.kind == ChartKind::Bar) || (data.is_empty() && kind == ChartKind::Bar);
    let scatter = kind.xy();
    let whole = kind.whole();
    let percent = ch.grouping == Grouping::Percent && !whole;
    let stacked = ch.grouping != Grouping::Standard && !whole;
    // Series on the secondary axis: not in stacks, not on a horizontal chart.
    let sec = |d: &SeriesData| d.look.secondary && !whole && !horizontal;
    let any_sec = data.iter().any(sec);
    let stats: Vec<Option<BoxStats>> = if boxes { data.iter().map(|d| box_stats(&d.vals)).collect() } else { Vec::new() };
    let water_bars = if water { data.first().map(waterfall).unwrap_or_default() } else { Vec::new() };
    let n = if boxes { data.len() } else if water { water_bars.len() } else { data.iter().map(|d| d.vals.len()).max().unwrap_or(0) };
    // Value ranges: the primary axis, and the secondary.
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    let (mut lo2, mut hi2) = (f64::INFINITY, f64::NEG_INFINITY);
    let mut take = |secondary: bool, v: f64| {
        if !v.is_finite() {
            return;
        }
        let (l, h) = if secondary { (&mut lo2, &mut hi2) } else { (&mut lo, &mut hi) };
        *l = l.min(v);
        *h = h.max(v);
    };
    if boxes {
        for s in stats.iter().flatten() {
            take(false, s.low.min(s.outliers.iter().copied().fold(f64::INFINITY, f64::min)));
            take(false, s.high.max(s.outliers.iter().copied().fold(f64::NEG_INFINITY, f64::max)));
        }
    } else if water {
        for (f, t) in water_bars.iter().flatten() {
            take(false, *f);
            take(false, *t);
        }
    } else {
        if stacked {
            for i in 0..n {
                let (mut pos, mut neg, mut abs) = (0.0, 0.0, 0.0);
                for d in data.iter().filter(|d| !d.kind.xy() && !sec(d)) {
                    let v = d.vals.get(i).copied().flatten().unwrap_or(0.0);
                    if v >= 0.0 {
                        pos += v;
                    } else {
                        neg += v;
                    }
                    abs += v.abs();
                }
                if percent && abs > 0.0 {
                    pos /= abs;
                    neg /= abs;
                }
                take(false, neg);
                take(false, pos);
            }
        }
        for d in data {
            if stacked && !sec(d) && !d.kind.xy() {
                continue;
            }
            for v in d.vals.iter().flatten() {
                take(sec(d), *v);
                if let Some(e) = d.look.errors {
                    let (l, h) = error_span(e, *v, &d.vals);
                    take(sec(d), l);
                    take(sec(d), h);
                }
            }
        }
    }
    let bars = data.iter().any(|d| !sec(d) && matches!(d.kind, ChartKind::Column | ChartKind::Bar | ChartKind::Area | ChartKind::Histogram)) || water;
    let (v0, v1, vstep) = scale(lo, hi, bars || stacked);
    let bars2 = data.iter().any(|d| sec(d) && matches!(d.kind, ChartKind::Column | ChartKind::Bar | ChartKind::Area));
    let (s0, s1, sstep) = scale(lo2, hi2, bars2);
    let (x0, x1, xstep) = if scatter {
        let (mut l, mut h) = (f64::INFINITY, f64::NEG_INFINITY);
        for x in data.iter().flat_map(|d| d.xs.iter()).flatten() {
            l = l.min(*x);
            h = h.max(*x);
        }
        if !l.is_finite() {
            (l, h) = (1.0, n.max(1) as f64);
        }
        scale(l, h, false)
    } else {
        (0.0, 0.0, 1.0)
    };
    let ticks = |a: f64, b: f64, s: f64| -> Vec<f64> { (0..=(((b - a) / s).round() as i64).clamp(0, 50)).map(|k| a + k as f64 * s).collect() };
    let vt = ticks(v0, v1, vstep);
    let vlabels: Vec<String> = vt.iter().map(|v| tick_text(*v, vstep, percent)).collect();
    let vlw = vlabels.iter().map(|t| measure(t, LABEL)).fold(0.0, f32::max);
    let st_ticks = ticks(s0, s1, sstep);
    let slabels: Vec<String> = if any_sec { st_ticks.iter().map(|v| tick_text(*v, sstep, false)).collect() } else { Vec::new() };
    let slw = slabels.iter().map(|t| measure(t, LABEL)).fold(0.0, f32::max);
    let lh = LABEL * 1.3;
    let mut p = a;
    if let Some(t) = &ch.y_title {
        if horizontal {
            out.push(Prim::Text { x: p.0 + p.2 / 2.0, y: p.1 + p.3 - lh, text: fit(t, p.2, LABEL, measure), size: LABEL, color: st.muted, align: Align::Middle, bold: true });
        } else {
            out.push(Prim::Text { x: p.0, y: p.1, text: fit(t, p.2, LABEL, measure), size: LABEL, color: st.muted, align: Align::Start, bold: true });
            p.1 += lh + 2.0;
        }
        p.3 -= lh + 2.0;
    }
    if let Some(t) = &ch.x_title {
        if horizontal {
            out.push(Prim::Text { x: p.0, y: p.1, text: fit(t, p.2, LABEL, measure), size: LABEL, color: st.muted, align: Align::Start, bold: true });
            p.1 += lh + 2.0;
        } else {
            out.push(Prim::Text { x: p.0 + p.2 / 2.0, y: p.1 + p.3 - lh, text: fit(t, p.2, LABEL, measure), size: LABEL, color: st.muted, align: Align::Middle, bold: true });
        }
        p.3 -= lh + 2.0;
    }
    let cats: Vec<String> = if scatter {
        ticks(x0, x1, xstep).iter().map(|v| tick_text(*v, xstep, false)).collect()
    } else if boxes {
        data.iter().map(|d| d.name.clone()).collect()
    } else {
        let d = data.iter().find(|d| !d.cats.is_empty());
        (0..n).map(|i| d.and_then(|d| d.cats.get(i).cloned()).unwrap_or_else(|| (i + 1).to_string())).collect()
    };
    let cw = if horizontal { cats.iter().take(2000).map(|t| measure(t, LABEL)).fold(0.0, f32::max).min(p.2 * 0.3) } else { 0.0 };
    let right = if any_sec { slw + 6.0 } else { 0.0 };
    let plot = if horizontal {
        (p.0 + cw + 6.0, p.1, (p.2 - cw - 6.0).max(1.0), (p.3 - lh - 4.0).max(1.0))
    } else {
        (p.0 + vlw + 6.0, p.1 + lh / 2.0, (p.2 - vlw - 6.0 - right).max(1.0), (p.3 - lh - 4.0 - lh / 2.0).max(1.0))
    };
    let (px, py, pw, ph) = plot;
    // Value → px along the value axis, on the primary or secondary scale.
    let vpos = |secondary: bool, v: f64| -> f32 {
        let (a, b) = if secondary { (s0, s1) } else { (v0, v1) };
        let t = ((v.clamp(a, b) - a) / (b - a)) as f32;
        if horizontal {
            px + t * pw
        } else {
            py + ph - t * ph
        }
    };
    for (v, t) in vt.iter().zip(&vlabels) {
        let q = vpos(false, *v).round();
        if horizontal {
            out.push(Prim::Rect { x: q, y: py, w: 1.0, h: ph, color: st.grid });
            out.push(Prim::Text { x: q, y: py + ph + 4.0, text: t.clone(), size: LABEL, color: st.muted, align: Align::Middle, bold: false });
        } else {
            out.push(Prim::Rect { x: px, y: q, w: pw, h: 1.0, color: st.grid });
            out.push(Prim::Text { x: px - 6.0, y: q - lh / 2.0, text: t.clone(), size: LABEL, color: st.muted, align: Align::End, bold: false });
        }
    }
    for (v, t) in st_ticks.iter().zip(&slabels) {
        let q = vpos(true, *v).round();
        out.push(Prim::Text { x: px + pw + 6.0, y: q - lh / 2.0, text: t.clone(), size: LABEL, color: st.muted, align: Align::Start, bold: false });
    }
    let z = vpos(false, 0.0).round();
    if horizontal {
        out.push(Prim::Rect { x: z, y: py, w: 1.0, h: ph, color: st.axis });
    } else {
        out.push(Prim::Rect { x: px, y: z, w: pw, h: 1.0, color: st.axis });
    }
    let slots = if scatter { 1 } else { n.max(1) };
    let along = if horizontal { ph } else { pw };
    let slot = along / slots as f32;
    let cpos = |i: f64| -> f32 {
        if horizontal {
            py + ph - (i as f32 + 0.5) * slot
        } else {
            px + (i as f32 + 0.5) * slot
        }
    };
    let xpos = |x: f64| -> f32 { px + ((x - x0) / (x1 - x0)) as f32 * pw };
    // A point: (along the category axis, along the value axis) → (x, y).
    let pt = |c: f32, v: f32| if horizontal { (v, c) } else { (c, v) };
    if scatter {
        let xt = ticks(x0, x1, xstep);
        for (v, t) in xt.iter().zip(&cats) {
            let q = xpos(*v).round();
            out.push(Prim::Rect { x: q, y: py, w: 1.0, h: ph, color: st.grid });
            out.push(Prim::Text { x: q, y: py + ph + 4.0, text: t.clone(), size: LABEL, color: st.muted, align: Align::Middle, bold: false });
        }
    } else if n > 0 {
        let room = if horizontal { lh } else { cats.iter().take(2000).map(|t| measure(t, LABEL)).fold(0.0, f32::max) + 6.0 };
        let every = ((room / slot).ceil() as usize).max(1);
        for i in (0..n).step_by(every) {
            let c = cpos(i as f64);
            if horizontal {
                out.push(Prim::Text { x: px - 6.0, y: c - lh / 2.0, text: fit(&cats[i], cw, LABEL, measure), size: LABEL, color: st.muted, align: Align::End, bold: false });
            } else {
                out.push(Prim::Text { x: c, y: py + ph + 4.0, text: fit(&cats[i], slot * every as f32, LABEL, measure), size: LABEL, color: st.muted, align: Align::Middle, bold: false });
            }
        }
    }
    let bar_series: Vec<usize> = (0..data.len()).filter(|&i| matches!(data[i].kind, ChartKind::Column | ChartKind::Bar) && !(stacked && !sec(&data[i]))).collect();
    let m = if stacked { 1 + bar_series.len() } else { bar_series.len().max(1) };
    let bw = if matches!(kind, ChartKind::Histogram) { slot * 0.96 } else { slot / (m as f32 + 1.5) };
    let sums: Vec<f64> = (0..n).map(|i| data.iter().filter(|d| !sec(d)).map(|d| d.vals.get(i).copied().flatten().unwrap_or(0.0).abs()).sum()).collect();
    let mut tops = vec![(0.0f64, 0.0f64); n];
    let norm = |i: usize, v: f64| if percent && sums.get(i).is_some_and(|&s| s > 0.0) { v / sums[i] } else { v };
    let stride = ((n as f32 / along).ceil() as usize).max(1);
    let line = |out: &mut Vec<Prim>, pts: Vec<(f32, f32)>, width: f32, color: Rgba| out.push(Prim::Line { pts, width, color });
    let mut equations: Vec<(String, Rgba)> = Vec::new();
    if water {
        let colors = [st.series[0], st.series[1 % st.series.len()], st.series[2 % st.series.len()]];
        let w = slot * 0.6;
        for (i, b) in water_bars.iter().enumerate().step_by(stride) {
            let Some((f, t)) = *b else { continue };
            let total = data[0].look.totals.binary_search(&(i as u32)).is_ok();
            let col = if total { colors[2] } else if t >= f { colors[0] } else { colors[1] };
            let (q0, q1) = (vpos(false, f), vpos(false, t));
            let c = cpos(i as f64) - w / 2.0;
            let (x, y) = pt(c, q0.min(q1));
            out.push(if horizontal { Prim::Rect { x: q0.min(q1), y: c, w: (q1 - q0).abs().max(1.0), h: w, color: col } } else { Prim::Rect { x, y, w, h: (q1 - q0).abs().max(1.0), color: col } });
            if data[0].look.labels && stride == 1 {
                let v = data[0].vals[i].unwrap_or(0.0);
                let (lx, ly) = pt(cpos(i as f64), q0.min(q1) - LABEL * 1.4);
                out.push(Prim::Text { x: lx, y: ly.max(py), text: label_text(v), size: LABEL, color: st.text, align: Align::Middle, bold: false });
            }
        }
        return;
    }
    if stock {
        // High, low, close, or open, high, low, close.
        let ohlc = data.len() >= 4;
        let (o, h, l, c) = if ohlc { (Some(0), 1, 2, 3) } else { (None, 0, 1, 2.min(data.len().saturating_sub(1))) };
        if data.len() < 3 {
            return;
        }
        let at = |s: usize, i: usize| data[s].vals.get(i).copied().flatten();
        let w = slot * 0.5;
        for i in (0..n).step_by(stride) {
            let x = cpos(i as f64);
            if let (Some(hv), Some(lv)) = (at(h, i), at(l, i)) {
                line(out, vec![(x, vpos(false, hv)), (x, vpos(false, lv))], 1.0, st.text);
            }
            match (o.and_then(|o| at(o, i)), at(c, i)) {
                (Some(ov), Some(cv)) => {
                    let (q0, q1) = (vpos(false, ov), vpos(false, cv));
                    let up = cv >= ov;
                    out.push(Prim::Rect { x: x - w / 2.0, y: q0.min(q1), w, h: (q1 - q0).abs().max(1.0), color: if up { st.background } else { st.text } });
                    let (t, b) = (q0.min(q1), q0.max(q1).max(q0.min(q1) + 1.0));
                    line(out, vec![(x - w / 2.0, t), (x + w / 2.0, t), (x + w / 2.0, b), (x - w / 2.0, b), (x - w / 2.0, t)], 1.0, st.text);
                }
                (None, Some(cv)) => {
                    let y = vpos(false, cv);
                    line(out, vec![(x, y), (x + w / 2.0, y)], 2.0, st.text);
                }
                _ => {}
            }
        }
        return;
    }
    for (si, d) in data.iter().enumerate() {
        let col = st.series[si % st.series.len()];
        let s2 = sec(d);
        let vp = |v: f64| vpos(s2, v);
        // Each drawn point: (category or x px, the value it shows, where on the value axis it sits).
        let mut drawn: Vec<(f32, f64, f64)> = Vec::new();
        if boxes {
            let Some(b) = &stats[si] else { continue };
            let c = cpos(si as f64);
            let w = slot * 0.5;
            let (q1, q3) = (vp(b.q1), vp(b.q3));
            out.push(Prim::Rect { x: c - w / 2.0, y: q3.min(q1), w, h: (q1 - q3).abs().max(1.0), color: col });
            line(out, vec![(c - w / 2.0, vp(b.median)), (c + w / 2.0, vp(b.median))], 2.0, st.background);
            for (end, edge) in [(b.high, q3.min(q1)), (b.low, q3.max(q1))] {
                line(out, vec![(c, edge), (c, vp(end))], 1.0, st.text);
                line(out, vec![(c - w / 4.0, vp(end)), (c + w / 4.0, vp(end))], 1.0, st.text);
            }
            let (mx, my, k) = (c, vp(b.mean), 3.0);
            line(out, vec![(mx - k, my - k), (mx + k, my + k)], 1.5, st.text);
            line(out, vec![(mx - k, my + k), (mx + k, my - k)], 1.5, st.text);
            for o in &b.outliers {
                out.push(Prim::Dot { x: c, y: vp(*o), r: 2.5, color: col });
            }
            if d.look.labels {
                for v in [b.low, b.q1, b.median, b.q3, b.high] {
                    out.push(Prim::Text { x: c + w / 2.0 + 3.0, y: vp(v) - LABEL * 0.65, text: label_text(v), size: LABEL, color: st.text, align: Align::Start, bold: false });
                }
            }
            continue;
        }
        match d.kind {
            ChartKind::Column | ChartKind::Bar | ChartKind::Histogram => {
                let k = bar_series.iter().position(|&b| b == si).map_or(0, |k| k + usize::from(stacked));
                let stack = stacked && !s2;
                for i in (0..d.vals.len().min(n.max(d.vals.len()))).step_by(stride) {
                    let Some(v0r) = d.vals[i] else { continue };
                    let v = if s2 { v0r } else { norm(i, v0r) };
                    let (b0, b1) = if stack && i < tops.len() {
                        let t = &mut tops[i];
                        if v >= 0.0 {
                            t.0 += v;
                            (t.0 - v, t.0)
                        } else {
                            t.1 += v;
                            (t.1 - v, t.1)
                        }
                    } else {
                        (0.0, v)
                    };
                    let (q0, q1) = (vp(b0), vp(b1));
                    let off = if stack || kind == ChartKind::Histogram { -bw / 2.0 } else { -(m as f32) * bw / 2.0 + k as f32 * bw };
                    let c = cpos(i as f64) + off;
                    let thick = (bw * stride as f32).max(1.0);
                    if horizontal {
                        out.push(Prim::Rect { x: q0.min(q1), y: c - thick + bw, w: (q1 - q0).abs().max(1.0), h: thick, color: col });
                    } else {
                        out.push(Prim::Rect { x: c, y: q0.min(q1), w: thick, h: (q1 - q0).abs().max(1.0), color: col });
                    }
                    drawn.push((c + bw / 2.0, v0r, b1));
                }
            }
            ChartKind::Line | ChartKind::Area => {
                let stack = stacked && !s2;
                let mut base = vec![0.0f64; d.vals.len()];
                let vals: Vec<Option<f64>> = d
                    .vals
                    .iter()
                    .enumerate()
                    .map(|(i, v)| {
                        let v = v.map(|v| if s2 { v } else { norm(i.min(n.saturating_sub(1)), v) });
                        if stack && i < n {
                            base[i] = tops[i].0;
                            let s = tops[i].0 + v.unwrap_or(0.0);
                            tops[i].0 = s;
                            Some(s)
                        } else {
                            v
                        }
                    })
                    .collect();
                if stride == 1 {
                    for (i, (v, raw)) in vals.iter().zip(&d.vals).enumerate() {
                        if let (Some(v), Some(r)) = (v, raw) {
                            drawn.push((cpos(i as f64), *r, *v));
                        }
                    }
                }
                let pts = |vals: &[Option<f64>]| -> Vec<Vec<(f32, f32)>> { decimate(vals, &|i| cpos(i as f64), &|v| vp(v), horizontal) };
                if d.kind == ChartKind::Area {
                    let top: Vec<(f32, f32)> = pts(&vals).into_iter().flatten().collect();
                    let bottom: Vec<(f32, f32)> = pts(&base.iter().map(|b| Some(*b)).collect::<Vec<_>>()).into_iter().flatten().rev().collect();
                    if top.len() >= 2 {
                        let mut poly = top;
                        if stack && bottom.len() >= 2 {
                            poly.extend(bottom);
                        } else {
                            let zz = vp(0.0);
                            let (f, l) = (poly[0], poly[poly.len() - 1]);
                            if horizontal {
                                poly.push((zz, l.1));
                                poly.push((zz, f.1));
                            } else {
                                poly.push((l.0, zz));
                                poly.push((f.0, zz));
                            }
                        }
                        out.push(Prim::Area { pts: poly, color: col });
                    }
                } else {
                    for run in pts(&vals) {
                        if run.len() >= 2 {
                            out.push(Prim::Line { pts: run, width: 2.0, color: col });
                        } else if let Some(&(x, y)) = run.first() {
                            out.push(Prim::Dot { x, y, r: 2.0, color: col });
                        }
                    }
                }
            }
            ChartKind::Scatter | ChartKind::Bubble => {
                let mut seen = HashSet::new();
                let big = d.sizes.iter().flatten().fold(0.0f64, |m, s| m.max(s.abs()));
                let rmax = pw.min(ph) * 0.125;
                let bubble = d.kind == ChartKind::Bubble;
                let mut c = col;
                if bubble {
                    c[3] = 0.75;
                }
                for (i, v) in d.vals.iter().enumerate() {
                    let Some(v) = v else { continue };
                    let xv = if d.xs.is_empty() { Some(i as f64 + 1.0) } else { d.xs.get(i).copied().flatten() };
                    let Some(xv) = xv else { continue };
                    let (x, y) = (xpos(xv), vp(*v));
                    let r = if bubble {
                        let Some(s) = d.sizes.get(i).copied().flatten() else { continue };
                        if big > 0.0 { ((s.abs() / big).sqrt() as f32 * rmax).max(1.0) } else { 1.0 }
                    } else {
                        2.5
                    };
                    let r = r.min(x - px).min(px + pw - x).min(y - py).min(py + ph - y).max(0.5);
                    if bubble || seen.insert((x as i32, y as i32)) {
                        out.push(Prim::Dot { x, y, r, color: c });
                        if bubble || d.vals.len() <= 2000 {
                            drawn.push((x, *v, *v));
                        }
                    }
                }
            }
            _ => {}
        }
        // Error bars, then data labels, on the points just drawn.
        if let Some(e) = d.look.errors {
            for &(c, v, top) in drawn.iter().take(5000) {
                let (l, h) = error_span(e, v, &d.vals);
                let (l, h) = (top - v + l, top - v + h);
                let (a, b) = (vp(l), vp(h));
                line(out, vec![pt(c, a), pt(c, b)], 1.0, st.text);
                for q in [a, b] {
                    let (x, y) = pt(c, q);
                    line(out, if horizontal { vec![(x, y - 3.0), (x, y + 3.0)] } else { vec![(x - 3.0, y), (x + 3.0, y)] }, 1.0, st.text);
                }
            }
        }
        if d.look.labels && drawn.len() <= 500 {
            for &(c, v, top) in &drawn {
                let q = vp(top);
                let (x, y) = if horizontal { (q + 4.0, c - LABEL * 0.65) } else { (c, (q - LABEL * 1.4).clamp(py, py + ph - LABEL)) };
                out.push(Prim::Text { x, y, text: label_text(v), size: LABEL, color: st.text, align: if horizontal { Align::Start } else { Align::Middle }, bold: false });
            }
        }
        if let Some(t) = d.look.trend {
            let pts: Vec<(f64, f64)> = d.vals.iter().enumerate().filter_map(|(i, v)| {
                let x = if d.kind.xy() { if d.xs.is_empty() { Some(i as f64 + 1.0) } else { d.xs.get(i).copied().flatten() } } else { Some(i as f64 + 1.0) };
                Some((x?, (*v)?))
            }).collect();
            if let Some(f) = trend_fit(t.kind, &pts) {
                let to_px = |x: f64| if d.kind.xy() { xpos(x) } else { cpos(x - 1.0) };
                let mut run: Vec<(f32, f32)> = Vec::new();
                if f.moving.is_empty() {
                    let (a0, b0) = pts.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), p| (a.min(p.0), b.max(p.0)));
                    let (a0, b0) = (a0 - t.backward, b0 + t.forward);
                    for k in 0..=100 {
                        let x = a0 + (b0 - a0) * k as f64 / 100.0;
                        let y = f.at(x);
                        let c = to_px(x);
                        if y.is_finite() && (if horizontal { c >= py - 0.5 && c <= py + ph + 0.5 } else { c >= px - 0.5 && c <= px + pw + 0.5 }) {
                            run.push(pt(c, vp(y)));
                        }
                    }
                } else {
                    run = f.moving.iter().map(|&(x, y)| pt(to_px(x), vp(y))).collect();
                }
                if run.len() >= 2 {
                    let mut c = col;
                    c[3] = 0.8;
                    out.push(Prim::Line { pts: run, width: 1.5, color: c });
                }
                let mut parts = Vec::new();
                if t.equation && f.moving.is_empty() {
                    parts.push(f.equation());
                }
                if t.r_squared && f.r2.is_finite() {
                    parts.push(format!("R² = {:.4}", f.r2));
                }
                if !parts.is_empty() {
                    equations.push((parts.join("   "), col));
                }
            }
        }
    }
    for (k, (t, c)) in equations.iter().take(8).enumerate() {
        out.push(Prim::Text { x: px + pw - 4.0, y: py + 2.0 + k as f32 * LABEL * 1.3, text: fit(t, pw - 8.0, LABEL, measure), size: LABEL, color: *c, align: Align::End, bold: false });
    }
}

/// A line's points, split at gaps, with at most the first, minimum, maximum and last
/// point per pixel column of the category axis.
fn decimate(vals: &[Option<f64>], cat: &dyn Fn(usize) -> f32, val: &dyn Fn(f64) -> f32, horizontal: bool) -> Vec<Vec<(f32, f32)>> {
    let pt = |c: f32, v: f32| if horizontal { (v, c) } else { (c, v) };
    let mut runs: Vec<Vec<(f32, f32)>> = Vec::new();
    let mut run: Vec<(f32, f32)> = Vec::new();
    // The pixel column being gathered: its index and (first, min, max, last) values.
    let mut colpx: Option<(i64, f32, [(f32, usize); 4])> = None;
    let flush = |run: &mut Vec<(f32, f32)>, c: &Option<(i64, f32, [(f32, usize); 4])>| {
        if let Some((_, cx, v)) = c {
            let mut pts = v.to_vec();
            pts.sort_by_key(|p| p.1);
            pts.dedup_by_key(|p| p.1);
            for (y, _) in pts {
                run.push(pt(*cx, y));
            }
        }
    };
    for (i, v) in vals.iter().enumerate() {
        let Some(v) = v else {
            flush(&mut run, &colpx);
            colpx = None;
            if !run.is_empty() {
                runs.push(std::mem::take(&mut run));
            }
            continue;
        };
        let (c, y) = (cat(i), val(*v));
        let px = c.floor() as i64;
        match &mut colpx {
            Some((p, _, s)) if *p == px => {
                if y < s[1].0 {
                    s[1] = (y, i);
                }
                if y > s[2].0 {
                    s[2] = (y, i);
                }
                s[3] = (y, i);
            }
            _ => {
                flush(&mut run, &colpx);
                colpx = Some((px, c, [(y, i); 4]));
            }
        }
    }
    flush(&mut run, &colpx);
    if !run.is_empty() {
        runs.push(run);
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;
    use officina_grid::addr::Rect;
    use officina_grid::charts::{Anchor, Series};

    fn measure(t: &str, s: f32) -> f32 {
        t.chars().count() as f32 * s * 0.55
    }

    fn chart(kind: ChartKind, grouping: Grouping) -> Chart {
        let r = Rect { sheet: 0, r0: 0, c0: 0, r1: 0, c1: 0 };
        Chart {
            kind,
            grouping,
            title: None,
            series: vec![Series { name: None, cats: None, vals: r, kind: None, look: Default::default() }],
            legend: Some(LegendPos::Right),
            x_title: Some("x".into()),
            y_title: Some("y".into()),
            anchor: Anchor { sheet: 0, from: (0, 0, 0.0, 0.0), to: (1, 1, 0.0, 0.0) }, alt: None
        }
    }

    fn sd(kind: ChartKind, vals: Vec<Option<f64>>) -> SeriesData {
        SeriesData { name: "S".into(), cats: (0..vals.len()).map(|i| format!("c{i}")).collect(), xs: Vec::new(), vals, kind, sizes: Vec::new(), look: Default::default() }
    }

    fn inside(p: &[Prim], w: f32, h: f32) -> bool {
        let ok = |x: f32, y: f32| x >= -0.5 && y >= -0.5 && x <= w + 0.5 && y <= h + 0.5;
        p.iter().all(|p| match p {
            Prim::Rect { x, y, w: rw, h: rh, .. } => ok(*x, *y) && ok(x + rw, y + rh),
            Prim::Line { pts, .. } | Prim::Area { pts, .. } => pts.iter().all(|(x, y)| ok(*x, *y)),
            Prim::Dot { x, y, .. } => ok(*x, *y),
            Prim::Wedge { cx, cy, r_out, .. } => ok(cx - r_out, cy - r_out) && ok(cx + r_out, cy + r_out),
            Prim::Text { x, y, .. } => ok(*x, *y),
        })
    }

    #[test]
    fn every_kind_lays_out_inside_its_box() {
        let st = Style::default();
        for kind in ChartKind::ALL {
            for g in [Grouping::Standard, Grouping::Stacked, Grouping::Percent] {
                let mut d = vec![sd(kind, vec![Some(3.0), Some(-2.0), None, Some(7.5)]), sd(kind, vec![Some(1.0), Some(4.0), Some(2.0), Some(0.0)])];
                d[1].xs = vec![Some(1.0), Some(2.0), Some(3.0), Some(4.0)];
                d[0].xs = d[1].xs.clone();
                let p = layout(&chart(kind, g), &d, Some("A title that is rather long for the box"), 360.0, 220.0, &st, &measure);
                assert!(p.len() > 5, "{kind:?} {g:?}");
                assert!(inside(&p, 360.0, 220.0), "{kind:?} {g:?}: {p:?}");
            }
        }
    }

    #[test]
    fn a_million_points_stay_a_few_hundred_shapes() {
        let st = Style::default();
        let n = 1_000_000;
        let vals: Vec<Option<f64>> = (0..n).map(|i| Some(((i as f64) * 0.001).sin() * 100.0 + (i % 7) as f64)).collect();
        for kind in [ChartKind::Line, ChartKind::Column, ChartKind::Area, ChartKind::Scatter] {
            let mut d = sd(kind, vals.clone());
            d.cats.truncate(10);
            let t = std::time::Instant::now();
            let p = layout(&chart(kind, Grouping::Standard), &[d], None, 600.0, 300.0, &st, &measure);
            let points: usize = p.iter().map(|p| if let Prim::Line { pts, .. } | Prim::Area { pts, .. } = p { pts.len() } else { 1 }).sum();
            assert!(points < 4 * 600 + 200 || kind == ChartKind::Scatter && p.len() < 600 * 300, "{kind:?}: {points}");
            assert!(t.elapsed().as_millis() < 2000, "{kind:?} took {:?}", t.elapsed());
        }
    }

    #[test]
    fn pie_slices_go_round_once() {
        let st = Style::default();
        let d = sd(ChartKind::Pie, vec![Some(1.0), Some(2.0), Some(1.0), None, Some(1e-9)]);
        let p = layout(&chart(ChartKind::Pie, Grouping::Standard), &[d], None, 300.0, 200.0, &st, &measure);
        let sweep: f32 = p.iter().map(|p| if let Prim::Wedge { a0, a1, .. } = p { a1 - a0 } else { 0.0 }).sum();
        assert!((sweep - 2.0 * PI).abs() < 1e-3, "{sweep}");
        // Legend: one entry per category.
        assert_eq!(p.iter().filter(|p| matches!(p, Prim::Text { .. })).count(), 5);
    }

    #[test]
    fn ticks_are_round() {
        assert_eq!(scale(0.3, 9.7, true), (0.0, 10.0, 2.0));
        assert_eq!(tick_text(0.25, 0.05, false), "0.25");
        assert_eq!(tick_text(0.5, 0.1, true), "50%");
        assert_eq!(tick_text(2000.0, 500.0, false), "2000");
    }

    #[test]
    fn trendlines_fit_exactly_what_they_model() {
        let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
        let line: Vec<(f64, f64)> = (1..=8).map(|x| (x as f64, 3.0 + 2.0 * x as f64)).collect();
        let f = trend_fit(TrendKind::Linear, &line).unwrap();
        assert!(close(f.coef[0], 3.0) && close(f.coef[1], 2.0) && close(f.r2, 1.0), "{f:?}");
        let quad: Vec<(f64, f64)> = (0..9).map(|x| (x as f64, 1.0 - x as f64 + 0.5 * (x * x) as f64)).collect();
        let f = trend_fit(TrendKind::Polynomial(2), &quad).unwrap();
        assert!(close(f.coef[0], 1.0) && close(f.coef[1], -1.0) && close(f.coef[2], 0.5) && close(f.r2, 1.0), "{f:?}");
        let exp: Vec<(f64, f64)> = (0..6).map(|x| (x as f64, 2.0 * (0.3 * x as f64).exp())).collect();
        let f = trend_fit(TrendKind::Exponential, &exp).unwrap();
        assert!(close(f.coef[0], 2.0) && close(f.coef[1], 0.3), "{f:?}");
        let pow: Vec<(f64, f64)> = (1..7).map(|x| (x as f64, 4.0 * (x as f64).powf(1.5))).collect();
        let f = trend_fit(TrendKind::Power, &pow).unwrap();
        assert!(close(f.coef[0], 4.0) && close(f.coef[1], 1.5), "{f:?}");
        // Exponential needs positive values; a moving average its period of points.
        assert!(trend_fit(TrendKind::Exponential, &[(0.0, 1.0), (1.0, -1.0)]).is_none());
        let f = trend_fit(TrendKind::MovingAverage(3), &line).unwrap();
        assert_eq!(f.moving.len(), 6);
        assert!(close(f.moving[0].1, 7.0) && f.moving[0].0 == 3.0);
        assert!(trend_fit(TrendKind::MovingAverage(9), &line).is_none());
    }

    #[test]
    fn box_stats_are_excels_exclusive_quartiles() {
        let mut v: Vec<Option<f64>> = (1..=10).map(|x| Some(x as f64)).collect();
        v.push(Some(100.0));
        v.push(None);
        let b = box_stats(&v).unwrap();
        assert_eq!((b.q1, b.median, b.q3), (3.0, 6.0, 9.0));
        // 100 is past Q3 + 1.5 IQR (18): an outlier, and the whisker stops at 10.
        assert_eq!((b.low, b.high), (1.0, 10.0));
        assert_eq!(b.outliers, vec![100.0]);
        assert!((b.mean - 155.0 / 11.0).abs() < 1e-12);
        assert!(box_stats(&[None]).is_none());
    }

    #[test]
    fn waterfall_floats_from_the_running_total_and_totals_stand_on_the_axis() {
        let mut d = sd(ChartKind::Waterfall, vec![Some(10.0), Some(-3.0), Some(5.0), Some(12.0), None, Some(2.0)]);
        d.look.totals = vec![3];
        assert_eq!(waterfall(&d), vec![Some((0.0, 10.0)), Some((10.0, 7.0)), Some((7.0, 12.0)), Some((0.0, 12.0)), None, Some((12.0, 14.0))]);
    }

    #[test]
    fn histogram_bins_count_like_excel() {
        let mut d = sd(ChartKind::Histogram, [1.0, 2.0, 2.0, 3.0, 4.0, 5.0].iter().map(|x| Some(*x)).collect());
        d.look.bins = Some(Bins::Count(2));
        let b = histogram_bins(&d);
        // [1, 3] holds 1, 2, 2, 3; (3, 5] holds 4 and 5.
        assert_eq!(b, vec![("[1, 3]".to_string(), 4.0), ("(3, 5]".to_string(), 2.0)]);
        d.look.bins = Some(Bins::Width(1.0));
        let counts: Vec<f64> = histogram_bins(&d).into_iter().map(|b| b.1).collect();
        assert_eq!(counts, vec![3.0, 1.0, 1.0, 1.0]);
        // Error bars: a percentage of each value; standard deviation around the mean.
        let vals = [Some(2.0), Some(4.0), Some(6.0)];
        assert_eq!(error_span(ErrorBars::Percent(50.0), 4.0, &vals), (2.0, 6.0));
        assert_eq!(error_span(ErrorBars::StdDev(1.0), 6.0, &vals), (2.0, 6.0));
    }
}
