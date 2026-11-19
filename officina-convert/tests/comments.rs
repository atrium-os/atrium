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
fn rich_comments_round_trip_through_xlsx_and_ods() {
    use officina_grid::comments::{CommentRun, CommentStyle};

    let mut wb = book();
    let text = "Plain élan\nwith a tab\tand <symbols>.";
    let comment = Comment::with_runs("Ana", text, [
        CommentRun { start: 6, end: "Plain élan\nwith a tab\t".len(), style: CommentStyle { bold: true, italic: true, ..CommentStyle::default() } },
        CommentRun { start: text.find('<').unwrap(), end: text.len(), style: CommentStyle { underline: true, strike: true, ..CommentStyle::default() } },
    ]);
    wb.set_comments(0, vec![(1, 1, Some(comment.clone()))]);
    let x = officina_convert::xlsx::write(&wb, Vec::new()).unwrap();
    let (back, _) = officina_convert::xlsx::read(&x).unwrap();
    assert_eq!(back.comment(0, 1, 1), Some(&comment));
    let o = officina_convert::ods::write(&back, Vec::new()).unwrap();
    let (back, _) = officina_convert::ods::read(&o).unwrap();
    assert_eq!(back.comment(0, 1, 1), Some(&comment));
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

/// Real XLSX relationships, including a reply ahead of its parent without a
/// ref and a root whose legacy placeholder supplies the authoritative cell.
#[test]
fn threaded_comments_preserve_authors_and_replies_through_native_and_xlsx() {
    let mut wb = book();
    wb.add_sheet("Thread only").unwrap();
    let x = officina_convert::xlsx::write(&wb, Vec::new()).unwrap();
    let z = officina_convert::zip::Archive::open(&x).unwrap();
    let mut w = officina_convert::zip::ZipWriter::new(Vec::new());
    let rel = |id: &str, ty: &str, target: &str| format!(r#"<Relationship Id="{id}" Type="http://schemas.microsoft.com/office/2017/10/relationships/{ty}" Target="{target}"/>"#);
    let root = "{AAAAAAAA-0000-4000-8000-000000000001}";
    let reply = "{AAAAAAAA-0000-4000-8000-000000000002}";
    let nested = "{AAAAAAAA-0000-4000-8000-000000000003}";
    let other = "{AAAAAAAA-0000-4000-8000-000000000004}";
    let ana = "{BBBBBBBB-0000-4000-8000-000000000001}";
    let bo = "{BBBBBBBB-0000-4000-8000-000000000002}";
    let ns = "http://schemas.microsoft.com/office/spreadsheetml/2018/threadedcomments";
    for e in &z.entries {
        let mut data = z.read(e).unwrap();
        let xml = String::from_utf8(data.clone()).unwrap_or_default();
        match e.name.as_str() {
            "xl/comments1.xml" => {
                data = xml.replace("<author>Ana Ruiz</author>", &format!("<author>tc={root}</author>"))
                    .replace("Check this against March.\nThe total looks high &amp; &lt;odd&gt;.", "[Threaded comment] Legacy placeholder").into_bytes();
            }
            "xl/worksheets/_rels/sheet1.xml.rels" => {
                data = xml.replace("</Relationships>", &rel("rIdTc", "threadedComment", "../threadedComments/threadedComment1.xml")).into_bytes();
            }
            "xl/_rels/workbook.xml.rels" => {
                data = xml.replace("</Relationships>", &rel("rIdP", "person", "persons/person.xml")).into_bytes();
            }
            _ => {}
        }
        w.add(&e.name, &data).unwrap();
    }
    w.add("xl/worksheets/_rels/sheet3.xml.rels", format!(r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">{}</Relationships>"#, rel("rIdTc", "threadedComment", "../threadedComments/threadedComment3.xml")).as_bytes()).unwrap();
    w.add("xl/persons/person.xml", format!(r#"<personList xmlns="{ns}"><person id="{ana}" displayName="Ana Ruiz" userId="ana@example.invalid" providerId="AD"/><person id="{bo}" displayName="Bo Chen" userId="bo&#9;team" providerId="None"/></personList>"#).as_bytes()).unwrap();
    w.add("xl/threadedComments/threadedComment1.xml", format!(r#"<ThreadedComments xmlns="{ns}">
    <threadedComment id="{reply}" parentId="{root}" personId="{bo}" dT="2026-11-18T10:05:00Z"><text>Yes &amp; fixed.</text></threadedComment>
    <threadedComment ref="C2" id="{root}" personId="{ana}" dT="2026-11-18T10:00:00Z" done="1"><text>Is this right?</text></threadedComment>
    <threadedComment ref="B2" id="{nested}" parentId="{reply}" personId="{ana}"><text> Thanks! _x005F_x000D_ </text></threadedComment>
    </ThreadedComments>"#).as_bytes()).unwrap();
    w.add("xl/threadedComments/threadedComment3.xml", format!(r#"<ThreadedComments xmlns="{ns}"><threadedComment ref="C3" id="{other}" personId="{bo}"><text><![CDATA[Without a legacy note.]]></text></threadedComment></ThreadedComments>"#).as_bytes()).unwrap();

    let (imported, report) = officina_convert::xlsx::read(&w.finish().unwrap()).unwrap();
    assert!(report.report.is_empty(), "{report:?}");
    let k = imported.comment(0, 1, 1).unwrap();
    assert_eq!((&*k.author, &*k.text), ("Ana Ruiz", "Is this right?\n\nBo Chen: Yes & fixed.\n\nAna Ruiz:  Thanks! _x000D_ "));
    assert!(imported.comment(0, 1, 2).is_none());
    let [head, child, grandchild] = k.thread() else { panic!("expected three messages") };
    assert_eq!((&*head.author.name, &*child.author.name, &*grandchild.author.name), ("Ana Ruiz", "Bo Chen", "Ana Ruiz"));
    assert_eq!((&*head.author.user_id, &*head.author.provider_id), ("ana@example.invalid", "AD"));
    assert_eq!(&*child.author.user_id, "bo\tteam");
    assert_eq!(child.parent, Some(head.id));
    assert_eq!(grandchild.parent, Some(child.id));
    assert_eq!(&*child.timestamp, "2026-11-18T10:05:00Z");
    assert!(head.done);
    assert_eq!(imported.comment(2, 2, 2).unwrap().thread()[0].text.as_ref(), "Without a legacy note.");
    assert_eq!(imported.comment(0, 4, 3).unwrap().text.as_ref(), "Fill in by Friday");

    // The same codec is used when a journal contains a comment edit.
    use officina_grid::ops::GridOp;
    let op = GridOp::Comments { sheet: 0, set: vec![(1, 1, Some(k.clone()))] };
    assert_eq!(GridOp::decode(&op.encode()).unwrap(), op);

    let native = officina_grid::native::load(&officina_grid::native::save_to_vec(&imported)).unwrap();
    for s in 0..3 {
        assert_eq!(native.comments(s), imported.comments(s));
    }
    let x = officina_convert::xlsx::write(&native, Vec::new()).unwrap();
    let z = officina_convert::zip::Archive::open(&x).unwrap();
    let part = |name| String::from_utf8(z.read_name(name).unwrap().unwrap()).unwrap();
    let content_types = part("[Content_Types].xml");
    assert!(content_types.contains("application/vnd.ms-excel.threadedcomments+xml"));
    assert!(content_types.contains("application/vnd.ms-excel.person+xml"));
    assert!(part("xl/_rels/workbook.xml.rels").contains("relationships/person"));
    assert!(part("xl/worksheets/_rels/sheet1.xml.rels").contains("relationships/threadedComment"));
    let legacy = part("xl/comments1.xml");
    assert!(legacy.contains(&format!("<author>tc={root}</author>")) && legacy.contains(&format!(r#"xr:uid="{root}""#)));
    let people = part("xl/persons/person.xml");
    assert_eq!(people.matches("<person ").count(), 2);
    let (back, report) = officina_convert::xlsx::read(&x).unwrap();
    assert!(report.report.is_empty(), "{report:?}");
    for s in 0..3 {
        assert_eq!(back.comments(s), imported.comments(s));
    }

    // Copying a conversation to another cell needs new message IDs but the
    // same authors, and the parent links must point at the copy's messages.
    let mut copy = native;
    copy.set_comments(0, vec![(2, 2, Some(k.clone()))]);
    let x = officina_convert::xlsx::write(&copy, Vec::new()).unwrap();
    let (back, _) = officina_convert::xlsx::read(&x).unwrap();
    assert_eq!(back.comment(0, 1, 1).unwrap(), k);
    let [copied_head, copied_child, copied_grandchild] = back.comment(0, 2, 2).unwrap().thread() else { panic!("expected a copied conversation") };
    assert_ne!(copied_head.id, head.id);
    assert_eq!(copied_child.parent, Some(copied_head.id));
    assert_eq!(copied_grandchild.parent, Some(copied_child.id));
    assert_eq!(copied_child.author, child.author);
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
