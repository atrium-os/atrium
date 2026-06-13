//! The choragusd→lyrad control wire, end-to-end on the host: Choragus resolves a
//! ducking change and sends it; a stand-in listener (lyrad's role) decodes it.

use choragus::policy::{diff, Device, DeviceKind, Role, Session, Stream};
use lyra_protocol::{Ctl, FRAME_LEN};
use std::io::Read;
use std::os::unix::net::UnixListener;

#[test]
fn ducking_change_crosses_the_wire_to_the_engine() {
    let sock = std::env::temp_dir().join(format!("choragus_wire_{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).expect("bind");

    // Choragus side: a call arrives while media plays -> media ducks to -18 dB.
    let spk = Device { id: 0, kind: DeviceKind::Speakers };
    let mut s = Session::new(vec![spk], spk.id);
    s.open(Stream::new(0, Role::Media)).unwrap();
    let before = s.resolve();
    s.open(Stream::new(1, Role::Communication)).unwrap();
    let changes = diff(&before, &s.resolve());

    let sock_s = sock.to_string_lossy().to_string();
    let sender = std::thread::spawn(move || choragus::control::send(&sock_s, &changes));

    // lyrad side: accept, decode the frames.
    let (mut conn, _) = listener.accept().expect("accept");
    let mut got = Vec::new();
    let mut frame = [0u8; FRAME_LEN];
    while conn.read_exact(&mut frame).is_ok() {
        got.push(Ctl::decode(&frame).expect("valid frame"));
    }
    sender.join().unwrap().expect("send ok");
    let _ = std::fs::remove_file(&sock);

    // the engine received exactly the ducking instruction.
    assert_eq!(got, vec![Ctl::SetGainDb { stream: 0, db: -18.0 }]);
}
