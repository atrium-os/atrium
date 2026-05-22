//! End-to-end: spawn the `tabellarius-relay` daemon,
//! connect a device + a publisher over real TCP, drive
//! the protocol, assert the push routes through.

use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;
use tabellarius_relay::proto::{read_msg, write_msg, ClientMsg, RelayMsg};

fn relay_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tabellarius-relay"))
}

/// Spawn the relay on an ephemeral port; parse the
/// `relay listening on <addr>` line off stdout.
fn spawn_relay() -> (Child, String) {
    let mut child = Command::new(relay_binary())
        .env("INSULA_TABELLARIUS_RELAY_ADDR", "127.0.0.1:0")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tabellarius-relay");

    let stdout = child.stdout.take().expect("relay stdout");
    let mut lines = BufReader::new(stdout).lines();
    let first = lines.next()
        .expect("relay should print a line")
        .expect("readable line");
    let addr = first.strip_prefix("relay listening on ")
        .unwrap_or_else(|| panic!("unexpected first line: {first:?}"))
        .to_string();
    (child, addr)
}

fn connect(addr: &str) -> TcpStream {
    // Brief retry — the listener is up by the time the
    // address line printed, but be forgiving.
    for _ in 0..50 {
        if let Ok(s) = TcpStream::connect(addr) {
            return s;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("could not connect to relay at {addr}");
}

#[test]
fn device_receives_a_publishers_blob_over_tcp() {
    let (mut relay, addr) = spawn_relay();

    let key = [0xabu8; 32];

    // Device connection: subscribe to `key`.
    let mut device = connect(&addr);
    write_msg(&mut device, &ClientMsg::Subscribe { keys: vec![key] })
        .expect("device subscribe");

    // Small beat so the relay processes the subscribe
    // before the publish arrives.
    thread::sleep(Duration::from_millis(100));

    // Publisher connection: publish a blob to `key`.
    let mut publisher = connect(&addr);
    let payload = vec![0xde, 0xad, 0xbe, 0xef];
    write_msg(&mut publisher, &ClientMsg::Publish {
        to_key: key,
        blob: payload.clone(),
        ttl_secs: 60,
    }).expect("publish");

    // Publisher should get an accept with delivered=1.
    let mut pub_reader = std::io::BufReader::new(
        publisher.try_clone().unwrap());
    match read_msg::<_, RelayMsg>(&mut pub_reader).expect("publisher reply") {
        RelayMsg::PublishAccepted { delivered, .. } => {
            assert_eq!(delivered, 1, "blob should reach the one device");
        }
        other => panic!("expected PublishAccepted, got {other:?}"),
    }

    // Device should receive the Push.
    let mut dev_reader = std::io::BufReader::new(
        device.try_clone().unwrap());
    match read_msg::<_, RelayMsg>(&mut dev_reader).expect("device push") {
        RelayMsg::Push { to_key, blob, .. } => {
            assert_eq!(to_key, key);
            assert_eq!(blob, payload);
        }
        other => panic!("expected Push, got {other:?}"),
    }

    let _ = relay.kill();
    let _ = relay.wait();
}

#[test]
fn ping_gets_a_pong_over_tcp() {
    let (mut relay, addr) = spawn_relay();
    let mut conn = connect(&addr);
    write_msg(&mut conn, &ClientMsg::Ping).expect("ping");

    let mut reader = std::io::BufReader::new(conn.try_clone().unwrap());
    match read_msg::<_, RelayMsg>(&mut reader).expect("pong") {
        RelayMsg::Pong => {}
        other => panic!("expected Pong, got {other:?}"),
    }

    let _ = relay.kill();
    let _ = relay.wait();
}

#[test]
fn publish_to_a_key_nobody_wants_delivers_to_zero() {
    let (mut relay, addr) = spawn_relay();
    let mut publisher = connect(&addr);
    write_msg(&mut publisher, &ClientMsg::Publish {
        to_key: [0x01u8; 32],
        blob: vec![1, 2, 3],
        ttl_secs: 1,
    }).expect("publish");

    let mut reader = std::io::BufReader::new(publisher.try_clone().unwrap());
    match read_msg::<_, RelayMsg>(&mut reader).expect("accept") {
        RelayMsg::PublishAccepted { delivered, .. } => {
            assert_eq!(delivered, 0);
        }
        other => panic!("expected PublishAccepted, got {other:?}"),
    }

    let _ = relay.kill();
    let _ = relay.wait();
}

#[test]
fn two_devices_on_the_same_key_both_get_the_push() {
    let (mut relay, addr) = spawn_relay();
    let key = [0x77u8; 32];

    let mut d1 = connect(&addr);
    let mut d2 = connect(&addr);
    write_msg(&mut d1, &ClientMsg::Subscribe { keys: vec![key] }).unwrap();
    write_msg(&mut d2, &ClientMsg::Subscribe { keys: vec![key] }).unwrap();
    thread::sleep(Duration::from_millis(100));

    let mut publisher = connect(&addr);
    write_msg(&mut publisher, &ClientMsg::Publish {
        to_key: key, blob: vec![9, 9], ttl_secs: 1,
    }).unwrap();

    let mut pr = std::io::BufReader::new(publisher.try_clone().unwrap());
    match read_msg::<_, RelayMsg>(&mut pr).unwrap() {
        RelayMsg::PublishAccepted { delivered, .. } => {
            assert_eq!(delivered, 2, "both devices should be reached");
        }
        other => panic!("got {other:?}"),
    }

    for (label, d) in [("d1", &d1), ("d2", &d2)] {
        let mut r = std::io::BufReader::new(d.try_clone().unwrap());
        match read_msg::<_, RelayMsg>(&mut r).unwrap() {
            RelayMsg::Push { blob, .. } => assert_eq!(blob, vec![9, 9]),
            other => panic!("{label} expected Push, got {other:?}"),
        }
    }

    let _ = relay.kill();
    let _ = relay.wait();
}
