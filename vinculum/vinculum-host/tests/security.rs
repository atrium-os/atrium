//! Host link security against a scripted controller: SMP routing, the
//! encryption rules, and disconnects that let the last PDU out. Pairing
//! itself is tested in vinculum-smp and end to end against Bumble
//! (harness/e2e.sh).

use vinculum_hci_proto::h4::{Frame, Kind};
use vinculum_hci_proto::hci::{self, ErrorCode, OpCode};
use vinculum_hci_proto::Packet;
use vinculum_host::testing::{event, le_meta, ready_host, status};
use vinculum_host::{Host, HostEvent, PairConfig, DISCONNECT_DRAIN_MS};
use vinculum_smp::pairing::IoCap;

fn le_conn(handle: u16, role: hci::Role) -> Frame {
    le_meta(hci::LeConnectionComplete {
        status: ErrorCode::Success,
        connection_handle: handle,
        role,
        peer_address_type: hci::AddressType::RandomDeviceAddress,
        peer_address: hci::Address::try_from(0xC000_0000_0001u64).unwrap(),
        connection_interval: 24,
        peripheral_latency: 0,
        supervision_timeout: 500,
        central_clock_accuracy: hci::ClockAccuracy::Ppm500,
    })
}

fn acl(handle: u16, cid: u16, payload: &[u8]) -> Frame {
    let mut p = (payload.len() as u16).to_le_bytes().to_vec();
    p.extend_from_slice(&cid.to_le_bytes());
    p.extend_from_slice(payload);
    let a = hci::Acl {
        handle,
        packet_boundary_flag: hci::PacketBoundaryFlag::FirstAutomaticallyFlushable,
        broadcast_flag: hci::BroadcastFlag::PointToPoint,
        payload: p,
    };
    Frame { kind: Kind::Acl, packet: a.encode_to_vec().unwrap() }
}

/// Commands and ACL payloads (L2CAP PDUs, unfragmented) sent.
fn drain(h: &mut Host) -> (Vec<OpCode>, Vec<Vec<u8>>) {
    let (mut c, mut a) = (Vec::new(), Vec::new());
    while let Some(f) = h.poll_frame() {
        match f.kind {
            Kind::Command => c.push(hci::Command::decode_full(&f.packet).unwrap().op_code),
            Kind::Acl => a.push(hci::Acl::decode_full(&f.packet).unwrap().payload),
            _ => {}
        }
    }
    (c, a)
}

fn events(h: &mut Host) -> Vec<HostEvent> {
    std::iter::from_fn(|| h.poll_event()).collect()
}

fn enc_change(handle: u16, on: bool) -> Frame {
    event(hci::EncryptionChange {
        status: ErrorCode::Success,
        connection_handle: handle,
        encryption_enabled: if on { hci::EncryptionEnabled::On } else { hci::EncryptionEnabled::Off },
    })
}

fn cfg() -> PairConfig {
    PairConfig { io_cap: IoCap::KeyboardDisplay, irk: [7; 16] }
}

#[test]
fn ltk_requests_are_refused() {
    let mut h = ready_host(4);
    h.on_frame(le_conn(1, hci::Role::Peripheral), 0);
    h.on_frame(le_meta(hci::LeLongTermKeyRequest { connection_handle: 1, random_number: [0; 8], encrypted_diversifier: 0 }), 0);
    assert_eq!(drain(&mut h).0, vec![OpCode::LeLongTermKeyRequestNegativeReply]);
}

#[test]
fn only_a_central_pairs() {
    let mut h = ready_host(4);
    h.on_frame(le_conn(1, hci::Role::Peripheral), 0);
    assert!(h.pair(1, cfg(), 0).is_err());
    assert!(h.encrypt(1, [1; 16], 0).is_err());
    assert!(h.pair(9, cfg(), 0).is_err(), "unknown handle");
}

#[test]
fn pairing_request_goes_out_on_the_smp_channel() {
    let mut h = ready_host(4);
    h.on_frame(le_conn(1, hci::Role::Central), 0);
    events(&mut h);
    h.pair(1, cfg(), 0).unwrap();
    let (_, acl) = drain(&mut h);
    assert_eq!(acl.len(), 1);
    assert_eq!(&acl[0][2..5], &[0x06, 0x00, 0x01], "CID 6, Pairing Request");
    assert!(h.pair(1, cfg(), 0).is_err(), "one procedure at a time");
    // The peer fails it: reported, and SMP traffic is not handed up as L2CAP.
    h.on_frame(acl_frame(1, &[0x05, 0x03]), 1);
    assert!(matches!(events(&mut h).as_slice(), [HostEvent::PairingFailed { handle: 1, reason: 3, .. }]));
}

fn acl_frame(handle: u16, smp: &[u8]) -> Frame {
    acl(handle, 0x0006, smp)
}

#[test]
fn smp_timeout_ends_the_link() {
    let mut h = ready_host(4);
    h.on_frame(le_conn(1, hci::Role::Central), 0);
    events(&mut h);
    h.pair(1, cfg(), 0).unwrap();
    drain(&mut h);
    let d = h.deadline().unwrap();
    h.tick(d);
    assert!(matches!(events(&mut h).as_slice(), [HostEvent::PairingFailed { handle: 1, .. }]));
    let (cmds, acl) = drain(&mut h);
    assert_eq!(cmds, vec![OpCode::Disconnect]);
    assert!(acl.is_empty(), "no SMP PDU after a timeout");
}

#[test]
fn unsolicited_smp_is_refused_and_security_request_reported() {
    let mut h = ready_host(4);
    h.on_frame(le_conn(1, hci::Role::Central), 0);
    events(&mut h);
    h.on_frame(acl_frame(1, &[0x0B, 0x0D]), 0);
    assert_eq!(events(&mut h), vec![HostEvent::SecurityRequest { handle: 1, auth_req: 0x0D }]);
    h.on_frame(acl_frame(1, &[0x01, 3, 0, 1, 16, 0, 0]), 0);
    let (_, acl) = drain(&mut h);
    assert_eq!(acl, vec![vec![2, 0, 6, 0, 0x05, 0x05]], "Pairing Not Supported");
    assert_eq!(h.stats().link_violations, 1);
}

#[test]
fn stored_key_encryption_and_its_failures() {
    let mut h = ready_host(4);
    h.on_frame(le_conn(1, hci::Role::Central), 0);
    events(&mut h);
    // An Encryption Change nobody asked for is a violation, not trust.
    h.on_frame(enc_change(1, true), 0);
    assert!(!h.is_encrypted(1));
    assert_eq!(h.stats().link_violations, 1);

    h.encrypt(1, [1; 16], 0).unwrap();
    assert_eq!(drain(&mut h).0, vec![OpCode::LeStartEncryption]);
    h.on_frame(status(OpCode::LeStartEncryption, ErrorCode::Success), 0);
    h.on_frame(enc_change(1, true), 0);
    assert!(h.is_encrypted(1));
    assert_eq!(events(&mut h), vec![HostEvent::Encrypted { handle: 1, key_size: 16 }]);

    // Encryption switched off under us: the link goes.
    h.on_frame(enc_change(1, false), 0);
    assert!(!h.is_encrypted(1));
    assert_eq!(drain(&mut h).0, vec![OpCode::Disconnect]);
}

#[test]
fn short_key_or_refused_start_fails() {
    let mut h = ready_host(4);
    h.on_frame(le_conn(1, hci::Role::Central), 0);
    events(&mut h);
    h.encrypt(1, [1; 16], 0).unwrap();
    drain(&mut h);
    h.on_frame(status(OpCode::LeStartEncryption, ErrorCode::Success), 0);
    h.on_frame(
        event(hci::EncryptionChangeV2 {
            status: ErrorCode::Success,
            connection_handle: 1,
            encryption_enabled: hci::EncryptionEnabled::On,
            key_size: 7,
        }),
        0,
    );
    assert!(matches!(events(&mut h).as_slice(), [HostEvent::EncryptionFailed { handle: 1, .. }]));
    assert!(drain(&mut h).0.contains(&OpCode::Disconnect));

    let mut h = ready_host(4);
    h.on_frame(le_conn(1, hci::Role::Central), 0);
    events(&mut h);
    h.encrypt(1, [1; 16], 0).unwrap();
    drain(&mut h);
    h.on_frame(status(OpCode::LeStartEncryption, ErrorCode::PinOrKeyMissing), 0);
    assert!(matches!(events(&mut h).as_slice(), [HostEvent::EncryptionFailed { handle: 1, .. }]));
    assert!(!h.is_encrypted(1));
}

#[test]
fn disconnect_waits_for_queued_data() {
    let mut h = ready_host(4);
    h.on_frame(le_conn(1, hci::Role::Central), 0);
    h.send_l2cap(1, 4, &[1, 2, 3], 0).unwrap();
    h.disconnect(1, 0);
    let (cmds, acl) = drain(&mut h);
    assert_eq!(acl.len(), 1);
    assert!(cmds.is_empty(), "held until the controller completes the packet");
    h.on_frame(
        event(hci::NumberOfCompletedPackets {
            completed_packets: vec![hci::CompletedPackets { connection_handle: 1, host_num_of_completed_packets: 1 }],
        }),
        5,
    );
    assert_eq!(drain(&mut h).0, vec![OpCode::Disconnect]);

    // A controller that never completes: the drain gives up.
    let mut h = ready_host(4);
    h.on_frame(le_conn(1, hci::Role::Central), 0);
    h.send_l2cap(1, 4, &[1], 0).unwrap();
    h.disconnect(1, 0);
    drain(&mut h);
    assert_eq!(h.deadline(), Some(DISCONNECT_DRAIN_MS));
    h.tick(DISCONNECT_DRAIN_MS);
    assert_eq!(drain(&mut h).0, vec![OpCode::Disconnect]);
}
