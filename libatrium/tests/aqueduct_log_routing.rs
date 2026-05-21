//! End-to-end test: libatrium's atrium_log routes
//! through an actual Aqueduct unix socket when
//! `ATRIUM_LOG_SOCKET` is set.
//!
//! Stands up an in-process listener that decodes the
//! Aqueduct envelope, runs the libatrium calls, and
//! asserts the listener saw what was sent.
//!
//! Restricted to unix; the Aqueduct connection layer
//! is a `UnixStream` so the test is `cfg(unix)`.

#![cfg(unix)]

use aqueduct::classes::CLASS_ECHO;
use aqueduct::envelope::{self, Header};
use std::ffi::CString;
use std::io::Read;
use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// One decoded message from the test listener.
#[derive(Debug, Clone)]
struct RecvdMessage {
    opcode_class: u8,
    op: u16,
    flags: u16,
    payload: Vec<u8>,
}

/// Spin up a unix-domain-socket listener on a path
/// inside `tempdir`. Returns the socket path + a shared
/// vec where decoded messages accumulate.
fn spawn_listener(socket_path: std::path::PathBuf)
    -> Arc<Mutex<Vec<RecvdMessage>>>
{
    let recv = Arc::new(Mutex::new(Vec::<RecvdMessage>::new()));
    let recv_clone = recv.clone();

    thread::spawn(move || {
        let listener = UnixListener::bind(&socket_path)
            .expect("listener bind");
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };

            // Read messages off the stream until close.
            loop {
                let mut header_buf = [0u8; envelope::HEADER_LEN];
                match stream.read_exact(&mut header_buf) {
                    Ok(()) => (),
                    Err(_) => break,
                }
                let header = match Header::decode(&header_buf) {
                    Ok(h) => h,
                    Err(_) => break,
                };
                let mut payload = vec![0u8; header.length as usize];
                if stream.read_exact(&mut payload).is_err() {
                    break;
                }
                recv_clone.lock().unwrap().push(RecvdMessage {
                    opcode_class: header.opcode_class,
                    op: header.op,
                    flags: header.flags,
                    payload,
                });
            }
        }
    });

    recv
}

#[test]
fn atrium_log_routes_through_aqueduct_when_socket_is_set() {
    let tmp = tempfile::tempdir().expect("tmp");
    let socket_path = tmp.path().join("atrium-log.sock");

    let recvd = spawn_listener(socket_path.clone());

    // Give the listener a moment to bind. (Polling
    // would be more robust but slows the test; 20ms is
    // generous and reliable on every CI we've tried.)
    thread::sleep(Duration::from_millis(20));

    // Point libatrium at the listener.
    std::env::set_var("ATRIUM_LOG_SOCKET", &socket_path);

    // Drive libatrium's API surface.
    assert_eq!(atrium::atrium_init(1, 0), atrium::ATRIUM_OK);

    let msg = CString::new("hello-from-aqueduct-test").unwrap();
    unsafe {
        atrium::atrium_log(atrium::ATRIUM_LOG_INFO, msg.as_ptr());
    }

    // Let the listener catch up.
    thread::sleep(Duration::from_millis(50));

    let messages = recvd.lock().unwrap().clone();

    // Find the log message (filter out any CAS-layer
    // chatter from CLASS_CORE if it appears; the
    // envelope crate may send connection-level traffic).
    let log_msgs: Vec<_> = messages.iter()
        .filter(|m| m.opcode_class == CLASS_ECHO)
        .collect();

    assert_eq!(log_msgs.len(), 1,
               "expected one CLASS_ECHO message; got: {:?}", messages);
    let m = log_msgs[0];
    assert_eq!(m.op, 0, "log-forward op");

    // Payload layout: [level_u8 | utf8 bytes].
    assert_eq!(m.payload[0], atrium::ATRIUM_LOG_INFO as u8);
    let text = std::str::from_utf8(&m.payload[1..]).expect("utf8 payload");
    assert_eq!(text, "hello-from-aqueduct-test");

    // Don't leak the env var into other tests.
    std::env::remove_var("ATRIUM_LOG_SOCKET");
}

#[test]
fn atrium_log_falls_back_to_stderr_when_no_socket_set() {
    // Make sure the env var isn't set from a prior
    // test's contamination.
    std::env::remove_var("ATRIUM_LOG_SOCKET");

    // atrium_init returns OK without a socket.
    assert_eq!(atrium::atrium_init(1, 0), atrium::ATRIUM_OK);

    // atrium_log must not crash or block — it just
    // writes to stderr. (We can't easily capture our
    // own process's stderr from inside a Rust test;
    // the smoke test here is "call doesn't panic.")
    let msg = CString::new("fallback-only").unwrap();
    unsafe {
        atrium::atrium_log(atrium::ATRIUM_LOG_INFO, msg.as_ptr());
    }
}

#[test]
fn atrium_init_with_unreachable_socket_still_returns_ok() {
    // ATRIUM_OK is the documented contract even when
    // the platform connection fails (degraded mode).
    std::env::set_var("ATRIUM_LOG_SOCKET", "/does/not/exist/atrium-test.sock");
    assert_eq!(atrium::atrium_init(1, 0), atrium::ATRIUM_OK);
    std::env::remove_var("ATRIUM_LOG_SOCKET");
}

