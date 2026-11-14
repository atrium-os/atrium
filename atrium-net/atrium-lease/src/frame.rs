//! Ethernet + IPv4 + UDP around DHCP. atrium-net's bpf filter passes only
//! UDP to port 68 on its interface; this side still checks everything,
//! because the filter is not a parser and the frame is attacker-chosen.

use crate::dhcp::{CLIENT_PORT, SERVER_PORT};
use std::net::Ipv4Addr;

fn csum(b: &[u8], init: u32) -> u16 {
    let mut s = init;
    for c in b.chunks(2) {
        s += u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]) as u32;
    }
    while s > 0xffff {
        s = (s & 0xffff) + (s >> 16);
    }
    !(s as u16)
}

/// (source IPv4, DHCP payload) of a frame from a DHCP server, or `None`.
pub fn dhcp_from_frame(f: &[u8]) -> Option<(Ipv4Addr, &[u8])> {
    if f.len() < 14 + 20 + 8 || f[12..14] != [0x08, 0x00] {
        return None;
    }
    let ip = &f[14..];
    let ihl = ((ip[0] & 0x0f) as usize) * 4;
    if ip[0] >> 4 != 4 || ihl < 20 || ip.len() < ihl + 8 {
        return None;
    }
    let total = u16::from_be_bytes([ip[2], ip[3]]) as usize;
    if total < ihl + 8 || total > ip.len() {
        return None;
    }
    // No fragments: MF clear and offset zero.
    if u16::from_be_bytes([ip[6], ip[7]]) & 0x3fff != 0 || ip[9] != 17 {
        return None;
    }
    if csum(&ip[..ihl], 0) != 0 {
        return None;
    }
    let src = Ipv4Addr::new(ip[12], ip[13], ip[14], ip[15]);
    let udp = &ip[ihl..total];
    let sport = u16::from_be_bytes([udp[0], udp[1]]);
    let dport = u16::from_be_bytes([udp[2], udp[3]]);
    let ulen = u16::from_be_bytes([udp[4], udp[5]]) as usize;
    if sport != SERVER_PORT || dport != CLIENT_PORT || ulen < 8 || ulen > udp.len() {
        return None;
    }
    let udp = &udp[..ulen];
    if u16::from_be_bytes([udp[6], udp[7]]) != 0 {
        let mut pseudo = 0u32;
        for c in ip[12..20].chunks(2) {
            pseudo += u16::from_be_bytes([c[0], c[1]]) as u32;
        }
        pseudo += 17 + ulen as u32;
        if csum(udp, pseudo) != 0 {
            return None;
        }
    }
    Some((src, &udp[8..]))
}

/// Build a frame for tests (and the hostile-server fuzzer).
pub fn frame_for_test(src: Ipv4Addr, dst: Ipv4Addr, payload: &[u8]) -> Vec<u8> {
    let mut f = vec![0xffu8; 6];
    f.extend_from_slice(&[2, 0, 0, 0, 0, 1, 0x08, 0x00]);
    let ulen = 8 + payload.len();
    let total = 20 + ulen;
    let mut ip = vec![0x45, 0, (total >> 8) as u8, total as u8, 0, 0, 0, 0, 64, 17, 0, 0];
    ip.extend_from_slice(&src.octets());
    ip.extend_from_slice(&dst.octets());
    let c = csum(&ip, 0);
    ip[10..12].copy_from_slice(&c.to_be_bytes());
    f.extend_from_slice(&ip);
    f.extend_from_slice(&SERVER_PORT.to_be_bytes());
    f.extend_from_slice(&CLIENT_PORT.to_be_bytes());
    f.extend_from_slice(&(ulen as u16).to_be_bytes());
    f.extend_from_slice(&[0, 0]); // no UDP checksum
    f.extend_from_slice(payload);
    f
}
