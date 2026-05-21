//! End-to-end: libatrium's atrium_window_upload_texture
//! drives the aqueduct CAS state machine (UPLOAD_BEGIN
//! with inline body for small blobs → wait for
//! UPLOAD_ACK), then sends OP_SLOT_SET on CLASS_DISPLAY
//! to bind the slot. atrium_window_frame_texture then
//! emits SCENE_NODE_SET with op_id = ATRIUM_CORE_TEXTURE.
//!
//! Stub server: ACKs the upload, captures the SLOT_SET
//! + SCENE_NODE_SET, decodes via fresco_protocol to
//! confirm slot_id + texture params survive intact.

#![cfg(unix)]

use aqueduct::classes::CLASS_DISPLAY;
use aqueduct::envelope::{self, Header};
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const CLASS_CORE: u8 = 0;
const OP_UPLOAD_BEGIN:   u16 = 0x01;
const OP_UPLOAD_ACK:     u16 = 0x04;
const OP_SLOT_SET:       u16 = 0x0020;
const OP_SCENE_NODE_SET: u16 = 0x0040;

#[derive(Debug, Default, Clone)]
struct Captured {
    slot_set: Option<fresco_protocol::SlotSetPayload>,
    texture_node: Option<(u32, u32, fresco_protocol::TextureParams)>,
    upload_seen: bool,
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

            if hdr.opcode_class == CLASS_CORE && hdr.op == OP_UPLOAD_BEGIN {
                // UPLOAD_BEGIN body: [32B hash | 8B size | inline data].
                // The hash is the first 32 bytes; echo it back as UPLOAD_ACK.
                let mut hash_bytes = [0u8; 32];
                hash_bytes.copy_from_slice(&payload[..32]);
                cap2.lock().unwrap().upload_seen = true;

                let ack_hdr = Header::new(CLASS_CORE, OP_UPLOAD_ACK, 0, 32);
                let _ = stream.write_all(&ack_hdr.encode());
                let _ = stream.write_all(&hash_bytes);
                let _ = stream.flush();
                continue;
            }

            if hdr.opcode_class == CLASS_DISPLAY && hdr.op == OP_SLOT_SET {
                let p: fresco_protocol::SlotSetPayload =
                    postcard::from_bytes(&payload).expect("decode SlotSet");
                cap2.lock().unwrap().slot_set = Some(p);
                continue;
            }
            if hdr.opcode_class == CLASS_DISPLAY && hdr.op == OP_SCENE_NODE_SET {
                let p: fresco_protocol::SceneNodeSetPayload =
                    postcard::from_bytes(&payload).expect("decode NodeSet");
                if p.op_id == fresco_protocol::scene_ops::ATRIUM_CORE_TEXTURE {
                    let tp: fresco_protocol::TextureParams =
                        postcard::from_bytes(&p.params).expect("decode TextureParams");
                    cap2.lock().unwrap().texture_node = Some((p.node_id, p.op_id, tp));
                }
            }
        }
    });
    cap
}

static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn upload_then_frame_texture_round_trips() {
    let _guard = ENV_LOCK.lock().unwrap();
    atrium::atrium_window_disconnect();

    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("fresco.sock");
    let cap = spawn_stub(sock.clone());
    while !sock.exists() { thread::sleep(Duration::from_millis(5)); }
    std::env::set_var("ATRIUM_FRESCO_SOCKET", &sock);

    // 2x2 RGBA8: solid red.
    let pixels: [u8; 16] = [
        0xff, 0x00, 0x00, 0xff,   0xff, 0x00, 0x00, 0xff,
        0xff, 0x00, 0x00, 0xff,   0xff, 0x00, 0x00, 0xff,
    ];
    let r = unsafe {
        atrium::atrium_window_upload_texture(
            7,                      // window
            42,                     // slot_id
            pixels.as_ptr(), pixels.len(),
            2, 2,
            atrium::ATRIUM_TEX_FORMAT_RGBA8_SRGB,
        )
    };
    assert_eq!(r, 0, "upload should succeed; got {}", r);

    assert_eq!(atrium::atrium_window_frame_begin(7), 0);
    let r = atrium::atrium_window_frame_texture(
        100,        // node_id
        42,         // slot_id (matches upload)
        0.0, 0.0, 64.0, 64.0,
    );
    assert_eq!(r, 0);
    assert_eq!(atrium::atrium_window_frame_end(), 0);

    std::env::remove_var("ATRIUM_FRESCO_SOCKET");
    atrium::atrium_window_disconnect();
    thread::sleep(Duration::from_millis(50));

    let c = cap.lock().unwrap();
    assert!(c.upload_seen, "stub never saw UPLOAD_BEGIN");

    let slot = c.slot_set.as_ref().expect("SlotSet not captured");
    assert_eq!(slot.slot_id, 42);
    match &slot.kind {
        fresco_protocol::SlotKind::Texture(desc) => {
            assert_eq!(desc.width, 2);
            assert_eq!(desc.height, 2);
            assert!(matches!(desc.format,
                fresco_protocol::TextureFormat::Rgba8UnormSrgb));
        }
    }

    let (node_id, op_id, tp) = c.texture_node.as_ref()
        .cloned().expect("texture node not captured");
    assert_eq!(node_id, 100);
    assert_eq!(op_id, fresco_protocol::scene_ops::ATRIUM_CORE_TEXTURE);
    assert_eq!(tp.slot_id, 42);
    assert_eq!(tp.x, 0.0);
    assert_eq!(tp.y, 0.0);
    assert_eq!(tp.w, 64.0);
    assert_eq!(tp.h, 64.0);
}

#[test]
fn upload_texture_with_null_bytes_errors() {
    let _guard = ENV_LOCK.lock().unwrap();
    atrium::atrium_window_disconnect();
    let r = unsafe {
        atrium::atrium_window_upload_texture(
            1, 1, std::ptr::null(), 0, 1, 1,
            atrium::ATRIUM_TEX_FORMAT_RGBA8_SRGB,
        )
    };
    assert_eq!(r, atrium::ATRIUM_ERR_INVALID_PATH);
}

#[test]
fn upload_texture_with_unknown_format_errors() {
    let _guard = ENV_LOCK.lock().unwrap();
    atrium::atrium_window_disconnect();
    let buf = [0u8; 4];
    let r = unsafe {
        atrium::atrium_window_upload_texture(
            1, 1, buf.as_ptr(), buf.len(), 1, 1,
            999,    // not a recognized format
        )
    };
    assert_eq!(r, atrium::ATRIUM_ERR_INVALID_PATH);
}

#[test]
fn frame_texture_outside_frame_errors() {
    let _guard = ENV_LOCK.lock().unwrap();
    atrium::atrium_window_disconnect();
    let r = atrium::atrium_window_frame_texture(
        1, 1, 0.0, 0.0, 1.0, 1.0,
    );
    assert_eq!(r, atrium::ATRIUM_ERR_FRESCO_RPC);
}
