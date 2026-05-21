//! `insula info` against a bundle directory or
//! `.insula` archive — without installing.
//!
//! Lets a publisher / reviewer inspect a bundle's
//! capability surface + signature before committing
//! to install. Completes the publish-side toolchain.

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
version = "1.2.3"
sdk-version = "1.x"

[bundle]
form = "native"
arches = ["aarch64-darwin"]
entry = "bin/x"

[network]
hosts = [
  {{ name = "api.example.com", port = 443, proto = "tcp" }}
]
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
fn info_on_bundle_directory_works_without_install() {
    let install_root = tempfile::tempdir().unwrap();
    let bundle = tempfile::tempdir().unwrap();
    build_bundle(bundle.path(), "com.example.preinstall");

    // The app is NOT installed under install_root.
    let out = run(install_root.path(), &["info", bundle.path().to_str().unwrap()]);
    assert!(out.status.success(),
            "info against a bundle dir should work; stderr: {}",
            String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("com.example.preinstall"));
    assert!(stdout.contains("source:      bundle-dir"),
            "expected source line marking bundle-dir mode; got: {}", stdout);
    // Pre-install: no container line.
    assert!(!stdout.contains("container:"),
            "bundle-dir info shouldn't print a container path");
    // The network capability surface should still show.
    assert!(stdout.contains("api.example.com:443"));
    // Unsigned-publisher hint (not the installed variant).
    assert!(stdout.contains("sign with `insula sign`"),
            "expected publisher-side unsigned hint; got: {}", stdout);
}

#[test]
fn info_on_archive_works_without_install() {
    let install_root = tempfile::tempdir().unwrap();
    let bundle = tempfile::tempdir().unwrap();
    let arc_dir = tempfile::tempdir().unwrap();
    let arc = arc_dir.path().join("app.insula");
    build_bundle(bundle.path(), "com.example.archived-info");

    // Pack it.
    let out = run(install_root.path(),
                  &["bundle", bundle.path().to_str().unwrap(), arc.to_str().unwrap()]);
    assert!(out.status.success());

    // Info against the archive.
    let out = run(install_root.path(), &["info", arc.to_str().unwrap()]);
    assert!(out.status.success(),
            "info against archive: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("com.example.archived-info"));
    assert!(stdout.contains("source:      archive"),
            "expected source line marking archive mode; got: {}", stdout);
    assert!(stdout.contains("api.example.com:443"));
}

#[test]
fn info_on_installed_app_still_says_installed() {
    let install_root = tempfile::tempdir().unwrap();
    let bundle = tempfile::tempdir().unwrap();
    build_bundle(bundle.path(), "com.example.was-installed");

    let _ = run(install_root.path(), &[
        "install", bundle.path().to_str().unwrap(), "--allow-unsigned",
    ]);

    let out = run(install_root.path(), &["info", "com.example.was-installed"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("source:      installed"));
    // Installed apps DO show a container line.
    assert!(stdout.contains("container:"));
}

#[test]
fn info_on_nonexistent_path_or_id_errors() {
    let install_root = tempfile::tempdir().unwrap();
    let out = run(install_root.path(), &["info", "/no/such/path"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not found"),
            "stderr: {}", stderr);
}
