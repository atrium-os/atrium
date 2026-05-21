//! Install + launch-installed integration test.
//!
//! Doesn't depend on the insula-hello crate building
//! (those tests live in insula-hello/). This test
//! exercises install/launch on a hand-rolled bundle
//! that wraps a shell script — keeps the dependency
//! graph simple and ensures install + launch work
//! independent of any specific demo app.

#![cfg(target_os = "macos")]

use insula_bundle::InsulaBundle;
use insula_host_macos::{install, launch_installed};
use std::fs;
use std::path::Path;

fn sandbox_exec_available() -> bool {
    std::process::Command::new("/usr/bin/sandbox-exec")
        .arg("-h")
        .output()
        .map(|o| o.status.code().is_some())
        .unwrap_or(false)
}

/// Build a minimal valid bundle on disk at `root`. The
/// binary is `/bin/sh` re-housed as a script-shaped
/// payload.
fn build_test_bundle(root: &Path) {
    fs::write(
        root.join("manifest.toml"),
        r#"
[app]
name = "com.example.install-test"
version = "0.1.0"
sdk-version = "1.x"

[bundle]
form = "native"
arches = ["aarch64-darwin"]
entry = "bin/run.sh"
"#,
    )
    .unwrap();

    fs::create_dir(root.join("bin")).unwrap();
    let script = "#!/bin/sh\necho INSTALLED_APP_RAN\n";
    fs::write(root.join("bin/run.sh"), script).unwrap();

    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(root.join("bin/run.sh"))
        .unwrap()
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(root.join("bin/run.sh"), perms).unwrap();
}

#[test]
fn install_creates_expected_layout() {
    let src_root = tempfile::tempdir().unwrap();
    build_test_bundle(src_root.path());
    let bundle = InsulaBundle::read(src_root.path()).unwrap();

    let install_root = tempfile::tempdir().unwrap();
    let app = install(&bundle, install_root.path()).expect("install should succeed");

    assert_eq!(app.app_id, "com.example.install-test");

    // Expected layout:
    //   <install_root>/apps/<app-id>/bundle/
    //   <install_root>/apps/<app-id>/container/
    let app_root = install_root.path()
        .join("apps")
        .join("com.example.install-test");
    assert!(app_root.join("bundle/manifest.toml").is_file(),
            "bundle/manifest.toml should exist");
    assert!(app_root.join("bundle/bin/run.sh").is_file(),
            "bundle/bin/run.sh should exist");
    assert!(app_root.join("container").is_dir(),
            "container/ should exist (initially empty)");

    // The InstalledApp's paths match what we created.
    assert_eq!(app.binary_path, app_root.join("bundle/bin/run.sh"));
    assert_eq!(app.container_dir, app_root.join("container"));
}

#[test]
fn install_preserves_executable_bit() {
    let src_root = tempfile::tempdir().unwrap();
    build_test_bundle(src_root.path());
    let bundle = InsulaBundle::read(src_root.path()).unwrap();

    let install_root = tempfile::tempdir().unwrap();
    let app = install(&bundle, install_root.path()).unwrap();

    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(&app.binary_path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert!(mode & 0o111 != 0,
            "executable bit should survive install; got mode {:o}", mode);
}

#[test]
fn launch_installed_runs_the_sandboxed_app() {
    if !sandbox_exec_available() {
        eprintln!("skipping: sandbox-exec not available");
        return;
    }

    let src_root = tempfile::tempdir().unwrap();
    build_test_bundle(src_root.path());
    let bundle = InsulaBundle::read(src_root.path()).unwrap();

    let install_root = tempfile::tempdir().unwrap();
    let app = install(&bundle, install_root.path()).unwrap();

    let child = launch_installed(&app, &[], true)
        .expect("launching installed app should succeed");

    let output = child.child.wait_with_output().unwrap();

    assert!(output.status.success(),
            "installed app should exit cleanly; stderr: {}",
            String::from_utf8_lossy(&output.stderr));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("INSTALLED_APP_RAN"),
            "expected app's stdout to contain marker, got: {:?}", stdout);
}

#[test]
fn reinstall_replaces_bundle_but_preserves_container() {
    let src_root = tempfile::tempdir().unwrap();
    build_test_bundle(src_root.path());
    let bundle = InsulaBundle::read(src_root.path()).unwrap();

    let install_root = tempfile::tempdir().unwrap();
    let app1 = install(&bundle, install_root.path()).unwrap();

    // Pretend the app wrote something into its container.
    fs::write(app1.container_dir.join("user-state.txt"), b"important").unwrap();

    // "Update" the bundle (change a file).
    fs::write(src_root.path().join("bin/run.sh"), "#!/bin/sh\necho UPDATED\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(src_root.path().join("bin/run.sh"))
        .unwrap().permissions();
    perms.set_mode(0o755);
    fs::set_permissions(src_root.path().join("bin/run.sh"), perms).unwrap();

    let bundle2 = InsulaBundle::read(src_root.path()).unwrap();
    let _app2 = install(&bundle2, install_root.path()).unwrap();

    // Bundle was replaced.
    let new_script = fs::read_to_string(&app1.binary_path).unwrap();
    assert!(new_script.contains("UPDATED"),
            "reinstall should overwrite bundle; got: {:?}", new_script);

    // Container survived.
    assert!(app1.container_dir.join("user-state.txt").exists(),
            "container should survive reinstall (iOS/macOS-style)");
    assert_eq!(
        fs::read_to_string(app1.container_dir.join("user-state.txt")).unwrap(),
        "important"
    );
}
