//! Smoke test for B2-kmod per-client ring slices.
//!
//! Prints the slot index assigned by the kmod, creates a single
//! window with a label argv[1], and idles. Run two instances side
//! by side — they should land in different slots and each see only
//! their own window's lifecycle events.

use fresco_rs::{blob, flags, matrix_identity, node_type, Connection, Event};

fn quad_renderable(conn: &Connection, r: f32, g: f32, b: f32) -> std::io::Result<[u8; 32]> {
    let verts: [f32; 12] = [-0.5,-0.5,0.0,  0.5,-0.5,0.0,  0.5,0.5,0.0,  -0.5,0.5,0.0];
    let idx: [u16; 6] = [0, 1, 2, 0, 2, 3];
    let v = conn.cas_put(&blob::vertex_data(&verts))?;
    let i = conn.cas_put(&blob::index_data(&idx))?;
    let m = conn.cas_put(&blob::mesh(4, 6, 0x0100, &v, &i))?;
    let mat = conn.cas_put(&blob::material_solid(r, g, b, 1.0))?;
    conn.cas_put(&blob::renderable(&m, &mat))
}

fn main() -> std::io::Result<()> {
    let label = std::env::args().nth(1).unwrap_or_else(|| "client".into());
    let r: f32 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(0.7);
    let g: f32 = std::env::args().nth(3).and_then(|s| s.parse().ok()).unwrap_or(0.4);
    let b: f32 = std::env::args().nth(4).and_then(|s| s.parse().ok()).unwrap_or(0.2);
    let x: f32 = std::env::args().nth(5).and_then(|s| s.parse().ok()).unwrap_or(150.0);
    let y: f32 = std::env::args().nth(6).and_then(|s| s.parse().ok()).unwrap_or(200.0);

    let conn = Connection::open()?;
    println!("[{label}] connected — slot={}", conn.client_slot());

    let cam_xform = [1.0,0.0,0.0,0.0, 0.0,1.0,0.0,0.0, 0.0,0.0,1.0,0.0, 0.0,0.0,5.0,1.0];
    let cam_xform_h = conn.cas_put(&blob::transform(&cam_xform))?;
    let disp = conn.display();
    let aspect = disp.width as f32 / disp.height as f32;
    let cam_h = conn.cas_put(&blob::camera(0.7853982, aspect, 0.1, 100.0, &cam_xform_h))?;

    let win = conn.create_window(400, 300, Some(&label))?;
    conn.window_set_pos(win, x, y)?;
    let rend = quad_renderable(&conn, r, g, b)?;
    conn.set_default_window(win);
    conn.set_camera(&cam_h)?;
    conn.slot_alloc(1, node_type::FRESCO_NODE_RENDERABLE, flags::FRESCO_SLOT_FLAG_VISIBLE)?;
    conn.slot_set_xform_inline(1, &matrix_identity())?;
    conn.slot_set_content(1, &rend)?;
    conn.slot_set_root(1)?;
    conn.frame_begin(0)?;
    conn.frame_end()?;

    println!("[{label}] window {win} created — close button to exit");

    // No deadline — exit only on close-button click. Multi-client
    // smoke test runs until you've exercised both windows.
    loop {
        match conn.wait_event(200)? {
            Some(Event::CloseRequested { window_id }) => {
                println!("[{label}] close requested: window={window_id}");
                if window_id as u16 == win {
                    let _ = conn.destroy_window(win);
                    break;
                }
            }
            Some(Event::WindowFocus { window_id, focused }) =>
                println!("[{label}] focus: window={window_id} focused={focused}"),
            Some(Event::WindowResized { window_id, width, height }) =>
                println!("[{label}] resized: window={window_id} {width}x{height}"),
            Some(_) => {}
            None => {}
        }
    }
    println!("[{label}] exiting");
    Ok(())
}
