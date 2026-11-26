//! Checkboxes in cells and Custom Views through .xlsx and .ods
//! (docs/spec/officina.md §16.166).

use officina_core::history::Op;
use officina_convert::{ods, xlsx};
use officina_grid::addr::Rect;
use officina_grid::ops::GridOp;
use officina_grid::style::Style;
use officina_grid::views::CustomView;
use officina_grid::{CellAddr, Value, Workbook};

fn book() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("Tasks").unwrap();
    wb.add_sheet("Plan").unwrap();
    for r in 0..4 {
        wb.set_value(CellAddr::new(0, r, 0), Value::Bool(r % 2 == 0));
    }
    let boxed = Style { checkbox: true, ..Style::default() };
    wb.set_style(&Rect { sheet: 0, r0: 0, c0: 0, r1: 3, c1: 0 }, boxed);
    wb.sheet_mut(1).zoom = 150;
    let a = CustomView::capture(&wb, "Plan wide", 1, (4, 2)).unwrap();
    wb.sheet_mut(1).zoom = 100;
    let b = CustomView::capture(&wb, "Tasks & notes", 0, (2, 0)).unwrap();
    GridOp::CustomViews(vec![a, b]).apply(&mut wb);
    wb
}

fn check(back: &Workbook, wb: &Workbook) {
    for r in 0..4 {
        assert!(back.style(CellAddr::new(0, r, 0)).checkbox, "row {r}");
        assert_eq!(back.value(0, r, 0), wb.value(0, r, 0));
    }
    assert!(!back.style(CellAddr::new(0, 5, 0)).checkbox);
    assert_eq!(back.custom_views(), wb.custom_views());
}

#[test]
fn checkboxes_and_views_survive_xlsx_and_ods() {
    let wb = book();
    let x = xlsx::write(&wb, Vec::new()).unwrap();
    check(&xlsx::read(&x).unwrap().0, &wb);
    check(&ods::read(&ods::write(&wb, Vec::new()).unwrap()).unwrap().0, &wb);

    // What Excel reads: the feature property bag, the xf's complement, the views by name.
    let z = officina_convert::zip::Archive::open(&x).unwrap();
    let part = |n: &str| String::from_utf8(z.read_name(n).unwrap().unwrap_or_default()).unwrap();
    assert!(part("xl/featurePropertyBag/featurePropertyBag.xml").contains(r#"<bag type="Checkbox"/>"#));
    assert!(part("xl/_rels/workbook.xml.rels").contains("featurePropertyBag/featurePropertyBag.xml"));
    assert!(part("xl/styles.xml").contains(r#"<xfpb:xfComplement i="0"/>"#));
    let w = part("xl/workbook.xml");
    assert!(w.contains(r#"<customWorkbookView name="Plan wide""#) && w.contains(r#"name="Tasks &amp; notes""#) && w.contains(r#"activeSheetId="2""#), "{w}");
    let s2 = part("xl/worksheets/sheet2.xml");
    assert!(s2.contains(r#"scale="150"><selection activeCell="C5" sqref="C5"/>"#), "{s2}");

    // A plain book has neither.
    let mut plain = Workbook::new();
    plain.add_sheet("S").unwrap();
    let p = xlsx::write(&plain, Vec::new()).unwrap();
    let z = officina_convert::zip::Archive::open(&p).unwrap();
    assert!(z.find("xl/featurePropertyBag/featurePropertyBag.xml").is_none());
    assert!(!String::from_utf8(z.read_name("xl/workbook.xml").unwrap().unwrap()).unwrap().contains("customWorkbookView"));
}

#[test]
fn bad_views_are_dropped_not_trusted() {
    let wb = book();
    let o = ods::write(&wb, Vec::new()).unwrap();
    let z = officina_convert::zip::Archive::open(&o).unwrap();
    for (what, bad) in [("odd hex", "abc"), ("not hex", "zz"), ("truncated", "2701")] {
        let mut out = officina_convert::zip::ZipWriter::new(Vec::new());
        for e in &z.entries {
            let mut data = z.read(e).unwrap();
            if e.name == "content.xml" {
                let s = String::from_utf8(data).unwrap();
                let (head, rest) = s.split_once("<officina:views").unwrap();
                let open = rest.find('>').unwrap();
                let close = rest.find("</officina:views>").unwrap();
                data = format!("{head}<officina:views{}{bad}{}", &rest[..=open], &rest[close..]).into_bytes();
            }
            out.add(&e.name, &data).unwrap();
        }
        let (back, loaded) = ods::read(&out.finish().unwrap()).unwrap();
        assert!(back.custom_views().is_empty(), "{what}");
        assert!(format!("{:?}", loaded.report).contains("malformed custom views"), "{what}");
    }
}
