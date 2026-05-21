//! End-to-end: spawn the atrium-paint binary against a
//! stub Fresco server, verify the full Pergola
//! lifecycle:
//!
//!   1. The app sends OP_WINDOW_CREATE; stub replies
//!      with a window_id.
//!   2. The app emits frame_begin + scene_node_set +
//!      frame_end (the magenta fill).
//!   3. The stub pushes a CLOSE_REQUESTED event.
//!   4. The app responds with OP_WINDOW_DESTROY and
//!      exits 0.

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

const OP_WINDOW_CREATE:          u16 = 0x0500;
const OP_WINDOW_DESTROY:         u16 = 0x0501;
const OP_SCENE_FRAME_BEGIN:      u16 = 0x0030;
const OP_SCENE_FRAME_END:        u16 = 0x0031;
const OP_SCENE_NODE_SET:         u16 = 0x0040;
const EV_WINDOW_CLOSE_REQUESTED: u16 = 0x0582;

fn write_envelope<W: Write>(stream: &mut W, op: u16, flags: u16, payload: &[u8]) {
    let hdr = Header::new(CLASS_DISPLAY, op, flags, payload.len() as u32);
    let _ = stream.write_all(&hdr.encode());
    let _ = stream.write_all(payload);
    let _ = stream.flush();
}

fn read_one<R: Read>(stream: &mut R) -> Option<(u16, u16, Vec<u8>)> {
    let mut hdr_bytes = [0u8; envelope::HEADER_LEN];
    stream.read_exact(&mut hdr_bytes).ok()?;
    let hdr = Header::decode(&hdr_bytes).ok()?;
    let mut payload = vec![0u8; hdr.length as usize];
    stream.read_exact(&mut payload).ok()?;
    Some((hdr.op, hdr.flags, payload))
}

/// Spin up a stub Fresco server that handles a single
/// atrium-paint session: accept the connection, reply
/// to WINDOW_CREATE, capture the frame emitted by
/// fill_rect, push a CLOSE_REQUESTED, then wait for
/// the app's WINDOW_DESTROY before closing.
fn spawn_stub(socket_path: PathBuf, window_id: u32)
    -> Arc<Mutex<Vec<(u16, u16)>>>
{
    let ops = Arc::new(Mutex::new(Vec::<(u16, u16)>::new()));
    let ops2 = ops.clone();
    thread::spawn(move || {
        let listener = UnixListener::bind(&socket_path).expect("bind");
        let (mut stream, _) = listener.accept().expect("accept");

        // 1. WINDOW_CREATE -> reply with window_id.
        let (op, flags, _payload) = read_one(&mut stream).expect("read create");
        ops2.lock().unwrap().push((op, flags));
        assert_eq!(op, OP_WINDOW_CREATE);
        let reply = postcard::to_stdvec(&window_id).unwrap();
        write_envelope(&mut stream, OP_WINDOW_CREATE, flag::IS_RESPONSE, &reply);

        // 2. Three scene-graph envelopes for the fill.
        for _ in 0..3 {
            let (op, flags, _) = read_one(&mut stream).expect("read scene");
            ops2.lock().unwrap().push((op, flags));
        }

        // 3. Push CLOSE_REQUESTED.
        let close = postcard::to_stdvec(
            &fresco_protocol::WindowCloseRequestedEvent { window_id }
        ).unwrap();
        write_envelope(&mut stream, EV_WINDOW_CLOSE_REQUESTED, 0, &close);

        // 4. Read the app's DESTROY.
        if let Some((op, flags, _)) = read_one(&mut stream) {
            ops2.lock().unwrap().push((op, flags));
        }
        // Linger briefly so the app's destroy isn't
        // dropped on a closed socket.
        thread::sleep(Duration::from_millis(50));
    });
    ops
}

#[test]
fn atrium_paint_runs_full_pergola_lifecycle() {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_atrium-paint"));
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("fresco.sock");

    let ops = spawn_stub(sock.clone(), 7);
    while !sock.exists() {
        thread::sleep(Duration::from_millis(5));
    }

    // Run the app with a tight deadline so it doesn't
    // hang past the close event if anything goes wrong.
    let out = Command::new(&bin)
        .env("ATRIUM_FRESCO_SOCKET", &sock)
        .env("ATRIUM_PAINT_MAX_MS", "3000")
        .output()
        .expect("run atrium-paint");
    assert!(out.status.success(),
            "atrium-paint should exit 0 on close-requested; \
             status={:?} stderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr));

    // Give the stub thread a beat to drain.
    thread::sleep(Duration::from_millis(100));

    let observed = ops.lock().unwrap().clone();
    let just_ops: Vec<u16> = observed.iter().map(|(op, _)| *op).collect();

    // The expected wire sequence the app should drive:
    //   CREATE, FRAME_BEGIN, NODE_SET, FRAME_END, DESTROY
    assert!(just_ops.starts_with(&[
        OP_WINDOW_CREATE,
        OP_SCENE_FRAME_BEGIN,
        OP_SCENE_NODE_SET,
        OP_SCENE_FRAME_END,
    ]), "got: {:?}", just_ops);
    assert!(just_ops.contains(&OP_WINDOW_DESTROY),
            "expected DESTROY after close-requested; got: {:?}", just_ops);

    // Scene-graph envelopes route by window_id via the
    // envelope flags field — same convention
    // fresco-client uses.
    let (frame_begin_op, frame_begin_flags) = observed
        .iter().find(|(op, _)| *op == OP_SCENE_FRAME_BEGIN)
        .copied().unwrap();
    assert_eq!(frame_begin_op, OP_SCENE_FRAME_BEGIN);
    assert_eq!(frame_begin_flags, 7,
               "scene ops must carry window_id in flags");
}

#[test]
fn atrium_paint_exits_1_when_no_socket_env() {
    let bin = PathBuf::from(env!("CARGO_BIN_EXE_atrium-paint"));
    let out = Command::new(&bin)
        .env_remove("ATRIUM_FRESCO_SOCKET")
        .env("ATRIUM_PAINT_MAX_MS", "500")
        .output()
        .expect("run atrium-paint");
    let code = out.status.code().expect("exited");
    assert_eq!(code, 1,
               "no fresco socket should exit 1; stderr: {}",
               String::from_utf8_lossy(&out.stderr));
}
