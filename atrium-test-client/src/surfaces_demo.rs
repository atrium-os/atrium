//! atrium-surfaces-demo — exercises Fresco's per-window surfaces and the
//! compose pass (F4, docs/spec/fresco-f4-split.md).
//!
//! Opens three windows and places them itself over the window-management
//! socket (no forum-wm), overlapping, then slides the top one each second:
//!
//! - window A: opaque background, a rounded rect;
//! - window B: draws only a rounded rect (no background), so its undrawn
//!   pixels must show A and the desktop under it;
//! - window C: a half-transparent background and a solid bar, moving.
//!
//! Pointer and key events are printed as they arrive, on the app's
//! connection (`app:`) and on the WM's (`wm:`), so input routing can be
//! checked with frescod's injector. `SURFACES_STILL=1` keeps C in place.
//!
//! Usage: atrium-surfaces-demo [client-sock] [wm-sock]

use fresco_client::Connection;
use fresco_protocol::{RectParams, WindowHints, WmDeclareLayoutPayload, WmRect, WmSlot};
use std::time::Duration;

fn rect(x: f32, y: f32, w: f32, h: f32, c: [f32; 4], radius: f32) -> RectParams {
    RectParams { x, y, w, h, r: c[0], g: c[1], b: c[2], a: c[3], radius }
}

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let sock = args.next().unwrap_or_else(|| "/tmp/frescod.sock".into());
    let wm_sock = args.next().unwrap_or_else(|| "/tmp/frescod-wm.sock".into());
    let mut conn = Connection::connect(&sock)?;
    let (aw, ah, bw, bh, cw, ch) = (640u32, 420u32, 520u32, 360u32, 480u32, 300u32);
    let a = conn.window_create(aw, ah, "A", WindowHints::default())?;
    let b = conn.window_create(bw, bh, "B", WindowHints::default())?;
    let c = conn.window_create(cw, ch, "C", WindowHints::default())?;
    eprintln!("windows a={a} b={b} c={c}");

    conn.set_default_window(a as u16);
    conn.scene_frame_begin()?;
    conn.scene_node_rect(1, rect(0.0, 0.0, aw as f32, ah as f32, [0.85, 0.85, 0.8, 1.0], 0.0))?;
    conn.scene_node_rect(2, rect(40.0, 40.0, 300.0, 200.0, [0.8, 0.1, 0.1, 1.0], 24.0))?;
    conn.scene_frame_end()?;

    conn.set_default_window(b as u16);
    conn.scene_frame_begin()?;
    conn.scene_node_rect(1, rect(20.0, 20.0, 260.0, 160.0, [0.1, 0.3, 0.9, 1.0], 40.0))?;
    conn.scene_frame_end()?;

    conn.set_default_window(c as u16);
    conn.scene_frame_begin()?;
    conn.scene_node_rect(1, rect(0.0, 0.0, cw as f32, ch as f32, [0.1, 0.7, 0.2, 0.5], 0.0))?;
    conn.scene_node_rect(2, rect(30.0, 120.0, 420.0, 40.0, [1.0, 0.9, 0.1, 1.0], 8.0))?;
    conn.scene_frame_end()?;

    let mut wm = Connection::connect(&wm_sock)?;
    let slot = |id: u32, x: i32, y: i32, w: u32, h: u32, layer: i32| WmSlot {
        surface_id: id,
        rect: WmRect { x, y, w: w as i32, h: h as i32 },
        layer,
    };
    let still = std::env::var("SURFACES_STILL").is_ok();
    for step in 0.. {
        let cx = if still { 700 } else { 700 + (step % 8) * 40 };
        wm.wm_declare_layout(&WmDeclareLayoutPayload {
            slots: vec![
                slot(a, 200, 150, aw, ah, 2),
                slot(b, 560, 380, bw, bh, 1),
                slot(c, cx, 250, cw, ch, 0),
            ],
            focus: c,
            focus_cause: 0,
        })?;
        eprintln!("step {step}: C at x={cx}");
        // Drain events so the server's writer never blocks on us.
        for (who, c) in [("app", &mut conn), ("wm", &mut wm)] {
            while let Some(e) = c.poll_event()? {
                use fresco_client::Event as E;
                if matches!(e, E::PointerButton { .. } | E::Key { .. } | E::PointerScroll { .. }) {
                    println!("{who}: {e:?}");
                }
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    Ok(())
}
