//! Capitals, small capitals, hidden text, double strikethrough, letter
//! spacing and width, text effects and page breaks inside a paragraph
//! (docs/spec/officina.md §16.170), through .docx, .odt and the native file.

use officina_convert::{docx, odt};
use officina_text::model::{Caps, CharAttrs, TextDoc, EFFECT_EMBOSS, EFFECT_OUTLINE, EFFECT_SHADOW};

/// One run per look, each a word, then a page break and more text.
pub fn letters_doc() -> TextDoc {
    let looks = letter_looks();
    let mut d = TextDoc::new();
    d.blocks.remove(0);
    let mut text = String::new();
    let mut runs = Vec::new();
    for (i, a) in looks.iter().enumerate() {
        runs.push((text.len() as u32, d.attrs.intern(a.clone())));
        text.push_str(&format!("word{i} "));
    }
    runs.push((text.len() as u32, 0));
    text.push_str("before\u{000C}after the break");
    d.push_para(&text, 0, runs);
    d
}

pub fn letter_looks() -> Vec<CharAttrs> {
    vec![
        CharAttrs { caps: Some(Caps::All), ..Default::default() },
        CharAttrs { caps: Some(Caps::Small), ..Default::default() },
        CharAttrs { hidden: Some(true), ..Default::default() },
        CharAttrs { strike: Some(true), dstrike: Some(true), ..Default::default() },
        CharAttrs { spacing: Some(60), ..Default::default() },
        CharAttrs { spacing: Some(-20), ..Default::default() },
        CharAttrs { scale: Some(150), ..Default::default() },
        CharAttrs { effects: Some(EFFECT_OUTLINE), ..Default::default() },
        CharAttrs { effects: Some(EFFECT_SHADOW | EFFECT_EMBOSS), ..Default::default() },
    ]
}

/// Each word's look, as read back.
fn looks_of(d: &TextDoc) -> Vec<CharAttrs> {
    let mut out = Vec::new();
    for b in 0..d.blocks.len() {
        let p = d.para(b).unwrap();
        for (s, e, a) in p.spans() {
            if p.text[s as usize..e as usize].starts_with("word") {
                let a = d.attrs.get(a);
                out.push(CharAttrs { caps: a.caps, hidden: a.hidden, dstrike: a.dstrike, strike: a.strike.filter(|_| a.dstrike == Some(true)), spacing: a.spacing, scale: a.scale, effects: a.effects.filter(|&x| x != 0), ..Default::default() });
            }
        }
    }
    out
}

#[test]
fn letters_round_trip_docx() {
    let d = letters_doc();
    let r = docx::read(&docx::write(&d, Vec::new()).unwrap()).unwrap().0;
    assert_eq!(looks_of(&r), letter_looks());
    assert!(r.para(0).unwrap().text.contains('\u{000C}'), "the page break stays inside the paragraph");
}

#[test]
fn letters_round_trip_odt() {
    let d = letters_doc();
    let bytes = odt::write(&d, Vec::new()).unwrap();
    assert!(std::str::from_utf8(&bytes).is_err() || !String::from_utf8_lossy(&bytes).contains('\u{000C}'));
    let r = odt::read(&bytes).unwrap().0;
    assert_eq!(looks_of(&r), letter_looks());
    // ODF has no page break inside a paragraph: the rest starts a page.
    assert_eq!(r.blocks.len(), 2);
    assert_eq!(r.para(1).unwrap().text, "after the break");
    assert_eq!(r.para(1).unwrap().props.page_break_before, Some(true));
}

#[test]
fn letters_round_trip_native() {
    let d = letters_doc();
    let bytes = officina_text::native::save_to_vec(&d, &[]);
    let r = officina_text::native::load(&bytes).unwrap().doc;
    assert_eq!(looks_of(&r), letter_looks());
}
