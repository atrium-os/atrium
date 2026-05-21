//! `insula info` actively verifies signatures (not
//! just reports their presence). Three outcomes:
//!
//!   - VALID (trusted)   — verifies + publisher trusted
//!   - VALID (untrusted) — verifies + publisher not in
//!                         the local trust store
//!   - INVALID           — bundle tampered after sign
//!
//! Closes a gap in the prior `insula info` behavior:
//! a tampered bundle would print its (now-broken)
//! signature with no warning. Reviewers running `info`
//! before installing get the same answer install would
//! give them — without committing to install.

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

fn keygen_and_sign(
    install_root: &Path, bundle: &Path,
    key_dir: &Path, key_id: &str,
) {
    let out = run(install_root, &[
        "keygen", key_id, key_dir.to_str().unwrap(),
    ]);
    assert!(out.status.success());
    let out = run(install_root, &[
        "sign", bundle.to_str().unwrap(),
        "--key", key_dir.join(format!("{}.sk", key_id)).to_str().unwrap(),
    ]);
    assert!(out.status.success());
}

#[test]
fn info_reports_valid_trusted_when_publisher_in_store() {
    let install_root = tempfile::tempdir().unwrap();
    let bundle = tempfile::tempdir().unwrap();
    let key_dir = tempfile::tempdir().unwrap();
    build_bundle(bundle.path(), "com.example.trusted-sig");
    keygen_and_sign(install_root.path(), bundle.path(), key_dir.path(), "trusted-pub");

    // Add the publisher to the trust store.
    let out = run(install_root.path(), &[
        "publishers", "add", "trusted-pub",
        key_dir.path().join("trusted-pub.pub").to_str().unwrap(),
    ]);
    assert!(out.status.success());

    // Info against the (pre-install) bundle should say VALID (trusted).
    let out = run(install_root.path(), &["info", bundle.path().to_str().unwrap()]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("VALID (trusted)"),
            "expected VALID (trusted); got: {}", stdout);
    assert!(stdout.contains("key_id=trusted-pub"));
}

#[test]
fn info_reports_valid_untrusted_when_publisher_not_in_store() {
    let install_root = tempfile::tempdir().unwrap();
    let bundle = tempfile::tempdir().unwrap();
    let key_dir = tempfile::tempdir().unwrap();
    build_bundle(bundle.path(), "com.example.untrusted-sig");
    keygen_and_sign(install_root.path(), bundle.path(), key_dir.path(), "stranger");
    // Note: we do NOT run `publishers add` — stranger is not trusted.

    let out = run(install_root.path(), &["info", bundle.path().to_str().unwrap()]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("VALID (untrusted publisher)"),
            "expected VALID (untrusted); got: {}", stdout);
    assert!(stdout.contains("key_id=stranger"));
    // Should suggest the fix.
    assert!(stdout.contains("insula publishers add stranger"));
}

#[test]
fn info_reports_invalid_when_bundle_tampered() {
    let install_root = tempfile::tempdir().unwrap();
    let bundle = tempfile::tempdir().unwrap();
    let key_dir = tempfile::tempdir().unwrap();
    build_bundle(bundle.path(), "com.example.tampered-sig");
    keygen_and_sign(install_root.path(), bundle.path(), key_dir.path(), "tamper-pub");

    // Tamper: rewrite the entry binary post-sign.
    fs::write(bundle.path().join("bin/x"), b"#!/bin/sh\nrm -rf $HOME\n").unwrap();

    let out = run(install_root.path(), &["info", bundle.path().to_str().unwrap()]);
    assert!(out.status.success(),
            "info should still succeed structurally — verification result \
             goes on stdout, not as an exit code");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("INVALID"),
            "expected INVALID for tampered bundle; got: {}", stdout);
    assert!(stdout.contains("bundle modified after sign"),
            "expected the explanatory clause; got: {}", stdout);
}

#[test]
fn info_reports_mismatch_when_trusted_pubkey_differs() {
    // Edge case: someone signs as `evil` with their
    // own keypair, then we add a DIFFERENT keypair to
    // the trust store under the same id `evil`. The
    // signature is self-consistent but doesn't match
    // the locally trusted pubkey. install would
    // refuse this; info should flag it too.
    let install_root = tempfile::tempdir().unwrap();
    let bundle = tempfile::tempdir().unwrap();
    let attacker_dir = tempfile::tempdir().unwrap();
    let legit_dir = tempfile::tempdir().unwrap();
    build_bundle(bundle.path(), "com.example.mismatch-sig");

    // Attacker keypair + sign claiming key_id=evil.
    let out = run(install_root.path(), &[
        "keygen", "evil", attacker_dir.path().to_str().unwrap(),
    ]);
    assert!(out.status.success());
    let out = run(install_root.path(), &[
        "sign", bundle.path().to_str().unwrap(),
        "--key", attacker_dir.path().join("evil.sk").to_str().unwrap(),
    ]);
    assert!(out.status.success());

    // Legitimate keypair we DO trust as `evil`.
    let out = run(install_root.path(), &[
        "keygen", "evil", legit_dir.path().to_str().unwrap(),
    ]);
    assert!(out.status.success());
    let out = run(install_root.path(), &[
        "publishers", "add", "evil",
        legit_dir.path().join("evil.pub").to_str().unwrap(),
    ]);
    assert!(out.status.success());

    let out = run(install_root.path(), &["info", bundle.path().to_str().unwrap()]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("MISMATCHED"),
            "expected MISMATCHED; got: {}", stdout);
}
