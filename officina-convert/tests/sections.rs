//! Sections and columns through .docx and .odt.

use officina_convert::{docx, odt};
use officina_text::model::{Section, SectionStart, TextDoc};

/// Two columns from the start, three with a line after a continuous break,
/// one again on a new page, a table before a last continuous two-column run.
pub fn sections_doc() -> TextDoc {
    let mut d = TextDoc::new();
    d.blocks.remove(0);
    for i in 0..14 {
        d.push_para(&format!("Paragraph {i} with enough words in it to wrap across a narrow column more than once."), 0, vec![]);
    }
    let mark = |d: &mut TextDoc, k: usize, s: Section| {
        d.blocks.update(k, |b| b.as_para_mut().unwrap().props.section = Some(s));
    };
    mark(&mut d, 0, Section { columns: 2, ..Section::default() });
    mark(&mut d, 4, Section { start: SectionStart::Continuous, columns: 3, gap: 360, rule: true });
    mark(&mut d, 8, Section { start: SectionStart::NextPage, columns: 1, ..Section::default() });
    mark(&mut d, 12, Section { start: SectionStart::Continuous, columns: 2, gap: 1080, rule: false });
    d
}

/// Each section's first paragraph and its settings (the first section's
/// start doesn't matter).
pub fn shape(d: &TextDoc) -> Vec<(String, Option<SectionStart>, u8, i32, bool)> {
    d.blocks.iter().enumerate().filter_map(|(k, b)| {
        let p = b.as_para()?;
        let s = p.props.section.filter(|_| p.props.cell.is_none())?;
        Some((p.text.clone(), (k > 0).then_some(s.start), s.columns, s.gap, s.rule))
    }).collect()
}

#[test]
fn sections_round_trip() {
    let d = sections_doc();
    let want = shape(&d);
    assert_eq!(want.len(), 4);
    for fmt in ["docx", "odt"] {
        let r = if fmt == "docx" { docx::read(&docx::write(&d, Vec::new()).unwrap()).unwrap().0 } else { odt::read(&odt::write(&d, Vec::new()).unwrap()).unwrap().0 };
        assert_eq!(shape(&r), want, "{fmt}");
        assert!(r.blocks.iter().filter_map(|b| b.as_para()).all(|p| p.props.page_break_before.is_none()), "{fmt}: no stray page breaks");
        assert_eq!(r.blocks.len(), d.blocks.len(), "{fmt}");
    }
}
