//! ★ THE CONTENT SOURCE (pergola.md §10.4): `want` in; `extent`,
//! `band_ready`, `band_updated`, `band_evicted` out; one scene group per
//! band and an overlay group, written to a sink; only the wanted range plus
//! prefetch resident.

use navigator_render::{fontset::FontSet, html::render_html, source::{ContentSource, DocumentSource, Event, GroupId, SceneSink, Want}, Scene, U};
use navigator_style::cascade::Env;
use std::collections::BTreeMap;

const PX: i64 = 64;

#[derive(Default)]
struct Sink { groups: BTreeMap<GroupId, (Option<(U, U)>, usize)>, puts: usize, clears: usize }
impl SceneSink for Sink {
    fn put(&mut self, g: GroupId, clip: Option<(U, U)>, s: &Scene) { self.groups.insert(g, (clip, s.order.len())); self.puts += 1 }
    fn clear(&mut self, g: GroupId) { self.groups.remove(&g); self.clears += 1 }
}

fn doc(n: usize) -> String {
    let mut b = String::from(r#"<html><head><style>body { margin: 8px } p { margin: 0px 0px 10px }
        section { background-color: #eef4ff; padding: 6px; margin: 0px 0px 12px }
        .tag { position: relative; top: -4px; background-color: #ffeecc }</style></head><body>"#);
    for s in 0..n {
        b.push_str("<section>");
        for i in 0..6 { b.push_str(&format!("<p>Paragraph {s}.{i}: words enough to wrap once across the width of the page at the default size here.</p>")) }
        b.push_str(&format!(r#"<div class="tag">Tag {s}</div></section>"#));
    }
    b.push_str("</body></html>");
    b
}

fn bands_in(sink: &Sink) -> Vec<u32> { sink.groups.keys().filter_map(|g| match g { GroupId::Band(b) => Some(*b), _ => None }).collect() }

#[test]
fn only_the_wanted_range_and_prefetch_stay_resident() {
    let fonts = FontSet::load().unwrap();
    let html = doc(60);
    let full = render_html(&html, &fonts, &Env::default()).scene;
    let mut src = DocumentSource::new(html, &fonts, Default::default(), (800.0, 600.0), 600 * PX);
    let mut sink = Sink::default();
    // The first screen.
    let ev = src.want(Want { range: (0, 600 * PX), zoom_64: 64, prefetch: 600 * PX }, &mut sink);
    assert!(ev.iter().any(|e| matches!(e, Event::BandReady { band: 0, .. })), "{ev:?}");
    assert!(ev.iter().any(|e| matches!(e, Event::Extent { settled: false, .. })), "the extent is an estimate at first: {ev:?}");
    assert!(src.banded().laid_out() <= 3, "only what the range reaches is laid out: {}", src.banded().laid_out());
    // Scroll down the document a screen at a time: resident stays bounded.
    let mut y = 0;
    let mut most = 0;
    while y < full.height {
        src.want(Want { range: (y, y + 600 * PX), zoom_64: 64, prefetch: 600 * PX }, &mut sink);
        let r = src.resident();
        most = most.max(r.len());
        for &k in &r {
            let (a, b) = src.banded().bands[k].range;
            assert!(b > y - 600 * PX && a < y + 1200 * PX, "band {k} {a}..{b} is outside want + prefetch at {y}");
        }
        assert_eq!(bands_in(&sink), r.iter().map(|k| *k as u32).collect::<Vec<_>>(), "the sink holds exactly the resident bands");
        y += 600 * PX;
    }
    assert!(most <= 5, "at most {most} bands resident");
    assert!(sink.clears > 0, "bands were evicted");
}

#[test]
fn a_jump_to_the_end_settles_the_extent_and_shows_the_last_band() {
    let fonts = FontSet::load().unwrap();
    let html = doc(40);
    let full = render_html(&html, &fonts, &Env::default()).scene;
    let mut src = DocumentSource::new(html, &fonts, Default::default(), (800.0, 600.0), 600 * PX);
    let mut sink = Sink::default();
    src.want(Want { range: (0, 600 * PX), zoom_64: 64, prefetch: 0 }, &mut sink);
    let end = full.height;
    let mut ev = src.want(Want { range: (end - 600 * PX, end), zoom_64: 64, prefetch: 0 }, &mut sink);
    assert!(ev.iter().any(|e| matches!(e, Event::BandEvicted { band: 0 })), "the first screen left: {ev:?}");
    // Far ahead of the first pass, the view gets a provisional band at once;
    // idle work settles the document and the exact last band replaces it.
    for _ in 0..100 {
        if ev.iter().any(|e| matches!(e, Event::Extent { settled: true, .. })) { break }
        ev.extend(src.idle(8, &mut sink));
    }
    let last = src.banded().laid_out() as u32 - 1;
    assert!(ev.iter().any(|e| matches!(e, Event::BandReady { band, .. } if *band == last)), "{ev:?}");
    assert!(ev.iter().any(|e| *e == Event::Extent { height: full.height, settled: true }), "{ev:?}");
    assert!(bands_in(&sink).iter().all(|b| *b < navigator_render::source::PROVISIONAL), "no provisional band left");
    // Back to the top: laid out again from its checkpoint.
    let before = src.banded().stats.reruns;
    src.want(Want { range: (0, 600 * PX), zoom_64: 64, prefetch: 0 }, &mut sink);
    assert!(src.banded().stats.reruns > before);
    assert_eq!(bands_in(&sink), vec![0]);
}

#[test]
fn idle_work_settles_the_document_without_making_it_resident() {
    let fonts = FontSet::load().unwrap();
    let html = doc(30);
    let mut src = DocumentSource::new(html, &fonts, Default::default(), (800.0, 600.0), 600 * PX);
    let mut sink = Sink::default();
    src.want(Want { range: (0, 600 * PX), zoom_64: 64, prefetch: 0 }, &mut sink);
    let mut settled = false;
    for _ in 0..100 {
        let ev = src.idle(4, &mut sink);
        if ev.iter().any(|e| matches!(e, Event::Extent { settled: true, .. })) { settled = true; break }
    }
    assert!(settled);
    assert!(src.resident().len() <= 2, "{:?}", src.resident());
}

#[test]
fn a_new_zoom_lays_the_document_out_again() {
    let fonts = FontSet::load().unwrap();
    let html = doc(20);
    let mut src = DocumentSource::new(html, &fonts, Default::default(), (800.0, 600.0), 600 * PX);
    let mut sink = Sink::default();
    src.want(Want { range: (0, 600 * PX), zoom_64: 64, prefetch: 0 }, &mut sink);
    let h1 = src.banded().extent().0;
    let ev = src.want(Want { range: (0, 300 * PX), zoom_64: 128, prefetch: 0 }, &mut sink);
    assert!(ev.iter().any(|e| matches!(e, Event::BandEvicted { band: 0 })), "{ev:?}");
    assert!(ev.iter().any(|e| matches!(e, Event::BandReady { band: 0, .. })), "{ev:?}");
    for _ in 0..100 { src.idle(8, &mut sink); }
    assert!(src.banded().extent().0 > h1, "at 200% the page is laid out at half the width, so it is taller");
}

#[derive(Default)]
struct Texts { groups: BTreeMap<GroupId, Vec<String>> }
impl SceneSink for Texts {
    fn put(&mut self, g: GroupId, _: Option<(U, U)>, s: &Scene) { self.groups.insert(g, s.runs.iter().map(|r| r.text.clone()).collect()); }
    fn clear(&mut self, g: GroupId) { self.groups.remove(&g); }
}

#[test]
fn a_far_jump_shows_a_provisional_band_at_once_and_the_exact_one_later() {
    use navigator_render::source::PROVISIONAL;
    let fonts = FontSet::load().unwrap();
    let html = doc(400);
    let full = render_html(&html, &fonts, &Env::default()).scene;
    let mut src = DocumentSource::new(html, &fonts, Default::default(), (800.0, 600.0), 600 * PX);
    let mut sink = Texts::default();
    src.want(Want { range: (0, 600 * PX), zoom_64: 64, prefetch: 600 * PX }, &mut sink);
    // Three quarters of the way down, long before the first pass is there.
    let y = full.height * 3 / 4;
    let ev = src.want(Want { range: (y, y + 600 * PX), zoom_64: 64, prefetch: 600 * PX }, &mut sink);
    let prov = ev.iter().find_map(|e| match e { Event::BandReady { band, range } if *band >= PROVISIONAL => Some((*band, *range)), _ => None })
        .unwrap_or_else(|| panic!("a provisional band: {ev:?}"));
    assert!(prov.1 .0 <= y && prov.1 .1 >= y + 600 * PX, "it covers the wanted rows: {:?}", prov.1);
    assert!(src.banded().laid_out() < 20, "without laying out what is above: {} bands", src.banded().laid_out());
    // Its text is the document's, from about where the reader jumped.
    let texts = &sink.groups[&GroupId::Band(prov.0)];
    let n: Vec<usize> = texts.iter().filter_map(|t| t.trim_end_matches(':').split('.').next().and_then(|v| v.parse().ok())).collect();
    assert!(!n.is_empty(), "it holds the document's paragraphs: {texts:?}");
    assert!(n.iter().any(|&s| (240..360).contains(&s)), "from about three quarters through: {n:?}");
    // Idle work reaches it: the provisional band goes and exact ones come.
    let mut evicted = false;
    for _ in 0..400 {
        let ev = src.idle(8, &mut sink);
        if ev.iter().any(|e| *e == Event::BandEvicted { band: prov.0 }) { evicted = true; break }
    }
    assert!(evicted, "the exact bands replaced it");
    assert!(!sink.groups.keys().any(|g| matches!(g, GroupId::Band(b) if *b >= PROVISIONAL)));
    assert!(sink.groups.keys().any(|g| matches!(g, GroupId::Band(b) if *b < PROVISIONAL)));
}
