//! End-to-end: spawn the insula-clock binary against a
//! stub Fresco server, verify it composes a real
//! multi-primitive frame AND that the startup texture
//! upload bound the hub slot.
//!
//! At startup the app uploads a 32x32 RGBA8
//! checkerboard via the CAS state machine + binds it
//! to slot 100 (the hub texture).
//!
//! Every frame thereafter contains:
//!   -  1 OP_SCENE_FRAME_BEGIN
//!   -  1 RECT     (dial background)
//!   - 14 PATHs    (12 tick marks + 2 hands)
//!   -  1 TEXTURE  (hub, referencing slot 100)
//!   -  1 OP_SCENE_FRAME_END

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
const OP_SLOT_SET:          u16 = 0x0020;

// CAS state machine on CLASS_CORE.
const CLASS_CORE:        u8 = 0;
const OP_UPLOAD_BEGIN:   u16 = 0x01;
const OP_UPLOAD_ACK:     u16 = 0x04;

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
    texture_nodes: usize,
    other_nodes: usize,
    saw_begin: bool,
    saw_end: bool,
}

#[derive(Default)]
struct Captured {
    frames: Vec<OneFrame>,
    upload_seen: bool,
    slot_set: Option<fresco_protocol::SlotSetPayload>,
}

fn spawn_stub(socket_path: PathBuf, window_id: u32)
    -> Arc<Mutex<Captured>>
{
    let cap = Arc::new(Mutex::new(Captured::default()));
    let cap2 = cap.clone();
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

        // Track frames; flush a CLOSE after a couple
        // so the app tears down promptly.
        let mut cur = OneFrame::default();
        let mut frames_drained = 0u64;
        loop {
            let mut hdr_bytes = [0u8; envelope::HEADER_LEN];
            if stream.read_exact(&mut hdr_bytes).is_err() { break; }
            let hdr = Header::decode(&hdr_bytes).unwrap();
            let mut payload = vec![0u8; hdr.length as usize];
            if stream.read_exact(&mut payload).is_err() { break; }

            // CAS upload state machine for the hub
            // texture (CLASS_CORE, runs at startup).
            if hdr.opcode_class == CLASS_CORE && hdr.op == OP_UPLOAD_BEGIN {
                let mut hash_bytes = [0u8; 32];
                hash_bytes.copy_from_slice(&payload[..32]);
                cap2.lock().unwrap().upload_seen = true;
                let ack_hdr = Header::new(CLASS_CORE, OP_UPLOAD_ACK, 0, 32);
                let _ = stream.write_all(&ack_hdr.encode());
                let _ = stream.write_all(&hash_bytes);
                let _ = stream.flush();
                continue;
            }
            if hdr.opcode_class != CLASS_DISPLAY { continue; }

            match hdr.op {
                OP_SLOT_SET => {
                    let p: fresco_protocol::SlotSetPayload =
                        postcard::from_bytes(&payload).expect("SlotSet");
                    cap2.lock().unwrap().slot_set = Some(p);
                }
                OP_SCENE_FRAME_BEGIN => { cur.saw_begin = true; }
                OP_SCENE_NODE_SET => {
                    let node: fresco_protocol::SceneNodeSetPayload =
                        postcard::from_bytes(&payload).expect("NodeSet");
                    if node.op_id == fresco_protocol::scene_ops::ATRIUM_CORE_RECT {
                        cur.rect_nodes += 1;
                    } else if node.op_id == fresco_protocol::scene_ops::ATRIUM_CORE_PATH {
                        cur.path_nodes += 1;
                    } else if node.op_id == fresco_protocol::scene_ops::ATRIUM_CORE_TEXTURE {
                        cur.texture_nodes += 1;
                    } else {
                        cur.other_nodes += 1;
                    }
                }
                OP_SCENE_FRAME_END => {
                    cur.saw_end = true;
                    cap2.lock().unwrap().frames.push(std::mem::take(&mut cur));
                    frames_drained += 1;
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
    cap
}

#[test]
fn insula_clock_composes_multi_primitive_frame() {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_insula-clock"));
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("fresco.sock");
    let cap = spawn_stub(sock.clone(), 11);
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

    let c = cap.lock().unwrap();

    // Startup texture upload happened.
    assert!(c.upload_seen, "stub never saw UPLOAD_BEGIN for hub texture");
    let slot = c.slot_set.as_ref().expect("hub SlotSet not captured");
    assert_eq!(slot.slot_id, 100, "hub texture should bind slot 100");
    match &slot.kind {
        fresco_protocol::SlotKind::Texture(desc) => {
            assert_eq!(desc.width, 32);
            assert_eq!(desc.height, 32);
            assert!(matches!(desc.format,
                fresco_protocol::TextureFormat::Rgba8UnormSrgb));
        }
    }

    // Frames composed correctly.
    assert!(c.frames.len() >= 1,
            "expected at least one full frame; got {}", c.frames.len());
    let f = &c.frames[0];
    assert!(f.saw_begin && f.saw_end, "frame must have begin + end");
    assert_eq!(f.rect_nodes, 1,
               "expected exactly 1 RECT (dial background); got {}",
               f.rect_nodes);
    assert_eq!(f.path_nodes, 14,
               "expected 14 PATHs (12 ticks + 2 hands); got {}",
               f.path_nodes);
    assert_eq!(f.texture_nodes, 1,
               "expected 1 TEXTURE node (hub); got {}", f.texture_nodes);
    assert_eq!(f.other_nodes, 0, "no other node kinds expected");
}
