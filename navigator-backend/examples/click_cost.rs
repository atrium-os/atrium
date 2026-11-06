//! What a CLICK costs the worker's session, in-process: `Session::navigate`
//! (find the trigger's transition, apply it, record the trail, re-measure)
//! and `back`, over every offerable trigger of every recording. `nav_cost`
//! times `History::go` alone; this is the whole step a reader's click takes.
//!
//!   click_cost <recordings-dir>

use navigator_backend::session::{SessionLimits, Sessions};
use navigator_backend::{ingest, Limits};
use std::time::Instant;

fn main() {
    let dir = std::env::args().nth(1).expect("recordings directory");
    let (mut go_us, mut back_us) = (vec![], vec![]);
    let mut files: Vec<_> = std::fs::read_dir(&dir).expect("readable").flatten()
        .map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "json")).collect();
    files.sort();
    for p in &files {
        let bytes = std::fs::read(p).expect("readable");
        let Ok(r) = ingest(&bytes, &Limits::default()) else { continue };
        let mut sessions = Sessions::new(SessionLimits::default());
        let Ok(id) = sessions.open(&r, 0) else { continue };
        let triggers: Vec<String> = r.transitions.iter().filter(|t| t.anchored).map(|t| t.trigger.clone()).collect();
        let mut first = true;
        for trig in &triggers {
            let s = sessions.get_mut(id, 0).expect("open");
            let t0 = Instant::now();
            if s.navigate(trig).is_err() { continue }
            let took = t0.elapsed().as_secs_f64() * 1e6;
            // The first step compiles the document state once: not a click's cost.
            if first { first = false } else { go_us.push(took) }
            let t0 = Instant::now();
            if s.back().is_err() { continue }
            back_us.push(t0.elapsed().as_secs_f64() * 1e6);
        }
    }
    for (name, v) in [("click", &mut go_us), ("back", &mut back_us)] {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        if v.is_empty() { println!("{name}_us n=0"); continue }
        let q = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
        println!("{name}_us n={} p50={:.2} p90={:.2} p99={:.2} max={:.1}", v.len(), q(0.5), q(0.9), q(0.99), v[v.len() - 1]);
    }
}
