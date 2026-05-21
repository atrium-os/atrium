//! End-to-end: install atrium-paint via the CLI, then
//! `insula launch` it with $INSULA_FRESCO_SOCKET set.
//! Verifies that the host adapter:
//!   - threads $ATRIUM_FRESCO_SOCKET into the
//!     sandboxed child
//!   - emits an SBPL grant for the fresco socket path
//!     so the child's connect() succeeds under
//!     sandbox-exec
//!
//! Without these, atrium_window_open would return
//! ATRIUM_ERR_NO_FRESCO inside the jail.

#![cfg(target_os = "macos")]

use aqueduct::classes::CLASS_DISPLAY;
use aqueduct::envelope::{self, flag, Header};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const OP_WINDOW_CREATE:          u16 = 0x0500;
const OP_WINDOW_DESTROY:         u16 = 0x0501;
const EV_WINDOW_CLOSE_REQUESTED: u16 = 0x0582;

fn write_envelope<W: Write>(stream: &mut W, op: u16, flags: u16, payload: &[u8]) {
    let hdr = Header::new(CLASS_DISPLAY, op, flags, payload.len() as u32);
    let _ = stream.write_all(&hdr.encode());
    let _ = stream.write_all(payload);
    let _ = stream.flush();
}

fn read_one<R: Read>(stream: &mut R) -> Option<(u16, u16, Vec<u8>)> {
    let mut hdr_bytes = [0u8; envelope::HEADER_LEN];
    stream.read_exact(&mut hdr_bytes).ok()?;
    let hdr = Header::decode(&hdr_bytes).ok()?;
    let mut payload = vec![0u8; hdr.length as usize];
    stream.read_exact(&mut payload).ok()?;
    Some((hdr.op, hdr.flags, payload))
}

fn build_neighbor(name: &str) -> PathBuf {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().to_path_buf();
    let dir = workspace.join(name);
    let out = Command::new("cargo")
        .arg("build").arg("--quiet")
        .arg("--manifest-path").arg(dir.join("Cargo.toml"))
        .arg("--bin").arg(name)
        .output().expect("cargo build neighbor");
    assert!(out.status.success(),
            "cargo build {}: {}", name, String::from_utf8_lossy(&out.stderr));
    dir.join("target").join("debug").join(name)
}

fn sandbox_exec_available() -> bool {
    Command::new("/usr/bin/sandbox-exec").arg("-h").output()
        .map(|o| o.status.code().is_some()).unwrap_or(false)
}

/// Stub Fresco server with the same shape as the
/// libatrium poll_event test, but durable enough to
/// survive the sandbox-exec dance.
fn spawn_stub(socket_path: PathBuf, window_id: u32)
    -> Arc<Mutex<Vec<u16>>>
{
    let ops_seen = Arc::new(Mutex::new(Vec::<u16>::new()));
    let ops2 = ops_seen.clone();
    thread::spawn(move || {
        let listener = UnixListener::bind(&socket_path).expect("bind");
        let (mut stream, _) = listener.accept().expect("accept");

        // 1. CREATE -> reply id.
        let (op, _flags, _) = read_one(&mut stream).expect("create");
        ops2.lock().unwrap().push(op);
        assert_eq!(op, OP_WINDOW_CREATE);
        let reply = postcard::to_stdvec(&window_id).unwrap();
        write_envelope(&mut stream, OP_WINDOW_CREATE, flag::IS_RESPONSE, &reply);

        // 2. fill_rect's three scene envelopes.
        for _ in 0..3 {
            let (op, _flags, _) = read_one(&mut stream).expect("scene");
            ops2.lock().unwrap().push(op);
        }

        // 3. Push CLOSE_REQUESTED.
        let close = postcard::to_stdvec(
            &fresco_protocol::WindowCloseRequestedEvent { window_id }
        ).unwrap();
        write_envelope(&mut stream, EV_WINDOW_CLOSE_REQUESTED, 0, &close);

        // 4. DESTROY from the app.
        if let Some((op, _flags, _)) = read_one(&mut stream) {
            ops2.lock().unwrap().push(op);
        }
        thread::sleep(Duration::from_millis(100));
    });
    ops_seen
}

fn build_paint_bundle(bundle: &Path, paint_bin: &Path) {
    fs::create_dir_all(bundle.join("bin")).unwrap();
    fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("manifest.toml"),
        bundle.join("manifest.toml"),
    ).unwrap();
    fs::copy(paint_bin, bundle.join("bin/atrium-paint")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut p = fs::metadata(bundle.join("bin/atrium-paint")).unwrap().permissions();
    p.set_mode(0o755);
    fs::set_permissions(bundle.join("bin/atrium-paint"), p).unwrap();
}

fn insula_binary() -> PathBuf {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent().unwrap().to_path_buf();
    build_neighbor_at(&workspace, "insula-cli", "insula")
}

fn build_neighbor_at(workspace: &Path, crate_name: &str, bin_name: &str) -> PathBuf {
    let dir = workspace.join(crate_name);
    let out = Command::new("cargo")
        .arg("build").arg("--quiet")
        .arg("--manifest-path").arg(dir.join("Cargo.toml"))
        .arg("--bin").arg(bin_name)
        .output().expect("cargo build neighbor");
    assert!(out.status.success(),
            "cargo build {}: {}", crate_name, String::from_utf8_lossy(&out.stderr));
    dir.join("target").join("debug").join(bin_name)
}

fn run_insula(
    install_root: &Path,
    extra_env: &[(&str, &str)],
    args: &[&str],
) -> std::process::Output {
    let logd_bin = build_neighbor("insula-logd");
    let vest_bin = build_neighbor("vestibulum-macos");
    let netd_bin = build_neighbor("atrium-netd-macos");
    let praeco_bin = build_neighbor("praeco-macos");
    let tabel_bin = build_neighbor("tabellarius-macos");

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
    cmd.args(args).output().expect("insula")
}

#[test]
fn insula_launch_threads_fresco_socket_into_sandboxed_child() {
    if !sandbox_exec_available() {
        eprintln!("skipping: sandbox-exec not available");
        return;
    }

    let install_root = tempfile::tempdir().unwrap();
    let bundle_src = tempfile::tempdir().unwrap();
    let sock_dir = tempfile::tempdir().unwrap();
    let sock = sock_dir.path().join("fresco.sock");

    let paint_bin = PathBuf::from(env!("CARGO_BIN_EXE_atrium-paint"));
    build_paint_bundle(bundle_src.path(), &paint_bin);

    let ops = spawn_stub(sock.clone(), 13);
    while !sock.exists() {
        thread::sleep(Duration::from_millis(5));
    }

    // install
    let out = run_insula(install_root.path(), &[], &[
        "install", bundle_src.path().to_str().unwrap(), "--allow-unsigned",
    ]);
    assert!(out.status.success(),
            "install: {}", String::from_utf8_lossy(&out.stderr));

    // launch — pass INSULA_FRESCO_SOCKET so the CLI
    // threads $ATRIUM_FRESCO_SOCKET + SBPL grant.
    let sock_str = sock.to_string_lossy().to_string();
    let out = run_insula(install_root.path(),
                         &[("INSULA_FRESCO_SOCKET", sock_str.as_str()),
                           ("ATRIUM_PAINT_MAX_MS", "3000")],
                         &["launch", "com.atrium-os.atrium-paint"]);
    assert!(out.status.success(),
            "launch: stdout={} stderr={}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr));

    thread::sleep(Duration::from_millis(200));

    // The stub should have seen the full sequence.
    let observed = ops.lock().unwrap().clone();
    assert!(observed.starts_with(&[
        OP_WINDOW_CREATE, 0x0030, 0x0040, 0x0031,
    ]), "expected CREATE + scene-graph; got {:?}", observed);
    assert!(observed.contains(&OP_WINDOW_DESTROY),
            "expected DESTROY after close-requested; got {:?}", observed);

    run_insula(install_root.path(), &[], &["daemons", "down"]);
}
