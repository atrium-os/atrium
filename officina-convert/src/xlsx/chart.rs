//! `.xlsx` charts: a sheet's drawing part (`xl/drawings/drawingN.xml`) anchors each
//! chart (`xl/charts/chartN.xml`, DrawingML charts) to cells.
//!
//! Read: column, bar, line, area, scatter, pie, doughnut, radar, bubble and stock
//! groups (3-D ones drawn flat), combos of them on the primary and secondary axes,
//! series names, categories, values and bubble sizes by reference, data labels,
//! trendlines and y error bars, titles, axis titles, the legend's place and
//! embedded raster pictures; and Excel 2016's chart extensions (`chartExN.xml`,
//! `cx:` markup) for waterfall (with totals), histogram (with bins) and box and
//! whisker. Other drawing objects are reported.

use std::fmt::Write as _;
use std::sync::Arc;

#[cfg(feature = "read")]
use quick_xml::events::Event;

use officina_grid::addr::{col_name, CellAddr, Rect};
use officina_grid::charts::{Anchor, Bins, Chart, ChartKind, ChartText, ErrorBars, Grouping, LegendPos, Series, SeriesLook, TrendKind};
#[cfg(feature = "read")]
use officina_grid::charts::MAX_TOTALS;
use officina_grid::Workbook;

#[cfg(feature = "read")]
use super::read::{attr, decode_escapes, external_rels, internal_rels, parse_ref, xml, XlsxError};
use crate::report::{Kind, Report};
use crate::zip::Archive;

/// EMU per point.
const EMU_PT: f64 = 12_700.0;
/// Text read from a chart part, at most.
const MAX_TEXT: usize = officina_grid::charts::MAX_TITLE;

#[cfg(feature = "read")]
/// `Sheet 1!$B$2:$B$9` → the range, on a sheet of `wb`.
fn range(wb: &Workbook, f: &str) -> Option<Rect> {
    let f = f.trim().trim_start_matches('=');
    let (sh, rf) = f.rsplit_once('!')?;
    let sh = sh.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')).map_or(sh.to_string(), |s| s.replace("''", "'"));
    let sheet = wb.sheet_id(&sh)?;
    let rf = rf.replace('$', "");
    let (r0, c0, r1, c1) = parse_ref(&rf).or_else(|| officina_grid::addr::parse_a1(&rf).map(|(r, c)| (r, c, r, c)))?;
    Some(Rect { sheet, r0, c0, r1, c1 })
}

#[cfg(feature = "read")]
/// Reads the charts of sheet `sheet` (its part is `part`).
pub(crate) fn read_sheet(wb: &mut Workbook, sheet: u16, z: &Archive, part: &str, rep: &mut Report) -> Result<(), XlsxError> {
    let mut drawings: Vec<String> = internal_rels(z, part)?.into_values().filter(|(t, _)| t.ends_with("/drawing")).map(|(_, p)| p).collect();
    drawings.sort();
    for d in drawings {
        let Some(data) = z.read_name(&d)? else { continue };
        let drels = internal_rels(z, &d)?;
        let external = external_rels(z, &d)?;
        let mut r = xml(&data);
        let mut buf = Vec::new();
        // The anchor being read: from/to (col, colOff, row, rowOff), which corner.
        let mut corner = 0usize;
        let mut field: Option<&'static str> = None;
        let mut pos = [[0i64; 4]; 2];
        let mut anchor_kind: &[u8] = b"";
        let mut picture = None;
        let mut link = None;
        let mut in_picture = false;
        let mut crop = [0; 4];
        let mut rotation = 0;
        // The chart part and whether it is a chart extension (`cx:`).
        let mut chart: Option<(String, bool)> = None;
        let mut descr: Option<String> = None;
        let mut other = false;
        // Inside `mc:Fallback`: the stand-in for a reader without the `mc:Choice`.
        let mut fallback = 0u32;
        loop {
            match r.read_event_into(&mut buf)? {
                Event::Start(e) if fallback > 0 => fallback += u32::from(e.local_name().as_ref() == b"Fallback"),
                Event::Empty(_) if fallback > 0 => {}
                Event::End(e) if fallback > 0 => fallback -= u32::from(e.local_name().as_ref() == b"Fallback"),
                Event::Start(e) if e.local_name().as_ref() == b"Fallback" => fallback = 1,
                Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                    b"twoCellAnchor" | b"oneCellAnchor" | b"absoluteAnchor" => {
                        anchor_kind = match e.local_name().as_ref() {
                            b"twoCellAnchor" => b"twoCellAnchor",
                            b"oneCellAnchor" => b"oneCellAnchor",
                            _ => b"absoluteAnchor",
                        };
                        picture = None;
                        link = None;
                        in_picture = false;
                        crop = [0; 4];
                        rotation = 0;
                        corner = 0;
                        field = None;
                        chart = None;
                        descr = None;
                        other = false;
                        pos = [[0; 4]; 2];
                    }
                    b"pos" if anchor_kind == b"absoluteAnchor" => {
                        pos[0][1] = attr(&e, b"x")?.and_then(|s| s.parse().ok()).unwrap_or(0);
                        pos[0][3] = attr(&e, b"y")?.and_then(|s| s.parse().ok()).unwrap_or(0);
                    }
                    b"ext" if !in_picture && matches!(anchor_kind, b"oneCellAnchor" | b"absoluteAnchor") => {
                        pos[1][1] = attr(&e, b"cx")?.and_then(|s| s.parse().ok()).unwrap_or(0);
                        pos[1][3] = attr(&e, b"cy")?.and_then(|s| s.parse().ok()).unwrap_or(0);
                    }
                    b"from" => corner = 0,
                    b"to" => corner = 1,
                    b"col" => field = Some("col"),
                    b"colOff" => field = Some("colOff"),
                    b"row" => field = Some("row"),
                    b"rowOff" => field = Some("rowOff"),
                    b"chart" if !anchor_kind.is_empty() => {
                        if let Some(id) = attr(&e, b"id")? {
                            chart = drels.get(&id).map(|(t, p)| (p.clone(), t.ends_with("/chartEx")));
                        }
                    }
                    b"pic" if !anchor_kind.is_empty() => in_picture = true,
                    b"cNvPr" if !anchor_kind.is_empty() => descr = attr(&e, b"descr")?,
                    b"srcRect" if in_picture => {
                        for (inset, side) in crop.iter_mut().zip([b"l", b"t", b"r", b"b"]) {
                            if let Some(value) = attr(&e, side)? {
                                *inset = if let Some(percent) = value.strip_suffix('%') {
                                    percent.parse::<f64>().ok().filter(|n| n.is_finite())
                                        .map_or(0, |n| (n * 1000.0).round() as i32)
                                } else {
                                    value.parse::<i32>().unwrap_or(0)
                                }.clamp(-1_000_000, 1_000_000);
                            }
                        }
                    }
                    b"xfrm" if in_picture => {
                        rotation = attr(&e, b"rot")?.and_then(|n| n.parse::<i32>().ok()).unwrap_or(0);
                    }
                    b"blip" if in_picture => {
                        if let Some(id) = attr(&e, b"embed")? {
                            picture = drels.get(&id).filter(|(ty, _)| ty.ends_with("/image")).map(|(_, p)| p.clone());
                        }
                        if let Some(id) = attr(&e, b"link")? {
                            let target = |relationships: &std::collections::HashMap<String, (String, String)>| {
                                relationships.get(&id).filter(|(ty, _)| ty.ends_with("/image")).map(|(_, p)| p.clone())
                            };
                            link = target(&external).map(Arc::from);
                            if picture.is_none() {
                                picture = target(&drels);
                            }
                        }
                    }
                    b"sp" | b"cxnSp" | b"grpSp" if !anchor_kind.is_empty() => other = true,
                    _ => {}
                },
                Event::Text(t) => {
                    if let Some(f) = field.take() {
                        let v: i64 = t.unescape().ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0);
                        let i = match f {
                            "col" => 0,
                            "colOff" => 1,
                            "row" => 2,
                            _ => 3,
                        };
                        pos[corner][i] = v;
                    }
                }
                Event::End(e) => match e.local_name().as_ref() {
                    b"col" | b"colOff" | b"row" | b"rowOff" => field = None,
                    b"pic" => {
                        in_picture = false;
                        if picture.is_none() && link.is_none() {
                            rep.note(Kind::Dropped, "pictures without a usable image relationship");
                        }
                    }
                    b"twoCellAnchor" | b"oneCellAnchor" | b"absoluteAnchor" => {
                        if anchor_kind != b"twoCellAnchor" {
                            pos[1][0] = pos[0][0];
                            pos[1][2] = pos[0][2];
                            pos[1][1] = pos[0][1].saturating_add(pos[1][1]);
                            pos[1][3] = pos[0][3].saturating_add(pos[1][3]);
                        }
                        anchor_kind = b"";
                        if other {
                            rep.note(Kind::Dropped, "drawing shapes");
                        }
                        let cl = |v: i64, max: u32| v.clamp(0, max as i64 - 1) as u32;
                        let off = |v: i64| (v as f64 / EMU_PT).clamp(0.0, 1e6) as f32;
                        let corner = |p: [i64; 4]| (cl(p[2], officina_grid::addr::MAX_ROWS), cl(p[0], officina_grid::addr::MAX_COLS), off(p[1]), off(p[3]));
                        let anchor = Anchor { sheet, from: corner(pos[0]), to: corner(pos[1]) };
                        if !other && (picture.is_some() || link.is_some()) {
                            let bytes = match picture.take().and_then(|p| z.find(&p)) {
                                Some(entry) if usize::try_from(entry.size).ok().is_some_and(|size| wb.can_add_picture(size)) => Some(z.read(entry)?),
                                _ => None,
                            };
                            let bytes = bytes.filter(|b| officina_grid::charts::Picture::extension(b).is_some());
                            if wb.add_picture(officina_grid::charts::Picture {
                                anchor, bytes: Arc::from(bytes.unwrap_or_default()), link: link.take(), crop, rotation,
                                alt: descr.take().and_then(|d| officina_grid::charts::clean_alt(&d)),
                            }).is_err() {
                                rep.note(Kind::Dropped, "missing, oversized or unsupported pictures (embedded PNG and JPEG supported)");
                            }
                        }
                        if let Some((p, ex)) = chart.take() {
                            if let Some(data) = z.read_name(&p)? {
                                let c = if ex { chartex_part(wb, &data, anchor, rep)? } else { chart_part(wb, &data, anchor, rep)? };
                                match c {
                                    Some(mut c) => {
                                        c.alt = descr.take().and_then(|d| officina_grid::charts::clean_alt(&d));
                                        if wb.add_chart(c).is_err() {
                                            rep.note(Kind::Dropped, "charts this engine can't use");
                                        }
                                    }
                                    None => rep.note(Kind::Dropped, "charts of kinds Officina doesn't draw yet (surface, Pareto, funnel, treemap, sunburst, map)"),
                                }
                            }
                        }
                    }
                    _ => {}
                },
                Event::Eof => break,
                _ => {}
            }
            buf.clear();
        }
    }
    Ok(())
}

#[cfg(feature = "read")]
#[derive(Default)]
struct SerIn {
    order: Option<usize>,
    name: Option<ChartText>,
    cats: Option<Rect>,
    vals: Option<Rect>,
    look: SeriesLook,
    /// The trendline being read (only a series' first one is kept).
    trend: Option<(Option<TrendKind>, u8, u8, bool, bool, f64, f64)>,
    trends: u8,
    /// The error bars being read: x bars are skipped, the y ones kept.
    err: Option<(bool, Option<String>, f64)>,
}

#[cfg(feature = "read")]
fn fnum(s: Option<String>) -> Option<f64> {
    s?.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

#[cfg(feature = "read")]
/// One chart part. `None`: no group of a kind Officina draws.
fn chart_part(wb: &Workbook, data: &[u8], anchor: Anchor, rep: &mut Report) -> Result<Option<Chart>, XlsxError> {
    let mut r = xml(data);
    let mut buf = Vec::new();
    // Element path (local names), for context.
    let mut path: Vec<Vec<u8>> = Vec::new();
    let mut kind: Option<ChartKind> = None;
    let mut grouping = Grouping::Standard;
    let mut series: Vec<(usize, Series)> = Vec::new();
    let mut group_kind: Option<ChartKind> = None;
    // The first group's axes are the primary ones; a group on others is secondary.
    let mut primary_axes: Option<Vec<String>> = None;
    let mut group_axes: Vec<String> = Vec::new();
    let mut group_start = 0usize;
    let mut ser: Option<SerIn> = None;
    let mut title: Option<ChartText> = None;
    let mut title_text = String::new();
    let mut deleted = false;
    let mut legend: Option<LegendPos> = None;
    let mut axis_titles: Vec<(bool, String)> = Vec::new(); // (category axis, text)
    let mut ax_text = String::new();
    let mut ax_pos_b = false;
    let mut skipped = false;
    let mut text_into: u8 = 0; // 1: f, 2: v, 3: a:t
    let mut text = String::new();
    loop {
        let ev = r.read_event_into(&mut buf)?;
        match &ev {
            Event::Start(e) | Event::Empty(e) => {
                let name = e.local_name().as_ref().to_vec();
                let empty = matches!(ev, Event::Empty(_));
                match name.as_slice() {
                    g @ (b"barChart" | b"bar3DChart" | b"lineChart" | b"line3DChart" | b"areaChart" | b"area3DChart" | b"scatterChart" | b"pieChart" | b"pie3DChart" | b"doughnutChart" | b"ofPieChart" | b"radarChart" | b"stockChart" | b"bubbleChart") => {
                        if g.windows(2).any(|w| w == b"3D") {
                            rep.note(Kind::Approximate, "3-D charts drawn flat");
                        }
                        group_kind = Some(match g {
                            b"barChart" | b"bar3DChart" => ChartKind::Column,
                            b"lineChart" | b"line3DChart" => ChartKind::Line,
                            b"areaChart" | b"area3DChart" => ChartKind::Area,
                            b"scatterChart" => ChartKind::Scatter,
                            b"doughnutChart" => ChartKind::Doughnut,
                            b"radarChart" => ChartKind::Radar,
                            b"stockChart" => ChartKind::Stock,
                            b"bubbleChart" => ChartKind::Bubble,
                            _ => ChartKind::Pie,
                        });
                        group_axes.clear();
                        group_start = series.len();
                    }
                    b"axId" if group_kind.is_some() && ser.is_none() => {
                        if let Some(id) = attr(e, b"val")? {
                            if group_axes.len() < 4 {
                                group_axes.push(id);
                            }
                        }
                    }
                    b"surfaceChart" | b"surface3DChart" => {
                        group_kind = None;
                        skipped = true;
                    }
                    b"barDir" if group_kind == Some(ChartKind::Column) => {
                        if attr(e, b"val")?.is_some_and(|v| v == "bar") {
                            group_kind = Some(ChartKind::Bar);
                        }
                    }
                    b"grouping" if kind.is_none() && group_kind.is_some() => {
                        grouping = match attr(e, b"val")?.as_deref() {
                            Some("stacked") => Grouping::Stacked,
                            Some("percentStacked") => Grouping::Percent,
                            _ => Grouping::Standard,
                        };
                    }
                    b"ser" if group_kind.is_some() => ser = Some(SerIn::default()),
                    b"order" if !path.last().is_some_and(|p| p.as_slice() == b"trendline") => {
                        if let Some(s) = &mut ser {
                            s.order = attr(e, b"val")?.and_then(|value| value.parse().ok());
                        }
                    }
                    b"showVal" if path.last().is_some_and(|p| p.as_slice() == b"dLbls") => {
                        if let Some(s) = &mut ser {
                            s.look.labels = attr(e, b"val")?.is_none_or(|v| v == "1" || v == "true");
                        }
                    }
                    b"trendline" => {
                        if let Some(s) = &mut ser {
                            s.trends = s.trends.saturating_add(1);
                            if s.trends == 1 {
                                s.trend = Some((None, 2, 2, false, false, 0.0, 0.0));
                            }
                        }
                    }
                    b"errBars" => {
                        if let Some(s) = &mut ser {
                            if s.look.errors.is_none() {
                                s.err = Some((false, None, 1.0));
                            }
                        }
                    }
                    n @ (b"trendlineType" | b"order" | b"period" | b"forward" | b"backward" | b"dispRSqr" | b"dispEq") if path.last().is_some_and(|p| p.as_slice() == b"trendline") => {
                        let v = attr(e, b"val")?;
                        if let Some(t) = ser.as_mut().and_then(|s| s.trend.as_mut()) {
                            let yes = v.as_deref().is_none_or(|v| v == "1" || v == "true");
                            let small = |v: Option<String>, lo: f64, hi: f64| fnum(v).map_or(lo, |x| x.clamp(lo, hi)) as u8;
                            match n {
                                b"trendlineType" => {
                                    t.0 = match v.as_deref() {
                                        Some("linear") => Some(TrendKind::Linear),
                                        Some("exp") => Some(TrendKind::Exponential),
                                        Some("log") => Some(TrendKind::Logarithmic),
                                        Some("poly") => Some(TrendKind::Polynomial(0)),
                                        Some("power") => Some(TrendKind::Power),
                                        Some("movingAvg") => Some(TrendKind::MovingAverage(0)),
                                        _ => None,
                                    }
                                }
                                b"order" => t.1 = small(v, 2.0, 6.0),
                                b"period" => t.2 = small(v, 2.0, 255.0),
                                b"forward" => t.5 = fnum(v).unwrap_or(0.0).clamp(0.0, 1e6),
                                b"backward" => t.6 = fnum(v).unwrap_or(0.0).clamp(0.0, 1e6),
                                b"dispRSqr" => t.4 = yes,
                                _ => t.3 = yes,
                            }
                        }
                    }
                    n @ (b"errDir" | b"errValType" | b"val") if path.last().is_some_and(|p| p.as_slice() == b"errBars") => {
                        let v = attr(e, b"val")?;
                        if let Some(b) = ser.as_mut().and_then(|s| s.err.as_mut()) {
                            match n {
                                b"errDir" => b.0 = v.as_deref() == Some("x"),
                                b"errValType" => b.1 = v,
                                _ => b.2 = fnum(v).filter(|x| *x >= 0.0).unwrap_or(1.0),
                            }
                        }
                    }
                    b"autoTitleDeleted" => deleted = attr(e, b"val")?.map_or(true, |v| v == "1" || v == "true"),
                    b"legendPos" => {
                        legend = Some(match attr(e, b"val")?.as_deref() {
                            Some("l") => LegendPos::Left,
                            Some("t") => LegendPos::Top,
                            Some("b") => LegendPos::Bottom,
                            _ => LegendPos::Right,
                        })
                    }
                    b"legend" if legend.is_none() => legend = Some(LegendPos::Right),
                    b"axPos" => ax_pos_b = attr(e, b"val")?.is_some_and(|v| v == "b" || v == "t"),
                    b"f" => {
                        text_into = 1;
                        text.clear();
                    }
                    b"v" => {
                        text_into = 2;
                        text.clear();
                    }
                    b"t" => {
                        text_into = 3;
                        text.clear();
                    }
                    _ => {}
                }
                if !empty {
                    path.push(name);
                }
            }
            Event::Text(t) if text_into != 0 => {
                if let Ok(s) = t.unescape() {
                    if text.len() + s.len() <= MAX_TEXT {
                        text.push_str(&s);
                    }
                }
            }
            Event::End(e) => {
                let name = e.local_name().as_ref().to_vec();
                path.pop();
                let inside = |n: &[u8]| path.iter().any(|p| p.as_slice() == n);
                match name.as_slice() {
                    b"f" | b"v" | b"t" => {
                        let t = std::mem::take(&mut text);
                        let what = text_into;
                        text_into = 0;
                        if let Some(s) = ser.as_mut() {
                            if inside(b"dLbls") || inside(b"trendline") || inside(b"errBars") {
                                // Label text and custom error amounts: not the series' own.
                            } else if inside(b"tx") && s.name.is_none() {
                                s.name = match what {
                                    1 => range(wb, &t).map(|r| ChartText::Cell(CellAddr::new(r.sheet, r.r0, r.c0))),
                                    _ => Some(ChartText::Lit(decode_escapes(&t).as_ref().into())),
                                };
                            } else if what == 1 && (inside(b"cat") || inside(b"xVal")) {
                                s.cats = range(wb, &t);
                            } else if what == 1 && (inside(b"val") || inside(b"yVal")) {
                                s.vals = range(wb, &t);
                            } else if what == 1 && inside(b"bubbleSize") {
                                s.look.sizes = range(wb, &t).filter(|r| r.r0 == r.r1 || r.c0 == r.c1);
                            }
                        } else if inside(b"title") {
                            let in_axis = inside(b"catAx") || inside(b"valAx") || inside(b"dateAx") || inside(b"serAx");
                            if in_axis {
                                if what == 3 {
                                    ax_text.push_str(&t);
                                }
                            } else if what == 1 {
                                title = range(wb, &t).map(|r| ChartText::Cell(CellAddr::new(r.sheet, r.r0, r.c0)));
                            } else if what == 3 && title_text.len() < MAX_TEXT {
                                title_text.push_str(&t);
                            }
                        }
                    }
                    b"trendline" => {
                        if let Some(s) = ser.as_mut() {
                            if let Some((Some(k), order, period, eq, r2, fw, bw)) = s.trend.take() {
                                let kind = match k {
                                    TrendKind::Polynomial(_) => TrendKind::Polynomial(order),
                                    TrendKind::MovingAverage(_) => TrendKind::MovingAverage(period),
                                    k => k,
                                };
                                s.look.trend = Some(officina_grid::charts::Trendline { kind, equation: eq, r_squared: r2, forward: fw, backward: bw });
                            }
                            if s.trends == 2 {
                                rep.note(Kind::Dropped, "second trendlines on a series");
                            }
                        }
                    }
                    b"errBars" => {
                        if let Some(s) = ser.as_mut() {
                            if let Some((false, ty, v)) = s.err.take() {
                                s.look.errors = match ty.as_deref() {
                                    Some("fixedVal") => Some(ErrorBars::Fixed(v)),
                                    Some("percentage") => Some(ErrorBars::Percent(v)),
                                    Some("stdDev") => Some(ErrorBars::StdDev(v)),
                                    Some("stdErr") => Some(ErrorBars::StdErr),
                                    _ => {
                                        rep.note(Kind::Dropped, "custom error bar amounts");
                                        None
                                    }
                                };
                            }
                        }
                    }
                    b"ser" => {
                        if let (Some(s), Some(gk)) = (ser.take(), group_kind) {
                            match s.vals {
                                Some(vals) if vals.r0 == vals.r1 || vals.c0 == vals.c1 => {
                                    let k = *kind.get_or_insert(gk);
                                    if (k.whole() || gk.whole()) && k != gk {
                                        rep.note(Kind::Dropped, "series combined with a radar, stock, pie or doughnut chart");
                                    } else if series.len() < officina_grid::charts::MAX_SERIES {
                                        let model = Series { name: s.name, cats: s.cats.filter(|c| c.r0 == c.r1 || c.c0 == c.c1), vals, kind: (gk != k).then_some(gk), look: s.look };
                                        series.push((s.order.unwrap_or(series.len()), model));
                                    }
                                }
                                _ => rep.note(Kind::Dropped, "chart series without a cell range (typed-in values)"),
                            }
                        }
                    }
                    b"title" if inside(b"catAx") || inside(b"valAx") || inside(b"dateAx") => {
                        let cat = inside(b"catAx") || inside(b"dateAx") || (kind.is_some_and(ChartKind::xy) && ax_pos_b);
                        axis_titles.push((cat, std::mem::take(&mut ax_text)));
                    }
                    b"catAx" | b"valAx" | b"dateAx" | b"serAx" => {
                        ax_pos_b = false;
                    }
                    b"barChart" | b"bar3DChart" | b"lineChart" | b"line3DChart" | b"areaChart" | b"area3DChart" | b"scatterChart" | b"pieChart" | b"pie3DChart" | b"doughnutChart" | b"ofPieChart" | b"radarChart" | b"stockChart" | b"surfaceChart" | b"surface3DChart" | b"bubbleChart" => {
                        if group_kind.is_some() {
                            let axes = std::mem::take(&mut group_axes);
                            let primary = primary_axes.get_or_insert_with(|| axes.clone());
                            if !axes.is_empty() && *primary != axes {
                                for (_, s) in series.iter_mut().skip(group_start) {
                                    s.look.secondary = true;
                                }
                            }
                        }
                        group_kind = None
                    }
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    let Some(kind) = kind else { return Ok(None) };
    if skipped {
        rep.note(Kind::Dropped, "surface chart series");
    }
    if title.is_none() && !title_text.is_empty() {
        title = Some(ChartText::Lit(decode_escapes(&title_text).as_ref().into()));
    }
    // OOXML groups series by type. Display and legend order come from c:order.
    series.sort_by_key(|(order, _)| *order);
    let series: Vec<_> = series.into_iter().map(|(_, series)| series).collect();
    // Excel's automatic title: a chart of one series is named after it.
    if title.is_none() && !deleted && series.len() == 1 {
        title = series[0].name.clone();
    }
    let pick = |cat: bool| axis_titles.iter().find(|(c, t)| *c == cat && !t.is_empty()).map(|(_, t)| Arc::from(t.as_str()));
    Ok(Some(Chart { kind, grouping, title, series, legend, x_title: pick(true), y_title: pick(false), anchor, alt: None }))
}

#[cfg(feature = "read")]
/// One chart-extension part (`cx:`): waterfall, histogram and box and whisker.
/// `None`: none of its series is of those layouts.
fn chartex_part(wb: &Workbook, data: &[u8], anchor: Anchor, rep: &mut Report) -> Result<Option<Chart>, XlsxError> {
    let mut r = xml(data);
    let mut buf = Vec::new();
    let mut path: Vec<Vec<u8>> = Vec::new();
    // Data blocks by id: (categories, values).
    let mut blocks: Vec<(String, Option<Rect>, Option<Rect>)> = Vec::new();
    let mut dim: Option<bool> = None; // Some(true): a category dimension
    let mut kind: Option<ChartKind> = None;
    let mut ser: Option<(Option<ChartText>, Option<String>, SeriesLook)> = None;
    let mut series: Vec<(Option<ChartText>, Option<String>, SeriesLook)> = Vec::new();
    let mut title: Option<ChartText> = None;
    let mut title_text = String::new();
    let mut axis: Option<String> = None;
    let mut axis_titles = (String::new(), String::new());
    let mut legend = None;
    let mut text = String::new();
    let mut other = false;
    loop {
        let ev = r.read_event_into(&mut buf)?;
        match &ev {
            Event::Start(e) | Event::Empty(e) => {
                let name = e.local_name().as_ref().to_vec();
                match name.as_slice() {
                    b"data" if path.last().is_some_and(|p| p.as_slice() == b"chartData") => {
                        if blocks.len() < 2 * officina_grid::charts::MAX_SERIES {
                            blocks.push((attr(e, b"id")?.unwrap_or_default(), None, None));
                        }
                    }
                    b"strDim" | b"numDim" => dim = Some(attr(e, b"type")?.as_deref() == Some("cat")),
                    b"series" => {
                        let k = match attr(e, b"layoutId")?.as_deref() {
                            Some("waterfall") => Some(ChartKind::Waterfall),
                            Some("clusteredColumn") => Some(ChartKind::Histogram),
                            Some("boxWhisker") => Some(ChartKind::BoxWhisker),
                            _ => None,
                        };
                        let hidden = attr(e, b"hidden")?.is_some_and(|v| v == "1" || v == "true");
                        match k {
                            Some(k) if !hidden && kind.is_none_or(|x| x == k) => {
                                kind = Some(k);
                                ser = Some((None, None, SeriesLook::default()));
                            }
                            _ => other = true,
                        }
                    }
                    b"dataId" => {
                        if let Some(s) = &mut ser {
                            s.1 = attr(e, b"val")?;
                        }
                    }
                    b"dataLabels" => {
                        if let Some(s) = &mut ser {
                            s.2.labels = true;
                        }
                    }
                    b"visibility" if path.last().is_some_and(|p| p.as_slice() == b"dataLabels") => {
                        if let Some(s) = &mut ser {
                            s.2.labels = attr(e, b"value")?.is_none_or(|v| v == "1" || v == "true");
                        }
                    }
                    b"idx" if path.last().is_some_and(|p| p.as_slice() == b"subtotals") => {
                        if let Some(s) = &mut ser {
                            if let Some(t) = attr(e, b"val")?.and_then(|v| v.parse::<u32>().ok()).filter(|&t| t < officina_grid::addr::MAX_ROWS) {
                                if s.2.totals.len() < MAX_TOTALS && !s.2.totals.contains(&t) {
                                    s.2.totals.push(t);
                                }
                            }
                        }
                    }
                    b"binCount" => {
                        if let Some(s) = &mut ser {
                            s.2.bins = fnum(attr(e, b"val")?).filter(|n| (1.0..=10_000.0).contains(n)).map(|n| Bins::Count(n as u32));
                        }
                    }
                    b"binSize" => {
                        if let Some(s) = &mut ser {
                            s.2.bins = fnum(attr(e, b"val")?).filter(|w| *w > 0.0).map(Bins::Width);
                        }
                    }
                    b"axis" => axis = attr(e, b"id")?,
                    b"legend" => {
                        legend = Some(match attr(e, b"pos")?.as_deref() {
                            Some("l") => LegendPos::Left,
                            Some("t") => LegendPos::Top,
                            Some("b") => LegendPos::Bottom,
                            _ => LegendPos::Right,
                        })
                    }
                    b"f" | b"v" | b"t" => text.clear(),
                    _ => {}
                }
                if matches!(ev, Event::Start(_)) {
                    if path.len() >= 64 {
                        return Ok(None);
                    }
                    path.push(name);
                }
            }
            Event::Text(t) => {
                if let Ok(s) = t.unescape() {
                    if text.len() + s.len() <= MAX_TEXT {
                        text.push_str(&s);
                    }
                }
            }
            Event::End(e) => {
                let name = e.local_name().as_ref().to_vec();
                path.pop();
                let inside = |n: &[u8]| path.iter().any(|p| p.as_slice() == n);
                match name.as_slice() {
                    b"f" | b"v" | b"t" => {
                        let t = std::mem::take(&mut text);
                        let f = name.as_slice() == b"f";
                        if inside(b"chartData") {
                            if let (true, Some(b), Some(cat)) = (f, blocks.last_mut(), dim) {
                                let r = range(wb, &t).filter(|r| r.r0 == r.r1 || r.c0 == r.c1);
                                if cat {
                                    b.1 = r;
                                } else if b.2.is_none() {
                                    b.2 = r;
                                }
                            }
                        } else if let Some(s) = ser.as_mut() {
                            if inside(b"tx") && s.0.is_none() {
                                s.0 = if f { range(wb, &t).map(|r| ChartText::Cell(CellAddr::new(r.sheet, r.r0, r.c0))) } else { Some(ChartText::Lit(decode_escapes(&t).as_ref().into())) };
                            }
                        } else if inside(b"title") {
                            let into = match (inside(b"axis"), axis.as_deref()) {
                                (true, Some("0")) => Some(&mut axis_titles.0),
                                (true, _) => Some(&mut axis_titles.1),
                                (false, _) if f => {
                                    title = range(wb, &t).map(|r| ChartText::Cell(CellAddr::new(r.sheet, r.r0, r.c0)));
                                    None
                                }
                                (false, _) => Some(&mut title_text),
                            };
                            if let Some(into) = into {
                                if !f && into.len() + t.len() <= MAX_TEXT {
                                    into.push_str(&t);
                                }
                            }
                        }
                    }
                    b"strDim" | b"numDim" => dim = None,
                    b"axis" => axis = None,
                    b"series" => {
                        if let Some(s) = ser.take() {
                            if series.len() < officina_grid::charts::MAX_SERIES {
                                series.push(s);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    let Some(kind) = kind else { return Ok(None) };
    if other {
        rep.note(Kind::Dropped, "chart-extension series Officina doesn't draw (Pareto lines and mixed layouts)");
    }
    let mut out = Vec::new();
    for (name, id, mut look) in series {
        let Some((_, cats, Some(vals))) = id.and_then(|id| blocks.iter().find(|b| b.0 == id)).cloned() else {
            rep.note(Kind::Dropped, "chart series without a cell range (typed-in values)");
            continue;
        };
        look.totals.sort_unstable();
        out.push(Series { name, cats, vals, kind: None, look });
    }
    if title.is_none() && !title_text.is_empty() {
        title = Some(ChartText::Lit(decode_escapes(&title_text).as_ref().into()));
    }
    let opt = |s: String| (!s.is_empty()).then(|| Arc::from(s.as_str()));
    Ok(Some(Chart { kind, grouping: Grouping::Standard, title, series: out, legend, x_title: opt(axis_titles.0), y_title: opt(axis_titles.1), anchor, alt: None }))
}

// ── Writing ─────────────────────────────────────────────────────────────

fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            c => o.push(c),
        }
    }
    o
}

fn ref_text(wb: &Workbook, r: &Rect) -> String {
    let sh = crate::ods::formula::sheet_a1(&wb.sheet_name(r.sheet));
    if r.r0 == r.r1 && r.c0 == r.c1 {
        format!("{sh}!${}${}", col_name(r.c0), r.r0 + 1)
    } else {
        format!("{sh}!${}${}:${}${}", col_name(r.c0), r.r0 + 1, col_name(r.c1), r.r1 + 1)
    }
}

fn rich(o: &mut String, t: &str) {
    let _ = write!(o, r#"<c:tx><c:rich><a:bodyPr/><a:p><a:r><a:t>{}</a:t></a:r></a:p></c:rich></c:tx><c:overlay val="0"/>"#, esc(t));
}

fn text_el(o: &mut String, wb: &Workbook, t: &ChartText) {
    match t {
        ChartText::Lit(s) => rich(o, s),
        ChartText::Cell(a) => {
            let _ = write!(
                o,
                r#"<c:tx><c:strRef><c:f>{}</c:f><c:strCache><c:ptCount val="1"/><c:pt idx="0"><c:v>{}</c:v></c:pt></c:strCache></c:strRef></c:tx><c:overlay val="0"/>"#,
                esc(&ref_text(wb, &Rect { sheet: a.sheet, r0: a.row, c0: a.col, r1: a.row, c1: a.col })),
                esc(&Chart::text(wb, t))
            );
        }
    }
}

/// Points cached in a written chart per series, at most (readers use the ranges).
const CACHE_MAX: usize = 100_000;

/// Whether `c` is one of Excel 2016's chart-extension kinds, saved as
/// `xl/charts/chartExN.xml` (`cx:` markup) rather than a DrawingML chart.
pub(crate) fn is_ex(c: &Chart) -> bool {
    matches!(c.kind, ChartKind::Waterfall | ChartKind::Histogram | ChartKind::BoxWhisker)
}

/// The series' data labels, trendline and error bars, where its group allows them.
fn series_extras(o: &mut String, k: ChartKind, look: &SeriesLook) {
    if look.labels {
        o.push_str(r#"<c:dLbls><c:showLegendKey val="0"/><c:showVal val="1"/><c:showCatName val="0"/><c:showSerName val="0"/><c:showPercent val="0"/><c:showBubbleSize val="0"/></c:dLbls>"#);
    }
    if !matches!(k, ChartKind::Column | ChartKind::Bar | ChartKind::Line | ChartKind::Area | ChartKind::Scatter | ChartKind::Bubble) {
        return;
    }
    if let Some(t) = &look.trend {
        let (ty, extra) = match t.kind {
            TrendKind::Linear => ("linear", String::new()),
            TrendKind::Exponential => ("exp", String::new()),
            TrendKind::Logarithmic => ("log", String::new()),
            TrendKind::Polynomial(n) => ("poly", format!(r#"<c:order val="{n}"/>"#)),
            TrendKind::Power => ("power", String::new()),
            TrendKind::MovingAverage(n) => ("movingAvg", format!(r#"<c:period val="{n}"/>"#)),
        };
        let _ = write!(o, r#"<c:trendline><c:trendlineType val="{ty}"/>{extra}"#);
        if !matches!(t.kind, TrendKind::MovingAverage(_)) {
            let _ = write!(o, r#"<c:forward val="{}"/><c:backward val="{}"/>"#, t.forward, t.backward);
        }
        let _ = write!(o, r#"<c:dispRSqr val="{}"/><c:dispEq val="{}"/></c:trendline>"#, u8::from(t.r_squared), u8::from(t.equation));
    }
    if let Some(e) = look.errors {
        let (ty, v) = match e {
            ErrorBars::Fixed(v) => ("fixedVal", v),
            ErrorBars::Percent(v) => ("percentage", v),
            ErrorBars::StdDev(v) => ("stdDev", v),
            ErrorBars::StdErr => ("stdErr", 1.0),
        };
        let dir = if k.xy() { r#"<c:errDir val="y"/>"# } else { "" };
        let _ = write!(o, r#"<c:errBars>{dir}<c:errBarType val="both"/><c:errValType val="{ty}"/><c:noEndCap val="0"/><c:val val="{v}"/></c:errBars>"#);
    }
}

fn num_ref(o: &mut String, el: &str, wb: &Workbook, r: &Rect, vals: &[Option<f64>]) {
    let _ = write!(o, "<c:{el}><c:numRef><c:f>{}</c:f><c:numCache><c:formatCode>General</c:formatCode><c:ptCount val=\"{}\"/>", esc(&ref_text(wb, r)), vals.len());
    for (j, v) in vals.iter().enumerate() {
        if let Some(v) = v {
            let _ = write!(o, r#"<c:pt idx="{j}"><c:v>{v}</c:v></c:pt>"#);
        }
    }
    let _ = write!(o, "</c:numCache></c:numRef></c:{el}>");
}

/// `xl/charts/chartN.xml` for `c` (or `chartExN.xml` when [`is_ex`]).
pub(crate) fn chart_xml(wb: &Workbook, c: &Chart) -> String {
    if is_ex(c) {
        return chartex_xml(wb, c);
    }
    let data = c.data(wb, CACHE_MAX);
    let mut o = String::from(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><c:roundedCorners val="0"/><c:chart>"#);
    match &c.title {
        Some(t) => {
            o.push_str("<c:title>");
            text_el(&mut o, wb, t);
            o.push_str(r#"</c:title><c:autoTitleDeleted val="0"/>"#);
        }
        None => o.push_str(r#"<c:autoTitleDeleted val="1"/>"#),
    }
    o.push_str("<c:plotArea><c:layout/>");
    // One group per kind and axis, in the order the series first use them.
    let whole = c.kind.whole();
    let group_of = |d: &officina_grid::charts::SeriesData| (if whole { c.kind } else { d.kind }, d.look.secondary && !whole && !c.kind.round());
    let mut groups: Vec<(ChartKind, bool)> = Vec::new();
    for d in &data {
        let g = group_of(d);
        if !groups.contains(&g) {
            groups.push(g);
        }
    }
    if groups.is_empty() {
        groups.push((c.kind, false));
    }
    let round = groups.iter().all(|g| g.0.round());
    let xy = groups.iter().any(|g| g.0.xy());
    let any_secondary = groups.iter().any(|g| g.1);
    let (ax_a, ax_b, ax_c, ax_d) = (500_000_001u32, 500_000_002u32, 500_000_003u32, 500_000_004u32);
    for &(k, secondary) in &groups {
        let (x_ax, y_ax) = if secondary { (ax_c, ax_d) } else { (ax_a, ax_b) };
        let grouping = match c.grouping {
            Grouping::Standard if matches!(k, ChartKind::Column | ChartKind::Bar) => "clustered",
            Grouping::Standard => "standard",
            Grouping::Stacked => "stacked",
            Grouping::Percent => "percentStacked",
        };
        match k {
            ChartKind::Column | ChartKind::Bar => {
                let _ = write!(o, r#"<c:barChart><c:barDir val="{}"/><c:grouping val="{grouping}"/><c:varyColors val="0"/>"#, if k == ChartKind::Bar { "bar" } else { "col" });
            }
            ChartKind::Line => {
                let _ = write!(o, r#"<c:lineChart><c:grouping val="{grouping}"/><c:varyColors val="0"/>"#);
            }
            ChartKind::Area => {
                let _ = write!(o, r#"<c:areaChart><c:grouping val="{grouping}"/><c:varyColors val="0"/>"#);
            }
            ChartKind::Scatter => o.push_str(r#"<c:scatterChart><c:scatterStyle val="lineMarker"/><c:varyColors val="0"/>"#),
            ChartKind::Pie => o.push_str(r#"<c:pieChart><c:varyColors val="1"/>"#),
            ChartKind::Doughnut => o.push_str(r#"<c:doughnutChart><c:varyColors val="1"/>"#),
            ChartKind::Radar => o.push_str(r#"<c:radarChart><c:radarStyle val="marker"/><c:varyColors val="0"/>"#),
            ChartKind::Bubble => o.push_str(r#"<c:bubbleChart><c:varyColors val="0"/>"#),
            ChartKind::Stock => o.push_str("<c:stockChart>"),
            ChartKind::Waterfall | ChartKind::Histogram | ChartKind::BoxWhisker => o.push_str(r#"<c:barChart><c:barDir val="col"/><c:grouping val="clustered"/><c:varyColors val="0"/>"#),
        }
        for (i, (s, d)) in c.series.iter().zip(&data).enumerate() {
            if group_of(d) != (k, secondary) {
                continue;
            }
            let _ = write!(o, r#"<c:ser><c:idx val="{i}"/><c:order val="{i}"/>"#);
            if let Some(n) = &s.name {
                match n {
                    ChartText::Lit(t) => {
                        let _ = write!(o, "<c:tx><c:v>{}</c:v></c:tx>", esc(t));
                    }
                    ChartText::Cell(a) => {
                        let _ = write!(
                            o,
                            r#"<c:tx><c:strRef><c:f>{}</c:f><c:strCache><c:ptCount val="1"/><c:pt idx="0"><c:v>{}</c:v></c:pt></c:strCache></c:strRef></c:tx>"#,
                            esc(&ref_text(wb, &Rect { sheet: a.sheet, r0: a.row, c0: a.col, r1: a.row, c1: a.col })),
                            esc(&d.name)
                        );
                    }
                }
            }
            if matches!(k, ChartKind::Scatter | ChartKind::Stock) {
                o.push_str(r#"<c:spPr><a:ln w="19050"><a:noFill/></a:ln></c:spPr>"#);
            }
            if k == ChartKind::Bubble {
                o.push_str(r#"<c:invertIfNegative val="0"/>"#);
            }
            if k == ChartKind::Stock {
                o.push_str(r#"<c:marker><c:symbol val="none"/></c:marker>"#);
            }
            series_extras(&mut o, k, &s.look);
            let (cat_el, val_el) = if k.xy() { ("xVal", "yVal") } else { ("cat", "val") };
            if let Some(cr) = &s.cats {
                let _ = write!(o, "<c:{cat_el}><c:strRef><c:f>{}</c:f><c:strCache><c:ptCount val=\"{}\"/>", esc(&ref_text(wb, cr)), d.cats.len());
                for (j, t) in d.cats.iter().enumerate() {
                    let _ = write!(o, r#"<c:pt idx="{j}"><c:v>{}</c:v></c:pt>"#, esc(t));
                }
                let _ = write!(o, "</c:strCache></c:strRef></c:{cat_el}>");
            }
            num_ref(&mut o, val_el, wb, &s.vals, &d.vals);
            if k == ChartKind::Bubble {
                if let Some(sz) = &s.look.sizes {
                    num_ref(&mut o, "bubbleSize", wb, sz, &d.sizes);
                }
                o.push_str(r#"<c:bubble3D val="0"/>"#);
            }
            if matches!(k, ChartKind::Line | ChartKind::Stock | ChartKind::Scatter) {
                o.push_str(r#"<c:smooth val="0"/>"#);
            }
            o.push_str("</c:ser>");
        }
        match k {
            ChartKind::Column | ChartKind::Bar | ChartKind::Waterfall | ChartKind::Histogram | ChartKind::BoxWhisker => {
                let overlap = if c.grouping == Grouping::Standard { "" } else { r#"<c:overlap val="100"/>"# };
                let _ = write!(o, r#"<c:gapWidth val="150"/>{overlap}<c:axId val="{x_ax}"/><c:axId val="{y_ax}"/></c:barChart>"#);
            }
            ChartKind::Line => {
                let _ = write!(o, r#"<c:marker val="1"/><c:axId val="{x_ax}"/><c:axId val="{y_ax}"/></c:lineChart>"#);
            }
            ChartKind::Area => {
                let _ = write!(o, r#"<c:axId val="{x_ax}"/><c:axId val="{y_ax}"/></c:areaChart>"#);
            }
            ChartKind::Scatter => {
                let _ = write!(o, r#"<c:axId val="{x_ax}"/><c:axId val="{y_ax}"/></c:scatterChart>"#);
            }
            ChartKind::Radar => {
                let _ = write!(o, r#"<c:axId val="{x_ax}"/><c:axId val="{y_ax}"/></c:radarChart>"#);
            }
            ChartKind::Bubble => {
                let _ = write!(o, r#"<c:bubbleScale val="100"/><c:showNegBubbles val="0"/><c:axId val="{x_ax}"/><c:axId val="{y_ax}"/></c:bubbleChart>"#);
            }
            ChartKind::Stock => {
                let updown = if data.len() >= 4 { r#"<c:upDownBars><c:gapWidth val="150"/><c:upBars/><c:downBars/></c:upDownBars>"# } else { "" };
                let _ = write!(o, r#"<c:hiLowLines/>{updown}<c:axId val="{x_ax}"/><c:axId val="{y_ax}"/></c:stockChart>"#);
            }
            ChartKind::Pie => o.push_str(r#"<c:firstSliceAng val="0"/></c:pieChart>"#),
            ChartKind::Doughnut => o.push_str(r#"<c:firstSliceAng val="0"/><c:holeSize val="50"/></c:doughnutChart>"#),
        }
    }
    if !round {
        let horizontal = groups.iter().any(|g| g.0 == ChartKind::Bar);
        let (cat_pos, val_pos) = if horizontal { ("l", "b") } else { ("b", "l") };
        let ax_title = |o: &mut String, t: &Option<Arc<str>>| {
            if let Some(t) = t {
                o.push_str("<c:title>");
                rich(o, t);
                o.push_str("</c:title>");
            }
        };
        if xy {
            let _ = write!(o, r#"<c:valAx><c:axId val="{ax_a}"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="0"/><c:axPos val="b"/>"#);
        } else {
            let _ = write!(o, r#"<c:catAx><c:axId val="{ax_a}"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="0"/><c:axPos val="{cat_pos}"/>"#);
        }
        ax_title(&mut o, &c.x_title);
        let _ = write!(o, r#"<c:numFmt formatCode="General" sourceLinked="1"/><c:majorTickMark val="out"/><c:minorTickMark val="none"/><c:tickLblPos val="nextTo"/><c:crossAx val="{ax_b}"/><c:crosses val="autoZero"/>"#);
        o.push_str(if xy { r#"<c:crossBetween val="midCat"/></c:valAx>"# } else { r#"<c:auto val="1"/><c:lblAlgn val="ctr"/><c:lblOffset val="100"/><c:noMultiLvlLbl val="0"/></c:catAx>"# });
        let _ = write!(o, r#"<c:valAx><c:axId val="{ax_b}"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="0"/><c:axPos val="{}"/><c:majorGridlines/>"#, if xy { "l" } else { val_pos });
        ax_title(&mut o, &c.y_title);
        let _ = write!(o, r#"<c:numFmt formatCode="General" sourceLinked="1"/><c:majorTickMark val="out"/><c:minorTickMark val="none"/><c:tickLblPos val="nextTo"/><c:crossAx val="{ax_a}"/><c:crosses val="autoZero"/><c:crossBetween val="between"/></c:valAx>"#);
        if any_secondary {
            // The secondary axes: a hidden category axis and values on the right.
            if xy {
                let _ = write!(o, r#"<c:valAx><c:axId val="{ax_c}"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="1"/><c:axPos val="b"/><c:majorTickMark val="out"/><c:minorTickMark val="none"/><c:tickLblPos val="nextTo"/><c:crossAx val="{ax_d}"/><c:crosses val="autoZero"/><c:crossBetween val="midCat"/></c:valAx>"#);
            } else {
                let _ = write!(o, r#"<c:catAx><c:axId val="{ax_c}"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="1"/><c:axPos val="b"/><c:majorTickMark val="out"/><c:minorTickMark val="none"/><c:tickLblPos val="nextTo"/><c:crossAx val="{ax_d}"/><c:crosses val="autoZero"/><c:auto val="1"/><c:lblAlgn val="ctr"/><c:lblOffset val="100"/><c:noMultiLvlLbl val="0"/></c:catAx>"#);
            }
            let _ = write!(o, r#"<c:valAx><c:axId val="{ax_d}"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="0"/><c:axPos val="r"/><c:numFmt formatCode="General" sourceLinked="1"/><c:majorTickMark val="out"/><c:minorTickMark val="none"/><c:tickLblPos val="nextTo"/><c:crossAx val="{ax_c}"/><c:crosses val="max"/><c:crossBetween val="between"/></c:valAx>"#);
        }
    }
    o.push_str("</c:plotArea>");
    if let Some(l) = c.legend {
        let p = match l {
            LegendPos::Right => "r",
            LegendPos::Left => "l",
            LegendPos::Top => "t",
            LegendPos::Bottom => "b",
        };
        let _ = write!(o, r#"<c:legend><c:legendPos val="{p}"/><c:overlay val="0"/></c:legend>"#);
    }
    o.push_str(r#"<c:plotVisOnly val="1"/><c:dispBlanksAs val="gap"/></c:chart></c:chartSpace>"#);
    o
}

/// A chart-extension part (`cx:` markup) for a waterfall, histogram or box
/// and whisker chart: one data block per series, by reference with cached
/// points, then the series with their layout.
fn chartex_xml(wb: &Workbook, c: &Chart) -> String {
    let data = c.data(wb, CACHE_MAX);
    let mut o = String::from(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><cx:chartSpace xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" xmlns:cx="http://schemas.microsoft.com/office/drawing/2014/chartex"><cx:chartData>"#);
    for (i, (s, d)) in c.series.iter().zip(&data).enumerate() {
        let _ = write!(o, r#"<cx:data id="{i}">"#);
        if let Some(cr) = &s.cats {
            let _ = write!(o, r#"<cx:strDim type="cat"><cx:f>{}</cx:f><cx:lvl ptCount="{}">"#, esc(&ref_text(wb, cr)), d.cats.len());
            for (j, t) in d.cats.iter().enumerate() {
                let _ = write!(o, r#"<cx:pt idx="{j}">{}</cx:pt>"#, esc(t));
            }
            o.push_str("</cx:lvl></cx:strDim>");
        }
        let _ = write!(o, r#"<cx:numDim type="val"><cx:f>{}</cx:f><cx:lvl ptCount="{}" formatCode="General">"#, esc(&ref_text(wb, &s.vals)), d.vals.len());
        for (j, v) in d.vals.iter().enumerate() {
            if let Some(v) = v {
                let _ = write!(o, r#"<cx:pt idx="{j}">{v}</cx:pt>"#);
            }
        }
        o.push_str("</cx:lvl></cx:numDim></cx:data>");
    }
    o.push_str("</cx:chartData><cx:chart>");
    if let Some(t) = &c.title {
        let _ = write!(o, r#"<cx:title pos="t" align="ctr" overlay="0"><cx:tx><cx:txData>"#);
        if let ChartText::Cell(a) = t {
            let _ = write!(o, "<cx:f>{}</cx:f>", esc(&ref_text(wb, &Rect { sheet: a.sheet, r0: a.row, c0: a.col, r1: a.row, c1: a.col })));
        }
        let _ = write!(o, "<cx:v>{}</cx:v></cx:txData></cx:tx></cx:title>", esc(&Chart::text(wb, t)));
    }
    o.push_str("<cx:plotArea><cx:plotAreaRegion>");
    let layout = match c.kind {
        ChartKind::Waterfall => "waterfall",
        ChartKind::Histogram => "clusteredColumn",
        _ => "boxWhisker",
    };
    for (i, (s, d)) in c.series.iter().zip(&data).enumerate() {
        let _ = write!(o, r#"<cx:series layoutId="{layout}" uniqueId="{{00000000-0000-4000-8000-{i:012}}}">"#);
        if let Some(n) = &s.name {
            o.push_str("<cx:tx><cx:txData>");
            if let ChartText::Cell(a) = n {
                let _ = write!(o, "<cx:f>{}</cx:f>", esc(&ref_text(wb, &Rect { sheet: a.sheet, r0: a.row, c0: a.col, r1: a.row, c1: a.col })));
            }
            let _ = write!(o, "<cx:v>{}</cx:v></cx:txData></cx:tx>", esc(&d.name));
        }
        if s.look.labels {
            o.push_str(r#"<cx:dataLabels pos="outEnd"><cx:visibility seriesName="0" categoryName="0" value="1"/></cx:dataLabels>"#);
        }
        let _ = write!(o, r#"<cx:dataId val="{i}"/>"#);
        match c.kind {
            ChartKind::Waterfall if !s.look.totals.is_empty() => {
                o.push_str("<cx:layoutPr><cx:subtotals>");
                for t in &s.look.totals {
                    let _ = write!(o, r#"<cx:idx val="{t}"/>"#);
                }
                o.push_str("</cx:subtotals></cx:layoutPr>");
            }
            ChartKind::Histogram => {
                o.push_str(r#"<cx:layoutPr><cx:binning intervalClosed="r">"#);
                match s.look.bins {
                    Some(Bins::Count(n)) => {
                        let _ = write!(o, r#"<cx:binCount val="{n}"/>"#);
                    }
                    Some(Bins::Width(w)) => {
                        let _ = write!(o, r#"<cx:binSize val="{w}"/>"#);
                    }
                    None => {}
                }
                o.push_str("</cx:binning></cx:layoutPr>");
            }
            ChartKind::BoxWhisker => o.push_str(r#"<cx:layoutPr><cx:visibility meanLine="0" meanMarker="1" nonoutliers="0" outliers="1"/><cx:statistics quartileMethod="exclusive"/></cx:layoutPr>"#),
            _ => {}
        }
        o.push_str("</cx:series>");
    }
    o.push_str(r#"</cx:plotAreaRegion><cx:axis id="0"><cx:catScaling gapWidth="0.5"/>"#);
    if let Some(t) = &c.x_title {
        let _ = write!(o, "<cx:title><cx:tx><cx:txData><cx:v>{}</cx:v></cx:txData></cx:tx></cx:title>", esc(t));
    }
    o.push_str(r#"<cx:tickLabels/></cx:axis><cx:axis id="1"><cx:valScaling/>"#);
    if let Some(t) = &c.y_title {
        let _ = write!(o, "<cx:title><cx:tx><cx:txData><cx:v>{}</cx:v></cx:txData></cx:tx></cx:title>", esc(t));
    }
    o.push_str("<cx:majorGridlines/><cx:tickLabels/></cx:axis></cx:plotArea>");
    if let Some(l) = c.legend {
        let p = match l {
            LegendPos::Right => "r",
            LegendPos::Left => "l",
            LegendPos::Top => "t",
            LegendPos::Bottom => "b",
        };
        let _ = write!(o, r#"<cx:legend pos="{p}" align="ctr" overlay="0"/>"#);
    }
    o.push_str("</cx:chart></cx:chartSpace>");
    o
}

/// `xl/drawings/drawingN.xml` for the charts `ids` (their numbers in the package,
/// from 1) of one sheet; the drawing's relationship `rId{j+1}` is chart `j`.
pub(crate) fn drawing_xml(wb: &Workbook, ids: &[usize], pictures: &[usize]) -> String {
    let mut o = String::from(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">"#);
    let anchor = |o: &mut String, a: Anchor| {
        let emu = |pt: f32| (f64::from(pt) * EMU_PT).round() as i64;

        let _ = write!(
            o,
            r#"<xdr:twoCellAnchor editAs="oneCell"><xdr:from><xdr:col>{}</xdr:col><xdr:colOff>{}</xdr:colOff><xdr:row>{}</xdr:row><xdr:rowOff>{}</xdr:rowOff></xdr:from><xdr:to><xdr:col>{}</xdr:col><xdr:colOff>{}</xdr:colOff><xdr:row>{}</xdr:row><xdr:rowOff>{}</xdr:rowOff></xdr:to>"#,
            a.from.1, emu(a.from.2), a.from.0, emu(a.from.3), a.to.1, emu(a.to.2), a.to.0, emu(a.to.3)
        );
    };
    for (j, &i) in ids.iter().enumerate() {
        let c = &wb.charts()[i - 1];
        anchor(&mut o, c.anchor);
        if is_ex(c) {
            // Excel 2016's chart extension, with the shape older readers show instead.
            let descr = c.alt.as_deref().map_or(String::new(), |a| format!(r#" descr="{}""#, esc(a).replace('\n', "&#10;")));
            let _ = write!(
                o,
                r#"<mc:AlternateContent xmlns:mc="http://schemas.openxmlformats.org/markup-compatibility/2006"><mc:Choice xmlns:cx1="http://schemas.microsoft.com/office/drawing/2015/9/8/chartex" Requires="cx1"><xdr:graphicFrame macro=""><xdr:nvGraphicFramePr><xdr:cNvPr id="{}" name="Chart {}"{descr}/><xdr:cNvGraphicFramePr/></xdr:nvGraphicFramePr><xdr:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/></xdr:xfrm><a:graphic><a:graphicData uri="http://schemas.microsoft.com/office/drawing/2014/chartex"><cx:chart xmlns:cx="http://schemas.microsoft.com/office/drawing/2014/chartex" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" r:id="rId{}"/></a:graphicData></a:graphic></xdr:graphicFrame></mc:Choice><mc:Fallback><xdr:sp macro="" textlink=""><xdr:nvSpPr><xdr:cNvPr id="0" name=""/><xdr:cNvSpPr><a:spLocks noTextEdit="1"/></xdr:cNvSpPr></xdr:nvSpPr><xdr:spPr><a:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/></a:xfrm><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></xdr:spPr><xdr:txBody><a:bodyPr/><a:lstStyle/><a:p><a:r><a:t>This chart isn't available in this version of Excel.</a:t></a:r></a:p></xdr:txBody></xdr:sp></mc:Fallback></mc:AlternateContent><xdr:clientData/></xdr:twoCellAnchor>"#,
                j + 2,
                j + 1,
                j + 1
            );
            continue;
        }
        let _ = write!(
            o,
            r#"<xdr:graphicFrame macro=""><xdr:nvGraphicFramePr><xdr:cNvPr id="{}" name="Chart {}"{}/><xdr:cNvGraphicFramePr/></xdr:nvGraphicFramePr><xdr:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/></xdr:xfrm><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/chart"><c:chart xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" r:id="rId{}"/></a:graphicData></a:graphic></xdr:graphicFrame><xdr:clientData/></xdr:twoCellAnchor>"#,
            j + 2,
            j + 1,
            // An attribute keeps a line break only as a reference.
            wb.charts()[i - 1].alt.as_deref().map_or(String::new(), |a| format!(r#" descr="{}""#, esc(a).replace('\n', "&#10;"))),
            j + 1
        );
    }
    for (j, &i) in pictures.iter().enumerate() {
        let picture = &wb.pictures()[i];
        anchor(&mut o, picture.anchor);
        let [l, t, r, b] = picture.crop;
        let rotation = picture.rotation;
        let embed = if picture.bytes.is_empty() { String::new() } else { format!(r#" r:embed="rIdPic{j}""#) };
        let link = picture.link.as_ref().map(|_| format!(r#" r:link="rIdLink{j}""#)).unwrap_or_default();
        let id = ids.len() + j + 2;
        let descr = picture.alt.as_deref().map_or(String::new(), |a| format!(r#" descr="{}""#, esc(a).replace('\n', "&#10;")));
        let _ = write!(o, r#"<xdr:pic><xdr:nvPicPr><xdr:cNvPr id="{id}" name="Picture {id}"{descr}/><xdr:cNvPicPr/></xdr:nvPicPr><xdr:blipFill><a:blip xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"{embed}{link}/><a:srcRect l="{l}" t="{t}" r="{r}" b="{b}"/><a:stretch><a:fillRect/></a:stretch></xdr:blipFill><xdr:spPr><a:xfrm rot="{rotation}"/><a:prstGeom prst="rect"><a:avLst/></a:prstGeom></xdr:spPr></xdr:pic><xdr:clientData/></xdr:twoCellAnchor>"#);
    }
    o.push_str("</xdr:wsDr>");
    o
}
