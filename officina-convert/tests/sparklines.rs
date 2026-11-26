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
