//! End-to-end test for per-app netd enforcement.
//!
//! Install insula-hello twice with different manifests:
//!   1. Manifest with [network].hosts including
//!      127.0.0.1:<port>  →  net-connect succeeds, the
//!      broker resolves the peer via SO_PEERPID +
//!      proc_pidpath + manifest lookup and allows.
//!   2. Manifest with no matching host  →  net-connect
//!      returns ATRIUM_ERR_NETD_DENIED, the broker has
//!      identified the peer as an installed app and is
//!      enforcing its (restrictive) [network] section.
//!
//! Closes the "not E2E-tested" gap from the prior
//! per-app-enforcement commit.

#![cfg(target_os = "macos")]

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::Duration;

fn insula_binary() -> PathBuf {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    build_neighbor(&workspace_root, "insula-cli", "insula")
}

fn build_neighbor(workspace_root: &Path, crate_name: &str, bin_name: &str) -> PathBuf {
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

/// Spin up a 127.0.0.1 echo server on an ephemeral
/// port. Returns the port; the listener stays alive
/// in a background thread that handles up to N
/// connections.
fn spawn_tcp_echo(connections: usize) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for _ in 0..connections {
            if let Ok((mut s, _)) = listener.accept() {
                let mut buf = [0u8; 256];
                if let Ok(n) = s.read(&mut buf) {
                    let _ = s.write_all(&buf[..n]);
                }
            }
        }
    });
    port
}

/// Build a bundle directory containing insula-hello's
/// cargo-built binary + a manifest with the given
/// [network] section (raw TOML body inserted under
/// `[network]`).
fn build_hello_bundle(bundle: &Path, hello_bin: &Path, network_toml: &str) {
    fs::create_dir_all(bundle.join("bin")).unwrap();
    let manifest = format!(
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

[network]
{}
"#,
        network_toml
    );
    fs::write(bundle.join("manifest.toml"), manifest).unwrap();
    fs::copy(hello_bin, bundle.join("bin/insula-hello")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut p = fs::metadata(bundle.join("bin/insula-hello"))
        .unwrap()
        .permissions();
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

    let mut cmd = Command::new(insula_binary());
    cmd.env("INSULA_INSTALL_ROOT", install_root)
        .env("INSULA_LOGD_BIN", logd_bin)
        .env("INSULA_VESTIBULUMD_BIN", vest_bin)
        .env("INSULA_NETD_BIN", netd_bin)
        .env("INSULA_PRAECOD_BIN", praeco_bin)
        .env_remove("INSULA_LOGD_SOCKET")
        .env_remove("INSULA_VESTIBULUMD_SOCKET")
        .env_remove("INSULA_NETD_SOCKET")
        .env_remove("INSULA_PRAECOD_SOCKET");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.args(args).output().expect("insula binary should be runnable")
}

#[test]
fn manifest_allowing_host_lets_app_connect() {
    if !sandbox_exec_available() {
        eprintln!("skipping: sandbox-exec not available");
        return;
    }

    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let hello_bin = PathBuf::from(env!("CARGO_BIN_EXE_insula-hello"));

    let install_root = tempfile::tempdir().unwrap();
    let bundle_src = tempfile::tempdir().unwrap();

    let port = spawn_tcp_echo(2);

    // Manifest [network] explicitly allows 127.0.0.1:port.
    let net_toml = format!(
        "hosts = [\n  {{ name = \"127.0.0.1\", port = {}, proto = \"tcp\" }}\n]",
        port
    );
    build_hello_bundle(bundle_src.path(), &hello_bin, &net_toml);

    // install
    let out = run_insula(install_root.path(), &workspace_root, &[],
                         &["install", bundle_src.path().to_str().unwrap()]);
    assert!(out.status.success(), "install failed: {}",
            String::from_utf8_lossy(&out.stderr));

    // launch with ATRIUM_NET_TEST_HOST = the echo server.
    // The env var propagates through insula-cli ->
    // sandbox-exec -> insula-hello.
    let target = format!("127.0.0.1:{}", port);
    let out = run_insula(install_root.path(), &workspace_root,
                         &[("ATRIUM_NET_TEST_HOST", target.as_str())],
                         &["launch", "com.atrium-os.insula-hello"]);
    assert!(out.status.success(), "launch failed: {}",
            String::from_utf8_lossy(&out.stderr));

    // Give the auto-spawned logd a beat to flush the
    // INFO line.
    thread::sleep(Duration::from_millis(250));

    let log_path = install_root.path().join("run").join("insula-logd.log");
    let log_contents = fs::read_to_string(&log_path).unwrap_or_default();

    assert!(log_contents.contains(&format!(
        "net-connect OK to 127.0.0.1:{}", port
    )),
            "expected manifest-allowed connect to succeed; log was: {:?}",
            log_contents);

    // Tear down daemons we auto-spawned.
    run_insula(install_root.path(), &workspace_root, &[],
               &["daemons", "down"]);
}

#[test]
fn manifest_denying_host_blocks_app_connect() {
    if !sandbox_exec_available() {
        eprintln!("skipping: sandbox-exec not available");
        return;
    }

    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf();
    let hello_bin = PathBuf::from(env!("CARGO_BIN_EXE_insula-hello"));

    let install_root = tempfile::tempdir().unwrap();
    let bundle_src = tempfile::tempdir().unwrap();

    let port = spawn_tcp_echo(1);

    // Manifest [network] allows ONLY 198.51.100.1
    // (TEST-NET-2; can't be the real port). 127.0.0.1
    // is therefore denied by per-app enforcement.
    let net_toml =
        "hosts = [\n  { name = \"198.51.100.1\", port = 80, proto = \"tcp\" }\n]";
    build_hello_bundle(bundle_src.path(), &hello_bin, net_toml);

    let out = run_insula(install_root.path(), &workspace_root, &[],
                         &["install", bundle_src.path().to_str().unwrap()]);
    assert!(out.status.success(), "install failed: {}",
            String::from_utf8_lossy(&out.stderr));

    let target = format!("127.0.0.1:{}", port);
    let out = run_insula(install_root.path(), &workspace_root,
                         &[("ATRIUM_NET_TEST_HOST", target.as_str())],
                         &["launch", "com.atrium-os.insula-hello"]);
    assert!(out.status.success(), "launch failed: {}",
            String::from_utf8_lossy(&out.stderr));

    thread::sleep(Duration::from_millis(250));

    let log_path = install_root.path().join("run").join("insula-logd.log");
    let log_contents = fs::read_to_string(&log_path).unwrap_or_default();

    // -21 = ATRIUM_ERR_NETD_DENIED
    assert!(log_contents.contains(&format!(
        "net-connect FAIL to 127.0.0.1:{} (code -21)", port
    )),
            "expected manifest-disallowed connect to be DENIED (-21); \
             log was: {:?}", log_contents);

    // Defense: the broker really did refuse, not just
    // return a different error. Sanity-check that the
    // OK string is absent.
    assert!(!log_contents.contains(&format!(
        "net-connect OK to 127.0.0.1:{}", port
    )),
            "manifest-disallowed host MUST NOT show as OK; log: {:?}",
            log_contents);

    run_insula(install_root.path(), &workspace_root, &[],
               &["daemons", "down"]);

    // Drop the unused TcpStream once.
    drop(TcpStream::connect(format!("127.0.0.1:{}", port)).ok());
}
