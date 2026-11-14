//! Writes a sample document (headings, bold, italic, underline, a link,
//! alignment, a long body) for looking at atrium-write: `scratch/sample.owrite`.
//!
//! cargo run --example sample_doc [OUT]

use officina_text::model::{Align, CharAttrs, ListKind, ListRef, ParaProps, TextDoc};
use officina_text::native;

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "../scratch/sample.owrite".into());
    let mut d = TextDoc::new();
    d.blocks.remove(0);
    let st = |d: &TextDoc, n: &str| d.styles.by_name(n).unwrap();
    let bold = d.attrs.intern(CharAttrs { bold: Some(true), ..Default::default() });
    let ital = d.attrs.intern(CharAttrs { italic: Some(true), ..Default::default() });
    let under = d.attrs.intern(CharAttrs { underline: Some(true), ..Default::default() });
    let link = d.attrs.intern(CharAttrs { link: Some("https://example.org".into()), color: Some(0x1f5fbf), ..Default::default() });
    let red = d.attrs.intern(CharAttrs { color: Some(0xb3261e), bold: Some(true), ..Default::default() });
    let mark = d.attrs.intern(CharAttrs { highlight: Some(0xfff3a0), ..Default::default() });

    let t = st(&d, "Title");
    d.push_para("Officina Write", t, vec![]);
    let i = d.push_para("A first look at the word processor: pages, styles and the text engine.", 0, vec![(0, ital)]);
    d.blocks.update(i, |b| b.as_para_mut().unwrap().props = ParaProps { space_after: Some(360), ..Default::default() });
    d.push_para("What it does today", st(&d, "Heading 1"), vec![]);
    let s = "Text is shaped with the pinned fonts and broken into lines by the Unicode rules, then laid out on pages with widow and orphan control. Bold, italic, underline, links, colour and highlighting are character attributes; headings are paragraph styles that keep with the next paragraph.";
    let b0 = s.find("Bold").unwrap() as u32;
    let i0 = s.find("italic").unwrap() as u32;
    let u0 = s.find("underline").unwrap() as u32;
    let l0 = s.find("links").unwrap() as u32;
    let c0 = s.find("colour").unwrap() as u32;
    let h0 = s.find("highlighting").unwrap() as u32;
    d.push_para(s, 0, vec![(0, 0), (b0, bold), (b0 + 4, 0), (i0, ital), (i0 + 6, 0), (u0, under), (u0 + 9, 0), (l0, link), (l0 + 5, 0), (c0, red), (c0 + 6, 0), (h0, mark), (h0 + 12, 0)]);
    let j = "Justified text stretches the spaces of every line but the last, so both edges of the column are straight. It is the usual setting for books and long reports, and the paginator treats it like any other paragraph: what changes is only where the glyphs sit on each line.";
    let k = d.push_para(j, 0, vec![]);
    d.blocks.update(k, |b| b.as_para_mut().unwrap().props.align = Some(Align::Justify));
    let k = d.push_para("A centred line.", 0, vec![]);
    d.blocks.update(k, |b| b.as_para_mut().unwrap().props.align = Some(Align::Center));
    let k = d.push_para("And one set to the right.", 0, vec![]);
    d.blocks.update(k, |b| b.as_para_mut().unwrap().props.align = Some(Align::End));
    let item = |d: &mut TextDoc, t: &str, id, kind, level| {
        let i = d.push_para(t, 0, vec![]);
        d.blocks.update(i, |b| b.as_para_mut().unwrap().props = ParaProps { list: Some(Some(ListRef { id, kind, level })), space_after: Some(60), ..Default::default() });
    };
    item(&mut d, "Lists number themselves from counts:", 1, ListKind::Numbered, 0);
    item(&mut d, "items at a deeper level letter themselves,", 1, ListKind::Numbered, 1);
    item(&mut d, "and number again below that;", 1, ListKind::Numbered, 1);
    item(&mut d, "bullets mark the levels by shape.", 1, ListKind::Numbered, 0);
    item(&mut d, "Tab and Shift+Tab move an item in and out.", 2, ListKind::Bullet, 0);
    item(&mut d, "Enter on an empty item ends the list.", 2, ListKind::Bullet, 1);
    d.push_para("Large documents", st(&d, "Heading 1"), vec![]);
    d.push_para("Pagination keeps a checkpoint at each page start. An edit repaginates from its page and stops when a page starts where it did before, so typing on page 750 of 1,500 relays one page. Only the pages near the view are sent to the compositor.", 0, vec![]);
    for n in 1..=24 {
        if n % 6 == 1 {
            d.push_para(&format!("Section {}", n / 6 + 1), st(&d, "Heading 2"), vec![]);
        }
        d.push_para(&format!("Paragraph {n}. The quick brown fox jumps over the lazy dog, and a naïve café owner counts façades while the paginator keeps up with every keystroke. Shaping handles ligatures such as fi and fl, accents and marks, and characters from the fallback face like ★ and ✓."), 0, vec![]);
    }
    d.footer = Some(officina_text::model::HeaderFooter { text: "Page {page} of {pages}".into(), align: officina_text::model::Align::Center });
    std::fs::write(&out, native::save_to_vec(&d, &[])).unwrap();
    println!("{out}: {} blocks", d.blocks.len());
}
