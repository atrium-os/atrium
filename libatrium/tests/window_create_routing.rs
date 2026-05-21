//! End-to-end test: libatrium's atrium_window_open
//! reaches a Fresco-shaped server over an Aqueduct
//! unix socket, sends a postcard-encoded
//! WindowCreatePayload on CLASS_DISPLAY / OP_WINDOW_CREATE,
//! and parses the server's u32 reply into the returned
//! window id.
//!
//! Doesn't require a real frescod — uses an in-process
//! stub server that knows just enough of the protocol
//! to assert the wire shape + send back a canned id.

#![cfg(unix)]

use aqueduct::classes::CLASS_DISPLAY;
use aqueduct::envelope::{self, Header};
use aqueduct::envelope::flag;
use std::ffi::CString;
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const OP_WINDOW_CREATE: u16 = 0x0500;

#[derive(Debug, Clone)]
struct Captured {
    title: String,
    width: u32,
    height: u32,
}

/// Spin up a stub Fresco server that handles exactly
/// one connection, decodes one OP_WINDOW_CREATE
/// envelope on CLASS_DISPLAY, records the
/// {title,w,h} we saw, and replies with the canned
/// `assigned_id` postcard-encoded as a u32. Returns
/// the recv-side handle the test reads back.
fn spawn_stub(socket_path: std::path::PathBuf, assigned_id: u32)
    -> Arc<Mutex<Option<Captured>>>
{
    let captured = Arc::new(Mutex::new(None));
    let cap_clone = captured.clone();
    thread::spawn(move || {
        let listener = UnixListener::bind(&socket_path).expect("bind");
        let (mut stream, _) = match listener.accept() {
            Ok(p) => p,
            Err(_) => return,
        };

        // Read one envelope header + payload.
        let mut hdr_bytes = [0u8; envelope::HEADER_LEN];
        if stream.read_exact(&mut hdr_bytes).is_err() { return; }
        let hdr = match Header::decode(&hdr_bytes) {
            Ok(h) => h,
            Err(_) => return,
        };
        let mut payload = vec![0u8; hdr.length as usize];
        if stream.read_exact(&mut payload).is_err() { return; }

        assert_eq!(hdr.opcode_class, CLASS_DISPLAY);
        assert_eq!(hdr.op, OP_WINDOW_CREATE);

        // Decode payload via fresco-protocol (the real
        // schema).
        let decoded: fresco_protocol::WindowCreatePayload =
            postcard::from_bytes(&payload).expect("decode WindowCreatePayload");
        *cap_clone.lock().unwrap() = Some(Captured {
            title: decoded.title.clone(),
            width: decoded.width,
            height: decoded.height,
        });

        // Reply: postcard u32 carrying the window id.
        let reply_payload = postcard::to_stdvec(&assigned_id).unwrap();
        let reply_hdr = Header::new(
            CLASS_DISPLAY,
            OP_WINDOW_CREATE,
            flag::IS_RESPONSE,
            reply_payload.len() as u32,
        );
        let mut reply = Vec::with_capacity(envelope::HEADER_LEN + reply_payload.len());
        reply.extend_from_slice(&reply_hdr.encode());
        reply.extend_from_slice(&reply_payload);
        let _ = stream.write_all(&reply);
        let _ = stream.flush();
        // Hold the connection open briefly so libatrium
        // doesn't see a peer-reset before reading.
        thread::sleep(Duration::from_millis(50));
    });
    captured
}

static ENV_LOCK: Mutex<()> = Mutex::new(());

#[test]
fn window_open_encodes_postcard_payload_and_parses_reply() {
    let _guard = ENV_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("fresco.sock");
    let captured = spawn_stub(sock.clone(), 42);

    // Give the listener a beat to bind.
    while !sock.exists() {
        thread::sleep(Duration::from_millis(5));
    }

    std::env::set_var("ATRIUM_FRESCO_SOCKET", &sock);
    let title = CString::new("hello-window").unwrap();
    let id = unsafe { atrium::atrium_window_open(title.as_ptr(), 800, 600) };
    std::env::remove_var("ATRIUM_FRESCO_SOCKET");

    assert_eq!(id, 42, "should return the canned window_id from the stub");

    // Wait for the stub to finish capturing.
    let mut tries = 0;
    let got = loop {
        if let Some(c) = captured.lock().unwrap().clone() { break c; }
        thread::sleep(Duration::from_millis(10));
        tries += 1;
        if tries > 100 { panic!("stub never captured"); }
    };
    assert_eq!(got.title, "hello-window");
    assert_eq!(got.width, 800);
    assert_eq!(got.height, 600);
}

#[test]
fn window_open_with_no_socket_returns_no_fresco() {
    let _guard = ENV_LOCK.lock().unwrap();
    std::env::remove_var("ATRIUM_FRESCO_SOCKET");
    let title = CString::new("x").unwrap();
    let r = unsafe { atrium::atrium_window_open(title.as_ptr(), 1, 1) };
    assert_eq!(r, atrium::ATRIUM_ERR_NO_FRESCO);
}

#[test]
fn window_open_with_null_title_errors() {
    let _guard = ENV_LOCK.lock().unwrap();
    let r = unsafe { atrium::atrium_window_open(std::ptr::null(), 1, 1) };
    assert_eq!(r, atrium::ATRIUM_ERR_INVALID_PATH);
}
