//! Deleting a sheet as one undoable edit made of the ops that exist
//! (officina.md §16.72): references to it become `#REF!`, what sits on it
//! goes, its cells are emptied, then it moves to the end and is taken away.
//! Undo brings each back in turn.

use std::sync::Arc;

use crate::addr::{Rect, MAX_COLS, MAX_ROWS};
use crate::book::Workbook;
use crate::formula::Expr;
use crate::ops::{GridOp, Region, StyleRegion};
use crate::value::ErrorKind;

/// `e` with references to sheet `gone` as `#REF!` (`own`: the formula's sheet).
fn break_refs(e: &Expr, own: u16, gone: u16) -> Expr {
    let hit = |s: Option<u16>| s.unwrap_or(own) == gone;
    let b = |x: &Expr| Box::new(break_refs(x, own, gone));
    match e {
        Expr::Ref(r) | Expr::Spill(r) if hit(r.sheet) => Expr::Err(ErrorKind::Ref),
        Expr::Range(r) if hit(r.sheet) => Expr::Err(ErrorKind::Ref),
        Expr::Neg(x) => Expr::Neg(b(x)),
        Expr::Plus(x) => Expr::Plus(b(x)),
        Expr::Pct(x) => Expr::Pct(b(x)),
        Expr::Paren(x) => Expr::Paren(b(x)),
        Expr::At(x) => Expr::At(b(x)),
        Expr::Bin(op, x, y) => Expr::Bin(*op, b(x), b(y)),
        Expr::Call(f, args) => Expr::Call(f.clone(), args.iter().map(|a| break_refs(a, own, gone)).collect()),
        Expr::Invoke(g, args) => Expr::Invoke(b(g), args.iter().map(|a| break_refs(a, own, gone)).collect()),
        Expr::Array(rows) => Expr::Array(rows.iter().map(|r| r.iter().map(|a| break_refs(a, own, gone)).collect()).collect()),
        other => other.clone(),
    }
}

/// Rename a tab and keep the targets of cell hyperlinks. Text typed into
/// formulas is left alone, including string arguments to HYPERLINK().
pub fn rename_sheet_op(wb: &Workbook, sheet: u16, name: &str) -> Option<GridOp> {
    if sheet as usize >= wb.sheet_count() || wb.sheet_name_ok(name, Some(sheet)).is_err() { return None; }
    let mut ops = vec![GridOp::RenameSheet { sheet, name: name.into() }];
    for own in 0..wb.sheet_count() as u16 {
        let set: Vec<_> = wb.links(own).iter().filter_map(|(&(row, col), link)| {
            let (prefix, range) = link.target.strip_prefix('#')?.rsplit_once('!')?;
            if link.place(wb, own)?.sheet != sheet { return None; }
            let new_prefix = if !prefix.starts_with('\'') && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                name.to_owned()
            } else {
                format!("'{}'", name.replace('\'', "''"))
            };
            Some((row, col, crate::links::Link::new(&format!("#{new_prefix}!{range}"))))
        }).collect();
        if !set.is_empty() { ops.push(GridOp::Links { sheet: own, set }); }
    }
    Some(GridOp::Batch(ops))
}

/// The edit that deletes sheet `s`; None for the only sheet or one that isn't.
pub fn delete_sheet_op(wb: &Workbook, s: u16) -> Option<GridOp> {
    let n = wb.sheet_count();
    if n < 2 || s as usize >= n {
        return None;
    }
    let mut ops = Vec::new();

    // Formulas elsewhere that read it: #REF!.
    for (_, g) in wb.groups() {
        if g.sheet == s {
            continue;
        }
        let e = break_refs(&g.expr, g.sheet, s);
        if e == *g.expr {
            continue;
        }
        let e = Arc::new(e);
        let cells = (g.r0..=g.r1).map(|r| (r, g.col, e.clone())).collect();
        let mut fills = Vec::new();
        crate::shift::fills_of(wb, cells, &mut fills);
        ops.push(GridOp::Replace(Region { rect: Rect { sheet: g.sheet, r0: g.r0, c0: g.col, r1: g.r1, c1: g.col }, values: Vec::new(), fills }));
    }

    // Names: its own go, others reading it break.
    let defs: Vec<_> = wb.names().iter().cloned().collect();
    let kept: Vec<_> = defs
        .iter()
        .filter(|d| d.scope != Some(s))
        .map(|d| crate::names::NameDef { expr: Arc::new(break_refs(&d.expr, d.scope.unwrap_or(u16::MAX), s)), ..d.clone() })
        .collect();
    if kept.len() != defs.len() || kept.iter().zip(&defs).any(|(a, b)| a.expr != b.expr) {
        ops.push(GridOp::names(wb, &kept));
    }

    // What sits on it or reads it: tables, charts, pivot tables, conditional formats.
    let tables: Vec<_> = wb.tables().iter().filter(|t| t.rect.sheet != s).cloned().collect();
    if tables.len() != wb.tables().len() {
        ops.push(GridOp::Tables(tables));
    }
    let reads_sheet = |text: &Option<crate::charts::ChartText>| matches!(text, Some(crate::charts::ChartText::Cell(a)) if a.sheet == s);
    let charts: Vec<_> = wb.charts().iter().filter(|c| c.anchor.sheet != s && !reads_sheet(&c.title)
        && c.series.iter().all(|x| x.vals.sheet != s && x.cats.is_none_or(|r| r.sheet != s) && !reads_sheet(&x.name))).cloned().collect();
    if charts.len() != wb.charts().len() {
        ops.push(GridOp::Charts(charts));
    }
    // Sparklines elsewhere that read it go (as charts do); its own go with it.
    for other in 0..n as u16 {
        let list = wb.sheet(other).sparklines();
        let kept: Vec<_> = list.iter().filter(|sp| other != s && sp.data.sheet != s).cloned().collect();
        if kept.len() != list.len() {
            ops.push(GridOp::Sparklines { sheet: other, list: kept });
        }
    }
    if !wb.sheet(s).edit_ranges().is_empty() {
        ops.push(GridOp::EditRanges { sheet: s, list: Vec::new() });
    }
    if !wb.sheet(s).ignored_errors().is_empty() {
        ops.push(GridOp::IgnoredErrors { sheet: s, list: Vec::new() });
    }
    if wb.sheet(s).protection.is_some() {
        ops.push(GridOp::Protection { sheet: s, protection: None });
    }
    if !wb.sheet(s).watches().is_empty() {
        ops.push(GridOp::Watches { sheet: s, list: Vec::new() });
    }
    if !wb.sheet(s).scenarios().is_empty() {
        ops.push(GridOp::Scenarios { sheet: s, list: Vec::new() });
    }
    let pictures: Vec<_> = wb.pictures().iter().filter(|p| p.anchor.sheet != s).cloned().collect();
    if pictures.len() != wb.pictures().len() {
        ops.push(GridOp::Pictures(pictures));
    }
    let pivots = wb.pivot_state();
    let kept_p: Vec<_> = pivots.iter().filter(|(p, _)| p.source.sheet != s && p.dest.sheet != s).cloned().collect();
    if kept_p.len() != pivots.len() {
        ops.push(GridOp::Pivots(kept_p));
    }
    let cfs: Vec<_> = wb.cond_formats().iter().filter(|f| f.sheet != s).map(|f| {
        let rules = f.rules.iter().map(|rule| crate::condfmt::Rule {
            kind: rule.kind.map_exprs(&|expr| break_refs(expr, f.sheet, s)), ..rule.clone()
        }).collect();
        crate::condfmt::CondFormat { rules, ..f.clone() }
    }).collect();
    if cfs != wb.cond_formats() {
        ops.push(GridOp::cond_formats(wb, &cfs));
    }
    let vs: Vec<_> = wb.validations().iter().filter(|v| v.sheet != s)
        .map(|v| v.map_exprs(&|expr| break_refs(expr, v.sheet, s))).collect();
    if vs != wb.validations() {
        ops.push(GridOp::validations(wb, &vs));
    }

    // Its filter, cells, styles and sizes.
    let st = wb.filter_state(s);
    if st.filter.is_some() || !st.hidden.is_empty() {
        ops.push(GridOp::Filter(crate::filter::FilterState { sheet: s, filter: None, hidden: Vec::new() }));
    }
    let (rows, cols) = wb.extent(s);
    if rows > 0 && cols > 0 {
        ops.push(GridOp::clear(Rect { sheet: s, r0: 0, c0: 0, r1: rows - 1, c1: cols - 1 }));
    }
    let styled = wb.sheet(s).styles.len() as u32;
    if styled > 0 {
        ops.push(GridOp::Restyle(StyleRegion { rect: Rect { sheet: s, r0: 0, c0: 0, r1: MAX_ROWS - 1, c1: (styled - 1).min(MAX_COLS - 1) }, spans: Vec::new() }));
    }
    let sh = wb.sheet(s);
    for (rows, sizes) in [(true, &sh.row_sizes), (false, &sh.col_sizes)] {
        if !sizes.overrides().is_empty() {
            ops.push(GridOp::Sizes { sheet: s, rows, over: Vec::new() });
        }
    }

    // Then to the end, and away.
    if wb.sheet(s).tab_color.is_some() {
        ops.push(GridOp::TabColor { sheet: s, color: None });
    }
    if wb.sheet(s).sheet_hidden {
        ops.push(GridOp::HideSheet { sheet: s, hidden: false });
    }
    if wb.sheet(s).freeze != (0, 0) {
        ops.push(GridOp::Freeze { sheet: s, rows: 0, cols: 0, from: (0, 0) });
    }
    if !wb.sheet(s).hidden_widths.is_empty() {
        ops.push(GridOp::HiddenWidths { sheet: s, list: Vec::new() });
    }
    if !wb.links(s).is_empty() {
        ops.push(GridOp::Links { sheet: s, set: wb.links(s).keys().map(|&(r, c)| (r, c, None)).collect() });
    }
    if !wb.comments(s).is_empty() {
        ops.push(GridOp::Comments { sheet: s, set: wb.comments(s).keys().map(|&(r, c)| (r, c, None)).collect() });
    }
    if !wb.merges(s).is_empty() {
        ops.push(GridOp::Merges { sheet: s, list: Vec::new() });
    }
    for axis in [crate::shift::Axis::Rows, crate::shift::Axis::Cols] {
        if !wb.sheet(s).outline(axis).groups().is_empty() {
            ops.push(GridOp::Outline { sheet: s, axis, outline: Default::default() });
        }
        if !wb.sheet(s).axis_ids[axis.index()].is_implicit() {
            ops.push(GridOp::AxisIdentities { sheet: s, axis, ids: Default::default() });
        }
    }
    ops.push(GridOp::MoveSheet { from: s, to: (n - 1) as u16 });
    ops.push(GridOp::DropSheet);
    Some(GridOp::Batch(ops))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr::CellAddr;
    use crate::book::Input;
    use crate::value::Value;
    use officina_core::history::Op;

    fn input(wb: &Workbook, s: u16, r: u32, c: u32) -> String {
        match wb.input(CellAddr::new(s, r, c)) {
            Input::Empty => String::new(),
            Input::Value(v) => v.display(),
            Input::Formula(f) => f,
        }
    }

    /// Three sheets reading each other, a name, a table, a chart, a
    /// conditional format and a spill.
    fn book() -> Workbook {
        let mut wb = Workbook::new();
        for n in ["Data", "Calc", "Report"] {
            wb.add_sheet(n).unwrap();
        }
        let put = |wb: &mut Workbook, s: u16, a: &str, t: &str| {
            let (r, c) = crate::addr::parse_a1(a).unwrap();
            wb.set_input(CellAddr::new(s, r, c), t).unwrap();
        };
        for (i, v) in ["Qty", "3", "4", "5"].iter().enumerate() {
            put(&mut wb, 0, &format!("A{}", i + 1), v);
        }
        put(&mut wb, 1, "A1", "=SUM(Data!A2:A4)");
        put(&mut wb, 1, "B1", "=SEQUENCE(3)");
        put(&mut wb, 2, "A1", "=Calc!A1*2+Data!A2");
        put(&mut wb, 2, "A2", "=Total");
        wb.define_name("Total", None, "=Calc!$A$1").unwrap();
        wb.define_name("Local", Some(2), "=Data!$A$3").unwrap();
        let t = crate::tables::Table { name: "Q".into(), rect: Rect { sheet: 0, r0: 0, c0: 0, r1: 3, c1: 0 }, header: true, totals: false, columns: vec!["Qty".into()], style: None, stripes: true, filter: None };
        GridOp::Tables(vec![t]).apply(&mut wb);
        let ch = crate::charts::Chart {
            kind: crate::charts::ChartKind::Column,
            grouping: crate::charts::Grouping::Standard,
            title: None,
            series: vec![crate::charts::Series { name: None, cats: None, vals: Rect { sheet: 0, r0: 1, c0: 0, r1: 3, c1: 0 }, kind: None }],
            legend: None,
            x_title: None,
            y_title: None,
            anchor: crate::charts::Anchor { sheet: 2, from: (5, 1, 0.0, 0.0), to: (15, 6, 0.0, 0.0) },
        };
        GridOp::Charts(vec![ch]).apply(&mut wb);
        wb.recalc();
        wb
    }

    fn all_inputs(wb: &Workbook) -> Vec<(String, Vec<String>)> {
        (0..wb.sheet_count() as u16).map(|s| (wb.sheet_name(s), (0..4).flat_map(|r| (0..3).map(move |c| (r, c))).map(|(r, c)| input(wb, s, r, c)).collect())).collect()
    }

    #[test]
    fn a_sheet_moves_with_everything_that_reads_it() {
        let mut wb = book();
        let before = all_inputs(&wb);
        let file = crate::native::save_to_vec(&wb);
        let inv = GridOp::MoveSheet { from: 0, to: 2 }.apply(&mut wb);
        wb.recalc();
        assert_eq!((0..3).map(|s| wb.sheet_name(s)).collect::<Vec<_>>(), ["Calc", "Report", "Data"]);
        assert_eq!(input(&wb, 1, 0, 0), "=Calc!A1*2+Data!A2");
        assert_eq!(wb.value(1, 0, 0), Value::Num(27.0));
        assert_eq!(wb.value(1, 1, 0), Value::Num(12.0), "a name still reads Calc!A1");
        assert_eq!(wb.value(0, 2, 1), Value::Num(3.0), "the spill moved with its sheet");
        assert_eq!(wb.tables()[0].rect.sheet, 2);
        assert_eq!((wb.charts()[0].anchor.sheet, wb.charts()[0].series[0].vals.sheet), (1, 2));
        assert_eq!(wb.names().iter().find(|d| &*d.name == "Local").unwrap().scope, Some(1));
        // An edit after the move recalculates through the moved references.
        wb.set_input(CellAddr::new(2, 1, 0), "10").unwrap();
        wb.recalc();
        assert_eq!(wb.value(1, 0, 0), Value::Num(2.0 * 19.0 + 10.0));
        wb.set_input(CellAddr::new(2, 1, 0), "3").unwrap();
        wb.recalc();
        inv.apply(&mut wb);
        wb.recalc();
        assert_eq!(all_inputs(&wb), before);
        assert_eq!(crate::native::save_to_vec(&wb), file, "moved and back is the same file");
    }

    #[test]
    fn deleting_a_sheet_breaks_what_read_it_and_undo_brings_all_back() {
        let mut wb = book();
        crate::shift::shift_op(&wb, 0, crate::shift::Axis::Rows, 50, 1).unwrap().apply(&mut wb);
        wb.sheet_mut(0).tab_color = Some(0xC62828);
        let before = all_inputs(&wb);
        let file = crate::native::save_to_vec(&wb);
        assert!(delete_sheet_op(&wb, 0).is_some());
        let op = delete_sheet_op(&wb, 0).unwrap();
        assert_eq!(GridOp::decode(&op.encode()).unwrap(), op);
        let inv = op.apply(&mut wb);
        wb.recalc();
        assert_eq!((0..wb.sheet_count() as u16).map(|s| wb.sheet_name(s)).collect::<Vec<_>>(), ["Calc", "Report"]);
        assert_eq!(input(&wb, 0, 0, 0), "=SUM(#REF!)");
        assert_eq!(wb.value(0, 0, 0), Value::Err(ErrorKind::Ref));
        assert_eq!(input(&wb, 1, 0, 0), "=Calc!A1*2+#REF!");
        assert!(wb.tables().is_empty() && wb.charts().is_empty());
        assert_eq!(wb.value(0, 2, 1), Value::Num(3.0));
        inv.apply(&mut wb);
        wb.recalc();
        assert_eq!(all_inputs(&wb), before);
        assert_eq!(wb.value(2, 0, 0), Value::Num(27.0));
        assert_eq!(crate::native::save_to_vec(&wb), file, "deleted and back is the same file");
        // The last one stays; the middle one goes as well as the first.
        let op = delete_sheet_op(&wb, 1).unwrap();
        op.apply(&mut wb);
        wb.recalc();
        assert_eq!(wb.sheet_count(), 2);
        assert_eq!(input(&wb, 1, 0, 0), "=#REF!*2+Data!A2");
        let mut one = Workbook::new();
        one.add_sheet("Only").unwrap();
        assert!(delete_sheet_op(&one, 0).is_none());
    }
}
