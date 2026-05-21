//! `insula info` capability-surface coverage:
//!
//! Build a bundle with a generous capability surface,
//! install it, run `insula info`, assert the output
//! contains the structured per-section lines.
//!
//! Also covers the signature-presence shape: a signed
//! install shows the key_id; an unsigned install
//! prints the "(unsigned …)" placeholder.

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

fn build_rich_bundle(root: &Path, app_id: &str) {
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::write(
        root.join("manifest.toml"),
        format!(
            r#"
[app]
name = "{app_id}"
version = "3.1.4"
sdk-version = "1.x"

[bundle]
form = "native"
arches = ["aarch64-darwin"]
entry = "bin/x"

[render]
fresco = true

[input]
keyboard = "focus"
pointer  = "focus"

[network]
hosts = [
  {{ name = "api.example.com", port = 443, proto = "tcp" }},
  {{ name = "telemetry.example.com", port = 443, proto = "tcp" }}
]
raw-network = false

[storage]
data  = "100MB"
cache = "1GB"

[ipc]
services = ["fresco-protocol", "clipboard"]

[compute]
cpu  = "100ms/s"
rss  = "256MB"
wall = "30s"

[background.resident]
entry = "bin/x"
priority = "low"

[entry-points]
"atrium-app" = "open"

[capabilities]
location = true
"#
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
fn info_lists_every_capability_section() {
    let install_root = tempfile::tempdir().unwrap();
    let bundle = tempfile::tempdir().unwrap();
    build_rich_bundle(bundle.path(), "com.example.richapp");

    let _ = run_insula(install_root.path(), &[
        "install", bundle.path().to_str().unwrap(), "--allow-unsigned",
    ]);

    let out = run_insula(install_root.path(), &["info", "com.example.richapp"]);
    assert!(out.status.success(),
            "info: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Identity + paths.
    assert!(stdout.contains("com.example.richapp"));
    assert!(stdout.contains("version:     3.1.4"));
    assert!(stdout.contains("paths:"));
    assert!(stdout.contains("bundle:"));
    assert!(stdout.contains("container:"));

    // Unsigned install — placeholder mentioned.
    assert!(stdout.contains("unsigned"),
            "expected signature line for unsigned install; got: {}", stdout);

    // Per-section lines.
    assert!(stdout.contains("capabilities:"), "got: {}", stdout);
    assert!(stdout.contains("render:"));
    assert!(stdout.contains("fresco=true"));
    assert!(stdout.contains("input:"));
    assert!(stdout.contains("keyboard="));
    assert!(stdout.contains("network:"));
    assert!(stdout.contains("api.example.com:443"));
    assert!(stdout.contains("telemetry.example.com:443"));
    assert!(stdout.contains("storage:"));
    assert!(stdout.contains("100MB"));
    assert!(stdout.contains("1GB"));
    assert!(stdout.contains("ipc:"));
    assert!(stdout.contains("fresco-protocol"));
    assert!(stdout.contains("compute:"));
    assert!(stdout.contains("256MB"));
    assert!(stdout.contains("background.resident"));
    assert!(stdout.contains("entry-points:"));
    assert!(stdout.contains("atrium-app"));
    assert!(stdout.contains("location"));
}

#[test]
fn info_shows_signature_keyid_for_signed_install() {
    let install_root = tempfile::tempdir().unwrap();
    let bundle = tempfile::tempdir().unwrap();
    let key_dir = tempfile::tempdir().unwrap();
    build_rich_bundle(bundle.path(), "com.example.signedinfo");

    // keygen + trust + sign.
    let _ = run_insula(install_root.path(), &[
        "keygen", "pub-info", key_dir.path().to_str().unwrap(),
    ]);
    let _ = run_insula(install_root.path(), &[
        "publishers", "add", "pub-info",
        key_dir.path().join("pub-info.pub").to_str().unwrap(),
    ]);
    let _ = run_insula(install_root.path(), &[
        "sign", bundle.path().to_str().unwrap(),
        "--key", key_dir.path().join("pub-info.sk").to_str().unwrap(),
    ]);

    let _ = run_insula(install_root.path(), &[
        "install", bundle.path().to_str().unwrap(),
    ]);

    let out = run_insula(install_root.path(), &["info", "com.example.signedinfo"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("signature:"),
            "expected signature line; got: {}", stdout);
    assert!(stdout.contains("key_id=pub-info"),
            "expected signature key_id; got: {}", stdout);
    assert!(stdout.contains("pk="),
            "expected pk-prefix; got: {}", stdout);
    assert!(!stdout.contains("unsigned"),
            "signed install must not show the unsigned placeholder");
}
