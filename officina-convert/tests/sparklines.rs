//! Sparklines survive .xlsx (Excel 2010's x14 extension), across sheets too.

use officina_convert::xlsx;
use officina_grid::addr::Rect;
use officina_grid::sparkline::{SparkKind, Sparkline, DEFAULT_NEGATIVE};
use officina_grid::Workbook;

#[test]
fn sparklines_round_trip_in_xlsx() {
    let mut wb = Workbook::new();
    wb.add_sheet("Data").unwrap();
    wb.add_sheet("My Sheet").unwrap();
    let sp = |row, col, sheet, kind, color, markers| Sparkline { row, col, data: Rect { sheet, r0: row, c0: 0, r1: row, c1: 3 }, kind, color, negative: DEFAULT_NEGATIVE, markers };
    let list = vec![
        sp(0, 5, 0, SparkKind::Line, 0x376092, true),
        sp(1, 5, 0, SparkKind::Line, 0x376092, true),
        sp(2, 5, 1, SparkKind::Column, 0x00B050, false),
        sp(3, 5, 0, SparkKind::WinLoss, 0x112233, false),
    ];
    assert!(wb.set_sparklines(0, list.clone()));
    let file = xlsx::write(&wb, Vec::new()).unwrap();
    let zip = officina_convert::zip::Archive::open(&file).unwrap();
    let part = zip.entries.iter().find(|e| e.name == "xl/worksheets/sheet1.xml").unwrap();
    let text = String::from_utf8(zip.read(part).unwrap()).unwrap();
    assert!(text.contains(r#"<x14:sparklineGroup displayEmptyCellsAs="gap" type="column">"#), "{text}");
    assert!(text.contains("<xm:f>&apos;My Sheet&apos;!A3:D3</xm:f><xm:sqref>F3</xm:sqref>") || text.contains("<xm:f>'My Sheet'!A3:D3</xm:f><xm:sqref>F3</xm:sqref>"), "{text}");
    assert_eq!(text.matches("<x14:sparklineGroup ").count(), 3, "the two alike share a group");
    let (back, rep) = xlsx::read(&file).unwrap();
    let mut got = back.sheet(0).sparklines().to_vec();
    got.sort_by_key(|s| s.row);
    assert_eq!(got, list, "{rep:?}");

    // A sparkline naming a sheet that isn't there is left out, with a note.
    let broken = text.replace("&apos;My Sheet&apos;!", "Gone!").replace("'My Sheet'!", "Gone!");
    let mut w = officina_convert::zip::ZipWriter::new(Vec::new());
    for e in &zip.entries {
        let data = if e.name == "xl/worksheets/sheet1.xml" { broken.clone().into_bytes() } else { zip.read(e).unwrap() };
        w.add(&e.name, &data).unwrap();
    }
    let (back, rep) = xlsx::read(&w.finish().unwrap()).unwrap();
    assert_eq!(back.sheet(0).sparklines().len(), 3);
    assert!(format!("{rep:?}").contains("sparklines with an unknown sheet"), "{rep:?}");
}

/// A sparkline reads the sheet it named: moving tabs (and undoing and
/// redoing the move) keeps it on Rates, in the workbook and in the saved
/// .xlsx; deleting Rates takes the sparkline away instead of repointing it.
#[test]
fn a_tab_move_keeps_a_sparkline_on_its_sheet() {
    use officina_core::history::History;
    use officina_grid::addr::CellAddr;
    use officina_grid::ops::GridOp;
    let mut wb = Workbook::new();
    for name in ["Report", "Rates", "Costs"] {
        wb.add_sheet(name).unwrap();
    }
    for (c, (rate, cost)) in [(1, 9), (5, 5), (9, 1)].into_iter().enumerate() {
        wb.set_input(CellAddr { sheet: 1, row: 0, col: c as u32 }, &rate.to_string()).unwrap();
        wb.set_input(CellAddr { sheet: 2, row: 0, col: c as u32 }, &cost.to_string()).unwrap();
    }
    wb.recalc();
    let rates = Rect { sheet: 1, r0: 0, c0: 0, r1: 0, c1: 2 };
    let sp = Sparkline { row: 0, col: 0, data: rates, kind: SparkKind::Line, color: 0x376092, negative: DEFAULT_NEGATIVE, markers: false };
    let mut h: History<GridOp> = History::new();
    h.run(&mut wb, "Sparklines", vec![GridOp::Sparklines { sheet: 0, list: vec![sp] }]);
    let points = |wb: &Workbook| wb.sheet(0).sparklines()[0].points(wb);
    let source = |wb: &Workbook| wb.sheet_name(wb.sheet(0).sparklines()[0].data.sheet);
    let saved = |wb: &Workbook| {
        let (back, _) = xlsx::read(&xlsx::write(wb, Vec::new()).unwrap()).unwrap();
        let mut back = back;
        back.recalc();
        (source(&back), points(&back))
    };
    let want = vec![Some(1.0), Some(5.0), Some(9.0)];
    // Costs moves before Rates.
    h.run(&mut wb, "Move Sheet", vec![GridOp::MoveSheet { from: 2, to: 1 }]);
    assert_eq!(wb.sheet_name(1), "Costs");
    assert_eq!((source(&wb), points(&wb)), ("Rates".to_string(), want.clone()));
    assert_eq!(saved(&wb), ("Rates".to_string(), want.clone()));
    h.undo(&mut wb);
    assert_eq!(wb.sheet_name(1), "Rates");
    assert_eq!(saved(&wb), ("Rates".to_string(), want.clone()));
    h.redo(&mut wb);
    assert_eq!(wb.sheet_name(2), "Rates");
    assert_eq!(saved(&wb), ("Rates".to_string(), want.clone()));
    // Report itself moving last keeps it too.
    h.run(&mut wb, "Move Sheet", vec![GridOp::MoveSheet { from: 0, to: 2 }]);
    assert_eq!(wb.sheet_name(2), "Report");
    let spark = &wb.sheet(2).sparklines()[0];
    assert_eq!((wb.sheet_name(spark.data.sheet), spark.points(&wb)), ("Rates".to_string(), want.clone()));
    // Deleting Rates (now sheet 1) takes the sparkline that read it away.
    let del = officina_grid::sheetops::delete_sheet_op(&wb, 1).unwrap();
    h.run(&mut wb, "Delete Sheet", vec![del]);
    assert_eq!(wb.sheet_count(), 2);
    assert!(wb.sheet(1).sparklines().is_empty());
    h.undo(&mut wb);
    assert_eq!(wb.sheet_name(wb.sheet(2).sparklines()[0].data.sheet), "Rates");
    // A sheet holding sparklines deletes too, and comes back with them.
    let del = officina_grid::sheetops::delete_sheet_op(&wb, 2).unwrap();
    h.run(&mut wb, "Delete Sheet", vec![del]);
    assert_eq!(wb.sheet_count(), 2);
    h.undo(&mut wb);
    assert_eq!(wb.sheet_name(2), "Report");
    assert_eq!(wb.sheet(2).sparklines()[0].points(&wb), want);
}
