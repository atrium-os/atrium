//! `FRESCOD_FRAME_STATS=<seconds>`: every that many seconds, log how often
//! frescod composed and, per window, how often a new drawing of it reached
//! the screen and the longest gaps between them. For the noisy-neighbour
//! gate (scripts/fresco-noisy-neighbour.sh, officina.md §10.5): an animating
//! window must keep reaching the screen while other clients send huge or
//! very costly scenes. Off unless set; costs a hash lookup per window per
//! frame when on.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

struct Win {
    title: String,
    key: u64,
    last_new: Instant,
    new: u32,
    gaps: Vec<f32>,
}

struct Stats {
    every: Duration,
    since: Instant,
    last: Option<Instant>,
    frames: Vec<f32>,
    wins: HashMap<u32, Win>,
}

static STATS: Mutex<Option<Stats>> = Mutex::new(None);

/// Read `FRESCOD_FRAME_STATS` (once, at start).
pub fn init() {
    let every = std::env::var("FRESCOD_FRAME_STATS").ok().and_then(|v| v.parse::<f32>().ok()).filter(|s| *s > 0.0);
    if let Some(s) = every {
        *STATS.lock().unwrap() = Some(Stats {
            every: Duration::from_secs_f32(s),
            since: Instant::now(),
            last: None,
            frames: Vec::new(),
            wins: HashMap::new(),
        });
    }
}

fn p99(v: &mut [f32]) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    v[((v.len() - 1) as f32 * 0.99).round() as usize]
}

/// One composed frame: each window shown, with a key that changes with
/// every new drawing of it, and its title.
pub fn frame(windows: impl Iterator<Item = (u32, u64, String)>) {
    let mut g = STATS.lock().unwrap();
    let Some(s) = g.as_mut() else { return };
    let now = Instant::now();
    if let Some(l) = s.last {
        s.frames.push((now - l).as_secs_f32() * 1000.0);
    }
    s.last = Some(now);
    for (id, key, title) in windows {
        let w = s.wins.entry(id).or_insert_with(|| Win { title: title.clone(), key, last_new: now, new: 0, gaps: Vec::new() });
        w.title = title;
        if w.key != key {
            w.gaps.push((now - w.last_new).as_secs_f32() * 1000.0);
            w.key = key;
            w.last_new = now;
            w.new += 1;
        }
    }
    let span = now - s.since;
    if span < s.every {
        return;
    }
    let secs = span.as_secs_f32();
    let n = s.frames.len();
    let max = s.frames.iter().copied().fold(0.0, f32::max);
    let mut line = format!("frescod: stats: {:.1} frames/s p99 {:.1} ms max {:.1} ms", n as f32 / secs, p99(&mut s.frames), max);
    let mut ids: Vec<u32> = s.wins.keys().copied().collect();
    ids.sort();
    for id in ids {
        let w = s.wins.get_mut(&id).expect("listed");
        let max = w.gaps.iter().copied().fold(0.0, f32::max);
        line += &format!(" | window {id} {:?}: {:.1} new/s gap p99 {:.1} ms max {:.1} ms", w.title, w.new as f32 / secs, p99(&mut w.gaps), max);
        w.new = 0;
        w.gaps.clear();
    }
    eprintln!("{line}");
    s.frames.clear();
    s.since = now;
}
