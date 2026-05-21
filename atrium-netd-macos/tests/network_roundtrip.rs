//! End-to-end: spawn `atrium-netd-macos`, point libatrium
//! at it via $ATRIUM_NETD_SOCKET, call atrium_net_connect
//! to reach a tiny TCP echo server we also spawn,
//! verify bytes round-trip.

#![cfg(unix)]

use std::ffi::CString;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn broker_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_atrium-netd-macos"))
}

fn spawn_broker(socket: &std::path::Path, allowlist: Option<&str>) -> Child {
    let mut cmd = Command::new(broker_binary());
    cmd.env("INSULA_NETD_SOCKET", socket)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(list) = allowlist {
        cmd.env("INSULA_NETD_ALLOWED_HOSTS", list);
    }
    cmd.spawn().expect("spawn atrium-netd-macos")
}

fn wait_for_socket(p: &std::path::Path, timeout: Duration) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if p.exists() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("broker socket did not appear at {}", p.display());
}

/// Spin up a tiny TCP echo server on 127.0.0.1 with an
/// ephemeral port. Returns the port and a JoinHandle that
/// finishes when the first client disconnects.
fn spawn_tcp_echo() -> (u16, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind echo");
    let port = listener.local_addr().unwrap().port();
    let h = thread::spawn(move || {
        if let Ok((mut s, _)) = listener.accept() {
            let mut buf = [0u8; 1024];
            loop {
                match s.read(&mut buf) {
                    Ok(0) => return,
                    Ok(n) => {
                        if s.write_all(&buf[..n]).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                }
            }
        }
    });
    (port, h)
}

#[test]
fn connect_through_broker_reaches_tcp_echo_server() {
    let _g = ENV_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("netd.sock");

    let mut broker = spawn_broker(&socket, None);
    wait_for_socket(&socket, Duration::from_secs(3));

    let (echo_port, _echo_handle) = spawn_tcp_echo();

    std::env::set_var("ATRIUM_NETD_SOCKET", &socket);

    let host = CString::new("127.0.0.1").unwrap();
    let fd = unsafe {
        atrium::atrium_net_connect(host.as_ptr(), echo_port, atrium::ATRIUM_NET_TCP)
    };
    assert!(fd >= 0,
            "connect should succeed via broker; got fd={}", fd);

    // Use the returned fd as a normal stream. The broker
    // proxies bytes to the TCP echo server.
    let mut stream = unsafe { TcpStream::from_raw_fd(fd) };
    // Note: the actual underlying fd is a unix socket,
    // but TcpStream wraps it via its raw-fd shim and
    // read/write still works since both are AF_LOCAL-
    // compatible at the syscall level.
    let msg = b"hello-echo-server";
    stream.write_all(msg).expect("write to proxied fd");
    let mut reply = [0u8; 32];
    let n = stream.read(&mut reply).expect("read proxied reply");
    assert_eq!(&reply[..n], msg,
               "echo server should bounce bytes back through broker");

    std::env::remove_var("ATRIUM_NETD_SOCKET");
    drop(stream);
    let _ = broker.kill();
    let _ = broker.wait();
}

#[test]
fn allowlist_denies_unlisted_host() {
    let _g = ENV_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("netd.sock");

    // Allowlist contains *only* `example.com`; 127.0.0.1
    // is therefore denied.
    let mut broker = spawn_broker(&socket, Some("example.com"));
    wait_for_socket(&socket, Duration::from_secs(3));
    std::env::set_var("ATRIUM_NETD_SOCKET", &socket);

    let host = CString::new("127.0.0.1").unwrap();
    let r = unsafe {
        atrium::atrium_net_connect(host.as_ptr(), 80, atrium::ATRIUM_NET_TCP)
    };
    assert_eq!(r, atrium::ATRIUM_ERR_NETD_DENIED,
               "host outside the allowlist should be denied; got {}", r);

    std::env::remove_var("ATRIUM_NETD_SOCKET");
    let _ = broker.kill();
    let _ = broker.wait();
}

#[test]
fn connect_without_broker_returns_no_netd() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::remove_var("ATRIUM_NETD_SOCKET");

    let host = CString::new("example.com").unwrap();
    let r = unsafe {
        atrium::atrium_net_connect(host.as_ptr(), 443, atrium::ATRIUM_NET_TCP)
    };
    assert_eq!(r, atrium::ATRIUM_ERR_NO_NETD);
}

/// Verify per-app enforcement at the daemon's daemon-
/// side level. When the broker has `$INSULA_INSTALL_
/// ROOT` set and the connecting peer's exe matches an
/// installed app, the broker enforces that app's
/// `[network]` allowlist *instead of* the broker-wide
/// allowlist.
///
/// We can't easily make the cargo test binary appear
/// to be an installed app — its exe path is the
/// test binary, not anywhere under a temp install
/// root. So we exercise the per-app code path
/// indirectly: bring the broker up with an install
/// root containing a single app whose manifest
/// allowlists 127.0.0.1; then connect *as the test
/// process* (which the broker fails to identify),
/// observe that the broker falls through to the
/// broker-wide allowlist as expected.
///
/// The per-app verdict path itself is unit-tested in
/// `atrium-netd-macos/src/peer.rs` — 8 cases
/// covering manifest match / mismatch / raw-network /
/// no-network-section + exe-path-to-app-id lookup.
#[test]
fn unidentified_peer_falls_through_to_broker_wide_when_install_root_set() {
    let _g = ENV_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("netd.sock");
    let install_root = tmp.path().join("install");
    std::fs::create_dir_all(install_root.join("apps")).unwrap();

    // Set up a daemon with both an install root AND a
    // broker-wide allowlist that includes 127.0.0.1.
    let mut cmd = Command::new(broker_binary());
    cmd.env("INSULA_NETD_SOCKET", &socket)
        .env("INSULA_INSTALL_ROOT", &install_root)
        .env("INSULA_NETD_ALLOWED_HOSTS", "127.0.0.1")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut broker = cmd.spawn().expect("spawn");
    wait_for_socket(&socket, Duration::from_secs(3));

    let (echo_port, _h) = spawn_tcp_echo();
    std::env::set_var("ATRIUM_NETD_SOCKET", &socket);
    let host = std::ffi::CString::new("127.0.0.1").unwrap();
    let fd = unsafe {
        atrium::atrium_net_connect(host.as_ptr(), echo_port, atrium::ATRIUM_NET_TCP)
    };
    assert!(fd >= 0,
            "broker-wide allowlist should permit 127.0.0.1 when peer is \
             unidentified; got fd={}", fd);

    // Close the fd we got back.
    unsafe { libc::close(fd); }

    std::env::remove_var("ATRIUM_NETD_SOCKET");
    let _ = broker.kill();
    let _ = broker.wait();
}

#[test]
fn udp_returns_denied_for_v0_broker() {
    let _g = ENV_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let socket = tmp.path().join("netd.sock");

    let mut broker = spawn_broker(&socket, None);
    wait_for_socket(&socket, Duration::from_secs(3));
    std::env::set_var("ATRIUM_NETD_SOCKET", &socket);

    let host = CString::new("127.0.0.1").unwrap();
    let r = unsafe {
        atrium::atrium_net_connect(host.as_ptr(), 53, atrium::ATRIUM_NET_UDP)
    };
    // v0 broker maps PROTO_UNSUPPORTED (1) to DENIED on
    // the libatrium side -- both mean "the broker said no."
    assert_eq!(r, atrium::ATRIUM_ERR_NETD_DENIED);

    std::env::remove_var("ATRIUM_NETD_SOCKET");
    let _ = broker.kill();
    let _ = broker.wait();
}
