//! SVG as an image (src/svg.rs, threat model §2.4): drawn at the painted
//! size under a strict profile, in the worker. Unit cases first, then a
//! mutation fuzzer whose POSITIVE CONTROLS (the attacks the profile exists
//! for) must each be refused by name, and whose reach assertion fails the
//! run unless every outcome class was produced: a fuzzer that never reaches
//! "use expansion" is not testing it.
//! `NAVIGATOR_SVG_CORPUS=<dir>[:<dir>]` adds every SVG in those blob
//! directories as seeds and checked inputs; `NAVIGATOR_SVG_FUZZ_ITERS`
//! raises the iteration count.

use navigator_backend::image::{decode_unaddressed, Ceilings, DecodeError, Format};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const NS: &str = "xmlns='http://www.w3.org/2000/svg'";
fn svg(body: &str, attrs: &str) -> Vec<u8> { format!("<svg {NS} {attrs}>{body}</svg>").into_bytes() }
fn at(target: (u32, u32)) -> Ceilings { Ceilings { target: Some(target), ..Ceilings::default() } }
fn refused(b: &[u8]) -> &'static str {
    match decode_unaddressed(b, at((64, 64))) { Err(DecodeError::Refused { why, .. }) => why, other => panic!("not refused: {other:?}") }
}

#[test]
fn a_shape_is_drawn_at_the_painted_size_with_straight_alpha() {
    let b = svg("<rect width='10' height='10' fill='#ff0000'/><rect x='10' width='10' height='10' fill='#0000ff' fill-opacity='0.5'/>",
                "width='20' height='10' viewBox='0 0 20 10'");
    // At its own size…
    let p = decode_unaddressed(&b, Ceilings::default()).unwrap();
    assert_eq!((p.width, p.height), (20, 10));
    assert_eq!(&p.rgba[..4], &[255, 0, 0, 255]);
    let i = (5 * 20 + 15) * 4;
    assert_eq!(p.rgba[i + 2], 255, "straight alpha: the blue channel is not darkened by premultiplying");
    assert!((120..=135).contains(&p.rgba[i + 3]), "half opacity: {}", p.rgba[i + 3]);
    // …and at four times it, drawn again (sharp), not scaled: an edge pixel stays pure.
    let p4 = decode_unaddressed(&b, at((80, 40))).unwrap();
    assert_eq!((p4.width, p4.height), (80, 40));
    assert_eq!(&p4.rgba[(39 * 80 + 39) * 4..][..4], &[255, 0, 0, 255]);
    assert_eq!(&p4.rgba[(39 * 80 + 40) * 4 + 2], &255);
}

#[test]
fn sizes_come_from_the_target_else_the_svg() {
    let vb = svg("<rect width='4' height='2'/>", "viewBox='0 0 4 2'");
    let p = decode_unaddressed(&vb, at((40, 20))).unwrap();
    assert_eq!((p.width, p.height), (40, 20));
    // With no target (the pipeline always sends one), the SVG's own size;
    // with no width, height or viewBox, usvg takes its content's extent.
    let none = svg("<rect width='4' height='2'/>", "");
    let p = decode_unaddressed(&none, Ceilings::default()).unwrap();
    assert_eq!((p.width, p.height), (4, 2));
}

#[test]
fn a_bare_doctype_is_dropped_and_a_dtd_with_entities_is_refused() {
    let mut b = b"<?xml version='1.0'?><!DOCTYPE svg PUBLIC '-//W3C//DTD SVG 1.1//EN' 'http://www.w3.org/Graphics/SVG/1.1/DTD/svg11.dtd'>".to_vec();
    b.extend(svg("<rect width='1' height='1'/>", "width='2' height='2'"));
    assert!(decode_unaddressed(&b, Ceilings::default()).is_ok());
    let laughs = format!("<?xml version='1.0'?><!DOCTYPE svg [<!ENTITY a 'aaaaaaaaaa'><!ENTITY b '&a;&a;&a;&a;&a;&a;&a;&a;&a;&a;'>]><svg {NS}><desc>&b;</desc></svg>");
    assert_eq!(refused(laughs.as_bytes()), "DTD entity");
    let subset = format!("<!DOCTYPE svg [ ]><svg {NS}/>");
    assert_eq!(refused(subset.as_bytes()), "DTD internal subset");
}

/// The attacks, each with the name it must be refused by.
fn controls() -> Vec<(Vec<u8>, &'static str)> {
    // <use> billion laughs: 10 levels of 10 uses = 10^10 nodes, in a few hundred bytes.
    let mut bomb = String::from("<defs><g id='l0'><rect width='1' height='1'/></g>");
    for i in 1..=10 { bomb.push_str(&format!("<g id='l{i}'>{}</g>", format!("<use href='#l{}'/>", i - 1).repeat(10))); }
    bomb.push_str("</defs><use href='#l10'/>");
    vec![
        (svg("<script>alert(1)</script>", ""), "script"),
        (svg("<foreignObject><div xmlns='http://www.w3.org/1999/xhtml'>x</div></foreignObject>", ""), "foreignObject"),
        (svg("<image href='https://evil.example/a.png' width='5' height='5'/>", ""), "embedded image"),
        (svg("<use href='https://evil.example/s.svg#a'/>", ""), "external reference"),
        (svg("<rect width='5' height='5' fill='url(https://evil.example/p.svg#g)'/>", ""), "external reference"),
        (svg("<style>@import url(https://evil.example/a.css);</style>", ""), "external reference"),
        (svg("<rect width='5' height='5' onclick='x()'/>", ""), "event handler"),
        (svg("<text>hello</text>", ""), "text (v1)"),
        (svg("<filter id='f'><feGaussianBlur stdDeviation='99'/></filter><rect filter='url(#f)' width='5' height='5'/>", ""), "filter"),
        (svg("<rect width='5' height='5'><animate attributeName='x' to='9'/></rect>", ""), "animation"),
        (svg(&bomb, ""), "too many nodes after use expansion"),
        (svg("<g id='a'><use href='#b'/></g><g id='b'><use href='#a'/></g>", ""), "use cycle"),
        (svg(&format!("{}{}", "<g>".repeat(70), "</g>".repeat(70)), ""), "nested too deeply"),
        (svg(&"<rect width='1' height='1'/>".repeat(10_001), ""), "too many elements"),
        (svg(&format!("<path d='{}'/>", "M0 0 L1 1 ".repeat(100_001)), ""), "too many path segments"),
        (svg(&format!("{}{}", "<g opacity='0.5'>".repeat(9), "</g>".repeat(9)), ""), "layers nested too deeply"),
        (svg("<marker id='m'/>", ""), "marker"),
        (b"<html><svg/></html>".to_vec(), "html-not-svg"),
    ]
}

#[test]
fn every_attack_is_refused_by_name_and_quickly() {
    for (b, want) in controls() {
        let t = Instant::now();
        if want == "html-not-svg" {
            assert!(matches!(decode_unaddressed(&b, Ceilings::default()), Err(DecodeError::NotARaster { kind: "html" })));
            continue;
        }
        assert_eq!(refused(&b), want);
        assert!(t.elapsed() < Duration::from_millis(500), "{want} took {:?}", t.elapsed());
    }
}

#[test]
fn pixels_are_charged_before_they_are_allocated() {
    let b = svg("<rect width='1' height='1'/>", "viewBox='0 0 1 1'");
    assert!(matches!(decode_unaddressed(&b, at((20_000, 10))), Err(DecodeError::TooLarge { format: Format::Svg, .. })));
    let c = Ceilings { budget: 1000, target: Some((100, 100)), ..Ceilings::default() };
    assert!(matches!(decode_unaddressed(&b, c), Err(DecodeError::OverBudget { format: Format::Svg, needed: 40_000, left: 1000 })));
    // Group layers each allocate a frame: charged against the transient ceiling.
    let layered = svg(&format!("{}<rect width='1' height='1'/>{}", "<g opacity='0.5'>".repeat(4), "</g>".repeat(4)), "viewBox='0 0 1 1'");
    let c = Ceilings { frame: 100 * 100 * 4 * 3, target: Some((100, 100)), ..Ceilings::default() };
    assert!(matches!(decode_unaddressed(&layered, c), Err(DecodeError::FrameTooLarge { format: Format::Svg, .. })));
    // Many nodes on a huge canvas: refused as work before drawing.
    let many = svg(&"<rect width='1' height='1'/>".repeat(9_000), "viewBox='0 0 1 1'");
    assert_eq!(match decode_unaddressed(&many, at((600, 600))) { Err(DecodeError::Refused { why, .. }) => why, o => panic!("{o:?}") },
               "too much drawing work for its size");
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 { self.0 ^= self.0 << 13; self.0 ^= self.0 >> 7; self.0 ^= self.0 << 17; self.0 }
    fn below(&mut self, n: usize) -> usize { (self.next() % n.max(1) as u64) as usize }
}

const SNIPPETS: &[&str] = &["<rect width='3' height='3'/>", "<circle r='4'/>", "<use href='#a'/>", "<g id='a'>", "</g>",
    "<script>", "<text>t</text>", "<image href='x.png'/>", " fill='url(#g)'", " fill='url(http://x/y#g)'", " href='#a'",
    "<linearGradient id='g'><stop offset='0'/></linearGradient>", "<path d='M0 0 C1 2 3 4 5 6 Z'/>", "<!ENTITY", "<!DOCTYPE svg>",
    " opacity='0.3'", "<mask id='m'/>", " mask='url(#m)'", "<foreignObject/>", "<filter/>", " onload='x'", "&amp;", "<", "\0", "é"];

fn mutate(r: &mut Rng, seed: &[u8]) -> Vec<u8> {
    let mut v = seed.to_vec();
    for _ in 0..1 + r.below(3) {
        match r.below(5) {
            0 if !v.is_empty() => { let i = r.below(v.len()); v[i] = r.next() as u8 }
            1 => { let at = r.below(v.len() + 1); v.splice(at..at, SNIPPETS[r.below(SNIPPETS.len())].bytes()); }
            2 => { let at = r.below(v.len() + 1); v.truncate(at) }
            3 if v.len() > 8 => { let i = r.below(v.len() - 4); let n = 1 + r.below(16.min(v.len() - i)); v.drain(i..i + n); }
            _ => { let at = r.below(v.len() + 1); let n = r.below(40); let s = SNIPPETS[r.below(SNIPPETS.len())]; v.splice(at..at, s.repeat(n).into_bytes()); }
        }
    }
    v
}

#[test]
fn fuzz_never_panics_stays_in_its_ceilings_and_reaches_every_outcome() {
    let mut seeds: Vec<Vec<u8>> = controls().into_iter().map(|(b, _)| b).collect();
    seeds.push(svg("<defs><linearGradient id='g'><stop offset='0' stop-color='red'/><stop offset='1'/></linearGradient></defs>\
                    <g id='a'><rect width='8' height='8' fill='url(#g)'/></g><use href='#a' x='8'/><circle cx='4' cy='4' r='3' opacity='0.5'/>",
                   "width='16' height='8'"));
    let real: Vec<Vec<u8>> = std::env::var("NAVIGATOR_SVG_CORPUS").ok().map(|ds| ds.split(':').flat_map(|d|
        std::fs::read_dir(d).expect("corpus dir").flatten().filter_map(|e| std::fs::read(e.path()).ok())
            .filter(|b| { let h = String::from_utf8_lossy(&b[..b.len().min(1024)]).to_lowercase(); h.contains("<svg") && !h.contains("<html") })
            .collect::<Vec<_>>()).collect()).unwrap_or_default();
    let mut reached: BTreeMap<String, usize> = BTreeMap::new();
    let mut check = |b: &[u8], reached: &mut BTreeMap<String, usize>| {
        let c = at((48, 32));
        let t = Instant::now();
        let r = decode_unaddressed(b, c);
        assert!(t.elapsed() < Duration::from_secs(5), "a case took {:?}", t.elapsed());
        let k = match &r {
            Ok(p) => {
                assert_eq!((p.width, p.height, p.rgba.len()), (48, 32, 48 * 32 * 4), "output not the painted size");
                "drawn".to_string()
            }
            Err(DecodeError::Refused { why, .. }) => format!("refused:{why}"),
            Err(e) => e.cause().to_string(),
        };
        *reached.entry(k).or_default() += 1;
    };
    for b in seeds.iter().chain(real.iter()) { check(b, &mut reached) }
    let iters: usize = std::env::var("NAVIGATOR_SVG_FUZZ_ITERS").ok().and_then(|v| v.parse().ok()).unwrap_or(3_000);
    let mut r = Rng(0xA5A5_5A5A_1234_8765);
    let pool: Vec<&Vec<u8>> = seeds.iter().chain(real.iter()).collect();
    for _ in 0..iters { let s = pool[r.below(pool.len())]; let m = mutate(&mut r, s); check(&m, &mut reached) }
    eprintln!("svg fuzz ({} seeds, {} real, {iters} mutants): {reached:?}", seeds.len(), real.len());
    for want in ["drawn", "corrupt", "refused:script", "refused:foreignObject", "refused:embedded image", "refused:external reference",
                 "refused:event handler", "refused:text (v1)", "refused:filter", "refused:too many nodes after use expansion",
                 "refused:use cycle", "refused:DTD entity", "not-a-raster"] {
        assert!(reached.get(want).copied().unwrap_or(0) > 0, "never reached {want}: {reached:?}");
    }
}
