//! Conditional formats (docs/spec/officina.md §16.31): which cells each kind of rule
//! picks, as Excel picks them, and the native file keeping them.

use officina_grid::condfmt::{Cfvo, CmpOp, CondFormat, Dxf, Rule, RuleKind, TextOp};
use officina_grid::{CellAddr, Rect, Workbook};
use std::sync::Arc;

const RED: u32 = 0xFFC7CE;

fn book() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("S").unwrap();
    // A1:A10 = 1..10, B1:B6 text, C1:C6 with blanks and an error.
    for i in 0..10u32 {
        wb.set_input(CellAddr::new(0, i, 0), &(i + 1).to_string()).unwrap();
    }
    for (i, t) in ["apple", "Banana", "cherry", "apple", "date", "Apple pie"].iter().enumerate() {
        wb.set_input(CellAddr::new(0, i as u32, 1), t).unwrap();
    }
    wb.set_input(CellAddr::new(0, 0, 2), "1").unwrap();
    wb.set_input(CellAddr::new(0, 2, 2), "=1/0").unwrap();
    wb.set_input(CellAddr::new(0, 3, 2), " ").unwrap();
    wb.recalc();
    wb
}

fn col(c: u32, rows: u32) -> Rect {
    Rect { sheet: 0, r0: 0, c0: c, r1: rows - 1, c1: c }
}

fn fill() -> Dxf {
    Dxf { fill: Some(RED), ..Dxf::default() }
}

/// Which rows of column `c` the rule fills.
fn picks(wb: &mut Workbook, c: u32, rows: u32, kind: RuleKind) -> Vec<u32> {
    while !wb.cond_formats().is_empty() {
        wb.remove_cond_format(0);
    }
    wb.add_cond_format(CondFormat { sheet: 0, ranges: vec![col(c, rows)], rules: vec![Rule { kind, dxf: fill(), stop: false }] }).unwrap();
    (0..rows).filter(|&r| wb.look(CellAddr::new(0, r, c)).dxf.fill == Some(RED)).map(|r| r + 1).collect()
}

#[test]
fn rules_pick_the_cells_excel_picks() {
    let mut wb = book();
    let f = |wb: &Workbook, s: &str| wb.cond_formula(s, 0, (0, 0)).unwrap();
    let k = RuleKind::CellIs { op: CmpOp::Greater, a: f(&wb, "7"), b: None };
    assert_eq!(picks(&mut wb, 0, 10, k), [8, 9, 10]);
    let k = RuleKind::CellIs { op: CmpOp::Between, a: f(&wb, "6"), b: Some(f(&wb, "3")) };
    assert_eq!(picks(&mut wb, 0, 10, k), [3, 4, 5, 6]);
    let k = RuleKind::CellIs { op: CmpOp::NotBetween, a: f(&wb, "3"), b: Some(f(&wb, "8")) };
    assert_eq!(picks(&mut wb, 0, 10, k), [1, 2, 9, 10]);
    let k = RuleKind::CellIs { op: CmpOp::Equal, a: f(&wb, "\"apple\""), b: None };
    assert_eq!(picks(&mut wb, 1, 6, k), [1, 4]);
    // Formulas are written for the first cell; relative references move.
    let k = RuleKind::Expression(f(&wb, "MOD(A1,3)=0"));
    assert_eq!(picks(&mut wb, 0, 10, k), [3, 6, 9]);
    let k = RuleKind::Expression(f(&wb, "$A1>$A$9"));
    assert_eq!(picks(&mut wb, 0, 10, k), [10]);
    let k = RuleKind::Text { op: TextOp::Contains, text: Arc::from("APP") };
    assert_eq!(picks(&mut wb, 1, 6, k), [1, 4, 6]);
    let k = RuleKind::Text { op: TextOp::BeginsWith, text: Arc::from("b") };
    assert_eq!(picks(&mut wb, 1, 6, k), [2]);
    let k = RuleKind::Text { op: TextOp::EndsWith, text: Arc::from("e") };
    assert_eq!(picks(&mut wb, 1, 6, k), [1, 4, 5, 6]);
    assert_eq!(picks(&mut wb, 2, 6, RuleKind::Blanks(true)), [2, 4, 5, 6]);
    assert_eq!(picks(&mut wb, 2, 6, RuleKind::Errors(true)), [3]);
    assert_eq!(picks(&mut wb, 0, 10, RuleKind::Top { n: 3, percent: false, bottom: false }), [8, 9, 10]);
    assert_eq!(picks(&mut wb, 0, 10, RuleKind::Top { n: 2, percent: false, bottom: true }), [1, 2]);
    assert_eq!(picks(&mut wb, 0, 10, RuleKind::Top { n: 20, percent: true, bottom: false }), [9, 10]);
    assert_eq!(picks(&mut wb, 0, 10, RuleKind::Average { below: false, equal: false, std_dev: 0 }), [6, 7, 8, 9, 10]);
    assert_eq!(picks(&mut wb, 0, 10, RuleKind::Average { below: true, equal: true, std_dev: 0 }), [1, 2, 3, 4, 5]);
    // Above one standard deviation (3.03) over the mean (5.5).
    assert_eq!(picks(&mut wb, 0, 10, RuleKind::Average { below: false, equal: false, std_dev: 1 }), [9, 10]);
    assert_eq!(picks(&mut wb, 1, 6, RuleKind::Duplicates { unique: false }), [1, 4]);
    assert_eq!(picks(&mut wb, 1, 6, RuleKind::Duplicates { unique: true }), [2, 3, 5, 6]);
}

#[test]
fn rules_follow_edits_and_stop() {
    let mut wb = book();
    let top = Rule { kind: RuleKind::Top { n: 1, percent: false, bottom: false }, dxf: Dxf { bold: Some(true), ..fill() }, stop: true };
    let big = Rule { kind: RuleKind::CellIs { op: CmpOp::Greater, a: wb.cond_formula("8", 0, (0, 0)).unwrap(), b: None }, dxf: Dxf { color: Some(0x9C0006), italic: Some(true), ..Dxf::default() }, stop: false };
    wb.add_cond_format(CondFormat { sheet: 0, ranges: vec![col(0, 10)], rules: vec![top, big] }).unwrap();
    let l = wb.look(CellAddr::new(0, 9, 0));
    assert_eq!((l.dxf.fill, l.dxf.bold, l.dxf.italic), (Some(RED), Some(true), None), "stop after the first");
    let l = wb.look(CellAddr::new(0, 8, 0));
    assert_eq!((l.dxf.fill, l.dxf.italic, l.dxf.color), (None, Some(true), Some(0x9C0006)));
    // Statistics follow a recalculation.
    wb.set_input(CellAddr::new(0, 0, 0), "50").unwrap();
    wb.recalc();
    assert_eq!(wb.look(CellAddr::new(0, 0, 0)).dxf.bold, Some(true));
    assert_eq!(wb.look(CellAddr::new(0, 9, 0)).dxf.bold, None);
}

#[test]
fn scales_and_bars() {
    let mut wb = book();
    let scale = RuleKind::ColorScale(vec![(Cfvo::Min, 0xF8696B), (Cfvo::Percentile(50.0), 0xFFEB84), (Cfvo::Max, 0x63BE7B)]);
    let bar = RuleKind::DataBar { min: Cfvo::Min, max: Cfvo::Max, color: 0x638EC6 };
    wb.add_cond_format(CondFormat { sheet: 0, ranges: vec![col(0, 10)], rules: vec![Rule { kind: scale, dxf: Dxf::default(), stop: false }, Rule { kind: bar, dxf: Dxf::default(), stop: false }] }).unwrap();
    let at = |r: u32| wb.look(CellAddr::new(0, r, 0));
    assert_eq!(at(0).scale, Some(0xF8696B));
    assert_eq!(at(9).scale, Some(0x63BE7B));
    // 5.5 is the median: the middle colour; 1 is the shortest bar, 10 the full one.
    assert_eq!(officina_grid::condfmt::scale_color(&[(1.0, 0xF8696B), (5.5, 0xFFEB84), (10.0, 0x63BE7B)], 5.5), 0xFFEB84);
    assert_eq!(at(0).bar.map(|b| b.0), Some(0.1));
    assert_eq!(at(9).bar.map(|b| b.0), Some(1.0));
    assert!((at(4).bar.unwrap().0 - 0.5).abs() < 1e-6, "5 is 4/9 of the way: 0.1 + 0.9 × 4/9");
}

#[test]
fn native_files_keep_conditional_formats() {
    let mut wb = book();
    let a = wb.cond_formula("$A1>B$1", 0, (2, 3)).unwrap();
    wb.add_cond_format(CondFormat {
        sheet: 0,
        ranges: vec![Rect { sheet: 0, r0: 2, c0: 3, r1: 9, c1: 4 }, col(6, 3)],
        rules: vec![
            Rule { kind: RuleKind::Expression(a), dxf: Dxf { fill: Some(1), num_fmt: Some(Arc::from("0.0")), strike: Some(false), ..Dxf::default() }, stop: true },
            Rule { kind: RuleKind::ColorScale(vec![(Cfvo::Num(1.0), 2), (Cfvo::Formula(wb.cond_formula("$A$10", 0, (2, 3)).unwrap()), 3)]), dxf: Dxf::default(), stop: false },
            Rule { kind: RuleKind::Text { op: TextOp::NotContains, text: Arc::from("x") }, dxf: fill(), stop: false },
        ],
    })
    .unwrap();
    let back = officina_grid::native::load(&officina_grid::native::save_to_vec(&wb)).unwrap();
    assert_eq!(back.cond_formats(), wb.cond_formats());
    assert_eq!(back.cond_formula_text(match &back.cond_formats()[0].rules[0].kind {
        RuleKind::Expression(e) => e,
        _ => unreachable!(),
    }, (2, 3)), "$A1>B$1");
}

#[test]
fn icon_sets_pick_by_threshold() {
    use officina_grid::condfmt::{icon_points, icon_set, ICON_SETS};
    let mut wb = book();
    let arrows = icon_set("3Arrows").unwrap();
    // 1..10 over 0 / 33 / 67 percent of the way from 1 to 10 (3.97, 7.03):
    // 1–3 the lowest icon, 4–7 the middle, 8–10 the top.
    let kind = RuleKind::IconSet { set: arrows, points: icon_points(3), reverse: false, show_value: true, icons: None };
    wb.add_cond_format(CondFormat { sheet: 0, ranges: vec![col(0, 10)], rules: vec![Rule { kind, dxf: Dxf::default(), stop: false }] }).unwrap();
    let icons: Vec<u8> = (0..10).map(|r| wb.look(CellAddr::new(0, r, 0)).icon.unwrap().1).collect();
    assert_eq!(icons, [0, 0, 0, 1, 1, 1, 1, 2, 2, 2]);
    assert!(!wb.look(CellAddr::new(0, 0, 0)).hide_value);
    // Numbers, greater-than, reversed, icon only: 5 is not above 5.
    wb.remove_cond_format(0);
    let rating = icon_set("4Rating").unwrap();
    let points = vec![(Cfvo::Num(0.0), true), (Cfvo::Num(5.0), false), (Cfvo::Num(8.0), true), (Cfvo::Percentile(100.0), true)];
    let kind = RuleKind::IconSet { set: rating, points, reverse: true, show_value: false, icons: None };
    wb.add_cond_format(CondFormat { sheet: 0, ranges: vec![col(0, 10)], rules: vec![Rule { kind, dxf: Dxf::default(), stop: false }] }).unwrap();
    let at = |r: u32| wb.look(CellAddr::new(0, r, 0));
    assert_eq!((at(4).icon, at(5).icon, at(7).icon, at(9).icon), (Some((rating, 3)), Some((rating, 2)), Some((rating, 1)), Some((rating, 0))));
    assert!(at(0).hide_value);
    // Text cells get no icon; the set table names every set once.
    assert_eq!(wb.look(CellAddr::new(0, 0, 1)).icon, None);
    assert_eq!(ICON_SETS.len(), 20);
    // Kept by the native file.
    let back = officina_grid::native::load(&officina_grid::native::save_to_vec(&wb)).unwrap();
    assert_eq!(back.cond_formats(), wb.cond_formats());
}

#[test]
fn changing_formats_is_one_undoable_journaled_op() {
    use officina_core::history::History;
    use officina_grid::ops::GridOp;
    let mut wb = book();
    let mut h: History<GridOp> = History::new();
    let a = |r| CellAddr::new(0, r, 0);
    let f = wb.cond_formula("A1>5", 0, (0, 0)).unwrap();
    let rule = Rule { kind: RuleKind::Expression(f), dxf: fill(), stop: false };
    let op = GridOp::cond_formats(&wb, &[CondFormat { sheet: 0, ranges: vec![col(0, 10)], rules: vec![rule] }]);
    // Through the journal's bytes and back.
    let op = GridOp::decode(&op.encode()).unwrap();
    h.run(&mut wb, "Conditional Format", vec![op]);
    assert_eq!(wb.cond_formats().len(), 1);
    assert_eq!(wb.look(a(5)).dxf.fill, Some(RED));
    assert_eq!(wb.look(a(4)).dxf.fill, None);
    h.undo(&mut wb);
    assert!(wb.cond_formats().is_empty() && wb.look(a(5)).is_empty());
    h.redo(&mut wb);
    assert_eq!(wb.look(a(9)).dxf.fill, Some(RED));
    // A list naming a sheet that isn't there changes nothing.
    let mut other = book();
    other.add_sheet("T").unwrap();
    let bad = GridOp::cond_formats(&other, &[CondFormat { sheet: 1, ranges: vec![Rect { sheet: 1, r0: 0, c0: 0, r1: 0, c1: 0 }], rules: vec![Rule { kind: RuleKind::Blanks(true), dxf: fill(), stop: false }] }]);
    h.run(&mut wb, "Bad", vec![bad]);
    assert_eq!(wb.cond_formats().len(), 1);
    assert!(GridOp::decode(&[8, 200]).is_err());
}

#[test]
fn appearance_filters_follow_the_displayed_priority() {
    use officina_grid::condfmt::{icon_points, icon_set};
    use officina_grid::filter::{AppearanceFilter, AutoFilter, ColorFilter, Cond, IconFilter};

    let mut wb = book();
    let range = col(0, 10);
    let arrows = icon_set("3Arrows").unwrap();
    let stars = icon_set("3Stars").unwrap();
    let formula = wb.cond_formula("A1>=5", 0, (0, 0)).unwrap();
    let rules = vec![
        Rule { kind: RuleKind::Expression(formula), dxf: fill(), stop: false },
        Rule { kind: RuleKind::ColorScale(vec![(Cfvo::Min, 0x008000), (Cfvo::Max, 0xFFFF00)]), dxf: Dxf::default(), stop: false },
        Rule { kind: RuleKind::IconSet { set: arrows, points: icon_points(3), reverse: false, show_value: true, icons: None }, dxf: Dxf::default(), stop: false },
        Rule { kind: RuleKind::IconSet { set: stars, points: icon_points(3), reverse: true, show_value: false, icons: None }, dxf: Dxf::default(), stop: false },
    ];
    wb.add_cond_format(CondFormat { sheet: 0, ranges: vec![range], rules }).unwrap();
    let top = wb.look(CellAddr::new(0, 9, 0));
    assert_eq!(top.scale.or(top.dxf.fill), Some(RED));
    assert_eq!(top.icon, Some((arrows, 2)));
    assert!(!top.hide_value);

    let hidden = |rule| wb.filtered(0, Some(AutoFilter { rect: range, conds: vec![(0, Cond::Appearance(rule))] })).hidden;
    assert_eq!(hidden(AppearanceFilter::Color(ColorFilter { cell: true, rgb: Some(RED) })), vec![(1, 3)]);
    assert_eq!(hidden(AppearanceFilter::Icon(IconFilter::new(arrows, Some(2)).unwrap())), vec![(1, 6)]);
}
