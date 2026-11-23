//! Pivot tables in `.xlsx` (ECMA-376 Part 1 §18.10): a pivot cache definition per
//! pivot (the source range and each field's items, no records: `saveData="0"` and
//! `refreshOnLoad="1"`, so Excel rebuilds it from the source on opening) and the
//! pivot table definition in Excel's tabular (`compact="0" outline="0"`) or compact form.
//! The output cells are written as ordinary cells, as Excel saves them.

use std::fmt::Write as _;
use std::sync::Arc;

use officina_grid::addr::{a1, Rect};
use officina_grid::pivot::{GroupKind, ScopeItems, Subtotal, item_key, same_item, Agg, AxisSort, BaseItem, DataField, Pivot, PivotField, ReferenceCalc, ShowAs};
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
        Agg::Auto => unreachable!("Excel needs an explicit aggregate"),
    }
}

#[cfg(feature = "read")]
fn agg_of(s: &str) -> Option<Agg> {
    Agg::ALL.into_iter().filter(|a| *a != Agg::Auto).find(|a| agg_name(*a) == s)
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
    let mut include = |value: &Value| {
        if !items.iter().any(|item| same_item(item, value)) { items.push(value.clone()); }
    };
    for axis in axis_fields(p) {
        match &axis.sort {
            AxisSort::Scoped { members, .. } => {
                for (member_field, value) in members {
                    if *member_field == field { include(value); }
                }
            }
            AxisSort::Selection { fields, .. } => {
                for (member_field, selector) in fields {
                    if *member_field != field { continue; }
                    if let ScopeItems::Members(values) = selector {
                        for value in values { include(value); }
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(GroupKind::NumericBins { items: domain, .. }) = p.groups.iter().find(|group| group.field == field).map(|group| &group.kind) {
        let domain: std::collections::HashSet<_> = domain.iter().map(item_key).collect();
        items.retain(|value| domain.contains(&item_key(value)));
    }
    items
}

fn model_field(width: u32, index: u32) -> u32 {
    if index < width { index } else { officina_grid::addr::MAX_COLS.saturating_add(index - width) }
}

struct Fields {
    width: u32,
    derived: Vec<u32>,
}

impl Fields {
    fn new(p: &Pivot) -> Self {
        let width = p.source.c1 - p.source.c0 + 1;
        let mut derived: Vec<_> = p.groups.iter().map(|group| group.field).filter(|&f| f >= width)
            .chain(p.calculations.iter().map(|field| field.field)).collect();
        derived.sort_unstable();
        Self { width, derived }
    }

    fn count(&self) -> usize { self.width as usize + self.derived.len() }

    fn ids(&self) -> impl Iterator<Item = u32> + '_ {
        (0..self.width).chain(self.derived.iter().copied())
    }

    fn raw(&self, field: u32) -> u32 {
        if field < self.width { field }
        else { self.width + self.derived.binary_search(&field).expect("validated virtual field") as u32 }
    }
}

pub fn supports(p: &Pivot) -> bool {
    p.groups.iter().all(|group| match &group.kind {
        GroupKind::Discrete(_) => {
            p.groups.iter().filter(|other| matches!(other.kind, GroupKind::Discrete(_)) && other.source == group.source).count() == 1
                && !p.groups.iter().any(|other| other.field == group.source && !matches!(other.kind, GroupKind::Discrete(_)))
        }
        GroupKind::NumericBins { .. } => true,
        _ => false,
    })
}

/// In a grouped cache, some fields have two lists. The pivot references the group list.
fn cache_field_items(wb: &Workbook, p: &Pivot, field: u32) -> Vec<Value> {
    let group = p.groups.iter().find(|g| g.field == field);
    if let Some(GroupKind::NumericBins { items, .. }) = group.map(|g| &g.kind) {
        return items.clone();
    }
    let mut items = field_items(wb, p, field);
    if p.groups.is_empty() { return items; }
    let mut seen: std::collections::HashSet<_> = items.iter().map(item_key).collect();
    if let Some(group) = group.filter(|g| matches!(g.kind, GroupKind::Discrete(_))) {
        let GroupKind::Discrete(mapping) = &group.kind else { unreachable!() };
        let translated: std::collections::HashSet<_> = mapping.iter().map(|(from, _)| item_key(from)).collect();
        for item in cache_field_items(wb, p, group.source) {
            if !translated.contains(&item_key(&item)) && seen.insert(item_key(&item)) {
                items.push(item);
            }
        }
    }
    let maps = p.groups.iter().filter_map(|g| {
        let GroupKind::Discrete(items) = &g.kind else { return None };
        if g.field == field { Some((items, false)) }
        else if g.source == field { Some((items, true)) }
        else { None }
    });
    for (pairs, source) in maps {
        for (from, to) in pairs {
            let item = if source { from } else { to };
            if seen.insert(item_key(item)) {
                items.push(item.clone());
            }
        }
    }
    items
}

fn write_cache_item(o: &mut String, v: &Value) {
    match v {
        Value::Num(x) => { let _ = write!(o, r#"<n v="{}"/>"#, num(*x)); }
        Value::Str(t) => { let _ = write!(o, r#"<s v="{}"/>"#, esc(t)); }
        Value::Bool(b) => { let _ = write!(o, r#"<b v="{}"/>"#, *b as u8); }
        Value::Err(e) => { let _ = write!(o, r#"<e v="{}"/>"#, e.text()); }
        Value::Empty => o.push_str("<m/>"),
    }
}

fn write_group(o: &mut String, wb: &Workbook, p: &Pivot, ids: &Fields, field: u32) {
    let parent = p.groups.iter().find(|group| group.source == field && matches!(group.kind, GroupKind::Discrete(_)));
    let group = p.groups.iter().find(|group| group.field == field);
    if parent.is_none() && group.is_none() { return; }
    o.push_str("<fieldGroup");
    if let Some(parent) = parent {
        let _ = write!(o, r#" par="{}""#, ids.raw(parent.field));
    }
    let Some(group) = group else { o.push_str("/>"); return };
    let mut base = group.source;
    while base >= ids.width {
        base = p.groups.iter().find(|group| group.field == base).expect("validated group dependency").source;
    }
    let _ = write!(o, r#" base="{base}">"#);
    let items = cache_field_items(wb, p, field);
    match &group.kind {
        GroupKind::NumericBins { start, end, step, .. } => {
            let _ = write!(o, r#"<rangePr autoStart="0" autoEnd="0" startNum="{start}" endNum="{end}" groupInterval="{step}"/>"#);
        }
        GroupKind::Discrete(mapping) => {
            let source = cache_field_items(wb, p, group.source);
            let indices: std::collections::HashMap<_, _> = items.iter().enumerate().map(|(i, value)| (item_key(value), i)).collect();
            let targets: std::collections::HashMap<_, _> = mapping.iter().map(|(from, to)| (item_key(from), to)).collect();
            let _ = write!(o, r#"<discretePr count="{}">"#, source.len());
            for value in &source {
                let target = targets.get(&item_key(value)).copied().unwrap_or(value);
                let index = indices[&item_key(target)];
                let _ = write!(o, r#"<x v="{index}"/>"#);
            }
            o.push_str("</discretePr>");
        }
        _ => unreachable!(),
    }
    let _ = write!(o, r#"<groupItems count="{}">"#, items.len());
    for item in &items { write_cache_item(o, item); }
    o.push_str("</groupItems></fieldGroup>");
}

fn show_as_xml(out: &mut String, wb: &Workbook, p: &Pivot, mode: &ShowAs, ids: &Fields) {
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
    let field = ids.raw(field);
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
    let ids = Fields::new(p);
    let _ = write!(o, r#"<cacheFields count="{}">"#, ids.count());
    let on_axis: Vec<u32> = axis_fields(p).iter().map(|f| f.field)
        .chain(p.data.iter().filter_map(|d| d.show_as.base_field()))
        .chain(p.groups.iter().map(|group| group.source)).collect();
    let mut names: Vec<String> = Vec::new();
    for f in ids.ids() {
        let mut name = wb.pivot_field_name(p, f);
        let base = name.clone();
        let mut k = 2;
        while names.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
            name = format!("{base}{k}");
            k += 1;
        }
        names.push(name.clone());
        let _ = write!(o, r#"<cacheField name="{}" numFmtId="0""#, esc(&name));
        if let Some(calc) = p.calculations.iter().find(|calc| calc.field == f) {
            let _ = write!(o, r#" databaseField="0" formula="{}"/>"#, esc(&calc.formula));
            continue;
        }
        if f >= ids.width {
            o.push_str(r#" databaseField="0">"#);
            write_group(&mut o, wb, p, &ids, f);
            o.push_str("</cacheField>");
            continue;
        }
        o.push('>');
        let grouped = p.groups.iter().any(|group| group.field == f);
        let items = if grouped { wb.pivot_source_items(p, f) } else { cache_field_items(wb, p, f) };
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
            for v in &items { write_cache_item(&mut o, v); }
            o.push_str("</sharedItems>");
        } else {
            let _ = write!(o, "<sharedItems{a}/>");
        }
        write_group(&mut o, wb, p, &ids, f);
        o.push_str("</cacheField>");
    }
    o.push_str("</cacheFields></pivotCacheDefinition>");
    o
}

/// `xl/pivotTables/pivotTableN.xml` for pivot `p` with cache `cache_id`.
pub fn table_xml(wb: &Workbook, p: &Pivot, i: usize, cache_id: usize) -> String {
    let shape = wb.pivot_shape(i).cloned().unwrap_or_default();
    let ids = Fields::new(p);
    let multi = p.data.len() > 1;
    let values_on_rows = multi && p.values_on_rows;
    let col_dims = p.cols.len() + usize::from(multi && !values_on_rows);
    let row_dims = p.rows.len() + usize::from(values_on_rows);
    let outline = u8::from(p.compact || p.rows.iter().any(|field| field.outline));
    let loc = shape.table().unwrap_or(Rect { sheet: p.dest.sheet, r0: p.dest.row, c0: p.dest.col, r1: p.dest.row, c1: p.dest.col });
    let values_axis = if values_on_rows {
        format!(r#" dataOnRows="1" dataPosition="{}""#, p.rows.len())
    } else {
        String::new()
    };
    let first_data_row = if col_dims == 0 { 1 } else { shape.header_rows };
    let mut o = format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><pivotTableDefinition {NS} name="{}" cacheId="{cache_id}" applyNumberFormats="0" applyBorderFormats="0" applyFontFormats="0" applyPatternFormats="0" applyAlignmentFormats="0" applyWidthHeightFormats="1" dataCaption="Values" updatedVersion="6" minRefreshableVersion="3" useAutoFormatting="1" itemPrintTitles="1" createdVersion="6"{} outline="{outline}" outlineData="{outline}" multipleFieldFilters="0"{}{}{}>"#,
        esc(&p.name),
        if p.compact { "" } else { r#" indent="0" compact="0" compactData="0""# },
        if p.grand_col { "" } else { r#" rowGrandTotals="0""# },
        if p.grand_row { "" } else { r#" colGrandTotals="0""# },
        values_axis,
    );
    let _ = write!(o, r#"<location ref="{}:{}" firstHeaderRow="1" firstDataRow="{first_data_row}" firstDataCol="{}""#, a1(loc.r0, loc.c0), a1(loc.r1, loc.c1), shape.label_cols);
    if !p.pages.is_empty() {
        let _ = write!(o, r#" rowPageCount="{}" colPageCount="1""#, p.pages.len());
    }
    o.push_str("/>");
    let _ = write!(o, r#"<pivotFields count="{}">"#, ids.count());
    for f in ids.ids() {
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
        let pf = axis_fields(p).into_iter().find(|x| x.field == f);
        if !p.compact {
            let outline = u8::from(pf.is_some_and(|field| field.outline));
            let _ = write!(o, r#" compact="0" outline="{outline}""#);
        }
        o.push_str(r#" showAll="0""#);
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
        let subtotal = pf.map_or(Subtotal::Off, |field| p.subtotal_at(field, axis == Some("axisRow")));
        if subtotal == Subtotal::Bottom && axis == Some("axisRow") && (p.compact || pf.is_some_and(|field| field.outline)) {
            o.push_str(r#" subtotalTop="0""#);
        } else if subtotal == Subtotal::Top && !p.compact {
            o.push_str(r#" subtotalTop="1""#);
        }
        if subtotal == Subtotal::Off && axis.is_some() {
            o.push_str(r#" defaultSubtotal="0""#);
        }
        match pf {
            Some(pf) => {
                let items = field_items(wb, p, f);
                let cache_indices = p.groups.iter().find(|group| group.field == f).and_then(|group| match &group.kind {
                    GroupKind::NumericBins { items, .. } => Some(items.iter()
                        .enumerate().map(|(i, value)| (item_key(value), i)).collect::<std::collections::HashMap<_, _>>()),
                    _ => None,
                });
                let n = items.len() + usize::from(subtotal != Subtotal::Off);
                let _ = write!(o, r#"><items count="{n}">"#);
                for (i, v) in items.iter().enumerate() {
                    let x = cache_indices.as_ref().map_or(i, |indices| indices[&item_key(v)]);
                    let hide = pf.hidden.iter().any(|h| officina_grid::pivot::same_item(h, v));
                    let _ = write!(o, r#"<item x="{x}"{}/>"#, if hide { r#" h="1""# } else { "" });
                }
                if subtotal != Subtotal::Off {
                    o.push_str(r#"<item t="default"/>"#);
                }
                o.push_str("</items>");
                if let Some(scope) = &value_sort {
                    let level = p.rows.iter().position(|field| field.field == f)
                        .or_else(|| p.cols.iter().position(|field| field.field == f))
                        .unwrap_or(0);
                    scope.write(&mut o, wb, p, level, &ids);
                }
                o.push_str("</pivotField>");
            }
            None => o.push_str("/>"),
        }
    }
    o.push_str("</pivotFields>");
    if row_dims > 0 {
        let _ = write!(o, r#"<rowFields count="{row_dims}">"#);
        for f in &p.rows {
            let _ = write!(o, r#"<field x="{}"/>"#, ids.raw(f.field));
        }
        if values_on_rows { o.push_str(r#"<field x="-2"/>"#); }
        o.push_str("</rowFields>");
    }
    if col_dims > 0 {
        let _ = write!(o, r#"<colFields count="{col_dims}">"#);
        for f in &p.cols {
            let _ = write!(o, r#"<field x="{}"/>"#, ids.raw(f.field));
        }
        if multi && !values_on_rows {
            o.push_str(r#"<field x="-2"/>"#);
        }
        o.push_str("</colFields>");
    }
    if !p.pages.is_empty() {
        let _ = write!(o, r#"<pageFields count="{}">"#, p.pages.len());
        for f in &p.pages {
            let _ = write!(o, r#"<pageField fld="{}" hier="-1"/>"#, ids.raw(f.field));
        }
        o.push_str("</pageFields>");
    }
    if !p.data.is_empty() {
        let _ = write!(o, r#"<dataFields count="{}">"#, p.data.len());
        for d in &p.data {
            let _ = write!(o, r#"<dataField name="{}" fld="{}""#, esc(&wb.data_caption(p, d)), ids.raw(d.field));
            let agg = if d.agg == Agg::Auto { wb.pivot_auto_agg(p, d.field) } else { d.agg };
            if agg != Agg::Sum {
                let _ = write!(o, r#" subtotal="{}""#, agg_name(agg));
            }
            show_as_xml(&mut o, wb, p, &d.show_as, &ids);
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
    outline: Option<bool>,
    subtotal: bool,
    subtotal_top: Option<bool>,
    custom_subtotal: bool,
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
    let mut outline = false;
    let mut data_on_rows = false;
    let mut data_position = None;
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
                    outline = attr(&e, b"outline")?.is_some_and(|v| matches!(v.as_str(), "1" | "true"));
                    data_on_rows = attr(&e, b"dataOnRows")?.is_some_and(|v| matches!(v.as_str(), "1" | "true"));
                    data_position = attr(&e, b"dataPosition")?.and_then(|v| v.parse::<usize>().ok());
                }
                b"location" => {
                    loc = attr(&e, b"ref")?.and_then(|v| parse_ref(&v));
                    pages_n = attr(&e, b"rowPageCount")?.and_then(|v| v.parse().ok()).unwrap_or(0);
                }
                b"pivotField" if fields.len() < 16_384 => {
                    let subtotal = attr(&e, b"defaultSubtotal")?.is_none_or(|v| v != "0" && v != "false");
                    let on = |v: Option<String>| v.is_none_or(|v| v != "0" && v != "false");
                    let compact = on(attr(&e, b"compact")?);
                    let outline = attr(&e, b"outline")?.map(|v| on(Some(v)));
                    let subtotal_top = attr(&e, b"subtotalTop")?.map(|v| on(Some(v)));
                    let mut custom_subtotal = false;
                    for attr_name in [b"sumSubtotal".as_slice(), b"countASubtotal", b"avgSubtotal",
                        b"maxSubtotal", b"minSubtotal", b"productSubtotal", b"countSubtotal",
                        b"stdDevSubtotal", b"stdDevPSubtotal", b"varSubtotal", b"varPSubtotal"] {
                        custom_subtotal |= attr(&e, attr_name)?.is_some_and(|v| on(Some(v)));
                    }
                    let sort = attr(&e, b"sortType")?;
                    fields.push(FieldIn { subtotal, compact, outline, subtotal_top, custom_subtotal, sort, ..Default::default() });
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
        PivotField { field: f, hidden: hidden(f), sort, ..PivotField::new(f) }
    };
    let page_top = r0.saturating_sub(if pages_n > 0 { pages_n + 1 } else { 0 });
    let mut p = Pivot::new(&name, source, CellAddr::new(sheet, page_top, c0));
    if let Err(reason) = c.definitions(&mut p) {
        why(rep, reason);
        return Ok(());
    }
    let width = source.c1 - source.c0 + 1;
    let model = |raw| model_field(width, raw);
    p.rows = rows.iter().filter_map(|&f| u32::try_from(f).ok()).map(|f| field(f, rep)).collect();
    p.cols = cols.iter().filter_map(|&f| u32::try_from(f).ok()).map(|f| field(f, rep)).collect();
    p.values_on_rows = datas.len() > 1 && (rows.contains(&-2) || data_on_rows);
    if datas.len() > 1 && (if p.values_on_rows {
        cols.contains(&-2) || rows.iter().position(|&f| f == -2).is_some_and(|i| i != p.rows.len())
            || data_position.is_some_and(|pos| pos != p.rows.len())
    } else {
        cols.iter().position(|&f| f == -2).is_some_and(|i| i + 1 != cols.len())
            || data_position.is_some_and(|pos| pos.saturating_add(1) != cols.len() && pos != cols.len())
    }) {
        why(rep, "the Values field is between other fields on its axis");
        return Ok(());
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
        let default = wb.data_caption(&p, &DataField { field: model(f), ..d.clone() });
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
    let axis = p.rows.iter().chain(&p.cols);
    if axis.clone().any(|f| fields.get(f.field as usize).is_some_and(|f| f.custom_subtotal)) {
        why(rep, "it uses custom pivot subtotal functions");
        return Ok(());
    }
    p.subtotals = axis.clone().any(|f| fields.get(f.field as usize).is_some_and(|f| f.subtotal));
    p.grand_col = grand_col;
    p.grand_row = grand_row;
    p.compact = compact && p.rows.iter().all(|f| fields.get(f.field as usize).is_none_or(|f| f.compact));
    for (members, row) in [(&mut p.rows, true), (&mut p.cols, false)] {
        for field in members {
            let Some(input) = fields.get(field.field as usize) else { continue };
            field.outline = row && !p.compact && input.outline.unwrap_or(outline);
            let default_top = row && (p.compact || field.outline);
            field.subtotal = match (p.subtotals, input.subtotal, row && input.subtotal_top.unwrap_or(default_top)) {
                (true, false, _) => Subtotal::Off,
                (true, true, top) if top != default_top => if top { Subtotal::Top } else { Subtotal::Bottom },
                _ => Subtotal::Default,
            };
        }
    }
    let area = Rect { sheet, r0: page_top, c0, r1, c1: c1.max(c0 + if pages_n > 0 { 1 } else { 0 }) };
    if area.intersects(&source) {
        why(rep, "its source overlaps the saved pivot output");
        return Ok(());
    }
    for field in p.rows.iter_mut().chain(&mut p.cols).chain(&mut p.pages) {
        field.field = model(field.field);
        match &mut field.sort {
            AxisSort::Scoped { members, .. } => {
                for (field, _) in members { *field = model(*field); }
            }
            AxisSort::Selection { fields, .. } => {
                for (field, _) in fields { *field = model(*field); }
            }
            _ => {}
        }
    }
    for data in &mut p.data {
        data.field = model(data.field);
        data.show_as.remap_field(model);
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
