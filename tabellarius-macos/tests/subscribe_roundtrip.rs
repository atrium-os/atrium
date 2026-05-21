//! End-to-end: spawn `tabellarius-macos`, then drive
//! the daemon through `libatrium`'s
//! `atrium_tabellarius_subscribe` / _unsubscribe /
//! _count ABI.
//!
//! Mirrors the shape of `vestibulum-macos/tests/
//! keychain_roundtrip.rs`.

#![cfg(target_os = "macos")]

use std::ffi::CString;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

// libatrium reads ATRIUM_TABELLARIUS_SOCKET from the
// process env; concurrent tests that mutate process-wide
// env vars must serialize.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn build_neighbor(name: &str) -> PathBuf {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().to_path_buf();
    let crate_dir = workspace.join(name);
    let out = Command::new("cargo")
        .arg("build").arg("--quiet")
        .arg("--manifest-path").arg(crate_dir.join("Cargo.toml"))
        .arg("--bin").arg(name)
        .output()
        .expect("cargo build neighbor");
    assert!(out.status.success(),
            "cargo build {}: {}",
            name, String::from_utf8_lossy(&out.stderr));
    crate_dir.join("target").join("debug").join(name)
}

fn spawn_daemon(bin: &std::path::Path, sock: &std::path::Path, store_dir: &std::path::Path) -> Child {
    Command::new(bin)
        .env("INSULA_TABELLARIUSD_SOCKET", sock)
        .env("INSULA_TABELLARIUSD_STORE", store_dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn tabellarius-macos")
}

fn wait_for_socket(p: &std::path::Path, timeout: Duration) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if p.exists() { return; }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("socket never appeared at {}", p.display());
}

#[test]
fn subscribe_then_unsubscribe_via_libatrium() {
    let _guard = ENV_LOCK.lock().unwrap();
    let bin = build_neighbor("tabellarius-macos");
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("tbd.sock");
    let store = tmp.path().join("subs");

    let mut daemon = spawn_daemon(&bin, &sock, &store);
    wait_for_socket(&sock, Duration::from_secs(3));

    std::env::set_var("ATRIUM_TABELLARIUS_SOCKET", &sock);

    let purpose = CString::new("primary").unwrap();
    let mut key_id = [0i8; 64];
    let mut pubkey = [0u8; 32];

    let n = unsafe {
        atrium::atrium_tabellarius_subscribe(
            purpose.as_ptr(),
            key_id.as_mut_ptr(),
            key_id.len(),
            pubkey.as_mut_ptr(),
        )
    };
    assert!(n > 0, "subscribe should return key_id length, got {}", n);
    assert_ne!(pubkey, [0u8; 32], "pubkey must not be all-zero");

    // count = 1
    let c = atrium::atrium_tabellarius_count();
    assert_eq!(c, 1, "exactly one sub should be active");

    // unsubscribe
    let key_id_cstr = unsafe {
        std::ffi::CStr::from_ptr(key_id.as_ptr())
    };
    let key_id_owned = CString::new(key_id_cstr.to_bytes()).unwrap();
    let r = unsafe { atrium::atrium_tabellarius_unsubscribe(key_id_owned.as_ptr()) };
    assert_eq!(r, 0, "unsubscribe should succeed; got {}", r);

    let c = atrium::atrium_tabellarius_count();
    assert_eq!(c, 0, "no subs after unsubscribe");

    // unknown id
    let bogus = CString::new("0123456789abcdef").unwrap();
    let r = unsafe { atrium::atrium_tabellarius_unsubscribe(bogus.as_ptr()) };
    assert_eq!(r, atrium::ATRIUM_ERR_TABELLARIUS_UNKNOWN_KEY);

    std::env::remove_var("ATRIUM_TABELLARIUS_SOCKET");
    let _ = daemon.kill();
    let _ = daemon.wait();
}

#[test]
fn subs_survive_daemon_restart() {
    let _guard = ENV_LOCK.lock().unwrap();
    let bin = build_neighbor("tabellarius-macos");
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("tbd.sock");
    let store = tmp.path().join("subs");

    // First daemon: subscribe.
    let mut daemon = spawn_daemon(&bin, &sock, &store);
    wait_for_socket(&sock, Duration::from_secs(3));
    std::env::set_var("ATRIUM_TABELLARIUS_SOCKET", &sock);

    let purpose = CString::new("persist").unwrap();
    let mut key_id = [0i8; 64];
    let mut pubkey_first = [0u8; 32];
    let n = unsafe {
        atrium::atrium_tabellarius_subscribe(
            purpose.as_ptr(),
            key_id.as_mut_ptr(),
            key_id.len(),
            pubkey_first.as_mut_ptr(),
        )
    };
    assert!(n > 0);

    // Kill + relaunch fresh daemon on the same store.
    let _ = daemon.kill();
    let _ = daemon.wait();
    let _ = std::fs::remove_file(&sock);

    let mut daemon2 = spawn_daemon(&bin, &sock, &store);
    wait_for_socket(&sock, Duration::from_secs(3));

    // count should still be 1 — the sub persisted to disk.
    let c = atrium::atrium_tabellarius_count();
    assert_eq!(c, 1, "sub must survive daemon restart");

    std::env::remove_var("ATRIUM_TABELLARIUS_SOCKET");
    let _ = daemon2.kill();
    let _ = daemon2.wait();
}

#[test]
fn no_socket_returns_no_tabellarius() {
    let _guard = ENV_LOCK.lock().unwrap();
    std::env::remove_var("ATRIUM_TABELLARIUS_SOCKET");
    let c = atrium::atrium_tabellarius_count();
    assert_eq!(c, atrium::ATRIUM_ERR_NO_TABELLARIUS);
}
