//! `.xlsx` charts: a sheet's drawing part (`xl/drawings/drawingN.xml`) anchors each
//! chart (`xl/charts/chartN.xml`, DrawingML charts) to cells.
//!
//! Read: column, bar, line, area, scatter, pie and doughnut groups (3-D ones drawn
//! flat), combos of them, series names, categories and values by reference, titles,
//! axis titles and the legend's place. Other chart kinds, pictures and shapes are
//! reported.

use std::fmt::Write as _;
use std::sync::Arc;

use quick_xml::events::Event;

use officina_grid::addr::{col_name, CellAddr, Rect};
use officina_grid::charts::{Anchor, Chart, ChartKind, ChartText, Grouping, LegendPos, Series};
use officina_grid::Workbook;

use super::read::{attr, decode_escapes, parse_ref, rels, xml, XlsxError};
use crate::report::{Kind, Report};
use crate::zip::Archive;

/// EMU per point.
const EMU_PT: f64 = 12_700.0;
/// Text read from a chart part, at most.
const MAX_TEXT: usize = officina_grid::charts::MAX_TITLE;

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

/// Reads the charts of sheet `sheet` (its part is `part`).
pub(crate) fn read_sheet(wb: &mut Workbook, sheet: u16, z: &Archive, part: &str, rep: &mut Report) -> Result<(), XlsxError> {
    let drawings: Vec<String> = rels(z, part)?.into_values().filter(|(t, _)| t.ends_with("/drawing")).map(|(_, p)| p).collect();
    for d in drawings {
        let Some(data) = z.read_name(&d)? else { continue };
        let drels = rels(z, &d)?;
        let mut r = xml(&data);
        let mut buf = Vec::new();
        // The anchor being read: from/to (col, colOff, row, rowOff), which corner.
        let mut corner = 0usize;
        let mut field: Option<&'static str> = None;
        let mut pos = [[0i64; 4]; 2];
        let mut in_anchor = false;
        let mut chart: Option<String> = None;
        let mut other = false;
        loop {
            match r.read_event_into(&mut buf)? {
                Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                    b"twoCellAnchor" => {
                        in_anchor = true;
                        chart = None;
                        other = false;
                        pos = [[0; 4]; 2];
                    }
                    b"oneCellAnchor" | b"absoluteAnchor" => rep.note(Kind::Dropped, "charts, pictures and shapes not anchored to two cells"),
                    b"from" => corner = 0,
                    b"to" => corner = 1,
                    b"col" => field = Some("col"),
                    b"colOff" => field = Some("colOff"),
                    b"row" => field = Some("row"),
                    b"rowOff" => field = Some("rowOff"),
                    b"chart" if in_anchor => {
                        if let Some(id) = attr(&e, b"id")? {
                            chart = drels.get(&id).map(|(_, p)| p.clone());
                        }
                    }
                    b"pic" | b"sp" | b"cxnSp" | b"grpSp" if in_anchor => other = true,
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
                    b"twoCellAnchor" => {
                        in_anchor = false;
                        if other {
                            rep.note(Kind::Dropped, "pictures and shapes");
                        }
                        if let Some(p) = chart.take() {
                            let cl = |v: i64, max: u32| v.clamp(0, max as i64 - 1) as u32;
                            let off = |v: i64| (v as f64 / EMU_PT).clamp(0.0, 1e6) as f32;
                            let anchor = Anchor {
                                sheet,
                                from: (cl(pos[0][2], officina_grid::addr::MAX_ROWS), cl(pos[0][0], officina_grid::addr::MAX_COLS), off(pos[0][1]), off(pos[0][3])),
                                to: (cl(pos[1][2], officina_grid::addr::MAX_ROWS), cl(pos[1][0], officina_grid::addr::MAX_COLS), off(pos[1][1]), off(pos[1][3])),
                            };
                            if let Some(data) = z.read_name(&p)? {
                                match chart_part(wb, &data, anchor, rep)? {
                                    Some(c) => {
                                        if wb.add_chart(c).is_err() {
                                            rep.note(Kind::Dropped, "charts this engine can't use");
                                        }
                                    }
                                    None => rep.note(Kind::Dropped, "charts of kinds Officina doesn't draw yet (radar, stock, surface, bubble)"),
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

#[derive(Default)]
struct SerIn {
    name: Option<ChartText>,
    cats: Option<Rect>,
    vals: Option<Rect>,
}

/// One chart part. `None`: no group of a kind Officina draws.
fn chart_part(wb: &Workbook, data: &[u8], anchor: Anchor, rep: &mut Report) -> Result<Option<Chart>, XlsxError> {
    let mut r = xml(data);
    let mut buf = Vec::new();
    // Element path (local names), for context.
    let mut path: Vec<Vec<u8>> = Vec::new();
    let mut kind: Option<ChartKind> = None;
    let mut grouping = Grouping::Standard;
    let mut series: Vec<Series> = Vec::new();
    let mut group_kind: Option<ChartKind> = None;
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
                    g @ (b"barChart" | b"bar3DChart" | b"lineChart" | b"line3DChart" | b"areaChart" | b"area3DChart" | b"scatterChart" | b"pieChart" | b"pie3DChart" | b"doughnutChart" | b"ofPieChart") => {
                        if g.windows(2).any(|w| w == b"3D") {
                            rep.note(Kind::Approximate, "3-D charts drawn flat");
                        }
                        group_kind = Some(match g {
                            b"barChart" | b"bar3DChart" => ChartKind::Column,
                            b"lineChart" | b"line3DChart" => ChartKind::Line,
                            b"areaChart" | b"area3DChart" => ChartKind::Area,
                            b"scatterChart" => ChartKind::Scatter,
                            b"doughnutChart" => ChartKind::Doughnut,
                            _ => ChartKind::Pie,
                        });
                    }
                    b"radarChart" | b"stockChart" | b"surfaceChart" | b"surface3DChart" | b"bubbleChart" => {
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
                            if inside(b"tx") && s.name.is_none() {
                                s.name = match what {
                                    1 => range(wb, &t).map(|r| ChartText::Cell(CellAddr::new(r.sheet, r.r0, r.c0))),
                                    _ => Some(ChartText::Lit(decode_escapes(&t).as_ref().into())),
                                };
                            } else if what == 1 && (inside(b"cat") || inside(b"xVal")) {
                                s.cats = range(wb, &t);
                            } else if what == 1 && (inside(b"val") || inside(b"yVal")) {
                                s.vals = range(wb, &t);
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
                    b"ser" => {
                        if let (Some(s), Some(gk)) = (ser.take(), group_kind) {
                            match s.vals {
                                Some(vals) if vals.r0 == vals.r1 || vals.c0 == vals.c1 => {
                                    let k = *kind.get_or_insert(gk);
                                    if series.len() < officina_grid::charts::MAX_SERIES {
                                        series.push(Series { name: s.name, cats: s.cats.filter(|c| c.r0 == c.r1 || c.c0 == c.c1), vals, kind: (gk != k).then_some(gk) });
                                    }
                                }
                                _ => rep.note(Kind::Dropped, "chart series without a cell range (typed-in values)"),
                            }
                        }
                    }
                    b"title" if inside(b"catAx") || inside(b"valAx") || inside(b"dateAx") => {
                        let cat = inside(b"catAx") || inside(b"dateAx") || ax_pos_b;
                        axis_titles.push((cat, std::mem::take(&mut ax_text)));
                    }
                    b"catAx" | b"valAx" | b"dateAx" | b"serAx" => {
                        ax_pos_b = false;
                    }
                    b"barChart" | b"bar3DChart" | b"lineChart" | b"line3DChart" | b"areaChart" | b"area3DChart" | b"scatterChart" | b"pieChart" | b"pie3DChart" | b"doughnutChart" | b"ofPieChart" | b"radarChart" | b"stockChart" | b"surfaceChart" | b"surface3DChart" | b"bubbleChart" => group_kind = None,
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
        rep.note(Kind::Dropped, "chart series of kinds Officina doesn't draw yet (radar, stock, surface, bubble)");
    }
    if title.is_none() && !title_text.is_empty() {
        title = Some(ChartText::Lit(decode_escapes(&title_text).as_ref().into()));
    }
    // Excel's automatic title: a chart of one series is named after it.
    if title.is_none() && !deleted && series.len() == 1 {
        title = series[0].name.clone();
    }
    let pick = |cat: bool| axis_titles.iter().find(|(c, t)| *c == cat && !t.is_empty()).map(|(_, t)| Arc::from(t.as_str()));
    Ok(Some(Chart { kind, grouping, title, series, legend, x_title: pick(true), y_title: pick(false), anchor }))
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

/// `xl/charts/chartN.xml` for `c`.
pub(crate) fn chart_xml(wb: &Workbook, c: &Chart) -> String {
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
    // One group per kind, in the order the series first use them.
    let mut kinds: Vec<ChartKind> = Vec::new();
    for d in &data {
        if !kinds.contains(&d.kind) {
            kinds.push(d.kind);
        }
    }
    if kinds.is_empty() {
        kinds.push(c.kind);
    }
    let round = kinds.iter().all(|k| k.round());
    let scatter = kinds.contains(&ChartKind::Scatter);
    let (ax_a, ax_b) = (500_000_001u32, 500_000_002u32);
    for k in &kinds {
        let grouping = match c.grouping {
            Grouping::Standard if matches!(k, ChartKind::Column | ChartKind::Bar) => "clustered",
            Grouping::Standard => "standard",
            Grouping::Stacked => "stacked",
            Grouping::Percent => "percentStacked",
        };
        match k {
            ChartKind::Column | ChartKind::Bar => {
                let _ = write!(o, r#"<c:barChart><c:barDir val="{}"/><c:grouping val="{grouping}"/><c:varyColors val="0"/>"#, if *k == ChartKind::Bar { "bar" } else { "col" });
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
        }
        for (i, (s, d)) in c.series.iter().zip(&data).enumerate() {
            if d.kind != *k {
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
            if *k == ChartKind::Scatter {
                o.push_str(r#"<c:spPr><a:ln w="19050"><a:noFill/></a:ln></c:spPr>"#);
            }
            let (cat_el, val_el) = if *k == ChartKind::Scatter { ("xVal", "yVal") } else { ("cat", "val") };
            if let Some(cr) = &s.cats {
                let _ = write!(o, "<c:{cat_el}><c:strRef><c:f>{}</c:f><c:strCache><c:ptCount val=\"{}\"/>", esc(&ref_text(wb, cr)), d.cats.len());
                for (j, t) in d.cats.iter().enumerate() {
                    let _ = write!(o, r#"<c:pt idx="{j}"><c:v>{}</c:v></c:pt>"#, esc(t));
                }
                let _ = write!(o, "</c:strCache></c:strRef></c:{cat_el}>");
            }
            let _ = write!(o, "<c:{val_el}><c:numRef><c:f>{}</c:f><c:numCache><c:formatCode>General</c:formatCode><c:ptCount val=\"{}\"/>", esc(&ref_text(wb, &s.vals)), d.vals.len());
            for (j, v) in d.vals.iter().enumerate() {
                if let Some(v) = v {
                    let _ = write!(o, r#"<c:pt idx="{j}"><c:v>{v}</c:v></c:pt>"#);
                }
            }
            let _ = write!(o, "</c:numCache></c:numRef></c:{val_el}>");
            if *k == ChartKind::Line {
                o.push_str(r#"<c:smooth val="0"/>"#);
            }
            o.push_str("</c:ser>");
        }
        match k {
            ChartKind::Column | ChartKind::Bar => {
                let overlap = if c.grouping == Grouping::Standard { "" } else { r#"<c:overlap val="100"/>"# };
                let _ = write!(o, r#"<c:gapWidth val="150"/>{overlap}<c:axId val="{ax_a}"/><c:axId val="{ax_b}"/></c:barChart>"#);
            }
            ChartKind::Line => {
                let _ = write!(o, r#"<c:marker val="1"/><c:axId val="{ax_a}"/><c:axId val="{ax_b}"/></c:lineChart>"#);
            }
            ChartKind::Area => {
                let _ = write!(o, r#"<c:axId val="{ax_a}"/><c:axId val="{ax_b}"/></c:areaChart>"#);
            }
            ChartKind::Scatter => {
                let _ = write!(o, r#"<c:axId val="{ax_a}"/><c:axId val="{ax_b}"/></c:scatterChart>"#);
            }
            ChartKind::Pie => o.push_str(r#"<c:firstSliceAng val="0"/></c:pieChart>"#),
            ChartKind::Doughnut => o.push_str(r#"<c:firstSliceAng val="0"/><c:holeSize val="50"/></c:doughnutChart>"#),
        }
    }
    if !round {
        let horizontal = kinds.contains(&ChartKind::Bar);
        let (cat_pos, val_pos) = if horizontal { ("l", "b") } else { ("b", "l") };
        let ax_title = |o: &mut String, t: &Option<Arc<str>>| {
            if let Some(t) = t {
                o.push_str("<c:title>");
                rich(o, t);
                o.push_str("</c:title>");
            }
        };
        if scatter {
            let _ = write!(o, r#"<c:valAx><c:axId val="{ax_a}"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="0"/><c:axPos val="b"/>"#);
        } else {
            let _ = write!(o, r#"<c:catAx><c:axId val="{ax_a}"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="0"/><c:axPos val="{cat_pos}"/>"#);
        }
        ax_title(&mut o, &c.x_title);
        let _ = write!(o, r#"<c:numFmt formatCode="General" sourceLinked="1"/><c:majorTickMark val="out"/><c:minorTickMark val="none"/><c:tickLblPos val="nextTo"/><c:crossAx val="{ax_b}"/><c:crosses val="autoZero"/>"#);
        o.push_str(if scatter { r#"<c:crossBetween val="midCat"/></c:valAx>"# } else { r#"<c:auto val="1"/><c:lblAlgn val="ctr"/><c:lblOffset val="100"/><c:noMultiLvlLbl val="0"/></c:catAx>"# });
        let _ = write!(o, r#"<c:valAx><c:axId val="{ax_b}"/><c:scaling><c:orientation val="minMax"/></c:scaling><c:delete val="0"/><c:axPos val="{}"/><c:majorGridlines/>"#, if scatter { "l" } else { val_pos });
        ax_title(&mut o, &c.y_title);
        let _ = write!(o, r#"<c:numFmt formatCode="General" sourceLinked="1"/><c:majorTickMark val="out"/><c:minorTickMark val="none"/><c:tickLblPos val="nextTo"/><c:crossAx val="{ax_a}"/><c:crosses val="autoZero"/><c:crossBetween val="between"/></c:valAx>"#);
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

/// `xl/drawings/drawingN.xml` for the charts `ids` (their numbers in the package,
/// from 1) of one sheet; the drawing's relationship `rId{j+1}` is chart `j`.
pub(crate) fn drawing_xml(wb: &Workbook, ids: &[usize]) -> String {
    let mut o = String::from(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><xdr:wsDr xmlns:xdr="http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing" xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">"#);
    for (j, &i) in ids.iter().enumerate() {
        let a = wb.charts()[i - 1].anchor;
        let emu = |v: f32| (v as f64 * EMU_PT).round() as i64;
        let _ = write!(
            o,
            r#"<xdr:twoCellAnchor editAs="oneCell"><xdr:from><xdr:col>{}</xdr:col><xdr:colOff>{}</xdr:colOff><xdr:row>{}</xdr:row><xdr:rowOff>{}</xdr:rowOff></xdr:from><xdr:to><xdr:col>{}</xdr:col><xdr:colOff>{}</xdr:colOff><xdr:row>{}</xdr:row><xdr:rowOff>{}</xdr:rowOff></xdr:to>"#,
            a.from.1, emu(a.from.2), a.from.0, emu(a.from.3), a.to.1, emu(a.to.2), a.to.0, emu(a.to.3)
        );
        let _ = write!(
            o,
            r#"<xdr:graphicFrame macro=""><xdr:nvGraphicFramePr><xdr:cNvPr id="{}" name="Chart {}"/><xdr:cNvGraphicFramePr/></xdr:nvGraphicFramePr><xdr:xfrm><a:off x="0" y="0"/><a:ext cx="0" cy="0"/></xdr:xfrm><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/chart"><c:chart xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships" r:id="rId{}"/></a:graphicData></a:graphic></xdr:graphicFrame><xdr:clientData/></xdr:twoCellAnchor>"#,
            j + 2,
            j + 1,
            j + 1
        );
    }
    o.push_str("</xdr:wsDr>");
    o
}
