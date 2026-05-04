//! atrium-test-client — single-shot Fresco protocol smoke test over Unix socket.
//!
//! Builds a complete scene tree (root → scene_node → renderable → mesh +
//! material) using `fresco_socket::wire`, uploads via the lib's
//! `Connection::upload_blob`, then issues `set_root`. The compositor
//! traverses the new root and the rect appears on screen.
//!
//! Hold the connection open after SET_ROOT so the server keeps the
//! scene visible.

use fresco_scene_server::command::protocol::NULL_HASH;
use fresco_socket::{wire, Connection};

fn main() -> std::io::Result<()> {
    let path = std::env::args().nth(1)
        .unwrap_or_else(|| "/tmp/frescod.sock".to_string());
    let mut conn = Connection::connect(&path)?;
    eprintln!("connected to {path}");

    // Vertex + index for a unit rect.
    let v = conn.upload_blob(&wire::vertex_data_xy(&[
        (0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0),
    ]))?;
    let i = conn.upload_blob(&wire::index_data_u16(&[0, 1, 2, 0, 2, 3]))?;
    let m = conn.upload_blob(&wire::mesh(4, 6, v, i))?;

    let mat = conn.upload_blob(&wire::solid_material([0xff, 0x33, 0xaa, 0xff]))?;

    let xform = wire::affine_2d(400.0, 300.0, 200.0, 200.0);
    let t = conn.upload_blob(&wire::transform_matrix(&xform))?;

    let r = conn.upload_blob(&wire::renderable(m, mat))?;
    let sn = conn.upload_blob(&wire::scene_node(t, r, NULL_HASH))?;
    let nl = conn.upload_blob(&wire::node_list(&[sn]))?;
    let sr = conn.upload_blob(&wire::scene_root(nl, NULL_HASH))?;

    conn.set_root(sr)?;
    eprintln!("SET_ROOT sent — magenta rect at (200,200) 400x300 should be visible");
    eprintln!("holding socket open; ^C to exit");
    std::thread::sleep(std::time::Duration::from_secs(3600));
    Ok(())
}
