//! `insula run` — install + launch in one shot.

#![cfg(target_os = "macos")]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn insula_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_insula"))
}

fn build_neighbor(crate_name: &str, bin_name: &str) -> PathBuf {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().to_path_buf();
    let crate_dir = workspace_root.join(crate_name);
    let out = Command::new("cargo")
        .arg("build").arg("--quiet")
        .arg("--manifest-path").arg(crate_dir.join("Cargo.toml"))
        .arg("--bin").arg(bin_name)
        .output().expect("cargo build neighbor");
    assert!(out.status.success(),
            "{}: {}", crate_name, String::from_utf8_lossy(&out.stderr));
    crate_dir.join("target").join("debug").join(bin_name)
}

fn sandbox_exec_available() -> bool {
    Command::new("/usr/bin/sandbox-exec").arg("-h").output()
        .map(|o| o.status.code().is_some()).unwrap_or(false)
}

fn run_insula(
    install_root: &Path,
    extra_env: &[(&str, &str)],
    args: &[&str],
) -> std::process::Output {
    let workspace_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().to_path_buf();
    let logd_bin = build_neighbor("insula-logd", "insula-logd");
    let vest_bin = build_neighbor("vestibulum-macos", "vestibulum-macos");
    let netd_bin = build_neighbor("atrium-netd-macos", "atrium-netd-macos");
    let praeco_bin = build_neighbor("praeco-macos", "praeco-macos");
    let tabel_bin = build_neighbor("tabellarius-macos", "tabellarius-macos");
    let _ = workspace_root; // used implicitly by build_neighbor

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
    cmd.args(args).output().expect("insula binary")
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
    ).unwrap();
    fs::copy(hello_bin, bundle.join("bin/insula-hello")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut p = fs::metadata(bundle.join("bin/insula-hello"))
        .unwrap().permissions();
    p.set_mode(0o755);
    fs::set_permissions(bundle.join("bin/insula-hello"), p).unwrap();
}

#[test]
fn run_installs_and_launches_from_directory() {
    if !sandbox_exec_available() {
        eprintln!("skipping: sandbox-exec not available");
        return;
    }
    let install_root = tempfile::tempdir().unwrap();
    let bundle_dir = tempfile::tempdir().unwrap();
    let hello_bin = build_neighbor("insula-hello", "insula-hello");
    build_hello_bundle(bundle_dir.path(), &hello_bin);

    let out = run_insula(install_root.path(), &[], &[
        "run", bundle_dir.path().to_str().unwrap(),
    ]);
    assert!(out.status.success(),
            "run: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr));

    // The app got installed (apps/<id>/bundle/ exists).
    let app_root = install_root.path()
        .join("apps").join("com.atrium-os.insula-hello");
    assert!(app_root.join("bundle/manifest.toml").is_file(),
            "expected installed app layout under {}", app_root.display());

    // And the app actually launched + logd captured it.
    std::thread::sleep(std::time::Duration::from_millis(250));
    let log = fs::read_to_string(
        install_root.path().join("run").join("insula-logd.log")
    ).unwrap_or_default();
    assert!(log.contains("hello from Insula"),
            "expected app log line; logd log: {:?}", log);

    run_insula(install_root.path(), &[], &["daemons", "down"]);
}

#[test]
fn run_works_from_archive() {
    if !sandbox_exec_available() {
        eprintln!("skipping: sandbox-exec not available");
        return;
    }
    let install_root = tempfile::tempdir().unwrap();
    let bundle_dir = tempfile::tempdir().unwrap();
    let arc_dir = tempfile::tempdir().unwrap();
    let archive_path = arc_dir.path().join("app.insula");
    let hello_bin = build_neighbor("insula-hello", "insula-hello");
    build_hello_bundle(bundle_dir.path(), &hello_bin);

    // Pack the bundle into a .insula archive.
    let out = run_insula(install_root.path(), &[], &[
        "bundle", bundle_dir.path().to_str().unwrap(),
        archive_path.to_str().unwrap(),
    ]);
    assert!(out.status.success());

    // Run from the archive.
    let out = run_insula(install_root.path(), &[], &[
        "run", archive_path.to_str().unwrap(),
    ]);
    assert!(out.status.success(),
            "run from archive: {}",
            String::from_utf8_lossy(&out.stderr));

    std::thread::sleep(std::time::Duration::from_millis(250));
    let log = fs::read_to_string(
        install_root.path().join("run").join("insula-logd.log")
    ).unwrap_or_default();
    assert!(log.contains("hello from Insula"));

    run_insula(install_root.path(), &[], &["daemons", "down"]);
}

#[test]
fn run_accepts_widened_capabilities_on_reinstall() {
    if !sandbox_exec_available() {
        eprintln!("skipping: sandbox-exec not available");
        return;
    }
    // First install: no [network]. Second install: adds
    // a network host. With a plain `insula install`,
    // step 2 would refuse without --accept-changes.
    // `insula run` should sail through silently.
    let install_root = tempfile::tempdir().unwrap();
    let hello_bin = build_neighbor("insula-hello", "insula-hello");

    let v1 = tempfile::tempdir().unwrap();
    build_hello_bundle(v1.path(), &hello_bin);
    let out = run_insula(install_root.path(), &[], &[
        "run", v1.path().to_str().unwrap(),
    ]);
    assert!(out.status.success());

    // v2: same bundle dir contents, but rewrite the
    // manifest to add a [network] section.
    let v2 = tempfile::tempdir().unwrap();
    build_hello_bundle(v2.path(), &hello_bin);
    let manifest = fs::read_to_string(v2.path().join("manifest.toml")).unwrap();
    let widened = format!(
        "{}\n[network]\nhosts = [\n  {{ name = \"api.example.com\", port = 443, proto = \"tcp\" }}\n]\n",
        manifest,
    );
    fs::write(v2.path().join("manifest.toml"), widened).unwrap();

    let out = run_insula(install_root.path(), &[], &[
        "run", v2.path().to_str().unwrap(),
    ]);
    assert!(out.status.success(),
            "run should auto-accept widened caps; stderr: {}",
            String::from_utf8_lossy(&out.stderr));

    run_insula(install_root.path(), &[], &["daemons", "down"]);
}

#[test]
fn run_missing_bundle_arg_errors() {
    let install_root = tempfile::tempdir().unwrap();
    let out = run_insula(install_root.path(), &[], &["run"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("missing"), "stderr: {}", stderr);
}
