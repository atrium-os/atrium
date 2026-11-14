//! scroll_cost [ROWS] [SECS] [--hz 60|120] [--pattern NAME|all]
//!
//! The O2 scroll gate, headless: atrium-sheets' own view (toolbar, formula bar, the
//! `GridView` over a ROWS-row sheet, default 1,048,576) driven by the scripted input of
//! `atrium_sheets::bench` on a virtual clock. Each vsync the app's real work is run and
//! timed (input, `on_frame` physics, render, diff); a frame shows at the first vsync
//! after its work ends, and input waits while a frame is being made. Reports the
//! ScrollView gates (pergola.md §10): p99.9 frame time ≤ 2 periods, ≤ 1 % missed
//! vsyncs while moving, no stall over 100 ms.
//!
//! Also checked: no viewport offset past what frescod accepts (a group
//! translated at most ±32,768 device px), which a live run would refuse.
//!
//! Not modelled: the socket, frescod's compose (a scroll frame is a shift plus the
//! exposed strips; fresco-compute's shift tests and the live run cover it). The live
//! run is `atrium-sheets --bench` on frescod.

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use atrium_sheets::app::{Sheets, Ui};
use atrium_sheets::bench::{self, Frame, Pattern, Report};
use pergola::geom::Point;
use pergola::widgets::{GridState, ScrollState};
use pergola::{App, Mutable};

fn main() {
    let mut args = std::env::args().skip(1).peekable();
    let mut rows: u32 = 1 << 20;
    let mut secs = 8.0;
    let mut hzs = vec![60.0, 120.0];
    let mut patterns = Pattern::ALL.to_vec();
    let mut pos = 0;
    while let Some(a) = args.next() {
        match a.as_str() {
            "--hz" => hzs = vec![args.next().and_then(|s| s.parse().ok()).expect("--hz N")],
            "--pattern" => {
                let p = args.next().expect("--pattern NAME");
                if p != "all" {
                    patterns = vec![Pattern::parse(&p).expect("swipe|diagonal|wheel|page|jump")];
                }
            }
            s => {
                match pos {
                    0 => rows = s.parse().expect("ROWS"),
                    _ => secs = s.parse().expect("SECS"),
                }
                pos += 1;
            }
        }
    }
    let t0 = Instant::now();
    let doc = bench::big_sheet(rows);
    println!("sheet {rows} rows × 10 columns built in {:.0} ms, {} MB of cells", t0.elapsed().as_secs_f64() * 1e3, doc.wb.storage_bytes() / (1 << 20));
    let (w, h) = (1100.0, 720.0);
    let dirty = Arc::new(AtomicBool::new(true));
    let sheets = Sheets::new(
        doc,
        Mutable::with_dirty(ScrollState::default(), dirty.clone()),
        Mutable::with_dirty(GridState::default(), dirty.clone()),
        Mutable::with_dirty(Ui::default(), dirty.clone()),
        Mutable::with_dirty(0u64, dirty.clone()),
        (w, h),
    );
    let mut app = App::new_with_flag(sheets.clone(), dirty);
    let t = Instant::now();
    let first = app.tick();
    println!("first frame: {} nodes in {:.1} ms", first.len(), t.elapsed().as_secs_f64() * 1e3);

    let g = Sheets::grid_rect(w, h);
    let at = Point::new(g.x() + g.w() * 0.5, g.y() + g.h() * 0.5);
    let mut all = true;
    // App time only goes forward: each run starts after the last.
    let mut base = 0.0;
    for &hz in &hzs {
        let period = 1000.0 / hz;
        for &p in &patterns {
            // Each run starts fresh at the top left (a fling left going would carry
            // into the next run): the same document, a new view state.
            let dirty = Arc::new(AtomicBool::new(true));
            let fresh = Sheets {
                scroll: Mutable::with_dirty(ScrollState::default(), dirty.clone()),
                grid: Mutable::with_dirty(GridState::default(), dirty.clone()),
                ui: Mutable::with_dirty(Ui::default(), dirty.clone()),
                rev: Mutable::with_dirty(0u64, dirty.clone()),
                ..sheets.clone()
            };
            let (scroll, grid) = (fresh.scroll.clone(), fresh.grid.clone());
            let mut app = App::new_with_flag(fresh, dirty);
            app.set_time(base);
            app.tick();
            let mut events = bench::script(p, secs, at).into_iter().peekable();
            let n = (secs * hz) as usize;
            let mut frames = Vec::new();
            let mut moving = Vec::with_capacity(n);
            let mut busy_until = 0.0f64;
            let mut most_nodes = 0;
            let mut out_of_bounds = 0;
            let mut deltas = Vec::new();
            for v in 0..n {
                let now = v as f64 * period;
                let t_app = base + now;
                if now < busy_until {
                    moving.push(true);
                    continue;
                }
                let start = Instant::now();
                let mut input = false;
                while events.peek().is_some_and(|(t, _)| *t <= now) {
                    let (_, ev) = events.next().unwrap();
                    app.set_time(t_app);
                    app.handle_event(ev);
                    input = true;
                }
                let wants = input || app.wants_frame() || app.is_dirty();
                moving.push(wants);
                if !wants {
                    continue;
                }
                app.set_time(t_app);
                app.frame();
                let d = app.tick();
                let cost = start.elapsed().as_secs_f64() * 1e3;
                deltas.push(d.len());
                most_nodes = most_nodes.max(app.tree().iter().count());
                // What frescod would refuse: a viewport translated past
                // ±32,768 device px (at scale 2).
                for (_, n) in app.tree().iter() {
                    if let pergola::node::Node::Scroll { offset, offset_x, .. } = n {
                        if offset.abs() * 2.0 > 32768.0 || offset_x.abs() * 2.0 > 32768.0 {
                            out_of_bounds += 1;
                        }
                    }
                }
                frames.push(Frame { start: now, cost });
                busy_until = now + cost;
            }
            base += secs * 1000.0 + 1000.0;
            let r = Report::new(&frames, period, &moving);
            all &= out_of_bounds == 0;
            if out_of_bounds > 0 {
                println!("    {out_of_bounds} viewport offsets past frescod's bound");
            }
            all &= r.passes(period);
            deltas.sort_unstable();
            let st = scroll.get();
            let cur = grid.get_cloned().cursor;
            println!(
                "{hz:>3} Hz {:<8} {}  | nodes ≤ {most_nodes}, deltas p99 {} | ended at y {:.0} px x {:.0} px, cell R{}C{}",
                p.name(),
                r.line(period),
                deltas.get(deltas.len().saturating_sub(1).min(deltas.len() * 99 / 100)).copied().unwrap_or(0),
                st.physics.y,
                st.physics_x.y,
                cur.0 + 1,
                cur.1 + 1,
            );
            for (s, len) in &r.stalls {
                println!("    stall at {s:.0} ms for {len:.0} ms");
            }
        }
    }
    println!("{}", if all { "ALL PASS" } else { "SOME FAIL" });
}
