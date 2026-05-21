//! `insula release` — sign + pack as a single artifact.
//!
//! End-to-end: keygen, trust the publisher, build a
//! bundle dir, run release, then install the produced
//! `.insula` archive cleanly (signature verified).

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

fn build_bundle(root: &Path, app_id: &str) {
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
fn release_signs_then_packs_into_installable_archive() {
    let install_root = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let key_dir = tempfile::tempdir().unwrap();
    let arc_dir = tempfile::tempdir().unwrap();
    let archive_path = arc_dir.path().join("app.insula");

    // keygen + trust the publisher.
    let out = run(install_root.path(), &[
        "keygen", "rel-pub", key_dir.path().to_str().unwrap(),
    ]);
    assert!(out.status.success());
    let out = run(install_root.path(), &[
        "publishers", "add", "rel-pub",
        key_dir.path().join("rel-pub.pub").to_str().unwrap(),
    ]);
    assert!(out.status.success());

    build_bundle(src.path(), "com.example.releaseapp");

    // The big one — single release call.
    let out = run(install_root.path(), &[
        "release",
        src.path().to_str().unwrap(),
        archive_path.to_str().unwrap(),
        "--key", key_dir.path().join("rel-pub.sk").to_str().unwrap(),
    ]);
    assert!(out.status.success(),
            "release failed: stderr={}",
            String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("release complete"),
            "expected completion banner; got: {}", stdout);
    assert!(stdout.contains("com.example.releaseapp"));

    // Artifacts on disk:
    assert!(src.path().join("signature").is_file(),
            "release must leave a signature file in the source dir");
    assert!(archive_path.is_file(),
            "release must produce the archive at the requested path");

    // The archive must install cleanly under the
    // trusted publisher.
    let out = run(install_root.path(), &[
        "install", archive_path.to_str().unwrap(),
    ]);
    assert!(out.status.success(),
            "install of released artifact failed: stderr={}",
            String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("signature verified"),
            "released archive must install via the trusted path; got: {}",
            stdout);
    assert!(stdout.contains("Installed com.example.releaseapp"));
}

#[test]
fn release_requires_key_flag() {
    let install_root = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let arc_dir = tempfile::tempdir().unwrap();
    let archive_path = arc_dir.path().join("app.insula");
    build_bundle(src.path(), "com.example.nokey");

    let out = run(install_root.path(), &[
        "release",
        src.path().to_str().unwrap(),
        archive_path.to_str().unwrap(),
    ]);
    assert!(!out.status.success(),
            "release without --key should fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--key"), "stderr: {}", stderr);
}

#[test]
fn release_fails_cleanly_when_src_is_not_a_bundle() {
    let install_root = tempfile::tempdir().unwrap();
    let key_dir = tempfile::tempdir().unwrap();
    let _ = run(install_root.path(), &[
        "keygen", "rel-pub2", key_dir.path().to_str().unwrap(),
    ]);
    let not_a_bundle = tempfile::tempdir().unwrap();
    fs::write(not_a_bundle.path().join("hi.txt"), b"not a manifest").unwrap();
    let out_path = tempfile::tempdir().unwrap();
    let arc = out_path.path().join("a.insula");

    let out = run(install_root.path(), &[
        "release",
        not_a_bundle.path().to_str().unwrap(),
        arc.to_str().unwrap(),
        "--key", key_dir.path().join("rel-pub2.sk").to_str().unwrap(),
    ]);
    assert!(!out.status.success(),
            "release should fail when src isn't a valid bundle");
    // No archive should have been produced.
    assert!(!arc.exists(),
            "release must not leave a partial archive on failure");
}
