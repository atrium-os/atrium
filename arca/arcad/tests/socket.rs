//! The wire end to end: the label comes from the kernel's peer uid.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use arca_store::codec::Writer;
use arcad::label::{Label, Resolver};
use arcad::proto::{self, op, status};
use arcad::server::State;

#[test]
fn identity_is_the_peer_uid_not_the_message() {
    let d = tempfile::tempdir().unwrap();
    let me = unsafe { libc::getuid() };
    let reg = d.path().join("registry");
    std::fs::write(&reg, format!("{me} alice org.example.mail {}\n", "07".repeat(32))).unwrap();
    let resolver = Resolver { registry: reg, vestibulum_uid: None, ui_uid: None, allow_unsigned: false };
    let state = Arc::new(Mutex::new(State::new(d.path().join("db")).unwrap()));
    // Vestibulum makes alice's store (directly; it would be another uid).
    let mut w = Writer::new();
    w.str("alice");
    w.raw(&[1; 32]);
    assert_eq!(state.lock().unwrap().handle(&Label::Vestibulum, op::V_CREATE, &w.buf).status, status::OK);

    let (mut client, server) = UnixStream::pair().unwrap();
    let (uid, _) = portcullis_peer::uid_gid(server.as_raw_fd()).unwrap();
    let label = resolver.resolve(uid);
    assert_eq!(label, Label::App { user: "alice".into(), publisher: [7; 32], app_id: "org.example.mail".into() });
    let st = state.clone();
    let t = std::thread::spawn(move || arcad::serve_conn(server, &label, &st));

    // Whatever the app claims in its bytes, it is org.example.mail: the
    // Vestibulum op is refused.
    let mut w = Writer::new();
    w.str("alice");
    w.raw(&[1; 32]);
    proto::write_request(&mut client, op::V_UNLOCK, &w.buf).unwrap();
    assert_eq!(proto::read_reply(&mut client).unwrap().0, status::DENIED);

    let mut w = Writer::new();
    w.u16(1);
    w.u8(2);
    w.str("db");
    w.bytes(b"pw");
    proto::write_request(&mut client, op::STORE, &w.buf).unwrap();
    let (s, body) = proto::read_reply(&mut client).unwrap();
    assert_eq!(s, status::OK);
    assert_eq!(body.len(), 16);

    // An oversized frame ends the connection without reading it.
    client.write_all(&(1u32 << 30).to_le_bytes()).unwrap();
    assert!(t.join().unwrap().is_err());
}
