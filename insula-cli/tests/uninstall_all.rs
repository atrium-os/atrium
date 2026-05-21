//! `insula uninstall --all` behavior.

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
version = "0.1.0"
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
fn uninstall_all_removes_every_installed_app() {
    let install_root = tempfile::tempdir().unwrap();

    // Install three apps.
    for id in ["com.example.one", "com.example.two", "com.example.three"] {
        let b = tempfile::tempdir().unwrap();
        build_bundle(b.path(), id);
        let out = run(install_root.path(), &[
            "install", b.path().to_str().unwrap(), "--allow-unsigned",
        ]);
        assert!(out.status.success(),
                "install {}: {}", id, String::from_utf8_lossy(&out.stderr));
    }

    let out = run(install_root.path(), &["list"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("com.example.one"));
    assert!(stdout.contains("com.example.two"));
    assert!(stdout.contains("com.example.three"));

    // Nuke them.
    let out = run(install_root.path(), &["uninstall", "--all"]);
    assert!(out.status.success(),
            "uninstall --all: {}",
            String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    for id in ["com.example.one", "com.example.two", "com.example.three"] {
        assert!(stdout.contains(&format!("Uninstalled {}", id)),
                "missing 'Uninstalled {}'; got: {}", id, stdout);
    }
    assert!(stdout.contains("Uninstalled 3 app(s)"),
            "expected summary count; got: {}", stdout);

    // list now shows the empty marker.
    let out = run(install_root.path(), &["list"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("no apps installed"),
            "list should be empty; got: {}", stdout);
}

#[test]
fn uninstall_all_on_empty_root_reports_no_apps() {
    let install_root = tempfile::tempdir().unwrap();
    let out = run(install_root.path(), &["uninstall", "--all"]);
    assert!(out.status.success(),
            "empty root should be a clean no-op");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("no apps"),
            "got: {}", stdout);
}

#[test]
fn uninstall_single_app_still_works() {
    // Make sure --all didn't break the single-target path.
    let install_root = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    build_bundle(b.path(), "com.example.keep-the-old-path");
    let _ = run(install_root.path(), &[
        "install", b.path().to_str().unwrap(), "--allow-unsigned",
    ]);
    let out = run(install_root.path(), &[
        "uninstall", "com.example.keep-the-old-path",
    ]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Uninstalled com.example.keep-the-old-path"));
}
