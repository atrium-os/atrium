//! End-to-end: libatrium's atrium_window_frame_glyph_run
//! emits an OP_SCENE_NODE_SET carrying a postcard-encoded
//! GlyphRunParams body under op_id =
//! ATRIUM_TEXT_GLYPH_RUN. Stub server decodes via the
//! canonical fresco_protocol crate and asserts every
//! glyph survives the wire intact.

#![cfg(unix)]

use aqueduct::classes::CLASS_DISPLAY;
use aqueduct::envelope::{self, Header};
use std::io::Read;
use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const OP_SCENE_NODE_SET: u16 = 0x0040;

#[derive(Debug, Clone)]
struct Captured {
    node_id: u32,
    op_id: u32,
    run: fresco_protocol::GlyphRunParams,
}

fn spawn_stub(socket_path: std::path::PathBuf) -> Arc<Mutex<Vec<Captured>>> {
    let cap = Arc::new(Mutex::new(Vec::<Captured>::new()));
    let cap2 = cap.clone();
    thread::spawn(move || {
        let listener = UnixListener::bind(&socket_path).expect("bind");
        let (mut stream, _) = listener.accept().expect("accept");
        loop {
            let mut hdr_bytes = [0u8; envelope::HEADER_LEN];
            if stream.read_exact(&mut hdr_bytes).is_err() { break; }
            let hdr = Header::decode(&hdr_bytes).unwrap();
            let mut payload = vec![0u8; hdr.length as usize];
            if stream.read_exact(&mut payload).is_err() { break; }
            if hdr.opcode_class == CLASS_DISPLAY && hdr.op == OP_SCENE_NODE_SET {
                let node: fresco_protocol::SceneNodeSetPayload =
                    postcard::from_bytes(&payload).unwrap();
                if node.op_id == fresco_protocol::scene_ops::ATRIUM_TEXT_GLYPH_RUN {
                    let run: fresco_protocol::GlyphRunParams =
                        postcard::from_bytes(&node.params).unwrap();
                    cap2.lock().unwrap().push(Captured {
                        node_id: node.node_id, op_id: node.op_id, run,
                    });
                }
            }
        }
    });
    cap
}

static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn frame_glyph_run_round_trips_three_glyphs() {
    let _guard = ENV_LOCK.lock().unwrap();
    atrium::atrium_window_disconnect();

    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("fresco.sock");
    let cap = spawn_stub(sock.clone());
    while !sock.exists() { thread::sleep(Duration::from_millis(5)); }
    std::env::set_var("ATRIUM_FRESCO_SOCKET", &sock);

    // Three glyphs spelling "hi!" — fake atlas
    // coordinates, real wire roundtrip.
    let glyphs = [
        atrium::AtriumGlyph {
            dx: 0.0, dy: 0.0,
            atlas_u: 0, atlas_v: 0, atlas_w: 8, atlas_h: 16,
            bearing_x: 0.0, bearing_y: 12.0,
        },
        atrium::AtriumGlyph {
            dx: 9.0, dy: 0.0,
            atlas_u: 8, atlas_v: 0, atlas_w: 4, atlas_h: 16,
            bearing_x: 0.0, bearing_y: 12.0,
        },
        atrium::AtriumGlyph {
            dx: 14.0, dy: 0.0,
            atlas_u: 12, atlas_v: 0, atlas_w: 3, atlas_h: 16,
            bearing_x: 0.0, bearing_y: 12.0,
        },
    ];

    assert_eq!(atrium::atrium_window_frame_begin(7), 0);
    let r = unsafe {
        atrium::atrium_window_frame_glyph_run(
            55,                          // node_id
            42, 256, 256,                // atlas slot + dims
            100.0, 50.0,                 // run origin
            0.0, 0.0, 0.0, 1.0,          // black, opaque
            glyphs.as_ptr(), glyphs.len(),
        )
    };
    assert_eq!(r, 0);
    assert_eq!(atrium::atrium_window_frame_end(), 0);

    std::env::remove_var("ATRIUM_FRESCO_SOCKET");
    atrium::atrium_window_disconnect();
    thread::sleep(Duration::from_millis(50));

    let runs = cap.lock().unwrap().clone();
    assert_eq!(runs.len(), 1, "expected one glyph-run node");
    let c = &runs[0];
    assert_eq!(c.node_id, 55);
    assert_eq!(c.op_id, fresco_protocol::scene_ops::ATRIUM_TEXT_GLYPH_RUN);
    assert_eq!(c.run.x, 100.0);
    assert_eq!(c.run.y, 50.0);
    assert_eq!(c.run.atlas_slot_id, 42);
    assert_eq!(c.run.atlas_width, 256);
    assert_eq!(c.run.atlas_height, 256);
    assert_eq!(c.run.glyphs.len(), 3);

    // Spot-check the first and last glyphs.
    let g0 = &c.run.glyphs[0];
    assert_eq!(g0.dx, 0.0);
    assert_eq!(g0.atlas_w, 8);
    assert_eq!(g0.bearing_y, 12.0);
    let g2 = &c.run.glyphs[2];
    assert_eq!(g2.dx, 14.0);
    assert_eq!(g2.atlas_u, 12);
    assert_eq!(g2.atlas_w, 3);
}

#[test]
fn frame_glyph_run_outside_frame_errors() {
    let _guard = ENV_LOCK.lock().unwrap();
    atrium::atrium_window_disconnect();
    let r = unsafe {
        atrium::atrium_window_frame_glyph_run(
            1, 1, 64, 64,
            0.0, 0.0,
            0.0, 0.0, 0.0, 1.0,
            std::ptr::null(), 0,
        )
    };
    assert_eq!(r, atrium::ATRIUM_ERR_FRESCO_RPC);
}

#[test]
fn frame_glyph_run_with_null_glyphs_and_nonzero_count_errors() {
    let _guard = ENV_LOCK.lock().unwrap();
    atrium::atrium_window_disconnect();
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("fresco.sock");
    let _cap = spawn_stub(sock.clone());
    while !sock.exists() { thread::sleep(Duration::from_millis(5)); }
    std::env::set_var("ATRIUM_FRESCO_SOCKET", &sock);

    assert_eq!(atrium::atrium_window_frame_begin(1), 0);
    let r = unsafe {
        atrium::atrium_window_frame_glyph_run(
            1, 1, 64, 64,
            0.0, 0.0,
            0.0, 0.0, 0.0, 1.0,
            std::ptr::null(),
            5,                  // claim 5 glyphs but pass null
        )
    };
    assert_eq!(r, atrium::ATRIUM_ERR_INVALID_PATH);
    let _ = atrium::atrium_window_frame_end();

    std::env::remove_var("ATRIUM_FRESCO_SOCKET");
    atrium::atrium_window_disconnect();
}

#[test]
fn frame_glyph_run_with_zero_glyphs_is_allowed() {
    // A glyph-run with zero glyphs is degenerate but
    // structurally valid (e.g. shaped from an empty
    // string). Mustn't error.
    let _guard = ENV_LOCK.lock().unwrap();
    atrium::atrium_window_disconnect();

    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("fresco.sock");
    let cap = spawn_stub(sock.clone());
    while !sock.exists() { thread::sleep(Duration::from_millis(5)); }
    std::env::set_var("ATRIUM_FRESCO_SOCKET", &sock);

    assert_eq!(atrium::atrium_window_frame_begin(1), 0);
    let r = unsafe {
        atrium::atrium_window_frame_glyph_run(
            1, 1, 64, 64,
            0.0, 0.0,
            1.0, 1.0, 1.0, 1.0,
            std::ptr::null(),
            0,
        )
    };
    assert_eq!(r, 0);
    assert_eq!(atrium::atrium_window_frame_end(), 0);

    std::env::remove_var("ATRIUM_FRESCO_SOCKET");
    atrium::atrium_window_disconnect();
    thread::sleep(Duration::from_millis(50));

    let runs = cap.lock().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].run.glyphs.len(), 0);
}
