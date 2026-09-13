//! portcullisd-daemon must serve every in-jail client while others hold
//! connections open. atrium-portcullisd-aq lingers on its connection after
//! an attach; with the old serve-to-completion loop the next jail's request
//! would have waited for it to exit. Runs the real daemon binary.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use aqueduct::{classes, envelope::flag, Connection};
use portcullis_protocol::OP_MOUNT_REPLY;

/// Reply deadline. A starved client waits forever, so every read is bounded.
const DEADLINE: Duration = Duration::from_secs(3);

/// An opcode the daemon does not implement: it answers with
/// MountReply::Error without touching jaild, which this test does not run.
const OP_UNKNOWN: u16 = 0x7777;

struct Daemon {
    child: Child,
    sock:  PathBuf,
    _dir:  tempfile::TempDir,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start() -> Daemon {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("portcullisd.sock");
    let svcdir = dir.path().join("services.d");
    std::fs::create_dir(&svcdir).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_atrium-portcullisd-daemon"))
        .arg("--socket").arg(&sock)
        .arg("--jaild-socket").arg(dir.path().join("no-jaild.sock"))
        .arg("--services-dir").arg(&svcdir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon");
    let t0 = Instant::now();
    while UnixStream::connect(&sock).is_err() {
        assert!(t0.elapsed() < Duration::from_secs(10), "daemon never bound its socket");
        std::thread::sleep(Duration::from_millis(20));
    }
    Daemon { child, sock, _dir: dir }
}

fn connect(sock: &Path) -> Connection {
    let c = Connection::connect(sock).unwrap();
    c.set_read_timeout(Some(DEADLINE)).unwrap();
    c
}

fn request(c: &mut Connection) {
    c.send_message(classes::CLASS_PORTCULLIS, OP_UNKNOWN,
        flag::RESPONSE_EXPECTED, b"{}").unwrap();
}

fn expect_reply(c: &mut Connection, who: &str) {
    match c.recv_message() {
        Ok(m) => {
            assert_eq!(m.opcode_class, classes::CLASS_PORTCULLIS, "{who}");
            assert_eq!(m.op, OP_MOUNT_REPLY, "{who}");
        }
        Err(e) => panic!("{who}: no reply within {DEADLINE:?}: {e}"),
    }
}

#[test]
fn second_client_is_served_while_first_connection_stays_open() {
    let d = start();
    let mut a = connect(&d.sock);
    request(&mut a);
    expect_reply(&mut a, "A");

    // A lingers, as atrium-portcullisd-aq does after its attach.
    let mut b = connect(&d.sock);
    request(&mut b);
    expect_reply(&mut b, "B (while A is open)");

    request(&mut a);
    expect_reply(&mut a, "A (again)");
}

#[test]
fn half_sent_envelope_does_not_block_other_clients() {
    let d = start();
    let mut a = UnixStream::connect(&d.sock).unwrap();
    a.write_all(&[aqueduct::envelope::ENVELOPE_VERSION, classes::CLASS_PORTCULLIS, 0x77])
        .unwrap();
    std::thread::sleep(Duration::from_millis(100));

    let mut b = connect(&d.sock);
    request(&mut b);
    expect_reply(&mut b, "B (while A is mid-envelope)");
}
