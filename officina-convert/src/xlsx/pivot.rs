//! Pivot tables in `.xlsx` (ECMA-376 Part 1 §18.10): a pivot cache definition per
//! pivot (the source range and each field's items, no records: `saveData="0"` and
//! `refreshOnLoad="1"`, so Excel rebuilds it from the source on opening) and the
//! pivot table definition in Excel's tabular (`compact="0" outline="0"`) or compact form.
//! The output cells are written as ordinary cells, as Excel saves them.

use std::fmt::Write as _;
use std::sync::Arc;

use officina_grid::addr::{a1, Rect};
use officina_grid::pivot::{Agg, DataField, Pivot, PivotField};
use officina_grid::{CellAddr, ErrorKind, Value, Workbook};
use quick_xml::events::Event;

use super::read::{attr, parse_ref, xml, XlsxError};
use crate::report::{Kind, Report};

const NS: &str = r#"xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn num(x: f64) -> String {
    format!("{x}")
}

fn agg_name(a: Agg) -> &'static str {
    match a {
        Agg::Sum => "sum",
        Agg::Count => "count",
        Agg::Average => "average",
        Agg::Max => "max",
        Agg::Min => "min",
        Agg::Product => "product",
        Agg::CountNums => "countNums",
        Agg::StdDev => "stdDev",
        Agg::StdDevP => "stdDevp",
        Agg::Var => "var",
        Agg::VarP => "varp",
    }
}

fn agg_of(s: &str) -> Option<Agg> {
    Agg::ALL.into_iter().find(|a| agg_name(*a) == s)
}

/// The fields every pivot puts on an axis (rows, columns, pages), in source order.
fn axis_fields(p: &Pivot) -> Vec<&PivotField> {
    p.rows.iter().chain(&p.cols).chain(&p.pages).collect()
}

/// `xl/pivotCache/pivotCacheDefinitionN.xml` for pivot `p`.
pub fn cache_xml(wb: &Workbook, p: &Pivot) -> String {
    let s = &p.source;
    let r = format!("{}:{}", a1(s.r0, s.c0), a1(s.r1, s.c1));
    let mut o = format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><pivotCacheDefinition {NS} refreshOnLoad="1" saveData="0" createdVersion="6" refreshedVersion="6" minRefreshableVersion="3" recordCount="{}"><cacheSource type="worksheet"><worksheetSource ref="{r}" sheet="{}"/></cacheSource>"#, s.r1 - s.r0, esc(&wb.sheet_name(s.sheet)));
    let width = s.c1 - s.c0 + 1;
    let _ = write!(o, r#"<cacheFields count="{width}">"#);
    let on_axis: Vec<u32> = axis_fields(p).iter().map(|f| f.field).collect();
    let mut names: Vec<String> = Vec::new();
    for f in 0..width {
        // Field names are unique in a cache: a repeat gets a number.
        let mut name = wb.pivot_field_name(p, f);
        let base = name.clone();
        let mut k = 2;
        while names.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
            name = format!("{base}{k}");
            k += 1;
        }
        names.push(name.clone());
        let _ = write!(o, r#"<cacheField name="{}" numFmtId="0">"#, esc(&name));
        let items = wb.pivot_items(p, f);
        let (mut str_, mut nums, mut blank, mut other, mut ints) = (false, false, false, false, true);
        let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
        for v in &items {
            match v {
                Value::Num(x) => {
                    nums = true;
                    ints &= x.fract() == 0.0;
                    lo = lo.min(*x);
                    hi = hi.max(*x);
                }
                Value::Str(_) => str_ = true,
                Value::Empty => blank = true,
                _ => other = true,
            }
        }
        let mut a = String::new();
        if !str_ && !other && (nums || blank) {
            a.push_str(r#" containsSemiMixedTypes="0" containsString="0""#);
        }
        if nums && (str_ || other) {
            a.push_str(r#" containsMixedTypes="1""#);
        }
        if nums {
            a.push_str(r#" containsNumber="1""#);
            if ints {
                a.push_str(r#" containsInteger="1""#);
            }
            let _ = write!(a, r#" minValue="{}" maxValue="{}""#, num(lo), num(hi));
        }
        if blank {
            a.push_str(r#" containsBlank="1""#);
        }
        if on_axis.contains(&f) {
            let _ = write!(o, r#"<sharedItems{a} count="{}">"#, items.len());
            for v in &items {
                match v {
                    Value::Num(x) => {
                        let _ = write!(o, r#"<n v="{}"/>"#, num(*x));
                    }
                    Value::Str(t) => {
                        let _ = write!(o, r#"<s v="{}"/>"#, esc(t));
                    }
                    Value::Bool(b) => {
                        let _ = write!(o, r#"<b v="{}"/>"#, *b as u8);
                    }
                    Value::Err(e) => {
                        let _ = write!(o, r#"<e v="{}"/>"#, e.text());
                    }
                    Value::Empty => o.push_str("<m/>"),
                }
            }
            o.push_str("</sharedItems>");
        } else {
            let _ = write!(o, "<sharedItems{a}/>");
        }
        o.push_str("</cacheField>");
    }
    o.push_str("</cacheFields></pivotCacheDefinition>");
    o
}

/// `xl/pivotTables/pivotTableN.xml` for pivot `p` with cache `cache_id`.
pub fn table_xml(wb: &Workbook, p: &Pivot, i: usize, cache_id: usize) -> String {
    let shape = wb.pivot_shape(i).cloned().unwrap_or_default();
    let width = p.source.c1 - p.source.c0 + 1;
    let multi = p.data.len() > 1;
    let col_dims = p.cols.len() + multi as usize;
    let loc = shape.table().unwrap_or(Rect { sheet: p.dest.sheet, r0: p.dest.row, c0: p.dest.col, r1: p.dest.row, c1: p.dest.col });
    let first_data_row = if col_dims == 0 { 1 } else { shape.header_rows };
    let mut o = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><pivotTableDefinition {NS} name="{}" cacheId="{cache_id}" applyNumberFormats="0" applyBorderFormats="0" applyFontFormats="0" applyPatternFormats="0" applyAlignmentFormats="0" applyWidthHeightFormats="1" dataCaption="Values" updatedVersion="6" minRefreshableVersion="3" useAutoFormatting="1" itemPrintTitles="1" createdVersion="6"{} outline="1" outlineData="1" multipleFieldFilters="0"{}{}>"#,
        esc(&p.name),
        if p.compact { "" } else { r#" indent="0" compact="0" compactData="0""# },
        if p.grand_col { "" } else { r#" rowGrandTotals="0""# },
        if p.grand_row { "" } else { r#" colGrandTotals="0""# },
    );
    let _ = write!(o, r#"<location ref="{}:{}" firstHeaderRow="1" firstDataRow="{first_data_row}" firstDataCol="{}""#, a1(loc.r0, loc.c0), a1(loc.r1, loc.c1), shape.label_cols);
    if !p.pages.is_empty() {
        let _ = write!(o, r#" rowPageCount="{}" colPageCount="1""#, p.pages.len());
    }
    o.push_str("/>");
    let _ = write!(o, r#"<pivotFields count="{width}">"#);
    for f in 0..width {
        let axis = if p.rows.iter().any(|x| x.field == f) {
            Some("axisRow")
        } else if p.cols.iter().any(|x| x.field == f) {
            Some("axisCol")
        } else if p.pages.iter().any(|x| x.field == f) {
            Some("axisPage")
        } else {
            None
        };
        o.push_str("<pivotField");
        if let Some(a) = axis {
            let _ = write!(o, r#" axis="{a}""#);
        }
        if p.data.iter().any(|d| d.field == f) {
            o.push_str(r#" dataField="1""#);
        }
        o.push_str(if p.compact { r#" showAll="0""# } else { r#" compact="0" outline="0" showAll="0""# });
        let pf = axis_fields(p).into_iter().find(|x| x.field == f);
        if pf.is_some() && axis == Some("axisPage") && pf.is_some_and(|x| x.hidden.len() > 0) {
            o.push_str(r#" multipleItemSelectionAllowed="1""#);
        }
        if !p.subtotals && axis.is_some() {
            o.push_str(r#" defaultSubtotal="0""#);
        }
        match pf {
            Some(pf) => {
                let items = wb.pivot_items(p, f);
                let n = items.len() + p.subtotals as usize;
                let _ = write!(o, r#"><items count="{n}">"#);
                for (x, v) in items.iter().enumerate() {
                    let hide = pf.hidden.iter().any(|h| officina_grid::pivot::same_item(h, v));
                    let _ = write!(o, r#"<item x="{x}"{}/>"#, if hide { r#" h="1""# } else { "" });
                }
                if p.subtotals {
                    o.push_str(r#"<item t="default"/>"#);
                }
                o.push_str("</items></pivotField>");
            }
            None => o.push_str("/>"),
        }
    }
    o.push_str("</pivotFields>");
    if !p.rows.is_empty() {
        let _ = write!(o, r#"<rowFields count="{}">"#, p.rows.len());
        for f in &p.rows {
            let _ = write!(o, r#"<field x="{}"/>"#, f.field);
        }
        o.push_str("</rowFields>");
    }
    if col_dims > 0 {
        let _ = write!(o, r#"<colFields count="{col_dims}">"#);
        for f in &p.cols {
            let _ = write!(o, r#"<field x="{}"/>"#, f.field);
        }
        if multi {
            o.push_str(r#"<field x="-2"/>"#);
        }
        o.push_str("</colFields>");
    }
    if !p.pages.is_empty() {
        let _ = write!(o, r#"<pageFields count="{}">"#, p.pages.len());
        for f in &p.pages {
            let _ = write!(o, r#"<pageField fld="{}" hier="-1"/>"#, f.field);
        }
        o.push_str("</pageFields>");
    }
    if !p.data.is_empty() {
        let _ = write!(o, r#"<dataFields count="{}">"#, p.data.len());
        for d in &p.data {
            let _ = write!(o, r#"<dataField name="{}" fld="{}""#, esc(&wb.data_caption(p, d)), d.field);
            if d.agg != Agg::Sum {
                let _ = write!(o, r#" subtotal="{}""#, agg_name(d.agg));
            }
            o.push_str(r#" baseField="0" baseItem="0"/>"#);
        }
        o.push_str("</dataFields>");
    }
    o.push_str(r#"<pivotTableStyleInfo name="PivotStyleLight16" showRowHeaders="1" showColHeaders="1" showRowStripes="0" showColStripes="0" showLastColumn="1"/></pivotTableDefinition>"#);
    o
}

// ---------------------------------------------------------------------------
// Reading.

/// A cache definition as read: source range (sheet, rect) and each field's items.
struct Cache {
    sheet: Option<String>,
    rf: Option<String>,
    name: Option<String>,
    items: Vec<Vec<Value>>,
    calculated: bool,
}

fn read_cache(data: &[u8]) -> Result<Cache, XlsxError> {
    let mut c = Cache { sheet: None, rf: None, name: None, items: Vec::new(), calculated: false };
    let mut r = xml(data);
    let mut buf = Vec::new();
    let mut depth_items = false;
    loop {
        buf.clear();
        match r.read_event_into(&mut buf)? {
            Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                b"worksheetSource" => {
                    c.sheet = attr(&e, b"sheet")?;
                    c.rf = attr(&e, b"ref")?;
                    c.name = attr(&e, b"name")?;
                }
                b"cacheField" => {
                    if attr(&e, b"formula")?.is_some() || attr(&e, b"databaseField")?.is_some_and(|v| v == "0") {
                        c.calculated = true;
                    }
                    c.items.push(Vec::new());
                }
                b"sharedItems" => depth_items = true,
                b"fieldGroup" => depth_items = false,
                tag if depth_items => {
                    let v = attr(&e, b"v")?.unwrap_or_default();
                    let val = match tag {
                        b"s" => Value::str(&v),
                        b"n" => v.parse::<f64>().map(Value::Num).unwrap_or(Value::str(&v)),
                        b"b" => Value::Bool(v == "1" || v == "true"),
                        b"e" => Value::Err(ErrorKind::from_text(&v).unwrap_or(ErrorKind::NA)),
                        b"m" => Value::Empty,
                        b"d" => Value::str(&v),
                        _ => continue,
                    };
                    if let Some(l) = c.items.last_mut() {
                        if l.len() < officina_grid::pivot::MAX_ITEMS {
                            l.push(val);
                        }
                    }
                }
                _ => {}
            },
            Event::End(e) if e.local_name().as_ref() == b"sharedItems" => depth_items = false,
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(c)
}

#[derive(Default)]
struct FieldIn {
    hidden: Vec<u32>,
    /// Shown in compact form (`compact`, default on) and outline form (`outline`).
    compact: bool,
    outline: bool,
    subtotal: bool,
}

/// One pivot table part (with its cache) into the workbook.
pub fn read_table(wb: &mut Workbook, sheet: u16, data: &[u8], cache: &[u8], rep: &mut Report) -> Result<(), XlsxError> {
    let c = read_cache(cache)?;
    let mut r = xml(data);
    let mut buf = Vec::new();
    let mut name = String::from("PivotTable");
    let (mut loc, mut pages_n) = (None, 0u32);
    let (mut grand_col, mut grand_row) = (true, true);
    let mut fields: Vec<FieldIn> = Vec::new();
    let mut rows: Vec<i64> = Vec::new();
    let mut cols: Vec<i64> = Vec::new();
    let mut pages: Vec<(u32, Option<u32>)> = Vec::new();
    let mut datas: Vec<(u32, Agg, String)> = Vec::new();
    let mut list = 0u8; // 1 rows, 2 cols
    let mut compact = false;
    let mut shown_as = false;
    loop {
        buf.clear();
        match r.read_event_into(&mut buf)? {
            Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                b"pivotTableDefinition" => {
                    name = attr(&e, b"name")?.unwrap_or(name);
                    grand_col = attr(&e, b"rowGrandTotals")?.is_none_or(|v| v != "0" && v != "false");
                    grand_row = attr(&e, b"colGrandTotals")?.is_none_or(|v| v != "0" && v != "false");
                    compact |= attr(&e, b"compact")?.is_none_or(|v| v != "0" && v != "false");
                }
                b"location" => {
                    loc = attr(&e, b"ref")?.and_then(|v| parse_ref(&v));
                    pages_n = attr(&e, b"rowPageCount")?.and_then(|v| v.parse().ok()).unwrap_or(0);
                }
                b"pivotField" if fields.len() < 16_384 => {
                    let subtotal = attr(&e, b"defaultSubtotal")?.is_none_or(|v| v != "0" && v != "false");
                    let on = |v: Option<String>| v.is_none_or(|v| v != "0" && v != "false");
                    let (compact, outline) = (on(attr(&e, b"compact")?), on(attr(&e, b"outline")?));
                    fields.push(FieldIn { hidden: Vec::new(), subtotal, compact, outline });
                }
                b"item" => {
                    if let (Some(f), Some(x)) = (fields.last_mut(), attr(&e, b"x")?.and_then(|v| v.parse::<u32>().ok())) {
                        if attr(&e, b"h")?.is_some_and(|v| v == "1" || v == "true") {
                            f.hidden.push(x);
                        }
                    }
                }
                b"rowFields" => list = 1,
                b"colFields" => list = 2,
                b"field" => {
                    let x = attr(&e, b"x")?.and_then(|v| v.parse::<i64>().ok()).unwrap_or(-1);
                    match list {
                        1 if rows.len() < 64 => rows.push(x),
                        2 if cols.len() < 64 => cols.push(x),
                        _ => {}
                    }
                }
                b"pageField" if pages.len() < 64 => {
                    if let Some(f) = attr(&e, b"fld")?.and_then(|v| v.parse::<u32>().ok()) {
                        pages.push((f, attr(&e, b"item")?.and_then(|v| v.parse().ok())));
                    }
                }
                b"dataField" if datas.len() < 64 => {
                    if let Some(f) = attr(&e, b"fld")?.and_then(|v| v.parse::<u32>().ok()) {
                        let agg = attr(&e, b"subtotal")?.map_or(Some(Agg::Sum), |v| agg_of(&v));
                        if attr(&e, b"showDataAs")?.is_some_and(|v| v != "normal") {
                            shown_as = true;
                        }
                        match agg {
                            Some(a) => datas.push((f, a, attr(&e, b"name")?.unwrap_or_default())),
                            None => rep.note(Kind::Dropped, "pivot table value fields summed in ways this engine lacks"),
                        }
                    }
                }
                _ => {}
            },
            Event::End(e) if matches!(e.local_name().as_ref(), b"rowFields" | b"colFields") => list = 0,
            Event::Eof => break,
            _ => {}
        }
    }
    let why = |rep: &mut Report, w: &str| rep.note_at(Kind::Dropped, "pivot tables this engine can't use (their cells kept as values)", || format!("{name}: {w}"));
    if c.calculated {
        rep.note(Kind::Dropped, "calculated pivot fields (pivot tables kept as values)");
        return Ok(());
    }
    // The source: a range on a sheet, or a table's name.
    let source = match (&c.sheet, &c.rf, &c.name) {
        (Some(sh), Some(rf), _) => wb.sheet_id(sh).zip(parse_ref(rf)).map(|(s, (r0, c0, r1, c1))| Rect { sheet: s, r0, c0, r1, c1 }),
        (None, Some(rf), _) => parse_ref(rf).map(|(r0, c0, r1, c1)| Rect { sheet, r0, c0, r1, c1 }),
        (_, _, Some(n)) => wb.tables().iter().find(|t| t.name.eq_ignore_ascii_case(n)).map(|t| Rect { r1: t.rect.r1 - t.totals as u32, ..t.rect }),
        _ => None,
    };
    let Some(mut source) = source else {
        why(rep, "its source is not a range in this workbook");
        return Ok(());
    };
    // Whole columns: down to the last row in use.
    source.r1 = source.r1.min(wb.extent(source.sheet).0.saturating_sub(1).max(source.r0));
    let Some((r0, c0, r1, c1)) = loc else {
        why(rep, "no location");
        return Ok(());
    };
    let hidden = |f: u32| -> Vec<Value> {
        let items = c.items.get(f as usize);
        fields.get(f as usize).map(|x| x.hidden.iter().filter_map(|&i| items.and_then(|v| v.get(i as usize)).cloned()).collect()).unwrap_or_default()
    };
    let field = |f: i64| (f >= 0).then(|| PivotField { field: f as u32, hidden: hidden(f as u32) });
    let page_top = r0.saturating_sub(if pages_n > 0 { pages_n + 1 } else { 0 });
    let mut p = Pivot::new(&name, source, CellAddr::new(sheet, page_top, c0));
    p.rows = rows.iter().filter_map(|&f| field(f)).collect();
    p.cols = cols.iter().filter_map(|&f| field(f)).collect();
    if rows.contains(&-2) {
        rep.note(Kind::Approximate, "pivot table values shown down the rows (shown across)");
    }
    for (f, item) in pages {
        let mut pf = PivotField { field: f, hidden: hidden(f) };
        if let Some(i) = item {
            // One item picked: the rest hidden.
            if let Some(all) = c.items.get(f as usize) {
                pf.hidden = all.iter().enumerate().filter(|(k, _)| *k as u32 != i).map(|(_, v)| v.clone()).collect();
            }
        }
        p.pages.push(pf);
    }
    p.data = datas
        .into_iter()
        .map(|(f, agg, n)| {
            let d = DataField { field: f, agg, name: None };
            let default = wb.data_caption(&p, &d);
            DataField { name: (!n.is_empty() && n != default).then(|| Arc::from(n.as_str())), ..d }
        })
        .collect();
    let axis: Vec<u32> = p.rows.iter().chain(&p.cols).map(|f| f.field).collect();
    p.subtotals = axis.iter().any(|&f| fields.get(f as usize).is_some_and(|x| x.subtotal));
    p.grand_col = grand_col;
    p.grand_row = grand_row;
    // Compact form when the table and its row fields say so; outline form (a
    // column per field without compact) is shown tabular.
    let row_fields: Vec<&FieldIn> = p.rows.iter().filter_map(|f| fields.get(f.field as usize)).collect();
    p.compact = compact && row_fields.iter().all(|f| f.compact);
    if !p.compact && row_fields.len() > 1 && row_fields.iter().any(|f| f.outline) {
        rep.note(Kind::Approximate, "pivot tables in outline form (shown in tabular form once refreshed)");
    }
    if shown_as {
        rep.note(Kind::Approximate, "pivot table values shown as percentages or differences (refreshing shows the plain values)");
    }
    let area = Rect { sheet, r0: page_top, c0, r1, c1: c1.max(c0 + if pages_n > 0 { 1 } else { 0 }) };
    if let Err(w) = wb.add_pivot_as_is(p, Some(area)) {
        why(rep, w);
    }
    Ok(())
}

/// Every pivot table, from each sheet's relationships, in their parts' order.
pub fn read_all(wb: &mut Workbook, z: &crate::zip::Archive, sheets: &[String], rep: &mut Report) -> Result<(), XlsxError> {
    let mut parts: Vec<(u16, String)> = Vec::new();
    for (s, part) in sheets.iter().enumerate() {
        for (t, p) in super::read::rels(z, part)?.into_values() {
            if t.ends_with("/pivotTable") {
                parts.push((s as u16, p));
            }
        }
    }
    // pivotTable2 before pivotTable10.
    let number = |p: &str| p.trim_end_matches(".xml").chars().rev().take_while(|c| c.is_ascii_digit()).collect::<String>().chars().rev().collect::<String>().parse::<u64>().unwrap_or(0);
    parts.sort_by(|a, b| (number(&a.1), &a.1).cmp(&(number(&b.1), &b.1)));
    for (sheet, p) in parts {
        let Some(data) = z.read_name(&p)? else { continue };
        let cache = super::read::rels(z, &p)?.into_values().find(|(t, _)| t.ends_with("/pivotCacheDefinition")).map(|(_, c)| c);
        let Some(cache) = cache.and_then(|c| z.read_name(&c).ok().flatten()) else {
            rep.note(Kind::Dropped, "pivot tables without their cache (kept as values)");
            continue;
        };
        read_table(wb, sheet, &data, &cache, rep)?;
    }
    Ok(())
}
