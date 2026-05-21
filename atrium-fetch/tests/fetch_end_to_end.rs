//! End-to-end test for atrium-fetch.
//!
//! Spawn a mock HTTP server on 127.0.0.1, build a
//! bundle for atrium-fetch with [network] allowing
//! that host, install + launch through the CLI, verify
//! the HTTP response bytes reach the CLI's stdout.
//!
//! Exercises:
//!   - atrium-fetch as a second sample Insula app.
//!   - libatrium's atrium_net_connect ABI.
//!   - The auto-spawned atrium-netd-macos broker.
//!   - Per-app netd enforcement via the bundle manifest.

#![cfg(target_os = "macos")]

use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn build_neighbor(crate_name: &str, bin_name: &str) -> PathBuf {
    let crate_dir = workspace_root().join(crate_name);
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

/// Spin up a 127.0.0.1 mock HTTP server returning a
/// fixed response body. Accepts up to `connections`
/// connections.
fn spawn_mock_http(connections: usize, body: &'static str) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for _ in 0..connections {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let response = format!(
                    "HTTP/1.0 200 OK\r\n\
                     Content-Type: text/plain\r\n\
                     Content-Length: {}\r\n\
                     \r\n\
                     {}",
                    body.len(), body
                );
                let _ = s.write_all(response.as_bytes());
            }
        }
    });
    port
}

fn build_fetch_bundle(bundle: &Path, fetch_bin: &Path) {
    fs::create_dir_all(bundle.join("bin")).unwrap();
    fs::copy(
        workspace_root().join("atrium-fetch/manifest.toml"),
        bundle.join("manifest.toml"),
    )
    .unwrap();
    fs::copy(fetch_bin, bundle.join("bin/atrium-fetch")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut p = fs::metadata(bundle.join("bin/atrium-fetch"))
        .unwrap()
        .permissions();
    p.set_mode(0o755);
    fs::set_permissions(bundle.join("bin/atrium-fetch"), p).unwrap();
}

fn run_insula(install_root: &Path, args: &[&str]) -> std::process::Output {
    let cli = build_neighbor("insula-cli", "insula");
    let logd = build_neighbor("insula-logd", "insula-logd");
    let vest = build_neighbor("vestibulum-macos", "vestibulum-macos");
    let netd = build_neighbor("atrium-netd-macos", "atrium-netd-macos");
    let praeco = build_neighbor("praeco-macos", "praeco-macos");

    Command::new(cli)
        .env("INSULA_INSTALL_ROOT", install_root)
        .env("INSULA_LOGD_BIN", logd)
        .env("INSULA_VESTIBULUMD_BIN", vest)
        .env("INSULA_NETD_BIN", netd)
        .env("INSULA_PRAECOD_BIN", praeco)
        .env_remove("INSULA_LOGD_SOCKET")
        .env_remove("INSULA_VESTIBULUMD_SOCKET")
        .env_remove("INSULA_NETD_SOCKET")
        .env_remove("INSULA_PRAECOD_SOCKET")
        .args(args)
        .output()
        .expect("insula binary should be runnable")
}

#[test]
fn fetch_reaches_mock_http_server_through_broker() {
    if !sandbox_exec_available() {
        eprintln!("skipping: sandbox-exec not available");
        return;
    }

    let fetch_bin = PathBuf::from(env!("CARGO_BIN_EXE_atrium-fetch"));
    let install_root = tempfile::tempdir().unwrap();
    let bundle = tempfile::tempdir().unwrap();
    build_fetch_bundle(bundle.path(), &fetch_bin);

    let body = "atrium-fetch saw this body bytes\n";
    let port = spawn_mock_http(2, body);

    // install
    let out = run_insula(install_root.path(),
                         &["install", bundle.path().to_str().unwrap(), "--allow-unsigned"]);
    assert!(out.status.success(),
            "install failed: {}", String::from_utf8_lossy(&out.stderr));

    // launch: atrium-fetch <host> <port> /
    let port_str = port.to_string();
    let out = run_insula(install_root.path(),
                         &["launch", "com.atrium-os.atrium-fetch",
                           "127.0.0.1", &port_str, "/"]);
    assert!(out.status.success(),
            "launch failed: stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr));

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("HTTP/1.0 200 OK"),
            "expected HTTP response on stdout; got: {:?}", stdout);
    assert!(stdout.contains(body),
            "expected body bytes on stdout; got: {:?}", stdout);

    // Daemon log should show the lifecycle markers.
    thread::sleep(Duration::from_millis(150));
    let log = fs::read_to_string(install_root.path().join("run").join("insula-logd.log"))
        .unwrap_or_default();
    assert!(log.contains("connecting to 127.0.0.1:"));
    assert!(log.contains("got fd"));
    assert!(log.contains("read"));

    // Clean up daemons.
    run_insula(install_root.path(), &["daemons", "down"]);
}
