//! End-to-end: libatrium's multi-node frame builder.
//!
//! Drives:
//!   atrium_window_frame_begin(7)
//!   atrium_window_frame_rect(1, ...)  -> red
//!   atrium_window_frame_rect(2, ...)  -> green
//!   atrium_window_frame_rect(3, ...)  -> blue
//!   atrium_window_frame_end()
//!
//! Stub server captures the envelopes and decodes each
//! RECT via fresco_protocol to confirm node_id, op_id,
//! and RectParams arrive intact. Also covers the
//! double-begin and orphan-end error paths.

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
struct CapturedNode {
    node_id: u32,
    op_id: u32,
    rect: fresco_protocol::RectParams,
}

#[derive(Debug, Clone, Default)]
struct Captured {
    ops: Vec<(u16, u16)>,
    nodes: Vec<CapturedNode>,
}

fn spawn_stub(socket_path: std::path::PathBuf) -> Arc<Mutex<Captured>> {
    let cap = Arc::new(Mutex::new(Captured::default()));
    let cap2 = cap.clone();
    thread::spawn(move || {
        let listener = UnixListener::bind(&socket_path).expect("bind");
        let (mut stream, _) = listener.accept().expect("accept");

        loop {
            let mut hdr_bytes = [0u8; envelope::HEADER_LEN];
            if stream.read_exact(&mut hdr_bytes).is_err() { break; }
            let hdr = Header::decode(&hdr_bytes).expect("hdr");
            let mut payload = vec![0u8; hdr.length as usize];
            if stream.read_exact(&mut payload).is_err() { break; }

            assert_eq!(hdr.opcode_class, CLASS_DISPLAY);
            let mut c = cap2.lock().unwrap();
            c.ops.push((hdr.op, hdr.flags));
            if hdr.op == OP_SCENE_NODE_SET {
                let node: fresco_protocol::SceneNodeSetPayload =
                    postcard::from_bytes(&payload).expect("SceneNodeSet");
                let rect: fresco_protocol::RectParams =
                    postcard::from_bytes(&node.params).expect("RectParams");
                c.nodes.push(CapturedNode {
                    node_id: node.node_id,
                    op_id: node.op_id,
                    rect,
                });
            }
        }
    });
    cap
}

static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn frame_builder_emits_three_node_frame() {
    let _guard = ENV_LOCK.lock().unwrap();
    atrium::atrium_window_disconnect();

    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("fresco.sock");
    let cap = spawn_stub(sock.clone());
    while !sock.exists() { thread::sleep(Duration::from_millis(5)); }
    std::env::set_var("ATRIUM_FRESCO_SOCKET", &sock);

    assert_eq!(atrium::atrium_window_frame_begin(7), 0);
    assert_eq!(
        atrium::atrium_window_frame_rect(1, 0.0, 0.0, 10.0, 10.0,
                                         1.0, 0.0, 0.0, 1.0),
        0
    );
    assert_eq!(
        atrium::atrium_window_frame_rect(2, 10.0, 0.0, 10.0, 10.0,
                                         0.0, 1.0, 0.0, 1.0),
        0
    );
    assert_eq!(
        atrium::atrium_window_frame_rect(3, 20.0, 0.0, 10.0, 10.0,
                                         0.0, 0.0, 1.0, 1.0),
        0
    );
    assert_eq!(atrium::atrium_window_frame_end(), 0);

    std::env::remove_var("ATRIUM_FRESCO_SOCKET");
    atrium::atrium_window_disconnect();
    thread::sleep(Duration::from_millis(50));

    let c = cap.lock().unwrap();
    let just_ops: Vec<u16> = c.ops.iter().map(|(o, _)| *o).collect();
    assert_eq!(just_ops, vec![
        OP_SCENE_FRAME_BEGIN,
        OP_SCENE_NODE_SET,
        OP_SCENE_NODE_SET,
        OP_SCENE_NODE_SET,
        OP_SCENE_FRAME_END,
    ], "expected one begin + three nodes + one end");

    // All five envelopes carry flags = window_id = 7.
    for (_, flags) in &c.ops {
        assert_eq!(*flags, 7, "all envelopes route to window 7");
    }

    // Three nodes captured, ids 1/2/3, all RECT op, colors match.
    assert_eq!(c.nodes.len(), 3);
    assert_eq!(c.nodes[0].node_id, 1);
    assert_eq!(c.nodes[0].op_id, fresco_protocol::scene_ops::ATRIUM_CORE_RECT);
    assert_eq!(c.nodes[0].rect.r, 1.0);
    assert_eq!(c.nodes[1].node_id, 2);
    assert_eq!(c.nodes[1].rect.g, 1.0);
    assert_eq!(c.nodes[2].node_id, 3);
    assert_eq!(c.nodes[2].rect.b, 1.0);
}

#[test]
fn double_begin_returns_error() {
    let _guard = ENV_LOCK.lock().unwrap();
    atrium::atrium_window_disconnect();
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("fresco.sock");
    let _cap = spawn_stub(sock.clone());
    while !sock.exists() { thread::sleep(Duration::from_millis(5)); }
    std::env::set_var("ATRIUM_FRESCO_SOCKET", &sock);

    assert_eq!(atrium::atrium_window_frame_begin(1), 0);
    // Second begin without end -> error.
    let r = atrium::atrium_window_frame_begin(1);
    assert_eq!(r, atrium::ATRIUM_ERR_FRESCO_RPC);

    // Clean up so other tests don't inherit state.
    let _ = atrium::atrium_window_frame_end();
    std::env::remove_var("ATRIUM_FRESCO_SOCKET");
    atrium::atrium_window_disconnect();
}

#[test]
fn rect_or_end_without_begin_errors() {
    let _guard = ENV_LOCK.lock().unwrap();
    atrium::atrium_window_disconnect();
    let r = atrium::atrium_window_frame_rect(1, 0.0, 0.0, 1.0, 1.0,
                                             0.0, 0.0, 0.0, 1.0);
    assert_eq!(r, atrium::ATRIUM_ERR_FRESCO_RPC);
    let r = atrium::atrium_window_frame_end();
    assert_eq!(r, atrium::ATRIUM_ERR_FRESCO_RPC);
}
