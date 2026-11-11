//! fake-pad — a stand-in vinculumd that hands tactusd one Bluetooth
//! gamepad and plays a fixed script on it, for the gamepad check
//! (`GAMEPAD=1 scripts/vm-tactus-check.sh`, atrium-input.md §8 T5).
//!
//! Usage: fake-pad HID_SOCKET [PAUSE_SECONDS]
//!
//! Binds HID_SOCKET (tactusd's `TACTUS_BLUETOOTH_SOCKET`), waits for
//! tactusd, attaches the pad, then: centred sticks; left stick right and
//! button 1; release; hat right. Then it waits PAUSE_SECONDS (default 10;
//! the check moves focus meanwhile), presses and releases button 2, and
//! detaches the pad. Each step is printed.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::time::Duration;

use vinculum_hid_wire as bt;

/// Report id 1: four 16-bit sticks (X, Y, Rx, Ry), a 10-bit Z trigger,
/// a 10-bit Simulation Brake, a 1..=8 hat, 15 buttons (tactus's
/// `GAMEPAD_16BIT` test descriptor).
const PAD: &[u8] = &[
    0x05, 0x01, 0x09, 0x05, 0xa1, 0x01, 0x85, 0x01, //
    0x09, 0x01, 0xa1, 0x00, 0x09, 0x30, 0x09, 0x31, 0x15, 0x00, 0x27, 0xff, 0xff, 0x00, 0x00, 0x75, 0x10, 0x95, 0x02,
    0x81, 0x02, 0xc0, //
    0x09, 0x01, 0xa1, 0x00, 0x09, 0x33, 0x09, 0x34, 0x15, 0x00, 0x27, 0xff, 0xff, 0x00, 0x00, 0x75, 0x10, 0x95, 0x02,
    0x81, 0x02, 0xc0, //
    0x05, 0x01, 0x09, 0x32, 0x15, 0x00, 0x26, 0xff, 0x03, 0x75, 0x0a, 0x95, 0x01, 0x81, 0x02, 0x75, 0x06, 0x95, 0x01,
    0x81, 0x03, //
    0x05, 0x02, 0x09, 0xc5, 0x15, 0x00, 0x26, 0xff, 0x03, 0x75, 0x0a, 0x95, 0x01, 0x81, 0x02, 0x75, 0x06, 0x95, 0x01,
    0x81, 0x03, //
    0x05, 0x01, 0x09, 0x39, 0x15, 0x01, 0x25, 0x08, 0x35, 0x00, 0x46, 0x3b, 0x01, 0x66, 0x14, 0x00, 0x75, 0x04, 0x95,
    0x01, 0x81, 0x42, //
    0x75, 0x04, 0x95, 0x01, 0x15, 0x00, 0x25, 0x00, 0x35, 0x00, 0x45, 0x00, 0x65, 0x00, 0x81, 0x03, //
    0x05, 0x09, 0x19, 0x01, 0x29, 0x0f, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x0f, 0x81, 0x02, 0x75, 0x01, 0x95,
    0x01, 0x81, 0x03, //
    0xc0,
];

fn report(lx: u16, hat: u8, buttons: u16) -> Vec<u8> {
    let mut r = Vec::new();
    for v in [lx, 0x8000, 0x8000, 0x8000] {
        r.extend(v.to_le_bytes());
    }
    r.extend([0, 0, 0, 0, hat]);
    r.extend(buttons.to_le_bytes());
    bt::packet(bt::ReportHeader { kind: bt::Kind::Input, report_id: 1, ts_ns: 0 }, &r)
}

fn main() -> std::io::Result<()> {
    let path = std::env::args().nth(1).expect("usage: fake-pad HID_SOCKET [PAUSE_SECONDS]");
    let pause: u64 = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(10);
    let _ = std::fs::remove_file(&path);
    let l = UnixListener::bind(&path)?;
    println!("fake-pad: listening on {path}");
    let (ctl, _) = l.accept()?;
    println!("fake-pad: tactusd connected");
    let (mine, theirs) = bt::seqpacket_pair()?;
    let rec = bt::Record { name: "Fake Pad".into(), vendor: 0x1234, product: 0x5678, version: 1 };
    let msgs = bt::ToTactus::attach(1, rec, bt::Transport::Le, true, PAD);
    bt::send_with_fd(&ctl, &bt::encode(&msgs[0]), theirs.as_raw_fd())?;
    for m in &msgs[1..] {
        (&ctl).write_all(&bt::encode(m))?;
    }
    drop(theirs);
    let mut pad = std::fs::File::from(mine);
    let step = |pad: &mut std::fs::File, what: &str, r: Vec<u8>| -> std::io::Result<()> {
        std::thread::sleep(Duration::from_millis(1500));
        println!("fake-pad: {what}");
        pad.write_all(&r)
    };
    step(&mut pad, "centred", report(0x8000, 0, 0))?;
    step(&mut pad, "left stick right, button 1", report(0xffff, 0, 1))?;
    step(&mut pad, "release", report(0x8000, 0, 0))?;
    step(&mut pad, "hat right", report(0x8000, 3, 0))?;
    println!("fake-pad: pausing {pause} s");
    std::thread::sleep(Duration::from_secs(pause));
    step(&mut pad, "button 2", report(0x8000, 3, 2))?;
    step(&mut pad, "release", report(0x8000, 3, 0))?;
    std::thread::sleep(Duration::from_millis(1000));
    (&ctl).write_all(&bt::encode(&bt::ToTactus::Detach { device: 1 }))?;
    drop(pad);
    println!("fake-pad: detached");
    std::thread::sleep(Duration::from_secs(2));
    Ok(())
}
