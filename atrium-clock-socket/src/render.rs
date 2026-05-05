//! Clock renderer on the envelope-based stack.
//!
//! Each tick / hand / hub is one `OP_SCENE_NODE_SET` carrying a
//! `PathParams` (rotated quad). 16 nodes per frame:
//!
//!   ids   1..=12   hour ticks
//!   ids  13..=15   hour / minute / second hands
//!   id      16     centre hub (axis-aligned little square)
//!
//! Bracketed by `SCENE_FRAME_BEGIN` / `SCENE_FRAME_END`. Repeating
//! the same node_ids each frame replaces the previous bindings, so
//! the per-tick CAS-blob dance the legacy renderer did is gone — the
//! whole frame is ~17 envelopes (1 begin + 16 sets + 1 end).

use std::f32::consts::TAU;

use fresco_client::Connection;
use fresco_protocol::PathParams;

pub struct Renderer {
    win_w: f32,
    win_h: f32,
}

impl Renderer {
    pub fn new(win_w: u32, win_h: u32) -> Self {
        Self { win_w: win_w as f32, win_h: win_h as f32 }
    }

    pub fn render(
        &self,
        conn: &mut Connection,
        hour: u32, minute: u32, second: u32,
    ) -> std::io::Result<()> {
        let cx = self.win_w * 0.5;
        let cy = self.win_h * 0.5;
        let radius = self.win_w.min(self.win_h) * 0.42;

        conn.scene_frame_begin()?;

        /* Hour ticks (12 of them). theta = 0 is "3 o'clock" (math
         * convention); -π/2 puts unit=0 at "12 o'clock up". With the
         * pixel coordinate system being top-left / Y-down, sin(-π/2)
         * = -1 so my = cy - radius is visually above center — the
         * 12-o'clock position. Same expression works for screen + math
         * because it's symmetric across the X axis. */
        let r_inner = radius * 0.92;
        let r_outer = radius;
        let tick_len = r_outer - r_inner;
        let tick_mid = (r_outer + r_inner) * 0.5;
        for i in 0..12 {
            let theta = (i as f32) / 12.0 * TAU - std::f32::consts::FRAC_PI_2;
            let mx = cx + theta.cos() * tick_mid;
            let my = cy + theta.sin() * tick_mid;
            let thickness = if i % 3 == 0 { 4.0 } else { 2.0 };
            conn.scene_node_path(/*node_id=*/ 1 + i, PathParams {
                cx: mx, cy: my,
                length: tick_len, width: thickness, angle: theta,
                r: 1.0, g: 1.0, b: 1.0, a: 0.75,
            })?;
        }

        /* Hands. Each hand is a rectangle of (length × thickness)
         * centered at the midpoint between the dial centre and the
         * hand tip — so the centre hub stays at (cx, cy) and the tip
         * sits `len` away. */
        let h_unit = ((hour % 12) as f32 + minute as f32 / 60.0) / 12.0;
        let m_unit = (minute as f32 + second as f32 / 60.0) / 60.0;
        let s_unit =  second as f32 / 60.0;
        let hands = [
            /* (node_id, unit, len_frac, thick, color) */
            (13u32, h_unit, 0.50, 8.0, [1.0, 1.0, 1.0, 1.0]),
            (14,    m_unit, 0.75, 5.0, [1.0, 1.0, 1.0, 0.93]),
            (15,    s_unit, 0.85, 1.6, [1.0, 0.4, 0.4, 1.0]),
        ];
        for (id, unit, len_frac, thick, c) in hands {
            let theta = unit * TAU - std::f32::consts::FRAC_PI_2;
            let len = radius * len_frac;
            let mx = cx + theta.cos() * len * 0.5;
            let my = cy + theta.sin() * len * 0.5;
            conn.scene_node_path(id, PathParams {
                cx: mx, cy: my,
                length: len, width: thick, angle: theta,
                r: c[0], g: c[1], b: c[2], a: c[3],
            })?;
        }

        /* Centre hub. */
        conn.scene_node_path(16, PathParams {
            cx, cy,
            length: 10.0, width: 10.0, angle: 0.0,
            r: 1.0, g: 0.27, b: 0.27, a: 1.0,
        })?;

        conn.scene_frame_end()?;
        Ok(())
    }
}
