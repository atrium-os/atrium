//! L2CAP signalling and dynamic channels (BR/EDR) against a scripted peer.

use vinculum_hci_proto::h4::{Frame, Kind};
use vinculum_hci_proto::hci::{self, ErrorCode};
use vinculum_hci_proto::Packet;
use vinculum_host::testing::{event, ready_host};
use vinculum_host::{Host, HostEvent};

const H: u16 = 7;

fn classic_host() -> Host {
    let mut h = ready_host(4);
    h.on_frame(
        event(hci::ConnectionComplete {
            status: ErrorCode::Success,
            connection_handle: H,
            bd_addr: hci::Address::try_from(0xA1A2_A3A4_A5A6u64).unwrap(),
            link_type: hci::LinkType::Acl,
            encryption_enabled: hci::Enable::Disabled,
        }),
        0,
    );
    while h.poll_event().is_some() {}
    while h.poll_frame().is_some() {}
    h
}

fn l2(cid: u16, payload: &[u8]) -> Frame {
    let mut p = (payload.len() as u16).to_le_bytes().to_vec();
    p.extend_from_slice(&cid.to_le_bytes());
    p.extend_from_slice(payload);
    let a = hci::Acl {
        handle: H,
        packet_boundary_flag: hci::PacketBoundaryFlag::FirstAutomaticallyFlushable,
        broadcast_flag: hci::BroadcastFlag::PointToPoint,
        payload: p,
    };
    Frame { kind: Kind::Acl, packet: a.encode_to_vec().unwrap() }
}

fn sig(code: u8, ident: u8, data: &[u8]) -> Frame {
    let mut c = vec![code, ident];
    c.extend_from_slice(&(data.len() as u16).to_le_bytes());
    c.extend_from_slice(data);
    l2(1, &c)
}

/// Signalling commands we sent: (code, ident, data). The controller
/// completes every ACL packet, so credits never run out.
fn sent(h: &mut Host) -> Vec<(u8, u8, Vec<u8>)> {
    let mut v = Vec::new();
    while let Some(f) = h.poll_frame() {
        if f.kind != Kind::Acl {
            continue;
        }
        h.on_frame(
            event(hci::NumberOfCompletedPackets {
                completed_packets: vec![hci::CompletedPackets { connection_handle: H, host_num_of_completed_packets: 1 }],
            }),
            0,
        );
        let a = hci::Acl::decode_full(&f.packet).unwrap();
        let p = a.payload;
        if u16::from_le_bytes([p[2], p[3]]) == 1 {
            let n = u16::from_le_bytes([p[6], p[7]]) as usize;
            v.push((p[4], p[5], p[8..8 + n].to_vec()));
        }
    }
    v
}

fn events(h: &mut Host) -> Vec<HostEvent> {
    std::iter::from_fn(|| h.poll_event()).collect()
}

fn le(v: &[u16]) -> Vec<u8> {
    v.iter().flat_map(|x| x.to_le_bytes()).collect()
}

/// Makes the link "encrypted" via a stored-key reconnect script.
fn encrypt(h: &mut Host) {
    use vinculum_host::testing::{complete, status};
    use hci::OpCode;
    h.encrypt(H, [1; 16], 0).unwrap();
    h.on_frame(status(OpCode::AuthenticationRequested, ErrorCode::Success), 0);
    h.on_frame(event(hci::AuthenticationComplete { status: ErrorCode::Success, connection_handle: H }), 0);
    h.on_frame(status(OpCode::SetConnectionEncryption, ErrorCode::Success), 0);
    h.on_frame(
        event(hci::EncryptionChange { status: ErrorCode::Success, connection_handle: H, encryption_enabled: hci::EncryptionEnabled::BrEdrAesCcm }),
        0,
    );
    h.on_frame(complete(hci::ReadEncryptionKeySizeComplete { status: ErrorCode::Success, connection_handle: H, key_size: 16, num_hci_command_packets: 1 }), 0);
    assert!(h.is_encrypted(H));
    events(h);
    while h.poll_frame().is_some() {}
}

#[test]
fn incoming_channels_need_a_registered_psm_and_an_encrypted_link() {
    let mut h = classic_host();
    h.on_frame(sig(0x02, 1, &le(&[0x0013, 0x0041])), 0);
    let r = sent(&mut h);
    assert_eq!(r[0].0, 0x03);
    assert_eq!(&r[0].2[4..6], &2u16.to_le_bytes(), "PSM not supported");
    h.register_psm(0x0013);
    h.on_frame(sig(0x02, 2, &le(&[0x0013, 0x0041])), 0);
    assert_eq!(&sent(&mut h)[0].2[4..6], &3u16.to_le_bytes(), "security block: not encrypted");

    encrypt(&mut h);
    h.on_frame(sig(0x02, 3, &le(&[0x0013, 0x0041])), 0);
    let r = sent(&mut h);
    assert_eq!(r[0].0, 0x03);
    assert_eq!(&r[0].2[4..6], &0u16.to_le_bytes(), "success");
    let local = u16::from_le_bytes([r[0].2[0], r[0].2[1]]);
    assert_eq!(r[1].0, 0x04, "our Configuration Request follows");
    let our_ident = r[1].1;
    // Peer configures us (MTU 100), then accepts ours.
    let mut conf = le(&[local, 0]);
    conf.extend_from_slice(&[0x01, 2, 100, 0]);
    h.on_frame(sig(0x04, 9, &conf), 0);
    let r = sent(&mut h);
    assert_eq!((r[0].0, &r[0].2[4..6]), (0x05, &0u16.to_le_bytes()[..]));
    h.on_frame(sig(0x05, our_ident, &le(&[local, 0, 0])), 0);
    assert_eq!(events(&mut h), vec![HostEvent::ChannelOpen { handle: H, cid: local, psm: 0x13, peer_mtu: 100 }]);
    h.on_frame(l2(local, &[0xA1, 1, 2]), 0);
    assert_eq!(events(&mut h), vec![HostEvent::ChannelData { handle: H, cid: local, payload: vec![0xA1, 1, 2] }]);
    assert!(h.l2cap_send(H, local, &[0; 101], 0).is_err(), "over the peer's MTU");
    h.l2cap_send(H, local, &[0; 100], 0).unwrap();
    // Peer closes.
    h.on_frame(sig(0x06, 10, &le(&[local, 0x0041])), 0);
    assert_eq!(sent(&mut h)[0].0, 0x07);
    assert_eq!(events(&mut h), vec![HostEvent::ChannelClosed { handle: H, cid: local }]);
}

#[test]
fn outgoing_channel_and_refusals() {
    let mut h = classic_host();
    let local = h.l2cap_connect(H, 0x0001, 0).unwrap();
    let r = sent(&mut h);
    assert_eq!(r[0].0, 0x02);
    let ident = r[0].1;
    // A response with the wrong ident is ignored (and counted).
    h.on_frame(sig(0x03, ident.wrapping_add(1), &le(&[0x0050, local, 0, 0])), 0);
    assert!(sent(&mut h).is_empty());
    h.on_frame(sig(0x03, ident, &le(&[0x0050, local, 0, 0])), 0);
    let r = sent(&mut h);
    assert_eq!(r[0].0, 0x04);
    let ci = r[0].1;
    h.on_frame(sig(0x05, ci, &le(&[local, 0, 0])), 0);
    h.on_frame(sig(0x04, 4, &le(&[local, 0])), 0); // no options: MTU 672
    assert!(matches!(events(&mut h).as_slice(), [HostEvent::ChannelOpen { psm: 1, peer_mtu: 672, .. }]));
    assert_eq!(sent(&mut h)[0].0, 0x05, "we accepted their configuration");

    // Refused connect.
    let l2c = h.l2cap_connect(H, 0x0011, 0).unwrap();
    let ident = sent(&mut h)[0].1;
    h.on_frame(sig(0x03, ident, &le(&[0, l2c, 3, 0])), 0);
    let ev = events(&mut h);
    assert!(matches!(ev.as_slice(), [HostEvent::ChannelFailed { psm: 0x11, .. }]), "{ev:?}");

    // A non-basic mode is unacceptable.
    let l3 = h.l2cap_connect(H, 0x0013, 0).unwrap();
    let ident = sent(&mut h)[0].1;
    h.on_frame(sig(0x03, ident, &le(&[0x0060, l3, 0, 0])), 0);
    sent(&mut h);
    let mut conf = le(&[l3, 0]);
    conf.extend_from_slice(&[0x04, 9, 3, 0, 0, 0, 0, 0, 0, 0, 0]); // ERTM
    h.on_frame(sig(0x04, 5, &conf), 0);
    let r = sent(&mut h);
    assert_eq!(&r[0].2[4..6], &1u16.to_le_bytes(), "unacceptable");
}

#[test]
fn malformed_signalling_and_timeouts() {
    let mut h = classic_host();
    h.on_frame(sig(0x42, 1, &[]), 0); // unknown code
    assert_eq!(sent(&mut h)[0].0, 0x01, "command reject");
    h.on_frame(l2(1, &[0x02, 1, 9, 0, 1]), 0); // length past the end
    h.on_frame(l2(0x0077, &[1, 2, 3]), 0); // data for no channel
    assert_eq!(h.stats().link_violations, 3);
    // Echo and information requests are answered.
    h.on_frame(sig(0x08, 2, &[1, 2]), 0);
    h.on_frame(sig(0x0A, 3, &le(&[2])), 0);
    let r = sent(&mut h);
    assert_eq!((r[0].0, r[1].0), (0x09, 0x0B));

    let local = h.l2cap_connect(H, 1, 100).unwrap();
    sent(&mut h);
    let d = h.deadline().unwrap();
    h.tick(d);
    assert!(matches!(events(&mut h).as_slice(), [HostEvent::ChannelFailed { psm: 1, .. }]));
    assert!(h.l2cap_send(H, local, &[1], d).is_err());
}
