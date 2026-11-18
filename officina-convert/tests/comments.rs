//! Cell comments through .xlsx and .ods and back, and (when LibreOffice is
//! installed) as LibreOffice reads our files and writes its own.

use officina_grid::comments::Comment;
use officina_grid::{CellAddr, Value, Workbook};

fn book() -> Workbook {
    let mut wb = Workbook::new();
    wb.add_sheet("One").unwrap();
    wb.add_sheet("Two").unwrap();
    wb.set_value(CellAddr::new(0, 1, 1), Value::Num(42.0));
    wb.set_value(CellAddr::new(0, 0, 0), Value::str("Name"));
    wb.set_comments(0, vec![
        (1, 1, Some(Comment::new("Ana Ruiz", "Check this against March.\nThe total looks high & <odd>."))),
        // On an empty cell, no author.
        (4, 3, Some(Comment::new("", "Fill in by Friday"))),
    ]);
    wb.set_comments(1, vec![(0, 2, Some(Comment::new("Bo", "Second sheet")))]);
    wb
}

fn all(wb: &Workbook) -> Vec<Vec<(u32, u32, String, String)>> {
    (0..wb.sheet_count() as u16).map(|s| wb.comments(s).iter().map(|(&(r, c), k)| (r, c, k.author.to_string(), k.text.to_string())).collect()).collect()
}

#[test]
fn comments_round_trip_through_xlsx_and_ods() {
    let wb = book();
    let want = all(&wb);
    let x = officina_convert::xlsx::write(&wb, Vec::new()).unwrap();
    let (back, _) = officina_convert::xlsx::read(&x).unwrap();
    assert_eq!(all(&back), want);
    let o = officina_convert::ods::write(&wb, Vec::new()).unwrap();
    let (back, rep) = officina_convert::ods::read(&o).unwrap();
    assert_eq!(all(&back), want);
    assert!(!format!("{rep:?}").contains("comments"), "{rep:?}");
    assert_eq!(back.value(0, 1, 1), Value::Num(42.0));
    assert_eq!(back.value(0, 4, 3), Value::Empty);
}

#[test]
fn excels_bold_author_prefix_is_taken_off() {
    let wb = book();
    let x = officina_convert::xlsx::write(&wb, Vec::new()).unwrap();
    let z = officina_convert::zip::Archive::open(&x).unwrap();
    let mut w = officina_convert::zip::ZipWriter::new(Vec::new());
    for e in &z.entries {
        let mut d = z.read(e).unwrap();
        if e.name == "xl/comments2.xml" {
            let t = String::from_utf8(d).unwrap().replace(r#"<text><r><t xml:space="preserve">Second sheet</t></r></text>"#, r#"<text><r><rPr><b/><sz val="9"/></rPr><t>Bo:</t></r><r><rPr><sz val="9"/></rPr><t xml:space="preserve">
Second sheet</t></r></text>"#);
            d = t.into_bytes();
        }
        w.add(&e.name, &d).unwrap();
    }
    let (back, _) = officina_convert::xlsx::read(&w.finish().unwrap()).unwrap();
    assert_eq!(back.comment(1, 0, 2).map(|k| k.text.to_string()), Some("Second sheet".into()));
}

/// Excel 365's threaded comments: the conversation replaces the legacy
/// stand-in note, replies as "Name: text" paragraphs.
#[test]
fn threaded_comments_are_read_as_their_conversation() {
    let wb = book();
    let x = officina_convert::xlsx::write(&wb, Vec::new()).unwrap();
    let z = officina_convert::zip::Archive::open(&x).unwrap();
    let mut w = officina_convert::zip::ZipWriter::new(Vec::new());
    let rel = |id: &str, ty: &str, target: &str| format!(r#"<Relationship Id="{id}" Type="http://schemas.microsoft.com/office/2017/10/relationships/{ty}" Target="{target}"/></Relationships>"#);
    for e in &z.entries {
        let mut d = z.read(e).unwrap();
        let t = String::from_utf8(d.clone()).unwrap_or_default();
        match e.name.as_str() {
            "xl/comments1.xml" => d = t.replace("Check this against March.\nThe total looks high &amp; &lt;odd&gt;.", "[Threaded comment]\n\nYour version of Excel allows you to read this threaded comment").into_bytes(),
            "xl/worksheets/_rels/sheet1.xml.rels" => d = t.replace("</Relationships>", &rel("rIdTc", "threadedComment", "../threadedComments/threadedComment1.xml")).into_bytes(),
            "xl/_rels/workbook.xml.rels" => d = t.replace("</Relationships>", &rel("rIdP", "person", "persons/person.xml")).into_bytes(),
            _ => {}
        }
        w.add(&e.name, &d).unwrap();
    }
    w.add("xl/persons/person.xml", br#"<?xml version="1.0" encoding="UTF-8"?><personList xmlns="http://schemas.microsoft.com/office/spreadsheetml/2018/threadedcomments"><person displayName="Ana Ruiz" id="{A1}" userId="ana" providerId="None"/><person displayName="Bo Chen" id="{B2}" userId="bo" providerId="None"/></personList>"#).unwrap();
    w.add("xl/threadedComments/threadedComment1.xml", br#"<?xml version="1.0" encoding="UTF-8"?><ThreadedComments xmlns="http://schemas.microsoft.com/office/spreadsheetml/2018/threadedcomments"><threadedComment ref="B2" dT="2026-11-18T10:00:00.00" personId="{A1}" id="{T1}"><text>Is this right?</text></threadedComment><threadedComment ref="B2" dT="2026-11-18T10:05:00.00" personId="{B2}" id="{T2}" parentId="{T1}"><text>Yes, checked &amp; fixed.</text></threadedComment></ThreadedComments>"#).unwrap();
    let (back, rep) = officina_convert::xlsx::read(&w.finish().unwrap()).unwrap();
    let k = back.comment(0, 1, 1).unwrap();
    assert_eq!((&*k.author, &*k.text), ("Ana Ruiz", "Is this right?\n\nBo Chen: Yes, checked & fixed."));
    // The other notes are as they were.
    assert_eq!(back.comment(0, 4, 3).map(|k| k.text.to_string()), Some("Fill in by Friday".into()));
    assert!(format!("{rep:?}").contains("threaded comments"), "{rep:?}");
}

/// LibreOffice opens our file and saves it as the other format; we read that.
#[test]
fn libreoffice_keeps_our_comments() {
    if !std::process::Command::new("soffice").arg("--version").output().is_ok_and(|o| o.status.success()) {
        eprintln!("skipped: no LibreOffice");
        return;
    }
    let wb = book();
    let want = all(&wb);
    let tmp = std::env::temp_dir().join(format!("officina-comments-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    for (ext, to, bytes) in [("xlsx", "ods", officina_convert::xlsx::write(&wb, Vec::new()).unwrap()), ("ods", "xlsx", officina_convert::ods::write(&wb, Vec::new()).unwrap())] {
        let src = tmp.join(format!("c.{ext}"));
        std::fs::write(&src, bytes).unwrap();
        let out = tmp.join(to);
        let st = std::process::Command::new("soffice")
            .args(["--headless", &format!("-env:UserInstallation=file://{}/profile", tmp.display()), "--convert-to", to, "--outdir"])
            .arg(&out)
            .arg(&src)
            .output()
            .unwrap();
        assert!(st.status.success(), "{st:?}");
        let data = std::fs::read(out.join(format!("c.{to}"))).unwrap();
        let back = if to == "ods" { officina_convert::ods::read(&data).unwrap().0 } else { officina_convert::xlsx::read(&data).unwrap().0 };
        let got = all(&back);
        if ext == "xlsx" {
            // LibreOffice drops authors reading any .xlsx (its own too): cells and text only.
            let strip = |v: &Vec<Vec<(u32, u32, String, String)>>| v.iter().map(|s| s.iter().map(|x| (x.0, x.1, x.3.clone())).collect::<Vec<_>>()).collect::<Vec<_>>();
            assert_eq!(strip(&got), strip(&want), "{ext} -> LibreOffice -> {to}");
        } else {
            assert_eq!(got, want, "{ext} -> LibreOffice -> {to}");
        }
    }
    let _ = std::fs::remove_dir_all(&tmp);
}
