//! `insula daemons restart [name|all]`.

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
        .output().expect("cargo build neighbor");
    assert!(out.status.success(),
            "{}: {}", crate_name, String::from_utf8_lossy(&out.stderr));
    crate_dir.join("target").join("debug").join(bin_name)
}

fn run_insula_full(install_root: &Path, args: &[&str]) -> std::process::Output {
    let logd_bin = build_neighbor("insula-logd", "insula-logd");
    let vest_bin = build_neighbor("vestibulum-macos", "vestibulum-macos");
    let netd_bin = build_neighbor("atrium-netd-macos", "atrium-netd-macos");
    let praeco_bin = build_neighbor("praeco-macos", "praeco-macos");
    let tabel_bin = build_neighbor("tabellarius-macos", "tabellarius-macos");

    Command::new(insula_binary())
        .env("INSULA_INSTALL_ROOT", install_root)
        .env("INSULA_LOGD_BIN", logd_bin)
        .env("INSULA_VESTIBULUMD_BIN", vest_bin)
        .env("INSULA_NETD_BIN", netd_bin)
        .env("INSULA_PRAECOD_BIN", praeco_bin)
        .env("INSULA_TABELLARIUSD_BIN", tabel_bin)
        .env_remove("INSULA_LOGD_SOCKET")
        .env_remove("INSULA_VESTIBULUMD_SOCKET")
        .env_remove("INSULA_NETD_SOCKET")
        .env_remove("INSULA_PRAECOD_SOCKET")
        .env_remove("INSULA_TABELLARIUSD_SOCKET")
        .args(args)
        .output().expect("insula binary should be runnable")
}

fn read_pid_file(install_root: &Path, slug: &str) -> Option<i32> {
    let pid_path = install_root.join("run").join(format!("{}.pid", slug));
    let s = std::fs::read_to_string(&pid_path).ok()?;
    s.trim().parse().ok()
}

#[test]
fn restart_one_daemon_replaces_its_pid() {
    let install_root = tempfile::tempdir().unwrap();

    // Start praeco specifically (only the one we need
    // for this test — keeps the test fast).
    let out = run_insula_full(install_root.path(), &[
        "notify", "warm-up", "first-launch",
    ]);
    assert!(out.status.success(),
            "warm-up notify: {}", String::from_utf8_lossy(&out.stderr));

    thread::sleep(Duration::from_millis(150));
    let pid_before = read_pid_file(install_root.path(), "praeco-macos")
        .expect("praeco pid file after warm-up");

    // Restart only praeco.
    let out = run_insula_full(install_root.path(), &[
        "daemons", "restart", "praeco-macos",
    ]);
    assert!(out.status.success(),
            "restart praeco: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("praeco-macos: restarted"),
            "stdout: {}", stdout);

    thread::sleep(Duration::from_millis(150));
    let pid_after = read_pid_file(install_root.path(), "praeco-macos")
        .expect("praeco pid file after restart");
    assert_ne!(pid_before, pid_after,
               "restart should replace the daemon's pid; \
                before={} after={}", pid_before, pid_after);

    run_insula_full(install_root.path(), &["daemons", "down"]);
}

#[test]
fn restart_all_runs_for_every_managed_daemon() {
    let install_root = tempfile::tempdir().unwrap();

    // up
    let out = run_insula_full(install_root.path(), &["daemons", "up"]);
    assert!(out.status.success());

    // restart all (default target)
    let out = run_insula_full(install_root.path(), &["daemons", "restart"]);
    assert!(out.status.success(),
            "restart all: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    // One restart line per managed daemon.
    for slug in [
        "insula-logd", "vestibulum-macos", "atrium-netd-macos",
        "praeco-macos", "tabellarius-macos",
    ] {
        assert!(stdout.contains(&format!("{}: restarted", slug)),
                "expected {} restart line; got: {}", slug, stdout);
    }

    run_insula_full(install_root.path(), &["daemons", "down"]);
}

#[test]
fn restart_unknown_daemon_errors() {
    let install_root = tempfile::tempdir().unwrap();
    let out = run_insula_full(install_root.path(), &[
        "daemons", "restart", "not-a-real-daemon",
    ]);
    assert!(!out.status.success(),
            "unknown daemon name must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown daemon"), "stderr: {}", stderr);
}

#[test]
fn restart_stopped_daemon_just_starts_it() {
    // restart should be idempotent over the "is it
    // currently running?" question: if praeco isn't
    // running, restart starts it cleanly.
    let install_root = tempfile::tempdir().unwrap();
    let out = run_insula_full(install_root.path(), &[
        "daemons", "restart", "praeco-macos",
    ]);
    assert!(out.status.success(),
            "restart of stopped daemon: {}",
            String::from_utf8_lossy(&out.stderr));
    assert!(read_pid_file(install_root.path(), "praeco-macos").is_some(),
            "praeco should be running after restart");

    run_insula_full(install_root.path(), &["daemons", "down"]);
}
