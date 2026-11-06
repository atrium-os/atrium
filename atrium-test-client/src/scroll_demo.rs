//! atrium-scroll-demo — a scrolled list in a ScrollView-shaped group tree
//! (docs/spec/fresco-scene-groups.md §4–5): a SCROLL viewport with its
//! opaque background and a content group of 600 rows (rects, with a filled
//! wave every tenth row), and a floating button drawn over it. Each frame
//! moves only the content group.
//!
//! Usage: atrium-scroll-demo [SOCK] [SECONDS] [PX_PER_FRAME] [FPS]
//! A whole-pixel step lets the renderer shift; a fractional one (6.5)
//! makes it draw the viewport in full, for comparison.

use fresco_client::{Connection, Event};
use fresco_protocol::{fill_verb as v, group_flags, FillPaint, FillParams, GroupParams, RectParams, WindowHints};
use std::time::{Duration, Instant};

const VIEW: [f32; 4] = [40.0, 40.0, 860.0, 560.0];
const ROW: f32 = 22.0;
const ROWS: u32 = 600;

fn rect(x: f32, y: f32, w: f32, h: f32, c: [f32; 4], radius: f32) -> RectParams {
    RectParams { x, y, w, h, r: c[0], g: c[1], b: c[2], a: c[3], radius }
}

fn group(parent: Option<u32>, order: u32, dy: f32, clip: Option<[f32; 4]>, flags: u32) -> GroupParams {
    GroupParams { parent, order, transform: [1.0, 0.0, 0.0, 1.0, 0.0, dy], clip, opacity: 1.0, flags }
}

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let sock = args.next().unwrap_or_else(|| "/tmp/frescod.sock".to_string());
    let secs: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(600);
    let step: f32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(7.0);
    let fps: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(30);
    let mut conn = Connection::connect(&sock)?;
    let win = conn.window_create(900, 600, "scrolling", WindowHints::default())?;
    conn.set_default_window(win as u16);
    eprintln!("window {win}");

    conn.scene_frame_begin()?;
    conn.scene_node_rect(1, rect(0.0, 0.0, 900.0, 600.0, [0.3, 0.32, 0.36, 1.0], 0.0))?;
    // The viewport (100), its background (101), the content group (102).
    conn.scene_group(100, group(None, 0, 0.0, Some(VIEW), group_flags::SCROLL))?;
    conn.scene_node_rect(101, rect(VIEW[0], VIEW[1], VIEW[2] - VIEW[0], VIEW[3] - VIEW[1], [0.97, 0.97, 0.95, 1.0], 0.0))?;
    conn.scene_place(101, Some(100), 0)?;
    conn.scene_group(102, group(Some(100), 1, 0.0, None, 0))?;
    let mut id = 1000;
    for i in 0..ROWS {
        let y = VIEW[1] + i as f32 * ROW;
        let shade = if i % 2 == 0 { 0.92 } else { 0.86 };
        for (k, (x, w)) in [(8.0, 120.0), (140.0, 260.0), (420.0, 90.0 + (i % 7) as f32 * 30.0)].into_iter().enumerate() {
            let c = if k == 0 { [0.2, 0.4 + (i % 5) as f32 * 0.1, 0.8, 1.0] } else { [shade - 0.5, shade - 0.45, shade - 0.4, 1.0] };
            conn.scene_node_rect(id, rect(VIEW[0] + x, y + 5.0, w, 12.0, c, 3.0))?;
            conn.scene_place(id, Some(102), id)?;
            id += 1;
        }
        if i % 10 == 0 {
            let n = 60;
            let mut pts = vec![[0.0, 18.0]];
            pts.extend((0..=n).map(|j| [j as f32 * 4.0, 9.0 + 7.0 * (j as f32 * 0.3 + i as f32).sin()]));
            pts.push([n as f32 * 4.0, 18.0]);
            let mut verbs = vec![v::MOVE];
            verbs.extend(std::iter::repeat_n(v::LINE, pts.len() - 1));
            verbs.push(v::CLOSE);
            let f = FillParams {
                rule: 0,
                transform: [1.0, 0.0, 0.0, 1.0, VIEW[0] + 560.0, y + 2.0],
                paint: FillPaint::Solid { r: 0.6, g: 0.2, b: 0.5, a: 0.9 },
                opacity: 1.0,
                clip: None,
                verbs,
                points: pts,
            };
            conn.scene_node_fill(id, &f)?;
            conn.scene_place(id, Some(102), id)?;
            id += 1;
        }
    }
    // A floating button over the list.
    conn.scene_node_rect(2, rect(760.0, 480.0, 80.0, 50.0, [0.1, 0.6, 0.3, 0.9], 10.0))?;
    conn.scene_place(2, None, 1)?;
    conn.scene_frame_end()?;

    let max = ROWS as f32 * ROW - (VIEW[3] - VIEW[1]);
    let (mut off, mut dir) = (0.0f32, -1.0f32);
    let start = Instant::now();
    let mut frame = 0u64;
    while start.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_millis(1000 / fps));
        off += dir * step;
        if off < -max || off > 0.0 {
            dir = -dir;
            off = off.clamp(-max, 0.0);
        }
        conn.scene_frame_begin()?;
        conn.scene_group(102, group(Some(100), 1, off, None, 0))?;
        conn.scene_frame_end()?;
        while let Some(ev) = conn.poll_event()? {
            if let Event::FillsDropped { window_id, dropped } = ev {
                eprintln!("window {window_id}: dropped {dropped:?}");
            }
        }
        frame += 1;
        if frame % 100 == 0 {
            eprintln!("frame {frame}: offset {off}");
        }
    }
    Ok(())
}
