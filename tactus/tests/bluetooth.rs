//! tactusd's Bluetooth HID source (bluetooth.md §10) against a stand-in
//! vinculumd: Attach with an fd, report-id packets, Dropped, keyboard
//! rights, Detach.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use tactus::wire::{self, Bus, Message};
use vinculum_hid_wire as bt;

/// A boot keyboard with report id 1 (Bluetooth keyboards number their
/// reports).
const KEYBOARD: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x85, 0x01, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0x00, 0x25, 0x01, 0x75,
    0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01, 0x95, 0x06, 0x75, 0x08, 0x15, 0x00, 0x25, 0x65,
    0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00, 0xc0,
];

struct Kill(Child);
impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn input(report: &[u8]) -> Vec<u8> {
    bt::packet(bt::ReportHeader { kind: bt::Kind::Input, report_id: 1, ts_ns: 0 }, report)
}

/// The next message that is not a repeat.
fn next(c: &mut UnixStream) -> Message {
    loop {
        match wire::read(c).expect("tactusd message") {
            Message::Key { repeat: true, .. } => continue,
            m => return m,
        }
    }
}

#[test]
fn bluetooth_hid_source() {
    let dir = std::env::temp_dir().join(format!("tactus-bt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let hid = dir.join("hid.sock");
    let sock = dir.join("tactus.sock");
    let vinculumd = UnixListener::bind(&hid).unwrap();
    let _d = Kill(
        Command::new(env!("CARGO_BIN_EXE_tactusd"))
            .arg("--socket")
            .arg(&sock)
            .env("TACTUS_PEERS", "any")
            .env("TACTUS_BLUETOOTH_SOCKET", &hid)
            .spawn()
            .unwrap(),
    );

    // tactusd says hello to vinculumd.
    let (ctl, _) = vinculumd.accept().unwrap();
    ctl.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut r = bt::Reader::default();
    let mut buf = [0u8; 256];
    let hello = loop {
        if let Some(m) = r.next::<bt::ToVinculum>().unwrap() {
            break m;
        }
        let (n, _) = bt::recv_with_fd(&ctl, &mut buf).unwrap();
        assert!(n > 0, "tactusd closed");
        r.push(&buf[..n]);
    };
    assert_eq!(hello, bt::ToVinculum::Hello { version: bt::VERSION });

    // frescod's side.
    let t0 = Instant::now();
    let mut c = loop {
        match UnixStream::connect(&sock) {
            Ok(c) => break c,
            Err(_) if t0.elapsed() < Duration::from_secs(5) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => panic!("tactusd socket: {e}"),
        }
    };
    c.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    assert_eq!(next(&mut c), Message::Hello { version: wire::VERSION });

    let attach = |device: u64, keyboard_allowed: bool| {
        let (mine, theirs) = bt::seqpacket_pair().unwrap();
        let rec = bt::Record { name: format!("bt kbd {device}"), vendor: 0x05ac, product: 0x0256, version: 1 };
        let msgs = bt::ToTactus::attach(device, rec, bt::Transport::Classic, keyboard_allowed, KEYBOARD);
        bt::send_with_fd(&ctl, &bt::encode(&msgs[0]), theirs.as_raw_fd()).unwrap();
        std::fs::File::from(mine)
    };

    // A bonded keyboard: its keys arrive, and Dropped releases them.
    let mut kbd = attach(7, true);
    let Message::Device { id, bus, name, .. } = next(&mut c) else { panic!("expected Device") };
    assert_eq!((bus, name.as_str()), (Bus::Bluetooth, "bt kbd 7"));
    kbd.write_all(&input(&[0x02, 0, 0x04, 0, 0, 0, 0, 0])).unwrap(); // LShift+a
    let mut downs = Vec::new();
    for _ in 0..2 {
        match next(&mut c) {
            Message::Key { id: i, usage, down: true, .. } if i == id => downs.push(usage),
            m => panic!("expected a press, got {m:?}"),
        }
    }
    downs.sort();
    assert_eq!(downs, vec![0x0007_0004, 0x0007_00e1]);
    kbd.write_all(&bt::packet(bt::ReportHeader { kind: bt::Kind::Dropped, report_id: 0, ts_ns: 0 }, &3u32.to_le_bytes()))
        .unwrap();
    let mut ups = Vec::new();
    for _ in 0..2 {
        match next(&mut c) {
            Message::Key { usage, down: false, .. } => ups.push(usage),
            m => panic!("expected a release, got {m:?}"),
        }
    }
    ups.sort();
    assert_eq!(ups, downs);

    // A Just Works bond: no keyboard rights, so its keys are dropped.
    let mut weak = attach(8, false);
    let Message::Device { id: weak_id, .. } = next(&mut c) else { panic!("expected Device") };
    weak.write_all(&input(&[0, 0, 0x05, 0, 0, 0, 0, 0])).unwrap();
    std::thread::sleep(Duration::from_millis(100));
    (&ctl).write_all(&bt::encode(&bt::ToTactus::Detach { device: 8 })).unwrap();
    drop(weak);
    assert_eq!(next(&mut c), Message::Gone { id: weak_id }, "no key from a device without keyboard rights");

    // The keyboard's link goes: its fd closes and it is gone.
    drop(kbd);
    assert_eq!(next(&mut c), Message::Gone { id });
    let _ = std::fs::remove_dir_all(&dir);
}
