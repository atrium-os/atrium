//! Saved values only. Do not give source cell formulas to the editor importer.
use super::Error;
use mail_office_copy::OfficeCopy;
use officina_convert::{
    ods, xlsx,
    zip::{Archive, ZipWriter},
};
use officina_grid::{
    addr::{CellAddr, Rect},
    Value, Workbook,
};
use crate::package::{xml, same_xml};
use roxmltree::Node;
use std::collections::BTreeMap;
mod plain;
mod raster;

const X: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const O: &str = "urn:oasis:names:tc:opendocument:xmlns:office:1.0";
const T: &str = "urn:oasis:names:tc:opendocument:xmlns:table:1.0";
const TEXT: &str = "urn:oasis:names:tc:opendocument:xmlns:text:1.0";
type Parts = BTreeMap<String, Vec<u8>>;

fn check(ok: bool, message: &'static str) -> Result<(), Error> {
    if ok {
        Ok(())
    } else {
        Err(message.into())
    }
}
fn unpack(bytes: &[u8]) -> Result<Parts, Error> {
    let z = Archive::open(bytes)?;
    let mut parts = Parts::new();
    let mut total = 0;
    check(z.entries.len() <= 160, "too many package parts")?;
    for e in &z.entries {
        total += e.size;
        check(
            total <= 8 << 20 && !parts.contains_key(&e.name),
            "oversized or duplicate package part",
        )?;
        check(
            e.name == "mimetype" || e.name.ends_with(".xml") || e.name.ends_with(".rels"),
            "unsupported workbook object",
        )?;
        parts.insert(e.name.clone(), z.read(e)?);
    }
    Ok(parts)
}

// Remove only the expression, after proving the same cell supplies a single
// typed cache. An unsupported cache or extra formula attributes cause refusal.
fn freeze(name: &str, bytes: &[u8], odf: bool) -> Result<Vec<u8>, Error> {
    if name == "mimetype" {
        return Ok(bytes.to_vec());
    }
    let doc = xml(bytes)?;
    let mut cut = vec![];
    let mut expanded_cells = 0u64;
    let mut sheet_rows = 0u32;
    for n in doc.descendants().filter(Node::is_element) {
        let tag = n.tag_name().name();
        if n.has_tag_name((T, "table")) {
            sheet_rows = 0;
        }
        if n.has_tag_name((T, "table-row")) {
            let repeat = |cell: Node, key: &str| -> Result<u32, Error> {
                let n = cell.attribute((T, key)).unwrap_or("1").parse()?;
                check((1..=4096).contains(&n), "oversized repeated range")?;
                Ok(n)
            };
            let rows = repeat(n, "number-rows-repeated")?;
            sheet_rows = sheet_rows.checked_add(rows).ok_or("too many sheet rows")?;
            let mut cols = 0;
            for cell in n.children().filter(Node::is_element) {
                cols += repeat(cell, "number-columns-repeated")?;
                check(cols <= 256, "worksheet too wide")?;
            }
            expanded_cells += u64::from(rows) * u64::from(cols);
            check(
                sheet_rows <= 4096 && expanded_cells <= 100_000,
                "worksheet expansion limit",
            )?;
        }
        if n.has_tag_name((X, "c")) {
            let (r, c) =
                officina_grid::addr::parse_a1(n.attribute("r").ok_or("missing cell address")?)
                    .ok_or("invalid cell address")?;
            expanded_cells += 1;
            check(
                r < 4096 && c < 256 && expanded_cells <= 100_000,
                "worksheet cell limit",
            )?;
        }
        check(
            !matches!(
                tag,
                "definedName"
                    | "named-expression"
                    | "conditionalFormatting"
                    | "content-validation"
                    | "table-source"
                    | "database-range"
                    | "dataPilotTable"
                    | "externalReference"
                    | "script"
                    | "encryption-data"
                    | "pivotTableDefinition"
            ),
            "unsupported workbook rule or active object",
        )?;
        for a in n.attributes() {
            check(
                !(a.name() == "state" && matches!(a.value(), "hidden" | "veryHidden")
                    || a.name() == "hidden" && matches!(a.value(), "true" | "1")
                    || a.name() == "display" && matches!(a.value(), "false" | "0")
                    || a.name() == "visibility" && a.value() != "visible"),
                "workbook has hidden content",
            )?;
            if matches!(a.name(), "number-rows-repeated" | "number-columns-repeated") {
                check(
                    a.value().parse::<usize>()? <= 4096,
                    "oversized repeated range",
                )?;
            }
        }
        if n.has_tag_name((X, "f")) {
            let c = n.parent_element().ok_or("formula outside cell")?;
            check(
                !odf && name.starts_with("xl/worksheets/")
                    && c.has_tag_name((X, "c"))
                    && n.attributes().len() == 0
                    && n.children().all(|x| x.is_text())
                    && c.children().filter(|x| x.has_tag_name((X, "f"))).count() == 1,
                "unsupported formula shape",
            )?;
            let mut vals = c.children().filter(|v| v.has_tag_name((X, "v")));
            let v = vals.next().ok_or("formula has no saved result")?;
            check(
                vals.next().is_none() && v.children().all(|n| n.is_text()),
                "ambiguous formula cache",
            )?;
            let value = v.text().unwrap_or("");
            match c.attribute("t").unwrap_or("n") {
                "n" => check(value.parse::<f64>()?.is_finite(), "invalid saved number")?,
                "b" => check(matches!(value, "0" | "1"), "invalid saved boolean")?,
                "str" => {}
                _ => return Err("unsupported saved result type".into()),
            }
            cut.push(n.range());
        }
        if let Some(a) = n.attribute_node((T, "formula")) {
            check(
                odf && name == "content.xml" && n.has_tag_name((T, "table-cell")),
                "formula outside spreadsheet cell",
            )?;
            match n.attribute((O, "value-type")) {
                Some("float" | "percentage" | "currency") => check(
                    n.attribute((O, "value"))
                        .and_then(|v| v.parse::<f64>().ok())
                        .is_some_and(f64::is_finite),
                    "formula has no saved number",
                )?,
                Some("date") => check(
                    n.attribute((O, "date-value")).is_some(),
                    "formula has no saved date",
                )?,
                Some("boolean") => check(
                    matches!(n.attribute((O, "boolean-value")), Some("true" | "false")),
                    "formula has no saved boolean",
                )?,
                Some("string") => check(
                    n.attribute((O, "string-value")).is_some()
                        || n.children().any(|c| c.has_tag_name((TEXT, "p"))),
                    "formula has no saved string",
                )?,
                _ => return Err("formula has no supported saved result".into()),
            }
            cut.push(a.range());
        }
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut start = 0;
    for range in cut {
        check(start <= range.start, "overlapping formula markup")?;
        out.extend_from_slice(&bytes[start..range.start]);
        start = range.end;
    }
    out.extend_from_slice(&bytes[start..]);
    Ok(out)
}

fn safe_model(w: &Workbook) -> Result<Workbook, Error> {
    check(
        (1..=24).contains(&w.sheet_count()),
        "invalid worksheet count",
    )?;
    let mut clean = Workbook::new();
    let mut cells = 0u64;
    for i in 0..w.sheet_count() as u16 {
        let sh = w.sheet(i);
        clean.add_sheet(&w.sheet_name(i))?;
        check(
            sh.hidden.is_empty() && sh.hidden_widths.is_empty(),
            "hidden rows or columns",
        )?;
        let (rows, cols) = w.extent(i);
        cells += u64::from(rows) * u64::from(cols);
        check(
            rows <= 4096 && cols <= 256 && cells <= 100_000,
            "workbook cell budget exceeded",
        )?;
        clean.sheet_mut(i).row_sizes = sh.row_sizes.clone();
        clean.sheet_mut(i).col_sizes = sh.col_sizes.clone();
        for r in 0..rows {
            for c in 0..cols {
                let a = CellAddr::new(i, r, c);
                let val = w.get(a);
                check(
                    w.formula_text(a).is_none() && !matches!(val, Value::Err(_)),
                    "unsupported cached cell",
                )?;
                if !matches!(val, Value::Empty) {
                    clean.set_value(a, val);
                }
                let style = w.style(a);
                if style != &Default::default() {
                    clean.set_style(
                        &Rect {
                            sheet: i,
                            r0: r,
                            c0: c,
                            r1: r,
                            c1: c,
                        },
                        style.clone(),
                    );
                }
            }
        }
        clean.set_merges(i, w.merges(i).to_vec());
    }
    for chart in w.charts() {
        clean.add_chart(chart.clone())?;
    }
    Ok(clean)
}

pub(super) fn convert(odf: bool, bytes: &[u8]) -> Result<OfficeCopy, Error> {
    let mut parts = unpack(bytes)?;
    let mut frozen = ZipWriter::new(Vec::new());
    for (name, data) in parts.iter_mut() {
        *data = freeze(name, data, odf)?;
        frozen.add(name, data)?;
    }
    // Standards-minimal producers need not write Officina's default styles.
    if let Ok(book) = plain::read(odf, &parts) {
        return raster::workbook(&book);
    }
    let bytes = frozen.finish()?;
    let (book, report) = if odf {
        let (b, r) = ods::read(&bytes)?;
        (b, r.report)
    } else {
        let (b, r) = xlsx::read(&bytes)?;
        (b, r.report)
    };
    check(report.is_empty(), "Officina reported incomplete import")?;
    let clean = safe_model(&book)?;
    // Regenerate only the model this renderer accounts for. The editor can skip
    // unknown XML, so its successful import alone cannot certify completeness.
    let canonical = unpack(&if odf {
        ods::write(&clean, Vec::new())?
    } else {
        xlsx::write(&clean, Vec::new())?
    })?;
    check(
        parts.keys().eq(canonical.keys()),
        "unsupported workbook package parts",
    )?;
    for (name, expected) in canonical {
        let original = &parts[&name];
        if original != &expected && (name == "mimetype" || !same_xml(original, &expected)?) {
            return Err(format!("unsupported workbook content in {name}").into());
        }
    }
    raster::workbook(&clean)
}

#[cfg(test)]
mod tests {
    use super::*;
    const XLSX: &[u8] = include_bytes!("../../tests/fixtures/complete.xlsx");
    const ODS: &[u8] = include_bytes!("../../tests/fixtures/complete.ods");

    fn edit(input: &[u8], part: &str, old: &str, new: &str) -> Vec<u8> {
        let mut parts = unpack(input).unwrap();
        let text = String::from_utf8(parts.remove(part).unwrap()).unwrap();
        assert!(text.contains(old));
        parts.insert(part.to_owned(), text.replacen(old, new, 1).into_bytes());
        let mut z = ZipWriter::new(Vec::new());
        for (n, b) in parts {
            z.add(&n, &b).unwrap();
        }
        z.finish().unwrap()
    }

    #[test]
    fn renders_saved_results_and_all_sheets() {
        for (odf, input, part, formula) in [
            (
                false,
                XLSX,
                "xl/worksheets/sheet3.xml",
                "<f>Data!B1+900</f>",
            ),
            (
                true,
                ODS,
                "content.xml",
                "table:formula=\"of:=[Data.B1]+900\"",
            ),
        ] {
            let saved = convert(odf, input).unwrap();
            assert_eq!(saved.pages.len(), 3);
            let literal = convert(odf, &edit(input, part, formula, "")).unwrap();
            for (a, b) in saved.pages.iter().zip(&literal.pages) {
                assert_eq!(a.samples, b.samples);
            }
            let chart =
                miniz_oxide::inflate::decompress_to_vec_zlib(&saved.pages[1].samples).unwrap();
            // The column series actually painted, in Officina's first series colour.
            assert!(
                chart
                    .chunks_exact(3)
                    .filter(|p| *p == [68, 114, 196])
                    .count()
                    > 500
            );
        }
    }

    #[test]
    fn pages_in_both_directions_and_keeps_empty_tabs() {
        let mut w = Workbook::new();
        w.add_sheet("Empty").unwrap();
        w.add_sheet("Wide and tall").unwrap();
        w.set_value(CellAddr::new(1, 60, 12), Value::str("End"));
        let pages = raster::workbook(&w).unwrap().pages;
        assert_eq!(pages.len(), 5);
        assert!(pages[0].samples != pages[1].samples);
        w.set_value(
            CellAddr::new(1, 60, 12),
            Value::str("A final value that cannot fit in its column"),
        );
        assert!(raster::workbook(&w).is_err());
    }

    #[test]
    fn minimal_packages_preserve_a_saved_zero() {
        let mut expected = Workbook::new();
        expected.add_sheet("Saved").unwrap();
        expected.set_value(CellAddr::new(0, 0, 0), Value::Num(0.));
        let painted = raster::workbook(&expected).unwrap();
        for (odf, input) in [
            (
                false,
                include_bytes!("../../tests/fixtures/minimal.xlsx").as_slice(),
            ),
            (
                true,
                include_bytes!("../../tests/fixtures/minimal.ods").as_slice(),
            ),
        ] {
            let result = convert(odf, input).unwrap();
            assert_eq!(result.pages.len(), 1);
            assert!(result.pages[0].samples == painted.pages[0].samples);
        }
    }
    #[test]
    fn refuses_hidden_last_sheet_missing_cache_and_incomplete_chart() {
        for (odf, input, part, old, new) in [
            (
                false,
                XLSX,
                "xl/workbook.xml",
                "name=\"Last\"",
                "state=\"veryHidden\" name=\"Last\"",
            ),
            (
                true,
                ODS,
                "content.xml",
                "table:name=\"Last\"",
                "table:display=\"false\" table:name=\"Last\"",
            ),
            (false, XLSX, "xl/worksheets/sheet3.xml", "<v>42</v>", ""),
            (true, ODS, "content.xml", "office:value=\"37\"", ""),
            (
                false,
                XLSX,
                "xl/charts/chart1.xml",
                "<c:barChart>",
                "<c:barChart><c:extLst/>",
            ),
            (
                true,
                ODS,
                "Object 1/content.xml",
                "chart:class=\"chart:bar\"",
                "chart:class=\"chart:stock\"",
            ),
        ] {
            assert!(convert(odf, &edit(input, part, old, new)).is_err());
        }
    }
}
