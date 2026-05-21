//! End-to-end: spawn the insula-clock binary against a
//! stub Fresco server, verify it composes a real
//! multi-primitive frame.
//!
//! One frame should contain:
//!   - 1 OP_SCENE_FRAME_BEGIN
//!   - 1 RECT (dial background, op_id ATRIUM_CORE_RECT)
//!   - 12 PATHs (tick marks, op_id ATRIUM_CORE_PATH)
//!   - 2 PATHs (hour + minute hands)
//!   - 1 OP_SCENE_FRAME_END
//! = 17 envelopes per frame.

#![cfg(unix)]

use aqueduct::classes::CLASS_DISPLAY;
use aqueduct::envelope::{self, flag, Header};
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const OP_WINDOW_CREATE:     u16 = 0x0500;
const OP_SCENE_FRAME_BEGIN: u16 = 0x0030;
const OP_SCENE_FRAME_END:   u16 = 0x0031;
const OP_SCENE_NODE_SET:    u16 = 0x0040;

fn write_envelope<W: Write>(stream: &mut W, op: u16, flags: u16, payload: &[u8]) {
    let hdr = Header::new(CLASS_DISPLAY, op, flags, payload.len() as u32);
    let _ = stream.write_all(&hdr.encode());
    let _ = stream.write_all(payload);
    let _ = stream.flush();
}

#[derive(Default)]
struct OneFrame {
    rect_nodes: usize,
    path_nodes: usize,
    other_nodes: usize,
    saw_begin: bool,
    saw_end: bool,
}

fn spawn_stub(socket_path: PathBuf, window_id: u32)
    -> Arc<Mutex<Vec<OneFrame>>>
{
    let frames = Arc::new(Mutex::new(Vec::<OneFrame>::new()));
    let frames2 = frames.clone();
    thread::spawn(move || {
        let listener = UnixListener::bind(&socket_path).expect("bind");
        let (mut stream, _) = listener.accept().expect("accept");

        // Service WINDOW_CREATE.
        let mut hdr_bytes = [0u8; envelope::HEADER_LEN];
        if stream.read_exact(&mut hdr_bytes).is_err() { return; }
        let hdr = Header::decode(&hdr_bytes).unwrap();
        let mut p = vec![0u8; hdr.length as usize];
        if stream.read_exact(&mut p).is_err() { return; }
        assert_eq!(hdr.op, OP_WINDOW_CREATE);
        let reply = postcard::to_stdvec(&window_id).unwrap();
        write_envelope(&mut stream, OP_WINDOW_CREATE, flag::IS_RESPONSE, &reply);

        // Track frames; flush a CLOSE after the first
        // one closes cleanly so the app exits without
        // hanging the test.
        let mut cur = OneFrame::default();
        let mut frames_drained = 0u64;
        loop {
            let mut hdr_bytes = [0u8; envelope::HEADER_LEN];
            if stream.read_exact(&mut hdr_bytes).is_err() { break; }
            let hdr = Header::decode(&hdr_bytes).unwrap();
            let mut payload = vec![0u8; hdr.length as usize];
            if stream.read_exact(&mut payload).is_err() { break; }
            if hdr.opcode_class != CLASS_DISPLAY { continue; }

            match hdr.op {
                OP_SCENE_FRAME_BEGIN => { cur.saw_begin = true; }
                OP_SCENE_NODE_SET => {
                    let node: fresco_protocol::SceneNodeSetPayload =
                        postcard::from_bytes(&payload).expect("NodeSet");
                    if node.op_id == fresco_protocol::scene_ops::ATRIUM_CORE_RECT {
                        cur.rect_nodes += 1;
                    } else if node.op_id == fresco_protocol::scene_ops::ATRIUM_CORE_PATH {
                        cur.path_nodes += 1;
                    } else {
                        cur.other_nodes += 1;
                    }
                }
                OP_SCENE_FRAME_END => {
                    cur.saw_end = true;
                    frames2.lock().unwrap().push(std::mem::take(&mut cur));
                    frames_drained += 1;
                    // After 2 frames, push CLOSE so
                    // the app tears down promptly.
                    if frames_drained == 2 {
                        let close = postcard::to_stdvec(
                            &fresco_protocol::WindowCloseRequestedEvent {
                                window_id,
                            }
                        ).unwrap();
                        write_envelope(&mut stream,
                                       0x0582,        // EV_WINDOW_CLOSE_REQUESTED
                                       0, &close);
                    }
                }
                _ => {}
            }
        }
        thread::sleep(Duration::from_millis(50));
    });
    frames
}

#[test]
fn insula_clock_composes_multi_primitive_frame() {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_insula-clock"));
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("fresco.sock");
    let frames = spawn_stub(sock.clone(), 11);
    while !sock.exists() { thread::sleep(Duration::from_millis(5)); }

    let out = Command::new(&bin)
        .env("ATRIUM_FRESCO_SOCKET", &sock)
        .env("ATRIUM_CLOCK_MAX_MS", "2000")
        .output()
        .expect("run insula-clock");
    assert!(out.status.success(),
            "insula-clock should exit 0; status={:?} stderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr));

    thread::sleep(Duration::from_millis(100));

    let drained = frames.lock().unwrap();
    assert!(drained.len() >= 1,
            "expected at least one full frame; got {}", drained.len());
    let f = &drained[0];
    assert!(f.saw_begin && f.saw_end, "frame must have begin + end");
    // 1 background rect.
    assert_eq!(f.rect_nodes, 1,
               "expected exactly 1 RECT node (dial background); got {}",
               f.rect_nodes);
    // 12 tick paths + 2 hand paths = 14 PATH nodes.
    assert_eq!(f.path_nodes, 14,
               "expected 14 PATH nodes (12 ticks + 2 hands); got {}",
               f.path_nodes);
    assert_eq!(f.other_nodes, 0, "no other node kinds expected");
}
