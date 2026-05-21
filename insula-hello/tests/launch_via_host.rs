//! End-to-end test: launch insula-hello through the
//! macOS host adapter using the manifest in this crate.
//!
//! This is the moment of "an Insula app, built like an
//! Insula app, launched by the host adapter, exercising
//! the libatrium C ABI." If this test passes the bring-
//! up loop is closed at the smallest possible scope.

#![cfg(target_os = "macos")]

use insula_host_macos::{launch, LaunchOptions};
use insula_manifest::Manifest;
use std::path::PathBuf;

fn manifest_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("manifest.toml")
}

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_insula-hello"))
}

fn sandbox_exec_available() -> bool {
    std::process::Command::new("/usr/bin/sandbox-exec")
        .arg("-h")
        .output()
        .map(|o| o.status.code().is_some())
        .unwrap_or(false)
}

#[test]
fn launches_insula_hello_sandboxed_and_observes_expected_stderr() {
    if !sandbox_exec_available() {
        eprintln!("skipping: sandbox-exec not available");
        return;
    }

    // 1. Parse the manifest.
    let manifest_src = std::fs::read_to_string(manifest_path())
        .expect("manifest.toml should be readable from CARGO_MANIFEST_DIR");
    let manifest = Manifest::parse(&manifest_src)
        .expect("insula-hello's manifest should be valid");

    // Sanity-check the manifest matches what we expect
    // to ship (regression guard).
    assert_eq!(manifest.app.name, "com.atrium-os.insula-hello");
    assert_eq!(manifest.bundle.entry, "bin/insula-hello");

    // 2. Find the binary cargo built for us.
    let binary = binary_path();
    assert!(binary.exists(), "cargo should have built insula-hello at {:?}", binary);

    // 3. Provision a container directory.
    let container = tempfile::tempdir().expect("tempdir for sandbox container");

    // 4. Launch through the host adapter.
    let opts = LaunchOptions {
        binary_path: &binary,
        container_dir: container.path(),
        args: &[],
        capture_output: true,
    };

    let child = launch(&manifest, &opts)
        .expect("host adapter should successfully launch insula-hello");

    let output = child.child.wait_with_output()
        .expect("waiting on child should succeed");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);

    // 5. Exit cleanly with status 0.
    assert!(
        output.status.success(),
        "insula-hello should exit cleanly; got {:?}\n--- stderr ---\n{}\n--- stdout ---\n{}",
        output.status, stderr, stdout
    );

    // 6. libatrium emitted the expected lines on stderr.
    // (v0 atrium_log goes to stderr; later slices route
    // via Aqueduct.)
    assert!(
        stderr.contains("atrium_init"),
        "expected atrium_init log line, stderr was: {}", stderr
    );
    assert!(
        stderr.contains("[INFO] hello from Insula"),
        "expected hello-from-Insula log line, stderr was: {}", stderr
    );
    assert!(
        stderr.contains("atrium_exit(0)"),
        "expected atrium_exit log line, stderr was: {}", stderr
    );
}

#[test]
fn manifest_parses() {
    // Lightweight regression test that doesn't depend
    // on sandbox-exec being available — useful for any
    // CI environment that strips it.
    let manifest_src = std::fs::read_to_string(manifest_path()).unwrap();
    let m = Manifest::parse(&manifest_src).expect("manifest should parse");
    assert_eq!(m.app.name, "com.atrium-os.insula-hello");
    assert_eq!(m.app.sdk_version, "1.x");
    assert_eq!(
        m.bundle.form,
        insula_manifest::BundleForm::Native
    );
    assert!(m.storage.is_some());
}
