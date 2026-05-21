//! End-to-end: libatrium's `atrium_window_frame_path`
//! emits a SCENE_NODE_SET carrying a PathParams payload
//! under op_id = ATRIUM_CORE_PATH. Stub server decodes
//! via the canonical fresco_protocol crate and asserts
//! every field round-trips.

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
struct CapturedPath {
    node_id: u32,
    op_id: u32,
    params: fresco_protocol::PathParams,
}

fn spawn_stub(socket_path: std::path::PathBuf) -> Arc<Mutex<Vec<CapturedPath>>> {
    let cap = Arc::new(Mutex::new(Vec::<CapturedPath>::new()));
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
            assert_eq!(hdr.opcode_class, CLASS_DISPLAY);
            if hdr.op == OP_SCENE_NODE_SET {
                let node: fresco_protocol::SceneNodeSetPayload =
                    postcard::from_bytes(&payload).unwrap();
                if node.op_id == fresco_protocol::scene_ops::ATRIUM_CORE_PATH {
                    let params: fresco_protocol::PathParams =
                        postcard::from_bytes(&node.params).unwrap();
                    cap2.lock().unwrap().push(CapturedPath {
                        node_id: node.node_id,
                        op_id: node.op_id,
                        params,
                    });
                }
            }
        }
    });
    cap
}

static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn frame_path_round_trips_fields() {
    let _guard = ENV_LOCK.lock().unwrap();
    atrium::atrium_window_disconnect();

    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("fresco.sock");
    let cap = spawn_stub(sock.clone());
    while !sock.exists() { thread::sleep(Duration::from_millis(5)); }
    std::env::set_var("ATRIUM_FRESCO_SOCKET", &sock);

    assert_eq!(atrium::atrium_window_frame_begin(7), 0);
    // Rotated quad: pivot (50, 60), length 80, width 4,
    // angle 0.5 rad CCW, magenta with half-alpha.
    let r = atrium::atrium_window_frame_path(
        42,
        50.0, 60.0, 80.0, 4.0, 0.5,
        1.0, 0.0, 1.0, 0.5,
    );
    assert_eq!(r, 0);
    assert_eq!(atrium::atrium_window_frame_end(), 0);

    std::env::remove_var("ATRIUM_FRESCO_SOCKET");
    atrium::atrium_window_disconnect();
    thread::sleep(Duration::from_millis(50));

    let paths = cap.lock().unwrap().clone();
    assert_eq!(paths.len(), 1, "expected exactly one path node; got {:?}", paths);
    let p = &paths[0];
    assert_eq!(p.node_id, 42);
    assert_eq!(p.op_id, fresco_protocol::scene_ops::ATRIUM_CORE_PATH);
    assert_eq!(p.params.cx, 50.0);
    assert_eq!(p.params.cy, 60.0);
    assert_eq!(p.params.length, 80.0);
    assert_eq!(p.params.width, 4.0);
    assert_eq!(p.params.angle, 0.5);
    assert_eq!(p.params.r, 1.0);
    assert_eq!(p.params.g, 0.0);
    assert_eq!(p.params.b, 1.0);
    assert_eq!(p.params.a, 0.5);
}

#[test]
fn frame_path_outside_frame_errors() {
    let _guard = ENV_LOCK.lock().unwrap();
    atrium::atrium_window_disconnect();
    let r = atrium::atrium_window_frame_path(
        1, 0.0, 0.0, 1.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
    );
    assert_eq!(r, atrium::ATRIUM_ERR_FRESCO_RPC,
               "path emit without a begin must fail cleanly");
}
