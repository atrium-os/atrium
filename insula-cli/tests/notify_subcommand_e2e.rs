//! `insula notify` E2E: auto-spawn praeco, post a
//! notification, parse the returned id, then read the
//! daemon's log file and assert the record appears.

#![cfg(target_os = "macos")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

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
    assert!(out.status.success(),
            "cargo build {}: {}", crate_name,
            String::from_utf8_lossy(&out.stderr));
    crate_dir.join("target").join("debug").join(bin_name)
}

fn run_insula(install_root: &Path, praeco_bin: &Path, args: &[&str]) -> std::process::Output {
    Command::new(insula_binary())
        .env("INSULA_INSTALL_ROOT", install_root)
        .env("INSULA_PRAECOD_BIN", praeco_bin)
        .env_remove("INSULA_PRAECOD_SOCKET")
        .args(args)
        .output()
        .expect("insula binary should be runnable")
}

#[test]
fn notify_posts_and_logs_record() {
    let install_root = tempfile::tempdir().unwrap();
    let praeco_bin = build_neighbor("praeco-macos", "praeco-macos");

    let out = run_insula(install_root.path(), &praeco_bin, &[
        "notify", "Build done", "All 144 tests passed",
        "--urgency", "high",
    ]);
    assert!(out.status.success(),
            "notify: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let id_str = stdout.trim();
    let id: u64 = id_str.parse()
        .unwrap_or_else(|_| panic!("expected numeric id, got {:?}", stdout));
    assert!(id > 0, "id should be positive, got {}", id);

    // Give the daemon a beat to flush.
    thread::sleep(Duration::from_millis(150));

    // Praeco writes its log under <install_root>/run/.
    let log_path = install_root.path().join("run").join("praeco-macos.log");
    let log = fs::read_to_string(&log_path).unwrap_or_default();
    assert!(log.contains(&format!("id={}", id)),
            "log should contain the assigned id; log: {:?}", log);
    assert!(log.contains("urgency=high"),
            "log should reflect the --urgency=high; log: {:?}", log);
    assert!(log.contains("Build done"),
            "log should include the title; log: {:?}", log);
    assert!(log.contains("All 144 tests passed"),
            "log should include the body; log: {:?}", log);

    run_insula(install_root.path(), &praeco_bin, &["daemons", "down"]);
}

#[test]
fn notify_default_urgency_is_normal() {
    let install_root = tempfile::tempdir().unwrap();
    let praeco_bin = build_neighbor("praeco-macos", "praeco-macos");

    let out = run_insula(install_root.path(), &praeco_bin,
                         &["notify", "T", "B"]);
    assert!(out.status.success());
    thread::sleep(Duration::from_millis(150));

    let log = fs::read_to_string(
        install_root.path().join("run").join("praeco-macos.log")
    ).unwrap_or_default();
    assert!(log.contains("urgency=normal"),
            "default urgency must be normal; log: {:?}", log);

    run_insula(install_root.path(), &praeco_bin, &["daemons", "down"]);
}

#[test]
fn notify_bad_urgency_fails() {
    let install_root = tempfile::tempdir().unwrap();
    let praeco_bin = build_neighbor("praeco-macos", "praeco-macos");
    let out = run_insula(install_root.path(), &praeco_bin, &[
        "notify", "T", "B", "--urgency", "extreme",
    ]);
    assert!(!out.status.success(), "bad --urgency must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("urgency"), "stderr: {}", stderr);
}

#[test]
fn notify_missing_body_fails() {
    let install_root = tempfile::tempdir().unwrap();
    let praeco_bin = build_neighbor("praeco-macos", "praeco-macos");
    let out = run_insula(install_root.path(), &praeco_bin,
                         &["notify", "OnlyTitle"]);
    assert!(!out.status.success(), "missing body must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("missing <body>"), "stderr: {}", stderr);
}
