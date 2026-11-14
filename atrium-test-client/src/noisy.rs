//! atrium-noisy — the noisy-neighbour clients (officina.md §10.5;
//! scripts/fresco-noisy-neighbour.sh).
//!
//! Usage: atrium-noisy SOCK MODE SECONDS
//!
//! - `nodes`: a window that sends a million rect nodes (10,000 per frame)
//!   and keeps adding. Its renderer must refuse past the node budget and
//!   tell it (`EV_BUDGET_REFUSED`), not grow until it is killed.
//! - `fills`: a 1280x800 window of 65,536 overlapping full-window
//!   translucent fills in one group whose opacity changes every frame, so
//!   every frame rasters all of them.
//! - `victim`: a small window that animates a rect at 60 frames a second,
//!   titled `noisy-victim`; frescod's `FRESCOD_FRAME_STATS` says how often
//!   its drawings reach the screen.
//!
//! Each prints one `noisy: ...` summary line at the end; `nodes` and
//! `fills` also print every refusal they are told about.

use fresco_client::{Connection, Event};
use fresco_protocol::{fill_verb as v, FillPaint, FillParams, GroupParams, RectParams, WindowHints};
use std::time::{Duration, Instant};

fn rect(x: f32, y: f32, w: f32, h: f32, c: [f32; 4]) -> RectParams {
    RectParams { x, y, w, h, r: c[0], g: c[1], b: c[2], a: c[3], radius: 0.0 }
}

#[derive(Default)]
struct Told {
    refusals: u32,
    first: Option<(u8, u64, u64)>,
}

fn drain(conn: &mut Connection, told: &mut Told) -> std::io::Result<()> {
    while let Some(ev) = conn.poll_event()? {
        if let Event::BudgetRefused { window_id, resource, id, used, limit } = ev {
            told.refusals += 1;
            told.first.get_or_insert((resource, used, limit));
            println!("noisy: refused window {window_id} resource {resource} id {id} used {used} limit {limit}");
        }
    }
    Ok(())
}

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let sock = args.next().unwrap_or_else(|| "/tmp/frescod.sock".to_string());
    let mode = args.next().unwrap_or_else(|| "victim".to_string());
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(30);
    let end = Instant::now() + Duration::from_secs(secs);
    let mut conn = Connection::connect(&sock)?;
    let mut told = Told::default();
    match mode.as_str() {
        "nodes" => {
            let win = conn.window_create(800, 600, "noisy-nodes", WindowHints::default())?;
            conn.set_default_window(win as u16);
            let mut sent = 0u32;
            while sent < 1_000_000 && Instant::now() < end {
                conn.scene_frame_begin()?;
                for _ in 0..10_000 {
                    sent += 1;
                    let (x, y) = ((sent % 400) as f32 * 2.0, ((sent / 400) % 300) as f32 * 2.0);
                    conn.scene_node_rect(sent, rect(x, y, 2.0, 2.0, [0.2, 0.5, 0.9, 1.0]))?;
                }
                conn.scene_frame_end()?;
                drain(&mut conn, &mut told)?;
            }
            // Keep at it: one more node, and one it has, every 50 ms.
            let mut k = 0u32;
            while Instant::now() < end {
                conn.scene_frame_begin()?;
                sent += 1;
                conn.scene_node_rect(sent, rect(0.0, 0.0, 4.0, 4.0, [1.0, 0.0, 0.0, 1.0]))?;
                k += 1;
                conn.scene_node_rect(1, rect((k % 400) as f32, 0.0, 8.0, 8.0, [1.0, 1.0, 0.0, 1.0]))?;
                conn.scene_frame_end()?;
                drain(&mut conn, &mut told)?;
                std::thread::sleep(Duration::from_millis(50));
            }
            println!("noisy: nodes sent {sent}, refusals told {}, first {:?}, still connected", told.refusals, told.first);
        }
        "fills" => {
            let (w, h) = (1280.0, 800.0);
            let win = conn.window_create(w as u32, h as u32, "noisy-fills", WindowHints::default())?;
            conn.set_default_window(win as u16);
            let group = |opacity: f32| GroupParams { parent: None, order: 0, transform: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0], clip: None, opacity, flags: 0 };
            conn.scene_frame_begin()?;
            conn.scene_group(1, group(1.0))?;
            for i in 0..65_536u32 {
                let f = FillParams {
                    rule: 0,
                    transform: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
                    paint: FillPaint::Solid { r: (i % 7) as f32 / 7.0, g: 0.3, b: 0.6, a: 0.02 },
                    opacity: 1.0,
                    clip: None,
                    verbs: vec![v::MOVE, v::LINE, v::LINE, v::LINE, v::CLOSE],
                    points: vec![[0.0, 0.0], [w, 0.0], [w, h], [0.0, h]],
                };
                let id = 10 + i;
                conn.scene_node_fill(id, &f)?;
                conn.scene_place(id, Some(1), i)?;
                if i % 4096 == 4095 {
                    conn.scene_frame_end()?;
                    drain(&mut conn, &mut told)?;
                    conn.scene_frame_begin()?;
                }
            }
            conn.scene_frame_end()?;
            let mut frames = 0u32;
            while Instant::now() < end {
                frames += 1;
                conn.scene_frame_begin()?;
                conn.scene_group(1, group(0.5 + 0.5 * ((frames % 60) as f32 / 60.0)))?;
                conn.scene_frame_end()?;
                drain(&mut conn, &mut told)?;
                std::thread::sleep(Duration::from_millis(16));
            }
            println!("noisy: fills frames {frames}, refusals told {}, first {:?}, still connected", told.refusals, told.first);
        }
        _ => {
            let win = conn.window_create(320, 200, "noisy-victim", WindowHints::default())?;
            conn.set_default_window(win as u16);
            let period = Duration::from_nanos(1_000_000_000 / 60);
            let mut next = Instant::now();
            let mut frames = 0u32;
            while Instant::now() < end {
                frames += 1;
                let x = (frames % 120) as f32 * 2.0;
                conn.scene_frame_begin()?;
                conn.scene_node_rect(1, rect(0.0, 0.0, 320.0, 200.0, [0.95, 0.95, 0.92, 1.0]))?;
                conn.scene_node_rect(2, rect(x, 80.0, 40.0, 40.0, [0.9, 0.2, 0.2, 1.0]))?;
                conn.scene_frame_end()?;
                drain(&mut conn, &mut told)?;
                next += period;
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                }
            }
            println!("noisy: victim frames {frames}");
        }
    }
    Ok(())
}
