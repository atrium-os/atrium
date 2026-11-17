//! A PDF of right-to-left and mixed paragraphs (a look at bidi layout).
//!
//! cargo run --example bidi_sample OUT.pdf

use officina_text::layout::PinnedFonts;
use officina_text::model::{CharAttrs, TextDoc};

fn main() {
    let out = std::env::args().nth(1).expect("OUT.pdf");
    let mut doc = TextDoc::new();
    let big = doc.attrs.intern(CharAttrs { size: Some(28), ..Default::default() });
    let style = doc.para(0).unwrap().style;
    for t in [
        "Left to right with a Hebrew word: שלום עולם, then English again.",
        "שלום עולם! זהו משפט בעברית עם מילה באנגלית (Atrium) בתוכו.",
        "مرحبا بالعالم، هذه جملة عربية فيها كلمة Atrium ورقم 2026.",
        "हिन्दी और English एक ही पंक्ति में। தமிழ் உரை.",
    ] {
        doc.push_para(t, style, vec![(0, big)]);
    }
    let fonts = PinnedFonts::new().expect("fonts");
    let f = std::io::BufWriter::new(std::fs::File::create(out).expect("create"));
    let (_, s) = officina_pdf::export(&doc, &fonts, &fonts, &officina_pdf::Info { title: "Bidi".into() }, f, &mut |_, _| {}).expect("export");
    println!("{s:?}");
}
