//! `.ods` charts: a `draw:frame` holding a `draw:object` whose sub-document
//! (`Object N/content.xml`) is an ODF chart, anchored to a cell (the frame sits in it)
//! or to the sheet (`table:shapes`).
//!
//! Read: bar (vertical bars are Excel's horizontal "bar"), line, area, scatter,
//! circle (pie), ring (doughnut), radar, bubble and stock, stacked and percent,
//! series by cell ranges, titles, axis titles and the legend's place, and per
//! series data labels, the secondary y axis, regression curves (trendlines) and
//! y error indicators. Written the same way, anchored to the sheet at the chart's
//! place. Waterfall, histogram and box-and-whisker, which ODF lacks, are written as
//! bar charts with `officina:kind` naming them, and waterfall totals and histogram
//! bins as `officina:` attributes on the series, which other programs ignore.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

#[cfg(feature = "read")]
use quick_xml::events::Event;
#[cfg(feature = "read")]
use quick_xml::Reader;

use officina_grid::addr::{CellAddr, Rect, MAX_COLS, MAX_ROWS};
use officina_grid::charts::{Anchor, Bins, Chart, ChartKind, ChartText, ErrorBars, Grouping, LegendPos, Series, TrendKind};
#[cfg(feature = "read")]
use officina_grid::charts::{SeriesLook, Trendline, MAX_TOTALS};
use officina_grid::Workbook;

use crate::report::{Kind, Report};
use crate::zip::Archive;

/// A frame read from the sheet: where it is and which object it shows.
#[derive(Debug, Clone, Default)]
pub(crate) struct FrameIn {
    pub sheet: u16,
    /// The anchor cell; `None`: anchored to the sheet (x and y from its top-left).
    pub cell: Option<(u32, u32)>,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub end: Option<String>,
    pub end_x: f64,
    pub end_y: f64,
    pub href: Option<String>,
    pub image: Option<String>,
    pub transformed: bool,
    /// The frame's `svg:desc` (a chart's alt text), at most `MAX_ALT` bytes.
    pub desc: String,
    pub in_desc: bool,
}

const PT_PX: f64 = 72.0 / 96.0;
const MAX_TEXT: usize = officina_grid::charts::MAX_TITLE;

#[cfg(feature = "read")]
fn attrs(e: &quick_xml::events::BytesStart) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for a in e.attributes().with_checks(false).flatten() {
        if let Ok(v) = a.unescape_value() {
            m.insert(String::from_utf8_lossy(a.key.as_ref()).into_owned(), v.into_owned());
        }
    }
    m
}

#[cfg(feature = "read")]
/// `Data.B2:Data.B13`, `'My sheet'.$B$2:.$B$13` → the range.
fn range(wb: &Workbook, a: &str) -> Option<Rect> {
    // A range list is whitespace-separated outside quoted sheet names.
    let a = a.trim_start();
    let mut quoted = false;
    let end = a.char_indices().find_map(|(i, ch)| {
        if ch == '\'' { quoted = !quoted; }
        (ch.is_whitespace() && !quoted).then_some(i)
    }).unwrap_or(a.len());
    let a = &a[..end];
    let a1 = super::formula::from_of(&format!("of:=[{a}]"))?;
    let a1 = a1.trim_start_matches('=');
    let (sh, rf) = a1.rsplit_once('!')?;
    let sh = sh.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')).map_or(sh.to_string(), |s| s.replace("''", "'"));
    let rf = rf.replace('$', "");
    let (r0, c0, r1, c1) = crate::xlsx::read::parse_ref(&rf).or_else(|| officina_grid::addr::parse_a1(&rf).map(|(r, c)| (r, c, r, c)))?;
    Some(Rect { sheet: wb.sheet_id(&sh)?, r0, c0, r1, c1 })
}

#[cfg(feature = "read")]
fn line(r: &Rect) -> bool {
    r.r0 == r.r1 || r.c0 == r.c1
}

#[cfg(feature = "read")]
/// The cell under sheet position (x, y) px and the offset into it, in points.
fn cell_at(wb: &Workbook, sheet: u16, x: f64, y: f64) -> (u32, u32, f32, f32) {
    let sh = wb.sheet(sheet);
    // A hair of slack, so a position written at a cell's edge stays in that cell.
    let c = sh.col_sizes.index_at(x.max(0.0) + 0.05).min(MAX_COLS - 1);
    let r = sh.row_sizes.index_at(y.max(0.0) + 0.05).min(MAX_ROWS - 1);
    let dx = (x - sh.col_sizes.offset(c)).max(0.0) * PT_PX;
    let dy = (y - sh.row_sizes.offset(r)).max(0.0) * PT_PX;
    (r, c, dx as f32, dy as f32)
}

#[cfg(feature = "read")]
/// Turns sheet frames into charts and bounded embedded pictures.
pub(crate) fn read_frames(wb: &mut Workbook, z: &Archive, frames: Vec<FrameIn>, rep: &mut Report) {
    for f in frames {
        if (f.sheet as usize) >= wb.sheet_count() {
            continue;
        }
        // The anchor: from the cell (or the sheet's corner) by the frame's offset; to
        // the end cell, or by the frame's size.
        let (x0, y0) = match f.cell {
            Some((r, c)) => {
                let sh = wb.sheet(f.sheet);
                (sh.col_sizes.offset(c) + f.x, sh.row_sizes.offset(r) + f.y)
            }
            None => (f.x, f.y),
        };
        let from = cell_at(wb, f.sheet, x0, y0);
        let to = match f.end.as_deref().and_then(|e| range(wb, e)) {
            Some(e) => (e.r0, e.c0, (f.end_x * PT_PX) as f32, (f.end_y * PT_PX) as f32),
            None => cell_at(wb, f.sheet, x0 + f.w, y0 + f.h),
        };
        let anchor = Anchor { sheet: f.sheet, from, to };
        // Chart frames can also carry a cached preview. Prefer the chart.
        let Some(href) = f.href.as_deref() else {
            let added = f.image.as_deref().is_some_and(|href| {
                let path = href.strip_prefix("./").unwrap_or(href);
                if path.contains([':', '\\']) || path.split('/').any(|c| matches!(c, "" | "." | "..")) {
                    return false;
                }
                let Some(part) = z.find(path) else { return false };
                if !wb.can_add_picture(part.size as usize) { return false; }
                let Ok(data) = z.read(part) else { return false };
                wb.add_picture(officina_grid::charts::Picture {
                    anchor, bytes: data.into(), link: None, crop: [0; 4], rotation: 0,
                    alt: officina_grid::charts::clean_alt(&f.desc),
                }).is_ok()
            });
            if !added {
                rep.note(Kind::Dropped, "unsupported images and drawings");
            } else if f.transformed {
                rep.note(Kind::Approximate, "picture transforms");
            }
            continue;
        };
        let dir = href.trim_start_matches("./").trim_end_matches('/');
        let Ok(Some(data)) = z.read_name(&format!("{dir}/content.xml")) else {
            rep.note(Kind::Dropped, "embedded objects other than charts");
            continue;
        };
        match chart_doc(wb, &data, anchor, rep) {
            Some(mut c) => {
                c.alt = officina_grid::charts::clean_alt(&f.desc);
                if wb.add_chart(c).is_err() {
                    rep.note(Kind::Dropped, "charts this engine can't use");
                }
            }
            None => rep.note(Kind::Dropped, "embedded objects other than charts, and chart kinds Officina doesn't draw yet"),
        }
    }
}

#[cfg(feature = "read")]
#[derive(Default, Clone, Copy)]
struct Props {
    vertical: bool,
    stacked: bool,
    percent: bool,
}

/// The most chart styles one chart document keeps (a style per series, trendline
/// and error indicator is three each).
#[cfg(feature = "read")]
const MAX_STYLES: usize = 4 * officina_grid::charts::MAX_SERIES + 16;

#[cfg(feature = "read")]
#[derive(Default)]
struct SeriesIn {
    vals: Option<String>,
    label: Option<String>,
    class: Option<String>,
    /// Up to two: x values, or a bubble chart's y then x.
    domains: Vec<String>,
    lit: Option<String>,
    style: Option<String>,
    secondary: bool,
    totals: Option<String>,
    bins: Option<String>,
    trend_style: Option<String>,
    equation: (bool, bool),
    error_style: Option<String>,
}

/// A style property from ODF's own namespace or LibreOffice's older one.
#[cfg(feature = "read")]
fn prop<'a>(p: Option<&'a HashMap<String, String>>, name: &str) -> Option<&'a str> {
    let p = p?;
    p.get(&format!("chart:{name}")).or_else(|| p.get(&format!("loext:{name}"))).map(String::as_str)
}

#[cfg(feature = "read")]
fn num(s: Option<&str>) -> Option<f64> {
    s?.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

/// What one series shows beyond its values, from its styles and Officina's own
/// attributes; anything out of range is left off.
#[cfg(feature = "read")]
fn look_of(s: &SeriesIn, styles: &HashMap<String, HashMap<String, String>>) -> SeriesLook {
    let get = |n: &Option<String>| n.as_ref().and_then(|n| styles.get(n));
    let st = get(&s.style);
    let mut look = SeriesLook { labels: prop(st, "data-label-number").is_some_and(|v| v != "none"), secondary: s.secondary, ..SeriesLook::default() };
    if let Some(t) = s.trend_style.as_ref().map(|_| get(&s.trend_style)) {
        let deg = num(prop(t, "regression-max-degree")).map_or(2, |d| d.clamp(2.0, 6.0) as u8);
        let per = num(prop(t, "regression-period")).map_or(2, |d| d.clamp(2.0, 255.0) as u8);
        let kind = match prop(t, "regression-type").unwrap_or("linear") {
            "linear" => Some(TrendKind::Linear),
            "exponential" => Some(TrendKind::Exponential),
            "logarithmic" => Some(TrendKind::Logarithmic),
            "power" => Some(TrendKind::Power),
            "polynomial" => Some(TrendKind::Polynomial(deg)),
            "moving-average" => Some(TrendKind::MovingAverage(per)),
            _ => None,
        };
        let ext = |n| num(prop(t, n)).unwrap_or(0.0).clamp(0.0, 1e6);
        look.trend = kind.map(|kind| Trendline { kind, equation: s.equation.0, r_squared: s.equation.1, forward: ext("regression-extrapolate-forward"), backward: ext("regression-extrapolate-backward") });
    }
    // The error settings on the indicator's style, or (older files) the series'.
    let e = get(&s.error_style).filter(|e| prop(Some(e), "error-category").is_some()).or(st);
    let amount = |n| num(prop(e, n)).filter(|v| *v >= 0.0);
    look.errors = match prop(e, "error-category") {
        Some("constant") => amount("error-upper-limit").or_else(|| amount("error-lower-limit")).map(ErrorBars::Fixed),
        Some("percentage") => amount("error-percentage").map(ErrorBars::Percent),
        Some("standard-deviation") => Some(ErrorBars::StdDev(e.and_then(|e| e.get("officina:error-multiple")).and_then(|v| num(Some(v))).filter(|v| *v >= 0.0).unwrap_or(1.0))),
        Some("standard-error") => Some(ErrorBars::StdErr),
        _ => None,
    };
    if let Some(t) = &s.totals {
        let mut v: Vec<u32> = t.split_ascii_whitespace().take(MAX_TOTALS).filter_map(|x| x.parse().ok()).filter(|&x| x < MAX_ROWS).collect();
        v.sort_unstable();
        v.dedup();
        look.totals = v;
    }
    look.bins = s.bins.as_deref().and_then(|b| match b.split_once(' ')? {
        ("count", n) => n.parse::<u32>().ok().filter(|n| (1..=10_000).contains(n)).map(Bins::Count),
        ("width", x) => num(Some(x)).filter(|x| *x > 0.0).map(Bins::Width),
        _ => None,
    });
    look
}

#[cfg(feature = "read")]
/// One chart sub-document. `None`: not a chart of a kind Officina draws.
fn chart_doc(wb: &Workbook, data: &[u8], anchor: Anchor, rep: &mut Report) -> Option<Chart> {
    let mut r = Reader::from_reader(data);
    r.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut props: HashMap<String, Props> = HashMap::new();
    let mut styles: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut style: Option<String> = None;
    let mut class: Option<String> = None;
    let mut ours: Option<String> = None;
    let mut plot_style: Option<String> = None;
    let mut legend = None;
    let mut path: Vec<Vec<u8>> = Vec::new();
    let mut title = String::new();
    let mut axis_title = (String::new(), String::new());
    let mut axis_dim: Option<(char, bool)> = None;
    let mut cats: Option<Rect> = None;
    let mut series: Vec<SeriesIn> = Vec::new();
    let mut in_p = false;
    loop {
        let ev = match r.read_event_into(&mut buf) {
            Ok(ev) => ev,
            Err(_) => return None,
        };
        match &ev {
            Event::Start(e) | Event::Empty(e) => {
                let name = e.name().as_ref().to_vec();
                let a = attrs(e);
                let in_series = path.iter().any(|p| p.as_slice() == b"chart:series");
                match name.as_slice() {
                    b"style:style" => style = a.get("style:name").cloned(),
                    b"style:chart-properties" => {
                        if let Some(s) = &style {
                            let p = props.entry(s.clone()).or_default();
                            p.vertical |= a.get("chart:vertical").is_some_and(|v| v == "true");
                            p.stacked |= a.get("chart:stacked").is_some_and(|v| v == "true");
                            p.percent |= a.get("chart:percentage").is_some_and(|v| v == "true");
                            if styles.len() < MAX_STYLES || styles.contains_key(s) {
                                styles.entry(s.clone()).or_default().extend(a.into_iter().filter(|(k, v)| v.len() <= 64 && (k.starts_with("chart:") || k.starts_with("loext:") || k.starts_with("officina:"))));
                            }
                        }
                    }
                    b"chart:chart" => {
                        class = a.get("chart:class").cloned();
                        ours = a.get("officina:kind").cloned();
                    }
                    b"chart:legend" => {
                        legend = Some(match a.get("chart:legend-position").map(String::as_str) {
                            Some("start") => LegendPos::Left,
                            Some("top") => LegendPos::Top,
                            Some("bottom") => LegendPos::Bottom,
                            _ => LegendPos::Right,
                        })
                    }
                    b"chart:plot-area" => plot_style = a.get("chart:style-name").cloned(),
                    b"chart:axis" => axis_dim = a.get("chart:dimension").and_then(|d| d.chars().next()).map(|d| (d, a.get("chart:name").is_some_and(|n| n.starts_with("secondary")))),
                    b"chart:categories" => cats = a.get("table:cell-range-address").and_then(|x| range(wb, x)).filter(line),
                    b"chart:series" => {
                        if series.len() < officina_grid::charts::MAX_SERIES {
                            series.push(SeriesIn {
                                vals: a.get("chart:values-cell-range-address").cloned(),
                                label: a.get("chart:label-cell-address").cloned(),
                                class: a.get("chart:class").cloned(),
                                lit: a.get("loext:label-string").map(|t| t.chars().take(MAX_TEXT).collect()),
                                style: a.get("chart:style-name").cloned(),
                                secondary: a.get("chart:attached-axis").is_some_and(|x| x.starts_with("secondary")),
                                totals: a.get("officina:totals").map(|t| t.chars().take(8 * MAX_TOTALS).collect()),
                                bins: a.get("officina:bins").cloned(),
                                ..SeriesIn::default()
                            });
                        }
                    }
                    b"chart:domain" if in_series => {
                        if let Some(s) = series.last_mut() {
                            if s.domains.len() < 2 {
                                s.domains.extend(a.get("table:cell-range-address").cloned());
                            }
                        }
                    }
                    b"chart:regression-curve" if in_series => {
                        if let Some(s) = series.last_mut() {
                            if s.trend_style.is_none() {
                                s.trend_style = Some(a.get("chart:style-name").cloned().unwrap_or_default());
                            }
                        }
                    }
                    b"chart:equation" if in_series => {
                        if let Some(s) = series.last_mut() {
                            s.equation = (a.get("chart:display-equation").is_some_and(|v| v == "true"), a.get("chart:display-r-square").is_some_and(|v| v == "true"));
                        }
                    }
                    b"chart:error-indicator" if in_series => {
                        if let Some(s) = series.last_mut() {
                            if a.get("chart:dimension").is_none_or(|d| d == "y") {
                                s.error_style = a.get("chart:style-name").cloned();
                            }
                        }
                    }
                    b"text:p" => in_p = matches!(ev, Event::Start(_)),
                    _ => {}
                }
                if matches!(ev, Event::Start(_)) && path.len() < 64 {
                    path.push(name);
                } else if matches!(ev, Event::Start(_)) {
                    return None;
                }
            }
            Event::Text(t) if in_p => {
                let inside = |n: &[u8]| path.iter().any(|p| p.as_slice() == n);
                if let Ok(s) = t.unescape() {
                    let into = if inside(b"chart:axis") {
                        match axis_dim {
                            Some(('x', false)) => Some(&mut axis_title.0),
                            Some((_, false)) => Some(&mut axis_title.1),
                            _ => None,
                        }
                    } else if inside(b"chart:title") {
                        Some(&mut title)
                    } else {
                        None
                    };
                    if let Some(into) = into {
                        if inside(b"chart:title") && into.len() + s.len() <= MAX_TEXT {
                            into.push_str(&s);
                        }
                    }
                }
            }
            Event::End(_) => {
                if path.last().is_some_and(|p| p.as_slice() == b"text:p") {
                    in_p = false;
                }
                if path.last().is_some_and(|p| p.as_slice() == b"style:style") {
                    style = None;
                }
                path.pop();
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    let p = plot_style.as_ref().and_then(|s| props.get(s)).copied().unwrap_or_default();
    let kind_of = |c: &str| -> Option<ChartKind> {
        Some(match c.trim_start_matches("chart:") {
            "bar" if p.vertical => ChartKind::Bar,
            "bar" => ChartKind::Column,
            "line" => ChartKind::Line,
            "area" => ChartKind::Area,
            "scatter" => ChartKind::Scatter,
            "circle" => ChartKind::Pie,
            "ring" => ChartKind::Doughnut,
            "radar" | "filled-radar" => ChartKind::Radar,
            "bubble" => ChartKind::Bubble,
            "stock" => ChartKind::Stock,
            _ => return None,
        })
    };
    let kind = kind_of(class.as_deref()?)?;
    // Officina's own kinds that ODF lacks travel as bars, named on the chart.
    let kind = match (kind, ours.as_deref()) {
        (ChartKind::Column | ChartKind::Bar, Some("waterfall")) => ChartKind::Waterfall,
        (ChartKind::Column | ChartKind::Bar, Some("histogram")) => ChartKind::Histogram,
        (ChartKind::Column | ChartKind::Bar, Some("box-whisker")) => ChartKind::BoxWhisker,
        (k, _) => k,
    };
    let grouping = if p.percent {
        Grouping::Percent
    } else if p.stacked {
        Grouping::Stacked
    } else {
        Grouping::Standard
    };
    let mut out = Vec::new();
    for s in series {
        let range_of = |x: Option<&str>| x.and_then(|v| range(wb, v)).filter(line);
        // A bubble series' values are its sizes; its first domain the y values,
        // the second the x values.
        let (vals, sizes, xs) = if kind == ChartKind::Bubble {
            let sizes = range_of(s.vals.as_deref());
            match s.domains.first().map(|d| range_of(Some(d))) {
                Some(y) => (y, sizes.filter(|z| Some(*z) != y), range_of(s.domains.get(1).map(String::as_str))),
                None => (sizes, None, None),
            }
        } else {
            (range_of(s.vals.as_deref()), None, range_of(s.domains.first().map(String::as_str)))
        };
        let Some(vals) = vals else {
            rep.note(Kind::Dropped, "chart series without a cell range (typed-in values)");
            continue;
        };
        // LibreOffice names a ring chart's series "circle": a round chart's series take its kind.
        let k = if kind.whole() { kind } else { s.class.as_deref().and_then(kind_of).filter(|k| !k.whole()).unwrap_or(kind) };
        let name = s.label.as_deref().and_then(|l| range(wb, l)).map(|r| ChartText::Cell(CellAddr::new(r.sheet, r.r0, r.c0))).or_else(|| s.lit.clone().filter(|t| !t.is_empty()).map(|t| ChartText::Lit(t.as_str().into())));
        let cats = if k.xy() { xs } else { cats };
        let mut look = look_of(&s, &styles);
        look.sizes = sizes;
        out.push(Series { name, cats, vals, kind: (k != kind).then_some(k), look });
    }
    let opt = |s: String| (!s.is_empty()).then(|| Arc::from(s.as_str()));
    Some(Chart {
        kind,
        grouping,
        title: (!title.is_empty()).then(|| ChartText::Lit(title.as_str().into())),
        series: out,
        legend,
        x_title: opt(axis_title.0),
        y_title: opt(axis_title.1),
        anchor, alt: None
    })
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

fn addr(wb: &Workbook, r: &Rect) -> String {
    let sh = super::formula::sheet_a1(&wb.sheet_name(r.sheet));
    let a = |row: u32, col: u32| format!("{sh}.{}{}", officina_grid::addr::col_name(col), row + 1);
    format!("{}:{}", a(r.r0, r.c0), a(r.r1, r.c1))
}

const NS: &str = r#"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:chart="urn:oasis:names:tc:opendocument:xmlns:chart:1.0" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:loext="urn:org:documentfoundation:names:experimental:office:xmlns:loext:1.0" xmlns:officina="urn:atrium:officina:chart:1" office:version="1.3""#;

/// `Object N/content.xml` for `c`.
pub(crate) fn object_xml(wb: &Workbook, c: &Chart) -> String {
    let class = |k: ChartKind| match k {
        ChartKind::Column | ChartKind::Bar | ChartKind::Waterfall | ChartKind::Histogram | ChartKind::BoxWhisker => "chart:bar",
        ChartKind::Line => "chart:line",
        ChartKind::Area => "chart:area",
        ChartKind::Scatter => "chart:scatter",
        ChartKind::Pie => "chart:circle",
        ChartKind::Doughnut => "chart:ring",
        ChartKind::Radar => "chart:radar",
        ChartKind::Bubble => "chart:bubble",
        ChartKind::Stock => "chart:stock",
    };
    let mut o = String::new();
    let _ = write!(o, r#"<?xml version="1.0" encoding="UTF-8"?><office:document-content {NS}><office:automatic-styles>"#);
    let _ = write!(
        o,
        r#"<style:style style:name="plot" style:family="chart"><style:chart-properties chart:vertical="{}" chart:stacked="{}" chart:percentage="{}"/></style:style>"#,
        c.kind == ChartKind::Bar,
        c.grouping == Grouping::Stacked,
        c.grouping == Grouping::Percent
    );
    // Each series' own styles: data labels, its trendline and its error bars.
    for (i, s) in c.series.iter().enumerate() {
        let l = &s.look;
        if l.labels {
            let _ = write!(o, r#"<style:style style:name="s{i}" style:family="chart"><style:chart-properties chart:data-label-number="value"/></style:style>"#);
        }
        if let Some(t) = &l.trend {
            let (ty, extra) = match t.kind {
                TrendKind::Linear => ("linear", String::new()),
                TrendKind::Exponential => ("exponential", String::new()),
                TrendKind::Logarithmic => ("logarithmic", String::new()),
                TrendKind::Power => ("power", String::new()),
                TrendKind::Polynomial(n) => ("polynomial", format!(r#" chart:regression-max-degree="{n}""#)),
                TrendKind::MovingAverage(n) => ("moving-average", format!(r#" chart:regression-period="{n}""#)),
            };
            let _ = write!(o, r#"<style:style style:name="t{i}" style:family="chart"><style:chart-properties chart:regression-type="{ty}"{extra} chart:regression-extrapolate-forward="{}" chart:regression-extrapolate-backward="{}"/></style:style>"#, t.forward, t.backward);
        }
        if let Some(e) = &l.errors {
            let detail = match *e {
                ErrorBars::Fixed(v) => format!(r#"chart:error-category="constant" chart:error-lower-limit="{v}" chart:error-upper-limit="{v}""#),
                ErrorBars::Percent(v) => format!(r#"chart:error-category="percentage" chart:error-percentage="{v}""#),
                ErrorBars::StdDev(v) => format!(r#"chart:error-category="standard-deviation" officina:error-multiple="{v}""#),
                ErrorBars::StdErr => r#"chart:error-category="standard-error""#.to_string(),
            };
            let _ = write!(o, r#"<style:style style:name="e{i}" style:family="chart"><style:chart-properties {detail} chart:error-lower-indicator="true" chart:error-upper-indicator="true"/></style:style>"#);
        }
    }
    let ours = match c.kind {
        ChartKind::Waterfall => r#" officina:kind="waterfall""#,
        ChartKind::Histogram => r#" officina:kind="histogram""#,
        ChartKind::BoxWhisker => r#" officina:kind="box-whisker""#,
        _ => "",
    };
    let _ = write!(o, r#"</office:automatic-styles><office:body><office:chart><chart:chart svg:width="12cm" svg:height="8cm" chart:class="{}"{ours}>"#, class(c.kind));
    if let Some(t) = &c.title {
        let _ = write!(o, "<chart:title><text:p>{}</text:p></chart:title>", esc(&Chart::text(wb, t)));
    }
    if let Some(l) = c.legend {
        let p = match l {
            LegendPos::Right => "end",
            LegendPos::Left => "start",
            LegendPos::Top => "top",
            LegendPos::Bottom => "bottom",
        };
        let _ = write!(o, r#"<chart:legend chart:legend-position="{p}"/>"#);
    }
    o.push_str(r#"<chart:plot-area chart:style-name="plot">"#);
    if !c.kind.round() {
        o.push_str(r#"<chart:axis chart:dimension="x" chart:name="primary-x">"#);
        if let Some(t) = &c.x_title {
            let _ = write!(o, "<chart:title><text:p>{}</text:p></chart:title>", esc(t));
        }
        if !c.kind.xy() {
            if let Some(cr) = c.series.iter().find_map(|s| s.cats) {
                let _ = write!(o, r#"<chart:categories table:cell-range-address="{}"/>"#, esc(&addr(wb, &cr)));
            }
        }
        o.push_str(r#"</chart:axis><chart:axis chart:dimension="y" chart:name="primary-y">"#);
        if let Some(t) = &c.y_title {
            let _ = write!(o, "<chart:title><text:p>{}</text:p></chart:title>", esc(t));
        }
        o.push_str(r#"<chart:grid chart:class="major"/></chart:axis>"#);
        if c.series.iter().any(|s| s.look.secondary) {
            o.push_str(r#"<chart:axis chart:dimension="y" chart:name="secondary-y"/>"#);
        }
    } else if let Some(cr) = c.series.iter().find_map(|s| s.cats) {
        let _ = write!(o, r#"<chart:axis chart:dimension="x" chart:name="primary-x"><chart:categories table:cell-range-address="{}"/></chart:axis><chart:axis chart:dimension="y" chart:name="primary-y"/>"#, esc(&addr(wb, &cr)));
    }
    for (i, s) in c.series.iter().enumerate() {
        let k = s.kind.unwrap_or(c.kind);
        // A bubble series' values are its sizes (its own values when it has none),
        // then its y and x values as domains.
        let vals = if k == ChartKind::Bubble { s.look.sizes.unwrap_or(s.vals) } else { s.vals };
        let _ = write!(o, r#"<chart:series chart:values-cell-range-address="{}""#, esc(&addr(wb, &vals)));
        if let Some(ChartText::Cell(a)) = &s.name {
            let _ = write!(o, r#" chart:label-cell-address="{}""#, esc(&addr(wb, &Rect { sheet: a.sheet, r0: a.row, c0: a.col, r1: a.row, c1: a.col })));
        }
        if s.look.labels {
            let _ = write!(o, r#" chart:style-name="s{i}""#);
        }
        if s.look.secondary {
            o.push_str(r#" chart:attached-axis="secondary-y""#);
        }
        if !s.look.totals.is_empty() {
            let t: Vec<String> = s.look.totals.iter().map(u32::to_string).collect();
            let _ = write!(o, r#" officina:totals="{}""#, t.join(" "));
        }
        match s.look.bins {
            Some(Bins::Count(n)) => {
                let _ = write!(o, r#" officina:bins="count {n}""#);
            }
            Some(Bins::Width(x)) => {
                let _ = write!(o, r#" officina:bins="width {x}""#);
            }
            None => {}
        }
        // A typed-in name has no place in ODF; LibreOffice's loext:label-string (read
        // above) makes LibreOffice 24.2 drop the whole chart, so it is not written.
        // The series' own kind only when it differs (LibreOffice loses ring series
        // that name theirs).
        match s.kind.filter(|k| *k != c.kind) {
            Some(k) => {
                let _ = write!(o, r#" chart:class="{}">"#, class(k));
            }
            None => o.push('>'),
        }
        if k == ChartKind::Bubble {
            let _ = write!(o, r#"<chart:domain table:cell-range-address="{}"/>"#, esc(&addr(wb, &s.vals)));
        }
        if k.xy() {
            if let Some(x) = &s.cats {
                let _ = write!(o, r#"<chart:domain table:cell-range-address="{}"/>"#, esc(&addr(wb, x)));
            }
        }
        if let Some(t) = &s.look.trend {
            let _ = write!(o, r#"<chart:regression-curve chart:style-name="t{i}"><chart:equation chart:display-equation="{}" chart:display-r-square="{}"/></chart:regression-curve>"#, t.equation, t.r_squared);
        }
        if s.look.errors.is_some() {
            let _ = write!(o, r#"<chart:error-indicator chart:style-name="e{i}" chart:dimension="y"/>"#);
        }
        o.push_str("</chart:series>");
    }
    o.push_str("</chart:plot-area></chart:chart></office:chart></office:body></office:document-content>");
    o
}

/// Package part for an embedded picture.
pub(crate) fn picture_path(p: &officina_grid::charts::Picture, i: usize) -> Option<String> {
    let ext = officina_grid::charts::Picture::extension(&p.bytes)?;
    Some(format!("Pictures/image{}.{ext}", i + 1))
}

pub(crate) fn picture_xml(wb: &Workbook, p: &officina_grid::charts::Picture, i: usize) -> Option<String> {
    let path = picture_path(p, i)?;
    let [x, y, w, h] = frame_inches(wb, p.anchor);
    let desc = p.alt.as_deref().map_or(String::new(), |a| format!("<svg:desc>{}</svg:desc>", crate::xml::esc(a)));
    Some(format!(
        r#"<draw:frame draw:name="Picture {}" svg:x="{x}" svg:y="{y}" svg:width="{w}" svg:height="{h}"><draw:image xlink:href="{path}" xlink:type="simple" xlink:show="embed" xlink:actuate="onLoad"/>{desc}</draw:frame>"#,
        i + 1,
    ))
}

fn frame_inches(wb: &Workbook, a: Anchor) -> [String; 4] {
    let sh = wb.sheet(a.sheet);
    let pos = |(r, col, dx, dy): (u32, u32, f32, f32)| (sh.col_sizes.offset(col) + dx as f64 / PT_PX, sh.row_sizes.offset(r) + dy as f64 / PT_PX);
    let (x0, y0) = pos(a.from);
    let (x1, y1) = pos(a.to);
    [x0, y0, (x1 - x0).max(1.0), (y1 - y0).max(1.0)].map(|px| format!("{:.6}in", px / 96.0))
}

/// The `draw:frame` for chart `i` (shown as `Object {i+1}`), anchored to its sheet.
pub(crate) fn frame_xml(wb: &Workbook, c: &Chart, i: usize) -> String {
    let [x, y, w, h] = frame_inches(wb, c.anchor);
    let desc = c.alt.as_deref().map_or(String::new(), |a| format!("<svg:desc>{}</svg:desc>", crate::xml::esc(a)));
    format!(
        r#"<draw:frame draw:z-index="{i}" draw:name="Chart {}" svg:x="{}" svg:y="{}" svg:width="{}" svg:height="{}"><draw:object xlink:href="./Object {}" xlink:type="simple" xlink:show="embed" xlink:actuate="onLoad"/>{desc}</draw:frame>"#,
        i + 1,
        x, y, w, h,
        i + 1
    )
}
