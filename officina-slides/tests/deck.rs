//! O6a gates (docs/spec/officina.md §16.33): inheritance, theme colours,
//! drawing, ops and their inverses, the native file read lazily, the journal.

use std::sync::Arc;

use officina_core::history::{History, Op};
use officina_slides::draw::{draw_slide, DrawItem, Notes};
use officina_slides::model::*;
use officina_slides::native::{self, decode_record, encode_record};
use officina_slides::ops::{new_slide, DeckOp, Target, NOTES};
use officina_slides::resolve::{color, resolve, Owner};
use officina_text::layout::FixedFonts;
use officina_text::ops::{Fragment, TextOp};
use officina_text::paint::Item;

fn deck_with(n: usize) -> Deck {
    let mut d = Deck::new();
    let title = d.layout_of_kind("title").unwrap().id;
    let obj = d.layout_of_kind("obj").unwrap().id;
    for i in 0..n {
        let mut s = new_slide(&mut d, if i == 0 { title } else { obj });
        for sh in s.shapes.iter_mut() {
            let t = match sh.ph.unwrap().kind {
                PhKind::CtrTitle | PhKind::Title => format!("Slide {}", i + 1),
                _ => format!("First point {i}\n\tA detail\nSecond point"),
            };
            set_plain(&mut sh.text_mut().unwrap().doc, &t);
        }
        d.push_slide(s);
    }
    d
}

fn bytes(d: &Deck) -> Vec<u8> {
    native::save_to_vec(d)
}

/// The deck's content (not its id watermark, which undo never takes back).
fn content(d: &mut Deck) -> (usize, Vec<Slide>, Vec<Master>, Vec<Layout>, Theme, (i64, i64)) {
    let slides = (0..d.slide_count()).map(|i| d.slide(i).unwrap().clone()).collect();
    (d.slide_count(), slides, d.masters.clone(), d.layouts.clone(), d.theme.clone(), d.size)
}

fn texts(items: &[DrawItem]) -> Vec<String> {
    let mut v = Vec::new();
    for it in items {
        if let DrawItem::Text { items, .. } = it {
            let s: String = items.iter().filter_map(|i| if let Item::Glyphs(g) = i { Some(g.text.as_str()) } else { None }).collect::<Vec<_>>().join("|");
            v.push(s);
        }
    }
    v
}

#[test]
fn placeholders_inherit_from_layout_and_master() {
    let mut d = deck_with(2);
    let lid = d.slide(1).unwrap().layout;
    let s = d.slide(1).unwrap().clone();
    let title = s.shapes.iter().find(|x| x.ph.unwrap().kind == PhKind::Title).unwrap();
    let body = s.shapes.iter().find(|x| x.ph.unwrap().kind == PhKind::Body).unwrap();
    let rt = resolve(&d, Owner::Slide(lid), title);
    let rb = resolve(&d, Owner::Slide(lid), body);
    // Frames from the layout; title text style from the master (44 pt, the
    // theme's heading font), body levels with bullets and indents.
    assert_eq!(rt.frame, Rect::new(838_200, 365_125, 10_515_600, 1_325_563));
    assert_eq!(rb.frame, Rect::new(838_200, 1_825_625, 10_515_600, 4_351_338));
    assert_eq!(rt.levels[0].chars.size, Some(88));
    assert_eq!(rt.levels[0].chars.family.as_deref(), Some("+mj-lt"));
    assert_eq!(rb.levels[0].chars.size, Some(56));
    assert_eq!(rb.levels[1].chars.size, Some(48));
    assert!(matches!(rb.levels[0].bullet, Some(Bullet::Char { ch: '•', .. })));
    assert_eq!(rb.levels[1].para.indent_start, Some(1080));
    // A frame of its own wins; the title slide's centred subtitle has no bullet.
    let mut moved = body.clone();
    moved.frame = Some(Rect::new(1, 2, 3, 4));
    assert_eq!(resolve(&d, Owner::Slide(lid), &moved).frame, Rect::new(1, 2, 3, 4));
    let s0 = d.slide(0).unwrap().clone();
    let sub = s0.shapes.iter().find(|x| x.ph.unwrap().kind == PhKind::SubTitle).unwrap();
    let rs = resolve(&d, Owner::Slide(s0.layout), sub);
    assert_eq!(rs.levels[0].bullet, Some(Bullet::None));
    assert_eq!(rs.levels[0].chars.size, Some(48));
    assert_eq!(rs.levels[0].para.align, Some(officina_text::model::Align::Center));
}

#[test]
fn theme_colours_resolve_and_follow_the_theme() {
    let t = Theme::default();
    assert_eq!(color(&t, &Color::theme(Slot::Accent1)), 0x4472C4FF);
    // accent1 75 % luminance (PowerPoint's "darker 25 %") is 2F5597.
    assert_eq!(color(&t, &Color::theme(Slot::Accent1).with(ColorMod::LumMod(75_000))), 0x2F5597FF);
    // lighter 40 %: lumMod 60000 lumOff 40000 is 8FAADC.
    let near = |a: u32, b: u32| (0..4).all(|k| ((a >> (8 * k)) & 255).abs_diff((b >> (8 * k)) & 255) <= 1);
    assert!(near(color(&t, &Color::theme(Slot::Accent1).with(ColorMod::LumMod(60_000)).with(ColorMod::LumOff(40_000))), 0x8FAADCFF));
    assert_eq!(color(&t, &Color::rgb(0x123456).with(ColorMod::Alpha(50_000))), 0x12345680);
    let mut t2 = t.clone();
    t2.colors[Slot::Accent1 as usize] = 0xFF0000;
    assert_eq!(color(&t2, &Color::theme(Slot::Accent1)), 0xFF0000FF);
}

#[test]
fn slides_draw_with_master_layout_and_own_text() {
    let mut d = deck_with(3);
    let mut n = Notes::default();
    let items = draw_slide(&mut d, 1, &FixedFonts, &mut n).unwrap();
    // The background first, white from the master's lt1.
    match &items[0] {
        DrawItem::Path { fill: Some(officina_slides::draw::Paint::Solid(c)), .. } => assert_eq!(*c, 0xFFFFFFFF),
        x => panic!("{x:?}"),
    }
    let t = texts(&items);
    assert_eq!(t.len(), 2, "{t:?}");
    assert!(t[0].contains("Slide") && t[0].contains('2'), "{t:?}");
    // Bullets before the body's points, the detail indented further.
    assert!(t[1].starts_with('•'), "{t:?}");
    assert!(t[1].contains("First") && t[1].contains("detail"), "{t:?}");
    // A layout placeholder with no slide shape (date) draws nothing.
    assert!(n.unknown_presets.is_empty());
    // Text moves with its frame: the body's first glyph is below the title's.
    let tops: Vec<f64> = items.iter().filter_map(|i| if let DrawItem::Text { xf, .. } = i { Some(xf[5]) } else { None }).collect();
    assert!(tops[1] > tops[0]);
    // A shape of a preset we don't draw is drawn as its box and reported.
    let sid = d.slide_ids().nth(1).unwrap();
    let id = d.new_id();
    let mut s = Shape::rect(id, Rect::new(0, 0, 100_000, 100_000), Fill::Solid(Color::theme(Slot::Accent2)));
    if let ShapeKind::Geom { geom, .. } = &mut s.kind {
        *geom = Some(Geom::preset("notAPreset"));
    }
    DeckOp::InsertShape { target: Target::Slide(sid), group: None, at: 9, shape: Box::new(s) }.apply(&mut d);
    let items = draw_slide(&mut d, 1, &FixedFonts, &mut n).unwrap();
    assert_eq!(n.unknown_presets, vec![Arc::<str>::from("notAPreset")]);
    assert!(items.iter().any(|i| matches!(i, DrawItem::Path { fill: Some(officina_slides::draw::Paint::Solid(0xED7D31FF)), .. })));
}

#[test]
fn groups_map_their_children() {
    let mut d = deck_with(1);
    let sid = d.slide_ids().next().unwrap();
    let a = d.new_id();
    let g = d.new_id();
    let child = Shape::rect(a, Rect::new(100, 100, 100, 100), Fill::Solid(Color::rgb(0x00FF00)));
    let mut group = Shape::new(g, Rect::new(1000, 2000, 2000, 2000), ShapeKind::Group { child: Rect::new(100, 100, 200, 200), shapes: vec![child] });
    group.name = "Group".into();
    DeckOp::InsertShape { target: Target::Slide(sid), group: None, at: 0, shape: Box::new(group) }.apply(&mut d);
    let items = draw_slide(&mut d, 0, &FixedFonts, &mut Notes::default()).unwrap();
    let xf = items.iter().find_map(|i| match i {
        DrawItem::Path { xf, fill: Some(officina_slides::draw::Paint::Solid(0x00FF00FF)), .. } => Some(*xf),
        _ => None,
    });
    let xf = xf.unwrap();
    // The child's box (100..200 in the group's 100..300) is the group's first half.
    assert_eq!(officina_slides::draw::apply(&xf, 0.0, 0.0), (1000.0, 2000.0));
    assert_eq!(officina_slides::draw::apply(&xf, 100.0, 100.0), (2000.0, 3000.0));
}

/// A little random generator (no dependencies).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn random_op(d: &mut Deck, r: &mut Rng) -> DeckOp {
    let n = d.slide_count();
    let pick_slide = |d: &mut Deck, r: &mut Rng| d.slide_ids().nth(r.below(n)).unwrap();
    match r.below(10) {
        0 => {
            let l = d.layouts[r.below(d.layouts.len())].id;
            let s = new_slide(d, l);
            DeckOp::InsertSlide { at: r.below(n + 1), slide: Box::new(s) }
        }
        1 if n > 1 => DeckOp::RemoveSlide { id: pick_slide(d, r) },
        2 => DeckOp::MoveSlide { id: pick_slide(d, r), to: r.below(n) },
        3 => {
            let id = pick_slide(d, r);
            let i = d.slide_index(id).unwrap();
            let mut p = d.slide(i).unwrap().props();
            p.hidden = !p.hidden;
            p.transition = Some(Transition { kind: TransitionKind::Push(Dir::Left), ms: 500, on_click: true, after_ms: None });
            p.notes = Some(TextBody::plain(&format!("notes {}", r.next() % 100)));
            DeckOp::SetSlide { id, props: Box::new(p) }
        }
        4 => {
            let id = pick_slide(d, r);
            let sid = d.new_id();
            let x = r.below(1000) as i64 * 10_000;
            DeckOp::InsertShape { target: Target::Slide(id), group: None, at: r.below(4), shape: Box::new(Shape::rect(sid, Rect::new(x, x, 500_000, 300_000), Fill::Solid(Color::theme(Slot::Accent3)))) }
        }
        5 => {
            let id = pick_slide(d, r);
            let i = d.slide_index(id).unwrap();
            let s = d.slide(i).unwrap();
            if s.shapes.is_empty() {
                return DeckOp::Nop;
            }
            let sh = s.shapes[r.below(s.shapes.len())].id;
            DeckOp::RemoveShape { target: Target::Slide(id), id: sh }
        }
        6 | 7 => {
            let id = pick_slide(d, r);
            let i = d.slide_index(id).unwrap();
            let s = d.slide(i).unwrap();
            let Some(sh) = s.shapes.iter().find(|x| x.text().is_some()) else { return DeckOp::Nop };
            let at = sh.text().unwrap().doc.para(0).unwrap().text.len() as u32;
            DeckOp::Text { target: Target::Slide(id), shape: sh.id, op: TextOp::Splice { block: 0, at, del: 0, ins: Fragment { text: "x".into(), runs: vec![(0, 0)] } } }
        }
        8 => {
            let id = pick_slide(d, r);
            let i = d.slide_index(id).unwrap();
            let s = d.slide(i).unwrap();
            let Some(mut sh) = s.shapes.first().cloned() else { return DeckOp::Nop };
            sh.frame = Some(Rect::new(r.below(100) as i64, 5, 600_000, 400_000));
            sh.rot = 60_000 * r.below(360) as i32;
            DeckOp::SetShape { target: Target::Slide(id), shape: Box::new(sh) }
        }
        _ => {
            let id = pick_slide(d, r);
            let i = d.slide_index(id).unwrap();
            let s = d.slide(i).unwrap();
            if s.notes.is_none() {
                return DeckOp::Nop;
            }
            DeckOp::Text { target: Target::Slide(id), shape: NOTES, op: TextOp::Splice { block: 0, at: 0, del: 0, ins: Fragment { text: "n".into(), runs: vec![(0, 0)] } } }
        }
    }
}

#[test]
fn random_ops_undo_and_redo_exactly() {
    for seed in 1..6u64 {
        let mut d = deck_with(5);
        let start = content(&mut d);
        let mut h = History::new();
        let mut r = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let mut journal = Vec::new();
        for _ in 0..150 {
            let op = random_op(&mut d, &mut r);
            h.run(&mut d, "op", vec![op.clone()]);
            journal.push(op);
        }
        let end = content(&mut d);
        while h.undo(&mut d).is_some() {}
        assert!(content(&mut d) == start, "seed {seed}: undo all");
        while h.redo(&mut d).is_some() {}
        assert!(content(&mut d) == end, "seed {seed}: redo all");
        let _ = journal;
    }
}

#[test]
fn journal_replays_to_the_same_deck() {
    let mut d = deck_with(4);
    let mut base = native::load(Arc::new(bytes(&d))).unwrap();
    let mut r = Rng(77);
    let mut recs = Vec::new();
    for _ in 0..200 {
        let op = random_op(&mut d, &mut r);
        let _inv = op.clone().apply(&mut d);
        recs.push(encode_record(&d, &op));
    }
    for rec in &recs {
        let op = decode_record(&mut base, rec).unwrap();
        op.apply(&mut base);
    }
    base.load_all().unwrap();
    assert_eq!(bytes(&base), bytes(&d));
}

#[test]
fn native_round_trip_is_exact_and_lazy() {
    let mut d = deck_with(20);
    let png = vec![7u8; 1000];
    let m1 = d.add_media("image/png", &png);
    let m2 = d.add_media("image/png", &png);
    assert_eq!(m1, m2, "the same bytes are one piece");
    let big: Vec<u8> = (0..(native::MEDIA_PIECE + 12345)).map(|i| (i * 31 % 251) as u8).collect();
    let m3 = d.add_media("video/mp4", &big);
    let lone = d.add_media("image/png", b"unused");
    for i in 0..20 {
        let sid = d.slide_ids().nth(i).unwrap();
        let id = d.new_id();
        let pic = Shape::new(id, Rect::new(0, 0, 1000, 1000), ShapeKind::Picture { media: m1, crop: [0; 4], line: None });
        DeckOp::InsertShape { target: Target::Slide(sid), group: None, at: 0, shape: Box::new(pic) }.apply(&mut d);
    }
    let sid = d.slide_ids().next().unwrap();
    let id = d.new_id();
    DeckOp::InsertShape { target: Target::Slide(sid), group: None, at: 0, shape: Box::new(Shape::new(id, Rect::new(0, 0, 10, 10), ShapeKind::Video { media: m3, poster: Some(m1) })) }.apply(&mut d);
    let b = bytes(&d);
    // The logo on 20 slides is stored once; an unused piece not at all.
    assert!(b.len() < 1000 * 2 + big.len() + 200_000, "{}", b.len());
    let mut l = native::load(Arc::new(b.clone())).unwrap();
    assert_eq!(l.slide_count(), 20);
    assert_eq!(l.loaded_count(), 0, "opening reads no slide");
    assert!(l.media(lone).is_none());
    assert_eq!(bytes(&l), b, "saving copies unread slides as they are");
    assert_eq!(*l.media_bytes(m3).unwrap(), big[..]);
    assert_eq!(*l.media_bytes(m1).unwrap(), png[..]);
    let s = l.slide(3).unwrap().clone();
    assert_eq!(l.loaded_count(), 1);
    assert_eq!(s, *d.slide(3).unwrap());
    // A slide read and unchanged goes back to the file; a changed one stays.
    assert!(l.unload_slide(3));
    l.slide_mut(4).unwrap().hidden = true;
    assert!(!l.unload_slide(4));
    // Adding the same bytes to a loaded deck finds the piece in the file.
    assert_eq!(l.add_media("image/png", &png), m1);
    // New ids never collide with the file's.
    let n = l.new_id();
    assert!(n >= d.id_watermark());
}

#[test]
fn previews_are_kept_with_their_pictures() {
    let mut d = deck_with(3);
    let photo: Vec<u8> = (0..50_000u32).map(|i| (i * 7 % 253) as u8).collect();
    let m = d.add_media("image/jpeg", &photo);
    let gone = d.add_media("image/png", b"not used");
    assert_eq!(d.pictures_without_preview(), vec![m, gone]);
    assert!(d.set_preview(m, "image/jpeg", 512, 341, b"small copy"));
    assert!(d.set_preview(gone, "image/png", 10, 10, b"x"));
    assert!(!d.set_preview(m, "image/jpeg", 0, 10, b"x"), "an empty preview");
    let sid = d.slide_ids().next().unwrap();
    let id = d.new_id();
    let pic = Shape::new(id, Rect::new(0, 0, 1000, 1000), ShapeKind::Picture { media: m, crop: [0; 4], line: None });
    DeckOp::InsertShape { target: Target::Slide(sid), group: None, at: 0, shape: Box::new(pic) }.apply(&mut d);
    let b = bytes(&d);
    let l = native::load(Arc::new(b.clone())).unwrap();
    assert_eq!(l.preview(m), Some((512, 341)));
    assert_eq!(&*l.preview_bytes(m).unwrap().unwrap(), b"small copy");
    assert!(l.pictures_without_preview().is_empty(), "the unused picture and its preview are dropped");
    assert_eq!(bytes(&l), b, "saving a read deck copies its previews");
    // A damaged preview table costs the previews only.
    let pos = b.windows(10).position(|w| w == b"small copy").unwrap();
    let mut bad = b.clone();
    bad[pos] ^= 1;
    let l = native::load(Arc::new(bad)).unwrap();
    assert!(l.preview_bytes(m).unwrap().is_err(), "the preview fails its checksum");
    assert_eq!(*l.media_bytes(m).unwrap(), photo[..]);
}

#[test]
fn damage_fails_alone_and_never_panics() {
    let d = deck_with(6);
    let b = bytes(&d);
    let l = native::load(Arc::new(b.clone())).unwrap();
    // Flip a byte inside the third slide's chunk: that slide fails, the rest read.
    let pos = b.windows(7).position(|w| w == b"Slide 3").unwrap();
    let mut bad = b.clone();
    bad[pos] ^= 1;
    let mut l2 = native::load(Arc::new(bad)).unwrap();
    assert!(l2.slide(2).is_err());
    assert!(l2.slide(1).is_ok() && l2.slide(5).is_ok());
    drop(l);
    // Truncations and flips: errors, never panics.
    for n in (0..b.len()).step_by(7) {
        let _ = native::load(Arc::new(b[..n].to_vec()));
    }
    for i in (0..b.len()).step_by(3) {
        let mut x = b.clone();
        x[i] ^= 0x41;
        if let Ok(mut dk) = native::load(Arc::new(x)) {
            for s in 0..dk.slide_count() {
                if dk.slide(s).is_ok() {
                    let _ = draw_slide(&mut dk, s, &FixedFonts, &mut Notes::default());
                }
            }
        }
    }
}

#[test]
fn a_thousand_slide_deck_opens_reading_only_what_it_shows() {
    let d = deck_with(1000);
    let b = bytes(&d);
    let t = std::time::Instant::now();
    let mut l = native::load(Arc::new(b)).unwrap();
    let mut n = Notes::default();
    let items = draw_slide(&mut l, 0, &FixedFonts, &mut n).unwrap();
    let took = t.elapsed();
    assert!(!items.is_empty());
    assert_eq!(l.loaded_count(), 1);
    assert!(took.as_millis() < 1000, "{took:?}");
}
