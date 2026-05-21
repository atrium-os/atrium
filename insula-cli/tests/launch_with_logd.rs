//! Full-stack test: spawn insula-logd, install
//! insula-hello via `insula install`, launch via
//! `insula launch` with $INSULA_LOGD_SOCKET set,
//! verify the daemon's log file contains the line
//! libatrium emitted.
//!
//! The end-to-end demo at the user-facing-CLI level.

#![cfg(target_os = "macos")]

use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn insula_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_insula"))
}

/// We need `insula-logd` and `insula-hello` built on
/// the side. Build them once before the test runs.
fn build_neighbor(crate_name: &str, bin_name: &str) -> PathBuf {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let crate_dir = workspace_root.join(crate_name);

    let out = Command::new("cargo")
        .arg("build")
        .arg("--quiet")
        .arg("--manifest-path")
        .arg(crate_dir.join("Cargo.toml"))
        .arg("--bin")
        .arg(bin_name)
        .output()
        .expect("cargo build for neighbor");
    assert!(out.status.success(),
            "cargo build for {} failed:\n{}",
            crate_name,
            String::from_utf8_lossy(&out.stderr));

    crate_dir.join("target").join("debug").join(bin_name)
}

fn sandbox_exec_available() -> bool {
    Command::new("/usr/bin/sandbox-exec")
        .arg("-h")
        .output()
        .map(|o| o.status.code().is_some())
        .unwrap_or(false)
}

fn spawn_logd(binary: &std::path::Path, sock: &std::path::Path, log: &std::path::Path) -> Child {
    Command::new(binary)
        .env("INSULA_LOGD_SOCKET", sock)
        .env("INSULA_LOGD_LOG_FILE", log)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn insula-logd")
}

fn wait_for_socket(p: &std::path::Path, timeout: Duration) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if p.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("socket did not appear at {}", p.display());
}

#[test]
fn launch_with_logd_socket_routes_to_daemon() {
    if !sandbox_exec_available() {
        eprintln!("skipping: sandbox-exec not available");
        return;
    }

    let logd_bin = build_neighbor("insula-logd", "insula-logd");
    let hello_bin = build_neighbor("insula-hello", "insula-hello");

    let tmp = tempfile::tempdir().expect("tmp");
    let socket = tmp.path().join("logd.sock");
    let log_file = tmp.path().join("insula.log");
    let install_root = tmp.path().join("install");

    // 1. Spawn the daemon.
    let mut daemon = spawn_logd(&logd_bin, &socket, &log_file);
    wait_for_socket(&socket, Duration::from_secs(3));

    // 2. Build a bundle for insula-hello.
    let bundle = tmp.path().join("bundle");
    fs::create_dir_all(bundle.join("bin")).unwrap();
    fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .join("insula-hello/manifest.toml"),
        bundle.join("manifest.toml"),
    )
    .unwrap();
    fs::copy(&hello_bin, bundle.join("bin/insula-hello")).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let mut p = fs::metadata(bundle.join("bin/insula-hello"))
            .unwrap()
            .permissions();
        p.set_mode(0o755);
        fs::set_permissions(bundle.join("bin/insula-hello"), p).unwrap();
    }

    // 3. `insula install`.
    let out = Command::new(insula_binary())
        .env("INSULA_INSTALL_ROOT", &install_root)
        .arg("install")
        .arg(&bundle)
        .arg("--allow-unsigned")
        .output()
        .expect("insula install");
    assert!(out.status.success(),
            "install failed: {}",
            String::from_utf8_lossy(&out.stderr));

    // 4. `insula launch` with the logd socket env set.
    let out = Command::new(insula_binary())
        .env("INSULA_INSTALL_ROOT", &install_root)
        .env("INSULA_LOGD_SOCKET", &socket)
        .arg("launch")
        .arg("com.atrium-os.insula-hello")
        .output()
        .expect("insula launch");
    assert!(out.status.success(),
            "launch failed; stderr: {}",
            String::from_utf8_lossy(&out.stderr));

    // 5. Give the daemon a beat to flush the message.
    thread::sleep(Duration::from_millis(200));

    // 6. Read the daemon's log file and verify.
    let contents = fs::read_to_string(&log_file).unwrap_or_default();
    assert!(contents.contains("INFO\thello from Insula"),
            "expected daemon log to contain insula-hello's INFO line; got: {:?}",
            contents);

    let _ = daemon.kill();
    let _ = daemon.wait();
}
