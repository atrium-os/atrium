//! atrium-vector-demo — filled paths (ATRIUM_CORE_FILL) in one window:
//! a ring (cubics, nonzero with a hole), an even-odd star, a quadratic
//! blob, a clipped wedge and a 4,000-segment chart line turned into a
//! fill, then a slow zoom of the star sent as FILL_TRANSFORM only.
//!
//! Usage: atrium-vector-demo [SOCK] [SECONDS]

use fresco_client::Connection;
use fresco_protocol::{fill_verb as v, FillParams, FillPaint, FillTransformParams, RectParams, WindowHints};
use std::time::{Duration, Instant};

fn solid(r: f32, g: f32, b: f32, a: f32) -> FillPaint {
    FillPaint::Solid { r, g, b, a }
}

fn fill(verbs: Vec<u8>, points: Vec<[f32; 2]>, transform: [f32; 6], rule: u8, paint: FillPaint) -> FillParams {
    FillParams { rule, transform, paint, opacity: 1.0, clip: None, verbs, points }
}

fn circle(cx: f32, cy: f32, r: f32, rev: bool) -> (Vec<u8>, Vec<[f32; 2]>) {
    let k = 0.5523;
    let mut p = vec![[cx + r, cy]];
    let q = [[1.0, 0.0], [0.0, 1.0], [-1.0, 0.0], [0.0, -1.0], [1.0, 0.0]];
    for i in 0..4 {
        let (a, b): ([f32; 2], [f32; 2]) = (q[i], q[i + 1]);
        p.push([cx + r * (a[0] + k * b[0]), cy + r * (a[1] + k * b[1])]);
        p.push([cx + r * (b[0] + k * a[0]), cy + r * (b[1] + k * a[1])]);
        p.push([cx + r * b[0], cy + r * b[1]]);
    }
    if rev {
        p.reverse();
    }
    (vec![v::MOVE, v::CUBIC, v::CUBIC, v::CUBIC, v::CUBIC, v::CLOSE], p)
}

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let sock = args.next().unwrap_or_else(|| "/tmp/frescod.sock".to_string());
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(600);
    let mut conn = Connection::connect(&sock)?;
    let win = conn.window_create(900, 600, "vector shapes", WindowHints::default())?;
    conn.set_default_window(win as u16);
    eprintln!("window {win}");

    conn.scene_frame_begin()?;
    conn.scene_node_rect(1, RectParams { x: 0.0, y: 0.0, w: 4000.0, h: 4000.0, r: 0.9, g: 0.9, b: 0.88, a: 1.0, radius: 0.0 })?;
    // Ring: outer circle, reversed inner circle.
    let (mut rv, mut rp) = circle(0.0, 0.0, 110.0, false);
    let (v2, p2) = circle(0.0, 0.0, 55.0, true);
    rv.extend(v2);
    rp.extend(p2);
    conn.scene_node_fill(10, &fill(rv, rp, [1.0, 0.0, 0.0, 1.0, 160.0, 170.0], 0, solid(0.05, 0.35, 0.6, 1.0)))?;
    // Even-odd star (its centre stays empty).
    let star: Vec<[f32; 2]> = (0..5)
        .map(|i| {
            let a = i as f32 * 4.0 * std::f32::consts::PI / 5.0 - std::f32::consts::FRAC_PI_2;
            [a.cos() * 100.0, a.sin() * 100.0]
        })
        .collect();
    let star_xf = |s: f32| [s, 0.0, 0.0, s, 430.0, 170.0];
    conn.scene_node_fill(11, &fill(vec![v::MOVE, v::LINE, v::LINE, v::LINE, v::LINE, v::CLOSE], star, star_xf(1.0), 1, solid(0.85, 0.3, 0.05, 1.0)))?;
    // Quadratic blob, translucent, over the ring.
    conn.scene_node_fill(12, &fill(
        vec![v::MOVE, v::QUAD, v::QUAD, v::QUAD, v::CLOSE],
        vec![[0.0, 0.0], [120.0, -60.0], [160.0, 40.0], [120.0, 140.0], [20.0, 120.0], [-60.0, 60.0], [0.0, 0.0]],
        [1.0, 0.0, 0.0, 1.0, 200.0, 220.0], 0, solid(0.9, 0.75, 0.1, 0.6)))?;
    // A wedge clipped to a box.
    let mut wedge = fill(vec![v::MOVE, v::LINE, v::LINE, v::CLOSE], vec![[0.0, 0.0], [200.0, 40.0], [60.0, 200.0]], [1.0, 0.0, 0.0, 1.0, 620.0, 60.0], 0, solid(0.2, 0.6, 0.2, 1.0));
    wedge.clip = Some([650.0, 80.0, 780.0, 220.0]);
    conn.scene_node_fill(13, &wedge)?;
    // A 4,000-point chart line as the fill under it.
    let n = 4000;
    let mut pts = vec![[0.0, 120.0]];
    for i in 0..n {
        let x = i as f32 * 800.0 / n as f32;
        let y = 60.0 + 40.0 * (x * 0.05).sin() * (x * 0.007).cos() + 10.0 * (x * 0.9).sin();
        pts.push([x, y]);
    }
    pts.push([800.0, 120.0]);
    let mut verbs = vec![v::MOVE];
    verbs.extend(std::iter::repeat_n(v::LINE, pts.len() - 1));
    verbs.push(v::CLOSE);
    conn.scene_node_fill(14, &fill(verbs, pts, [1.0, 0.0, 0.0, 1.0, 50.0, 430.0], 0, solid(0.4, 0.1, 0.6, 0.85)))?;
    conn.scene_frame_end()?;

    // Zoom the star by transform only.
    let start = Instant::now();
    let mut frame = 0u64;
    while start.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_millis(100));
        let t = start.elapsed().as_secs_f32();
        let s = 1.0 + 0.3 * (t * 0.8).sin();
        conn.scene_frame_begin()?;
        conn.scene_node_fill_transform(11, FillTransformParams { transform: star_xf(s), opacity: 1.0, clip: None })?;
        conn.scene_frame_end()?;
        frame += 1;
        if frame % 50 == 0 {
            eprintln!("frame {frame}: star at scale {s:.2}");
        }
    }
    Ok(())
}
