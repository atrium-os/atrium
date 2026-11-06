//! band_peak <normalized.html> <mode> [--band PX]: one process per arm, so
//! each arm's peak resident set is its own (backend §4.8 step 3, phase 2).
//!
//! Modes:
//! - `whole`: today's path — the whole layout and scene (`render_html`).
//! - `source`: the content source a worker runs (`source::DocumentSource`,
//!   bands of `--band` px, default 600, one viewport of prefetch): the first
//!   screen; a jump to the end (a `want` at the estimated extent, repeated
//!   until the extent settles, as a scrollbar dragged to the bottom does);
//!   back to the top and to the middle (each laid out from its checkpoint);
//!   then the whole document scrolled a screen at a time.
//!
//! One TSV line: `stem mode key=value…` (times in ms, memory in MiB).
use navigator_render::{fontset::FontSet, html::render_html, source::{ContentSource, DocumentSource, GroupId, SceneSink, Want}, Scene, U};
use navigator_style::cascade::Env;
use std::time::Instant;

const PX: i64 = 64;

fn peak_mib() -> f64 {
    let mut u: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut u) };
    let b = if cfg!(target_os = "macos") { u.ru_maxrss as f64 } else { u.ru_maxrss as f64 * 1024.0 };
    b / (1024.0 * 1024.0)
}

/// The resident set now (from `ps`), MiB.
fn now_mib() -> f64 {
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse::<f64>().unwrap_or(0.0) / 1024.0
}

/// A sink that keeps what a window would hold: each group's leaf count.
#[derive(Default)]
struct Sink { leaves: std::collections::BTreeMap<GroupId, usize> }
impl SceneSink for Sink {
    fn put(&mut self, g: GroupId, _: Option<(U, U)>, s: &Scene) { self.leaves.insert(g, s.order.len()); }
    fn clear(&mut self, g: GroupId) { self.leaves.remove(&g); }
}

fn ms(t: Instant) -> f64 { t.elapsed().as_secs_f64() * 1e3 }

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let html = std::fs::read_to_string(&a[0]).unwrap();
    let stem = std::path::Path::new(&a[0]).file_stem().unwrap().to_string_lossy().to_string();
    let band_px: i64 = a.iter().position(|x| x == "--band").map(|i| a[i + 1].parse().unwrap()).unwrap_or(600);
    let fonts = FontSet::load().unwrap();
    let base = peak_mib();
    match a[1].as_str() {
        "whole" => {
            let t = Instant::now();
            let s = render_html(&html, &fonts, &Env::default()).scene;
            println!("{stem}\twhole\tms={:.0}\tnodes={}\textent_px={}\tpeak={:.0}\tnow={:.0}\tbase={base:.0}", ms(t), s.order.len(), s.height / PX, peak_mib(), now_mib());
        }
        "source" => {
            let vh = 600 * PX;
            let mut sink = Sink::default();
            let t = Instant::now();
            let mut src = DocumentSource::new(html, &fonts, Default::default(), (800.0, 600.0), band_px * PX);
            let w = |y: U| Want { range: (y, y + vh), zoom_64: 64, prefetch: vh };
            src.want(w(0), &mut sink);
            let first_ms = ms(t);
            let first_peak = peak_mib();
            // Jump to the end: follow the estimate until it settles.
            let t = Instant::now();
            let mut wants = 0;
            loop {
                let (e, settled) = src.banded().extent();
                src.want(w((e - vh).max(0)), &mut sink);
                wants += 1;
                if settled && src.banded().extent().1 { break }
            }
            let end_ms = ms(t);
            let (extent, _) = src.banded().extent();
            let end_peak = peak_mib();
            let t = Instant::now();
            src.want(w(0), &mut sink);
            let top_ms = ms(t);
            let t = Instant::now();
            src.want(w(extent / 2), &mut sink);
            let mid_ms = ms(t);
            // Scroll the whole document a screen at a time.
            let mut steps: Vec<f64> = vec![];
            let (mut most, mut most_leaves) = (0usize, 0usize);
            let mut y = 0;
            while y < extent {
                let t = Instant::now();
                src.want(w(y), &mut sink);
                steps.push(ms(t));
                most = most.max(src.resident().len());
                most_leaves = most_leaves.max(sink.leaves.values().sum());
                y += vh;
            }
            steps.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let q = |p: f64| steps[((steps.len() as f64 - 1.0) * p).round() as usize];
            let st = src.banded().stats;
            println!("{stem}\tsource\tfirst_ms={first_ms:.0}\tfirst_peak={first_peak:.0}\tend_ms={end_ms:.0}\tend_wants={wants}\tend_peak={end_peak:.0}\ttop_ms={top_ms:.1}\tmid_ms={mid_ms:.1}\tbands={}\tscroll_steps={}\tstep_p50={:.1}\tstep_p99={:.1}\tstep_max={:.1}\tresident_max={most}\twindow_leaves_max={most_leaves}\tpeak={:.0}\tnow={:.0}\textent_px={}\twhole={}\tbase={base:.0}",
                     src.banded().laid_out(), steps.len(), q(0.5), q(0.99), q(1.0), peak_mib(), now_mib(), extent / PX, st.whole as u8);
        }
        m => panic!("mode {m}?"),
    }
}
