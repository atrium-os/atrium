//! Writes a deck that exercises presenting (officina.md §16.33 O6d): every
//! transition kind and build effect, notes and a hidden slide. Used by
//! scripts/slides-present-vm.sh.
//!
//!   cargo run --example show_deck -- out.oslides

use atrium_slides::doc::Doc;
use officina_slides::model::*;
use officina_slides::ops::new_slide;

fn main() {
    let out = std::env::args().nth(1).unwrap_or_else(|| "show.oslides".into());
    let mut deck = Deck::new();
    let (w, h) = deck.size;
    let title = deck.layout_of_kind("title").or_else(|| deck.layout_of_kind("obj")).map(|l| l.id).expect("a layout");
    let obj = deck.layout_of_kind("obj").map(|l| l.id).unwrap_or(title);
    let tr = |kind, ms| Some(Transition { kind, ms, on_click: true, after_ms: None });
    let colours = [0x2F6FDB, 0xE0533D, 0x2DA44E, 0xBF8700];
    let slides: Vec<(&str, Option<Transition>, Vec<Effect>, bool)> = vec![
        ("Presenting in Officina", None, vec![], false),
        ("Push", tr(TransitionKind::Push(Dir::Left), 700), vec![Effect::FlyIn(Dir::Left), Effect::Fade, Effect::Zoom], false),
        ("Fade", tr(TransitionKind::Fade, 600), vec![Effect::Wipe(Dir::Left), Effect::Appear], false),
        ("Hidden", tr(TransitionKind::Cut, 0), vec![], true),
        ("Wipe", tr(TransitionKind::Wipe(Dir::Right), 600), vec![Effect::FlyIn(Dir::Down), Effect::FadeOut], false),
        ("Cover", tr(TransitionKind::Cover(Dir::Up), 600), vec![], false),
        ("Uncover", tr(TransitionKind::Uncover(Dir::Left), 600), vec![], false),
        ("Split", tr(TransitionKind::Split { vertical: true, out: true }, 600), vec![], false),
        ("Zoom", tr(TransitionKind::Zoom { out: false }, 600), vec![], false),
    ];
    for (i, (name, transition, effects, hidden)) in slides.into_iter().enumerate() {
        let mut s = new_slide(&mut deck, if i == 0 { title } else { obj });
        if let Some(t) = s.shapes.iter_mut().find(|x| x.ph.is_some()).and_then(|x| x.text_mut()) {
            *t = TextBody::plain(name);
        }
        // Body placeholders stay empty (they don't draw in a show).
        s.shapes.retain(|x| x.ph.is_none() || x.text().is_some_and(|t| !t.text().is_empty()));
        let side = h / 5;
        for (k, e) in effects.iter().enumerate() {
            let id = deck.new_id();
            let x = w / 8 + k as i64 * (side + w / 16);
            let mut sh = Shape::rect(id, Rect::new(x, h / 2, side, side), Fill::Solid(Color::rgb(colours[k % colours.len()])));
            sh.name = format!("Box {id}").into();
            s.shapes.push(sh);
            let trigger = if k == 0 { Trigger::OnClick } else { Trigger::AfterPrevious };
            s.builds.push(Build { shape: id, effect: *e, trigger, ms: 500, delay_ms: 0, by_paragraph: false });
        }
        s.transition = transition;
        s.hidden = hidden;
        s.notes = Some(TextBody::plain(&format!("Notes for “{name}”.\nSay something about it.")));
        deck.push_slide(s);
    }
    let doc = Doc::from_deck(deck, "show.oslides");
    let f = std::fs::File::create(&out).expect("create");
    doc.write_native(f).expect("write");
    eprintln!("wrote {out}");
}
