//! End-to-end: libatrium's atrium_window_poll_event
//! pulls async events pushed by the Fresco scene server
//! on the persistent fresco connection.
//!
//! Stub server: accepts the persistent connection
//! (opened by atrium_window_open), then pushes three
//! events in sequence — close-requested, focus-changed
//! (gained), resized — and asserts the test reads them
//! back with the correct field decoding.

#![cfg(unix)]

use aqueduct::classes::CLASS_DISPLAY;
use aqueduct::envelope::{self, Header};
use std::ffi::CString;
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

const OP_WINDOW_CREATE:          u16 = 0x0500;
const EV_WINDOW_RESIZED:         u16 = 0x0580;
const EV_WINDOW_FOCUS_CHANGED:   u16 = 0x0581;
const EV_WINDOW_CLOSE_REQUESTED: u16 = 0x0582;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn write_envelope<W: Write>(stream: &mut W, op: u16, flags: u16, payload: &[u8]) {
    let hdr = Header::new(CLASS_DISPLAY, op, flags, payload.len() as u32);
    let _ = stream.write_all(&hdr.encode());
    let _ = stream.write_all(payload);
    let _ = stream.flush();
}

#[test]
fn poll_event_decodes_close_focus_resize() {
    let _guard = ENV_LOCK.lock().unwrap();
    atrium::atrium_window_disconnect();

    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("fresco.sock");
    let listener = UnixListener::bind(&sock).expect("bind");

    let sock_for_thread = sock.clone();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        // Read one OP_WINDOW_CREATE request and reply
        // so atrium_window_open returns.
        let mut hdr_bytes = [0u8; envelope::HEADER_LEN];
        stream.read_exact(&mut hdr_bytes).expect("read hdr");
        let hdr = Header::decode(&hdr_bytes).expect("decode hdr");
        let mut payload = vec![0u8; hdr.length as usize];
        stream.read_exact(&mut payload).expect("read payload");
        assert_eq!(hdr.op, OP_WINDOW_CREATE);

        let reply: Vec<u8> = postcard::to_stdvec(&(99u32)).unwrap();
        write_envelope(&mut stream,
                       OP_WINDOW_CREATE,
                       aqueduct::envelope::flag::IS_RESPONSE,
                       &reply);

        // Push three async events.
        let p_close = postcard::to_stdvec(
            &fresco_protocol::WindowCloseRequestedEvent { window_id: 99 }
        ).unwrap();
        write_envelope(&mut stream, EV_WINDOW_CLOSE_REQUESTED, 0, &p_close);

        let p_focus = postcard::to_stdvec(
            &fresco_protocol::WindowFocusChangedEvent {
                window_id: 99, gained: true,
            }
        ).unwrap();
        write_envelope(&mut stream, EV_WINDOW_FOCUS_CHANGED, 0, &p_focus);

        let p_resize = postcard::to_stdvec(
            &fresco_protocol::WindowResizedEvent {
                window_id: 99, width: 1024, height: 768,
            }
        ).unwrap();
        write_envelope(&mut stream, EV_WINDOW_RESIZED, 0, &p_resize);

        // Keep the connection alive a moment so the
        // client can drain.
        thread::sleep(Duration::from_millis(200));
        let _ = sock_for_thread; // suppress unused
    });

    std::env::set_var("ATRIUM_FRESCO_SOCKET", &sock);
    let title = CString::new("evtwin").unwrap();
    let id = unsafe { atrium::atrium_window_open(title.as_ptr(), 10, 10) };
    assert_eq!(id, 99);

    // Drain events. Poll up to 1s; expect close, focus
    // (gained=1), resized (1024x768) in order.
    let mut events: Vec<atrium::AtriumWindowEvent> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(1);
    while events.len() < 3 && Instant::now() < deadline {
        let mut ev = atrium::AtriumWindowEvent {
            kind: 0, _pad: 0, window_id: 0, arg1: 0, arg2: 0,
        };
        let r = unsafe { atrium::atrium_window_poll_event(&mut ev) };
        if r == 1 {
            events.push(ev);
        } else {
            thread::sleep(Duration::from_millis(10));
        }
    }
    std::env::remove_var("ATRIUM_FRESCO_SOCKET");
    atrium::atrium_window_disconnect();
    let _ = handle.join();

    assert_eq!(events.len(), 3,
               "should have drained all three events; got {} {:?}",
               events.len(), events);
    assert_eq!(events[0].kind, atrium::ATRIUM_EV_WINDOW_CLOSE_REQUESTED);
    assert_eq!(events[0].window_id, 99);

    assert_eq!(events[1].kind, atrium::ATRIUM_EV_WINDOW_FOCUS_CHANGED);
    assert_eq!(events[1].window_id, 99);
    assert_eq!(events[1].arg1, 1, "gained=true should encode as 1");

    assert_eq!(events[2].kind, atrium::ATRIUM_EV_WINDOW_RESIZED);
    assert_eq!(events[2].window_id, 99);
    assert_eq!(events[2].arg1, 1024);
    assert_eq!(events[2].arg2, 768);
}

#[test]
fn poll_event_with_no_connection_errors() {
    let _guard = ENV_LOCK.lock().unwrap();
    atrium::atrium_window_disconnect();
    let mut ev = atrium::AtriumWindowEvent {
        kind: 0, _pad: 0, window_id: 0, arg1: 0, arg2: 0,
    };
    let r = unsafe { atrium::atrium_window_poll_event(&mut ev) };
    assert_eq!(r, atrium::ATRIUM_ERR_NO_FRESCO);
}

#[test]
fn poll_event_with_null_out_errors() {
    let _guard = ENV_LOCK.lock().unwrap();
    let r = unsafe { atrium::atrium_window_poll_event(std::ptr::null_mut()) };
    assert_eq!(r, atrium::ATRIUM_ERR_INVALID_PATH);
}
