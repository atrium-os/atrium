//! Pivot tables in `.xlsx` (ECMA-376 Part 1 §18.10): a pivot cache definition per
//! pivot (the source range and each field's items, no records: `saveData="0"` and
//! `refreshOnLoad="1"`, so Excel rebuilds it from the source on opening) and the
//! pivot table definition in Excel's tabular (`compact="0" outline="0"`) or compact form.
//! The output cells are written as ordinary cells, as Excel saves them.

use std::fmt::Write as _;
use std::sync::Arc;

use officina_grid::addr::{a1, Rect};
use officina_grid::pivot::{same_item, Agg, AxisSort, BaseItem, DataField, Pivot, PivotField, ReferenceCalc, ShowAs};
use officina_grid::{CellAddr, Value, Workbook};
#[cfg(feature = "read")]
use quick_xml::events::Event;

#[cfg(feature = "read")]
use super::read::{attr, parse_ref, xml, XlsxError};
use crate::report::{Kind, Report};

mod scope;
#[cfg(feature = "read")]
mod cache;
#[cfg(feature = "read")]
use cache::Cache;

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

#[cfg(feature = "read")]
fn agg_of(s: &str) -> Option<Agg> {
    Agg::ALL.into_iter().find(|a| agg_name(*a) == s)
}

/// The fields every pivot puts on an axis (rows, columns, pages), in source order.
fn axis_fields(p: &Pivot) -> Vec<&PivotField> {
    p.rows.iter().chain(&p.cols).chain(&p.pages).collect()
}

fn field_items(wb: &Workbook, p: &Pivot, field: u32) -> Vec<Value> {
    let mut items = axis_fields(p).into_iter().find(|f| f.field == field)
        .map_or_else(|| wb.pivot_items(p, field), |f| wb.pivot_export_items(p, f));
    // Keep named bases addressable if the source edit removes their last row.
    for data in &p.data {
        if let ShowAs::Reference { field: base, item: BaseItem::Named(value), .. } = &data.show_as {
            if *base == field && !items.iter().any(|item| same_item(item, value)) {
                items.push(value.clone());
            }
        }
    }
    for axis in axis_fields(p) {
        if let AxisSort::Scoped { members, .. } = &axis.sort {
            for (member_field, value) in members {
                if *member_field == field && !items.iter().any(|item| same_item(item, value)) {
                    items.push(value.clone());
                }
            }
        }
    }
    items
}

fn show_as_xml(out: &mut String, wb: &Workbook, p: &Pivot, mode: &ShowAs) {
    let (name, field, item) = match mode {
        ShowAs::Normal => ("normal", 0, 0),
        ShowAs::PercentOfRow => ("percentOfRow", 0, 0),
        ShowAs::PercentOfColumn => ("percentOfCol", 0, 0),
        ShowAs::PercentOfTotal => ("percentOfTotal", 0, 0),
        ShowAs::Index => ("index", 0, 0),
        ShowAs::RunningTotal(field) => ("runTotal", *field, 0),
        ShowAs::PercentOfParentRow => ("percentOfParentRow", 0, 0),
        ShowAs::PercentOfParentColumn => ("percentOfParentCol", 0, 0),
        ShowAs::PercentOfParent(field) => ("percentOfParent", *field, 0),
        ShowAs::PercentOfRunningTotal(field) => ("percentOfRunningTotal", *field, 0),
        ShowAs::Rank { field, descending } => (if *descending { "rankDescending" } else { "rankAscending" }, *field, 0),
        ShowAs::Reference { calc, field, item } => {
            let name = match calc {
                ReferenceCalc::Difference => "difference",
                ReferenceCalc::Percent => "percent",
                ReferenceCalc::PercentDifference => "percentDiff",
            };
            let index = match item {
                BaseItem::Previous => 1_048_828,
                BaseItem::Next => 1_048_829,
                BaseItem::Named(value) => field_items(wb, p, *field).iter()
                    .position(|item| same_item(item, value)).unwrap_or(0),
            };
            (name, *field, index)
        }
    };
    let extended = mode.is_excel_extension();
    if !extended && !matches!(mode, ShowAs::Normal) {
        let _ = write!(out, r#" showDataAs="{name}""#);
    }
    if mode.is_percent() {
        out.push_str(r#" numFmtId="10""#);
    }
    let _ = write!(out, r#" baseField="{field}" baseItem="{item}""#);
    if extended {
        let _ = write!(out, r#"><extLst><ext uri="{{E15A36E0-9728-4E99-A89B-3F7291B0FE68}}"><x14:dataField xmlns:x14="http://schemas.microsoft.com/office/spreadsheetml/2009/9/main" pivotShowAs="{name}"/></ext></extLst></dataField>"#);
    } else {
        out.push_str("/>");
    }
}

/// `xl/pivotCache/pivotCacheDefinitionN.xml` for pivot `p`.
pub fn cache_xml(wb: &Workbook, p: &Pivot) -> String {
    let s = &p.source;
    let r = format!("{}:{}", a1(s.r0, s.c0), a1(s.r1, s.c1));
    let mut o = format!(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><pivotCacheDefinition {NS} refreshOnLoad="1" saveData="0" createdVersion="6" refreshedVersion="6" minRefreshableVersion="3" recordCount="{}"><cacheSource type="worksheet"><worksheetSource ref="{r}" sheet="{}"/></cacheSource>"#, s.r1 - s.r0, esc(&wb.sheet_name(s.sheet)));
    let width = s.c1 - s.c0 + 1;
    let _ = write!(o, r#"<cacheFields count="{width}">"#);
    let on_axis: Vec<u32> = axis_fields(p).iter().map(|f| f.field)
        .chain(p.data.iter().filter_map(|d| d.show_as.base_field())).collect();
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
        let items = field_items(wb, p, f);
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
        let value_sort = pf.and_then(|field| scope::Scope::new(wb, p, &field.sort));
        if let Some(field) = pf {
            let sort = match (&field.sort, &value_sort) {
                (_, Some(scope)) if scope.descending => "descending",
                (AxisSort::Manual(_) | AxisSort::Source, _) => "manual",
                (AxisSort::Name { descending: true }, _) => "descending",
                _ => "ascending",
            };
            let _ = write!(o, r#" sortType="{sort}""#);
        }
        if !p.subtotals && axis.is_some() {
            o.push_str(r#" defaultSubtotal="0""#);
        }
        match pf {
            Some(pf) => {
                let items = field_items(wb, p, f);
                let n = items.len() + p.subtotals as usize;
                let _ = write!(o, r#"><items count="{n}">"#);
                for (x, v) in items.iter().enumerate() {
                    let hide = pf.hidden.iter().any(|h| officina_grid::pivot::same_item(h, v));
                    let _ = write!(o, r#"<item x="{x}"{}/>"#, if hide { r#" h="1""# } else { "" });
                }
                if p.subtotals {
                    o.push_str(r#"<item t="default"/>"#);
                }
                o.push_str("</items>");
                if let Some(scope) = &value_sort {
                    let level = p.rows.iter().position(|field| field.field == f)
                        .or_else(|| p.cols.iter().position(|field| field.field == f))
                        .unwrap_or(0);
                    scope.write(&mut o, wb, p, level);
                }
                o.push_str("</pivotField>");
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
            show_as_xml(&mut o, wb, p, &d.show_as);
        }
        o.push_str("</dataFields>");
    }
    o.push_str(r#"<pivotTableStyleInfo name="PivotStyleLight16" showRowHeaders="1" showColHeaders="1" showRowStripes="0" showColStripes="0" showLastColumn="1"/></pivotTableDefinition>"#);
    o
}

// ---------------------------------------------------------------------------
// Reading.

#[cfg(feature = "read")]
#[derive(Default)]
struct FieldIn {
    hidden: Vec<u32>,
    /// Shared-item index addressed by each position in this pivot field's items.
    items: Vec<Option<u32>>,
    sort: Option<String>,
    scoped_sort: bool,
    scope: Option<scope::ScopeIn>,
    /// Shown in compact form (`compact`, default on) and outline form (`outline`).
    compact: bool,
    outline: bool,
    subtotal: bool,
}

#[cfg(feature = "read")]
/// One pivot table part (with its cache) into the workbook.
fn read_table(
    wb: &mut Workbook, sheet: u16, data: &[u8], rep: &mut Report,
    resolve: impl FnOnce(&Workbook, Option<u32>) -> Result<(Cache, Rect, bool), &'static str>,
) -> Result<(), XlsxError> {
    let mut r = xml(data);
    let mut buf = Vec::new();
    let mut name = String::from("PivotTable");
    let mut cache_id = None;
    let (mut loc, mut pages_n) = (None, 0u32);
    let (mut grand_col, mut grand_row) = (true, true);
    let mut fields: Vec<FieldIn> = Vec::new();
    let mut rows: Vec<i64> = Vec::new();
    let mut cols: Vec<i64> = Vec::new();
    let mut pages: Vec<(u32, Option<u32>)> = Vec::new();
    let mut datas: Vec<(u32, Agg, String, Option<String>, Option<u32>, Option<u32>)> = Vec::new();
    let mut data_indices = Vec::new();
    let mut list = 0u8; // 1 rows, 2 cols
    let mut compact = false;
    let mut active_data = None;
    loop {
        buf.clear();
        let event = r.read_event_into(&mut buf)?;
        let start = matches!(event, Event::Start(_));
        match event {
            Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                b"pivotTableDefinition" => {
                    name = attr(&e, b"name")?.unwrap_or(name);
                    cache_id = attr(&e, b"cacheId")?.and_then(|id| id.parse::<u32>().ok());
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
                    let sort = attr(&e, b"sortType")?;
                    fields.push(FieldIn { subtotal, compact, outline, sort, ..Default::default() });
                }
                b"item" => {
                    if let Some(f) = fields.last_mut().filter(|f| f.items.len() < officina_grid::pivot::MAX_ITEMS) {
                        let x = attr(&e, b"x")?.and_then(|v| v.parse::<u32>().ok());
                        f.items.push(x);
                        if attr(&e, b"h")?.is_some_and(|v| v == "1" || v == "true") {
                            f.hidden.extend(x);
                        }
                    }
                }
                b"autoSortScope" => {
                    let scope = if start {
                        let span = r.read_to_end(e.name())?;
                        scope::ScopeIn::read(data.get(span.start as usize..span.end as usize).unwrap_or_default())?
                    } else {
                        None
                    };
                    if let Some(field) = fields.last_mut() {
                        field.scoped_sort = true;
                        field.scope = scope;
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
                b"dataField" if data_indices.len() < 64 => {
                    if let Some(f) = attr(&e, b"fld")?.and_then(|v| v.parse::<u32>().ok()) {
                        let agg = attr(&e, b"subtotal")?.map_or(Some(Agg::Sum), |v| agg_of(&v));
                        let mode = attr(&e, b"showDataAs")?;
                        let field = attr(&e, b"baseField")?.and_then(|v| v.parse().ok());
                        let item = attr(&e, b"baseItem")?.and_then(|v| v.parse().ok());
                        data_indices.push(agg.map(|_| datas.len() as u32));
                        match agg {
                            Some(a) => {
                                active_data = start.then(|| (datas.len(), e.name().as_ref().to_vec()));
                                datas.push((f, a, attr(&e, b"name")?.unwrap_or_default(), mode, field, item));
                            }
                            None => rep.note(Kind::Dropped, "pivot table value fields summed in ways this engine lacks"),
                        }
                    } else if let (Some((index, _)), Some(mode)) = (&active_data, attr(&e, b"pivotShowAs")?) {
                        datas[*index].3 = Some(mode);
                    }
                }
                _ => {}
            },
            Event::End(e) if matches!(e.local_name().as_ref(), b"rowFields" | b"colFields") => list = 0,
            Event::End(e) if active_data.as_ref().is_some_and(|(_, name)| e.name().as_ref() == name) => active_data = None,
            Event::Eof => break,
            _ => {}
        }
    }
    let why = |rep: &mut Report, w: &str| rep.note_at(Kind::Dropped, "pivot tables this engine can't use (their cells kept as values)", || format!("{name}: {w}"));
    let Some((r0, c0, r1, c1)) = loc else {
        why(rep, "no location");
        return Ok(());
    };
    let (c, source, relinked) = match resolve(wb, cache_id) {
        Ok(resolved) => resolved,
        Err(reason) => {
            why(rep, reason);
            return Ok(());
        }
    };
    let axes = rows.iter().chain(&cols).filter_map(|&f| u32::try_from(f).ok())
        .chain(pages.iter().map(|(f, _)| *f));
    if axes.clone().any(|f| {
        fields.get(f as usize).is_some_and(|info| {
            info.hidden.iter().any(|&index| c.items(f).and_then(|items| items.get(index as usize)).is_none())
        })
    }) {
        why(rep, "hidden pivot items could not be identified without their cache entries");
        return Ok(());
    }
    let missing_items: Vec<_> = axes.filter(|&f| fields.get(f as usize).is_some_and(|info| {
        info.items.iter().flatten().any(|&index| c.items(f).and_then(|items| items.get(index as usize)).is_none())
    })).collect();
    let rebuild = c.needs_rebuild(source) || !missing_items.is_empty();
    let missing_order: Vec<_> = missing_items.into_iter().filter(|&f| {
        matches!(fields[f as usize].sort.as_deref(), None | Some("manual"))
    }).collect();
    let hidden = |f: u32| -> Vec<Value> {
        let items = c.items(f);
        fields.get(f as usize).map(|x| x.hidden.iter().filter_map(|&i| items.and_then(|v| v.get(i as usize)).cloned()).collect()).unwrap_or_default()
    };
    let field = |f: u32, rep: &mut Report| {
        let sort = fields.get(f as usize).map_or(AxisSort::Default, |field| {
            match field.sort.as_deref() {
                Some("descending") => AxisSort::Name { descending: true },
                None | Some("manual") if !field.items.is_empty() => {
                    let mut items = Vec::new();
                    for index in field.items.iter().flatten() {
                        if let Some(item) = c.items(f).and_then(|items| items.get(*index as usize)) {
                            items.push(item.clone());
                        } else {
                            rep.note(Kind::Approximate, "pivot manual item order with missing cache entries");
                        }
                    }
                    if items.is_empty() { AxisSort::Default } else { AxisSort::Manual(items) }
                }
                _ => AxisSort::Default,
            }
        });
        PivotField { field: f, hidden: hidden(f), sort }
    };
    let page_top = r0.saturating_sub(if pages_n > 0 { pages_n + 1 } else { 0 });
    let mut p = Pivot::new(&name, source, CellAddr::new(sheet, page_top, c0));
    p.rows = rows.iter().filter_map(|&f| u32::try_from(f).ok()).map(|f| field(f, rep)).collect();
    p.cols = cols.iter().filter_map(|&f| u32::try_from(f).ok()).map(|f| field(f, rep)).collect();
    if rows.contains(&-2) {
        rep.note(Kind::Approximate, "pivot table values shown down the rows (shown across)");
    }
    for (f, item) in pages {
        let mut pf = field(f, rep);
        if let Some(i) = item {
            // A page selection addresses the pivot items, not the shared cache.
            let index = fields.get(f as usize).and_then(|field| field.items.get(i as usize))
                .copied().flatten();
            let selection = index.and_then(|index| c.items(f).filter(|all| (index as usize) < all.len()).map(|all| (all, index)));
            let Some((all, index)) = selection else {
                why(rep, "its selected page item could not be identified without its cache entry");
                return Ok(());
            };
            pf.hidden = all.iter().enumerate().filter(|(k, _)| *k as u32 != index).map(|(_, v)| v.clone()).collect();
        }
        p.pages.push(pf);
    }
    let active = |field: u32| p.rows.iter().chain(&p.cols).any(|x| x.field == field);
    let mut data = Vec::new();
    for (f, agg, n, mode, base_field, base_item) in datas {
        let base_field = base_field.filter(|&field| active(field));
        let item = |field: u32| match base_item.unwrap_or(0) {
            1_048_828 => Some(BaseItem::Previous),
            1_048_829 => Some(BaseItem::Next),
            index => {
                let indices = &fields.get(field as usize)?.items;
                let shared = if indices.is_empty() { index } else { (*indices.get(index as usize)?)? };
                c.items(field)?.get(shared as usize).cloned().map(BaseItem::Named)
            }
        };
        let show_as = match mode.as_deref().unwrap_or("normal") {
            "normal" => Some(ShowAs::Normal),
            "percentOfRow" => Some(ShowAs::PercentOfRow),
            "percentOfCol" => Some(ShowAs::PercentOfColumn),
            "percentOfTotal" => Some(ShowAs::PercentOfTotal),
            "index" => Some(ShowAs::Index),
            "runTotal" => base_field.map(ShowAs::RunningTotal),
            "percentOfParentRow" => Some(ShowAs::PercentOfParentRow),
            "percentOfParentCol" => Some(ShowAs::PercentOfParentColumn),
            "percentOfParent" => base_field.map(ShowAs::PercentOfParent),
            "percentOfRunningTotal" => base_field.map(ShowAs::PercentOfRunningTotal),
            name @ ("rankAscending" | "rankDescending") => base_field.map(|field| ShowAs::Rank {
                field, descending: name == "rankDescending",
            }),
            name @ ("difference" | "percent" | "percentDiff") => base_field.and_then(|field| {
                let calc = match name {
                    "difference" => ReferenceCalc::Difference,
                    "percent" => ReferenceCalc::Percent,
                    _ => ReferenceCalc::PercentDifference,
                };
                Some(ShowAs::Reference { calc, field, item: item(field)? })
            }),
            _ => None,
        };
        let Some(show_as) = show_as else {
            why(rep, "its display calculation or base item could not be resolved");
            return Ok(());
        };
        if show_as.base_field().is_some_and(|field| missing_order.contains(&field))
            && matches!(&show_as, ShowAs::RunningTotal(_) | ShowAs::PercentOfRunningTotal(_)
                | ShowAs::Reference { item: BaseItem::Previous | BaseItem::Next, .. })
        {
            why(rep, "a display calculation depends on a manual item order that could not be recovered");
            return Ok(());
        }
        let d = DataField { field: f, agg, name: None, show_as };
        let default = wb.data_caption(&p, &d);
        data.push(DataField { name: (!n.is_empty() && n != default).then(|| Arc::from(n.as_str())), ..d });
    }
    p.data = data;
    let missing_scope_item = std::cell::Cell::new(false);
    let mut apply_scopes = |axis: &mut [PivotField], opposite: &[PivotField]| {
        for field in axis {
            let Some(input) = fields.get(field.field as usize).filter(|input| input.scoped_sort) else { continue };
            let member = |field: u32, index: u32, cache_index: bool| {
                let shared = if cache_index { Some(index) } else {
                    fields.get(field as usize).and_then(|info| info.items.get(index as usize)).copied().flatten()
                };
                let value = shared.and_then(|index| c.items(field)?.get(index as usize)).cloned();
                if value.is_none() {
                    missing_scope_item.set(true);
                }
                value
            };
            let sort = input.scope.as_ref().and_then(|scope| {
                scope.resolve(input.sort.as_deref(), opposite, &data_indices, member)
            });
            if let Some(sort) = sort {
                field.sort = sort;
            } else {
                rep.note(Kind::Approximate, "unresolved pivot value sort scope (refresh uses label order)");
            }
        }
    };
    apply_scopes(&mut p.rows, &p.cols);
    apply_scopes(&mut p.cols, &p.rows);
    if missing_scope_item.get() {
        why(rep, "a selected sort member could not be identified without its cache entry");
        return Ok(());
    }
    if p.pages.iter().any(|field| fields.get(field.field as usize).is_some_and(|input| input.scoped_sort)) {
        rep.note(Kind::Approximate, "pivot value sort scope on a page field");
    }
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
    let area = Rect { sheet, r0: page_top, c0, r1, c1: c1.max(c0 + if pages_n > 0 { 1 } else { 0 }) };
    if area.intersects(&source) {
        why(rep, "its source overlaps the saved pivot output");
        return Ok(());
    }
    if let Err(w) = wb.add_pivot_as_is(p, Some(area)) {
        why(rep, w);
    } else if rebuild {
        rep.note_at(Kind::Approximate, "pivot cache recovered from its local source (saved cells kept until Refresh)", || name.clone());
    } else if relinked {
        rep.note_at(Kind::Approximate, "pivot cache recovered through the workbook cache ID", || name.clone());
    }
    Ok(())
}

#[cfg(feature = "read")]
/// Every pivot table, from each sheet's relationships, in their parts' order.
pub fn read_all(
    wb: &mut Workbook, z: &crate::zip::Archive, sheets: &[String], workbook: &str,
    cache_ids: &std::collections::HashMap<u32, Option<String>>, rep: &mut Report,
) -> Result<(), XlsxError> {
    use super::read::internal_rels;
    let mut parts: Vec<(u16, String)> = Vec::new();
    for (s, part) in sheets.iter().enumerate() {
        for (t, p) in internal_rels(z, part)?.into_values() {
            if t.ends_with("/pivotTable") {
                parts.push((s as u16, p));
            }
        }
    }
    if parts.is_empty() {
        return Ok(());
    }
    let workbook_links = internal_rels(z, workbook)?;
    // pivotTable2 before pivotTable10.
    let number = |p: &str| p.trim_end_matches(".xml").chars().rev().take_while(|c| c.is_ascii_digit()).collect::<String>().chars().rev().collect::<String>().parse::<u64>().unwrap_or(0);
    parts.sort_by(|a, b| (number(&a.1), &a.1).cmp(&(number(&b.1), &b.1)));
    parts.dedup();
    for (sheet, p) in parts {
        let Some(data) = z.read_name(&p)? else { continue };
        let links = internal_rels(z, &p).unwrap_or_default();
        let mut targets = links.values().filter(|(kind, _)| kind.ends_with("/pivotCacheDefinition")).map(|(_, path)| path.as_str());
        let direct = targets.next().filter(|path| targets.all(|target| target == *path));
        read_table(wb, sheet, &data, rep, |wb, id| {
            let owner = id.and_then(|id| cache_ids.get(&id)?.as_deref())
                .and_then(|rel| workbook_links.get(rel))
                .filter(|(kind, _)| kind.ends_with("/pivotCacheDefinition"))
                .map(|(_, path)| path.as_str());
            let load = |path: &str| {
                let bytes = z.read_name(path).map_err(|_| "its pivot cache part is unreadable")?
                    .ok_or("its pivot cache part is missing")?;
                let cache = Cache::read(&bytes, wb.date_system());
                let source = cache.resolve(wb, z, path, sheets, sheet)?;
                Ok((cache, source, Some(path) != direct))
            };
            // The workbook cache ID is authoritative. A separate table link
            // can supply intact items if it names the same local source.
            match (owner, direct) {
                (Some(owner), Some(direct)) if owner != direct => match load(owner) {
                    Ok(primary) if primary.0.needs_rebuild(primary.1) => {
                        if let Ok(other) = load(direct) {
                            if other.1 == primary.1 && !other.0.needs_rebuild(other.1) {
                                return Ok(other);
                            }
                        }
                        Ok(primary)
                    }
                    Ok(primary) => Ok(primary),
                    Err(reason @ ("its pivot cache part is unreadable" | "its pivot cache part is missing" | "the cache has no local worksheet source")) => {
                        load(direct).or(Err(reason))
                    }
                    Err(reason) => Err(reason),
                },
                (Some(path), _) | (_, Some(path)) => load(path),
                _ => Err("its pivot cache is missing and no local source can be identified"),
            }
        })?;
    }
    Ok(())
}
