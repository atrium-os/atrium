//! End-to-end: spawn praeco-macos, drive atrium_notify_post
//! via libatrium, verify the daemon writes the notification
//! to its log file and returns a positive id.

#![cfg(unix)]

use std::ffi::CString;
use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn praeco_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_praeco-macos"))
}

fn spawn_daemon(socket: &std::path::Path, log: &std::path::Path) -> Child {
    Command::new(praeco_binary())
        .env("INSULA_PRAECOD_SOCKET", socket)
        .env("INSULA_PRAECOD_LOG_FILE", log)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn praeco-macos")
}

fn wait_for_socket(p: &std::path::Path, timeout: Duration) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if p.exists() { return; }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("praeco socket did not appear at {}", p.display());
}

#[test]
fn post_returns_id_and_logs_record() {
    let _g = ENV_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("praeco.sock");
    let log = tmp.path().join("praeco.log");

    let mut daemon = spawn_daemon(&sock, &log);
    wait_for_socket(&sock, Duration::from_secs(3));
    std::env::set_var("ATRIUM_PRAECO_SOCKET", &sock);

    let title = CString::new("Hello").unwrap();
    let body = CString::new("This is an Insula notification.").unwrap();
    let id = unsafe {
        atrium::atrium_notify_post(
            title.as_ptr(),
            body.as_ptr(),
            atrium::ATRIUM_NOTIFY_NORMAL,
        )
    };
    assert!(id > 0, "expected positive notification id, got {}", id);

    thread::sleep(Duration::from_millis(150));
    let contents = fs::read_to_string(&log).unwrap_or_default();
    assert!(contents.contains(&format!("id={}", id)),
            "expected id={} in log; got: {:?}", id, contents);
    assert!(contents.contains("urgency=normal"));
    assert!(contents.contains("title=\"Hello\""));
    assert!(contents.contains("body=\"This is an Insula notification.\""));

    std::env::remove_var("ATRIUM_PRAECO_SOCKET");
    let _ = daemon.kill();
    let _ = daemon.wait();
}

#[test]
fn ids_are_monotonic() {
    let _g = ENV_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("praeco.sock");
    let log = tmp.path().join("praeco.log");

    let mut daemon = spawn_daemon(&sock, &log);
    wait_for_socket(&sock, Duration::from_secs(3));
    std::env::set_var("ATRIUM_PRAECO_SOCKET", &sock);

    let mut ids = Vec::new();
    for i in 0..3 {
        let title = CString::new(format!("n{}", i)).unwrap();
        let body = CString::new("...").unwrap();
        let id = unsafe {
            atrium::atrium_notify_post(
                title.as_ptr(),
                body.as_ptr(),
                atrium::ATRIUM_NOTIFY_LOW,
            )
        };
        assert!(id > 0);
        ids.push(id);
    }
    for w in ids.windows(2) {
        assert!(w[1] > w[0],
                "ids should be strictly increasing; got {} then {}", w[0], w[1]);
    }

    std::env::remove_var("ATRIUM_PRAECO_SOCKET");
    let _ = daemon.kill();
    let _ = daemon.wait();
}

#[test]
fn post_without_daemon_returns_no_praeco() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::remove_var("ATRIUM_PRAECO_SOCKET");

    let title = CString::new("x").unwrap();
    let body = CString::new("y").unwrap();
    let r = unsafe {
        atrium::atrium_notify_post(
            title.as_ptr(),
            body.as_ptr(),
            atrium::ATRIUM_NOTIFY_NORMAL,
        )
    };
    assert_eq!(r, atrium::ATRIUM_ERR_NO_PRAECO as i64);
}
