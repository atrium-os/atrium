//! `insula init` scaffolding.

#![cfg(target_os = "macos")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn insula_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_insula"))
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(insula_binary()).args(args).output().expect("insula")
}

fn run_in(install_root: &Path, args: &[&str]) -> std::process::Output {
    Command::new(insula_binary())
        .env("INSULA_INSTALL_ROOT", install_root)
        .args(args)
        .output().expect("insula")
}

#[test]
fn init_creates_a_parseable_bundle() {
    let parent = tempfile::tempdir().unwrap();
    let app_dir = parent.path().join("weatherly");

    let out = run(&["init", app_dir.to_str().unwrap()]);
    assert!(out.status.success(),
            "init: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("com.example.weatherly"),
            "default app id should be derived from dir name; got: {}", stdout);
    assert!(stdout.contains("bin/weatherly"));

    // Scaffolded files exist.
    assert!(app_dir.join("manifest.toml").is_file());
    assert!(app_dir.join("bin").is_dir());
    assert!(app_dir.join(".gitignore").is_file());

    // The manifest parses cleanly via insula_manifest.
    let src = fs::read_to_string(app_dir.join("manifest.toml")).unwrap();
    let m = insula_manifest::Manifest::parse(&src)
        .expect("scaffolded manifest must parse");
    assert_eq!(m.app.name, "com.example.weatherly");
    assert_eq!(m.bundle.entry, "bin/weatherly");
    assert_eq!(m.app.version, "0.1.0");
}

#[test]
fn init_with_explicit_name_and_entry() {
    let parent = tempfile::tempdir().unwrap();
    let app_dir = parent.path().join("anything");

    let out = run(&[
        "init", app_dir.to_str().unwrap(),
        "--name", "com.acme.weather-pro",
        "--entry", "bin/wp",
    ]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("com.acme.weather-pro"));
    assert!(stdout.contains("bin/wp"));

    let src = fs::read_to_string(app_dir.join("manifest.toml")).unwrap();
    let m = insula_manifest::Manifest::parse(&src).unwrap();
    assert_eq!(m.app.name, "com.acme.weather-pro");
    assert_eq!(m.bundle.entry, "bin/wp");
}

#[test]
fn init_refuses_nonempty_dir() {
    let parent = tempfile::tempdir().unwrap();
    let app_dir = parent.path().join("occupied");
    fs::create_dir(&app_dir).unwrap();
    fs::write(app_dir.join("leftover.txt"), b"hi").unwrap();

    let out = run(&["init", app_dir.to_str().unwrap()]);
    assert!(!out.status.success(), "non-empty dir must be refused");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not empty"), "stderr: {}", stderr);
}

#[test]
fn init_into_empty_existing_dir_works() {
    let parent = tempfile::tempdir().unwrap();
    let app_dir = parent.path().join("blank");
    fs::create_dir(&app_dir).unwrap();   // exists but empty

    let out = run(&["init", app_dir.to_str().unwrap()]);
    assert!(out.status.success(),
            "init into empty dir should succeed; stderr: {}",
            String::from_utf8_lossy(&out.stderr));
    assert!(app_dir.join("manifest.toml").is_file());
}

#[test]
fn init_output_is_installable_end_to_end() {
    // Higher-confidence test: the scaffold should
    // round-trip through `insula install` (after we
    // drop a binary at the declared entry path). If
    // the scaffold drifts from what install actually
    // accepts, this catches it.
    let parent = tempfile::tempdir().unwrap();
    let app_dir = parent.path().join("roundtrip");

    let out = run(&["init", app_dir.to_str().unwrap()]);
    assert!(out.status.success());

    // Drop a real (executable) binary at the entry path.
    fs::write(app_dir.join("bin/roundtrip"), b"#!/bin/sh\necho hi\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut p = fs::metadata(app_dir.join("bin/roundtrip")).unwrap().permissions();
    p.set_mode(0o755);
    fs::set_permissions(app_dir.join("bin/roundtrip"), p).unwrap();

    let install_root = tempfile::tempdir().unwrap();
    let out = run_in(install_root.path(), &[
        "install", app_dir.to_str().unwrap(), "--allow-unsigned",
    ]);
    assert!(out.status.success(),
            "scaffolded bundle must install cleanly; stderr: {}",
            String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("Installed com.example.roundtrip"));
}

#[test]
fn init_sanitizes_directory_name_into_app_id() {
    let parent = tempfile::tempdir().unwrap();
    // Names with spaces / slashes should get cleaned
    // up into a valid app id default.
    let app_dir = parent.path().join("hello world");

    let out = run(&["init", app_dir.to_str().unwrap()]);
    assert!(out.status.success());
    let src = fs::read_to_string(app_dir.join("manifest.toml")).unwrap();
    let m = insula_manifest::Manifest::parse(&src).unwrap();
    // Space sanitized to dash.
    assert_eq!(m.app.name, "com.example.hello-world");
}
