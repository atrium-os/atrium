//! `insula doctor` health-check coverage.

#![cfg(target_os = "macos")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn insula_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_insula"))
}

fn build_neighbor(crate_name: &str, bin_name: &str) -> PathBuf {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().to_path_buf();
    let crate_dir = workspace_root.join(crate_name);
    let out = Command::new("cargo")
        .arg("build").arg("--quiet")
        .arg("--manifest-path").arg(crate_dir.join("Cargo.toml"))
        .arg("--bin").arg(bin_name)
        .output()
        .expect("cargo build neighbor");
    assert!(out.status.success());
    crate_dir.join("target").join("debug").join(bin_name)
}

fn run_insula(install_root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(insula_binary())
        .env("INSULA_INSTALL_ROOT", install_root)
        .args(args)
        .output()
        .expect("insula binary should be runnable")
}

#[test]
fn doctor_on_fresh_install_root_is_clean() {
    let install_root = tempfile::tempdir().unwrap();
    let out = run_insula(install_root.path(), &["doctor"]);
    assert!(out.status.success(),
            "fresh install root should pass; stderr: {}",
            String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("install-root"));
    assert!(stdout.contains("(writable)"));
    assert!(stdout.contains("summary:"));
    // No apps -> warn, not error.
    assert!(!stdout.contains("0 warn"),
            "fresh install has at least the run-dir + no-apps warns");
    assert!(stdout.contains("0 error"),
            "fresh install must have zero errors; got: {}", stdout);
}

#[test]
fn doctor_lists_each_daemon() {
    let install_root = tempfile::tempdir().unwrap();
    let out = run_insula(install_root.path(), &["doctor"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    for daemon_slug in [
        "insula-logd", "vestibulum-macos", "atrium-netd-macos",
        "praeco-macos", "tabellarius-macos",
    ] {
        assert!(stdout.contains(daemon_slug),
                "doctor should mention {}; stdout: {}", daemon_slug, stdout);
    }
}

#[test]
fn doctor_reports_error_for_broken_app_layout() {
    let install_root = tempfile::tempdir().unwrap();
    // Synthesize a broken app: bundle/manifest.toml
    // present, container/ missing.
    let app_root = install_root.path().join("apps").join("com.example.broken");
    fs::create_dir_all(app_root.join("bundle")).unwrap();
    fs::write(
        app_root.join("bundle/manifest.toml"),
        b"[app]\nname = \"com.example.broken\"\nversion = \"1\"\nsdk-version = \"1.x\"\n\
          [bundle]\nform = \"native\"\narches = []\nentry = \"bin/x\"\n",
    ).unwrap();
    // No container/ directory.

    let out = run_insula(install_root.path(), &["doctor"]);
    assert!(!out.status.success(),
            "broken layout should make doctor fail");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("apps/com.example.broken"));
    assert!(stdout.contains("[error]"));
    assert!(stdout.contains("missing container"));
}

#[test]
fn doctor_after_publishers_add_reports_trusted_count() {
    let install_root = tempfile::tempdir().unwrap();
    let key_dir = tempfile::tempdir().unwrap();

    let out = run_insula(install_root.path(), &[
        "keygen", "doc-pub", key_dir.path().to_str().unwrap(),
    ]);
    assert!(out.status.success());
    let out = run_insula(install_root.path(), &[
        "publishers", "add", "doc-pub",
        key_dir.path().join("doc-pub.pub").to_str().unwrap(),
    ]);
    assert!(out.status.success());

    let out = run_insula(install_root.path(), &["doctor"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("trusted-publishers"));
    assert!(stdout.contains("1 publisher(s) trusted"),
            "expected the publisher count; got: {}", stdout);
}

#[test]
fn doctor_detects_running_daemons_after_up() {
    let install_root = tempfile::tempdir().unwrap();
    let praeco_bin = build_neighbor("praeco-macos", "praeco-macos");
    let logd_bin   = build_neighbor("insula-logd", "insula-logd");
    let vest_bin   = build_neighbor("vestibulum-macos", "vestibulum-macos");
    let netd_bin   = build_neighbor("atrium-netd-macos", "atrium-netd-macos");
    let tabel_bin  = build_neighbor("tabellarius-macos", "tabellarius-macos");

    let out = Command::new(insula_binary())
        .env("INSULA_INSTALL_ROOT", install_root.path())
        .env("INSULA_LOGD_BIN", &logd_bin)
        .env("INSULA_VESTIBULUMD_BIN", &vest_bin)
        .env("INSULA_NETD_BIN", &netd_bin)
        .env("INSULA_PRAECOD_BIN", &praeco_bin)
        .env("INSULA_TABELLARIUSD_BIN", &tabel_bin)
        .args(["daemons", "up"])
        .output().unwrap();
    assert!(out.status.success());

    std::thread::sleep(std::time::Duration::from_millis(200));

    let out = Command::new(insula_binary())
        .env("INSULA_INSTALL_ROOT", install_root.path())
        .args(["doctor"])
        .output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("accepts connections"),
            "doctor should confirm at least one running daemon \
             accepts connections; stdout: {}", stdout);

    Command::new(insula_binary())
        .env("INSULA_INSTALL_ROOT", install_root.path())
        .args(["daemons", "down"])
        .output().ok();
}
