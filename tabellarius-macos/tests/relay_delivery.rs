//! End-to-end Phase B push delivery:
//!
//!   1. spawn `tabellarius-relay`
//!   2. spawn `tabellarius-macos` pointed at the relay
//!   3. subscribe via libatrium  -> mints a keypair, the
//!      daemon announces the pubkey to the relay
//!   4. a publisher connects to the relay and submits a
//!      blob addressed to that pubkey
//!   5. the relay fans it to the daemon, which queues it
//!   6. drain via the GET_PUSH op -> the blob comes back
//!
//! Exercises the whole relay path: tabellarius-relay +
//! tabellarius-macos's relay_client + the GET_PUSH op.

#![cfg(target_os = "macos")]

use aqueduct::classes::CLASS_TABELLARIUS;
use aqueduct::envelope::flag;
use aqueduct::Connection;
use std::ffi::CString;
use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};
use tabellarius_relay::proto::{read_msg, write_msg, ClientMsg, RelayMsg};

static ENV_LOCK: Mutex<()> = Mutex::new(());

const OP_GET_PUSH: u16 = 3;

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn build_neighbor(name: &str) -> PathBuf {
    let crate_dir = workspace().join(name);
    let out = Command::new("cargo")
        .arg("build").arg("--quiet")
        .arg("--manifest-path").arg(crate_dir.join("Cargo.toml"))
        .arg("--bin").arg(name)
        .output().expect("cargo build neighbor");
    assert!(out.status.success(),
            "cargo build {}: {}", name, String::from_utf8_lossy(&out.stderr));
    crate_dir.join("target").join("debug").join(name)
}

fn tabellarius_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tabellarius-macos"))
}

/// Spawn the relay; parse its `relay listening on <addr>` line.
fn spawn_relay() -> (Child, String) {
    let bin = build_neighbor("tabellarius-relay");
    let mut child = Command::new(bin)
        .env("INSULA_TABELLARIUS_RELAY_ADDR", "127.0.0.1:0")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tabellarius-relay");
    let stdout = child.stdout.take().unwrap();
    let line = BufReader::new(stdout).lines().next()
        .expect("relay line").expect("readable");
    let addr = line.strip_prefix("relay listening on ")
        .unwrap_or_else(|| panic!("unexpected: {line:?}"))
        .to_string();
    (child, addr)
}

fn wait_for_socket(p: &std::path::Path, timeout: Duration) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if p.exists() { return; }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("socket never appeared at {}", p.display());
}

/// Send OP_GET_PUSH; return the raw response payload.
fn get_push(sock: &std::path::Path) -> Vec<u8> {
    let mut conn = Connection::connect(sock).expect("connect daemon");
    conn.send_message(CLASS_TABELLARIUS, OP_GET_PUSH,
                      flag::RESPONSE_EXPECTED, &[])
        .expect("send get_push");
    loop {
        let msg = conn.recv_message().expect("recv");
        if msg.opcode_class == CLASS_TABELLARIUS
            && msg.op == OP_GET_PUSH
            && (msg.flags & flag::IS_RESPONSE) != 0
        {
            return msg.payload;
        }
    }
}

#[test]
fn push_flows_relay_to_daemon_to_get_push() {
    let _guard = ENV_LOCK.lock().unwrap();

    let (mut relay, relay_addr) = spawn_relay();

    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("tbd.sock");
    let store = tmp.path().join("subs");

    // Daemon, pointed at the relay.
    let mut daemon = Command::new(tabellarius_binary())
        .env("INSULA_TABELLARIUSD_SOCKET", &sock)
        .env("INSULA_TABELLARIUSD_STORE", &store)
        .env("INSULA_TABELLARIUSD_RELAY", &relay_addr)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tabellarius-macos");
    wait_for_socket(&sock, Duration::from_secs(3));

    // Subscribe via libatrium — mints a keypair; the
    // daemon announces the pubkey to the relay.
    std::env::set_var("ATRIUM_TABELLARIUS_SOCKET", &sock);
    let purpose = CString::new("primary").unwrap();
    let mut key_id_buf = [0i8; 64];
    let mut pubkey = [0u8; 32];
    let n = unsafe {
        atrium::atrium_tabellarius_subscribe(
            purpose.as_ptr(),
            key_id_buf.as_mut_ptr(),
            key_id_buf.len(),
            pubkey.as_mut_ptr(),
        )
    };
    assert!(n > 0, "subscribe should succeed; got {}", n);
    let key_id = unsafe {
        std::ffi::CStr::from_ptr(key_id_buf.as_ptr())
    }.to_string_lossy().into_owned();
    std::env::remove_var("ATRIUM_TABELLARIUS_SOCKET");

    // Give the daemon's announce a beat to reach the relay.
    thread::sleep(Duration::from_millis(250));

    // Publisher: connect to the relay, submit a blob
    // addressed to the subscription's pubkey.
    let mut publisher = TcpStream::connect(&relay_addr).expect("publisher connect");
    let blob = b"encrypted-push-payload".to_vec();
    write_msg(&mut publisher, &ClientMsg::Publish {
        to_key: pubkey,
        blob: blob.clone(),
        ttl_secs: 60,
    }).expect("publish");

    // Confirm the relay accepted + delivered to 1 device.
    let mut pr = BufReader::new(publisher.try_clone().unwrap());
    match read_msg::<_, RelayMsg>(&mut pr).expect("publish reply") {
        RelayMsg::PublishAccepted { delivered, .. } => {
            assert_eq!(delivered, 1,
                       "relay should have the daemon as a subscriber");
        }
        other => panic!("expected PublishAccepted, got {other:?}"),
    }

    // Give the daemon's reader thread a beat to queue it.
    thread::sleep(Duration::from_millis(250));

    // Drain via GET_PUSH.
    let resp = get_push(&sock);
    assert!(!resp.is_empty());
    assert_eq!(resp[0], 0, "expected status 0 (a push follows)");
    // [0 | u8 id_len | key_id | u64 ts | blob]
    let id_len = resp[1] as usize;
    let got_key_id = std::str::from_utf8(&resp[2..2 + id_len]).unwrap();
    assert_eq!(got_key_id, key_id,
               "push should be tagged with the subscription's key_id");
    let blob_off = 2 + id_len + 8;
    assert_eq!(&resp[blob_off..], blob.as_slice(),
               "delivered blob must match what the publisher sent");

    // Queue now empty.
    let resp2 = get_push(&sock);
    assert_eq!(resp2, vec![1u8], "second get_push should report empty");

    let _ = daemon.kill();
    let _ = daemon.wait();
    let _ = relay.kill();
    let _ = relay.wait();
}

#[test]
fn push_drains_through_libatrium_get_push_abi() {
    let _guard = ENV_LOCK.lock().unwrap();

    let (mut relay, relay_addr) = spawn_relay();
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("tbd.sock");
    let store = tmp.path().join("subs");

    let mut daemon = Command::new(tabellarius_binary())
        .env("INSULA_TABELLARIUSD_SOCKET", &sock)
        .env("INSULA_TABELLARIUSD_STORE", &store)
        .env("INSULA_TABELLARIUSD_RELAY", &relay_addr)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tabellarius-macos");
    wait_for_socket(&sock, Duration::from_secs(3));

    std::env::set_var("ATRIUM_TABELLARIUS_SOCKET", &sock);

    // Subscribe.
    let purpose = CString::new("abi-test").unwrap();
    let mut key_id_buf = [0i8; 64];
    let mut pubkey = [0u8; 32];
    let n = unsafe {
        atrium::atrium_tabellarius_subscribe(
            purpose.as_ptr(),
            key_id_buf.as_mut_ptr(), key_id_buf.len(),
            pubkey.as_mut_ptr(),
        )
    };
    assert!(n > 0);
    let key_id = unsafe {
        std::ffi::CStr::from_ptr(key_id_buf.as_ptr())
    }.to_string_lossy().into_owned();

    thread::sleep(Duration::from_millis(250));

    // Publish.
    let mut publisher = TcpStream::connect(&relay_addr).unwrap();
    let blob = b"abi-delivered-ciphertext".to_vec();
    write_msg(&mut publisher, &ClientMsg::Publish {
        to_key: pubkey, blob: blob.clone(), ttl_secs: 60,
    }).unwrap();
    let mut pr = BufReader::new(publisher.try_clone().unwrap());
    assert!(matches!(
        read_msg::<_, RelayMsg>(&mut pr).unwrap(),
        RelayMsg::PublishAccepted { delivered: 1, .. }
    ));

    thread::sleep(Duration::from_millis(250));

    // Drain via the libatrium ABI rather than raw aqueduct.
    let mut hdr = atrium::AtriumPushHeader {
        key_id: [0; 64], timestamp: 0, ciphertext_len: 0,
    };
    let mut buf = vec![0u8; atrium::ATRIUM_TABELLARIUS_MAX_PUSH];
    let rc = unsafe {
        atrium::atrium_tabellarius_get_push(
            &mut hdr, buf.as_mut_ptr(), buf.len(),
        )
    };
    assert_eq!(rc, 1, "expected a push from the ABI; got {}", rc);

    let got_key_id = unsafe {
        std::ffi::CStr::from_ptr(hdr.key_id.as_ptr())
    }.to_string_lossy().into_owned();
    assert_eq!(got_key_id, key_id);
    assert_eq!(hdr.ciphertext_len as usize, blob.len());
    assert!(hdr.timestamp > 0, "push should carry a timestamp");
    assert_eq!(&buf[..blob.len()], blob.as_slice());

    // Second call: queue empty -> 0.
    let rc2 = unsafe {
        atrium::atrium_tabellarius_get_push(
            &mut hdr, buf.as_mut_ptr(), buf.len(),
        )
    };
    assert_eq!(rc2, 0, "second get_push should report empty");

    std::env::remove_var("ATRIUM_TABELLARIUS_SOCKET");
    let _ = daemon.kill();
    let _ = daemon.wait();
    let _ = relay.kill();
    let _ = relay.wait();
}

#[test]
fn get_push_abi_with_no_socket_errors() {
    let _guard = ENV_LOCK.lock().unwrap();
    std::env::remove_var("ATRIUM_TABELLARIUS_SOCKET");
    let mut hdr = atrium::AtriumPushHeader {
        key_id: [0; 64], timestamp: 0, ciphertext_len: 0,
    };
    let mut buf = [0u8; 64];
    let rc = unsafe {
        atrium::atrium_tabellarius_get_push(&mut hdr, buf.as_mut_ptr(), buf.len())
    };
    assert_eq!(rc, atrium::ATRIUM_ERR_NO_TABELLARIUS);
}

#[test]
fn push_for_an_app_fires_the_wake_hook() {
    let _guard = ENV_LOCK.lock().unwrap();

    let (mut relay, relay_addr) = spawn_relay();
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("tbd.sock");
    let store = tmp.path().join("subs");

    // Wake-hook stub: a script that records `$1 $2`
    // (app_id key_id) to a marker file, one push per
    // line.
    let hook = tmp.path().join("wake-hook.sh");
    let marker = tmp.path().join("woke.txt");
    std::fs::write(&hook, format!(
        "#!/bin/sh\nprintf '%s %s\\n' \"$1\" \"$2\" >> {}\n",
        marker.display(),
    )).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut p = std::fs::metadata(&hook).unwrap().permissions();
    p.set_mode(0o755);
    std::fs::set_permissions(&hook, p).unwrap();

    let mut daemon = Command::new(tabellarius_binary())
        .env("INSULA_TABELLARIUSD_SOCKET", &sock)
        .env("INSULA_TABELLARIUSD_STORE", &store)
        .env("INSULA_TABELLARIUSD_RELAY", &relay_addr)
        .env("INSULA_TABELLARIUSD_WAKE_CMD", &hook)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tabellarius-macos");
    wait_for_socket(&sock, Duration::from_secs(3));

    // Subscribe AS an app — libatrium derives the app_id
    // from $ATRIUM_CONTAINER_DIR
    // (`<root>/apps/<app_id>/container`).
    let app_id = "com.example.waketest";
    let container = tmp.path()
        .join("inst").join("apps").join(app_id).join("container");
    std::env::set_var("ATRIUM_TABELLARIUS_SOCKET", &sock);
    std::env::set_var("ATRIUM_CONTAINER_DIR", &container);

    let purpose = CString::new("primary").unwrap();
    let mut key_id_buf = [0i8; 64];
    let mut pubkey = [0u8; 32];
    let n = unsafe {
        atrium::atrium_tabellarius_subscribe(
            purpose.as_ptr(),
            key_id_buf.as_mut_ptr(), key_id_buf.len(),
            pubkey.as_mut_ptr(),
        )
    };
    assert!(n > 0);
    let key_id = unsafe {
        std::ffi::CStr::from_ptr(key_id_buf.as_ptr())
    }.to_string_lossy().into_owned();
    std::env::remove_var("ATRIUM_CONTAINER_DIR");
    std::env::remove_var("ATRIUM_TABELLARIUS_SOCKET");

    thread::sleep(Duration::from_millis(250));

    // Publish — should fan out to the daemon, queue the
    // push, AND fire the wake hook.
    let mut publisher = TcpStream::connect(&relay_addr).unwrap();
    write_msg(&mut publisher, &ClientMsg::Publish {
        to_key: pubkey, blob: b"wakeup".to_vec(), ttl_secs: 60,
    }).unwrap();
    let mut pr = BufReader::new(publisher.try_clone().unwrap());
    assert!(matches!(
        read_msg::<_, RelayMsg>(&mut pr).unwrap(),
        RelayMsg::PublishAccepted { delivered: 1, .. }
    ));

    // Give the daemon's reader thread + the spawned
    // hook a beat.
    thread::sleep(Duration::from_millis(400));

    let woke = std::fs::read_to_string(&marker).unwrap_or_default();
    assert!(woke.contains(app_id),
            "wake hook should have run with the app_id; marker: {:?}", woke);
    assert!(woke.contains(&key_id),
            "wake hook should have run with the key_id; marker: {:?}", woke);

    let _ = daemon.kill();
    let _ = daemon.wait();
    let _ = relay.kill();
    let _ = relay.wait();
}

#[test]
fn push_with_no_owning_app_does_not_fire_wake() {
    let _guard = ENV_LOCK.lock().unwrap();

    let (mut relay, relay_addr) = spawn_relay();
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("tbd.sock");
    let store = tmp.path().join("subs");

    let hook = tmp.path().join("wake-hook.sh");
    let marker = tmp.path().join("woke.txt");
    std::fs::write(&hook, format!(
        "#!/bin/sh\nprintf 'fired\\n' >> {}\n", marker.display(),
    )).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut p = std::fs::metadata(&hook).unwrap().permissions();
    p.set_mode(0o755);
    std::fs::set_permissions(&hook, p).unwrap();

    let mut daemon = Command::new(tabellarius_binary())
        .env("INSULA_TABELLARIUSD_SOCKET", &sock)
        .env("INSULA_TABELLARIUSD_STORE", &store)
        .env("INSULA_TABELLARIUSD_RELAY", &relay_addr)
        .env("INSULA_TABELLARIUSD_WAKE_CMD", &hook)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    wait_for_socket(&sock, Duration::from_secs(3));

    // Subscribe WITHOUT $ATRIUM_CONTAINER_DIR — no
    // owning app id. The wake hook must not fire.
    std::env::set_var("ATRIUM_TABELLARIUS_SOCKET", &sock);
    std::env::remove_var("ATRIUM_CONTAINER_DIR");
    let purpose = CString::new("orphan").unwrap();
    let mut key_id_buf = [0i8; 64];
    let mut pubkey = [0u8; 32];
    let n = unsafe {
        atrium::atrium_tabellarius_subscribe(
            purpose.as_ptr(),
            key_id_buf.as_mut_ptr(), key_id_buf.len(),
            pubkey.as_mut_ptr(),
        )
    };
    assert!(n > 0);
    std::env::remove_var("ATRIUM_TABELLARIUS_SOCKET");

    thread::sleep(Duration::from_millis(250));

    let mut publisher = TcpStream::connect(&relay_addr).unwrap();
    write_msg(&mut publisher, &ClientMsg::Publish {
        to_key: pubkey, blob: b"x".to_vec(), ttl_secs: 60,
    }).unwrap();
    let mut pr = BufReader::new(publisher.try_clone().unwrap());
    let _ = read_msg::<_, RelayMsg>(&mut pr);

    thread::sleep(Duration::from_millis(400));

    assert!(!marker.exists(),
            "wake hook must not fire for a subscription with no owning app");

    let _ = daemon.kill();
    let _ = daemon.wait();
    let _ = relay.kill();
    let _ = relay.wait();
}

#[test]
fn get_push_on_empty_queue_reports_empty() {
    let _guard = ENV_LOCK.lock().unwrap();

    // No relay configured — the queue is simply always
    // empty. GET_PUSH should still answer cleanly.
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("tbd.sock");
    let store = tmp.path().join("subs");

    let mut daemon = Command::new(tabellarius_binary())
        .env("INSULA_TABELLARIUSD_SOCKET", &sock)
        .env("INSULA_TABELLARIUSD_STORE", &store)
        .env_remove("INSULA_TABELLARIUSD_RELAY")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tabellarius-macos");
    wait_for_socket(&sock, Duration::from_secs(3));

    let resp = get_push(&sock);
    assert_eq!(resp, vec![1u8], "empty queue -> status 1");

    let _ = daemon.kill();
    let _ = daemon.wait();
}
