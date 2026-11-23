//! Workbook and desktop checkpoints for recovering Excel pivot caches.

use std::{env, fs, path::Path};

use officina_convert::xlsx;
use officina_convert::zip::{Archive, ZipWriter};
use officina_grid::pivot::{Agg, DataField, Pivot, PivotField, ShowAs};
use officina_grid::{CellAddr, Rect, Value, Workbook};

const CACHE: &str = "xl/pivotCache/pivotCacheDefinition1.xml";
#[cfg(test)]
const CACHE_RELS: &str = "xl/pivotCache/_rels/pivotCacheDefinition1.xml.rels";
#[cfg(test)]
const PIVOT: &str = "xl/pivotTables/pivotTable1.xml";
const WORKBOOK_RELS: &str = "xl/_rels/workbook.xml.rels";
const INPUT: Rect = Rect {
    sheet: 0,
    r0: 0,
    c0: 0,
    r1: 3,
    c1: 1,
};

fn at(row: u32, col: u32) -> CellAddr {
    CellAddr::new(0, row, col)
}

fn fixture() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("Sales").unwrap();
    wb.set_value(at(0, 0), Value::str("Region"));
    wb.set_value(at(0, 1), Value::str("Amount"));
    for (row, (region, amount)) in (1..).zip([("North", 4.0), ("South", 20.0), ("North", 5.0)]) {
        wb.set_value(at(row, 0), Value::str(region));
        wb.set_value(at(row, 1), Value::Num(amount));
    }
    wb.define_name("InputRange", None, "=Sales!$A$1:$B$4")
        .unwrap();
    let mut pivot = Pivot::new("Regional sales", INPUT, at(0, 5));
    pivot.rows.push(PivotField::new(0));
    pivot.data.push(DataField {
        field: 1,
        agg: Agg::Sum,
        name: None,
        show_as: ShowAs::Normal,
    });
    pivot.subtotals = false;
    wb.add_pivot(pivot).unwrap();

    // Saved worksheet output intentionally predates this edit. An import or
    // Save that refreshes implicitly will incorrectly show 15 instead of 9.
    wb.set_value(at(1, 1), Value::Num(10.0));
    wb
}

fn rewrite(
    bytes: &[u8],
    patch: impl Fn(&str, Vec<u8>) -> Option<Vec<u8>>,
    extra: &[(&str, &[u8])],
) -> Vec<u8> {
    let zip = Archive::open(bytes).unwrap();
    let mut out = ZipWriter::new(Vec::new());
    for entry in &zip.entries {
        if let Some(data) = patch(&entry.name, zip.read(entry).unwrap()) {
            out.add(&entry.name, &data).unwrap();
        }
    }
    for (name, data) in extra {
        out.add(name, data).unwrap();
    }
    out.finish().unwrap()
}

fn raw(kind: &str) -> Vec<u8> {
    let bytes = xlsx::write(&fixture(), Vec::new()).unwrap();
    match kind {
        "missing" => {
            let zip = Archive::open(&bytes).unwrap();
            let cache = zip.read_name(CACHE).unwrap().unwrap();
            rewrite(
                &bytes,
                |name, data| match name {
                    CACHE => None, // Direct pivotTable relationship now points nowhere.
                    WORKBOOK_RELS => Some(
                        String::from_utf8(data)
                            .unwrap()
                            .replace(
                                "pivotCache/pivotCacheDefinition1.xml",
                                "pivotCache/relocated1.xml",
                            )
                            .into_bytes(),
                    ),
                    _ => Some(data),
                },
                &[("xl/pivotCache/relocated1.xml", &cache)],
            )
        }
        "unusable" => rewrite(
            &bytes,
            |name, data| {
                Some(if name == CACHE {
                    // A complete source survives ahead of a truncated cache field.
                    br#"<pivotCacheDefinition xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" invalid="1"><cacheSource type="worksheet"><worksheetSource name="InputRange"/></cacheSource><cacheFields count="2"><cacheField name="Region""#.to_vec()
                } else {
                    data
                })
            },
            &[],
        ),
        _ => panic!("unknown fixture {kind}"),
    }
}

fn read(bytes: &[u8], fixture: Option<&str>) -> Workbook {
    let (wb, loaded) = xlsx::read(bytes).unwrap();
    let report = loaded.report.human();
    match fixture {
        Some("missing") => assert!(
            report
                .iter()
                .any(|note| note.contains("recovered through the workbook cache ID")),
            "{report:?}"
        ),
        Some("unusable") => assert!(
            report
                .iter()
                .any(|note| note.contains("recovered from its local source")),
            "{report:?}"
        ),
        None => assert!(report.is_empty(), "{report:?}"),
        _ => panic!("unknown fixture"),
    }
    wb
}

fn cells(wb: &Workbook, north: f64) {
    for (row, (region, value)) in (1..).zip([
        ("North", north),
        ("South", 20.0),
        ("Grand Total", north + 20.0),
    ]) {
        assert_eq!(wb.get(at(row, 5)), Value::str(region));
        assert_eq!(wb.get(at(row, 6)), Value::Num(value));
    }
}

fn check(wb: &Workbook, state: &str) {
    assert_eq!(wb.pivots(), fixture().pivots());
    let edited = matches!(state, "edited" | "refreshed");
    assert_eq!(
        wb.get(at(1, 1)),
        Value::Num(if edited { 8.0 } else { 10.0 })
    );
    assert_eq!(
        wb.get(at(1, 3)),
        if state == "initial" {
            Value::Empty
        } else {
            Value::Num(7.0)
        }
    );
    cells(wb, if state == "refreshed" { 13.0 } else { 9.0 });
}

fn main() {
    let args: Vec<_> = env::args().skip(1).collect();
    match args.as_slice() {
        [cmd, dir] if cmd == "make" => {
            for kind in ["missing", "unusable"] {
                fs::write(Path::new(dir).join(format!("{kind}.xlsx")), raw(kind)).unwrap();
            }
        }
        [cmd, file, state, kind] if cmd == "check" => {
            let path = Path::new(file);
            let bytes = fs::read(path).unwrap();
            let wb = if path.extension().is_some_and(|ext| ext == "osheet") {
                officina_grid::native::load(&bytes).unwrap()
            } else {
                read(&bytes, (state == "initial").then_some(kind.as_str()))
            };
            check(&wb, state);
        }
        _ => {
            panic!("usage: sheets_pivot_cache_fixture make DIR | check FILE STATE missing|unusable")
        }
    }
}

#[test]
fn repaired_caches_keep_saved_results_until_refresh() {
    use officina_core::history::Op;

    for kind in ["missing", "unusable"] {
        let mut wb = read(&raw(kind), Some(kind));
        check(&wb, "initial");
        wb.set_value(at(1, 3), Value::Num(7.0));
        let mut reopened = read(&xlsx::write(&wb, Vec::new()).unwrap(), None);
        check(&reopened, "saved");
        reopened.set_value(at(1, 1), Value::Num(8.0));
        check(&reopened, "edited");
        let change = reopened
            .pivot_op(Some(0), Some(reopened.pivots()[0].clone()))
            .unwrap();
        let undo = change.apply(&mut reopened);
        check(
            &read(&xlsx::write(&reopened, Vec::new()).unwrap(), None),
            "refreshed",
        );
        officina_grid::ops::GridOp::decode(&undo.encode())
            .unwrap()
            .apply(&mut reopened);
        check(&reopened, "edited");
    }
}

#[test]
fn unresolved_sources_and_hidden_items_stay_as_saved_cells() {
    let original = xlsx::write(&fixture(), Vec::new()).unwrap();
    let missing = rewrite(&original, |name, data| (name != CACHE).then_some(data), &[]);
    let external = rewrite(&original, |name, data| {
        Some(if name == CACHE {
            String::from_utf8(data).unwrap().replace("sheet=\"Sales\"/", "sheet=\"Sales\" r:id=\"external\"/").into_bytes()
        } else {
            data
        })
    }, &[(CACHE_RELS, br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="external" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" TargetMode="External" Target="other.xlsx"/></Relationships>"#)]);
    let hidden = rewrite(
        &raw("unusable"),
        |name, data| {
            Some(if name == PIVOT {
                let xml = String::from_utf8(data).unwrap();
                assert!(xml.contains(r#"<item x="0"/>"#));
                xml.replace(r#"<item x="0"/>"#, r#"<item x="0" h="1"/>"#)
                    .into_bytes()
            } else {
                data
            })
        },
        &[],
    );
    for (bytes, reason) in [
        (&missing, "pivot cache part is missing"),
        (&external, "worksheet source relationship is external"),
        (&hidden, "hidden pivot items could not be identified"),
    ] {
        let (mut wb, loaded) = xlsx::read(bytes).unwrap();
        assert!(
            loaded
                .report
                .human()
                .iter()
                .any(|note| note.contains(reason)),
            "{:?}",
            loaded.report
        );
        assert!(wb.pivots().is_empty());
        wb.set_value(at(1, 1), Value::Num(8.0));
        assert!(wb.refresh_pivots().is_empty());
        cells(&wb, 9.0);
    }
}

#[test]
fn malformed_shared_items_are_rebuilt_with_a_fixed_source() {
    let original = xlsx::write(&fixture(), Vec::new()).unwrap();
    let damaged = rewrite(
        &original,
        |name, data| {
            Some(if name == CACHE {
                let xml = String::from_utf8(data).unwrap();
                assert!(xml.contains(r#"<sharedItems count="2">"#));
                xml.replace(r#"<sharedItems count="2">"#, r#"<sharedItems count="3">"#)
                    .into_bytes()
            } else {
                data
            })
        },
        &[],
    );
    let mut wb = read(&damaged, Some("unusable"));
    cells(&wb, 9.0);
    wb.refresh_pivot(0).unwrap();
    cells(&wb, 15.0);
    assert_eq!(
        read(&xlsx::write(&wb, Vec::new()).unwrap(), None).pivots(),
        wb.pivots()
    );
}

#[test]
fn an_intact_table_cache_for_the_same_source_keeps_indexed_filters() {
    let mut wb = fixture();
    let mut pivot = wb.pivots()[0].clone();
    pivot.rows[0].hidden.push(Value::str("North"));
    wb.set_pivot(0, pivot.clone()).unwrap();
    let bytes = xlsx::write(&wb, Vec::new()).unwrap();
    let cache = Archive::open(&bytes)
        .unwrap()
        .read_name(CACHE)
        .unwrap()
        .unwrap();
    let patched = rewrite(
        &bytes,
        |name, data| {
            Some(match name {
            CACHE => br#"<pivotCacheDefinition><cacheSource type="worksheet"><worksheetSource ref="A1:B4" sheet="Sales"/></cacheSource></pivotCacheDefinition>"#.to_vec(),
            "xl/pivotTables/_rels/pivotTable1.xml.rels" => String::from_utf8(data).unwrap()
                .replace("pivotCacheDefinition1.xml", "alternate1.xml").into_bytes(),
            _ => data,
        })
        },
        &[("xl/pivotCache/alternate1.xml", &cache)],
    );
    let mut wb = read(&patched, None);
    assert_eq!(wb.pivots(), &[pivot]);
    wb.set_value(at(1, 1), Value::Num(8.0));
    wb.refresh_pivot(0).unwrap();
    assert_eq!(wb.get(at(1, 5)), Value::str("South"));
    assert_eq!(wb.get(at(1, 6)), Value::Num(20.0));
    assert_eq!(wb.get(at(2, 6)), Value::Num(20.0));
}
