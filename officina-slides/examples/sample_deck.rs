//! Writes a small sample deck: `cargo run --example sample_deck OUT.oslides`.
//! A title slide, a bulleted slide with shapes, and a section header.

use officina_slides::model::*;
use officina_slides::native;
use officina_slides::ops::new_slide;

fn fill_ph(s: &mut Slide, kind_of: &[PhKind], text: &str) {
    if let Some(sh) = s.shapes.iter_mut().find(|sh| sh.ph.as_ref().is_some_and(|p| kind_of.contains(&p.kind))) {
        if let Some(t) = sh.text_mut() {
            set_plain(&mut t.doc, text);
        }
    }
}

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "sample.oslides".into());
    let mut deck = Deck::new();
    let lay = |deck: &Deck, k: &str| deck.layout_of_kind(k).expect("layout").id;

    let l = lay(&deck, "title");
    let mut s = new_slide(&mut deck, l);
    fill_ph(&mut s, &[PhKind::CtrTitle, PhKind::Title], "Atrium Slides");
    fill_ph(&mut s, &[PhKind::SubTitle], "A presentation app for Atrium");
    deck.push_slide(s);

    let l = lay(&deck, "obj");
    let mut s = new_slide(&mut deck, l);
    fill_ph(&mut s, &[PhKind::Title], "What it does");
    fill_ph(&mut s, &[PhKind::Body, PhKind::Obj], "Slides inherit from layouts and masters\n\tTheme colours and fonts\n\tPlaceholders filled in place\nShapes, pictures and text frames\nAutosave with crash recovery");
    let id = deck.new_id();
    let mut star = Shape::new(id, Rect::new(8_600_000, 4_300_000, 1_800_000, 1_700_000), ShapeKind::Geom { geom: Some(Geom::preset("star5")), fill: Some(Fill::Solid(Color::theme(Slot::Accent2))), line: Some(Stroke::none()), text: None });
    star.name = "Star".into();
    s.shapes.push(star);
    let id = deck.new_id();
    let arrow = Shape::new(id, Rect::new(6_400_000, 4_800_000, 1_900_000, 700_000), ShapeKind::Geom { geom: Some(Geom::preset("rightArrow")), fill: Some(Fill::Solid(Color::theme(Slot::Accent1))), line: Some(Stroke::none()), text: None });
    s.shapes.push(arrow);
    deck.push_slide(s);

    let l = lay(&deck, "secHead");
    let mut s = new_slide(&mut deck, l);
    fill_ph(&mut s, &[PhKind::Title], "Next: presenting");
    fill_ph(&mut s, &[PhKind::Body], "Transitions, builds and the presenter view");
    deck.push_slide(s);

    std::fs::write(&out, native::save_to_vec(&deck)).expect("write");
    eprintln!("wrote {out}");
}
