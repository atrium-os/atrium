//! A chart laid out in a box as plain shapes and text ([`Prim`]), which a UI turns
//! into its own drawing nodes.
//!
//! Bounded at any size: a line of a million points becomes at most two points per
//! pixel column (its minimum and maximum there, in order), a scatter at most one dot
//! per pixel, bars at most one per pixel column; text is measured by the caller.

use std::collections::HashSet;
use std::f32::consts::PI;

use officina_grid::charts::{Chart, ChartKind, Grouping, LegendPos, SeriesData};

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
    // Legend entries: series, or a round chart's categories.
    let entries: Vec<(String, Rgba)> = if round {
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

fn axes(out: &mut Vec<Prim>, ch: &Chart, data: &[SeriesData], a: (f32, f32, f32, f32), st: &Style, measure: Measure) {
    let horizontal = data.iter().any(|d| d.kind == ChartKind::Bar) || (data.is_empty() && ch.kind == ChartKind::Bar);
    let scatter = ch.kind == ChartKind::Scatter;
    let percent = ch.grouping == Grouping::Percent;
    let stacked = ch.grouping != Grouping::Standard;
    let n = data.iter().map(|d| d.vals.len()).max().unwrap_or(0);
    // Value range.
    let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
    if stacked {
        for i in 0..n {
            let (mut pos, mut neg, mut abs) = (0.0, 0.0, 0.0);
            for d in data.iter().filter(|d| !matches!(d.kind, ChartKind::Scatter)) {
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
            lo = lo.min(neg);
            hi = hi.max(pos);
        }
    } else {
        for v in data.iter().flat_map(|d| d.vals.iter()).flatten() {
            lo = lo.min(*v);
            hi = hi.max(*v);
        }
    }
    let bars = data.iter().any(|d| matches!(d.kind, ChartKind::Column | ChartKind::Bar | ChartKind::Area));
    let (v0, v1, vstep) = scale(lo, hi, bars || stacked);
    let (x0, x1, xstep) = if scatter {
        let (mut l, mut h) = (f64::INFINITY, f64::NEG_INFINITY);
        for x in data.iter().flat_map(|d| d.xs.iter()).flatten() {
            l = l.min(*x);
            h = h.max(*x);
        }
        if !l.is_finite() {
            // No x numbers: 1, 2, 3 …
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
    let lh = LABEL * 1.3;
    // Titles take a line each.
    let mut p = a;
    if let Some(t) = &ch.y_title {
        if horizontal {
            out.push(Prim::Text { x: p.0 + p.2 / 2.0, y: p.1 + p.3 - lh, text: fit(t, p.2, LABEL, measure), size: LABEL, color: st.muted, align: Align::Middle, bold: true });
        } else {
            out.push(Prim::Text { x: p.0, y: p.1, text: fit(t, p.2, LABEL, measure), size: LABEL, color: st.muted, align: Align::Start, bold: true });
            p.1 += lh + 2.0;
        }
        if horizontal {
            p.3 -= lh + 2.0;
        } else {
            p.3 -= lh + 2.0;
        }
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
    // Category labels (x numbers for scatter).
    let cats: Vec<String> = if scatter {
        ticks(x0, x1, xstep).iter().map(|v| tick_text(*v, xstep, false)).collect()
    } else {
        let d = data.iter().find(|d| !d.cats.is_empty());
        (0..n).map(|i| d.and_then(|d| d.cats.get(i).cloned()).unwrap_or_else(|| (i + 1).to_string())).collect()
    };
    let cw = if horizontal { cats.iter().take(2000).map(|t| measure(t, LABEL)).fold(0.0, f32::max).min(p.2 * 0.3) } else { 0.0 };
    // The plot: room for value labels one side, category labels the other.
    let plot = if horizontal {
        (p.0 + cw + 6.0, p.1, (p.2 - cw - 6.0).max(1.0), (p.3 - lh - 4.0).max(1.0))
    } else {
        (p.0 + vlw + 6.0, p.1 + lh / 2.0, (p.2 - vlw - 6.0).max(1.0), (p.3 - lh - 4.0 - lh / 2.0).max(1.0))
    };
    let (px, py, pw, ph) = plot;
    // Value → px along the value axis.
    let vpos = |v: f64| -> f32 {
        let t = ((v - v0) / (v1 - v0)) as f32;
        if horizontal {
            px + t * pw
        } else {
            py + ph - t * ph
        }
    };
    // Gridlines and value labels.
    for (v, t) in vt.iter().zip(&vlabels) {
        let q = vpos(*v).round();
        if horizontal {
            out.push(Prim::Rect { x: q, y: py, w: 1.0, h: ph, color: st.grid });
            out.push(Prim::Text { x: q, y: py + ph + 4.0, text: t.clone(), size: LABEL, color: st.muted, align: Align::Middle, bold: false });
        } else {
            out.push(Prim::Rect { x: px, y: q, w: pw, h: 1.0, color: st.grid });
            out.push(Prim::Text { x: px - 6.0, y: q - lh / 2.0, text: t.clone(), size: LABEL, color: st.muted, align: Align::End, bold: false });
        }
    }
    // The category axis line, at zero.
    let z = vpos(0.0f64.clamp(v0, v1)).round();
    if horizontal {
        out.push(Prim::Rect { x: z, y: py, w: 1.0, h: ph, color: st.axis });
    } else {
        out.push(Prim::Rect { x: px, y: z, w: pw, h: 1.0, color: st.axis });
    }
    // Category px: the middle of slot i (first category at the left, or the bottom).
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
    // Category labels, thinned to fit.
    if scatter {
        let xt = ticks(x0, x1, xstep);
        for (v, t) in xt.iter().zip(&cats) {
            let q = (px + ((v - x0) / (x1 - x0)) as f32 * pw).round();
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
    // The series.
    let bar_series: Vec<usize> = (0..data.len()).filter(|&i| matches!(data[i].kind, ChartKind::Column | ChartKind::Bar)).collect();
    let m = if stacked { 1 } else { bar_series.len().max(1) };
    let bw = slot / (m as f32 + 1.5);
    // Running stack per category: (positive top, negative bottom), and absolute sums.
    let sums: Vec<f64> = (0..n).map(|i| data.iter().map(|d| d.vals.get(i).copied().flatten().unwrap_or(0.0).abs()).sum()).collect();
    let mut tops = vec![(0.0f64, 0.0f64); n];
    let norm = |i: usize, v: f64| if percent && sums[i] > 0.0 { v / sums[i] } else { v };
    // Bars: one per category, or per pixel when there are more categories than pixels.
    let stride = ((n as f32 / along).ceil() as usize).max(1);
    for (si, d) in data.iter().enumerate() {
        let col = st.series[si % st.series.len()];
        match d.kind {
            ChartKind::Column | ChartKind::Bar => {
                let k = bar_series.iter().position(|&b| b == si).unwrap_or(0);
                for i in (0..d.vals.len()).step_by(stride) {
                    let Some(v) = d.vals[i] else { continue };
                    let v = norm(i, v);
                    let (b0, b1) = if stacked {
                        let t = &mut tops[i];
                        if v >= 0.0 {
                            t.0 += v;
                            (t.0 - v, t.0)
                        } else {
                            t.1 += v;
                            (t.1 - v, t.1)
                        }
                    } else {
                        (0.0f64.clamp(v0, v1), v)
                    };
                    let (q0, q1) = (vpos(b0.clamp(v0, v1)), vpos(b1.clamp(v0, v1)));
                    let off = if stacked { -bw / 2.0 } else { -(m as f32) * bw / 2.0 + k as f32 * bw };
                    let c = cpos(i as f64) + off;
                    let thick = (bw * stride as f32).max(1.0);
                    if horizontal {
                        out.push(Prim::Rect { x: q0.min(q1), y: c - thick + bw, w: (q1 - q0).abs().max(1.0), h: thick, color: col });
                    } else {
                        out.push(Prim::Rect { x: c, y: q0.min(q1), w: thick, h: (q1 - q0).abs().max(1.0), color: col });
                    }
                }
            }
            ChartKind::Line | ChartKind::Area => {
                let mut base = vec![0.0f64; d.vals.len()];
                let vals: Vec<Option<f64>> = d
                    .vals
                    .iter()
                    .enumerate()
                    .map(|(i, v)| {
                        let v = v.map(|v| norm(i.min(n.saturating_sub(1)), v));
                        if stacked && i < n {
                            base[i] = tops[i].0;
                            let s = tops[i].0 + v.unwrap_or(0.0);
                            tops[i].0 = s;
                            Some(s)
                        } else {
                            v
                        }
                    })
                    .collect();
                let pts = |vals: &[Option<f64>]| -> Vec<Vec<(f32, f32)>> { decimate(vals, &|i| cpos(i as f64), &|v| vpos(v.clamp(v0, v1)), horizontal) };
                if d.kind == ChartKind::Area {
                    let top: Vec<(f32, f32)> = pts(&vals).into_iter().flatten().collect();
                    let bottom: Vec<(f32, f32)> = pts(&base.iter().map(|b| Some(*b)).collect::<Vec<_>>()).into_iter().flatten().rev().collect();
                    if top.len() >= 2 {
                        let mut poly = top;
                        if stacked && bottom.len() >= 2 {
                            poly.extend(bottom);
                        } else {
                            let (f, l) = (poly[0], poly[poly.len() - 1]);
                            if horizontal {
                                poly.push((z, l.1));
                                poly.push((z, f.1));
                            } else {
                                poly.push((l.0, z));
                                poly.push((f.0, z));
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
            ChartKind::Scatter => {
                let mut seen = HashSet::new();
                for (i, v) in d.vals.iter().enumerate() {
                    let Some(v) = v else { continue };
                    let xv = if d.xs.is_empty() { Some(i as f64 + 1.0) } else { d.xs.get(i).copied().flatten() };
                    let Some(xv) = xv else { continue };
                    let x = px + ((xv - x0) / (x1 - x0)) as f32 * pw;
                    let y = vpos(v.clamp(v0, v1));
                    // One dot per pixel.
                    if seen.insert((x as i32, y as i32)) {
                        out.push(Prim::Dot { x, y, r: 2.5, color: col });
                    }
                }
            }
            ChartKind::Pie | ChartKind::Doughnut => {}
        }
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
            series: vec![Series { name: None, cats: None, vals: r, kind: None }],
            legend: Some(LegendPos::Right),
            x_title: Some("x".into()),
            y_title: Some("y".into()),
            anchor: Anchor { sheet: 0, from: (0, 0, 0.0, 0.0), to: (1, 1, 0.0, 0.0) },
        }
    }

    fn sd(kind: ChartKind, vals: Vec<Option<f64>>) -> SeriesData {
        SeriesData { name: "S".into(), cats: (0..vals.len()).map(|i| format!("c{i}")).collect(), xs: Vec::new(), vals, kind }
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
}
