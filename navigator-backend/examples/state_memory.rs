//! What a laid-out state kept by the display cache costs in memory (backend
//! §4.3, replay part 2): normalize a recording once, then hold N sources
//! built on it and primed for the first screen. Run under `/usr/bin/time
//! -l` with N = 1 and N = 7: the difference over 6 is one state.
//!
//!   state_memory <recording.json> <N>
use navigator_render::fontset::FontSet;
use navigator_render::source::{DocumentSource, Want};
use navigator_style::cascade::Env;
fn main() {
    let mut a = std::env::args().skip(1);
    let (p, n) = (a.next().expect("recording"), a.next().expect("N").parse::<usize>().expect("N"));
    let bytes = std::fs::read(&p).expect("readable");
    let json = std::str::from_utf8(&bytes).unwrap();
    let fonts = FontSet::shared().expect("fonts");
    let env = Env::default();
    let rec = navigator_normalize::recording::parse(json).expect("parses");
    let html = navigator_normalize::normalize_and_measure(&rec.document, &rec.inputs, fonts, &env).0;
    drop(rec);
    let subs = navigator_render::conformance::subresources_from_recording_json(json);
    let vh = (env.height_px * 64.0) as i64;
    let mut kept = vec![];
    for _ in 0..n {
        let prep = navigator_render::html::prepare(&html, fonts, &env);
        let mut s = DocumentSource::from_prepared(html.clone(), prep, fonts, subs.clone(), (env.width_px, env.height_px), vh, 64, false);
        s.prime(Want { range: (0, vh), zoom_64: 64, prefetch: vh });
        kept.push(s);
    }
    eprintln!("normalized {} bytes, {} states", html.len(), kept.len());
}
