//! End-to-end: libatrium's atrium_window_fill_rect
//! sends three envelopes on one connection:
//!   - OP_SCENE_FRAME_BEGIN (empty payload)
//!   - OP_SCENE_NODE_SET    (RectParams under node 1)
//!   - OP_SCENE_FRAME_END   (empty payload)
//!
//! All three carry flags = window_id (the "routable"
//! convention fresco-client uses) so the scene server
//! knows which window the frame is for.
//!
//! Decoded via the real fresco-protocol crate so any
//! drift in wire shape between libatrium and the
//! canonical schema fails the test.

#![cfg(unix)]

use aqueduct::classes::CLASS_DISPLAY;
use aqueduct::envelope::{self, Header};
use std::io::Read;
use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const OP_SCENE_FRAME_BEGIN: u16 = 0x0030;
const OP_SCENE_FRAME_END:   u16 = 0x0031;
const OP_SCENE_NODE_SET:    u16 = 0x0040;

#[derive(Debug, Clone)]
struct Captured {
    ops: Vec<(u16, u16)>,   // (op, flags)
    rect: Option<fresco_protocol::RectParams>,
    node_op_id: Option<u32>,
    node_id: Option<u32>,
}

fn spawn_stub(socket_path: std::path::PathBuf) -> Arc<Mutex<Captured>> {
    let cap = Arc::new(Mutex::new(Captured {
        ops: Vec::new(),
        rect: None,
        node_op_id: None,
        node_id: None,
    }));
    let cap2 = cap.clone();
    thread::spawn(move || {
        let listener = UnixListener::bind(&socket_path).expect("bind");
        let (mut stream, _) = match listener.accept() {
            Ok(p) => p,
            Err(_) => return,
        };

        loop {
            let mut hdr_bytes = [0u8; envelope::HEADER_LEN];
            if stream.read_exact(&mut hdr_bytes).is_err() { break; }
            let hdr = match Header::decode(&hdr_bytes) {
                Ok(h) => h,
                Err(_) => break,
            };
            assert_eq!(hdr.opcode_class, CLASS_DISPLAY);
            let mut payload = vec![0u8; hdr.length as usize];
            if stream.read_exact(&mut payload).is_err() { break; }

            let mut c = cap2.lock().unwrap();
            c.ops.push((hdr.op, hdr.flags));
            if hdr.op == OP_SCENE_NODE_SET {
                let node: fresco_protocol::SceneNodeSetPayload =
                    postcard::from_bytes(&payload).expect("decode SceneNodeSet");
                c.node_id = Some(node.node_id);
                c.node_op_id = Some(node.op_id);
                let rect: fresco_protocol::RectParams =
                    postcard::from_bytes(&node.params).expect("decode RectParams");
                c.rect = Some(rect);
            }
        }
    });
    cap
}

static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn fill_rect_emits_begin_node_end_with_window_flags() {
    let _guard = ENV_LOCK.lock().unwrap();
    atrium::atrium_window_disconnect();
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("fresco.sock");
    let cap = spawn_stub(sock.clone());

    while !sock.exists() {
        thread::sleep(Duration::from_millis(5));
    }
    std::env::set_var("ATRIUM_FRESCO_SOCKET", &sock);

    let r = atrium::atrium_window_fill_rect(
        7,                      // window id
        12.0, 34.0, 100.0, 50.0,
        1.0, 0.5, 0.25, 1.0,
    );
    std::env::remove_var("ATRIUM_FRESCO_SOCKET");
    assert_eq!(r, 0, "fill_rect should succeed against a healthy stub");

    // Give the stub a beat to drain.
    thread::sleep(Duration::from_millis(50));

    let c = cap.lock().unwrap();
    // Three ops in order, all with flags = 7 (window id
    // routed via the envelope flags field).
    assert_eq!(c.ops.len(), 3, "expected exactly 3 envelopes; got {:?}", c.ops);
    assert_eq!(c.ops[0], (OP_SCENE_FRAME_BEGIN, 7));
    assert_eq!(c.ops[1], (OP_SCENE_NODE_SET,    7));
    assert_eq!(c.ops[2], (OP_SCENE_FRAME_END,   7));

    // Node was a RECT under id 1.
    assert_eq!(c.node_id, Some(1));
    assert_eq!(c.node_op_id, Some(fresco_protocol::scene_ops::ATRIUM_CORE_RECT));

    let rect = c.rect.expect("rect captured");
    assert_eq!(rect.x, 12.0);
    assert_eq!(rect.y, 34.0);
    assert_eq!(rect.w, 100.0);
    assert_eq!(rect.h, 50.0);
    assert_eq!(rect.r, 1.0);
    assert_eq!(rect.g, 0.5);
    assert_eq!(rect.b, 0.25);
    assert_eq!(rect.a, 1.0);
}

#[test]
fn fill_rect_with_no_socket_returns_no_fresco() {
    let _guard = ENV_LOCK.lock().unwrap();
    atrium::atrium_window_disconnect();
    std::env::remove_var("ATRIUM_FRESCO_SOCKET");
    let r = atrium::atrium_window_fill_rect(
        1, 0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 0.0, 1.0,
    );
    assert_eq!(r, atrium::ATRIUM_ERR_NO_FRESCO);
}

