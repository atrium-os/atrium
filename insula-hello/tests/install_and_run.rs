//! Install + run insula-hello through the macOS host
//! adapter using the real install pathway.
//!
//! Builds a temporary bundle directory by combining
//! the manifest in this crate's root with the
//! cargo-built binary, installs that bundle to a
//! tempdir via `insula_host_macos::install`, then
//! launches the installed app via `launch_installed`.
//!
//! This is the bring-up checkpoint at the right
//! abstraction level: not "launch a binary directly"
//! but "install once, launch by app-id."

#![cfg(target_os = "macos")]

use insula_bundle::InsulaBundle;
use insula_host_macos::{install, launch_installed};
use std::fs;
use std::path::PathBuf;

fn binary_path() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_insula-hello"))
}

fn manifest_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("manifest.toml")
}

fn sandbox_exec_available() -> bool {
    std::process::Command::new("/usr/bin/sandbox-exec")
        .arg("-h")
        .output()
        .map(|o| o.status.code().is_some())
        .unwrap_or(false)
}

#[test]
fn install_and_run_insula_hello() {
    if !sandbox_exec_available() {
        eprintln!("skipping: sandbox-exec not available");
        return;
    }

    // 1. Synthesize a bundle directory by combining
    //    the manifest + the cargo-built binary.
    let bundle_src = tempfile::tempdir().expect("bundle tempdir");
    fs::create_dir(bundle_src.path().join("bin")).unwrap();
    fs::copy(manifest_path(), bundle_src.path().join("manifest.toml"))
        .expect("copy manifest");
    fs::copy(binary_path(), bundle_src.path().join("bin/insula-hello"))
        .expect("copy binary");
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(bundle_src.path().join("bin/insula-hello"))
        .unwrap()
        .permissions();
    perms.set_mode(0o755);
    fs::set_permissions(bundle_src.path().join("bin/insula-hello"), perms)
        .expect("set exec");

    // 2. Read + validate the bundle.
    let bundle = InsulaBundle::read(bundle_src.path())
        .expect("synthesized insula-hello bundle should validate");

    // 3. Install into a fresh install root.
    let install_root = tempfile::tempdir().expect("install tempdir");
    let app = install(&bundle, install_root.path())
        .expect("install should succeed");

    assert_eq!(app.app_id, "com.atrium-os.insula-hello");

    // 4. Launch the installed app.
    let child = launch_installed(&app, &[], true)
        .expect("launching installed insula-hello should succeed");

    let output = child.child.wait_with_output()
        .expect("waiting on child should succeed");

    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "insula-hello should exit 0 from the installed location; stderr was: {}",
        stderr
    );

    assert!(stderr.contains("atrium_init"));
    assert!(stderr.contains("[INFO] hello from Insula"));
    assert!(stderr.contains("atrium_exit(0)"));
}
