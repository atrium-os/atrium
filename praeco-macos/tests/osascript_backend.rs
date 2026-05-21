//! End-to-end: with INSULA_PRAECOD_BACKEND=osascript,
//! the daemon shells out to osascript per notification.
//! We use a fake osascript script that appends its
//! arguments to a marker file; the test asserts the
//! file contains what we expect.
//!
//! Default (no backend env var set) must remain
//! file-only — that's the existing
//! notification_roundtrip.rs behavior.

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

fn build_libatrium_neighbor() {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().to_path_buf();
    let out = Command::new("cargo")
        .arg("build").arg("--quiet")
        .arg("--manifest-path").arg(workspace.join("libatrium/Cargo.toml"))
        .output().expect("cargo build libatrium");
    assert!(out.status.success(),
            "libatrium: {}", String::from_utf8_lossy(&out.stderr));
}

fn wait_for_socket(p: &std::path::Path, timeout: Duration) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if p.exists() { return; }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("socket never appeared at {}", p.display());
}

fn spawn_with(
    socket: &std::path::Path,
    log: &std::path::Path,
    osascript_bin: &std::path::Path,
) -> Child {
    Command::new(praeco_binary())
        .env("INSULA_PRAECOD_SOCKET", socket)
        .env("INSULA_PRAECOD_LOG_FILE", log)
        .env("INSULA_PRAECOD_BACKEND", "osascript")
        .env("INSULA_PRAECOD_OSASCRIPT_BIN", osascript_bin)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn praeco-macos")
}

#[test]
fn osascript_backend_invokes_external_helper_per_post() {
    let _guard = ENV_LOCK.lock().unwrap();
    build_libatrium_neighbor();

    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("praeco.sock");
    let log_path = tmp.path().join("praeco.log");

    // Fake osascript script that records its argv into
    // a marker file (one notification per line).
    let fake = tmp.path().join("fake-osascript.sh");
    let marker = tmp.path().join("invocations.txt");
    let script = format!(
        "#!/bin/sh\n\
         printf '%s\\n' \"$@\" >> {marker}\n\
         exit 0\n",
        marker = marker.display()
    );
    fs::write(&fake, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut p = fs::metadata(&fake).unwrap().permissions();
    p.set_mode(0o755);
    fs::set_permissions(&fake, p).unwrap();

    let mut daemon = spawn_with(&sock, &log_path, &fake);
    wait_for_socket(&sock, Duration::from_secs(3));

    std::env::set_var("ATRIUM_PRAECO_SOCKET", &sock);
    let title = CString::new("Demo title").unwrap();
    let body = CString::new("Demo body with content").unwrap();
    let id = unsafe { atrium::atrium_notify_post(title.as_ptr(), body.as_ptr(), 1) };
    assert!(id > 0, "post should succeed; got {}", id);

    // Give the daemon a beat to spawn the fake.
    thread::sleep(Duration::from_millis(200));

    let recorded = fs::read_to_string(&marker).unwrap_or_default();
    // Fake script records each argv element on its own
    // line. We expect "-e" + the AppleScript text.
    assert!(recorded.contains("-e"), "marker: {:?}", recorded);
    assert!(recorded.contains("display notification"),
            "expected the AppleScript form; got: {:?}", recorded);
    assert!(recorded.contains("Demo title"));
    assert!(recorded.contains("Demo body with content"));

    // Log path still got the structured record (osascript
    // is additive; it doesn't replace the file log).
    let log_contents = fs::read_to_string(&log_path).unwrap_or_default();
    assert!(log_contents.contains("Demo title"),
            "log should still capture; got: {:?}", log_contents);

    std::env::remove_var("ATRIUM_PRAECO_SOCKET");
    let _ = daemon.kill();
    let _ = daemon.wait();
}

#[test]
fn osascript_backend_sanitizes_embedded_quotes_and_control_chars() {
    let _guard = ENV_LOCK.lock().unwrap();
    build_libatrium_neighbor();

    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("praeco.sock");
    let log_path = tmp.path().join("praeco.log");
    let fake = tmp.path().join("fake-osascript.sh");
    let marker = tmp.path().join("invocations.txt");
    let script = format!(
        "#!/bin/sh\n\
         printf '%s\\n' \"$@\" >> {marker}\n",
        marker = marker.display()
    );
    fs::write(&fake, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut p = fs::metadata(&fake).unwrap().permissions();
    p.set_mode(0o755);
    fs::set_permissions(&fake, p).unwrap();

    let mut daemon = spawn_with(&sock, &log_path, &fake);
    wait_for_socket(&sock, Duration::from_secs(3));

    std::env::set_var("ATRIUM_PRAECO_SOCKET", &sock);
    // Title contains a double-quote (would break the
    // AppleScript literal if not sanitized) and the
    // body contains a tab + newline (control chars).
    let title = CString::new(r#"He said "hi""#).unwrap();
    let body = CString::new("first\tsecond\nthird").unwrap();
    let id = unsafe { atrium::atrium_notify_post(title.as_ptr(), body.as_ptr(), 1) };
    assert!(id > 0);

    thread::sleep(Duration::from_millis(200));
    let recorded = fs::read_to_string(&marker).unwrap_or_default();
    // No literal " in the AppleScript body (other than
    // the literal-wrapping ones from `display notification "..." with title "..."`).
    // Sanitizer replaces " with '.
    assert!(recorded.contains("He said 'hi'"),
            "expected quote-substituted title; got: {:?}", recorded);
    // Control chars (\t, \n) inside the body must be
    // stripped, not passed through.
    assert!(!recorded.contains("first\tsecond"),
            "tab should have been stripped; got: {:?}", recorded);

    std::env::remove_var("ATRIUM_PRAECO_SOCKET");
    let _ = daemon.kill();
    let _ = daemon.wait();
}

#[test]
fn default_backend_does_not_invoke_osascript() {
    let _guard = ENV_LOCK.lock().unwrap();
    build_libatrium_neighbor();

    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("praeco.sock");
    let log_path = tmp.path().join("praeco.log");
    let fake = tmp.path().join("fake-osascript.sh");
    let marker = tmp.path().join("invocations.txt");
    let script = format!(
        "#!/bin/sh\nprintf 'invoked!\\n' >> {marker}\n",
        marker = marker.display()
    );
    fs::write(&fake, script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut p = fs::metadata(&fake).unwrap().permissions();
    p.set_mode(0o755);
    fs::set_permissions(&fake, p).unwrap();

    // Note: NOT setting INSULA_PRAECOD_BACKEND, but
    // still pointing at the fake just to make sure
    // it'd be findable.
    let mut daemon = Command::new(praeco_binary())
        .env("INSULA_PRAECOD_SOCKET", &sock)
        .env("INSULA_PRAECOD_LOG_FILE", &log_path)
        .env("INSULA_PRAECOD_OSASCRIPT_BIN", &fake)
        .env_remove("INSULA_PRAECOD_BACKEND")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    wait_for_socket(&sock, Duration::from_secs(3));

    std::env::set_var("ATRIUM_PRAECO_SOCKET", &sock);
    let title = CString::new("t").unwrap();
    let body = CString::new("b").unwrap();
    let _ = unsafe { atrium::atrium_notify_post(title.as_ptr(), body.as_ptr(), 1) };

    thread::sleep(Duration::from_millis(200));

    // Marker must not exist — the file-only default
    // doesn't invoke osascript.
    assert!(!marker.exists(),
            "default backend must NOT invoke osascript; marker file: {}",
            marker.display());

    std::env::remove_var("ATRIUM_PRAECO_SOCKET");
    let _ = daemon.kill();
    let _ = daemon.wait();
}
