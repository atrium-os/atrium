use officina_convert::{ods, xlsx};
use officina_grid::charts::{Anchor, Picture};
use officina_grid::{native, print, Workbook};

#[test]
fn ods_keeps_jpeg_geometry_and_blank_print_bands() {
    let mut book = Workbook::new();
    book.add_sheet("Sales, West").unwrap();
    book.define_name(print::AREA, Some(0), "=$C$7:$F$24")
        .unwrap();
    book.define_name(print::ROWS, Some(0), "=$3:$4").unwrap();
    book.define_name(print::COLS, Some(0), "=$B:$B").unwrap();
    let jpeg = include_bytes!("../../test-assets/themes/wood_table_001_diff_1k.jpg");
    let anchor = Anchor {
        sheet: 0,
        from: (7, 3, 0.0, 0.0),
        to: (9, 5, 0.0, 0.0),
    };
    book.add_picture(Picture {
        anchor,
        bytes: jpeg[..].into(),
        link: None,
        crop: [0; 4],
        rotation: 0,
    })
    .unwrap();

    let package = ods::write(&book, Vec::new()).unwrap();
    let (imported, loaded) = ods::read(&package).unwrap();
    assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
    let back = native::load(&native::save_to_vec(&imported)).unwrap();
    assert_eq!(back.pictures().len(), 1);
    let p = &back.pictures()[0];
    assert_eq!(&*p.bytes, jpeg);
    assert_eq!(p.anchor.sheet, 0);
    for (a, b) in [(p.anchor.from, anchor.from), (p.anchor.to, anchor.to)] {
        assert_eq!((a.0, a.1), (b.0, b.1));
        assert!(a.2 < 0.01 && a.3 < 0.01);
    }
    let area = back.names().get(0, print::AREA).unwrap();
    assert_eq!(print::area(&area.expr, 0), Some([6, 2, 24, 6]));
    assert_eq!(print::titles(&back, 0), (Some((2, 4)), Some((1, 2))));
}

#[test]
fn xlsx_combines_both_titles_on_the_correct_sheet() {
    let mut book = Workbook::new();
    book.add_sheet("Summary").unwrap();
    book.add_sheet("Sales, West").unwrap();
    book.define_name(print::ROWS, Some(1), "=$2:$3").unwrap();
    book.define_name(print::COLS, Some(1), "=$B:$D").unwrap();
    let bytes = xlsx::write(&book, Vec::new()).unwrap();
    let (imported, loaded) = xlsx::read(&bytes).unwrap();
    assert!(loaded.report.is_empty(), "{:?}", loaded.report.human());
    assert_eq!(print::titles(&imported, 0), (None, None));
    assert_eq!(print::titles(&imported, 1), (Some((1, 3)), Some((1, 4))));
}
