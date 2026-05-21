//! Capability-diff consent flow on re-install:
//!
//!   1. Install an app with manifest M1 (--allow-unsigned).
//!   2. Re-install with M2 that adds a network host.
//!      Without --accept-changes: fails, lists the new
//!      grants. With --accept-changes: proceeds.
//!   3. Narrowing (M2 -> M1) is silent — no flag needed.
//!   4. First-install (no prior manifest) has no diff
//!      step and proceeds as before.

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

fn build_bundle(root: &Path, app_id: &str, extra_sections: &str) {
    fs::create_dir_all(root.join("bin")).unwrap();
    fs::write(
        root.join("manifest.toml"),
        format!(
            r#"
[app]
name = "{app_id}"
version = "1.0.0"
sdk-version = "1.x"

[bundle]
form = "native"
arches = ["aarch64-darwin"]
entry = "bin/x"

{extra_sections}
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
fn widening_reinstall_blocked_without_accept_flag() {
    let install_root = tempfile::tempdir().unwrap();
    let v1 = tempfile::tempdir().unwrap();
    let v2 = tempfile::tempdir().unwrap();

    // v1: no network section at all.
    build_bundle(v1.path(), "com.example.cap", "");
    // v2: adds a network host.
    build_bundle(
        v2.path(),
        "com.example.cap",
        "[network]\nhosts = [\n  { name = \"api.example.com\", port = 443, proto = \"tcp\" }\n]\n",
    );

    let out = run_insula(install_root.path(), &[
        "install", v1.path().to_str().unwrap(), "--allow-unsigned",
    ]);
    assert!(out.status.success(),
            "v1 install: {}", String::from_utf8_lossy(&out.stderr));

    // Re-install widens — must fail without --accept-changes.
    let out = run_insula(install_root.path(), &[
        "install", v2.path().to_str().unwrap(), "--allow-unsigned",
    ]);
    assert!(!out.status.success(),
            "widening re-install should be refused without --accept-changes");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("widens capabilities"),
            "stderr: {}", stderr);
    assert!(stderr.contains("api.example.com:443"),
            "stderr should name the new host; got: {}", stderr);
    assert!(stderr.contains("--accept-changes"),
            "stderr should mention the override flag; got: {}", stderr);
}

#[test]
fn widening_reinstall_proceeds_with_accept_flag() {
    let install_root = tempfile::tempdir().unwrap();
    let v1 = tempfile::tempdir().unwrap();
    let v2 = tempfile::tempdir().unwrap();

    build_bundle(v1.path(), "com.example.cap2", "");
    build_bundle(
        v2.path(),
        "com.example.cap2",
        "[network]\nhosts = [\n  { name = \"api.example.com\", port = 443, proto = \"tcp\" }\n]\n",
    );

    let _ = run_insula(install_root.path(), &[
        "install", v1.path().to_str().unwrap(), "--allow-unsigned",
    ]);
    let out = run_insula(install_root.path(), &[
        "install", v2.path().to_str().unwrap(),
        "--allow-unsigned", "--accept-changes",
    ]);
    assert!(out.status.success(),
            "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("accepting widened capabilities"),
            "stdout: {}", stdout);
}

#[test]
fn narrowing_reinstall_is_silent() {
    let install_root = tempfile::tempdir().unwrap();
    let wide = tempfile::tempdir().unwrap();
    let narrow = tempfile::tempdir().unwrap();

    build_bundle(
        wide.path(),
        "com.example.cap3",
        "[network]\nhosts = [\n  { name = \"api.example.com\", port = 443, proto = \"tcp\" }\n]\n",
    );
    build_bundle(narrow.path(), "com.example.cap3", "");

    let _ = run_insula(install_root.path(), &[
        "install", wide.path().to_str().unwrap(), "--allow-unsigned",
    ]);
    // Re-install with fewer grants — no flag required.
    let out = run_insula(install_root.path(), &[
        "install", narrow.path().to_str().unwrap(), "--allow-unsigned",
    ]);
    assert!(out.status.success(),
            "narrowing re-install should not require --accept-changes; \
             stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("accepting widened"),
            "narrowing must not print the accept-changes notice");
}

#[test]
fn first_install_skips_diff_step() {
    // Fresh install root, app never installed.
    let install_root = tempfile::tempdir().unwrap();
    let bundle = tempfile::tempdir().unwrap();

    build_bundle(
        bundle.path(),
        "com.example.first",
        "[network]\nhosts = [\n  { name = \"a.example.com\", port = 80, proto = \"tcp\" }\n]\n",
    );

    let out = run_insula(install_root.path(), &[
        "install", bundle.path().to_str().unwrap(), "--allow-unsigned",
    ]);
    assert!(out.status.success(),
            "first install should always proceed; stderr: {}",
            String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("accepting widened"));
}

#[test]
fn raw_network_flip_requires_accept() {
    let install_root = tempfile::tempdir().unwrap();
    let off = tempfile::tempdir().unwrap();
    let on = tempfile::tempdir().unwrap();

    build_bundle(
        off.path(),
        "com.example.rawnet",
        "[network]\nhosts = []\n",
    );
    build_bundle(
        on.path(),
        "com.example.rawnet",
        "[network]\nhosts = []\nraw-network = true\n",
    );

    let _ = run_insula(install_root.path(), &[
        "install", off.path().to_str().unwrap(), "--allow-unsigned",
    ]);
    let out = run_insula(install_root.path(), &[
        "install", on.path().to_str().unwrap(), "--allow-unsigned",
    ]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("raw-network"),
            "expected raw-network in summary; got: {}", stderr);
}
