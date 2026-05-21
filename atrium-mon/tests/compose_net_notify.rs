//! End-to-end: install atrium-mon via the host
//! adapter, launch it pointed at a local TCP echo
//! server, assert it both connects (via the broker)
//! AND posts a notification (via praeco) with the
//! result.
//!
//! Closes the "do two ABIs compose end-to-end in one
//! Insula app?" property — the existing samples each
//! exercise one ABI on their golden path.

#![cfg(target_os = "macos")]

use std::fs;
use std::net::TcpListener;
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
            "cargo build {}: {}", crate_name,
            String::from_utf8_lossy(&out.stderr));
    crate_dir.join("target").join("debug").join(bin_name)
}

fn sandbox_exec_available() -> bool {
    Command::new("/usr/bin/sandbox-exec").arg("-h")
        .output().map(|o| o.status.code().is_some()).unwrap_or(false)
}

fn build_bundle(bundle: &Path, mon_bin: &Path) {
    fs::create_dir_all(bundle.join("bin")).unwrap();
    fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("manifest.toml"),
        bundle.join("manifest.toml"),
    ).unwrap();
    fs::copy(mon_bin, bundle.join("bin/atrium-mon")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut p = fs::metadata(bundle.join("bin/atrium-mon")).unwrap().permissions();
    p.set_mode(0o755);
    fs::set_permissions(bundle.join("bin/atrium-mon"), p).unwrap();
}

fn run_insula(
    install_root: &Path,
    workspace_root: &Path,
    args: &[&str],
) -> std::process::Output {
    let logd_bin = build_neighbor(workspace_root, "insula-logd", "insula-logd");
    let vest_bin = build_neighbor(workspace_root, "vestibulum-macos", "vestibulum-macos");
    let netd_bin = build_neighbor(workspace_root, "atrium-netd-macos", "atrium-netd-macos");
    let praeco_bin = build_neighbor(workspace_root, "praeco-macos", "praeco-macos");
    let tabel_bin = build_neighbor(workspace_root, "tabellarius-macos", "tabellarius-macos");

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
        .output()
        .expect("insula binary should be runnable")
}

fn spawn_tcp_echo() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        // accept one connection then exit — atrium-mon
        // does a probe-and-close, no traffic.
        let _ = listener.accept();
    });
    port
}

#[test]
fn reachable_endpoint_posts_reachable_notification() {
    if !sandbox_exec_available() {
        eprintln!("skipping: sandbox-exec not available");
        return;
    }

    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().to_path_buf();
    let mon_bin = PathBuf::from(env!("CARGO_BIN_EXE_atrium-mon"));

    let install_root = tempfile::tempdir().unwrap();
    let bundle_src = tempfile::tempdir().unwrap();
    build_bundle(bundle_src.path(), &mon_bin);

    let port = spawn_tcp_echo();

    // install
    let out = run_insula(install_root.path(), &workspace_root, &[
        "install", bundle_src.path().to_str().unwrap(), "--allow-unsigned",
    ]);
    assert!(out.status.success(),
            "install: {}", String::from_utf8_lossy(&out.stderr));

    // launch atrium-mon pointed at 127.0.0.1:<echo port>
    let port_str = port.to_string();
    let out = run_insula(install_root.path(), &workspace_root, &[
        "launch", "com.atrium-os.atrium-mon", "127.0.0.1", &port_str,
    ]);
    assert!(out.status.success(),
            "launch (reachable): exit={:?} stderr={}",
            out.status.code(), String::from_utf8_lossy(&out.stderr));

    // Give the daemons a beat to flush.
    thread::sleep(Duration::from_millis(300));

    // Insula-logd captured the in-app log lines.
    let logd_log = fs::read_to_string(
        install_root.path().join("run").join("insula-logd.log")
    ).unwrap_or_default();
    let port_marker = format!("probing 127.0.0.1:{}", port);
    assert!(logd_log.contains(&port_marker),
            "expected probing line; logd log: {:?}", logd_log);
    assert!(logd_log.contains("posted notification id="),
            "expected posted-notification line; logd log: {:?}", logd_log);

    // Praeco captured the notification.
    let praeco_log = fs::read_to_string(
        install_root.path().join("run").join("praeco-macos.log")
    ).unwrap_or_default();
    assert!(praeco_log.contains("atrium-mon: reachable"),
            "expected reachable notification in praeco log; got: {:?}",
            praeco_log);
    assert!(praeco_log.contains(&format!("127.0.0.1:{}", port)),
            "notification body should mention the probed endpoint");

    run_insula(install_root.path(), &workspace_root, &["daemons", "down"]);
}

#[test]
fn unreachable_endpoint_posts_unreachable_notification() {
    if !sandbox_exec_available() {
        eprintln!("skipping: sandbox-exec not available");
        return;
    }

    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().to_path_buf();
    let mon_bin = PathBuf::from(env!("CARGO_BIN_EXE_atrium-mon"));

    let install_root = tempfile::tempdir().unwrap();
    let bundle_src = tempfile::tempdir().unwrap();
    build_bundle(bundle_src.path(), &mon_bin);

    let out = run_insula(install_root.path(), &workspace_root, &[
        "install", bundle_src.path().to_str().unwrap(), "--allow-unsigned",
    ]);
    assert!(out.status.success());

    // Bind+drop a listener to get a free port, but
    // don't keep it open — the connect will be refused.
    let dead_port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let p = l.local_addr().unwrap().port();
        drop(l);
        p
    };

    let port_str = dead_port.to_string();
    let out = run_insula(install_root.path(), &workspace_root, &[
        "launch", "com.atrium-os.atrium-mon", "127.0.0.1", &port_str,
    ]);
    // atrium-mon exits 1 on unreachable; the insula
    // launch wrapper propagates that exit code.
    assert!(!out.status.success(),
            "unreachable launch should propagate non-zero exit");

    thread::sleep(Duration::from_millis(300));

    let praeco_log = fs::read_to_string(
        install_root.path().join("run").join("praeco-macos.log")
    ).unwrap_or_default();
    assert!(praeco_log.contains("atrium-mon: unreachable"),
            "expected unreachable notification; praeco log: {:?}",
            praeco_log);
    assert!(praeco_log.contains("urgency=high"),
            "unreachable case should escalate to high-urgency; got: {:?}",
            praeco_log);

    run_insula(install_root.path(), &workspace_root, &["daemons", "down"]);
}
