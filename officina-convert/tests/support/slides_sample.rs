//! A deck using what the slide model has, and what to compare when it comes
//! back from a file (shared by tests/slides.rs and examples/slides_lo.rs).

use officina_slides::model::*;
use officina_slides::ops::new_slide;
use officina_slides::resolve::{self, resolve, Owner};
use officina_text::model::{CharAttrs, ListKind, ListRef, ParaProps};

/// A small valid PNG, `w` × `h`, one colour.
pub fn png(w: u32, h: u32) -> Vec<u8> {
    let chunk = |out: &mut Vec<u8>, kind: &[u8], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut c = kind.to_vec();
        c.extend_from_slice(data);
        out.extend_from_slice(&c);
        out.extend_from_slice(&officina_convert::zip::crc32(&c).to_be_bytes());
    };
    let mut raw = Vec::new();
    for _ in 0..h {
        raw.push(0);
        for _ in 0..w {
            raw.extend_from_slice(&[40, 120, 200]);
        }
    }
    let z = miniz_oxide::deflate::compress_to_vec_zlib(&raw, 6);
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

fn fill_ph(s: &mut Slide, kinds: &[PhKind], text: &str) {
    if let Some(sh) = s.shapes.iter_mut().find(|sh| sh.ph.as_ref().is_some_and(|p| kinds.contains(&p.kind))) {
        if let Some(t) = sh.text_mut() {
            set_plain(&mut t.doc, text);
        }
    }
}

fn text_shape(deck: &mut Deck, r: Rect, geom: &str, fill: Fill, text: &str) -> Shape {
    let id = deck.new_id();
    let mut t = TextBody::plain(text);
    t.anchor = Some(Anchor::Middle);
    Shape::new(id, r, ShapeKind::Geom { geom: Some(Geom::preset(geom)), fill: Some(fill), line: Some(Stroke::solid(Color::rgb(0x203040), 19_050)), text: Some(t) })
}

pub fn sample() -> Deck {
    let mut deck = Deck::new();
    let lay = |deck: &Deck, k: &str| deck.layout_of_kind(k).expect("layout").id;

    // 1: title slide with notes and a fade.
    let l = lay(&deck, "title");
    let mut s = new_slide(&mut deck, l);
    fill_ph(&mut s, &[PhKind::CtrTitle, PhKind::Title], "Quarterly review");
    fill_ph(&mut s, &[PhKind::SubTitle], "Prepared for the board");
    s.notes = Some(TextBody::plain("Welcome everyone and thank the hosts"));
    s.transition = Some(Transition { kind: TransitionKind::Fade, ms: 700, on_click: true, after_ms: None });
    deck.push_slide(s);

    // 2: bullets in levels, a numbered paragraph, shapes, builds.
    let l = lay(&deck, "obj");
    let mut s = new_slide(&mut deck, l);
    fill_ph(&mut s, &[PhKind::Title], "Highlights");
    fill_ph(&mut s, &[PhKind::Body, PhKind::Obj], "Revenue grew steadily\n\tEurope led the growth\n\tAsia followed closely\nCosts held flat\nNumbered outlook item");
    if let Some(sh) = s.shapes.iter_mut().find(|sh| sh.ph.is_some_and(|p| matches!(p.kind, PhKind::Body | PhKind::Obj))) {
        let t = sh.text_mut().unwrap();
        t.bullets.push(Bullet::Number { scheme: "arabicPeriod".into(), start: 1 });
        let k = t.doc.blocks.iter().position(|b| b.as_para().is_some_and(|p| p.text == "Numbered outlook item")).unwrap();
        t.doc.blocks.update(k, |b| b.as_para_mut().unwrap().props = ParaProps { list: Some(Some(ListRef { id: 1, kind: ListKind::Numbered, level: 0 })), ..Default::default() });
    }
    let star = {
        let id = deck.new_id();
        Shape::new(id, Rect::new(8_600_000, 4_300_000, 1_800_000, 1_700_000), ShapeKind::Geom { geom: Some(Geom::preset("star5")), fill: Some(Fill::Solid(Color::theme(Slot::Accent2))), line: Some(Stroke::none()), text: None })
    };
    let star_id = star.id;
    s.shapes.push(star);
    let mut turned = text_shape(&mut deck, Rect::new(6_000_000, 1_500_000, 2_000_000, 800_000), "rect", Fill::Gradient { stops: vec![GradientStop { pos: 0, color: Color::rgb(0x1F4E79) }, GradientStop { pos: 100_000, color: Color::rgb(0x9DC3E6) }], angle: 0, radial: false }, "Turned box");
    turned.rot = 30 * 60_000;
    let turned_id = turned.id;
    s.shapes.push(turned);
    let round = text_shape(&mut deck, Rect::new(9_000_000, 1_500_000, 1_800_000, 1_200_000), "ellipse", Fill::Solid(Color::rgb(0xFFC000)), "Round note");
    s.shapes.push(round);
    let line = {
        let id = deck.new_id();
        let mut st = Stroke::solid(Color::rgb(0xC00000), 28_575);
        st.tail = Arrow::Triangle;
        st.dash = Dash::Dash;
        Shape::new(id, Rect::new(6_000_000, 3_000_000, 2_500_000, 900_000), ShapeKind::Geom { geom: Some(Geom::preset("line")), fill: None, line: Some(st), text: None })
    };
    s.shapes.push(line);
    s.builds = vec![Build { shape: turned_id, effect: Effect::Fade, trigger: Trigger::OnClick, ms: 500, delay_ms: 0, by_paragraph: false }, Build { shape: star_id, effect: Effect::FlyIn(Dir::Left), trigger: Trigger::AfterPrevious, ms: 600, delay_ms: 0, by_paragraph: false }];
    s.transition = Some(Transition { kind: TransitionKind::Push(Dir::Left), ms: 500, on_click: true, after_ms: None });
    deck.push_slide(s);

    // 3: a picture, a group, a custom outline; a bold red run.
    let l = lay(&deck, "titleOnly");
    let mut s = new_slide(&mut deck, l);
    fill_ph(&mut s, &[PhKind::Title], "Pictures and groups");
    let m = deck.add_media("image/png", &png(40, 20));
    let id = deck.new_id();
    s.shapes.push(Shape::new(id, Rect::new(900_000, 1_800_000, 3_600_000, 1_800_000), ShapeKind::Picture { media: m, crop: [0; 4], line: None }));
    let a = text_shape(&mut deck, Rect::new(0, 0, 1_000_000, 600_000), "roundRect", Fill::Solid(Color::rgb(0x70AD47)), "Grouped one");
    let b = text_shape(&mut deck, Rect::new(1_200_000, 0, 1_000_000, 600_000), "rect", Fill::Solid(Color::rgb(0x5B9BD5)), "Grouped two");
    let id = deck.new_id();
    s.shapes.push(Shape::new(id, Rect::new(5_500_000, 2_000_000, 4_400_000, 1_200_000), ShapeKind::Group { child: Rect::new(0, 0, 2_200_000, 600_000), shapes: vec![a, b] }));
    let id = deck.new_id();
    let tri = Path { w: 100, h: 100, cmds: vec![PathCmd::Move(0, 100), PathCmd::Line(50, 0), PathCmd::Line(100, 100), PathCmd::Close], filled: true, stroked: true };
    s.shapes.push(Shape::new(id, Rect::new(5_500_000, 4_000_000, 1_500_000, 1_500_000), ShapeKind::Geom { geom: Some(Geom::Custom(vec![tri])), fill: Some(Fill::Solid(Color::rgb(0x7030A0))), line: Some(Stroke::none()), text: None }));
    let mut tb = text_shape(&mut deck, Rect::new(900_000, 4_200_000, 4_000_000, 700_000), "rect", Fill::None, "Plain then strong words");
    if let Some(t) = tb.text_mut() {
        let strong = t.doc.attrs.intern(CharAttrs { bold: Some(true), color: Some(0xC00000), ..Default::default() });
        t.doc.blocks.update(0, |b| {
            let p = b.as_para_mut().unwrap();
            let base = p.runs[0].1;
            p.runs = vec![(0, base), (11, strong)];
        });
    }
    s.shapes.push(tb);
    deck.push_slide(s);

    // 4: hidden, automatic advance.
    let l = lay(&deck, "secHead");
    let mut s = new_slide(&mut deck, l);
    fill_ph(&mut s, &[PhKind::Title], "Appendix");
    fill_ph(&mut s, &[PhKind::Body], "Hidden backup material");
    s.hidden = true;
    s.transition = Some(Transition { kind: TransitionKind::Wipe(Dir::Down), ms: 1000, on_click: false, after_ms: Some(3000) });
    deck.push_slide(s);
    deck
}

fn words(t: &str) -> Vec<String> {
    let mut v: Vec<String> = t.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(str::to_string).collect();
    v.sort();
    v
}

fn all_shapes(shapes: &[Shape], out: &mut Vec<Shape>) {
    for s in shapes {
        out.push(s.clone());
        if let ShapeKind::Group { shapes, .. } = &s.kind {
            all_shapes(shapes, out);
        }
    }
}

/// What a deck says, to compare across files.
#[derive(Debug, PartialEq)]
struct Facts {
    slides: usize,
    texts: Vec<Vec<String>>,
    notes: Vec<Vec<String>>,
    pictures: usize,
    hidden: Vec<bool>,
    transitions: Vec<Option<std::mem::Discriminant<TransitionKind>>>,
    builds: Vec<usize>,
}

fn facts(d: &mut Deck) -> Facts {
    let n = d.slide_count();
    let mut f = Facts { slides: n, texts: vec![], notes: vec![], pictures: 0, hidden: vec![], transitions: vec![], builds: vec![] };
    for i in 0..n {
        let s = d.slide(i).unwrap().clone();
        let mut all = Vec::new();
        all_shapes(&s.shapes, &mut all);
        let text: String = all.iter().filter_map(|x| x.text()).map(|t| t.text() + " ").collect();
        f.texts.push(words(&text));
        f.notes.push(words(&s.notes.as_ref().map(|t| t.text()).unwrap_or_default()));
        f.pictures += all.iter().filter(|x| matches!(x.kind, ShapeKind::Picture { .. })).count();
        f.hidden.push(s.hidden);
        f.transitions.push(s.transition.as_ref().map(|t| std::mem::discriminant(&t.kind)));
        f.builds.push(s.builds.len());
    }
    f
}

fn find<'a>(all: &'a [Shape], text: &str) -> Option<&'a Shape> {
    all.iter().find(|s| s.text().is_some_and(|t| t.text().contains(text)))
}

/// The body placeholder's paragraphs on slide 2: level, bulleted, numbered.
fn body_marks(d: &mut Deck) -> Vec<(u16, bool, bool)> {
    let s = d.slide(1).unwrap().clone();
    let Some(sh) = s.shapes.iter().find(|x| x.text().is_some_and(|t| t.text().contains("Revenue"))) else { return vec![] };
    let r = resolve(d, Owner::Slide(s.layout), sh);
    let t = sh.text().unwrap();
    t.doc
        .blocks
        .iter()
        .filter_map(|b| b.as_para())
        .map(|p| {
            let lvl = p.style;
            let b = match &p.props.list {
                Some(Some(lr)) => t.bullets.get(lr.id as usize - 1).cloned(),
                Some(None) => None,
                None => r.levels.get(lvl as usize).and_then(|l| l.bullet.clone()),
            };
            (lvl, b.as_ref().is_some_and(|b| *b != Bullet::None), matches!(b, Some(Bullet::Number { .. })))
        })
        .collect()
}

pub fn compare(c: &mut dyn FnMut(&str, bool, String), how: &str, want: &mut Deck, got: &mut Deck) {
    let (a, b) = (facts(want), facts(got));
    c(&format!("{how}: slide count"), a.slides == b.slides, format!("{} vs {}", a.slides, b.slides));
    c(&format!("{how}: each slide's words"), a.texts == b.texts, format!("{:?}\n vs {:?}", a.texts, b.texts));
    c(&format!("{how}: notes"), a.notes == b.notes, format!("{:?} vs {:?}", a.notes, b.notes));
    c(&format!("{how}: pictures"), a.pictures == b.pictures, format!("{} vs {}", a.pictures, b.pictures));
    c(&format!("{how}: hidden slide"), a.hidden == b.hidden, format!("{:?} vs {:?}", a.hidden, b.hidden));
    c(&format!("{how}: transitions"), a.transitions == b.transitions, format!("{:?} vs {:?}", a.transitions, b.transitions));
    c(&format!("{how}: builds"), a.builds == b.builds, format!("{:?} vs {:?}", a.builds, b.builds));
    let (ma, mb) = (body_marks(want), body_marks(got));
    c(&format!("{how}: bullet levels and numbering"), ma == mb, format!("{ma:?} vs {mb:?}"));
    let s = got.slide(1).unwrap().clone();
    let mut all = Vec::new();
    all_shapes(&s.shapes, &mut all);
    let turned = find(&all, "Turned");
    c(&format!("{how}: turned box at 30°"), turned.is_some_and(|t| (t.rot - 30 * 60_000).abs() <= 60_000), format!("{:?}", turned.map(|t| t.rot)));
    let grad = turned.and_then(|t| match &t.kind {
        ShapeKind::Geom { fill: Some(Fill::Gradient { stops, angle, radial: false }), .. } => Some((resolve::color(&got.theme, &stops[0].color) >> 8, *angle)),
        _ => None,
    });
    c(&format!("{how}: gradient kept, dark to light, left to right"), grad.is_some_and(|(c0, a)| c0 == 0x1F4E79 && (a <= 60_000 || a >= 21_540_000)), format!("{grad:?}"));
    let star = all.iter().find(|x| matches!(&x.kind, ShapeKind::Geom { geom: Some(Geom::Preset { name, .. }), .. } if &**name == "star5"));
    c(&format!("{how}: star keeps its preset"), star.is_some(), String::new());
    let arrow = all.iter().find_map(|x| match &x.kind {
        ShapeKind::Geom { line: Some(l), .. } if l.tail != Arrow::None || l.head != Arrow::None => Some(l.clone()),
        _ => None,
    });
    c(&format!("{how}: dashed line with an arrowhead"), arrow.as_ref().is_some_and(|l| l.dash != Dash::Solid), format!("{arrow:?}"));
    let s3 = got.slide(2).unwrap().clone();
    let mut all3 = Vec::new();
    all_shapes(&s3.shapes, &mut all3);
    let strong = find(&all3, "strong").and_then(|x| {
        let t = x.text()?;
        let p = t.doc.blocks.iter().find_map(|b| b.as_para())?;
        let at = p.text.find("strong")? as u32;
        let run = p.runs.iter().rev().find(|r| r.0 <= at)?;
        Some(t.doc.attrs.get(run.1).clone())
    });
    c(&format!("{how}: bold red run"), strong.as_ref().is_some_and(|a| a.bold == Some(true) && a.color == Some(0xC00000)), format!("{strong:?}"));
    let grouped = s3.shapes.iter().any(|x| matches!(&x.kind, ShapeKind::Group { shapes, .. } if shapes.len() == 2));
    c(&format!("{how}: group of two"), grouped, String::new());
}

