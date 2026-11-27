//! What the Write sweep (docs/spec/officina.md §16.169) found our writers
//! and readers losing between them, each kept.

use officina_convert::{docx, odt};
use officina_text::model::{Leader, CellRef, CharAttrs, ListKind, ListRef, Note, NoteKind, PageSetup, Section, SectionStart, TabAlign, TabStop, TextDoc, OBJECT_CHAR};

fn via(fmt: &str, d: &TextDoc) -> TextDoc {
    if fmt == "docx" { docx::read(&docx::write(d, Vec::new()).unwrap()).unwrap().0 } else { odt::read(&odt::write(d, Vec::new()).unwrap()).unwrap().0 }
}

fn sections(d: &TextDoc) -> Vec<(String, SectionStart, Option<PageSetup>)> {
    d.blocks.iter().filter_map(|b| b.as_para()).filter_map(|p| p.props.section.map(|s| (p.text.clone(), s.start, s.page))).collect()
}

/// A 2×2 table, then two paragraphs.
fn table_first() -> TextDoc {
    let mut d = TextDoc::new();
    d.blocks.remove(0);
    for r in 0..2u32 {
        for c in 0..2u16 {
            let k = d.push_para(&format!("cell {r}{c}"), 0, vec![]);
            d.blocks.update(k, |b| b.as_para_mut().unwrap().props.cell = Some(CellRef { table: 1, row: r, col: c, width: 2400 }));
        }
    }
    d.push_para("After the table.", 0, vec![]);
    d.push_para("More.", 0, vec![]);
    d
}

#[test]
fn a_document_that_starts_with_a_table_gains_no_section() {
    let d = table_first();
    for fmt in ["docx", "odt"] {
        let r = via(fmt, &d);
        assert_eq!(sections(&r), vec![], "{fmt}");
        let r2 = via(fmt, &r);
        assert_eq!(sections(&r2), vec![], "{fmt} twice");
    }
}

#[test]
fn a_new_page_section_on_the_same_page_survives_odt() {
    let mut d = TextDoc::new();
    d.blocks.remove(0);
    for t in ["One.", "Two.", "Three."] {
        d.push_para(t, 0, vec![]);
    }
    d.blocks.update(2, |b| b.as_para_mut().unwrap().props.section = Some(Section { start: SectionStart::NextPage, ..Section::default() }));
    for fmt in ["docx", "odt"] {
        assert_eq!(sections(&via(fmt, &d)), vec![("Three.".to_string(), SectionStart::NextPage, None)], "{fmt}");
    }
}

#[test]
fn a_first_section_with_its_own_page_keeps_the_document_page_in_odt() {
    let mut d = TextDoc::new();
    d.blocks.remove(0);
    for t in ["First page", "", "Second page"] {
        d.push_para(t, 0, vec![]);
    }
    let own = PageSetup { margin_top: 1080, ..d.page };
    d.blocks.update(0, |b| b.as_para_mut().unwrap().props.section = Some(Section { page: Some(own), ..Section::default() }));
    d.blocks.update(2, |b| b.as_para_mut().unwrap().props.section = Some(Section { start: SectionStart::NextPage, ..Section::default() }));
    let r = via("odt", &d);
    assert_eq!(r.page, d.page);
    assert_eq!(sections(&r), sections(&d));
}

#[test]
fn a_list_in_a_table_cell_survives_odt() {
    let mut d = table_first();
    for k in [1, 3] {
        d.blocks.update(k, |b| b.as_para_mut().unwrap().props.list = Some(Some(ListRef { id: 7, kind: ListKind::Bullet, level: 0 })));
    }
    let r = via("odt", &d);
    let items: Vec<String> = r.blocks.iter().filter_map(|b| b.as_para()).filter(|p| matches!(p.props.list, Some(Some(_)))).map(|p| p.text.clone()).collect();
    assert_eq!(items, ["cell 01", "cell 11"]);
    assert_eq!(r.blocks.iter().filter_map(|b| b.as_para()).filter(|p| p.props.cell.is_some()).count(), 4);
}

#[test]
fn a_styles_tab_stops_stay_in_the_style_through_odt() {
    let mut d = TextDoc::new();
    d.styles.styles[0].para.tabs = Some(vec![TabStop { pos: 2880, align: TabAlign::Start, leader: Leader::None }]);
    d.blocks.remove(0);
    d.push_para("a\tb", 0, vec![]);
    let r = via("odt", &d);
    let p = r.para(0).unwrap();
    assert_eq!(p.props.tabs, None);
    assert_eq!(r.styles.para_format(p.style, &p.props).0.tabs.iter().map(|t| t.pos).collect::<Vec<_>>(), [2880]);
}

#[test]
fn a_note_mark_keeps_its_runs_look_through_docx() {
    let mut d = TextDoc::new();
    d.blocks.remove(0);
    let id = d.notes.add(Note { kind: NoteKind::Foot, paras: vec![] }).unwrap();
    let plain = d.attrs.intern(CharAttrs { size: Some(28), family: Some("Liberation Serif".into()), ..Default::default() });
    let mark = d.attrs.intern(CharAttrs { size: Some(28), family: Some("Liberation Serif".into()), note: Some(id), ..Default::default() });
    let text = format!("Text{OBJECT_CHAR}");
    d.push_para(&text, 0, vec![(0, plain), (4, mark)]);
    let r = via("docx", &d);
    let p = r.para(0).unwrap();
    let a = r.attrs.get(p.attr_at(4));
    assert!(a.note.is_some());
    assert_eq!((a.size, a.family.as_deref()), (Some(28), Some("Liberation Serif")));
}
