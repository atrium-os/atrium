//! Paragraph borders and shading, dropped capitals, lists' own levels and
//! restarts, line numbers and hyphenation (docs/spec/officina.md §16.171),
//! through .docx, .odt and the native file.

use officina_convert::{docx, odt};
use officina_text::lists::Numbering;
use officina_text::model::{Border, BorderStyle, DropCap, LineNumbering, LineRestart, ListDef, ListKind, ListLevel, ListRef, NumFormat, ParaBorders, ParaProps, Settings, TextDoc};

pub fn paragraphs_doc() -> TextDoc {
    let mut d = TextDoc::new();
    d.blocks.remove(0);
    let boxed = ParaBorders { top: Some(Border { style: BorderStyle::Double, width: 12, color: Some(0x1F4E79), space: 4 }), bottom: Some(Border::default()), start: Some(Border { style: BorderStyle::Dashed, ..Border::default() }), end: None, between: None };
    let item = |level, start| ParaProps { list: Some(Some(ListRef { id: 3, kind: ListKind::Numbered, level })), list_start: start, ..Default::default() };
    let paras: Vec<(&str, ParaProps)> = vec![
        ("Once upon a time there was a paragraph with a dropped capital.", ParaProps { drop_cap: Some(Some(DropCap { lines: 3, distance: 72, margin: false })), ..Default::default() }),
        ("Boxed and shaded.", ParaProps { borders: Some(boxed), shading: Some(Some(0xDDEEFF)), ..Default::default() }),
        ("Not numbered, not hyphenated.", ParaProps { line_numbers: Some(false), hyphenate: Some(false), ..Default::default() }),
        ("First", item(0, None)),
        ("Nested", item(1, None)),
        ("Second", item(0, None)),
        ("Again from five", item(0, Some(5))),
        ("Six", item(0, None)),
    ];
    for (t, p) in paras {
        let k = d.push_para(t, 0, vec![]);
        d.blocks.update(k, |b| b.as_para_mut().unwrap().props = p.clone());
    }
    let mut def = ListDef::builtin(ListKind::Numbered);
    def.levels[0] = ListLevel { format: NumFormat::UpperRoman, text: "%1)".into(), start: 1, indent: 720, hang: 360 };
    def.levels[1] = ListLevel { format: NumFormat::LowerLetter, text: "%1.%2".into(), start: 1, indent: 1440, hang: 540 };
    d.styles.lists.insert(3, def);
    d.settings = Settings { hyphenate: true, line_numbers: Some(LineNumbering { start: 1, count_by: 5, distance: 360, restart: LineRestart::Continuous }) };
    d
}

/// What each paragraph shows: text, borders, shading, drop, the line
/// number and hyphenation flags, its list label and indents.
fn looks(d: &TextDoc) -> Vec<(String, Option<ParaBorders>, Option<u32>, Option<DropCap>, bool, bool, Option<String>, i32, i32)> {
    let n = Numbering::build(d);
    (0..d.blocks.len())
        .filter_map(|i| {
            let p = d.para(i)?;
            let (f, _) = d.styles.para_format(p.style, &p.props);
            let b = (!f.borders.is_empty()).then_some(f.borders);
            Some((p.text.clone(), b, f.shading, f.drop_cap, f.line_numbers, f.hyphenate, n.label(i), f.indent_start, f.first_line))
        })
        .collect()
}

#[test]
fn paragraphs_round_trip_docx() {
    let d = paragraphs_doc();
    let r = docx::read(&docx::write(&d, Vec::new()).unwrap()).unwrap().0;
    assert_eq!(looks(&r), looks(&d));
    assert_eq!(r.settings, d.settings);
}

#[test]
fn paragraphs_round_trip_odt() {
    let d = paragraphs_doc();
    let r = odt::read(&odt::write(&d, Vec::new()).unwrap()).unwrap().0;
    assert_eq!(looks(&r), looks(&d));
    assert_eq!(r.settings, d.settings);
}

#[test]
fn paragraphs_round_trip_native() {
    let d = paragraphs_doc();
    let r = officina_text::native::load(&officina_text::native::save_to_vec(&d, &[])).unwrap().doc;
    assert_eq!(looks(&r), looks(&d));
    assert_eq!(r.settings, d.settings);
}
