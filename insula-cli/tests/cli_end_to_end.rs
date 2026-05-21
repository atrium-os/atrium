//! End-to-end CLI tests. Drives the `insula` binary
//! through subprocesses to verify install / list /
//! info / launch / uninstall all work against a real
//! filesystem.
//!
//! Each test gets its own INSULA_INSTALL_ROOT so they
//! don't interfere with each other or with a real
//! install on the developer's machine.

#![cfg(target_os = "macos")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn insula_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_insula"))
}

fn build_test_bundle(root: &Path, app_id: &str, version: &str, marker: &str) {
    fs::write(
        root.join("manifest.toml"),
        format!(
            r#"
[app]
name = "{}"
version = "{}"
sdk-version = "1.x"

[bundle]
form = "native"
arches = ["aarch64-darwin"]
entry = "bin/run.sh"
"#,
            app_id, version
        ),
    )
    .unwrap();

    fs::create_dir(root.join("bin")).unwrap();
    fs::write(
        root.join("bin/run.sh"),
        format!("#!/bin/sh\necho {}\n", marker),
    )
    .unwrap();

    use std::os::unix::fs::PermissionsExt;
    let mut p = fs::metadata(root.join("bin/run.sh"))
        .unwrap()
        .permissions();
    p.set_mode(0o755);
    fs::set_permissions(root.join("bin/run.sh"), p).unwrap();
}

fn run_insula(install_root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(insula_binary())
        .env("INSULA_INSTALL_ROOT", install_root)
        .args(args)
        .output()
        .expect("insula binary should be runnable")
}

fn sandbox_exec_available() -> bool {
    Command::new("/usr/bin/sandbox-exec")
        .arg("-h")
        .output()
        .map(|o| o.status.code().is_some())
        .unwrap_or(false)
}

#[test]
fn install_then_list_shows_app() {
    let install_root = tempfile::tempdir().unwrap();
    let bundle_dir = tempfile::tempdir().unwrap();
    build_test_bundle(bundle_dir.path(), "com.example.listapp", "1.0.0", "hi");

    // 1. install
    let out = run_insula(install_root.path(), &[
        "install",
        bundle_dir.path().to_str().unwrap(),
    ]);
    assert!(out.status.success(),
            "install should succeed; stderr: {}",
            String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Installed com.example.listapp v1.0.0"));

    // 2. list
    let out = run_insula(install_root.path(), &["list"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("com.example.listapp"));
    assert!(stdout.contains("1.0.0"));
}

#[test]
fn list_with_no_apps_says_so() {
    let install_root = tempfile::tempdir().unwrap();
    let out = run_insula(install_root.path(), &["list"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("no apps installed"),
            "got: {}", stdout);
}

#[test]
fn info_prints_manifest_summary() {
    let install_root = tempfile::tempdir().unwrap();
    let bundle_dir = tempfile::tempdir().unwrap();
    build_test_bundle(bundle_dir.path(), "com.example.infoapp", "2.1.0", "hi");

    run_insula(install_root.path(), &[
        "install",
        bundle_dir.path().to_str().unwrap(),
    ]);

    let out = run_insula(install_root.path(), &[
        "info",
        "com.example.infoapp",
    ]);
    assert!(out.status.success(),
            "info should succeed; stderr: {}",
            String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("com.example.infoapp"));
    assert!(stdout.contains("version:     2.1.0"));
    assert!(stdout.contains("entry=bin/run.sh"));
}

#[test]
fn launch_runs_installed_app() {
    if !sandbox_exec_available() {
        eprintln!("skipping: sandbox-exec not available");
        return;
    }

    let install_root = tempfile::tempdir().unwrap();
    let bundle_dir = tempfile::tempdir().unwrap();
    build_test_bundle(bundle_dir.path(), "com.example.launchapp", "0.5.0", "MAGIC_MARKER_OUTPUT");

    run_insula(install_root.path(), &[
        "install",
        bundle_dir.path().to_str().unwrap(),
    ]);

    let out = run_insula(install_root.path(), &[
        "launch",
        "com.example.launchapp",
    ]);
    assert!(out.status.success(),
            "launch should succeed; stderr: {}",
            String::from_utf8_lossy(&out.stderr));

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("MAGIC_MARKER_OUTPUT"),
            "launched app's output should reach our stdout; got: {:?}",
            stdout);
}

#[test]
fn uninstall_removes_app() {
    let install_root = tempfile::tempdir().unwrap();
    let bundle_dir = tempfile::tempdir().unwrap();
    build_test_bundle(bundle_dir.path(), "com.example.byeapp", "1.0.0", "hi");

    run_insula(install_root.path(), &[
        "install",
        bundle_dir.path().to_str().unwrap(),
    ]);

    let out = run_insula(install_root.path(), &[
        "uninstall",
        "com.example.byeapp",
    ]);
    assert!(out.status.success(),
            "uninstall should succeed; stderr: {}",
            String::from_utf8_lossy(&out.stderr));

    // The app dir is gone.
    assert!(!install_root.path().join("apps").join("com.example.byeapp").exists());

    // list no longer shows it.
    let out = run_insula(install_root.path(), &["list"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!stdout.contains("com.example.byeapp"));
}

#[test]
fn missing_subcommand_prints_usage_and_fails() {
    let install_root = tempfile::tempdir().unwrap();
    let out = run_insula(install_root.path(), &[]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Usage: insula"));
}

#[test]
fn unknown_subcommand_prints_usage_and_fails() {
    let install_root = tempfile::tempdir().unwrap();
    let out = run_insula(install_root.path(), &["totally-not-a-command"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown command: totally-not-a-command"));
}

#[test]
fn info_on_uninstalled_app_fails() {
    let install_root = tempfile::tempdir().unwrap();
    let out = run_insula(install_root.path(), &["info", "com.nope.notinstalled"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not installed"));
}
