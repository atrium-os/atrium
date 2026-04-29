//! Smoke test for input-event target_window tagging.
//!
//! Creates two windows, then polls input events for 30 s, printing
//! the target_window on every event. Wave the cursor over each
//! window's content to confirm pointer events carry the expected id.

use fresco_rs::{blob, flags, matrix_identity, node_type, Connection, Event};

fn fill_window(conn: &Connection, win_id: u16, r: f32, g: f32, b: f32, cam_h: &[u8;32]) -> std::io::Result<()> {
    let verts: [f32; 12] = [
        -0.5, -0.5, 0.0,
         0.5, -0.5, 0.0,
         0.5,  0.5, 0.0,
        -0.5,  0.5, 0.0,
    ];
    let idx: [u16; 6] = [0, 1, 2, 0, 2, 3];
    let v = conn.cas_put(&blob::vertex_data(&verts))?;
    let i = conn.cas_put(&blob::index_data(&idx))?;
    let m = conn.cas_put(&blob::mesh(4, 6, 0x0100, &v, &i))?;
    let mat = conn.cas_put(&blob::material_solid(r, g, b, 1.0))?;
    let rend = conn.cas_put(&blob::renderable(&m, &mat))?;
    conn.set_default_window(win_id);
    conn.set_camera(cam_h)?;
    conn.slot_alloc(1, node_type::FRESCO_NODE_RENDERABLE, flags::FRESCO_SLOT_FLAG_VISIBLE)?;
    conn.slot_set_xform_inline(1, &matrix_identity())?;
    conn.slot_set_content(1, &rend)?;
    conn.slot_set_root(1)?;
    conn.frame_begin(0)?;
    conn.frame_end()?;
    Ok(())
}

fn main() -> std::io::Result<()> {
    let conn = Connection::open()?;
    let disp = conn.display();
    println!("display: {}x{}", disp.width, disp.height);

    let aspect = disp.width as f32 / disp.height as f32;
    let cam_xform = [
        1.0, 0.0, 0.0, 0.0,
        0.0, 1.0, 0.0, 0.0,
        0.0, 0.0, 1.0, 0.0,
        0.0, 0.0, 5.0, 1.0,
    ];
    let cam_xform_h = conn.cas_put(&blob::transform(&cam_xform))?;
    let cam_h = conn.cas_put(&blob::camera(0.7853982, aspect, 0.1, 100.0, &cam_xform_h))?;

    let w1 = conn.create_window(400, 300, Some("alpha"))?;
    let w2 = conn.create_window(400, 300, Some("beta"))?;
    conn.window_set_pos(w1, 100.0, 200.0)?;
    conn.window_set_pos(w2, 600.0, 400.0)?;
    // Fill each window with a solid color so they're visible.
    fill_window(&conn, w1, 0.0, 0.7, 0.7, &cam_h)?;     // alpha = teal
    fill_window(&conn, w2, 0.8, 0.3, 0.5, &cam_h)?;     // beta  = pink
    println!("created windows {w1} (alpha) and {w2} (beta)");
    println!("hover content rects to see pointer events tagged with window_id; 30 s");

    let mut alive: Vec<u16> = vec![w1, w2];
    let started = std::time::Instant::now();
    let mut resized_once = false;
    let deadline = started + std::time::Duration::from_secs(120);
    while !alive.is_empty() && std::time::Instant::now() < deadline {
        // After 3 s, shrink alpha to demonstrate resize + RESIZED event.
        if !resized_once && started.elapsed() >= std::time::Duration::from_secs(3) {
            resized_once = true;
            if alive.contains(&w1) {
                println!("resizing window {w1} to 250x180");
                conn.window_set_size(w1, 250, 180)?;
            }
        }
        match conn.wait_event(200)? {
            Some(Event::MouseMove { x, y, target_window })   => println!("move {target_window}: ({x},{y})"),
            Some(Event::MouseBtn  { button, pressed, target_window }) =>
                println!("btn  {target_window}: button={button} pressed={pressed}"),
            Some(Event::Key       { keysym, pressed, target_window }) =>
                println!("key  {target_window}: keysym=0x{keysym:02x} pressed={pressed}"),
            Some(Event::Scroll    { dx, dy, target_window }) =>
                println!("scrl {target_window}: ({dx},{dy})"),
            Some(Event::CloseRequested { window_id }) => {
                println!("close requested: window={window_id}");
                let id = window_id as u16;
                if let Some(pos) = alive.iter().position(|&w| w == id) {
                    alive.remove(pos);
                    conn.destroy_window(id)?;
                }
            }
            Some(Event::WindowResized { window_id, width, height }) =>
                println!("resized: window={window_id} {width}x{height}"),
            Some(Event::WindowFocus { window_id, focused }) =>
                println!("focus: window={window_id} focused={focused}"),
            Some(other) => println!("other: {other:?}"),
            None => {}
        }
    }
    for id in alive { let _ = conn.destroy_window(id); }
    println!("OK");
    Ok(())
}
