//! Clock renderer — emits 12 hour ticks + hour/minute/second hands +
//! a small center hub as a per-frame `set_root` scene tree. Uses a
//! single CAS-stored centered-unit-rect mesh (-0.5..0.5)² that every
//! oriented-rect references; world matrices encode rotation + scale
//! + translate per item.

use std::f32::consts::TAU;

use fresco_server::command::protocol::{Hash256, NULL_HASH};
use fresco_socket::{wire, Connection};

pub struct Renderer {
    rect_mesh:   Hash256,
    tick_mat:    Hash256,
    hour_mat:    Hash256,
    minute_mat:  Hash256,
    second_mat:  Hash256,
    hub_mat:     Hash256,
    win_w:       f32,
    win_h:       f32,
}

impl Renderer {
    pub fn new(
        conn: &mut Connection,
        win_w: u32,
        win_h: u32,
    ) -> std::io::Result<Self> {
        // Centered unit rect: vertices in (-0.5, -0.5)..(0.5, 0.5)
        // so a (length, thickness) scale + θ rotation + (cx, cy)
        // translate yields a tick / hand of the right span.
        let v = conn.upload_blob(&wire::vertex_data_xy(&[
            (-0.5, -0.5), (0.5, -0.5), (0.5, 0.5), (-0.5, 0.5),
        ]))?;
        let i = conn.upload_blob(&wire::index_data_u16(&[0, 1, 2, 0, 2, 3]))?;
        let rect_mesh = conn.upload_blob(&wire::mesh(4, 6, v, i))?;

        let tick_mat   = conn.upload_blob(&wire::solid_material([0xff, 0xff, 0xff, 0xc0]))?;
        let hour_mat   = conn.upload_blob(&wire::solid_material([0xff, 0xff, 0xff, 0xff]))?;
        let minute_mat = conn.upload_blob(&wire::solid_material([0xff, 0xff, 0xff, 0xee]))?;
        let second_mat = conn.upload_blob(&wire::solid_material([0xff, 0x66, 0x66, 0xff]))?;
        let hub_mat    = conn.upload_blob(&wire::solid_material([0xff, 0x44, 0x44, 0xff]))?;

        Ok(Self {
            rect_mesh, tick_mat, hour_mat, minute_mat, second_mat, hub_mat,
            win_w: win_w as f32, win_h: win_h as f32,
        })
    }

    pub fn render(
        &self,
        conn: &mut Connection,
        hour: u32,
        minute: u32,
        second: u32,
    ) -> std::io::Result<()> {
        let cx = self.win_w * 0.5;
        let cy = self.win_h * 0.5;
        let radius = self.win_w.min(self.win_h) * 0.42;

        let mut nodes: Vec<Hash256> = Vec::new();

        // ── Hour ticks ──────────────────────────────────────────
        let r_inner = radius * 0.92;
        let r_outer = radius;
        let tick_len = r_outer - r_inner;
        let tick_mid = (r_outer + r_inner) * 0.5;
        for i in 0..12 {
            let theta = (i as f32) / 12.0 * TAU - std::f32::consts::FRAC_PI_2;
            let mx = cx + theta.cos() * tick_mid;
            let my = cy + theta.sin() * tick_mid;
            let thickness = if i % 3 == 0 { 4.0 } else { 2.0 };
            let xform = oriented_rect(theta, tick_len, thickness, mx, my);
            self.push(conn, &mut nodes, xform, self.tick_mat)?;
        }

        // ── Hands. theta runs from "12 o'clock up" clockwise.
        let h_unit = ((hour % 12) as f32 + minute as f32 / 60.0) / 12.0;
        let m_unit = (minute as f32 + second as f32 / 60.0) / 60.0;
        let s_unit = second as f32 / 60.0;
        for (unit, len_frac, thick, mat) in [
            (h_unit, 0.50, 8.0, self.hour_mat),
            (m_unit, 0.75, 5.0, self.minute_mat),
            (s_unit, 0.85, 1.6, self.second_mat),
        ] {
            let theta = unit * TAU - std::f32::consts::FRAC_PI_2;
            let len = radius * len_frac;
            let mx = cx + theta.cos() * len * 0.5;
            let my = cy + theta.sin() * len * 0.5;
            let xform = oriented_rect(theta, len, thick, mx, my);
            self.push(conn, &mut nodes, xform, mat)?;
        }

        // ── Center hub (small square) ───────────────────────────
        let xform = oriented_rect(0.0, 10.0, 10.0, cx, cy);
        self.push(conn, &mut nodes, xform, self.hub_mat)?;

        let nl = conn.upload_blob(&wire::node_list(&nodes))?;
        let sr = conn.upload_blob(&wire::scene_root(nl, NULL_HASH))?;
        conn.set_root(sr)?;
        Ok(())
    }

    fn push(
        &self,
        conn: &mut Connection,
        nodes: &mut Vec<Hash256>,
        xform: [f32; 16],
        material: Hash256,
    ) -> std::io::Result<()> {
        let t  = conn.upload_blob(&wire::transform_matrix(&xform))?;
        let r  = conn.upload_blob(&wire::renderable(self.rect_mesh, material))?;
        let sn = conn.upload_blob(&wire::scene_node(t, r, NULL_HASH))?;
        nodes.push(sn);
        Ok(())
    }
}

/// Column-major 4x4 matrix encoding `T(cx, cy) · R(theta) · S(length, thickness)`.
/// Applied to a centered-unit-rect vertex (vx, vy) ∈ {±0.5}², yields
/// the corner of a (length × thickness) rectangle rotated by `theta`
/// and centered at (cx, cy) in screen pixels.
fn oriented_rect(theta: f32, length: f32, thickness: f32, cx: f32, cy: f32) -> [f32; 16] {
    let c = theta.cos();
    let s = theta.sin();
    [
        length * c,    length * s,    0.0, 0.0,    // col 0
       -thickness * s, thickness * c, 0.0, 0.0,    // col 1
        0.0,           0.0,           1.0, 0.0,    // col 2
        cx,            cy,            0.0, 1.0,    // col 3
    ]
}
