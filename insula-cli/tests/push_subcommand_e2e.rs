//! `insula push subscribe / list / unsubscribe` E2E:
//!
//!   1. subscribe — auto-spawns tabellarius, mints a
//!      keypair, prints the key_id + pubkey.
//!   2. list — shows the subscription.
//!   3. unsubscribe — removes it.
//!   4. list again — empty.
//!   5. unsubscribe of bogus id — error.

#![cfg(target_os = "macos")]

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
    assert!(out.status.success(),
            "cargo build {}: {}",
            crate_name, String::from_utf8_lossy(&out.stderr));
    crate_dir.join("target").join("debug").join(bin_name)
}

fn run_insula(install_root: &Path, tabel_bin: &Path, args: &[&str]) -> std::process::Output {
    Command::new(insula_binary())
        .env("INSULA_INSTALL_ROOT", install_root)
        .env("INSULA_TABELLARIUSD_BIN", tabel_bin)
        .env_remove("INSULA_TABELLARIUSD_SOCKET")
        .args(args)
        .output()
        .expect("insula binary should be runnable")
}

#[test]
fn subscribe_list_unsubscribe_lifecycle() {
    let install_root = tempfile::tempdir().unwrap();
    let tabel_bin = build_neighbor("tabellarius-macos", "tabellarius-macos");

    // subscribe
    let out = run_insula(install_root.path(), &tabel_bin,
                         &["push", "subscribe", "primary"]);
    assert!(out.status.success(),
            "subscribe failed: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("subscribed (purpose = primary)"),
            "expected subscribe banner; got: {}", stdout);

    // Parse out the printed key_id ("  key_id: <hex>")
    // for the unsubscribe step.
    let key_id = stdout.lines()
        .find_map(|l| l.trim().strip_prefix("key_id: "))
        .expect("subscribe output must include key_id")
        .to_string();
    assert!(!key_id.is_empty());

    // list — should include this key_id
    let out = run_insula(install_root.path(), &tabel_bin,
                         &["push", "list"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(&key_id),
            "list should show the subscription; got: {}", stdout);

    // unsubscribe
    let out = run_insula(install_root.path(), &tabel_bin,
                         &["push", "unsubscribe", &key_id]);
    assert!(out.status.success(),
            "unsubscribe failed: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains(&format!("unsubscribed {}", key_id)));

    // list again — empty
    let out = run_insula(install_root.path(), &tabel_bin,
                         &["push", "list"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("(no active subscriptions)"),
            "expected empty list; got: {}", stdout);

    // unsubscribe of bogus id — error
    let out = run_insula(install_root.path(), &tabel_bin,
                         &["push", "unsubscribe", "0000000000000000"]);
    assert!(!out.status.success(),
            "unsubscribe of unknown key_id should fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("no subscription"),
            "stderr: {}", stderr);

    // teardown
    run_insula(install_root.path(), &tabel_bin, &["daemons", "down"]);
}

#[test]
fn list_with_no_daemon_running_auto_spawns_then_reports_empty() {
    let install_root = tempfile::tempdir().unwrap();
    let tabel_bin = build_neighbor("tabellarius-macos", "tabellarius-macos");

    // Fresh install root, no prior state: `list` should
    // auto-spawn the daemon and print the empty marker.
    let out = run_insula(install_root.path(), &tabel_bin,
                         &["push", "list"]);
    assert!(out.status.success(),
            "list failed: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("(no active subscriptions)"));

    run_insula(install_root.path(), &tabel_bin, &["daemons", "down"]);
}
