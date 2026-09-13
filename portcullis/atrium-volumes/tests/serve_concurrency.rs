//! atrium-volumes must serve every client while any one connection stays
//! open. Its loop used to serve connections to completion, one at a time —
//! the shape that let the portcullisd bootstrap's long-lived jaild
//! connection starve the aqueduct attach at every boot. Drives the real
//! `serve` loop over a unix socket.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use atrium_volumes::policy::Policy;
use atrium_volumes::protocol::{read_frame, write_frame};
use atrium_volumes::{Request, Response};

/// Reply deadline. A starved client waits forever, so every read is bounded.
const DEADLINE: Duration = Duration::from_secs(3);

fn start() -> (PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("atrium-volumes.sock");
    let state = dir.path().join("atrium-volumes.state.json");
    let policy_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap()      // portcullis/
        .parent().unwrap()      // bsd/
        .join("etc/volumes.policy.toml");
    let policy = Policy::load(&policy_path).expect("load shipped policy");
    let listener = atrium_volumes::server::bind(&sock).unwrap();
    std::thread::spawn(move || {
        let _ = atrium_volumes::server::serve(&listener, &policy, &state);
    });
    (sock, dir)
}

fn connect(sock: &PathBuf) -> UnixStream {
    let c = UnixStream::connect(sock).unwrap();
    c.set_read_timeout(Some(DEADLINE)).unwrap();
    c
}

fn ping_frame() -> Vec<u8> {
    let body = serde_json::to_vec(&Request::Ping).unwrap();
    let mut framed = (body.len() as u32).to_le_bytes().to_vec();
    framed.extend_from_slice(&body);
    framed
}

fn expect_ok(c: &mut UnixStream, who: &str) {
    let body = match read_frame(&mut *c) {
        Ok(Some(b)) => b,
        other => panic!("{who}: no reply within {DEADLINE:?}: {other:?}"),
    };
    let resp: Response = serde_json::from_slice(&body).unwrap();
    assert!(matches!(resp, Response::Ok), "{who}: unexpected reply {resp:?}");
}

#[test]
fn second_client_is_served_while_first_connection_stays_open() {
    let (sock, _dir) = start();
    let mut a = connect(&sock);
    write_frame(&mut a, &serde_json::to_vec(&Request::Ping).unwrap()).unwrap();
    expect_ok(&mut a, "A");

    let mut b = connect(&sock);
    write_frame(&mut b, &serde_json::to_vec(&Request::Ping).unwrap()).unwrap();
    expect_ok(&mut b, "B (while A is open)");
}

#[test]
fn half_sent_frame_does_not_block_other_clients() {
    let (sock, _dir) = start();
    let framed = ping_frame();
    let mut a = connect(&sock);
    a.write_all(&framed[..2]).unwrap();
    std::thread::sleep(Duration::from_millis(100));

    let mut b = connect(&sock);
    b.write_all(&framed).unwrap();
    expect_ok(&mut b, "B (while A is mid-frame)");

    a.write_all(&framed[2..]).unwrap();
    expect_ok(&mut a, "A (completed frame)");
}
