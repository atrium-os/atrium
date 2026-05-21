//! Test the auto-spawn-daemons UX:
//!
//!   $ insula install <bundle>
//!   $ insula launch <app-id>     # no daemons running
//!     -> insula-logd auto-spawned
//!     -> vestibulum-macos auto-spawned (when wired up)
//!     -> app's log lines reach the auto-spawned logd
//!
//! Closes the "user types two commands and it works"
//! property.

#![cfg(target_os = "macos")]

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::thread;
use std::time::Duration;

fn insula_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_insula"))
}

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
        .expect("cargo build neighbor");
    assert!(out.status.success(),
            "cargo build {} failed:\n{}",
            crate_name, String::from_utf8_lossy(&out.stderr));
    crate_dir.join("target").join("debug").join(bin_name)
}

fn sandbox_exec_available() -> bool {
    Command::new("/usr/bin/sandbox-exec")
        .arg("-h")
        .output()
        .map(|o| o.status.code().is_some())
        .unwrap_or(false)
}

/// Helper: run `insula <args>` with the given install
/// root + binary overrides for the daemon paths.
fn run_insula(
    install_root: &std::path::Path,
    logd_bin: &std::path::Path,
    vest_bin: &std::path::Path,
    args: &[&str],
) -> std::process::Output {
    let netd_bin = build_neighbor("atrium-netd-macos", "atrium-netd-macos");
    Command::new(insula_binary())
        .env("INSULA_INSTALL_ROOT", install_root)
        .env("INSULA_LOGD_BIN", logd_bin)
        .env("INSULA_VESTIBULUMD_BIN", vest_bin)
        .env("INSULA_NETD_BIN", &netd_bin)
        // Make sure the test doesn't pick up the
        // user's running daemons / env.
        .env_remove("INSULA_LOGD_SOCKET")
        .env_remove("INSULA_VESTIBULUMD_SOCKET")
        .env_remove("INSULA_NETD_SOCKET")
        .args(args)
        .output()
        .expect("insula binary should be runnable")
}

#[test]
fn daemons_up_down_status_lifecycle() {
    let logd_bin = build_neighbor("insula-logd", "insula-logd");
    let vest_bin = build_neighbor("vestibulum-macos", "vestibulum-macos");

    let install_root = tempfile::tempdir().unwrap();

    // Initially: stopped.
    let out = run_insula(install_root.path(), &logd_bin, &vest_bin, &["daemons", "status"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("insula-logd: stopped"), "got: {}", stdout);
    assert!(stdout.contains("vestibulum-macos: stopped"));
    assert!(stdout.contains("atrium-netd-macos: stopped"));

    // up
    let out = run_insula(install_root.path(), &logd_bin, &vest_bin, &["daemons", "up"]);
    assert!(out.status.success(), "up failed: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("insula-logd: started"));
    assert!(stdout.contains("vestibulum-macos: started"));
    assert!(stdout.contains("atrium-netd-macos: started"));

    // status reflects it
    thread::sleep(Duration::from_millis(150));
    let out = run_insula(install_root.path(), &logd_bin, &vest_bin, &["daemons", "status"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("insula-logd: running"), "got: {}", stdout);
    assert!(stdout.contains("vestibulum-macos: running"));
    assert!(stdout.contains("atrium-netd-macos: running"));

    // up is idempotent
    let out = run_insula(install_root.path(), &logd_bin, &vest_bin, &["daemons", "up"]);
    assert!(out.status.success());

    // down
    let out = run_insula(install_root.path(), &logd_bin, &vest_bin, &["daemons", "down"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("insula-logd: stopped"));
    assert!(stdout.contains("vestibulum-macos: stopped"));

    // status confirms.
    let out = run_insula(install_root.path(), &logd_bin, &vest_bin, &["daemons", "status"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("insula-logd: stopped"));
}

#[test]
fn launch_auto_spawns_logd_and_routes_to_it() {
    if !sandbox_exec_available() {
        eprintln!("skipping: sandbox-exec not available");
        return;
    }

    let logd_bin = build_neighbor("insula-logd", "insula-logd");
    let vest_bin = build_neighbor("vestibulum-macos", "vestibulum-macos");
    let hello_bin = build_neighbor("insula-hello", "insula-hello");

    let install_root = tempfile::tempdir().unwrap();

    // Build a bundle for insula-hello.
    let bundle = install_root.path().join("bundle-src");
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

    // install
    let out = run_insula(install_root.path(), &logd_bin, &vest_bin,
                         &["install", bundle.to_str().unwrap(), "--allow-unsigned"]);
    assert!(out.status.success(),
            "install failed: {}", String::from_utf8_lossy(&out.stderr));

    // launch — should auto-spawn logd
    let out = run_insula(install_root.path(), &logd_bin, &vest_bin,
                         &["launch", "com.atrium-os.insula-hello"]);
    assert!(out.status.success(),
            "launch failed: {}", String::from_utf8_lossy(&out.stderr));

    // Give the daemon a moment to flush.
    thread::sleep(Duration::from_millis(200));

    // The auto-spawned logd wrote its log file at
    // <install_root>/run/insula-logd.log.
    let log_path = install_root.path().join("run").join("insula-logd.log");
    let contents = fs::read_to_string(&log_path).unwrap_or_default();
    assert!(contents.contains("INFO\thello from Insula"),
            "expected auto-spawned daemon to capture INFO line; \
             log file was: {:?}", contents);

    // Clean up daemons explicitly so we don't leak
    // processes past the test.
    run_insula(install_root.path(), &logd_bin, &vest_bin, &["daemons", "down"]);
}
