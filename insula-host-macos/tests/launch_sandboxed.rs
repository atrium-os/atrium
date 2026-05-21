//! End-to-end sandboxed-launch test.
//!
//! Actually invokes `sandbox-exec` on macOS hosts. Tests
//! are gated on `cfg(target_os = "macos")` and on the
//! tool being present (some sandboxed CI environments
//! strip it).

#![cfg(target_os = "macos")]

use insula_host_macos::{launch, LaunchOptions};
use insula_manifest::Manifest;
use std::path::PathBuf;
use std::process::Command;

const MINIMAL_MANIFEST: &str = r#"
[app]
name = "com.example.echo-test"
version = "0.0.1"
sdk-version = "1.x"

[bundle]
form = "native"
arches = ["aarch64-darwin"]
entry = "bin/echo"
"#;

fn sandbox_exec_available() -> bool {
    Command::new("/usr/bin/sandbox-exec")
        .arg("-h")
        .output()
        .map(|o| o.status.code().is_some()) // exits with any status = present
        .unwrap_or(false)
}

#[test]
fn launches_echo_inside_sandbox() {
    if !sandbox_exec_available() {
        eprintln!("skipping: sandbox-exec not available on this system");
        return;
    }

    let manifest = Manifest::parse(MINIMAL_MANIFEST).unwrap();
    let binary = PathBuf::from("/bin/echo");
    let container = std::env::temp_dir();

    let opts = LaunchOptions {
        binary_path: &binary,
        container_dir: &container,
        args: &["hello-from-sandboxed-insula"],
        capture_output: true,
        log_socket: None,
        vestibulum_socket: None,
        netd_socket: None,
        praeco_socket: None,
        tabellarius_socket: None,
    };

    let mut child = launch(&manifest, &opts)
        .expect("sandbox-exec launch should succeed for /bin/echo");

    let output = child.child.wait_with_output()
        .expect("waiting on child should succeed");

    assert!(output.status.success(),
            "sandboxed /bin/echo should exit successfully; got {:?}, stderr: {}",
            output.status, String::from_utf8_lossy(&output.stderr));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("hello-from-sandboxed-insula"),
            "expected our arg in stdout, got: {:?}", stdout);
}

#[test]
fn sandbox_actually_constrains_writes_outside_container() {
    if !sandbox_exec_available() {
        eprintln!("skipping: sandbox-exec not available on this system");
        return;
    }

    // /bin/sh is allowed by our SBPL (under /bin via the
    // system-libraries permit) but writing to /tmp/foo
    // is NOT inside the container_dir, so the sandbox
    // should reject the write.

    let manifest = Manifest::parse(MINIMAL_MANIFEST).unwrap();
    let binary = PathBuf::from("/bin/sh");
    // Container is a brand-new dir, not /tmp.
    let container = tempfile::tempdir().expect("tempdir");

    // Try to write a file outside the container — this
    // should fail because the SBPL only grants
    // `file* (subpath (param CONTAINER_DIR))`.
    let outside_path = "/tmp/insula-sandbox-leak-test-marker.txt";
    // Clean any stale marker from a prior run first.
    let _ = std::fs::remove_file(outside_path);

    let opts = LaunchOptions {
        binary_path: &binary,
        container_dir: container.path(),
        args: &["-c", &format!(
            "echo leaked > {} 2>/dev/null; \
             test -f {} && echo LEAKED || echo CONFINED",
            outside_path, outside_path
        )],
        capture_output: true,
        log_socket: None,
        vestibulum_socket: None,
        netd_socket: None,
        praeco_socket: None,
        tabellarius_socket: None,
    };

    let mut child = launch(&manifest, &opts)
        .expect("sandbox-exec launch should succeed");

    let output = child.child.wait_with_output()
        .expect("waiting on child should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Clean up if the sandbox failed to prevent the leak.
    let _ = std::fs::remove_file(outside_path);

    // We don't strictly require the shell to exit
    // non-zero — sandbox-exec may let the shell run,
    // and just deny the file write — but the file
    // must not exist afterward and the marker print
    // must report CONFINED.
    assert!(stdout.contains("CONFINED"),
            "sandbox should have prevented the write outside container_dir; \
             stdout was: {:?}", stdout);
}
