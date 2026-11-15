//! Decks through `.pptx` and `.odp` and back (docs/spec/officina.md §9.2, O6c).

#[path = "support/slides_sample.rs"]
mod sample;

use officina_convert::{odp, pptx};
use officina_slides::model::*;
use sample::{compare, sample};

fn check(fails: &mut Vec<String>) -> impl FnMut(&str, bool, String) + '_ {
    move |what, ok, detail| {
        if !ok {
            fails.push(format!("{what}: {detail}"));
        }
    }
}

#[test]
fn pptx_round_trip() {
    let mut deck = sample();
    let bytes = pptx::write(&mut deck, Vec::new()).unwrap();
    let (mut back, loaded) = pptx::read(&bytes).unwrap();
    let mut fails = Vec::new();
    compare(&mut check(&mut fails), "pptx", &mut deck, &mut back);
    assert!(fails.is_empty(), "{fails:#?}");
    assert_eq!(loaded.pictures, 1);
    // Masters, layouts and level styles come back as they went.
    assert_eq!(back.masters.len(), deck.masters.len());
    assert_eq!(back.layouts.len(), deck.layouts.len());
    assert_eq!(back.masters[0].body_style, deck.masters[0].body_style);
    let s = back.slide(1).unwrap();
    assert_eq!(s.builds.len(), 2);
    assert!(matches!(s.builds[1].effect, Effect::FlyIn(Dir::Left)));
    assert!(matches!(back.slide(0).unwrap().transition, Some(Transition { kind: TransitionKind::Fade, .. })));
}

#[test]
fn odp_round_trip() {
    let mut deck = sample();
    let bytes = odp::write(&mut deck, Vec::new()).unwrap();
    let (mut back, loaded) = odp::read(&bytes).unwrap();
    let mut fails = Vec::new();
    compare(&mut check(&mut fails), "odp", &mut deck, &mut back);
    assert!(fails.is_empty(), "{fails:#?}");
    assert_eq!(loaded.pictures, 1);
    assert!(loaded.report.is_empty(), "{:?}", loaded.report.lines());
    // One master page per layout.
    assert_eq!(back.masters.len(), deck.layouts.len());
    let s = back.slide(1).unwrap().clone();
    assert!(matches!(s.transition, Some(Transition { kind: TransitionKind::Push(Dir::Left), .. })));
    let b: Vec<(Effect, Trigger)> = s.builds.iter().map(|b| (b.effect, b.trigger)).collect();
    assert_eq!(b, vec![(Effect::Fade, Trigger::OnClick), (Effect::FlyIn(Dir::Left), Trigger::AfterPrevious)]);
    let s4 = back.slide(3).unwrap();
    assert_eq!(s4.transition.as_ref().and_then(|t| t.after_ms), Some(3000));
    assert!(s4.hidden);
    // The custom outline stays an outline.
    let s3 = back.slide(2).unwrap().clone();
    assert!(s3.shapes.iter().any(|x| matches!(&x.kind, ShapeKind::Geom { geom: Some(Geom::Custom(p)), .. } if p[0].cmds.len() == 4)));
    // Written again, the same file.
    let again = odp::write(&mut back, Vec::new()).unwrap();
    let (mut third, _) = odp::read(&again).unwrap();
    let mut fails = Vec::new();
    compare(&mut check(&mut fails), "odp twice", &mut deck, &mut third);
    assert!(fails.is_empty(), "{fails:#?}");
}

#[test]
fn across_formats() {
    let mut deck = sample();
    let (mut a, _) = odp::read(&odp::write(&mut deck, Vec::new()).unwrap()).unwrap();
    let (mut b, _) = pptx::read(&pptx::write(&mut a, Vec::new()).unwrap()).unwrap();
    let (mut c, _) = odp::read(&odp::write(&mut b, Vec::new()).unwrap()).unwrap();
    let mut fails = Vec::new();
    compare(&mut check(&mut fails), "odp→pptx→odp", &mut deck, &mut c);
    assert!(fails.is_empty(), "{fails:#?}");
}

fn mangle(bytes: &[u8], seed: u64, f: impl Fn(&[u8])) {
    let mut x = seed;
    for _ in 0..1500 {
        let mut b = bytes.to_vec();
        for _ in 0..1 + (x % 8) {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let i = (x as usize) % b.len();
            b[i] = (x >> 32) as u8;
        }
        if x % 5 == 0 {
            b.truncate((x as usize) % b.len());
        }
        f(&b);
    }
}

#[test]
fn hostile_packages_fail_cleanly() {
    let mut deck = sample();
    let p = pptx::write(&mut deck, Vec::new()).unwrap();
    mangle(&p, 0x51de, |b| {
        let _ = pptx::read(b);
    });
    let o = odp::write(&mut deck, Vec::new()).unwrap();
    mangle(&o, 0x0d9, |b| {
        let _ = odp::read(b);
    });
}

#[test]
fn hostile_odp_xml() {
    // Equations that loop, deep lists, absurd sizes: read without hanging.
    let mut deck = sample();
    let o = odp::write(&mut deck, Vec::new()).unwrap();
    let a = officina_convert::zip::Archive::open(&o).unwrap();
    let content = String::from_utf8(a.read_name("content.xml").unwrap().unwrap()).unwrap();
    let mut evil = String::from(r#"<draw:custom-shape svg:width="1e9cm" svg:height="-5cm" svg:x="NaNcm" draw:transform="rotate (1e300) skewX (1) translate (1e30cm 2cm)"><draw:enhanced-geometry draw:type="weird" draw:enhanced-path="M ?f0 ?f1 L ?f1 ?f0 T 0 0 10 10 0 7200 G 1 1 0 1e9 N"><draw:equation draw:name="f0" draw:formula="?f1+?f1+?f1"/><draw:equation draw:name="f1" draw:formula="?f0*?f0*(((((((((((1)))))))))))"/></draw:enhanced-geometry>"#);
    for _ in 0..30 {
        evil.push_str("<text:list><text:list-item>");
    }
    evil.push_str("<text:p>deep</text:p>");
    for _ in 0..30 {
        evil.push_str("</text:list-item></text:list>");
    }
    evil.push_str(r#"</draw:custom-shape><draw:path svg:viewBox="0 0 10 10" svg:d="M0 0 A 1e9 1e-9 45 1 1 10 10 Q 1 2 3 4 T 5 6 S 1 1 2 2 Z m 1 1 l 1 1"/>"#);
    let content = content.replacen("</draw:page>", &format!("{evil}</draw:page>"), 1);
    let mut z = officina_convert::zip::ZipWriter::new(Vec::new());
    for name in ["mimetype", "styles.xml", "META-INF/manifest.xml"] {
        z.add(name, &a.read_name(name).unwrap().unwrap()).unwrap();
    }
    z.add("content.xml", content.as_bytes()).unwrap();
    let bytes = z.finish().unwrap();
    let (mut d, _) = odp::read(&bytes).unwrap();
    assert_eq!(d.slide_count(), 4);
    let s = d.slide(0).unwrap();
    assert!(s.shapes.iter().any(|x| x.text().is_some_and(|t| t.text().contains("deep"))));
}
