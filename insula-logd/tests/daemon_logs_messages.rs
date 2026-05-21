//! End-to-end: spawn insula-logd, connect via aqueduct
//! exactly like libatrium would, send log messages,
//! read the daemon's log file, verify contents.

#![cfg(unix)]

use aqueduct::classes::CLASS_LOG;
use aqueduct::envelope::flag;
use aqueduct::Connection;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn logd_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_insula-logd"))
}

/// Spawn the daemon configured to use the given socket
/// + log file paths. Returns the Child so the test can
/// kill it on teardown.
fn spawn_daemon(socket: &std::path::Path, log_file: &std::path::Path) -> Child {
    Command::new(logd_binary())
        .env("INSULA_LOGD_SOCKET", socket)
        .env("INSULA_LOGD_LOG_FILE", log_file)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("insula-logd should be spawnable")
}

/// Wait for the daemon to bind the socket (or fail).
fn wait_for_socket(socket: &std::path::Path, timeout: Duration) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if socket.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("insula-logd did not bind socket within {:?}", timeout);
}

fn send_log_via_aqueduct(socket: &std::path::Path, level: u8, msg: &str) {
    let mut conn = Connection::connect(socket).expect("client connect");
    let mut payload = Vec::with_capacity(msg.len() + 1);
    payload.push(level);
    payload.extend_from_slice(msg.as_bytes());
    conn.send_message(CLASS_LOG, 0, flag::ASYNC_EVENT, &payload)
        .expect("send_message");
    // Connection drops at end of fn; daemon sees EOF and
    // the per-connection thread exits cleanly.
}

#[test]
fn daemon_writes_log_lines_for_each_received_message() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("logd.sock");
    let log_file = tmp.path().join("insula.log");

    let mut daemon = spawn_daemon(&socket, &log_file);
    wait_for_socket(&socket, Duration::from_secs(3));

    // Send three log lines via the same aqueduct shape
    // libatrium uses.
    send_log_via_aqueduct(&socket, 2, "first INFO line");
    send_log_via_aqueduct(&socket, 4, "an ERROR happened");
    send_log_via_aqueduct(&socket, 1, "DEBUG noise");

    // Give the daemon's worker threads a beat to flush.
    thread::sleep(Duration::from_millis(150));

    let log_contents = std::fs::read_to_string(&log_file)
        .expect("log file should exist after daemon writes");

    // One line per send. Format:
    //   YYYY-MM-DDTHH:MM:SS.mmmZ\tLEVEL\tmessage\n
    let lines: Vec<&str> = log_contents.lines().collect();
    assert_eq!(lines.len(), 3, "expected 3 log lines; got: {:?}", log_contents);

    assert!(lines[0].ends_with("INFO\tfirst INFO line"));
    assert!(lines[1].ends_with("ERROR\tan ERROR happened"));
    assert!(lines[2].ends_with("DEBUG\tDEBUG noise"));

    // Each timestamp is ISO-8601 with millisecond
    // precision.
    for l in &lines {
        let ts = &l[..24]; // "YYYY-MM-DDTHH:MM:SS.mmmZ"
        assert!(ts.starts_with('2'),
                "timestamp should start with current millennium: {:?}", ts);
        assert_eq!(ts.len(), 24);
        assert_eq!(&ts[ts.len() - 1..], "Z");
    }

    let _ = daemon.kill();
    let _ = daemon.wait();
}

#[test]
fn daemon_drops_messages_for_unknown_opcodes() {
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("logd.sock");
    let log_file = tmp.path().join("insula.log");

    let mut daemon = spawn_daemon(&socket, &log_file);
    wait_for_socket(&socket, Duration::from_secs(3));

    // Send a CLASS_LOG op=99 message — daemon should
    // ignore.
    {
        let mut conn = Connection::connect(&socket).unwrap();
        conn.send_message(CLASS_LOG, 99, flag::ASYNC_EVENT, b"\x02ignored")
            .unwrap();
    }
    // And a valid op=0.
    send_log_via_aqueduct(&socket, 2, "kept");

    thread::sleep(Duration::from_millis(150));

    let contents = std::fs::read_to_string(&log_file).unwrap_or_default();
    let lines: Vec<&str> = contents.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(lines.len(), 1,
               "only op=0 should be logged; got: {:?}", contents);
    assert!(lines[0].contains("INFO\tkept"));

    let _ = daemon.kill();
    let _ = daemon.wait();
}

#[test]
fn libatrium_can_drive_the_daemon_directly() {
    // Confirms that the wire shape libatrium uses is
    // exactly what the daemon expects — by going
    // through the libatrium ABI rather than crafting
    // the aqueduct message by hand.

    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("logd.sock");
    let log_file = tmp.path().join("insula.log");

    let mut daemon = spawn_daemon(&socket, &log_file);
    wait_for_socket(&socket, Duration::from_secs(3));

    // Point libatrium at the daemon and call the API.
    std::env::set_var("ATRIUM_LOG_SOCKET", &socket);
    assert_eq!(atrium::atrium_init(1, 0), atrium::ATRIUM_OK);

    let msg = std::ffi::CString::new("from libatrium").unwrap();
    unsafe {
        atrium::atrium_log(atrium::ATRIUM_LOG_INFO, msg.as_ptr());
    }
    std::env::remove_var("ATRIUM_LOG_SOCKET");

    // libatrium uses a fire-and-forget event; give the
    // bytes time to flush across the socket and into
    // the daemon's file.
    thread::sleep(Duration::from_millis(200));

    let contents = std::fs::read_to_string(&log_file).unwrap_or_default();
    assert!(contents.contains("INFO\tfrom libatrium"),
            "expected libatrium's log to land in the daemon's file; got: {:?}",
            contents);

    let _ = daemon.kill();
    let _ = daemon.wait();
}
