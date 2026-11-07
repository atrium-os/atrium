//! Replays recorded controller traffic through the parsers. The trace was
//! recorded by `vinculum/harness/run.sh --record` (Rootcanal 1.13 controller,
//! Bumble 0.0.236 peer advertising "VinculumPeer", vinculum-probe as host),
//! so this checks the generated parsers against what a real controller
//! implementation and a real peer stack produce, on any host, offline.

use vinculum_hci_proto::btsnoop::{self, Direction};
use vinculum_hci_proto::gap;
use vinculum_hci_proto::h4::{Decoder, Kind};
use vinculum_hci_proto::hci::{self, Command, Event, EventChild, LeMetaEventChild};
use vinculum_hci_proto::Packet;

const TRACE: &[u8] = include_bytes!("data/rootcanal-bumble-scan.btsnoop");

#[test]
fn every_packet_parses() {
    let recs = btsnoop::read_all(TRACE).unwrap();
    assert!(recs.len() >= 14, "trace has {} records", recs.len());
    let mut completes = Vec::new();
    let mut names = Vec::new();
    let mut commands = Vec::new();
    for r in &recs {
        match (r.direction, r.frame.kind) {
            (Direction::HostToController, Kind::Command) => {
                let c = Command::decode_full(&r.frame.packet).unwrap();
                c.specialize().unwrap();
                commands.push(c.op_code);
            }
            (Direction::ControllerToHost, Kind::Event) => {
                let e = Event::decode_full(&r.frame.packet).unwrap();
                match e.specialize().unwrap() {
                    EventChild::CommandComplete(cc) => {
                        assert_eq!(cc.payload[0], 0, "{:?} failed", cc.command_op_code);
                        completes.push(cc.command_op_code);
                        if cc.command_op_code == hci::OpCode::ReadBdAddr {
                            hci::ReadBdAddrComplete::try_from(&cc).unwrap();
                        }
                    }
                    EventChild::LeMetaEvent(m) => match m.specialize().unwrap() {
                        LeMetaEventChild::LeAdvertisingReport(rep) => {
                            for resp in rep.responses {
                                let ad = gap::parse(&resp.advertising_data).unwrap();
                                names.extend(gap::local_name(&ad));
                            }
                        }
                        other => panic!("unexpected LE event {other:?}"),
                    },
                    other => panic!("unexpected event {other:?}"),
                }
            }
            other => panic!("unexpected record {other:?}"),
        }
    }
    assert_eq!(commands, completes, "every command completed, in order");
    assert!(commands.contains(&hci::OpCode::LeSetScanEnable));
    assert!(names.iter().any(|n| n == "VinculumPeer"), "names seen: {names:?}");
}

#[test]
fn trace_survives_byte_stream_reassembly() {
    // Concatenate the controller's frames as a UART would deliver them and
    // feed them back in odd-sized pieces.
    let recs = btsnoop::read_all(TRACE).unwrap();
    let frames: Vec<_> = recs.iter().filter(|r| r.direction == Direction::ControllerToHost).map(|r| r.frame.clone()).collect();
    let stream: Vec<u8> = frames.iter().flat_map(|f| f.to_bytes()).collect();
    for chunk in [1, 2, 3, 7, 64, 4096] {
        let mut d = Decoder::new();
        let mut got = Vec::new();
        for piece in stream.chunks(chunk) {
            d.push(piece, |f| got.push(f)).unwrap();
        }
        assert_eq!(got, frames, "chunk size {chunk}");
    }
}

/// Mutated controller frames never panic the parsers (a cheap, always-on
/// complement to the cargo-fuzz targets in vinculum/fuzz).
#[test]
fn mutations_do_not_panic() {
    let recs = btsnoop::read_all(TRACE).unwrap();
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for r in recs.iter().filter(|r| r.frame.kind == Kind::Event) {
        for _ in 0..2000 {
            let mut p = r.frame.packet.clone();
            match rnd() % 3 {
                0 => {
                    let i = (rnd() as usize) % p.len();
                    p[i] = rnd() as u8;
                }
                1 => p.truncate((rnd() as usize) % (p.len() + 1)),
                _ => p.extend((0..rnd() % 8).map(|_| rnd() as u8)),
            }
            if let Ok(e) = Event::decode_full(&p) {
                if let Ok(EventChild::LeMetaEvent(m)) = e.specialize() {
                    if let Ok(LeMetaEventChild::LeAdvertisingReport(rep)) = m.specialize() {
                        for resp in rep.responses {
                            let _ = gap::parse(&resp.advertising_data);
                        }
                    }
                }
            }
        }
    }
}
