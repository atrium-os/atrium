//! Host behaviour against a scripted controller.

use vinculum_hci_proto::h4::{Frame, Kind};
use vinculum_hci_proto::hci::{self, ErrorCode, OpCode};
use vinculum_hci_proto::Packet;
use vinculum_host::testing::{complete, event, le_meta, ready_host};
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
