//! Pivot tables in `.ods`: `table:data-pilot-tables` (OpenDocument 1.3 §9.6), as
//! LibreOffice writes them (tabular layout, each field's members with what is
//! shown, value fields' captions as `tableooo:display-name`). The output cells
//! are ordinary cells.

use std::fmt::Write as _;
use std::sync::Arc;

use officina_grid::addr::{col_name, Rect};
use officina_grid::pivot::{same_item, Agg, DataField, Pivot, PivotField};
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

/// `table:data-pilot-tables`, or nothing.
pub fn tables_xml(wb: &Workbook) -> String {
    if wb.pivots().is_empty() {
        return String::new();
    }
    let mut o = String::from("<table:data-pilot-tables>");
    for (i, p) in wb.pivots().iter().enumerate() {
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
            let items = wb.pivot_items(p, f.field);
            let shown: Vec<&Value> = items.iter().filter(|v| !f.hidden.iter().any(|h| same_item(h, v))).collect();
            if orient == "page" && !f.hidden.is_empty() {
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
            let compact = if p.compact { r#" loext:layout-mode="compact-layout""# } else { "" };
            let _ = write!(o, r#"</table:data-pilot-members><table:data-pilot-layout-info table:add-empty-lines="false" table:layout-mode="tabular-layout"{compact}/></table:data-pilot-level></table:data-pilot-field>"#);
        };
        for f in &p.rows {
            field(&mut o, f, "row", p.subtotals);
        }
        for f in &p.cols {
            field(&mut o, f, "column", p.subtotals);
        }
        if p.data.len() > 1 {
            o.push_str(r#"<table:data-pilot-field table:source-field-name="" table:is-data-layout-field="true" table:orientation="column" table:used-hierarchy="-1" table:function="auto"><table:data-pilot-level/></table:data-pilot-field>"#);
        }
        for f in &p.pages {
            field(&mut o, f, "page", false);
        }
        for d in &p.data {
            let _ = write!(o, r#"<table:data-pilot-field table:source-field-name="{}" tableooo:display-name="{}" table:orientation="data" table:used-hierarchy="-1" table:function="{}"/>"#, esc(&wb.pivot_field_name(p, d.field)), esc(&wb.data_caption(p, d)), func(d.agg));
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
    /// LibreOffice's compact form (`loext:layout-mode="compact-layout"`).
    pub compact: bool,
    pub hidden: Vec<String>,
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
        let index = |name: &str| (0..width).find(|&f| wb.pivot_field_name(&probe, f) == name);
        let mut bad = None;
        let mut subtotals = false;
        let mut multi_on_rows = false;
        let mut compact = None;
        for f in &t.fields {
            if f.data_layout {
                multi_on_rows |= f.orient == "row";
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
                let d = DataField { field: x, agg, name: None };
                let default = wb.data_caption(&p, &d);
                let name = f.display.clone().filter(|n| *n != default).map(|n| Arc::from(n.as_str()));
                p.data.push(DataField { name, ..d });
                continue;
            }
            let items = wb.pivot_items(&p, x);
            let find = |n: &str| items.iter().find(|v| member(v).eq_ignore_ascii_case(n)).cloned().unwrap_or_else(|| if n.is_empty() { Value::Empty } else { Value::str(n) });
            let mut hidden: Vec<Value> = f.hidden.iter().map(|n| find(n)).collect();
            if let Some(sel) = f.selected.as_ref().filter(|_| !f.ignore_selected) {
                if f.orient == "page" {
                    let keep = find(sel);
                    hidden = items.iter().filter(|v| !same_item(v, &keep)).cloned().collect();
                }
            }
            let pf = PivotField { field: x, hidden };
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
        if multi_on_rows && p.data.len() > 1 {
            rep.note(Kind::Approximate, "pivot table values shown down the rows (shown across)");
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
