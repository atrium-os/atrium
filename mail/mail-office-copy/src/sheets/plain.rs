//! Minimal standards-based packages need no default styles or producer
//! templates. Read their saved cells directly, accounting for each XML node.
use super::{check, xml, Error, Parts, O, T, TEXT, X};
use officina_grid::{
    addr::{parse_a1, CellAddr},
    Value, Workbook,
};
use roxmltree::{Document, Node};
use std::collections::{BTreeMap, BTreeSet};

const CT: &str = "http://schemas.openxmlformats.org/package/2006/content-types";
const REL: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const MAN: &str = "urn:oasis:names:tc:opendocument:xmlns:manifest:1.0";
const MIME: &str = "application/vnd.oasis.opendocument.spreadsheet";
type N<'a> = Node<'a, 'a>;

fn part<'a>(parts: &'a Parts, name: &str) -> Result<Document<'a>, Error> {
    xml(parts.get(name).ok_or("missing workbook part")?)
}
fn elem(n: N, ns: &str, name: &str, attrs: &[(&str, &str)]) -> Result<(), Error> {
    check(
        n.has_tag_name((ns, name))
            && n.attributes()
                .all(|a| attrs.contains(&(a.namespace().unwrap_or(""), a.name()))),
        "unsupported spreadsheet markup",
    )
}
fn kids(n: N) -> Result<Vec<N>, Error> {
    check(
        n.children().all(|c| {
            c.is_element()
                || c.is_comment()
                || c.is_text() && c.text().unwrap_or("").trim().is_empty()
        }),
        "unexpected worksheet text",
    )?;
    Ok(n.children().filter(Node::is_element).collect())
}
fn only(n: N) -> Result<N, Error> {
    let k = kids(n)?;
    check(k.len() == 1, "missing or duplicate spreadsheet element")?;
    Ok(k[0])
}
fn text(n: N) -> Result<String, Error> {
    check(
        n.children().all(|c| c.is_text()),
        "unsupported rich cell text",
    )?;
    Ok(n.children().filter_map(|c| c.text()).collect())
}
fn num(v: &str) -> Result<Value, Error> {
    let n = v.parse::<f64>()?;
    check(n.is_finite(), "invalid stored number")?;
    Ok(Value::Num(n))
}
fn named<'a>(n: N<'a>, ns: &str, key: &str) -> Result<&'a str, Error> {
    n.attribute((ns, key))
        .ok_or_else(|| "missing spreadsheet attribute".into())
}
fn attr<'a>(n: N<'a>, key: &str) -> Result<&'a str, Error> {
    n.attribute(key)
        .ok_or_else(|| "missing spreadsheet attribute".into())
}

fn open_ods(parts: &Parts) -> Result<Workbook, Error> {
    check(
        parts.len() == 3 && parts.get("mimetype").is_some_and(|v| v == MIME.as_bytes()),
        "unsupported ODS package",
    )?;
    let manifest = part(parts, "META-INF/manifest.xml")?;
    let root = manifest.root_element();
    elem(root, MAN, "manifest", &[(MAN, "version")])?;
    let mut paths = BTreeSet::new();
    for n in kids(root)? {
        elem(
            n,
            MAN,
            "file-entry",
            &[(MAN, "full-path"), (MAN, "media-type"), (MAN, "version")],
        )?;
        let p = named(n, MAN, "full-path")?;
        check(
            kids(n)?.is_empty()
                && paths.insert(p)
                && matches!(
                    (p, named(n, MAN, "media-type")?),
                    ("/", MIME) | ("content.xml", "text/xml")
                ),
            "unsupported ODS manifest entry",
        )?;
    }
    check(paths.len() == 2, "incomplete ODS manifest")?;
    let doc = part(parts, "content.xml")?;
    let root = doc.root_element();
    elem(root, O, "document-content", &[(O, "version")])?;
    let body = only(root)?;
    elem(body, O, "body", &[])?;
    let book = only(body)?;
    elem(book, O, "spreadsheet", &[])?;
    let mut w = Workbook::new();
    let mut count = 0;
    for sheet in kids(book)? {
        elem(sheet, T, "table", &[(T, "name")])?;
        let i = w.add_sheet(named(sheet, T, "name")?)?;
        check(i < 24, "too many sheets")?;
        for (r, row) in kids(sheet)?.into_iter().enumerate() {
            elem(row, T, "table-row", &[])?;
            check(r < 4096, "worksheet too tall")?;
            for (c, cell) in kids(row)?.into_iter().enumerate() {
                count += 1;
                check(c < 256 && count <= 100_000, "too many cells")?;
                elem(
                    cell,
                    T,
                    "table-cell",
                    &[
                        (O, "value-type"),
                        (O, "value"),
                        (O, "boolean-value"),
                        (O, "string-value"),
                    ],
                )?;
                let k = kids(cell)?;
                check(k.len() <= 1, "unsupported multiline cell")?;
                let display = k
                    .first()
                    .map(|p| {
                        elem(*p, TEXT, "p", &[])?;
                        text(*p)
                    })
                    .transpose()?;
                let v = match cell.attribute((O, "value-type")) {
                    None if k.is_empty() && cell.attributes().len() == 0 => continue,
                    Some("string") => Value::str(
                        cell.attribute((O, "string-value"))
                            .or(display.as_deref())
                            .ok_or("missing string")?,
                    ),
                    Some("float") => num(named(cell, O, "value")?)?,
                    Some("boolean") => Value::Bool(named(cell, O, "boolean-value")?.parse()?),
                    _ => return Err("unsupported ODS cell type".into()),
                };
                let key = match v {
                    Value::Str(_) => "string-value",
                    Value::Num(_) => "value",
                    Value::Bool(_) => "boolean-value",
                    _ => unreachable!(),
                };
                check(
                    cell.attributes()
                        .all(|a| a.name() == "value-type" || a.name() == key),
                    "ambiguous ODS cell cache",
                )?;
                let a = CellAddr::new(i, r as u32, c as u32);
                w.set_value(a, v);
                check(
                    display.is_none_or(|d| w.formatted(a).text == d),
                    "saved display differs from value",
                )?;
            }
        }
    }
    check(w.sheet_count() > 0, "empty workbook")?;
    Ok(w)
}

fn rels<'a>(doc: &'a Document) -> Result<BTreeMap<&'a str, (&'a str, &'a str)>, Error> {
    let root = doc.root_element();
    elem(root, REL, "Relationships", &[])?;
    let mut out = BTreeMap::new();
    for n in kids(root)? {
        elem(
            n,
            REL,
            "Relationship",
            &[("", "Id"), ("", "Type"), ("", "Target")],
        )?;
        let target = attr(n, "Target")?;
        let kind = attr(n, "Type")?
            .strip_prefix(R)
            .and_then(|s| s.strip_prefix('/'))
            .ok_or("unsupported relationship")?;
        check(
            kids(n)?.is_empty()
                && !target.contains("..")
                && out.insert(attr(n, "Id")?, (kind, target)).is_none(),
            "invalid workbook relationship",
        )?;
    }
    Ok(out)
}

fn open_xlsx(parts: &Parts) -> Result<Workbook, Error> {
    let pkg = part(parts, "_rels/.rels")?;
    let links = rels(&pkg)?;
    check(
        links.len() == 1 && links.values().next() == Some(&("officeDocument", "xl/workbook.xml")),
        "invalid XLSX root",
    )?;
    let wb = part(parts, "xl/workbook.xml")?;
    let wr = part(parts, "xl/_rels/workbook.xml.rels")?;
    let mut links = rels(&wr)?;
    let root = wb.root_element();
    elem(root, X, "workbook", &[])?;
    let sheets = only(root)?;
    elem(sheets, X, "sheets", &[])?;
    let mut expected = BTreeMap::from([(
        "/xl/workbook.xml".to_owned(),
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml",
    )]);
    let mut w = Workbook::new();
    let mut count = 0;
    let mut ids = BTreeSet::new();
    for s in kids(sheets)? {
        elem(
            s,
            X,
            "sheet",
            &[("", "name"), ("", "sheetId"), ("", "state"), (R, "id")],
        )?;
        check(
            kids(s)?.is_empty()
                && s.attribute("state").unwrap_or("visible") == "visible"
                && ids.insert(attr(s, "sheetId")?.parse::<u32>()?),
            "invalid or hidden sheet",
        )?;
        let (kind, target) = links
            .remove(named(s, R, "id")?)
            .ok_or("missing worksheet relationship")?;
        check(
            kind == "worksheet" && target.starts_with("worksheets/"),
            "unsupported sheet type",
        )?;
        let path = format!("xl/{target}");
        check(
            expected
                .insert(
                    format!("/{path}"),
                    "application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml",
                )
                .is_none(),
            "duplicate worksheet",
        )?;
        let i = w.add_sheet(attr(s, "name")?)?;
        check(i < 24, "too many sheets")?;
        let doc = part(parts, &path)?;
        let root = doc.root_element();
        elem(root, X, "worksheet", &[])?;
        let data = only(root)?;
        elem(data, X, "sheetData", &[])?;
        let mut prev_row = 0;
        for row in kids(data)? {
            elem(row, X, "row", &[("", "r")])?;
            let rr = attr(row, "r")?.parse::<u32>()?;
            check(rr > prev_row && rr <= 4096, "invalid worksheet row")?;
            prev_row = rr;
            let mut previous = None;
            for cell in kids(row)? {
                elem(cell, X, "c", &[("", "r"), ("", "t")])?;
                let (r, c) = parse_a1(attr(cell, "r")?).ok_or("invalid cell address")?;
                count += 1;
                check(
                    r + 1 == rr
                        && c < 256
                        && count <= 100_000
                        && previous.is_none_or(|last| c > last),
                    "invalid worksheet cells",
                )?;
                previous = Some(c);
                let child = only(cell)?;
                let v = match cell.attribute("t").unwrap_or("n") {
                    "inlineStr" => {
                        elem(child, X, "is", &[])?;
                        let t = only(child)?;
                        elem(t, X, "t", &[])?;
                        Value::str(&text(t)?)
                    }
                    ty => {
                        elem(child, X, "v", &[])?;
                        let t = text(child)?;
                        match ty {
                            "n" => num(&t)?,
                            "b" if t == "1" || t == "0" => Value::Bool(t == "1"),
                            "str" => Value::str(&t),
                            _ => return Err("unsupported cell type".into()),
                        }
                    }
                };
                w.set_value(CellAddr::new(i, r, c), v);
            }
        }
    }
    check(
        w.sheet_count() > 0 && links.is_empty() && parts.len() == expected.len() + 3,
        "unaccounted workbook parts",
    )?;
    let ct = part(parts, "[Content_Types].xml")?;
    let root = ct.root_element();
    elem(root, CT, "Types", &[])?;
    for n in kids(root)? {
        check(kids(n)?.is_empty(), "unsupported package type")?;
        if n.has_tag_name((CT, "Override")) {
            elem(n, CT, "Override", &[("", "PartName"), ("", "ContentType")])?;
            check(
                expected.remove(attr(n, "PartName")?) == Some(attr(n, "ContentType")?),
                "unexpected package type",
            )?;
        } else {
            elem(n, CT, "Default", &[("", "Extension"), ("", "ContentType")])?;
            check(
                matches!(
                    (attr(n, "Extension")?, attr(n, "ContentType")?),
                    (
                        "rels",
                        "application/vnd.openxmlformats-package.relationships+xml"
                    ) | ("xml", "application/xml")
                ),
                "unsupported default content type",
            )?;
        }
    }
    check(expected.is_empty(), "missing workbook content type")?;
    Ok(w)
}

pub(super) fn read(odf: bool, parts: &Parts) -> Result<Workbook, Error> {
    if odf {
        open_ods(parts)
    } else {
        open_xlsx(parts)
    }
}
