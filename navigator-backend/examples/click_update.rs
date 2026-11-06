//! ★ WHAT A CLICK'S WHOLE UPDATE COSTS (backend §4.3, replay part 2): the
//! step (`Session::navigate` / `back`), the display update
//! (`display::Bands::update`) and the next `want`'s answer — the new scene
//! of the bands that changed, as NSG — in-process, over every anchored
//! trigger of every recording. Each click is timed twice:
//!
//! - `foreseen`: idle time had laid the clicked state out ahead
//!   (`Bands::precompute`), as it does for the triggers a reader can reach;
//! - `cold`: it had not;
//!
//! and every back (the state just left is always kept). The view is the
//! page's first screen. Prints p50/p90/p99
//! per arm and per kind of update.
//!
//!   click_update <recordings-dir>...

use navigator_backend::display::{Bands, BandsAsk, UpdateKind};
use navigator_backend::session::{SessionLimits, Sessions};
use navigator_backend::{ingest, Limits};
use std::collections::BTreeMap;
use std::time::Instant;

fn main() {
    let vh = (navigator_style::cascade::Env::default().height_px * 64.0) as i64;
    let mut t: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut files = vec![];
    for dir in std::env::args().skip(1) {
        let mut f: Vec<_> = std::fs::read_dir(&dir).expect("readable").flatten()
            .map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
        f.sort();
        files.extend(f);
    }
    for p in &files {
        let bytes = std::fs::read(p).expect("readable");
        let Ok(r) = ingest(&bytes, &Limits::default()) else { continue };
        let triggers: Vec<String> = r.transitions.iter().filter(|t| t.anchored).map(|t| t.trigger.clone()).collect();
        if triggers.is_empty() { continue }
        let mut n = 0;
        for arm in ["foreseen", "cold"] {
            let mut sessions = Sessions::new(SessionLimits::default());
            let Ok(id) = sessions.open(&r, 0) else { continue };
            let (mut b, _) = Bands::open(&bytes, None, None, 0).expect("opens");
            { let s = sessions.get(id).unwrap(); b.showing(s.document(), s.state_key()); }
            // Settle, then put the view where it is read.
            for _ in 0..100_000 { b.ask(BandsAsk::Idle { bands: 64 }).unwrap(); if b.settled() { break } }
            let mut want = BandsAsk::Want { range: (0, vh), zoom_64: 64, prefetch: vh };
            b.ask(want).unwrap();
            let mut gen = 0;
            for trig in &triggers {
                // VIEW=trigger: the view where the reader sees what they
                // click (an `#id` trigger; others keep the first screen).
                if std::env::var("VIEW").as_deref() == Ok("trigger") {
                    if let Some((y0, _)) = trig.strip_prefix('#').and_then(|id| b.band_range_of_id(id)) {
                        let y = y0.max(0);
                        want = BandsAsk::Want { range: (y, y + vh), zoom_64: 64, prefetch: vh };
                        b.ask(want).unwrap();
                    }
                }
                for forward in [true, false] {
                    if forward && arm == "foreseen" {
                        let s = sessions.get(id).unwrap();
                        let next: Vec<_> = s.next_states().into_iter().filter(|(t, _)| t.trigger == *trig).collect();
                        b.precompute(&bytes, s.document(), &next, 1).unwrap();
                    }
                    gen += 1;
                    let t0 = Instant::now();
                    let s = sessions.get_mut(id, 0).unwrap();
                    let ok = if forward { s.navigate(trig).is_ok() } else { s.back().is_ok() };
                    if !ok { break }
                    let s = sessions.get(id).unwrap();
                    let k = b.update(&bytes, s.document(), s.state_key(), gen).unwrap();
                    let a = b.ask(want).unwrap();
                    let ms = t0.elapsed().as_secs_f64() * 1e3;
                    drop(a);
                    let dir = if forward { arm } else { "back" };
                    t.entry(format!("{dir}")).or_default().push(ms);
                    if ms > 16.0 && k != UpdateKind::Miss && std::env::var_os("SLOW").is_some() { eprintln!("SLOW {} {trig} {dir} {k:?} {ms:.1} ms", p.file_name().unwrap().to_string_lossy()) }
                    t.entry(format!("{dir} {k:?}")).or_default().push(ms);
                    if k == UpdateKind::Hit || k == UpdateKind::Inert { t.entry("any hit or inert".into()).or_default().push(ms) }
                    // The reader reads (idle work settles the layout again,
                    // and lays the state just left out for going back, as the
                    // worker does on its idle asks).
                    for _ in 0..100_000 { b.ask(BandsAsk::Idle { bands: 64 }).unwrap(); if b.settled() { break } }
                    b.prepare_back();
                    b.ask(want).unwrap();
                    n += 1;
                }
            }
            if arm == "cold" { eprintln!("{} steps {n} {:?}", p.file_name().unwrap().to_string_lossy(), b.counts) }
        }
    }
    for (name, v) in &mut t {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let q = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
        println!("{name:>16} ms n={:>4} p50={:>8.3} p90={:>8.3} p99={:>8.3} max={:>8.1}", v.len(), q(0.5), q(0.9), q(0.99), v[v.len() - 1]);
    }
}
