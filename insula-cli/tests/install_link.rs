//! `insula install --link <bundle-dir>` — dev-iteration
//! mode: bundle/ is a symlink to the source instead of
//! a copy, so source edits show up on next launch.

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
    fs::write(root.join("bin/x"), b"#!/bin/sh\necho v1\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut p = fs::metadata(root.join("bin/x")).unwrap().permissions();
    p.set_mode(0o755);
    fs::set_permissions(root.join("bin/x"), p).unwrap();
}

#[test]
fn link_install_creates_symlink_pointing_at_source() {
    let install_root = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    build_bundle(src.path(), "com.example.linked");

    let out = run(install_root.path(), &[
        "install", src.path().to_str().unwrap(),
        "--allow-unsigned", "--link",
    ]);
    assert!(out.status.success(),
            "link install: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("mode:      link"),
            "expected link-mode marker; got: {}", stdout);

    // bundle/ is a symlink, not a directory.
    let bundle_path = install_root.path()
        .join("apps").join("com.example.linked").join("bundle");
    let metadata = std::fs::symlink_metadata(&bundle_path).unwrap();
    assert!(metadata.file_type().is_symlink(),
            "bundle/ should be a symlink in --link mode");

    // Symlink target resolves to the source dir.
    let target = std::fs::read_link(&bundle_path).unwrap();
    let canonical_src = src.path().canonicalize().unwrap();
    assert_eq!(target.canonicalize().unwrap(), canonical_src);

    // The manifest is readable through the symlink, so
    // `insula list` still sees the app.
    let out = run(install_root.path(), &["list"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("com.example.linked"));
}

#[test]
fn link_install_reflects_source_edits_immediately() {
    // The whole point of --link: edit the source, the
    // installed copy reflects it without reinstall.
    let install_root = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    build_bundle(src.path(), "com.example.live");

    let _ = run(install_root.path(), &[
        "install", src.path().to_str().unwrap(),
        "--allow-unsigned", "--link",
    ]);

    let installed_bin = install_root.path()
        .join("apps").join("com.example.live")
        .join("bundle").join("bin/x");
    let v1 = fs::read_to_string(&installed_bin).unwrap();
    assert!(v1.contains("echo v1"));

    // Edit the source in place.
    fs::write(src.path().join("bin/x"), b"#!/bin/sh\necho v2\n").unwrap();

    // Without re-install, the installed path should
    // already see v2 (it's a symlink).
    let v2 = fs::read_to_string(&installed_bin).unwrap();
    assert!(v2.contains("echo v2"),
            "edit should be live; read: {:?}", v2);
}

#[test]
fn link_install_replaces_a_prior_copy_install() {
    // If the user did a plain install first then
    // wanted to flip to --link, the install path
    // should remove the copied bundle/ directory and
    // replace it with the symlink — not error out on
    // the existing dir.
    let install_root = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    build_bundle(src.path(), "com.example.flip");

    let out = run(install_root.path(), &[
        "install", src.path().to_str().unwrap(), "--allow-unsigned",
    ]);
    assert!(out.status.success());
    let bundle_path = install_root.path()
        .join("apps").join("com.example.flip").join("bundle");
    assert!(!std::fs::symlink_metadata(&bundle_path).unwrap().file_type().is_symlink());

    // Now flip to link mode.
    let out = run(install_root.path(), &[
        "install", src.path().to_str().unwrap(),
        "--allow-unsigned", "--link",
    ]);
    assert!(out.status.success(),
            "flip to link: {}", String::from_utf8_lossy(&out.stderr));
    assert!(std::fs::symlink_metadata(&bundle_path).unwrap().file_type().is_symlink(),
            "bundle/ should now be a symlink");
}

#[test]
fn link_install_rejects_archive_arg() {
    let install_root = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let arc_dir = tempfile::tempdir().unwrap();
    let archive = arc_dir.path().join("app.insula");
    build_bundle(src.path(), "com.example.cantlink");

    // Pack the bundle so we have an archive to try.
    let out = run(install_root.path(), &[
        "bundle", src.path().to_str().unwrap(),
        archive.to_str().unwrap(),
    ]);
    assert!(out.status.success());

    // --link against the archive must fail.
    let out = run(install_root.path(), &[
        "install", archive.to_str().unwrap(),
        "--allow-unsigned", "--link",
    ]);
    assert!(!out.status.success(),
            "link install of an archive must fail");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("bundle directories only"),
            "stderr: {}", stderr);
}
