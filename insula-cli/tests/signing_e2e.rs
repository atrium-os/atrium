//! End-to-end signing flow through the CLI:
//!   1. `insula keygen <id> <dir>` generates a keypair.
//!   2. `insula publishers add <id> <pub>` trusts it.
//!   3. `insula sign <bundle> --key <sk>` signs.
//!   4. `insula install <bundle>` succeeds (verified).
//!
//! Plus negative cases:
//!   - Unsigned install (no --allow-unsigned) fails.
//!   - Tampered bundle fails signature verification.
//!   - Wrong-publisher signature is rejected.

#![cfg(target_os = "macos")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn insula_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_insula"))
}

fn run_insula(install_root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(insula_binary())
        .env("INSULA_INSTALL_ROOT", install_root)
        .args(args)
        .output()
        .expect("insula binary should be runnable")
}

/// Build a minimal bundle directory.
fn build_minimal_bundle(root: &Path, app_id: &str) {
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::write(
        root.join("manifest.toml"),
        format!(
            r#"
[app]
name = "{}"
version = "1.0.0"
sdk-version = "1.x"

[bundle]
form = "native"
arches = ["aarch64-darwin"]
entry = "bin/x"
"#,
            app_id
        ),
    )
    .unwrap();
    fs::write(root.join("bin/x"), b"#!/bin/sh\necho hi\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut p = fs::metadata(root.join("bin/x")).unwrap().permissions();
    p.set_mode(0o755);
    fs::set_permissions(root.join("bin/x"), p).unwrap();
}

#[test]
fn signed_bundle_with_trusted_publisher_installs() {
    let install_root = tempfile::tempdir().unwrap();
    let bundle_dir = tempfile::tempdir().unwrap();
    let key_dir = tempfile::tempdir().unwrap();

    // keygen
    let out = run_insula(install_root.path(), &[
        "keygen", "publisher-a", key_dir.path().to_str().unwrap(),
    ]);
    assert!(out.status.success(),
            "keygen failed: {}", String::from_utf8_lossy(&out.stderr));
    let sk = key_dir.path().join("publisher-a.sk");
    let pk = key_dir.path().join("publisher-a.pub");
    assert!(sk.exists() && pk.exists());

    // publishers add
    let out = run_insula(install_root.path(), &[
        "publishers", "add", "publisher-a", pk.to_str().unwrap(),
    ]);
    assert!(out.status.success(),
            "publishers add failed: {}", String::from_utf8_lossy(&out.stderr));

    // publishers list
    let out = run_insula(install_root.path(), &["publishers", "list"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("publisher-a"), "got: {}", stdout);

    // build + sign bundle
    build_minimal_bundle(bundle_dir.path(), "com.example.signedapp");
    let out = run_insula(install_root.path(), &[
        "sign", bundle_dir.path().to_str().unwrap(),
        "--key", sk.to_str().unwrap(),
    ]);
    assert!(out.status.success(),
            "sign failed: {}", String::from_utf8_lossy(&out.stderr));
    assert!(bundle_dir.path().join("signature").exists());

    // install (no --allow-unsigned needed)
    let out = run_insula(install_root.path(), &[
        "install", bundle_dir.path().to_str().unwrap(),
    ]);
    assert!(out.status.success(),
            "install failed: {}\n--stderr--\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("signature verified"));
    assert!(stdout.contains("Installed com.example.signedapp"));
}

#[test]
fn unsigned_install_fails_without_allow_flag() {
    let install_root = tempfile::tempdir().unwrap();
    let bundle_dir = tempfile::tempdir().unwrap();
    build_minimal_bundle(bundle_dir.path(), "com.example.x");

    let out = run_insula(install_root.path(), &[
        "install", bundle_dir.path().to_str().unwrap(),
    ]);
    assert!(!out.status.success(), "install should reject unsigned");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unsigned") || stderr.contains("signature"));
}

#[test]
fn unsigned_install_succeeds_with_allow_flag() {
    let install_root = tempfile::tempdir().unwrap();
    let bundle_dir = tempfile::tempdir().unwrap();
    build_minimal_bundle(bundle_dir.path(), "com.example.dev");

    let out = run_insula(install_root.path(), &[
        "install", bundle_dir.path().to_str().unwrap(), "--allow-unsigned",
    ]);
    assert!(out.status.success(),
            "install with --allow-unsigned should succeed; stderr: {}",
            String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("WARNING"));
}

#[test]
fn tampered_bundle_fails_signature_verify() {
    let install_root = tempfile::tempdir().unwrap();
    let bundle_dir = tempfile::tempdir().unwrap();
    let key_dir = tempfile::tempdir().unwrap();

    let _ = run_insula(install_root.path(), &[
        "keygen", "publisher-a", key_dir.path().to_str().unwrap(),
    ]);
    let _ = run_insula(install_root.path(), &[
        "publishers", "add", "publisher-a",
        key_dir.path().join("publisher-a.pub").to_str().unwrap(),
    ]);

    build_minimal_bundle(bundle_dir.path(), "com.example.x");
    let _ = run_insula(install_root.path(), &[
        "sign", bundle_dir.path().to_str().unwrap(),
        "--key", key_dir.path().join("publisher-a.sk").to_str().unwrap(),
    ]);

    // Tamper: append bytes to the binary.
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(bundle_dir.path().join("bin/x"))
        .unwrap();
    f.write_all(b"# malicious payload\n").unwrap();
    drop(f);

    let out = run_insula(install_root.path(), &[
        "install", bundle_dir.path().to_str().unwrap(),
    ]);
    assert!(!out.status.success(),
            "tampered bundle should fail install");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("signature") || stderr.contains("verify"),
            "stderr: {}", stderr);
}

#[test]
fn signature_from_untrusted_publisher_is_rejected() {
    let install_root = tempfile::tempdir().unwrap();
    let bundle_dir = tempfile::tempdir().unwrap();
    let key_dir_a = tempfile::tempdir().unwrap();
    let key_dir_b = tempfile::tempdir().unwrap();

    // Generate two publishers; trust only A.
    let _ = run_insula(install_root.path(), &[
        "keygen", "publisher-a", key_dir_a.path().to_str().unwrap(),
    ]);
    let _ = run_insula(install_root.path(), &[
        "keygen", "publisher-b", key_dir_b.path().to_str().unwrap(),
    ]);
    let _ = run_insula(install_root.path(), &[
        "publishers", "add", "publisher-a",
        key_dir_a.path().join("publisher-a.pub").to_str().unwrap(),
    ]);

    build_minimal_bundle(bundle_dir.path(), "com.example.x");

    // Sign with publisher-b's key but claim key_id =
    // publisher-a (i.e. attacker pretends to be A).
    let _ = run_insula(install_root.path(), &[
        "sign", bundle_dir.path().to_str().unwrap(),
        "--key", key_dir_b.path().join("publisher-b.sk").to_str().unwrap(),
        "--key-id", "publisher-a",
    ]);

    let out = run_insula(install_root.path(), &[
        "install", bundle_dir.path().to_str().unwrap(),
    ]);
    assert!(!out.status.success(),
            "untrusted publisher signature should fail install");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("untrusted") || stderr.contains("UntrustedPublisher")
            || stderr.contains("publisher") || stderr.contains("signature"),
            "stderr: {}", stderr);
}

#[test]
fn publishers_remove_works() {
    let install_root = tempfile::tempdir().unwrap();
    let key_dir = tempfile::tempdir().unwrap();

    let _ = run_insula(install_root.path(), &[
        "keygen", "pub-x", key_dir.path().to_str().unwrap(),
    ]);
    let _ = run_insula(install_root.path(), &[
        "publishers", "add", "pub-x",
        key_dir.path().join("pub-x.pub").to_str().unwrap(),
    ]);

    // list shows it
    let out = run_insula(install_root.path(), &["publishers", "list"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("pub-x"));

    // remove
    let out = run_insula(install_root.path(), &["publishers", "remove", "pub-x"]);
    assert!(out.status.success(),
            "remove failed: {}", String::from_utf8_lossy(&out.stderr));

    // list no longer shows it
    let out = run_insula(install_root.path(), &["publishers", "list"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("pub-x"));
    assert!(stdout.contains("no trusted publishers"));
}
