//! `insula clean` and `insula clean --all`.

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
        .output().expect("cargo build neighbor");
    assert!(out.status.success(),
            "{}: {}", crate_name, String::from_utf8_lossy(&out.stderr));
    crate_dir.join("target").join("debug").join(bin_name)
}

fn run_with_daemons(install_root: &Path, args: &[&str]) -> std::process::Output {
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
        .output().expect("insula")
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
fn clean_drops_run_dir_but_keeps_apps_and_publishers() {
    let install_root = tempfile::tempdir().unwrap();
    let bundle = tempfile::tempdir().unwrap();
    let key_dir = tempfile::tempdir().unwrap();
    build_bundle(bundle.path(), "com.example.keep-me");

    // Install + trust a publisher so we have non-run state.
    let out = run(install_root.path(), &[
        "install", bundle.path().to_str().unwrap(), "--allow-unsigned",
    ]);
    assert!(out.status.success());

    let out = run(install_root.path(), &[
        "keygen", "clean-pub", key_dir.path().to_str().unwrap(),
    ]);
    assert!(out.status.success());
    let out = run(install_root.path(), &[
        "publishers", "add", "clean-pub",
        key_dir.path().join("clean-pub.pub").to_str().unwrap(),
    ]);
    assert!(out.status.success());

    // Bring daemons up so run/ has stuff in it.
    let out = run_with_daemons(install_root.path(), &["daemons", "up"]);
    assert!(out.status.success());
    assert!(install_root.path().join("run").is_dir(),
            "expected run/ populated by daemons up");

    // Clean.
    let out = run(install_root.path(), &["clean"]);
    assert!(out.status.success(),
            "clean: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("daemon state cleared"),
            "expected default-mode banner; got: {}", stdout);
    assert!(stdout.contains("apps + trust store preserved"));

    // run/ gone, apps/ + trusted-publishers/ still there.
    assert!(!install_root.path().join("run").exists(),
            "run/ should be removed by clean");
    assert!(install_root.path().join("apps").join("com.example.keep-me")
            .is_dir(),
            "installed app must survive clean");
    assert!(install_root.path().join("trusted-publishers")
            .join("clean-pub.pub").is_file(),
            "trusted publisher must survive clean");
}

#[test]
fn clean_all_wipes_everything_under_install_root() {
    let install_root = tempfile::tempdir().unwrap();
    let bundle = tempfile::tempdir().unwrap();
    let key_dir = tempfile::tempdir().unwrap();
    build_bundle(bundle.path(), "com.example.bye-bye");

    let _ = run(install_root.path(), &[
        "install", bundle.path().to_str().unwrap(), "--allow-unsigned",
    ]);
    let _ = run(install_root.path(), &[
        "keygen", "doomed-pub", key_dir.path().to_str().unwrap(),
    ]);
    let _ = run(install_root.path(), &[
        "publishers", "add", "doomed-pub",
        key_dir.path().join("doomed-pub.pub").to_str().unwrap(),
    ]);
    let _ = run_with_daemons(install_root.path(), &["daemons", "up"]);

    let out = run(install_root.path(), &["clean", "--all"]);
    assert!(out.status.success(),
            "clean --all: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("install root reset"),
            "expected --all banner; got: {}", stdout);

    assert!(!install_root.path().join("run").exists());
    assert!(!install_root.path().join("apps").exists());
    assert!(!install_root.path().join("trusted-publishers").exists());
}

#[test]
fn clean_on_fresh_root_is_a_no_op() {
    let install_root = tempfile::tempdir().unwrap();
    let out = run(install_root.path(), &["clean"]);
    assert!(out.status.success(),
            "clean on fresh root must succeed; stderr: {}",
            String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("no run/ directory"),
            "expected fresh-root marker; got: {}", stdout);
}

#[test]
fn clean_unknown_flag_errors() {
    let install_root = tempfile::tempdir().unwrap();
    let out = run(install_root.path(), &["clean", "--nope"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown argument"), "stderr: {}", stderr);
}
