//! Headers and footers of their own paragraphs (pictures in line, formatted
//! page numbers, several lines) through our writers and readers; the same
//! document and checks serve the LibreOffice gate (examples/margins_lo.rs).

use std::sync::Arc;

use officina_text::model::{Align, CharAttrs, HeaderFooter, ImageData, Margins, ObjectRef, Para, TextDoc};

/// A 16 × 8 red PNG.
pub const PNG: [u8; 76] = [137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 16, 0, 0, 0, 8, 8, 2, 0, 0, 0, 127, 20, 232, 192, 0, 0, 0, 19, 73, 68, 65, 84, 120, 156, 99, 56, 33, 39, 71, 18, 98, 24, 213, 64, 11, 13, 0, 228, 85, 130, 1, 196, 74, 239, 141, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130];

pub fn margins_doc() -> TextDoc {
    let mut d = TextDoc::new();
    d.blocks.remove(0);
    for i in 0..120 {
        d.push_para(&format!("Body paragraph {i} with enough words to fill the line and run on."), 0, vec![]);
    }
    let img = d.images.add(ImageData { bytes: Arc::from(&PNG[..]), width: 16, height: 8 });
    let pic = d.attrs.intern(CharAttrs { object: Some(ObjectRef { image: img, width: 1440, height: 720, float: None, shape: false }), ..Default::default() });
    let bold = d.attrs.intern(CharAttrs { bold: Some(true), ..Default::default() });
    let logo = Para { runs: vec![(0, pic), (3, bold)], ..Para::new(d.new_id(), "\u{FFFC} Acme Ltd", 0) };
    let num = Para { runs: vec![(0, 0), (5, bold), (11, 0)], ..Para::new(d.new_id(), "Page {page} of {pages}", 0) };
    d.header = Some(HeaderFooter { text: "Acme Ltd Quarterly report".into(), align: Align::Start, paras: vec![logo, Para::new(d.new_id(), "Quarterly report", 0)] });
    d.footer = Some(HeaderFooter { text: "Page {page} of {pages}".into(), align: Align::Start, paras: vec![num] });
    // A title page with no header and a footer of its own; even pages
    // with their own header and footer.
    d.first_page = Some(Margins { header: None, footer: Some(HeaderFooter::line("Title page footer", Align::Center)) });
    let even = Para::new(d.new_id(), "Even page {page}", 0);
    d.even_pages = Some(Margins { header: Some(HeaderFooter::line("Even header", Align::End)), footer: Some(HeaderFooter { text: "Even page {page}".into(), align: Align::Start, paras: vec![even] }) });
    d
}

/// What `r` lost of [`margins_doc`]'s header and footer (empty: all kept).
pub fn check(r: &TextDoc) -> Vec<String> {
    let mut bad = Vec::new();
    let Some(h) = r.header.as_ref() else { return vec!["no header".into()] };
    let texts: Vec<&str> = h.paras.iter().map(|p| p.text.trim()).filter(|t| !t.is_empty()).collect();
    if texts != ["\u{FFFC} Acme Ltd", "Quarterly report"] {
        bad.push(format!("header paragraphs {texts:?}"));
    }
    let pic = h.paras.iter().flat_map(|p| p.runs.iter().map(|r| r.1)).find_map(|a| r.attrs.get(a).object.clone());
    match pic {
        Some(o) if (o.width as i32 - 1440).abs() <= 20 && (o.height as i32 - 720).abs() <= 20 && o.float.is_none() => {
            if r.images.get(o.image).map(|i| (i.width, i.height)) != Some((16, 8)) {
                bad.push("header picture's pixels".into());
            }
        }
        o => bad.push(format!("header picture {o:?}")),
    }
    let bold_at = |p: &Para, s: &str| p.text.find(s).is_some_and(|i| r.attrs.get(p.attr_at(i as u32)).bold == Some(true));
    if !h.paras.first().is_some_and(|p| bold_at(p, "Acme")) {
        bad.push("header bold".into());
    }
    match r.footer.as_ref().and_then(|f| f.paras.iter().find(|p| !p.text.trim().is_empty())) {
        Some(p) if p.text.trim() == "Page {page} of {pages}" => {
            if !bold_at(p, "{page}") || bold_at(p, "Page") {
                bad.push("footer page number's bold".into());
            }
        }
        p => bad.push(format!("footer {:?}", p.map(|p| &p.text))),
    }
    let line = |h: Option<&HeaderFooter>| h.map(|h| h.text.trim().to_string()).filter(|t| !t.is_empty());
    match r.first_page.as_ref() {
        Some(m) if line(m.header.as_ref()).is_none() && line(m.footer.as_ref()).as_deref() == Some("Title page footer") => {}
        m => bad.push(format!("first page {m:?}")),
    }
    match r.even_pages.as_ref() {
        Some(m) if line(m.header.as_ref()).as_deref() == Some("Even header") && line(m.footer.as_ref()).as_deref() == Some("Even page {page}") => {}
        m => bad.push(format!("even pages {m:?}")),
    }
    // Which page shows which.
    let shown = |i: usize| r.margins_of(i).1.map(|f| f.text.trim().to_string()).unwrap_or_default();
    if [shown(0), shown(1), shown(2)] != ["Title page footer", "Even page {page}", "Page {page} of {pages}"] {
        bad.push(format!("footers by page {:?}", [shown(0), shown(1), shown(2)]));
    }
    bad
}

#[test]
fn margins_through_docx_odt_and_native() {
    use officina_convert::{docx, odt};
    let d = margins_doc();
    let r = docx::read(&docx::write(&d, Vec::new()).unwrap()).unwrap().0;
    assert_eq!(check(&r), Vec::<String>::new(), "docx");
    let r = odt::read(&odt::write(&d, Vec::new()).unwrap()).unwrap().0;
    assert_eq!(check(&r), Vec::<String>::new(), "odt");
    // Across: what one format read, the other writes.
    let r = odt::read(&odt::write(&docx::read(&docx::write(&d, Vec::new()).unwrap()).unwrap().0, Vec::new()).unwrap()).unwrap().0;
    assert_eq!(check(&r), Vec::<String>::new(), "docx then odt");
    let b = officina_text::native::save(&d, &[], Vec::new()).unwrap();
    let r = officina_text::native::load(&b).unwrap().doc;
    assert_eq!(check(&r), Vec::<String>::new(), "native");
}
