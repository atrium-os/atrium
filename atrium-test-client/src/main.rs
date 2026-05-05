//! atrium-test-client — single-shot Fresco protocol smoke test on the
//! envelope-based wire format.
//!
//! Migrated to `fresco-client` (M2.7b). The legacy version built a
//! CAS-blob scene tree (vertex_data → index_data → mesh → material →
//! transform → renderable → scene_node → node_list → scene_root) and
//! committed via `set_root`. The new version uses a single
//! `OP_SCENE_NODE_SET` with `RectParams` — the per-node-delta model
//! makes the scene description ~30 lines shorter and the wire bytes
//! roughly 100× smaller for a one-rect scene.

use fresco_client::Connection;
use fresco_protocol::{PathParams, RectParams};

fn main() -> std::io::Result<()> {
    let path = std::env::args().nth(1)
        .unwrap_or_else(|| "/tmp/frescod.sock".to_string());
    let mut conn = Connection::connect(&path)?;
    eprintln!("connected to {path}");

    /* Magenta rect + a yellow rotated bar above it (atrium-core PATH
     * op smoke). One frame; both nodes commit together. */
    conn.scene_frame_begin()?;
    conn.scene_node_rect(/*node_id=*/ 0, RectParams {
        x: 200.0, y: 200.0,
        w: 400.0, h: 300.0,
        r: 1.0, g: 0.2, b: 0.6, a: 1.0,
    })?;
    conn.scene_node_path(/*node_id=*/ 1, PathParams {
        cx: 400.0, cy: 150.0,
        length: 300.0, width: 20.0,
        angle: std::f32::consts::FRAC_PI_6,  /* 30° */
        r: 1.0, g: 0.85, b: 0.1, a: 1.0,
    })?;
    conn.scene_frame_end()?;

    eprintln!("rect + rotated path committed");
    eprintln!("holding socket open; ^C to exit");
    std::thread::sleep(std::time::Duration::from_secs(3600));
    Ok(())
}
