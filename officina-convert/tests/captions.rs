//! Caption sequences, generated indexes and their page targets across file formats.

use officina_convert::{docx, odt, zip::Archive};
use officina_core::history::Op;
use officina_text::fields;
use officina_text::layout::{FixedFonts, LayoutCache};
use officina_text::model::{CaptionKind, CharAttrs, Field, TextDoc, OBJECT_CHAR};
use officina_text::{native, paginate::Paginator};

use CaptionKind::{Equation, Figure, Table};

fn caption(doc: &mut TextDoc, kind: CaptionKind, title: &str) {
    let prefix = format!("{} ", kind.name());
    let number = doc.attrs.intern(CharAttrs { sequence: Some(kind), ..Default::default() });
    let offset = prefix.len() as u32;
    doc.push_para(&format!("{prefix}{OBJECT_CHAR}: {title}"), 0, vec![(0, 0), (offset, number), (offset + OBJECT_CHAR.len_utf8() as u32, 0)]);
}

fn captions(doc: &TextDoc) -> Vec<(CaptionKind, String, String)> {
    let mut result = Vec::new();
    for p in doc.blocks.iter().filter_map(|b| b.as_para()) {
        for (at, ch) in p.text.char_indices() {
            if let Some(kind) = doc.attrs.get(p.attr_at(at as u32)).sequence.filter(|_| ch == OBJECT_CHAR) {
                result.push((kind, doc.caption_number(p.id, at as u32), doc.caption_text(p)));
            }
        }
    }
    result
}

fn update(doc: &mut TextDoc, kind: CaptionKind) {
    for op in fields::figure_list_ops(doc, 0, kind) {
        op.apply(doc);
    }
}

fn verify_entries(doc: &mut TextDoc, kind: CaptionKind, titles: &[&str]) {
    let mut pages = Paginator::new(doc);
    assert!(pages.run(doc, &mut LayoutCache::default(), &FixedFonts, usize::MAX));
    fields::resolve(doc, pages.pages());
    let (first, end) = fields::index_range(doc, None, Some(kind)).unwrap();
    assert_eq!(end - first, titles.len());
    for (i, &title) in (first..end).zip(titles) {
        let p = doc.para(i).unwrap();
        assert_eq!(p.props.toc, None);
        assert_eq!(p.text, format!("{title}\t{OBJECT_CHAR}"));
        let [(at, f)] = doc.fields_in(p)[..] else { panic!("missing or ambiguous target: {title}") };
        assert_eq!(at as usize, title.len() + 1);
        assert!(matches!(f, Field::PageRef(_)));
        let target = doc.index_of(f.target()).unwrap();
        assert_eq!(doc.caption_text(doc.para(target).unwrap()), title);
        assert_eq!(doc.field_text(f), (pages.page_of(target, 0) + 1).to_string());
    }
    assert!(fields::figure_list_ops(doc, first, kind).is_empty());
}

#[test]
fn native_and_foreign_caption_lists_keep_numbers_and_page_targets() {
    let mut doc = TextDoc::new();
    caption(&mut doc, Figure, "North");
    caption(&mut doc, Table, "Costs");
    doc.push_para("\u{c}Next page", 0, vec![]);
    caption(&mut doc, Equation, "Balance");
    caption(&mut doc, Figure, "South");
    for kind in CaptionKind::ALL { update(&mut doc, kind); }
    let figures = ["Figure 1: North", "Figure 2: South"];
    let check = |d: &mut TextDoc| {
        verify_entries(d, Figure, &figures);
        verify_entries(d, Table, &["Table 1: Costs"]);
        verify_entries(d, Equation, &["Equation 1: Balance"]);
        let (a, b) = fields::index_range(d, None, Some(Figure)).unwrap();
        let page = |i| d.field_text(d.fields_in(d.para(i).unwrap())[0].1);
        assert_ne!(page(a), page(b - 1));
        assert_eq!(captions(d).into_iter().map(|(k, n, _)| (k, n)).collect::<Vec<_>>(),
                   [(Figure, "1".into()), (Table, "1".into()), (Equation, "1".into()), (Figure, "2".into())]);
    };
    check(&mut doc);
    check(&mut native::load(&native::save_to_vec(&doc, &[])).unwrap().doc);
    let word = docx::write(&doc, Vec::new()).unwrap();
    let odf = odt::write(&doc, Vec::new()).unwrap();
    let xml = |bytes: &[u8], name| String::from_utf8(Archive::open(bytes).unwrap().read_name(name).unwrap().unwrap()).unwrap();
    let word_xml = xml(&word, "word/document.xml");
    let odf_xml = xml(&odf, "content.xml");
    for kind in CaptionKind::ALL {
        assert!(word_xml.contains(&format!(r"SEQ {} \* ARABIC", kind.name())));
        assert!(word_xml.contains(&format!(r#"TOC \c "{}""#, kind.name())));
        assert!(odf_xml.contains(&format!(r#"text:formula="ooow:{}+1""#, kind.name())));
        assert!(odf_xml.contains(&format!(r#"text:caption-sequence-name="{}""#, kind.name())));
    }
    check(&mut docx::read(&word).unwrap().0);
    check(&mut odt::read(&odf).unwrap().0);
}

/// OFFICINA_OOXMLEXPORT_DATA points at LibreOffice sw/qa/extras/ooxmlexport/data.
#[test]
#[ignore = "requires the external LibreOffice corpus"]
fn libreoffice_sequences_and_stale_illustration_index() {
    let data = std::path::PathBuf::from(std::env::var_os("OFFICINA_OOXMLEXPORT_DATA").unwrap());
    for (file, expected) in [
        ("EquationWithAboveAndBelowCaption.docx", vec![(Equation, "1"), (Equation, "2")]),
        ("TableWithAboveCaptions.docx", vec![(Table, "1")]),
        ("tdf149649.docx", vec![(Figure, "1"), (Table, "1")]),
    ] {
        let bytes = std::fs::read(data.join(file)).unwrap();
        let (doc, _) = docx::read(&bytes).unwrap();
        let got = captions(&doc);
        assert_eq!(got.iter().map(|(k, n, _)| (*k, n.as_str())).collect::<Vec<_>>(), expected, "{file}");
        assert_eq!(captions(&odt::read(&odt::write(&doc, Vec::new()).unwrap()).unwrap().0), got, "{file}");
    }
    let bytes = std::fs::read(data.join("Table-of-Figures.odt")).unwrap();
    let (mut doc, _) = odt::read(&bytes).unwrap();
    let titles = ["Abbildung 1: Das ist ein Schiff-modified", "Abbildung 2: Das ist ein Haus-modified"];
    let (a, b) = fields::index_range(&doc, None, Some(Figure)).unwrap();
    assert_eq!(b - a, 2);
    for (i, title) in (a..b).zip(titles) {
        let entry = doc.para(i).unwrap();
        assert!(!entry.text.contains("-modified"));
        let target = doc.fields_in(entry)[0].1.target();
        assert_eq!(doc.caption_text(doc.para(doc.index_of(target).unwrap()).unwrap()), title);
    }
    update(&mut doc, Figure);
    verify_entries(&mut doc, Figure, &titles);
    verify_entries(&mut docx::read(&docx::write(&doc, Vec::new()).unwrap()).unwrap().0, Figure, &titles);
}

#[test]
fn odt_index_binds_a_forward_sequence_page_ref_with_an_arbitrary_name() {
    let xml = br#"<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" xmlns:ooow="http://openoffice.org/2004/writer">
        <office:body><office:text>
        <text:table-index text:name="Tables"><text:table-index-source text:caption-sequence-name="Table"/>
        <text:index-body><text:p>Old title<text:tab/><text:sequence-ref text:reference-format="page" text:ref-name="myTable">4</text:sequence-ref></text:p></text:index-body></text:table-index>
        <text:p>Table <text:sequence text:name="Table" text:ref-name="myTable" text:formula="ooow:Table+1">1</text:sequence>: Costs</text:p>
        </office:text></office:body></office:document-content>"#;
    let mut zip = officina_convert::zip::ZipWriter::new(Vec::new());
    zip.add("content.xml", xml).unwrap();
    let (mut d, _) = odt::read(&zip.finish().unwrap()).unwrap();
    let p = d.para(0).unwrap();
    assert_eq!(p.props.figure_list, Some(Table));
    let f = d.fields_in(p)[0].1;
    assert_eq!(f, Field::PageRef(d.para(1).unwrap().id));
    assert_eq!(d.field_text(f), "4");
    update(&mut d, Table);
    verify_entries(&mut d, Table, &["Table 1: Costs"]);
}
