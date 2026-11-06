//! What a click's DISPLAY update costs (backend §4.3, replay part 2): after
//! `Session::navigate` applies a transition, the new scene for the view —
//! serialize the document, normalize it with the recording's inputs, prepare
//! (parse + the flat cascade), and lay out and write the bands the view
//! holds. Phase by phase, per click, over every anchored trigger of every
//! recording (each click, then back).
//!
//!   update_cost <recordings-dir>...      (env VIEW_Y=<px> puts the view there)

use navigator_backend::session::{SessionLimits, Sessions};
use navigator_backend::{ingest, Limits};
use navigator_render::fontset::FontSet;
use navigator_render::source::{self, ContentSource, GroupId, Want};
use navigator_style::cascade::Env;
use std::collections::BTreeMap;
use std::time::Instant;

struct Sink<'a> { fonts: &'a FontSet, bytes: usize }
impl source::SceneSink for Sink<'_> {
    fn put(&mut self, _: GroupId, _: Option<(i64, i64)>, s: &navigator_render::Scene) { self.bytes += navigator_render::nsg::write(s, self.fonts).len() }
    fn clear(&mut self, _: GroupId) {}
}

fn main() {
    let fonts = FontSet::shared().expect("fonts");
    let env = Env::default();
    let view_y: i64 = std::env::var("VIEW_Y").ok().and_then(|v| v.parse().ok()).unwrap_or(0) * 64;
    let vh = (env.height_px * 64.0) as i64;
    let mut t: BTreeMap<&'static str, Vec<f64>> = BTreeMap::new();
    let mut files = vec![];
    for dir in std::env::args().skip(1) {
        let mut f: Vec<_> = std::fs::read_dir(&dir).expect("readable").flatten()
            .map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
        f.sort();
        files.extend(f);
    }
    for p in &files {
        let bytes = std::fs::read(p).expect("readable");
        let r = match ingest(&bytes, &Limits::default()) { Ok(r) => r, Err(e) => { eprintln!("{}: {e}", p.display()); continue } };
        let triggers: Vec<String> = r.transitions.iter().filter(|t| t.anchored).map(|t| t.trigger.clone()).collect();
        if triggers.is_empty() { continue }
        let json = std::str::from_utf8(&bytes).unwrap();
        let mut sessions = Sessions::new(SessionLimits::default());
        let id = match sessions.open(&r, 0) { Ok(id) => id, Err(e) => { eprintln!("{}: open: {e}", p.display()); continue } };
        let subs = navigator_render::conformance::subresources_from_recording_json(json);
        let mut n = 0;
        let mut prev: Option<String> = None;
        let (mut same, mut diff_frac) = (0usize, vec![]);
        let max_steps: usize = std::env::var("MAX_STEPS").ok().and_then(|v| v.parse().ok()).unwrap_or(usize::MAX);
        for trig in triggers.iter().take(max_steps) {
            for dir in [true, false] {
                let s = sessions.get_mut(id, 0).expect("open");
                let ok = if dir { s.navigate(trig).map_err(|e| e.to_string()) } else { s.back().map(|_| ()).map_err(|e| e.to_string()) };
                if let Err(e) = ok { eprintln!("{}: step refused: {e}", p.display()); break }
                let t0 = Instant::now();
                let cur = s.document().dom.serialize();
                let t1 = Instant::now();
                let rec = navigator_normalize::recording::parse(json).expect("parses");
                let t2 = Instant::now();
                navigator_normalize::phases::start();
                let (norm, _) = navigator_normalize::normalize_and_measure(&cur, &rec.inputs, fonts, &env);
                let laps = navigator_normalize::phases::take();
                let t3 = Instant::now();
                if let Some(p) = &prev {
                    if *p == norm { same += 1 } else {
                        let (a, b) = (p.as_bytes(), norm.as_bytes());
                        let pre = a.iter().zip(b).take_while(|(x, y)| x == y).count();
                        let suf = a.iter().rev().zip(b.iter().rev()).take_while(|(x, y)| x == y).count().min(a.len().min(b.len()) - pre);
                        let span = b.len() - pre - suf;
                        diff_frac.push((pre as f64 / b.len() as f64, span));
                    }
                }
                prev = Some(norm.clone());
                let mut src = source::DocumentSource::new(norm, fonts, subs.clone(), (env.width_px, env.height_px), vh);
                let t4 = Instant::now();
                let mut sink = Sink { fonts, bytes: 0 };
                src.want(Want { range: (view_y, view_y + vh), zoom_64: 64, prefetch: vh }, &mut sink);
                let t5 = Instant::now();
                let us = |a: Instant, b: Instant| (b - a).as_secs_f64() * 1e3;
                t.entry("serialize").or_default().push(us(t0, t1));
                t.entry("recording").or_default().push(us(t1, t2));
                t.entry("normalize").or_default().push(us(t2, t3));
                for (name, d) in laps { t.entry(name).or_default().push(d.as_secs_f64() * 1e3) }
                t.entry("prepare").or_default().push(us(t3, t4));
                t.entry("layout+nsg").or_default().push(us(t4, t5));
                t.entry("total").or_default().push(us(t0, t5));
                n += 1;
            }
        }
        eprintln!("{} steps {n} same {same} diffs {:?}", p.file_name().unwrap().to_string_lossy(), diff_frac.iter().map(|(f, s)| format!("{:.2}@{s}", f)).collect::<Vec<_>>());
    }
    for (name, v) in &mut t {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let q = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
        println!("{name:>12} ms n={} p50={:.2} p90={:.2} p99={:.2} max={:.1}", v.len(), q(0.5), q(0.9), q(0.99), v[v.len() - 1]);
    }
}
