//! `insula bundle` + `insula install <archive>` round-trip:
//!
//!   1. Build a minimal signed bundle directory.
//!   2. `insula bundle <dir> <out.insula>` packs it.
//!   3. `insula install <out.insula>` extracts + installs.
//!   4. Signature still verifies (the archive preserves
//!      the detached `signature` file).
//!
//! Plus: tampering with the archived binary post-pack
//! makes install fail at signature verification — the
//! archive container does not weaken the integrity story.

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
fn bundle_then_install_archive_works() {
    let install_root = tempfile::tempdir().unwrap();
    let src_dir = tempfile::tempdir().unwrap();
    let key_dir = tempfile::tempdir().unwrap();
    let archive_dir = tempfile::tempdir().unwrap();
    let archive_path = archive_dir.path().join("app.insula");

    // keygen + trust
    let out = run_insula(install_root.path(), &[
        "keygen", "pub-arc", key_dir.path().to_str().unwrap(),
    ]);
    assert!(out.status.success());
    let out = run_insula(install_root.path(), &[
        "publishers", "add", "pub-arc",
        key_dir.path().join("pub-arc.pub").to_str().unwrap(),
    ]);
    assert!(out.status.success());

    // build + sign bundle directory
    build_minimal_bundle(src_dir.path(), "com.example.archived");
    let out = run_insula(install_root.path(), &[
        "sign", src_dir.path().to_str().unwrap(),
        "--key", key_dir.path().join("pub-arc.sk").to_str().unwrap(),
    ]);
    assert!(out.status.success(),
            "sign: {}", String::from_utf8_lossy(&out.stderr));

    // pack into .insula
    let out = run_insula(install_root.path(), &[
        "bundle", src_dir.path().to_str().unwrap(),
        archive_path.to_str().unwrap(),
    ]);
    assert!(out.status.success(),
            "bundle: {}", String::from_utf8_lossy(&out.stderr));
    assert!(archive_path.exists());

    // Sanity: archive starts with INSB magic.
    let head = fs::read(&archive_path).unwrap();
    assert_eq!(&head[..4], b"INSB",
               "archive should carry the documented magic");

    // install FROM the archive
    let out = run_insula(install_root.path(), &[
        "install", archive_path.to_str().unwrap(),
    ]);
    assert!(out.status.success(),
            "install from archive: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("signature verified"),
            "expected signed-install path; got: {}", stdout);
    assert!(stdout.contains("Installed com.example.archived"));
}

#[test]
fn tampered_archive_payload_fails_install() {
    let install_root = tempfile::tempdir().unwrap();
    let src_dir = tempfile::tempdir().unwrap();
    let key_dir = tempfile::tempdir().unwrap();
    let archive_dir = tempfile::tempdir().unwrap();
    let archive_path = archive_dir.path().join("app.insula");

    let _ = run_insula(install_root.path(), &[
        "keygen", "pub-tamper", key_dir.path().to_str().unwrap(),
    ]);
    let _ = run_insula(install_root.path(), &[
        "publishers", "add", "pub-tamper",
        key_dir.path().join("pub-tamper.pub").to_str().unwrap(),
    ]);
    build_minimal_bundle(src_dir.path(), "com.example.tampered");
    let _ = run_insula(install_root.path(), &[
        "sign", src_dir.path().to_str().unwrap(),
        "--key", key_dir.path().join("pub-tamper.sk").to_str().unwrap(),
    ]);

    // Pack.
    let out = run_insula(install_root.path(), &[
        "bundle", src_dir.path().to_str().unwrap(),
        archive_path.to_str().unwrap(),
    ]);
    assert!(out.status.success());

    // Tamper: flip a byte deep inside the archive (likely
    // inside the binary payload). We look for the shebang
    // line and overwrite the comment.
    let mut bytes = fs::read(&archive_path).unwrap();
    let needle = b"echo hi";
    let pos = bytes.windows(needle.len())
        .position(|w| w == needle)
        .expect("shebang must be present in the archive");
    bytes[pos] = b'X';
    fs::write(&archive_path, &bytes).unwrap();

    let out = run_insula(install_root.path(), &[
        "install", archive_path.to_str().unwrap(),
    ]);
    assert!(!out.status.success(),
            "install of tampered archive must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("signature") || stderr.contains("verify"),
            "expected signature-related failure; got: {}", stderr);
}

#[test]
fn install_from_unsigned_archive_still_needs_allow_flag() {
    let install_root = tempfile::tempdir().unwrap();
    let src_dir = tempfile::tempdir().unwrap();
    let archive_dir = tempfile::tempdir().unwrap();
    let archive_path = archive_dir.path().join("app.insula");

    build_minimal_bundle(src_dir.path(), "com.example.unsigned-arc");

    let out = run_insula(install_root.path(), &[
        "bundle", src_dir.path().to_str().unwrap(),
        archive_path.to_str().unwrap(),
    ]);
    assert!(out.status.success());

    // No --allow-unsigned: must fail.
    let out = run_insula(install_root.path(), &[
        "install", archive_path.to_str().unwrap(),
    ]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unsigned") || stderr.contains("signature"));

    // With --allow-unsigned: succeeds.
    let out = run_insula(install_root.path(), &[
        "install", archive_path.to_str().unwrap(), "--allow-unsigned",
    ]);
    assert!(out.status.success(),
            "stderr: {}", String::from_utf8_lossy(&out.stderr));
}
