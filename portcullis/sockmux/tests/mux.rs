//! The multiplexer's contract, on an echo server: every client is served
//! while others hold connections open, stall mid-frame, or pipeline.

use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::time::Duration;

use sockmux::{LengthPrefixed, Mux};

const DEADLINE: Duration = Duration::from_secs(3);
const MAX_FRAME: u32 = 64 * 1024;

/// Echo server: replies to each frame with the same frame. A body of
/// "slow" makes the server take 200 ms first, so a test can tell whether
/// another client waited behind it.
fn start() -> (PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("mux.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    std::thread::spawn(move || {
        let mut mux: Mux<LengthPrefixed> = Mux::new(listener).unwrap();
        let mut admit = |s: UnixStream| Some(LengthPrefixed::new(s, MAX_FRAME));
        loop {
            for fd in mux.next_round(&mut admit).unwrap() {
                let Some(c) = mux.session_mut(fd) else { continue };
                match c.take_frame() {
                    Ok(Some(body)) => {
                        if body == b"slow" {
                            std::thread::sleep(Duration::from_millis(200));
                        }
                        let mut out = (body.len() as u32).to_le_bytes().to_vec();
                        out.extend_from_slice(&body);
                        if c.stream().write_all(&out).is_err() {
                            mux.close(fd);
                        }
                    }
                    Ok(None) => {}
                    Err(_) => mux.close(fd),
                }
            }
        }
    });
    (sock, dir)
}

fn connect(sock: &PathBuf) -> UnixStream {
    let c = UnixStream::connect(sock).unwrap();
    c.set_read_timeout(Some(DEADLINE)).unwrap();
    c
}

fn frame(body: &[u8]) -> Vec<u8> {
    let mut f = (body.len() as u32).to_le_bytes().to_vec();
    f.extend_from_slice(body);
    f
}

fn expect_echo(c: &mut UnixStream, body: &[u8], who: &str) {
    let mut got = vec![0u8; 4 + body.len()];
    if let Err(e) = c.read_exact(&mut got) {
        panic!("{who}: no echo within {DEADLINE:?}: {e}");
    }
    assert_eq!(got, frame(body), "{who}: wrong echo");
}

#[test]
fn idle_connection_does_not_block_a_second_client() {
    let (sock, _dir) = start();
    let mut a = connect(&sock);
    a.write_all(&frame(b"a1")).unwrap();
    expect_echo(&mut a, b"a1", "A");

    let mut b = connect(&sock);
    b.write_all(&frame(b"b1")).unwrap();
    expect_echo(&mut b, b"b1", "B while A idles");

    a.write_all(&frame(b"a2")).unwrap();
    expect_echo(&mut a, b"a2", "A again");
}

#[test]
fn half_sent_frame_does_not_block_others() {
    let (sock, _dir) = start();
    let fa = frame(b"hello");
    let mut a = connect(&sock);
    a.write_all(&fa[..3]).unwrap();
    std::thread::sleep(Duration::from_millis(100));

    let mut b = connect(&sock);
    b.write_all(&frame(b"b")).unwrap();
    expect_echo(&mut b, b"b", "B while A is mid-frame");

    a.write_all(&fa[3..]).unwrap();
    expect_echo(&mut a, b"hello", "A completed");
}

#[test]
fn pipelined_frames_in_one_write_are_all_served_in_order() {
    let (sock, _dir) = start();
    let mut a = connect(&sock);
    let mut burst = Vec::new();
    for i in 0..20u8 {
        burst.extend_from_slice(&frame(&[i; 3]));
    }
    a.write_all(&burst).unwrap();
    for i in 0..20u8 {
        expect_echo(&mut a, &[i; 3], &format!("A #{i}"));
    }
}

#[test]
fn a_pipelining_client_does_not_starve_a_newcomer() {
    // A queues ten 200 ms requests in one write. Served to completion that
    // is 2 s; round-robin, B's single request waits for at most one of A's.
    let (sock, _dir) = start();
    let mut a = connect(&sock);
    let mut burst = Vec::new();
    for _ in 0..10 {
        burst.extend_from_slice(&frame(b"slow"));
    }
    a.write_all(&burst).unwrap();
    std::thread::sleep(Duration::from_millis(50));

    let mut b = connect(&sock);
    let t0 = std::time::Instant::now();
    b.write_all(&frame(b"b")).unwrap();
    expect_echo(&mut b, b"b", "B behind a pipelining A");
    let waited = t0.elapsed();
    assert!(waited < Duration::from_millis(1000),
        "B waited {waited:?} behind A's queue — not round-robin");
    for i in 0..10 {
        expect_echo(&mut a, b"slow", &format!("A slow #{i}"));
    }
}

#[test]
fn truncated_and_oversized_clients_are_dropped_without_harm() {
    let (sock, _dir) = start();
    {
        let mut t = connect(&sock);
        t.write_all(&[0x10, 0, 0, 0, b'x']).unwrap();
    } // closed mid-frame
    let mut big = connect(&sock);
    big.write_all(&u32::MAX.to_le_bytes()).unwrap();

    let mut b = connect(&sock);
    b.write_all(&frame(b"ok")).unwrap();
    expect_echo(&mut b, b"ok", "B after bad clients");

    // The oversized client was closed: its read ends (EOF or reset).
    let mut buf = [0u8; 1];
    assert!(matches!(big.read(&mut buf), Ok(0) | Err(_)));
}
