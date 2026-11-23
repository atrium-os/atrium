//! Pivot tables in `.ods`: `table:data-pilot-tables` (OpenDocument 1.3 §9.6), as
//! LibreOffice writes them (tabular layout, each field's members with what is
//! shown, value fields' captions as `tableooo:display-name`). The output cells
//! are ordinary cells.

use std::fmt::Write as _;
use std::sync::Arc;

use officina_grid::addr::{col_name, Rect, MAX_COLS};
use officina_grid::pivot::{
    same_item, Agg, AxisSort, BaseItem, DataField, DatePart, GroupField, GroupKind, ItemGroup,
    Pivot, PivotField, ReferenceCalc, ShowAs, Subtotal, MAX_GROUPS,
};
use officina_grid::{CellAddr, Value, Workbook};

use super::formula::sheet_a1;
use crate::report::{Kind, Report};

fn esc(s: &str) -> String {
    let mut o = String::new();
    super::write::esc(&mut o, s);
    o
}

fn func(a: Agg) -> &'static str {
    match a {
        Agg::Sum => "sum",
        Agg::Count => "count",
        Agg::Average => "average",
        Agg::Max => "max",
        Agg::Min => "min",
        Agg::Product => "product",
        Agg::CountNums => "countnums",
        Agg::StdDev => "stdev",
        Agg::StdDevP => "stdevp",
        Agg::Var => "var",
        Agg::VarP => "varp",
        Agg::Auto => "auto",
    }
}

fn func_of(s: &str) -> Option<Agg> {
    Agg::ALL.into_iter().find(|a| func(*a).eq_ignore_ascii_case(s))
}

fn range(wb: &Workbook, r: &Rect) -> String {
    let sh = sheet_a1(&wb.sheet_name(r.sheet));
    format!("{sh}.{}{}:{sh}.{}{}", col_name(r.c0), r.r0 + 1, col_name(r.c1), r.r1 + 1)
}

/// A member's name: how its item shows.
fn member(v: &Value) -> String {
    match v {
        Value::Empty => String::new(),
        Value::Str(s) => s.to_string(),
        v => v.display(),
    }
}

const DATE_PARTS: [&str; 7] = ["seconds", "minutes", "hours", "days", "months", "quarters", "years"];

fn reference_xml(out: &mut String, wb: &Workbook, p: &Pivot, mode: &ShowAs) {
    let kind = match mode {
        ShowAs::Normal => return,
        ShowAs::PercentOfRow => "row-percentage",
        ShowAs::PercentOfColumn => "column-percentage",
        ShowAs::PercentOfTotal => "total-percentage",
        ShowAs::Index => "index",
        ShowAs::RunningTotal(_) => "running-total",
        ShowAs::Reference { calc, .. } => match calc {
            ReferenceCalc::Difference => "member-difference",
            ReferenceCalc::Percent => "member-percentage",
            ReferenceCalc::PercentDifference => "member-percentage-difference",
        },
        _ => return, // Excel extensions have no ODS representation.
    };
    let _ = write!(out, r#"<table:data-pilot-field-reference table:type="{kind}""#);
    if let Some(field) = mode.base_field() {
        let _ = write!(out, r#" table:field-name="{}""#, esc(&wb.pivot_field_name(p, field)));
    }
    if let ShowAs::Reference { item, .. } = mode {
        match item {
            BaseItem::Named(value) => {
                let _ = write!(out, r#" table:member-type="named" table:member-name="{}""#, esc(&member(value)));
            }
            BaseItem::Previous => out.push_str(r#" table:member-type="previous""#),
            BaseItem::Next => out.push_str(r#" table:member-type="next""#),
        }
    }
    out.push_str("/>");
}

fn group_xml(out: &mut String, wb: &Workbook, p: &Pivot, field: u32) {
    let Some(group) = p.groups.iter().find(|group| group.field == field) else { return };
    out.push_str("<table:data-pilot-groups");
    if field != group.source {
        let _ = write!(out, r#" table:source-field-name="{}""#, esc(&wb.pivot_field_name(p, group.source)));
    }
    let mut bounds = |start: Option<f64>, end: Option<f64>, date: bool| {
        for (name, value) in [("start", start), ("end", end)] {
            let value = value.and_then(|value| {
                if date {
                    let serial = match wb.date_system() {
                        officina_grid::eval::dates::DateSystem::Excel1900 => value,
                        officina_grid::eval::dates::DateSystem::Excel1904 => value + 1462.0,
                    };
                    super::write::date_text(serial)
                } else {
                    Some(value.to_string())
                }
            }).unwrap_or_else(|| "auto".into());
            let prefix = if date { "date-" } else { "" };
            let _ = write!(out, r#" table:{prefix}{name}="{value}""#);
        }
    };
    match &group.kind {
        GroupKind::Named(groups) => {
            out.push('>');
            for group in groups {
                let _ = write!(out, r#"<table:data-pilot-group table:name="{}">"#, esc(&group.name));
                for value in &group.members {
                    let _ = write!(out, r#"<table:data-pilot-group-member table:name="{}"/>"#, esc(value));
                }
                out.push_str("</table:data-pilot-group>");
            }
            out.push_str("</table:data-pilot-groups>");
        }
        GroupKind::Intervals { start, end, step, dates } => {
            bounds(*start, *end, *dates);
            let _ = write!(out, r#" table:step="{step}"/>"#);
        }
        GroupKind::Discrete(_) | GroupKind::NumericBins { .. } => unreachable!(),
        GroupKind::DateParts { start, end, part } => {
            bounds(*start, *end, true);
            let _ = write!(out, r#" table:grouped-by="{}" table:step="1"/>"#, DATE_PARTS[*part as usize]);
        }
    }
}

fn sort_xml(out: &mut String, wb: &Workbook, pivot: &Pivot, sort: &AxisSort) {
    let (mode, descending, caption) = match sort {
        AxisSort::Default => return,
        AxisSort::Manual(_) => ("manual", false, None),
        AxisSort::Source => ("none", false, None),
        AxisSort::Name { descending } => ("name", *descending, None),
        AxisSort::Data { caption, descending } => ("data", *descending, Some(caption.to_string())),
        AxisSort::Scoped { data, descending, members } if members.is_empty() => {
            let Some(field) = pivot.data.get(*data as usize) else { return };
            ("data", *descending, Some(wb.data_caption(pivot, field)))
        }
        AxisSort::Scoped { .. } | AxisSort::Selection { .. } => return,
    };
    let order = if descending { "descending" } else { "ascending" };
    let _ = write!(out, r#"<table:data-pilot-sort-info table:sort-mode="{mode}" table:order="{order}""#);
    if let Some(caption) = caption {
        let _ = write!(out, r#" table:data-field="{}""#, esc(&caption));
    }
    out.push_str("/>");
}

pub fn supports(pivot: &Pivot) -> bool {
    pivot.calculations.is_empty()
        && pivot.data.iter().all(|d| !d.show_as.is_excel_extension())
        && pivot.groups.iter().all(|group| !matches!(group.kind, GroupKind::Discrete(_) | GroupKind::NumericBins { .. }))
}

/// `table:data-pilot-tables`, or nothing.
pub fn tables_xml(wb: &Workbook) -> String {
    let mut pivots = wb.pivots().iter().enumerate()
        .filter(|(_, p)| supports(p))
        .peekable();
    if pivots.peek().is_none() {
        return String::new();
    }
    let mut o = String::from("<table:data-pilot-tables>");
    for (i, p) in pivots {
        let area = wb.pivot_shape(i).and_then(|s| s.area).unwrap_or(Rect { sheet: p.dest.sheet, r0: p.dest.row, c0: p.dest.col, r1: p.dest.row, c1: p.dest.col });
        let grand = match (p.grand_row, p.grand_col) {
            (true, true) => "",
            (true, false) => r#" table:grand-total="column""#,
            (false, true) => r#" table:grand-total="row""#,
            (false, false) => r#" table:grand-total="none""#,
        };
        let _ = write!(o, r#"<table:data-pilot-table table:name="{}" table:target-range-address="{}"{grand} table:show-filter-button="false"><table:source-cell-range table:cell-range-address="{}"/>"#, esc(&p.name), esc(&range(wb, &area)), esc(&range(wb, &p.source)));
        let field = |o: &mut String, f: &PivotField, orient: &str, sub: bool| {
            let _ = write!(o, r#"<table:data-pilot-field table:source-field-name="{}" table:orientation="{orient}" table:used-hierarchy="-1" table:function="auto""#, esc(&wb.pivot_field_name(p, f.field)));
            // A page with one item shown: that item is its selected page.
            let items = wb.pivot_export_items(p, f);
            if orient == "page" && !f.hidden.is_empty() {
                let existing = wb.pivot_items(p, f.field);
                let shown: Vec<_> = existing.iter().filter(|v| !f.hidden.iter().any(|h| same_item(h, v))).collect();
                if let [one] = shown[..] {
                    let _ = write!(o, r#" table:selected-page="{}""#, esc(&member(one)));
                } else {
                    o.push_str(r#" loext:ignore-selected-page="true""#);
                }
            }
            o.push_str(r#"><table:data-pilot-level table:show-empty="false">"#);
            if sub {
                o.push_str(r#"<table:data-pilot-subtotals><table:data-pilot-subtotal table:function="auto"/></table:data-pilot-subtotals>"#);
            }
            o.push_str("<table:data-pilot-members>");
            for v in &items {
                let show = !f.hidden.iter().any(|h| same_item(h, v));
                let _ = write!(o, r#"<table:data-pilot-member table:name="{}" table:display="{show}" table:show-details="true"/>"#, esc(&member(v)));
            }
            // LibreOffice keeps the compact form as its own extension.
            let compact = if orient == "row" && p.compact { r#" loext:layout-mode="compact-layout""# } else { "" };
            let layout = if orient == "row" && (f.outline || (!p.compact && f.subtotal == Subtotal::Top)) {
                if p.subtotal_at(f, true) == Subtotal::Bottom { "outline-subtotals-bottom" } else { "outline-subtotals-top" }
            } else { "tabular-layout" };
            o.push_str("</table:data-pilot-members>");
            sort_xml(o, wb, p, &f.sort);
            let _ = write!(o, r#"<table:data-pilot-layout-info table:add-empty-lines="false" table:layout-mode="{layout}"{compact}/></table:data-pilot-level>"#);
            group_xml(o, wb, p, f.field);
            o.push_str("</table:data-pilot-field>");
        };
        for f in &p.rows {
            field(&mut o, f, "row", p.subtotal_at(f, true) != Subtotal::Off);
        }
        for f in &p.cols {
            field(&mut o, f, "column", p.subtotal_at(f, false) != Subtotal::Off);
        }
        if p.data.len() > 1 {
            let orient = if p.values_on_rows { "row" } else { "column" };
            let _ = write!(o, r#"<table:data-pilot-field table:source-field-name="" table:is-data-layout-field="true" table:orientation="{orient}" table:used-hierarchy="-1" table:function="auto"><table:data-pilot-level/></table:data-pilot-field>"#);
        }
        for f in &p.pages {
            field(&mut o, f, "page", false);
        }
        for group in &p.groups {
            if !p.rows.iter().chain(&p.cols).chain(&p.pages).any(|f| f.field == group.field) {
                field(&mut o, &PivotField::new(group.field), "hidden", false);
            }
        }
        for d in &p.data {
            let _ = write!(o, r#"<table:data-pilot-field table:source-field-name="{}" tableooo:display-name="{}" table:orientation="data" table:used-hierarchy="-1" table:function="{}">"#, esc(&wb.pivot_field_name(p, d.field)), esc(&wb.data_caption(p, d)), func(d.agg));
            reference_xml(&mut o, wb, p, &d.show_as);
            o.push_str("</table:data-pilot-field>");
        }
        o.push_str("</table:data-pilot-table>");
    }
    o.push_str("</table:data-pilot-tables>");
    o
}

// ---------------------------------------------------------------------------
// Reading.

#[derive(Default, Debug)]
pub struct FieldIn {
    pub name: String,
    pub orient: String,
    pub function: String,
    pub display: Option<String>,
    pub data_layout: bool,
    pub selected: Option<String>,
    /// LibreOffice's: the members shown say what the page shows, not `selected`.
    pub ignore_selected: bool,
    pub subtotals: bool,
    pub custom_subtotal: bool,
    pub outline_top: Option<bool>,
    /// LibreOffice's compact form (`loext:layout-mode="compact-layout"`).
    pub compact: bool,
    pub hidden: Vec<String>,
    pub members: Vec<String>,
    pub sort: Option<SortIn>,
    pub reference: Option<ReferenceIn>,
    pub group: Option<GroupIn>,
}

#[derive(Default, Debug)]
pub struct SortIn {
    pub mode: String,
    pub descending: bool,
    pub caption: String,
}

#[derive(Default, Debug)]
pub struct ReferenceIn {
    pub kind: String,
    pub field: String,
    pub member_type: String,
    pub member: String,
}

#[derive(Default, Debug)]
pub struct GroupIn {
    pub source: String,
    pub dates: bool,
    pub part: Option<String>,
    pub start: Option<String>,
    pub end: Option<String>,
    pub step: Option<String>,
    pub members: Vec<ItemGroup>,
    pub size: usize,
    pub clipped: bool,
}

#[cfg(feature = "read")]
impl GroupIn {
    fn kind(&self) -> Option<GroupKind> {
        if self.clipped {
            return None;
        }
        if self.part.is_none() && self.step.is_none() && !self.dates {
            return Some(GroupKind::Named(self.members.clone()));
        }
        // LibreOffice writes a zero step for named groups.
        if !self.members.is_empty() {
            return Some(GroupKind::Named(self.members.clone()));
        }
        let bound = |value: Option<&str>| match value.unwrap_or("auto") {
            "auto" => Some(None),
            value => {
                let n = if self.dates { super::read::date_value(value)? } else { value.parse::<f64>().ok()? };
                n.is_finite().then_some(Some(n))
            }
        };
        let (start, end) = (bound(self.start.as_deref())?, bound(self.end.as_deref())?);
        if let Some(part) = &self.part {
            let part = DatePart::ALL[DATE_PARTS.iter().position(|name| name == part)?];
            Some(GroupKind::DateParts { start, end, part })
        } else {
            let step: f64 = self.step.as_ref()?.parse().ok()?;
            (step.is_finite() && step > 0.0).then_some(GroupKind::Intervals { start, end, step, dates: self.dates })
        }
    }
}


#[derive(Default, Debug)]
pub struct TableIn {
    pub name: String,
    pub target: String,
    pub source: String,
    pub grand: String,
    pub fields: Vec<FieldIn>,
    /// A source that is a database range or another document: not modelled.
    pub other_source: bool,
}

#[cfg(feature = "read")]
/// The data pilots read into pivots over the cells they wrote.
pub fn make(wb: &mut Workbook, tables: Vec<TableIn>, rep: &mut Report) {
    for t in tables {
        let why = |rep: &mut Report, w: &str| rep.note_at(Kind::Dropped, "pivot tables this engine can't use (their cells kept as values)", || format!("{}: {w}", t.name));
        if t.other_source {
            why(rep, "its source is not a cell range");
            continue;
        }
        let (Some(src), Some(area)) = (super::read::target_rect(wb, &t.source), t.target.split_whitespace().next().and_then(|a| super::read::target_rect(wb, a))) else {
            why(rep, "a range outside the workbook");
            continue;
        };
        let source = Rect { sheet: src.0, r0: src.1, c0: src.2, r1: src.3, c1: src.4 };
        let area = Rect { sheet: area.0, r0: area.1, c0: area.2, r1: area.3, c1: area.4 };
        let mut p = Pivot::new(&t.name, source, CellAddr::new(area.sheet, area.r0, area.c0));
        let width = source.c1 - source.c0 + 1;
        let probe = p.clone();
        let source_index = |name: &str| (0..width).find(|&f| wb.pivot_field_name(&probe, f) == name);
        let mut bad = None;
        let grouped: Vec<_> = t.fields.iter().filter(|field| field.group.is_some()).collect();
        if grouped.len() > MAX_GROUPS {
            why(rep, "too many grouped fields");
            continue;
        }
        let index = |name: &str| source_index(name).or_else(|| {
            let mut matches = grouped.iter().enumerate().filter(|(_, field)| field.name == name);
            let (i, _) = matches.next()?;
            matches.next().is_none().then_some(MAX_COLS + i as u32)
        });
        for field in grouped.iter().copied() {
            let group = field.group.as_ref().unwrap();
            let source_name = if group.source.is_empty() { &field.name } else { &group.source };
            let (Some(field_id), Some(source), Some(kind)) = (index(&field.name), index(source_name), group.kind()) else {
                bad = Some(format!("unsupported grouping for {}", field.name));
                break;
            };
            p.groups.push(GroupField { field: field_id, source, name: Arc::from(field.name.as_str()), kind });
        }
        if let Some(w) = bad {
            why(rep, &w);
            continue;
        }
        if let Err(reason) = p.check_groups() {
            why(rep, reason);
            continue;
        }
        let mut subtotals = false;
        let mut values_position = None;
        let mut compact = None;
        for f in &t.fields {
            if f.data_layout {
                values_position = match f.orient.as_str() {
                    "row" => Some((true, p.rows.len())),
                    "column" => Some((false, p.cols.len())),
                    _ => None,
                };
                continue;
            }
            if f.orient == "hidden" {
                continue;
            }
            let Some(x) = index(&f.name) else {
                bad = Some(format!("no field {}", f.name));
                break;
            };
            if f.orient == "data" {
                let Some(agg) = func_of(&f.function) else {
                    bad = Some(format!("function {}", f.function));
                    break;
                };
                let mut show_as = ShowAs::Normal;
                if let Some(reference) = &f.reference {
                    let field = || index(&reference.field).filter(|&index| t.fields.iter().any(|f| {
                        f.name == reference.field && matches!(f.orient.as_str(), "row" | "column")
                            && !f.data_layout && wb.pivot_field_name(&p, index) == f.name
                    }));
                    let mode = match reference.kind.as_str() {
                        "" | "none" => Some(ShowAs::Normal),
                        "row-percentage" => Some(ShowAs::PercentOfRow),
                        "column-percentage" => Some(ShowAs::PercentOfColumn),
                        "total-percentage" => Some(ShowAs::PercentOfTotal),
                        "index" => Some(ShowAs::Index),
                        "running-total" => field().map(ShowAs::RunningTotal),
                        name @ ("member-difference" | "member-percentage" | "member-percentage-difference") => field().and_then(|field| {
                            let calc = match name {
                                "member-difference" => ReferenceCalc::Difference,
                                "member-percentage" => ReferenceCalc::Percent,
                                _ => ReferenceCalc::PercentDifference,
                            };
                            let item = match reference.member_type.as_str() {
                                "previous" => BaseItem::Previous,
                                "next" => BaseItem::Next,
                                "" | "named" => BaseItem::Named(wb.pivot_items(&p, field).into_iter()
                                    .find(|value| member(value).eq_ignore_ascii_case(&reference.member))
                                    .unwrap_or_else(|| Value::str(&reference.member))),
                                _ => return None,
                            };
                            Some(ShowAs::Reference { calc, field, item })
                        }),
                        _ => None,
                    };
                    let Some(mode) = mode else {
                        bad = Some(format!("unsupported field reference for {}", f.name));
                        break;
                    };
                    show_as = mode;
                }
                let d = DataField { field: x, agg, name: None, show_as };
                let default = wb.data_caption(&p, &d);
                let name = f.display.clone().filter(|n| *n != default).map(|n| Arc::from(n.as_str()));
                p.data.push(DataField { name, ..d });
                continue;
            }
            let items = wb.pivot_items(&p, x);
            let mut members = std::collections::HashMap::new();
            for value in &items {
                members.entry(member(value).to_lowercase()).or_insert(value);
            }
            let find = |name: &str| members.get(&name.to_lowercase()).map(|value| (*value).clone())
                .unwrap_or_else(|| if name.is_empty() { Value::Empty } else { Value::str(name) });
            let mut hidden: Vec<Value> = f.hidden.iter().map(|n| find(n)).collect();
            if let Some(sel) = f.selected.as_ref().filter(|_| !f.ignore_selected) {
                if f.orient == "page" {
                    let keep = find(sel);
                    hidden = items.iter().filter(|v| !same_item(v, &keep)).cloned().collect();
                }
            }
            let sort = match f.sort.as_ref() {
                None => AxisSort::Default,
                Some(sort) => match sort.mode.as_str() {
                    "none" => AxisSort::Source,
                    "manual" => AxisSort::Manual(f.members.iter().map(|n| find(n)).collect()),
                    "name" => AxisSort::Name { descending: sort.descending },
                    "data" if !sort.caption.is_empty() && sort.caption.len() <= officina_grid::strings::MAX_TEXT => AxisSort::Data {
                        caption: Arc::from(sort.caption.as_str()), descending: sort.descending,
                    },
                    _ => {
                        rep.note(Kind::Approximate, "unrecognized pivot item sort (refresh uses label order)");
                        AxisSort::Default
                    }
                },
            };
            let mut pf = PivotField { field: x, hidden, sort, ..PivotField::new(x) };
            if matches!(f.orient.as_str(), "row" | "column") {
                if f.custom_subtotal {
                    bad = Some(format!("custom subtotal functions in {}", f.name));
                    break;
                }
                pf.outline = f.orient == "row" && f.outline_top.is_some() && !f.compact;
                pf.subtotal = if !f.subtotals { Subtotal::Off }
                    else if pf.outline && f.outline_top == Some(false) { Subtotal::Bottom }
                    else { Subtotal::Default };
            }
            match f.orient.as_str() {
                "row" => {
                    subtotals |= f.subtotals;
                    compact = Some(compact.unwrap_or(true) && f.compact);
                    p.rows.push(pf);
                }
                "column" => {
                    subtotals |= f.subtotals;
                    p.cols.push(pf);
                }
                "page" => p.pages.push(pf),
                _ => {}
            }
        }
        if let Some(w) = bad {
            why(rep, &w);
            continue;
        }
        for field in p.rows.iter().chain(&p.cols) {
            if let AxisSort::Data { caption, .. } = &field.sort {
                if wb.pivot_sort_value(&p, caption).is_none() {
                    rep.note_at(Kind::Approximate, "pivot sort refers to an unavailable value field (refresh uses label order)",
                        || format!("{}: {caption}", t.name));
                }
            }
        }
        if let Some((row, position)) = values_position.filter(|_| p.data.len() > 1) {
            if position != if row { p.rows.len() } else { p.cols.len() } {
                why(rep, "the Values field is between other fields on its axis");
                continue;
            }
            p.values_on_rows = row;
        }
        if !subtotals {
            for field in p.rows.iter_mut().chain(&mut p.cols) { field.subtotal = Subtotal::Default; }
        }
        p.subtotals = subtotals;
        p.compact = compact.unwrap_or(false);
        (p.grand_row, p.grand_col) = match t.grand.as_str() {
            "none" => (false, false),
            "row" => (false, true),
            "column" => (true, false),
            _ => (true, true),
        };
        if let Err(w) = wb.add_pivot_as_is(p, Some(area)) {
            why(rep, w);
        }
    }
}
