//! `.ods` charts: a `draw:frame` holding a `draw:object` whose sub-document
//! (`Object N/content.xml`) is an ODF chart, anchored to a cell (the frame sits in it)
//! or to the sheet (`table:shapes`).
//!
//! Read: bar (vertical bars are Excel's horizontal "bar"), line, area, scatter,
//! circle (pie) and ring (doughnut), stacked and percent, series by cell ranges,
//! titles, axis titles and the legend's place. Written the same way, anchored to the
//! sheet at the chart's place.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use quick_xml::events::Event;
use quick_xml::Reader;

use officina_grid::addr::{CellAddr, Rect, MAX_COLS, MAX_ROWS};
use officina_grid::charts::{Anchor, Chart, ChartKind, ChartText, Grouping, LegendPos, Series};
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
}

const PT_PX: f64 = 72.0 / 96.0;
const MAX_TEXT: usize = officina_grid::charts::MAX_TITLE;

fn attrs(e: &quick_xml::events::BytesStart) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for a in e.attributes().with_checks(false).flatten() {
        if let Ok(v) = a.unescape_value() {
            m.insert(String::from_utf8_lossy(a.key.as_ref()).into_owned(), v.into_owned());
        }
    }
    m
}

/// `Data.B2:Data.B13`, `'My sheet'.$B$2:.$B$13` → the range.
fn range(wb: &Workbook, a: &str) -> Option<Rect> {
    // Several ranges (space-separated): the first.
    let a = a.split(' ').next()?;
    let a1 = super::formula::from_of(&format!("of:=[{a}]"))?;
    let a1 = a1.trim_start_matches('=');
    let (sh, rf) = a1.rsplit_once('!')?;
    let sh = sh.strip_prefix('\'').and_then(|s| s.strip_suffix('\'')).map_or(sh.to_string(), |s| s.replace("''", "'"));
    let rf = rf.replace('$', "");
    let (r0, c0, r1, c1) = crate::xlsx::read::parse_ref(&rf).or_else(|| officina_grid::addr::parse_a1(&rf).map(|(r, c)| (r, c, r, c)))?;
    Some(Rect { sheet: wb.sheet_id(&sh)?, r0, c0, r1, c1 })
}

fn line(r: &Rect) -> bool {
    r.r0 == r.r1 || r.c0 == r.c1
}

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

/// Turns the frames read into charts.
pub(crate) fn read_frames(wb: &mut Workbook, z: &Archive, frames: Vec<FrameIn>, rep: &mut Report) {
    for f in frames {
        let Some(href) = f.href.as_deref() else {
            rep.note(Kind::Dropped, "images and drawings");
            continue;
        };
        let dir = href.trim_start_matches("./").trim_end_matches('/');
        let Ok(Some(data)) = z.read_name(&format!("{dir}/content.xml")) else {
            rep.note(Kind::Dropped, "embedded objects other than charts");
            continue;
        };
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
        match chart_doc(wb, &data, anchor, rep) {
            Some(c) => {
                if wb.add_chart(c).is_err() {
                    rep.note(Kind::Dropped, "charts this engine can't use");
                }
            }
            None => rep.note(Kind::Dropped, "embedded objects other than charts, and chart kinds Officina doesn't draw yet"),
        }
    }
}

#[derive(Default, Clone, Copy)]
struct Props {
    vertical: bool,
    stacked: bool,
    percent: bool,
}

/// One chart sub-document. `None`: not a chart of a kind Officina draws.
fn chart_doc(wb: &Workbook, data: &[u8], anchor: Anchor, rep: &mut Report) -> Option<Chart> {
    let mut r = Reader::from_reader(data);
    r.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut props: HashMap<String, Props> = HashMap::new();
    let mut style: Option<String> = None;
    let mut class: Option<String> = None;
    let mut plot_style: Option<String> = None;
    let mut legend = None;
    let mut path: Vec<Vec<u8>> = Vec::new();
    let mut title = String::new();
    let mut axis_title = (String::new(), String::new());
    let mut axis_dim: Option<char> = None;
    let mut cats: Option<Rect> = None;
    let mut series: Vec<(Option<String>, Option<String>, Option<String>, Option<String>, Option<String>)> = Vec::new(); // values, label, class, domain, literal label
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
                match name.as_slice() {
                    b"style:style" => style = a.get("style:name").cloned(),
                    b"style:chart-properties" => {
                        if let Some(s) = &style {
                            let p = props.entry(s.clone()).or_default();
                            p.vertical |= a.get("chart:vertical").is_some_and(|v| v == "true");
                            p.stacked |= a.get("chart:stacked").is_some_and(|v| v == "true");
                            p.percent |= a.get("chart:percentage").is_some_and(|v| v == "true");
                        }
                    }
                    b"chart:chart" => class = a.get("chart:class").cloned(),
                    b"chart:legend" => {
                        legend = Some(match a.get("chart:legend-position").map(String::as_str) {
                            Some("start") => LegendPos::Left,
                            Some("top") => LegendPos::Top,
                            Some("bottom") => LegendPos::Bottom,
                            _ => LegendPos::Right,
                        })
                    }
                    b"chart:plot-area" => plot_style = a.get("chart:style-name").cloned(),
                    b"chart:axis" => axis_dim = a.get("chart:dimension").and_then(|d| d.chars().next()),
                    b"chart:categories" => cats = a.get("table:cell-range-address").and_then(|x| range(wb, x)).filter(line),
                    b"chart:series" => {
                        if series.len() < officina_grid::charts::MAX_SERIES {
                            series.push((a.get("chart:values-cell-range-address").cloned(), a.get("chart:label-cell-address").cloned(), a.get("chart:class").cloned(), None, a.get("loext:label-string").map(|t| t.chars().take(MAX_TEXT).collect())));
                        }
                    }
                    b"chart:domain" => {
                        if let Some(s) = series.last_mut() {
                            if s.3.is_none() {
                                s.3 = a.get("table:cell-range-address").cloned();
                            }
                        }
                    }
                    b"text:p" => in_p = matches!(ev, Event::Start(_)),
                    _ => {}
                }
                if matches!(ev, Event::Start(_)) {
                    path.push(name);
                }
            }
            Event::Text(t) if in_p => {
                let inside = |n: &[u8]| path.iter().any(|p| p.as_slice() == n);
                if let Ok(s) = t.unescape() {
                    let into = if inside(b"chart:axis") {
                        if axis_dim == Some('x') {
                            Some(&mut axis_title.0)
                        } else {
                            Some(&mut axis_title.1)
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
            _ => return None,
        })
    };
    let kind = kind_of(class.as_deref()?)?;
    let grouping = if p.percent {
        Grouping::Percent
    } else if p.stacked {
        Grouping::Stacked
    } else {
        Grouping::Standard
    };
    let mut out = Vec::new();
    for (vals, label, sclass, domain, lit) in series {
        let Some(vals) = vals.as_deref().and_then(|v| range(wb, v)).filter(line) else {
            rep.note(Kind::Dropped, "chart series without a cell range (typed-in values)");
            continue;
        };
        // LibreOffice names a ring chart's series "circle": a round chart's series take its kind.
        let k = if kind.round() { kind } else { sclass.as_deref().and_then(kind_of).unwrap_or(kind) };
        let name = label.as_deref().and_then(|l| range(wb, l)).map(|r| ChartText::Cell(CellAddr::new(r.sheet, r.r0, r.c0))).or_else(|| lit.filter(|t| !t.is_empty()).map(|t| ChartText::Lit(t.as_str().into())));
        let cats = if k == ChartKind::Scatter { domain.as_deref().and_then(|d| range(wb, d)).filter(line) } else { cats };
        out.push(Series { name, cats, vals, kind: (k != kind).then_some(k) });
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
        anchor,
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

const NS: &str = r#"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" xmlns:chart="urn:oasis:names:tc:opendocument:xmlns:chart:1.0" xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" xmlns:xlink="http://www.w3.org/1999/xlink" xmlns:loext="urn:org:documentfoundation:names:experimental:office:xmlns:loext:1.0" office:version="1.3""#;

/// `Object N/content.xml` for `c`.
pub(crate) fn object_xml(wb: &Workbook, c: &Chart) -> String {
    let class = |k: ChartKind| match k {
        ChartKind::Column | ChartKind::Bar => "chart:bar",
        ChartKind::Line => "chart:line",
        ChartKind::Area => "chart:area",
        ChartKind::Scatter => "chart:scatter",
        ChartKind::Pie => "chart:circle",
        ChartKind::Doughnut => "chart:ring",
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
    let _ = write!(o, r#"</office:automatic-styles><office:body><office:chart><chart:chart svg:width="12cm" svg:height="8cm" chart:class="{}">"#, class(c.kind));
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
        if c.kind != ChartKind::Scatter {
            if let Some(cr) = c.series.iter().find_map(|s| s.cats) {
                let _ = write!(o, r#"<chart:categories table:cell-range-address="{}"/>"#, esc(&addr(wb, &cr)));
            }
        }
        o.push_str(r#"</chart:axis><chart:axis chart:dimension="y" chart:name="primary-y">"#);
        if let Some(t) = &c.y_title {
            let _ = write!(o, "<chart:title><text:p>{}</text:p></chart:title>", esc(t));
        }
        o.push_str(r#"<chart:grid chart:class="major"/></chart:axis>"#);
    } else if let Some(cr) = c.series.iter().find_map(|s| s.cats) {
        let _ = write!(o, r#"<chart:axis chart:dimension="x" chart:name="primary-x"><chart:categories table:cell-range-address="{}"/></chart:axis><chart:axis chart:dimension="y" chart:name="primary-y"/>"#, esc(&addr(wb, &cr)));
    }
    for s in &c.series {
        let _ = write!(o, r#"<chart:series chart:values-cell-range-address="{}""#, esc(&addr(wb, &s.vals)));
        if let Some(ChartText::Cell(a)) = &s.name {
            let _ = write!(o, r#" chart:label-cell-address="{}""#, esc(&addr(wb, &Rect { sheet: a.sheet, r0: a.row, c0: a.col, r1: a.row, c1: a.col })));
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
        if s.kind.unwrap_or(c.kind) == ChartKind::Scatter {
            if let Some(x) = &s.cats {
                let _ = write!(o, r#"<chart:domain table:cell-range-address="{}"/>"#, esc(&addr(wb, x)));
            }
        }
        o.push_str("</chart:series>");
    }
    o.push_str("</chart:plot-area></chart:chart></office:chart></office:body></office:document-content>");
    o
}

/// The `draw:frame` for chart `i` (shown as `Object {i+1}`), anchored to its sheet.
pub(crate) fn frame_xml(wb: &Workbook, c: &Chart, i: usize) -> String {
    let a = c.anchor;
    let sh = wb.sheet(a.sheet);
    let pos = |(r, col, dx, dy): (u32, u32, f32, f32)| (sh.col_sizes.offset(col) + dx as f64 / PT_PX, sh.row_sizes.offset(r) + dy as f64 / PT_PX);
    let (x0, y0) = pos(a.from);
    let (x1, y1) = pos(a.to);
    let inch = |px: f64| format!("{:.4}in", px / 96.0);
    format!(
        r#"<draw:frame draw:z-index="{i}" draw:name="Chart {}" svg:x="{}" svg:y="{}" svg:width="{}" svg:height="{}"><draw:object xlink:href="./Object {}" xlink:type="simple" xlink:show="embed" xlink:actuate="onLoad"/></draw:frame>"#,
        i + 1,
        inch(x0),
        inch(y0),
        inch((x1 - x0).max(1.0)),
        inch((y1 - y0).max(1.0)),
        i + 1
    )
}
