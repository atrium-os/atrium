//! `insula daemons logs <daemon>` + the new log= column
//! in `insula daemons status`.
//!
//! Drives a real notification through praeco (the
//! `notify` subcommand) which writes a recognisable
//! line into praeco's log file, then asserts
//! `daemons logs praeco-macos` prints that line.

#![cfg(target_os = "macos")]

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
            "cargo build {}: {}",
            crate_name, String::from_utf8_lossy(&out.stderr));
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
fn daemons_logs_prints_log_contents() {
    let install_root = tempfile::tempdir().unwrap();
    let praeco_bin = build_neighbor("praeco-macos", "praeco-macos");

    // Drive one notification through praeco.
    let needle_title = "logs-test-marker-abcdef";
    let out = run_insula(install_root.path(), &praeco_bin,
                         &["notify", needle_title, "hi"]);
    assert!(out.status.success(),
            "notify: {}", String::from_utf8_lossy(&out.stderr));

    thread::sleep(Duration::from_millis(150));

    // daemons logs praeco-macos must surface that line.
    let out = run_insula(install_root.path(), &praeco_bin,
                         &["daemons", "logs", "praeco-macos"]);
    assert!(out.status.success(),
            "daemons logs: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(needle_title),
            "log output should contain the marker; got: {}", stdout);

    run_insula(install_root.path(), &praeco_bin, &["daemons", "down"]);
}

#[test]
fn daemons_logs_for_unknown_name_fails() {
    let install_root = tempfile::tempdir().unwrap();
    let praeco_bin = build_neighbor("praeco-macos", "praeco-macos");
    let out = run_insula(install_root.path(), &praeco_bin,
                         &["daemons", "logs", "not-a-daemon"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown daemon"), "stderr: {}", stderr);
}

#[test]
fn daemons_logs_missing_arg_lists_known_daemons() {
    let install_root = tempfile::tempdir().unwrap();
    let praeco_bin = build_neighbor("praeco-macos", "praeco-macos");
    let out = run_insula(install_root.path(), &praeco_bin,
                         &["daemons", "logs"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("insula-logd"));
    assert!(stderr.contains("vestibulum-macos"));
    assert!(stderr.contains("tabellarius-macos"));
}

#[test]
fn daemons_logs_for_daemon_that_never_ran_fails_cleanly() {
    let install_root = tempfile::tempdir().unwrap();
    let praeco_bin = build_neighbor("praeco-macos", "praeco-macos");
    // No daemon ever spawned under this install root.
    let out = run_insula(install_root.path(), &praeco_bin,
                         &["daemons", "logs", "tabellarius-macos"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no log file"), "stderr: {}", stderr);
}

#[test]
fn daemons_status_includes_log_path_column() {
    let install_root = tempfile::tempdir().unwrap();
    let praeco_bin = build_neighbor("praeco-macos", "praeco-macos");
    let out = run_insula(install_root.path(), &praeco_bin,
                         &["daemons", "status"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("log="),
            "status should expose a log= column; got: {}", stdout);
    assert!(stdout.contains("praeco-macos.log"),
            "status should print the log file path; got: {}", stdout);
}
