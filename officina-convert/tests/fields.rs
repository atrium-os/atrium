//! Tables of contents, cross-references and tab stops through .docx and .odt.

use officina_convert::{docx, odt};
use officina_core::history::Op;
use officina_text::fields;
use officina_text::model::{Field, Leader, Para, TabAlign, TabStop, TextDoc, OBJECT_CHAR};
use officina_text::ops::TextOp;

/// Headings with body text, a TOC at the top, a page and a text reference to
/// chapter 2, and a paragraph with its own tab stops.
pub fn toc_doc() -> TextDoc {
    let mut d = TextDoc::new();
    d.blocks.remove(0);
    let h1 = d.styles.by_name("Heading 1").unwrap();
    let h2 = d.styles.by_name("Heading 2").unwrap();
    d.push_para("Report", 0, vec![]);
    for c in 1..=3 {
        d.push_para(&format!("Chapter {c}"), h1, vec![]);
        d.push_para(&format!("Section {c}.1"), h2, vec![]);
        d.push_para("Body text.", 0, vec![]);
    }
    let ops = fields::toc_ops(&mut d, 1, 3);
    for op in ops {
        op.apply(&mut d);
    }
    let target = d.blocks.iter().filter_map(|b| b.as_para()).find(|p| p.text == "Chapter 2" && p.props.toc.is_none()).unwrap().id;
    let mut a = d.attrs.get(0).clone();
    a.field = Some(Field::PageRef(target));
    let pa = d.attrs.intern(a.clone());
    a.field = Some(Field::TextRef(target));
    let ta = d.attrs.intern(a);
    let text = format!("See {OBJECT_CHAR} on page {OBJECT_CHAR}.");
    let mut p = Para::new(d.new_id(), &text, 0);
    p.runs = vec![(0, 0), (4, ta), (7, 0), (16, pa), (19, 0)];
    p.props.tabs = Some(vec![TabStop { pos: 2880, align: TabAlign::Center, leader: Leader::None }, TabStop { pos: 7200, align: TabAlign::Decimal, leader: Leader::Dashes }]);
    p.props.indent_start = Some(720);
    let n = d.blocks.len() as u32;
    TextOp::Insert { at: n, blocks: vec![officina_text::model::Block::Para(p)] }.apply(&mut d);
    d.field_values.insert(Field::PageRef(target), "2".into());
    d
}

/// What a reader must get back: TOC entries (level, text, target heading's
/// text) and the references' targets and values, the tab stops.
pub fn shape(d: &TextDoc) -> (Vec<(u8, String, String)>, Vec<(String, String)>, Option<Vec<TabStop>>) {
    let name = |id| d.index_of(id).and_then(|i| d.para(i)).map_or("?".to_string(), |p| p.text.clone());
    let mut toc = Vec::new();
    let mut refs = Vec::new();
    let mut tabs = None;
    for b in d.blocks.iter() {
        let p = b.as_para().unwrap();
        let f = d.fields_in(p);
        if let Some(l) = p.props.toc {
            let target = f.first().map_or("none".to_string(), |x| name(x.1.target()));
            toc.push((l, p.text.split('\t').next().unwrap().to_string(), target));
        } else {
            refs.extend(f.iter().map(|x| (format!("{:?}", std::mem::discriminant(&x.1)).len().to_string() + &name(x.1.target()), d.field_text(x.1))));
        }
        if p.text.starts_with("See ") {
            tabs = p.props.tabs.clone();
        }
    }
    (toc, refs, tabs)
}

#[test]
fn toc_and_references_round_trip() {
    let d = toc_doc();
    let want = shape(&d);
    assert_eq!(want.0.len(), 6);
    assert_eq!(want.1.len(), 2);
    for (fmt, bytes) in [("docx", docx::write(&d, Vec::new()).unwrap().to_vec()), ("odt", odt::write(&d, Vec::new()).unwrap().to_vec())] {
        let r = if fmt == "docx" { docx::read(&bytes).unwrap().0 } else { odt::read(&bytes).unwrap().0 };
        let got = shape(&r);
        assert_eq!(got.0, want.0, "{fmt} contents");
        assert_eq!(got.1, want.1, "{fmt} references");
        assert_eq!(got.2, want.2, "{fmt} tab stops");
        // Update finds it current.
        let mut r = r;
        assert!(fields::toc_ops(&mut r, 0, 3).is_empty(), "{fmt}: contents read back are current");
    }
}
