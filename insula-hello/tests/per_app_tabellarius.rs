//! End-to-end test for the Tabellarius push-subscribe
//! path through the full install/launch flow:
//!
//!   1. `insula install` insula-hello.
//!   2. `insula launch com.atrium-os.insula-hello`
//!      with `ATRIUM_TABELLARIUS_TEST_PURPOSE=primary`.
//!   3. The CLI auto-spawns tabellarius-macos (5th
//!      daemon), threads $ATRIUM_TABELLARIUS_SOCKET
//!      into the sandboxed child, and the SBPL grant
//!      lets the unix-socket connect through.
//!   4. The app's `atrium_tabellarius_subscribe` call
//!      reaches the daemon, mints a keypair, and the
//!      app logs the returned key_id + pk-prefix via
//!      insula-logd.
//!
//! Closes the analogous gap that
//! `tests/per_app_netd.rs` closed for the network
//! broker — proves the whole tabellarius stack works
//! end-to-end at the user-facing CLI level, not just
//! the standalone daemon test.

#![cfg(target_os = "macos")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

fn insula_binary() -> PathBuf {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().to_path_buf();
    build_neighbor(&workspace_root, "insula-cli", "insula")
}

fn build_neighbor(workspace_root: &Path, crate_name: &str, bin_name: &str) -> PathBuf {
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

fn sandbox_exec_available() -> bool {
    Command::new("/usr/bin/sandbox-exec")
        .arg("-h").output()
        .map(|o| o.status.code().is_some())
        .unwrap_or(false)
}

fn build_hello_bundle(bundle: &Path, hello_bin: &Path) {
    fs::create_dir_all(bundle.join("bin")).unwrap();
    fs::write(
        bundle.join("manifest.toml"),
        r#"
[app]
name = "com.atrium-os.insula-hello"
version = "0.1.0"
sdk-version = "1.x"

[bundle]
form = "native"
arches = ["aarch64-darwin"]
entry = "bin/insula-hello"

[storage]
data = "1MB"
"#,
    )
    .unwrap();
    fs::copy(hello_bin, bundle.join("bin/insula-hello")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut p = fs::metadata(bundle.join("bin/insula-hello"))
        .unwrap().permissions();
    p.set_mode(0o755);
    fs::set_permissions(bundle.join("bin/insula-hello"), p).unwrap();
}

fn run_insula(
    install_root: &Path,
    workspace_root: &Path,
    extra_env: &[(&str, &str)],
    args: &[&str],
) -> std::process::Output {
    let logd_bin = build_neighbor(workspace_root, "insula-logd", "insula-logd");
    let vest_bin = build_neighbor(workspace_root, "vestibulum-macos", "vestibulum-macos");
    let netd_bin = build_neighbor(workspace_root, "atrium-netd-macos", "atrium-netd-macos");
    let praeco_bin = build_neighbor(workspace_root, "praeco-macos", "praeco-macos");
    let tabel_bin = build_neighbor(workspace_root, "tabellarius-macos", "tabellarius-macos");

    let mut cmd = Command::new(insula_binary());
    cmd.env("INSULA_INSTALL_ROOT", install_root)
        .env("INSULA_LOGD_BIN", logd_bin)
        .env("INSULA_VESTIBULUMD_BIN", vest_bin)
        .env("INSULA_NETD_BIN", netd_bin)
        .env("INSULA_PRAECOD_BIN", praeco_bin)
        .env("INSULA_TABELLARIUSD_BIN", tabel_bin)
        .env_remove("INSULA_LOGD_SOCKET")
        .env_remove("INSULA_VESTIBULUMD_SOCKET")
        .env_remove("INSULA_NETD_SOCKET")
        .env_remove("INSULA_PRAECOD_SOCKET")
        .env_remove("INSULA_TABELLARIUSD_SOCKET");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.args(args).output().expect("insula binary should be runnable")
}

#[test]
fn launch_drives_tabellarius_subscribe_end_to_end() {
    if !sandbox_exec_available() {
        eprintln!("skipping: sandbox-exec not available");
        return;
    }

    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().to_path_buf();
    let hello_bin = PathBuf::from(env!("CARGO_BIN_EXE_insula-hello"));

    let install_root = tempfile::tempdir().unwrap();
    let bundle_src = tempfile::tempdir().unwrap();
    build_hello_bundle(bundle_src.path(), &hello_bin);

    // install
    let out = run_insula(install_root.path(), &workspace_root, &[],
                         &["install", bundle_src.path().to_str().unwrap(), "--allow-unsigned"]);
    assert!(out.status.success(),
            "install failed: {}", String::from_utf8_lossy(&out.stderr));

    // launch with the probe env var set; insula-cli
    // auto-spawns tabellarius and threads the socket.
    let out = run_insula(install_root.path(), &workspace_root,
                         &[("ATRIUM_TABELLARIUS_TEST_PURPOSE", "primary")],
                         &["launch", "com.atrium-os.insula-hello"]);
    assert!(out.status.success(),
            "launch failed: {}", String::from_utf8_lossy(&out.stderr));

    thread::sleep(Duration::from_millis(250));

    let log_path = install_root.path().join("run").join("insula-logd.log");
    let log_contents = fs::read_to_string(&log_path).unwrap_or_default();

    assert!(log_contents.contains("tabellarius-subscribe OK"),
            "expected the in-app subscribe to succeed end-to-end; \
             log was: {:?}", log_contents);
    assert!(log_contents.contains("purpose=primary"),
            "purpose should appear verbatim in the log line; \
             log was: {:?}", log_contents);

    // Cleanup any daemons we auto-spawned.
    run_insula(install_root.path(), &workspace_root, &[],
               &["daemons", "down"]);
}
