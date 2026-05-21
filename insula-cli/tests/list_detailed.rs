//! `insula list` rich row output.
//!
//! Install a few bundles with varied manifests, run
//! list, assert the per-row tags surface the right
//! capability sections.

#![cfg(target_os = "macos")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn insula_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_insula"))
}

fn run(install_root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(insula_binary())
        .env("INSULA_INSTALL_ROOT", install_root)
        .args(args)
        .output().expect("insula")
}

fn build_bundle(root: &Path, app_id: &str, body: &str) {
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::write(
        root.join("manifest.toml"),
        format!(
            r#"
[app]
name = "{app_id}"
version = "0.1.0"
sdk-version = "1.x"

[bundle]
form = "native"
arches = ["aarch64-darwin"]
entry = "bin/x"

{body}
"#
        ),
    ).unwrap();
    fs::write(root.join("bin/x"), b"#!/bin/sh\necho hi\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut p = fs::metadata(root.join("bin/x")).unwrap().permissions();
    p.set_mode(0o755);
    fs::set_permissions(root.join("bin/x"), p).unwrap();
}

#[test]
fn list_shows_header_and_per_row_tags() {
    let install_root = tempfile::tempdir().unwrap();

    // App 1: bare. No declared capabilities.
    let b1 = tempfile::tempdir().unwrap();
    build_bundle(b1.path(), "com.example.bare", "");
    let _ = run(install_root.path(), &[
        "install", b1.path().to_str().unwrap(), "--allow-unsigned",
    ]);

    // App 2: storage + 2 network hosts + window.
    let b2 = tempfile::tempdir().unwrap();
    build_bundle(b2.path(), "com.example.rich", r#"
[storage]
data = "10MB"

[network]
hosts = [
  { name = "api.example.com", port = 443, proto = "tcp" },
  { name = "b.example.com", port = 443, proto = "tcp" }
]

[render]
fresco = true
"#);
    let _ = run(install_root.path(), &[
        "install", b2.path().to_str().unwrap(), "--allow-unsigned",
    ]);

    // App 3: raw-network only.
    let b3 = tempfile::tempdir().unwrap();
    build_bundle(b3.path(), "com.example.raw", r#"
[network]
raw-network = true
"#);
    let _ = run(install_root.path(), &[
        "install", b3.path().to_str().unwrap(), "--allow-unsigned",
    ]);

    let out = run(install_root.path(), &["list"]);
    assert!(out.status.success(),
            "list: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Header line.
    assert!(stdout.contains("APP-ID"),  "missing APP-ID header; got: {}", stdout);
    assert!(stdout.contains("VERSION"));
    assert!(stdout.contains("SIG"));
    assert!(stdout.contains("capabilities"));

    // All three apps listed.
    assert!(stdout.contains("com.example.bare"));
    assert!(stdout.contains("com.example.rich"));
    assert!(stdout.contains("com.example.raw"));

    // Bare app's row shows the "no capabilities" marker.
    assert!(stdout.contains("(no capabilities declared)"),
            "bare app row missing the no-caps marker; got: {}", stdout);

    // Rich app's row shows net:2 + storage + window tags.
    assert!(stdout.contains("net:2"), "expected net:2 tag; got: {}", stdout);
    assert!(stdout.contains("storage"));
    assert!(stdout.contains("window"));

    // Raw-network app shows net:raw.
    assert!(stdout.contains("net:raw"), "expected net:raw tag; got: {}", stdout);

    // Unsigned marker present (all installed with --allow-unsigned).
    assert!(stdout.contains("unsigned"));
}

#[test]
fn list_signed_app_shows_signed_marker() {
    let install_root = tempfile::tempdir().unwrap();
    let key_dir = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    build_bundle(b.path(), "com.example.signed-list", "");

    // keygen + trust + sign.
    let _ = run(install_root.path(), &[
        "keygen", "list-pub", key_dir.path().to_str().unwrap(),
    ]);
    let _ = run(install_root.path(), &[
        "publishers", "add", "list-pub",
        key_dir.path().join("list-pub.pub").to_str().unwrap(),
    ]);
    let _ = run(install_root.path(), &[
        "sign", b.path().to_str().unwrap(),
        "--key", key_dir.path().join("list-pub.sk").to_str().unwrap(),
    ]);
    let _ = run(install_root.path(), &[
        "install", b.path().to_str().unwrap(),
    ]);

    let out = run(install_root.path(), &["list"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    // The signed app's row should contain "signed" as its
    // SIG column, but not "unsigned" anywhere on its line.
    let signed_line = stdout.lines()
        .find(|l| l.contains("com.example.signed-list"))
        .expect("signed app row missing");
    assert!(signed_line.contains("signed"));
    assert!(!signed_line.contains("unsigned"),
            "signed app shouldn't show unsigned marker; line: {}", signed_line);
}
