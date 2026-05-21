//! End-to-end: spawn vestibulum-macos, drive it via
//! libatrium's atrium_keychain_* C ABI, verify the
//! returned pubkey + signature.

#![cfg(unix)]

use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use std::ffi::CString;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

/// Tests in this file all set $ATRIUM_VESTIBULUM_SOCKET
/// — std::env is process-global. Serialize.
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn vestibulum_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_vestibulum-macos"))
}

fn spawn_daemon(socket: &std::path::Path) -> Child {
    Command::new(vestibulum_binary())
        .env("INSULA_VESTIBULUMD_SOCKET", socket)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn vestibulum-macos")
}

fn wait_for_socket(p: &std::path::Path, timeout: Duration) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if p.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("vestibulum socket did not appear at {}", p.display());
}

#[test]
fn pubkey_then_sign_roundtrip_via_libatrium() {
    let _g = ENV_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("vest.sock");

    let mut daemon = spawn_daemon(&socket);
    wait_for_socket(&socket, Duration::from_secs(3));

    std::env::set_var("ATRIUM_VESTIBULUM_SOCKET", &socket);

    let service = CString::new("com.example.weather").unwrap();
    let mut pubkey = [0u8; 32];
    let n = unsafe {
        atrium::atrium_keychain_pubkey(
            service.as_ptr(),
            pubkey.as_mut_ptr(),
            pubkey.len(),
        )
    };
    assert_eq!(n, 32, "expected 32-byte pubkey, got n={}", n);

    let challenge = b"sign-this-please";
    let mut sig = [0u8; 64];
    let n = unsafe {
        atrium::atrium_keychain_sign(
            service.as_ptr(),
            challenge.as_ptr(),
            challenge.len(),
            sig.as_mut_ptr(),
            sig.len(),
        )
    };
    assert_eq!(n, 64, "expected 64-byte signature, got n={}", n);

    // The returned signature must verify under the
    // returned pubkey — the load-bearing check that
    // the daemon's keychain logic is consistent.
    let vk = VerifyingKey::from_bytes(&pubkey).expect("pubkey decode");
    let sigobj = Signature::from_bytes(&sig);
    vk.verify(challenge, &sigobj).expect("ed25519 verification");

    std::env::remove_var("ATRIUM_VESTIBULUM_SOCKET");
    let _ = daemon.kill();
    let _ = daemon.wait();
}

#[test]
fn same_service_yields_same_pubkey_across_calls() {
    let _g = ENV_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("vest.sock");

    let mut daemon = spawn_daemon(&socket);
    wait_for_socket(&socket, Duration::from_secs(3));
    std::env::set_var("ATRIUM_VESTIBULUM_SOCKET", &socket);

    let service = CString::new("com.example.weather").unwrap();
    let mut pk1 = [0u8; 32];
    let mut pk2 = [0u8; 32];
    unsafe {
        atrium::atrium_keychain_pubkey(service.as_ptr(), pk1.as_mut_ptr(), 32);
        atrium::atrium_keychain_pubkey(service.as_ptr(), pk2.as_mut_ptr(), 32);
    }
    assert_eq!(pk1, pk2, "same service should return same pubkey");

    std::env::remove_var("ATRIUM_VESTIBULUM_SOCKET");
    let _ = daemon.kill();
    let _ = daemon.wait();
}

#[test]
fn different_services_yield_different_pubkeys() {
    let _g = ENV_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("vest.sock");

    let mut daemon = spawn_daemon(&socket);
    wait_for_socket(&socket, Duration::from_secs(3));
    std::env::set_var("ATRIUM_VESTIBULUM_SOCKET", &socket);

    let s1 = CString::new("com.example.weather").unwrap();
    let s2 = CString::new("com.example.mail").unwrap();
    let mut pk1 = [0u8; 32];
    let mut pk2 = [0u8; 32];
    unsafe {
        atrium::atrium_keychain_pubkey(s1.as_ptr(), pk1.as_mut_ptr(), 32);
        atrium::atrium_keychain_pubkey(s2.as_ptr(), pk2.as_mut_ptr(), 32);
    }
    assert_ne!(pk1, pk2, "distinct services should have distinct keypairs");

    std::env::remove_var("ATRIUM_VESTIBULUM_SOCKET");
    let _ = daemon.kill();
    let _ = daemon.wait();
}

#[test]
fn pubkey_call_without_daemon_returns_no_vestibulum() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::remove_var("ATRIUM_VESTIBULUM_SOCKET");

    let service = CString::new("com.example.x").unwrap();
    let mut pubkey = [0u8; 32];
    let n = unsafe {
        atrium::atrium_keychain_pubkey(service.as_ptr(), pubkey.as_mut_ptr(), 32)
    };
    assert_eq!(n, atrium::ATRIUM_ERR_NO_VESTIBULUM);
}

#[test]
fn pubkey_call_with_unreachable_socket_returns_no_vestibulum() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::set_var("ATRIUM_VESTIBULUM_SOCKET", "/does/not/exist/v.sock");

    let service = CString::new("com.example.x").unwrap();
    let mut pubkey = [0u8; 32];
    let n = unsafe {
        atrium::atrium_keychain_pubkey(service.as_ptr(), pubkey.as_mut_ptr(), 32)
    };
    assert_eq!(n, atrium::ATRIUM_ERR_NO_VESTIBULUM);

    std::env::remove_var("ATRIUM_VESTIBULUM_SOCKET");
}
