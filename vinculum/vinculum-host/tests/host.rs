//! Host behaviour against a scripted controller.

use vinculum_hci_proto::h4::{Frame, Kind};
use vinculum_hci_proto::hci::{self, ErrorCode, OpCode};
use vinculum_hci_proto::Packet;
use vinculum_host::testing::{complete, event, le_meta, ready_host, status};
use vinculum_host::{Host, HostEvent, MAX_CONNECTIONS};

fn le_conn(handle: u16, addr: u64) -> Frame {
    le_meta(hci::LeConnectionComplete {
        status: ErrorCode::Success,
        connection_handle: handle,
        role: hci::Role::Central,
        peer_address_type: hci::AddressType::RandomDeviceAddress,
        peer_address: hci::Address::try_from(addr).unwrap(),
        connection_interval: 24,
        peripheral_latency: 0,
        supervision_timeout: 500,
        central_clock_accuracy: hci::ClockAccuracy::Ppm500,
    })
}

fn sent_ops(h: &mut Host) -> Vec<OpCode> {
    let mut v = Vec::new();
    while let Some(f) = h.poll_frame() {
        if f.kind == Kind::Command {
            v.push(hci::Command::decode_full(&f.packet).unwrap().op_code);
        }
    }
    v
}

fn acl(handle: u16, pb: hci::PacketBoundaryFlag, payload: &[u8]) -> Frame {
    let a = hci::Acl { handle, packet_boundary_flag: pb, broadcast_flag: hci::BroadcastFlag::PointToPoint, payload: payload.to_vec() };
    Frame { kind: Kind::Acl, packet: a.encode_to_vec().unwrap() }
}

#[test]
fn bring_up_reports_buffers() {
    let h = ready_host(3);
    assert_eq!(h.info().le_buffers, 3);
    assert_eq!(h.info().acl_mtu, 1021);
    assert_eq!(h.info().address, Some([0x11, 0x22, 0x33, 0x44, 0x55, 0x66]));
}

#[test]
fn bring_up_failure_is_fatal() {
    let mut h = Host::new();
    h.start(0);
    let _ = h.poll_frame();
    h.on_frame(complete(hci::ResetComplete { status: ErrorCode::HardwareFailure, num_hci_command_packets: 1 }), 0);
    assert!(matches!(h.poll_event(), Some(HostEvent::Fatal(_))));
}

#[test]
fn command_timeout_is_fatal() {
    let mut h = Host::new();
    h.start(0);
    h.tick(h.deadline().unwrap());
    assert!(matches!(h.poll_event(), Some(HostEvent::Fatal(_))));
}

#[test]
fn classic_connection_requests_go_to_the_caller_and_sco_is_refused() {
    let mut h = ready_host(3);
    let req = |link_type| {
        event(hci::ConnectionRequest { bd_addr: hci::Address::try_from(1u64).unwrap(), class_of_device: 0x002540, link_type })
    };
    h.on_frame(req(hci::ConnectionRequestLinkType::Acl), 0);
    assert!(sent_ops(&mut h).is_empty(), "nothing accepted on its own");
    let Some(HostEvent::ConnectionRequest { peer, class_of_device: 0x002540 }) = h.poll_event() else { panic!("no request event") };
    h.reject_connection(peer, 0);
    assert_eq!(sent_ops(&mut h), vec![OpCode::RejectConnectionRequest]);
    h.on_frame(vinculum_host::testing::status(OpCode::RejectConnectionRequest, ErrorCode::Success), 0);
    h.on_frame(req(hci::ConnectionRequestLinkType::Sco), 0);
    assert_eq!(sent_ops(&mut h), vec![OpCode::RejectConnectionRequest], "no SCO before HFP");
}

#[test]
fn connection_bound() {
    let mut h = ready_host(3);
    for i in 0..MAX_CONNECTIONS as u16 {
        h.on_frame(le_conn(i, 0xC000_0000_0000 + i as u64), 0);
        // each extra command needs its completion before the next goes
    }
    assert_eq!(h.connections(), MAX_CONNECTIONS);
    h.on_frame(le_conn(100, 0xC000_0000_00FF), 0);
    assert_eq!(h.connections(), MAX_CONNECTIONS);
    assert_eq!(sent_ops(&mut h), vec![OpCode::Disconnect]);
    assert_eq!(h.stats().dropped_connections, 1);
}

#[test]
fn acl_waits_for_credits_and_keeps_order() {
    let mut h = ready_host(1); // one LE buffer
    h.on_frame(le_conn(7, 0xC000_0000_0001), 0);
    while h.poll_event().is_some() {}
    // 60 bytes of payload at LE MTU 27 = 3 fragments (4-byte header + 60).
    h.send_l2cap(7, 4, &[0xAB; 60], 0).unwrap();
    let mut sent = Vec::new();
    while let Some(f) = h.poll_frame() {
        sent.push(f);
    }
    assert_eq!(sent.len(), 1, "one credit, one packet");
    for _ in 0..2 {
        h.on_frame(event(hci::NumberOfCompletedPackets { completed_packets: vec![hci::CompletedPackets { connection_handle: 7, host_num_of_completed_packets: 1 }] }), 0);
        sent.extend(std::iter::from_fn(|| h.poll_frame()));
    }
    assert_eq!(sent.len(), 3);
    let first = hci::Acl::decode_full(&sent[0].packet).unwrap();
    assert_eq!(first.packet_boundary_flag, hci::PacketBoundaryFlag::FirstNonAutomaticallyFlushable);
    let rest = hci::Acl::decode_full(&sent[2].packet).unwrap();
    assert_eq!(rest.packet_boundary_flag, hci::PacketBoundaryFlag::ContinuingFragment);
    // A lying controller returning 5 credits is clamped and counted.
    h.on_frame(event(hci::NumberOfCompletedPackets { completed_packets: vec![hci::CompletedPackets { connection_handle: 7, host_num_of_completed_packets: 5 }] }), 0);
    assert_eq!(h.stats().credit_violations, 1);
}

#[test]
fn l2cap_reassembled_and_oversize_peer_kicked() {
    let mut h = ready_host(3);
    h.on_frame(le_conn(9, 0xC000_0000_0002), 0);
    while h.poll_event().is_some() {}
    h.on_frame(acl(9, hci::PacketBoundaryFlag::FirstNonAutomaticallyFlushable, &[3, 0, 4, 0, 0x03]), 0);
    h.on_frame(acl(9, hci::PacketBoundaryFlag::ContinuingFragment, &[0x05, 0x02]), 0);
    assert_eq!(h.poll_event(), Some(HostEvent::L2cap { handle: 9, cid: 4, payload: vec![3, 5, 2] }));
    // header says 2 bytes, 5 arrive: fatal for the link
    h.on_frame(acl(9, hci::PacketBoundaryFlag::FirstNonAutomaticallyFlushable, &[2, 0, 4, 0, 1, 2, 3, 4, 5]), 0);
    assert_eq!(sent_ops(&mut h), vec![OpCode::Disconnect]);
    assert_eq!(h.stats().link_violations, 1);
}

#[test]
fn disconnect_returns_credits_and_forgets_queue() {
    let mut h = ready_host(1);
    h.on_frame(le_conn(3, 0xC000_0000_0003), 0);
    h.send_l2cap(3, 4, &[0; 60], 0).unwrap();
    while h.poll_frame().is_some() {}
    h.on_frame(
        event(hci::DisconnectionComplete { status: ErrorCode::Success, connection_handle: 3, reason: ErrorCode::RemoteUserTerminatedConnection }),
        0,
    );
    h.on_frame(le_conn(4, 0xC000_0000_0004), 0);
    h.send_l2cap(4, 4, &[1], 0).unwrap();
    assert!(h.poll_frame().is_some(), "credit came back with the disconnect");
}

/// A controller with no public address (BD_ADDR all zero), as Zephyr's
/// hci_usb on the nRF52840 dongle answers, reporting `manufacturer` and,
/// to Zephyr's Read Static Addresses, `static_addr`. Like that dongle it is
/// LE only (LMP feature bit 37) and refuses every BR/EDR command with
/// Unknown HCI Command.
fn bring_up_without_public_address(manufacturer: u16, static_addr: u64) -> (Host, Vec<hci::Command>) {
    let ok = ErrorCode::Success;
    let mut h = Host::new();
    h.start(0);
    let mut sent = Vec::new();
    for _ in 0..40 {
        let Some(cmd) = h.poll_frame() else { break };
        let c = hci::Command::decode_full(&cmd.packet).unwrap();
        sent.push(c.clone());
        let a = match c.op_code {
            OpCode::ReadLocalVersionInformation => complete(hci::ReadLocalVersionInformationComplete {
                status: ok,
                local_version_information: hci::LocalVersionInformation {
                    hci_version: hci::HciVersion::V54,
                    hci_revision: 0,
                    lmp_version: hci::LmpVersion::V54,
                    manufacturer_name: manufacturer,
                    lmp_subversion: 0xFFFF,
                },
                num_hci_command_packets: 1,
            }),
            OpCode::ReadBdAddr => complete(hci::ReadBdAddrComplete { status: ok, bd_addr: hci::Address::try_from(0u64).unwrap(), num_hci_command_packets: 1 }),
            OpCode::ZephyrReadStaticAddresses => complete(hci::ZephyrReadStaticAddressesComplete {
                status: ok,
                addresses: vec![hci::ZephyrStaticAddress { address: hci::Address::try_from(static_addr).unwrap(), identity_root: [7; 16] }],
                num_hci_command_packets: 1,
            }),
            OpCode::LeSetRandomAddress => complete(hci::LeSetRandomAddressComplete { status: ok, num_hci_command_packets: 1 }),
            OpCode::LeSetScanParameters => complete(hci::LeSetScanParametersComplete { status: ok, num_hci_command_packets: 1 }),
            OpCode::ReadLocalSupportedFeatures => complete(hci::ReadLocalSupportedFeaturesComplete { status: ok, lmp_features: 1 << 37, num_hci_command_packets: 1 }),
            OpCode::ReadBufferSize | OpCode::WriteSimplePairingMode | OpCode::WriteSecureConnectionsHostSupport => {
                status(c.op_code, ErrorCode::UnknownHciCommand)
            }
            _ => match vinculum_host::testing::answer(&cmd, 4) {
                Some(a) => a,
                None => continue,
            },
        };
        h.on_frame(a, 0);
    }
    (h, sent)
}

#[test]
fn zephyr_controller_without_public_address_uses_its_static_one() {
    let (mut h, sent) = bring_up_without_public_address(0x05F1, 0xC6_12_34_56_78_9A);
    assert!(h.is_ready());
    assert_eq!(h.info().address, Some([0xC6, 0x12, 0x34, 0x56, 0x78, 0x9A]));
    assert!(h.info().address_random);
    let set = sent.iter().find(|c| c.op_code == OpCode::LeSetRandomAddress).expect("LE Set Random Address sent");
    assert_eq!(set.payload, vec![0x9A, 0x78, 0x56, 0x34, 0x12, 0xC6]);
    // LE only: no BR/EDR command is sent; its LE buffers carry everything.
    assert!(!sent.iter().any(|c| matches!(c.op_code, OpCode::ReadBufferSize | OpCode::WriteSimplePairingMode)));
    assert!(h.info().le_buffers > 0);
    // Nor any inquiry when a user scans.
    h.inquiry(true, 0);
    assert!(h.poll_frame().is_none(), "no Classic inquiry on an LE-only controller");
    // Scans then say "random" for our own address.
    h.le_scan(true, 0);
    let f = h.poll_frame().unwrap();
    let c = hci::Command::decode_full(&f.packet).unwrap();
    let p = hci::LeSetScanParameters::try_from(c).unwrap();
    assert_eq!(p.own_address_type, hci::OwnAddressType::RandomDeviceAddress);
}

#[test]
fn no_address_and_no_way_to_get_one_is_fatal() {
    // Not Zephyr: nothing to ask.
    let (h, sent) = bring_up_without_public_address(0x005D, 0xC6_12_34_56_78_9A);
    assert!(!h.is_ready());
    assert!(!sent.iter().any(|c| c.op_code == OpCode::ZephyrReadStaticAddresses));
    // Zephyr, but the answer is not a static random address (top bits 01).
    let (h, _) = bring_up_without_public_address(0x05F1, 0x46_12_34_56_78_9A);
    assert!(!h.is_ready());
}

#[test]
fn public_address_controller_keeps_public_own_address() {
    let mut h = ready_host(4);
    assert!(!h.info().address_random);
    h.le_scan(true, 0);
    let c = hci::Command::decode_full(&h.poll_frame().unwrap().packet).unwrap();
    assert_eq!(hci::LeSetScanParameters::try_from(c).unwrap().own_address_type, hci::OwnAddressType::PublicDeviceAddress);
}

#[test]
fn bredr_secure_connections_comes_from_features_page_2() {
    // The canned controller lists page 2 bit 8.
    assert!(ready_host(4).info().bredr_secure_connections());
    // One with no page 2 (the command fails) still comes up, without it.
    let mut h = Host::new();
    h.start(0);
    for _ in 0..32 {
        let Some(cmd) = h.poll_frame() else { break };
        let c = hci::Command::decode_full(&cmd.packet).unwrap();
        let a = if c.op_code == OpCode::ReadLocalExtendedFeatures {
            complete(hci::ReadLocalExtendedFeaturesComplete {
                status: ErrorCode::UnknownHciCommand,
                page_number: 0,
                maximum_page_number: 0,
                extended_lmp_features: 0,
                num_hci_command_packets: 1,
            })
        } else {
            vinculum_host::testing::answer(&cmd, 4).expect("canned answer")
        };
        h.on_frame(a, 0);
    }
    assert!(h.is_ready());
    assert!(!h.info().bredr_secure_connections());
}
