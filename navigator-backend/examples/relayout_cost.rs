//! What INCREMENTAL RELAYOUT saves (backend §4.3, replay part 2): for every
//! step and rewind of every recording, the new state normalized and
//! prepared (not timed), then, timed:
//!
//! - `incremental`: `DocumentSource::replace` on the settled source — the
//!   layout redone only from the checkpoint above the change, and only the
//!   bands that changed sent (written as NSG);
//! - `full`: a source built on the new state and laid out to the same view
//!   (`from_prepared` + `want`, every band it shows written as NSG).
//!
//! The view is where the reader sees what they click (an `#id` trigger),
//! else the first screen.
//!
//!   relayout_cost <recordings-dir>...

use navigator_backend::session::{SessionLimits, Sessions};
use navigator_backend::{ingest, Limits};
use navigator_render::fontset::FontSet;
use navigator_render::source::{ContentSource, DocumentSource, GroupId, SceneSink, Want};
use navigator_style::cascade::Env;
use std::time::Instant;

struct Nsg<'a> { fonts: &'a FontSet, n: usize }
impl SceneSink for Nsg<'_> {
    fn put(&mut self, _: GroupId, _: Option<(i64, i64)>, s: &navigator_render::Scene) { self.n += navigator_render::nsg::write(s, self.fonts).len() }
    fn clear(&mut self, _: GroupId) {}
}

fn main() {
    let fonts = FontSet::shared().expect("fonts");
    let env = Env::default();
    let vh = (env.height_px * 64.0) as i64;
    let (mut inc, mut full, mut from) = (vec![], vec![], vec![]);
    let mut files = vec![];
    for dir in std::env::args().skip(1) {
        let mut f: Vec<_> = std::fs::read_dir(&dir).expect("readable").flatten().map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
        f.sort();
        files.extend(f);
    }
    for p in &files {
        let bytes = std::fs::read(p).expect("readable");
        let r = match ingest(&bytes, &Limits::default()) { Ok(r) => r, Err(e) => { eprintln!("{}: {e}", p.display()); continue } };
        let triggers: Vec<String> = r.transitions.iter().filter(|t| t.anchored).map(|t| t.trigger.clone()).collect();
        if triggers.is_empty() { continue }
        let json = std::str::from_utf8(&bytes).unwrap();
        let rec = navigator_normalize::recording::parse(json).expect("parses");
        let subs = navigator_render::conformance::subresources_from_recording_json(json);
        let norm = |html: &str| navigator_normalize::normalize_and_measure(html, &rec.inputs, fonts, &env).0;
        let mut sessions = Sessions::new(SessionLimits::default());
        let id = match sessions.open(&r, 0) { Ok(id) => id, Err(e) => { eprintln!("{}: {e}", p.display()); continue } };
        let mut src = DocumentSource::new(norm(&rec.document), fonts, subs.clone(), (env.width_px, env.height_px), vh);
        let mut sink = Nsg { fonts, n: 0 };
        src.idle(1 << 20, &mut sink);
        let mut n = 0;
        for trig in &triggers {
            let y = trig.strip_prefix('#').and_then(|i| src.banded().band_of_id(i)).map(|k| src.banded().bands[k].range.0.max(0)).unwrap_or(0);
            let want = Want { range: (y, y + vh), zoom_64: 64, prefetch: vh };
            src.want(want, &mut sink);
            for forward in [true, false] {
                let s = sessions.get_mut(id, 0).expect("open");
                let ok = if forward { s.navigate(trig).is_ok() } else { s.back().is_ok() };
                if !ok { break }
                let html = norm(&s.document().dom.serialize());
                let e = Env::zoomed(env.width_px, env.height_px, 64);
                let (p1, p2) = (navigator_render::html::prepare(&html, fonts, &e), navigator_render::html::prepare(&html, fonts, &e));
                let t0 = Instant::now();
                let mut s1 = Nsg { fonts, n: 0 };
                src.replace(html.clone(), Some(p1), &mut s1);
                inc.push(t0.elapsed().as_secs_f64() * 1e3);
                from.push(src.replaced_from().map(|k| k as f64).unwrap_or(-1.0));
                let t0 = Instant::now();
                let mut f = DocumentSource::from_prepared(html, p2, fonts, subs.clone(), (env.width_px, env.height_px), vh, 64, false);
                let mut s2 = Nsg { fonts, n: 0 };
                // Exact: the first pass as far as the view (no provisional band).
                let need = src.banded().band_at(y).map(|k| k + 1).unwrap_or(0);
                if need > 0 { f.idle(need, &mut s2); }
                f.want(want, &mut s2);
                full.push(t0.elapsed().as_secs_f64() * 1e3);
                if std::env::var_os("STEPS").is_some() {
                    eprintln!("{} {trig} {} inc {:.1} full {:.1} from {:?} laid {} of y {} fresh laid {}", p.file_name().unwrap().to_string_lossy(), if forward { "go" } else { "back" },
                        inc[inc.len() - 1], full[full.len() - 1], src.replaced_from(), src.banded().laid_out(), y / 64, f.banded().laid_out());
                }
                // The reader reads: the layout settles again.
                src.idle(1 << 20, &mut sink);
                n += 1;
            }
        }
        eprintln!("{} steps {n}", p.file_name().unwrap().to_string_lossy());
    }
    for (name, v) in [("incremental", &mut inc), ("full", &mut full)] {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let q = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
        println!("{name:>12} ms n={} p50={:.3} p90={:.3} p99={:.3} max={:.1}", v.len(), q(0.5), q(0.9), q(0.99), v[v.len() - 1]);
    }
}
