//! Per-app executor jails (docs/spec/tier2-robust-access.md §4): the
//! broker hands each connection to a worker process of its own and
//! fails closed when it can't start one.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use aqueduct::Connection;
use aqueduct_gpu::payloads::ClientKind;
use aqueduct_gpu_client::GpuClient;

fn sock(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("aqueduct-broker-{}-{name}.sock", std::process::id()))
}

/// Start the broker; returns it and a channel of its log lines.
fn broker(sock: &std::path::Path, launcher: Option<&str>) -> (Child, mpsc::Receiver<String>) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_aqueduct-gpu-host"));
    cmd.args(["--transport", "broker", "--socket"]).arg(sock)
        .args(["--backend", "software"])
        .env("RUST_LOG", "info")
        .stderr(Stdio::piped());
    match launcher {
        Some(l) => { cmd.env("AQUEDUCT_GPU_WORKER_LAUNCHER", l); }
        None => { cmd.env_remove("AQUEDUCT_GPU_WORKER_LAUNCHER"); }
    }
    let mut child = cmd.spawn().expect("spawn broker");
    let (tx, rx) = mpsc::channel();
    let err = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(err).lines().map_while(Result::ok) {
            eprintln!("[broker] {line}");
            let _ = tx.send(line);
        }
    });
    let t0 = Instant::now();
    while !sock.exists() {
        assert!(t0.elapsed() < Duration::from_secs(10), "broker never bound");
        std::thread::sleep(Duration::from_millis(20));
    }
    (child, rx)
}

/// The next log line containing `pat`.
fn wait_for(rx: &mpsc::Receiver<String>, pat: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(l) if l.contains(pat) => return l,
            Ok(_) => {}
            Err(_) => panic!("no log line with {pat:?}"),
        }
    }
}

fn pid_of(line: &str) -> u32 {
    line.rsplit("pid ").next().unwrap().trim_end_matches(')').parse().unwrap()
}

fn alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

#[test]
#[cfg(target_os = "linux")]
fn broker_gives_each_connection_its_own_worker() {
    let s = sock("own");
    let (mut b, rx) = broker(&s, Some("unconfined"));

    let mut c1 = GpuClient::new(Connection::connect(&s).unwrap());
    c1.handshake(ClientKind::FrescodRenderer).unwrap();
    let w1 = pid_of(&wait_for(&rx, "-> worker"));
    let mut c2 = GpuClient::new(Connection::connect(&s).unwrap());
    c2.handshake(ClientKind::FrescodRenderer).unwrap();
    let w2 = pid_of(&wait_for(&rx, "-> worker"));
    assert_ne!(w1, w2, "two apps share a worker");
    assert!(alive(w1) && alive(w2));

    // A client hanging up ends its worker, and only its worker.
    drop(c1);
    let t0 = Instant::now();
    while alive(w1) {
        assert!(t0.elapsed() < Duration::from_secs(10), "worker outlived its client");
        std::thread::sleep(Duration::from_millis(20));
    }
    c2.handshake(ClientKind::FrescodRenderer).unwrap_or_else(|_| panic!("the other app's worker went too"));
    assert!(alive(w2));

    drop(c2);
    let _ = b.kill();
    let _ = std::fs::remove_file(&s);
}

#[test]
fn broker_refuses_clients_without_portcullisd() {
    let s = sock("closed");
    let (mut b, rx) = broker(&s, None);
    let mut c = GpuClient::new(Connection::connect(&s).unwrap());
    assert!(c.handshake(ClientKind::FrescodRenderer).is_err(),
        "a client was served without a jailed worker");
    wait_for(&rx, "REFUSED");
    let _ = b.kill();
    let _ = std::fs::remove_file(&s);
}
