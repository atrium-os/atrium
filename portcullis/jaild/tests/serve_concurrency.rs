//! jaild must serve every client while any one connection stays open.
//!
//! atrium-portcullisd-bootstrap holds a single jaild connection for its whole
//! supervisor lifetime. When jaild handled connections one at a time, that
//! starved every other client — portcullisd-daemon's aqueduct AttachMount
//! waited in the listen backlog until the bootstrap exited, and the attach
//! smoke timed out at boot. These tests drive the real `serve` loop (dry-run)
//! over a real unix socket.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use jaild::protocol::{read_frame, write_frame};
use jaild::{Request, Response};
use jaild_policy::Policy;

/// Reply deadline. A starved client waits forever, so every read is bounded.
const DEADLINE: Duration = Duration::from_secs(3);

struct Server {
    sock: PathBuf,
    _dir: tempfile::TempDir,
}

fn start() -> Server {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("jaild.sock");
    let state = dir.path().join("jaild.state.toml");
    let policy_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap()      // portcullis/
        .parent().unwrap()      // bsd/
        .join("etc/jaild.policy.toml");
    let policy = Policy::load(policy_path).expect("load shipped policy");
    let listener = jaild::server::bind(&sock).unwrap();
    std::thread::spawn(move || {
        let _ = jaild::server::serve(&listener, &policy, /*dry_run=*/ true, &state);
    });
    Server { sock, _dir: dir }
}

fn connect(s: &Server) -> UnixStream {
    let c = UnixStream::connect(&s.sock).unwrap();
    c.set_read_timeout(Some(DEADLINE)).unwrap();
    c
}

fn send_ping(c: &mut UnixStream) {
    write_frame(&mut *c, &serde_json::to_vec(&Request::Ping).unwrap()).unwrap();
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
    let s = start();
    let mut a = connect(&s);
    send_ping(&mut a);
    expect_ok(&mut a, "A");

    // A stays connected and idle — the bootstrap's shape.
    let mut b = connect(&s);
    send_ping(&mut b);
    expect_ok(&mut b, "B (while A is open)");

    // And A is still served afterwards.
    send_ping(&mut a);
    expect_ok(&mut a, "A (again)");
}

#[test]
fn half_sent_frame_does_not_block_other_clients() {
    let s = start();
    let mut a = connect(&s);
    let body = serde_json::to_vec(&Request::Ping).unwrap();
    let mut framed = (body.len() as u32).to_le_bytes().to_vec();
    framed.extend_from_slice(&body);

    // A sends two bytes of its length header and stalls. A blocking
    // read_frame on A would hold the whole server here.
    a.write_all(&framed[..2]).unwrap();
    std::thread::sleep(Duration::from_millis(100));

    let mut b = connect(&s);
    send_ping(&mut b);
    expect_ok(&mut b, "B (while A is mid-frame)");

    a.write_all(&framed[2..]).unwrap();
    expect_ok(&mut a, "A (completed frame)");
}

#[test]
fn pipelined_requests_are_all_answered_without_starving_others() {
    let s = start();
    let mut a = connect(&s);
    let mut b = connect(&s);
    for _ in 0..5 {
        send_ping(&mut a);
    }
    send_ping(&mut b);
    expect_ok(&mut b, "B (while A pipelines)");
    for i in 0..5 {
        expect_ok(&mut a, &format!("A pipelined #{i}"));
    }
}

#[test]
fn client_closing_mid_frame_leaves_server_serving() {
    let s = start();
    {
        let mut a = connect(&s);
        a.write_all(&[0x10, 0x00, 0x00, 0x00, b'{']).unwrap();
    } // closed mid-frame
    let mut garbage = connect(&s);
    garbage.write_all(&u32::MAX.to_le_bytes()).unwrap(); // oversized frame
    let mut b = connect(&s);
    send_ping(&mut b);
    expect_ok(&mut b, "B (after a truncated and an oversized client)");
}
