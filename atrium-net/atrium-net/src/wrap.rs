//! atrium-net puts atrium-lease's DHCP payloads on the wire itself, after
//! checking them: a compromised lease process must not be able to send
//! arbitrary frames, only DHCP requests for this interface's MAC.

use std::net::{Ipv4Addr, Ipv6Addr};

pub const MAX_PAYLOAD: usize = 576;
const MAGIC: [u8; 4] = [99, 130, 83, 99];

#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    Size,
    NotRequest,
    Chaddr,
    Dest,
    Dmac,
}

/// Is this a BOOTREQUEST for `mac` that we are willing to send to `dest`?
/// `server` is the server of the lease currently applied, if any: the only
/// unicast destination allowed.
pub fn check_send(payload: &[u8], mac: &[u8; 6], dest: Option<Ipv4Addr>, dmac: &[u8; 6], server: Option<Ipv4Addr>) -> Result<(), Refused> {
    if payload.len() < 240 || payload.len() > MAX_PAYLOAD {
        return Err(Refused::Size);
    }
    if payload[0] != 1 || payload[1] != 1 || payload[2] != 6 || payload[236..240] != MAGIC {
        return Err(Refused::NotRequest);
    }
    if payload[28..34] != mac[..] {
        return Err(Refused::Chaddr);
    }
    match dest {
        None => {
            if *dmac != [0xff; 6] {
                return Err(Refused::Dmac);
            }
        }
        Some(d) => {
            if Some(d) != server {
                return Err(Refused::Dest);
            }
            if dmac[0] & 1 != 0 || *dmac == [0; 6] {
                return Err(Refused::Dmac);
            }
        }
    }
    Ok(())
}

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

/// Ethernet + IPv4 + UDP (68 -> 67) around a checked payload.
pub fn frame(src_mac: &[u8; 6], dst_mac: &[u8; 6], src: Ipv4Addr, dst: Ipv4Addr, payload: &[u8], id: u16) -> Vec<u8> {
    let ulen = 8 + payload.len();
    let total = 20 + ulen;
    let mut f = Vec::with_capacity(14 + total);
    f.extend_from_slice(dst_mac);
    f.extend_from_slice(src_mac);
    f.extend_from_slice(&[0x08, 0x00]);
    let mut ip = vec![0x45, 0x10];
    ip.extend_from_slice(&(total as u16).to_be_bytes());
    ip.extend_from_slice(&id.to_be_bytes());
    ip.extend_from_slice(&[0, 0, 64, 17, 0, 0]);
    ip.extend_from_slice(&src.octets());
    ip.extend_from_slice(&dst.octets());
    let c = csum(&ip, 0);
    ip[10..12].copy_from_slice(&c.to_be_bytes());
    f.extend_from_slice(&ip);
    let mut udp = Vec::with_capacity(ulen);
    udp.extend_from_slice(&68u16.to_be_bytes());
    udp.extend_from_slice(&67u16.to_be_bytes());
    udp.extend_from_slice(&(ulen as u16).to_be_bytes());
    udp.extend_from_slice(&[0, 0]);
    udp.extend_from_slice(payload);
    let mut pseudo = 0u32;
    for c in ip[12..20].chunks(2) {
        pseudo += u16::from_be_bytes([c[0], c[1]]) as u32;
    }
    pseudo += 17 + ulen as u32;
    let mut c = csum(&udp, pseudo);
    if c == 0 {
        c = 0xffff;
    }
    udp[6..8].copy_from_slice(&c.to_be_bytes());
    f.extend_from_slice(&udp);
    f
}

/// Is this a DHCPv6 client message for `mac` (DUID-LL of it) we are willing
/// to send? Only the message types and options atrium-lease builds.
pub fn check_send6(p: &[u8], mac: &[u8; 6]) -> Result<(), Refused> {
    if p.len() < 4 || p.len() > 256 {
        return Err(Refused::Size);
    }
    // Solicit, Request, Renew, Rebind, Information-Request.
    if !matches!(p[0], 1 | 3 | 5 | 6 | 11) {
        return Err(Refused::NotRequest);
    }
    let mut duid = [0u8, 3, 0, 1, 0, 0, 0, 0, 0, 0];
    duid[4..].copy_from_slice(mac);
    let (mut i, mut ids) = (4, 0);
    while i < p.len() {
        if i + 4 > p.len() {
            return Err(Refused::NotRequest);
        }
        let code = u16::from_be_bytes([p[i], p[i + 1]]);
        let len = u16::from_be_bytes([p[i + 2], p[i + 3]]) as usize;
        let Some(body) = p.get(i + 4..i + 4 + len) else { return Err(Refused::NotRequest) };
        match code {
            1 if body == duid => ids += 1,
            1 => return Err(Refused::Chaddr),
            // Server id, IA_NA, option request, elapsed time.
            2 | 3 | 6 | 8 => {}
            _ => return Err(Refused::NotRequest),
        }
        i += 4 + len;
    }
    if ids != 1 {
        return Err(Refused::Chaddr);
    }
    Ok(())
}

/// Ethernet + IPv6 + UDP (546 -> 547), from our link-local address to
/// All_DHCP_Relay_Agents_and_Servers (ff02::1:2), around a checked payload.
pub fn frame6(src_mac: &[u8; 6], src: Ipv6Addr, payload: &[u8]) -> Vec<u8> {
    let dst = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 1, 2);
    let ulen = 8 + payload.len();
    let mut f = vec![0x33, 0x33, 0, 1, 0, 2];
    f.extend_from_slice(src_mac);
    f.extend_from_slice(&[0x86, 0xdd, 0x60, 0, 0, 0]);
    f.extend_from_slice(&(ulen as u16).to_be_bytes());
    f.extend_from_slice(&[17, 1]);
    f.extend_from_slice(&src.octets());
    f.extend_from_slice(&dst.octets());
    let mut udp = Vec::with_capacity(ulen);
    udp.extend_from_slice(&546u16.to_be_bytes());
    udp.extend_from_slice(&547u16.to_be_bytes());
    udp.extend_from_slice(&(ulen as u16).to_be_bytes());
    udp.extend_from_slice(&[0, 0]);
    udp.extend_from_slice(payload);
    let mut pseudo = 0u32;
    for c in f[22..54].chunks(2) {
        pseudo += u16::from_be_bytes([c[0], c[1]]) as u32;
    }
    pseudo += 17 + ulen as u32;
    let mut c = csum(&udp, pseudo);
    if c == 0 {
        c = 0xffff;
    }
    udp[6..8].copy_from_slice(&c.to_be_bytes());
    f.extend_from_slice(&udp);
    f
}

/// An ARP probe (RFC 5227 §2.1.1) for `target`: sender address 0.0.0.0,
/// broadcast. Built here from the address alone.
pub fn arp_probe(src_mac: &[u8; 6], target: Ipv4Addr) -> Vec<u8> {
    let mut f = vec![0xff; 6];
    f.extend_from_slice(src_mac);
    f.extend_from_slice(&[0x08, 0x06, 0, 1, 8, 0, 6, 4, 0, 1]);
    f.extend_from_slice(src_mac);
    f.extend_from_slice(&[0; 4 + 6]);
    f.extend_from_slice(&target.octets());
    f
}

/// A router solicitation from the unspecified address (RFC 4861 §6.3.7:
/// no source link-layer option then), so routers answer at once instead of
/// at their next periodic RA. Built here; nothing in it comes from the lease
/// process.
pub fn rs_frame(src_mac: &[u8; 6]) -> Vec<u8> {
    let dst = [0xff02u16, 0, 0, 0, 0, 0, 0, 2];
    let mut f = vec![0x33, 0x33, 0, 0, 0, 2];
    f.extend_from_slice(src_mac);
    f.extend_from_slice(&[0x86, 0xdd, 0x60, 0, 0, 0, 0, 8, 58, 255]);
    f.extend_from_slice(&[0; 16]);
    for w in dst {
        f.extend_from_slice(&w.to_be_bytes());
    }
    let mut icmp = [133u8, 0, 0, 0, 0, 0, 0, 0];
    let mut s: u32 = dst.iter().map(|&w| w as u32).sum::<u32>() + 8 + 58 + (133 << 8);
    while s > 0xffff {
        s = (s & 0xffff) + (s >> 16);
    }
    icmp[2..4].copy_from_slice(&(!(s as u16)).to_be_bytes());
    f.extend_from_slice(&icmp);
    f
}

#[cfg(test)]
mod rs_tests {
    #[test]
    fn rs_checksum() {
        let f = super::rs_frame(&[2, 0, 0, 0, 0, 1]);
        assert_eq!(f.len(), 14 + 40 + 8);
        // Sum over the pseudo-header and message comes out as all ones.
        let mut s: u32 = 0;
        for c in f[22..54].chunks(2) {
            s += u16::from_be_bytes([c[0], c[1]]) as u32;
        }
        s += 8 + 58;
        for c in f[54..].chunks(2) {
            s += u16::from_be_bytes([c[0], c[1]]) as u32;
        }
        while s > 0xffff {
            s = (s & 0xffff) + (s >> 16);
        }
        assert_eq!(s, 0xffff);
    }
}

/// DHCP replies (IPv4, UDP, not a fragment, 67 -> 68), IPv6 router
/// advertisements (ICMPv6 134, hop limit 255, no extension headers),
/// DHCPv6 replies (UDP 547 -> 546, no extension headers) and ARP (for the
/// lease parser's conflict check before a new address is used, RFC 5227).
pub const RX_FILTER: [(u16, u8, u8, u32); 27] = [
    (0x28, 0, 0, 12),       // 0  ldh [12]
    (0x15, 0, 9, 0x0800),   // 1  jeq IPv4 ? 2 : 11
    (0x30, 0, 0, 23),       // 2  ldb [23]
    (0x15, 0, 22, 17),      // 3  jeq UDP ? 4 : drop
    (0x28, 0, 0, 20),       // 4  ldh [20]
    (0x45, 20, 0, 0x1fff),  // 5  fragment ? drop : 6
    (0xb1, 0, 0, 14),       // 6  ldx 4*([14]&0xf)
    (0x48, 0, 0, 14),       // 7  ldh [x+14]
    (0x15, 0, 17, 67),      // 8  jeq 67 ? 9 : drop
    (0x48, 0, 0, 16),       // 9  ldh [x+16]
    (0x15, 14, 15, 68),     // 10 jeq 68 ? take : drop
    (0x15, 0, 11, 0x86dd),  // 11 jeq IPv6 ? 12 : 23
    (0x30, 0, 0, 20),       // 12 ldb [20] next header
    (0x15, 0, 4, 58),       // 13 jeq ICMPv6 ? 14 : 18
    (0x30, 0, 0, 21),       // 14 ldb [21] hop limit
    (0x15, 0, 10, 255),     // 15 jeq 255 ? 16 : drop
    (0x30, 0, 0, 54),       // 16 ldb [54] ICMPv6 type
    (0x15, 7, 8, 134),      // 17 jeq RA ? take : drop
    (0x15, 0, 7, 17),       // 18 jeq UDP ? 19 : drop
    (0x28, 0, 0, 54),       // 19 ldh [54] source port
    (0x15, 0, 5, 547),      // 20 jeq 547 ? 21 : drop
    (0x28, 0, 0, 56),       // 21 ldh [56] destination port
    (0x15, 2, 3, 546),      // 22 jeq 546 ? take : drop
    (0x15, 1, 0, 0x0806),   // 23 jeq ARP ? take : 24
    (0x15, 0, 1, 0x888e),   // 24 jeq EAPOL (wired 802.1X) ? take : drop
    (0x06, 0, 0, 1514),     // 25 take
    (0x06, 0, 0, 0),        // 26 drop
];

#[cfg(test)]
mod filter_tests {
    use super::RX_FILTER;
    use atrium_lease::{dhcp6, frame, ra};
    use std::net::{Ipv4Addr, Ipv6Addr};

    /// Just the instructions RX_FILTER uses.
    fn run(f: &[u8]) -> u32 {
        let (mut a, mut x, mut pc) = (0u32, 0u32, 0usize);
        let ld = |at: usize, n: usize| -> Option<u32> { f.get(at..at + n).map(|b| b.iter().fold(0u32, |v, &c| v << 8 | c as u32)) };
        loop {
            let (code, jt, jf, k) = RX_FILTER[pc];
            pc += 1;
            match code {
                0x28 => match ld(k as usize, 2) { Some(v) => a = v, None => return 0 },
                0x30 => match ld(k as usize, 1) { Some(v) => a = v, None => return 0 },
                0x48 => match ld(x as usize + k as usize, 2) { Some(v) => a = v, None => return 0 },
                0xb1 => match ld(k as usize, 1) { Some(v) => x = 4 * (v & 0xf), None => return 0 },
                0x15 => pc += if a == k { jt } else { jf } as usize,
                0x45 => pc += if a & k != 0 { jt } else { jf } as usize,
                0x06 => return k,
                _ => panic!("opcode {code:#x}"),
            }
        }
    }

    #[test]
    fn passes_dhcp_ra_dhcpv6_arp_eapol_only() {
        let ll = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
        let me = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 2);
        assert!(run(&frame::frame_for_test(Ipv4Addr::new(10, 0, 0, 1), Ipv4Addr::BROADCAST, &[0; 300])) > 0);
        assert!(run(&ra::ra_for_test(ll, 1800, &[])) > 0);
        let d6 = dhcp6::frame_for_test(ll, me, &[7, 1, 2, 3]);
        assert!(run(&d6) > 0);
        // DHCPv6 with other ports, or a Solicit (546 -> 547) seen back.
        let mut g = d6.clone();
        g[54..58].copy_from_slice(&[0x02, 0x22, 0x02, 0x23]);
        assert_eq!(run(&g), 0);
        g[54..58].copy_from_slice(&[0x02, 0x23, 0x00, 0x35]);
        assert_eq!(run(&g), 0);
        // An RA with hop limit 64, a neighbor solicitation, an ARP frame.
        let mut r = ra::ra_for_test(ll, 1800, &[]);
        r[21] = 64;
        assert_eq!(run(&r), 0);
        let mut r = ra::ra_for_test(ll, 1800, &[]);
        r[54] = 135;
        assert_eq!(run(&r), 0);
        // ARP passes (the lease parser's conflict check), and EAPOL (wired
        // 802.1X, relayed to aura-keys); other types do not.
        let mut arp = vec![0xff; 12];
        arp.extend_from_slice(&[0x08, 0x06]);
        arp.extend_from_slice(&[0; 28]);
        assert!(run(&arp) > 0);
        arp[12..14].copy_from_slice(&[0x88, 0x8e]);
        assert!(run(&arp) > 0);
        arp[12..14].copy_from_slice(&[0x88, 0xcc]);
        assert_eq!(run(&arp), 0);
        let p = super::arp_probe(&[2, 0, 0, 0, 0, 1], Ipv4Addr::new(10, 0, 0, 5));
        assert!(run(&p) > 0);
        assert_eq!(frame::arp_from_frame(&p).map(|a| (a.op, a.spa, a.tpa)), Some((1, Ipv4Addr::UNSPECIFIED, Ipv4Addr::new(10, 0, 0, 5))));
    }

    #[test]
    fn send6_checked_and_framed() {
        let mac = [2, 0xaa, 0xbb, 0xcc, 0xdd, 0xee];
        let duid = dhcp6::duid_ll(&mac);
        let p = dhcp6::build(dhcp6::SOLICIT, [1, 2, 3], &duid, None, Some((7, &[])), 0);
        assert_eq!(super::check_send6(&p, &mac), Ok(()));
        let p2 = dhcp6::build(dhcp6::RENEW, [1, 2, 3], &duid, Some(&[0, 3, 0, 1, 1, 1, 1, 1, 1, 1]), Some((7, &[Ipv6Addr::LOCALHOST])), 9);
        assert_eq!(super::check_send6(&p2, &mac), Ok(()));
        // Another client's DUID, a server's message type, a relay option, cut short.
        assert!(super::check_send6(&dhcp6::build(1, [0; 3], &dhcp6::duid_ll(&[2, 0, 0, 0, 0, 9]), None, None, 0), &mac).is_err());
        let mut q = p.clone();
        q[0] = dhcp6::REPLY;
        assert!(super::check_send6(&q, &mac).is_err());
        let mut q = p.clone();
        q.extend_from_slice(&[0, 9, 0, 0]);
        assert!(super::check_send6(&q, &mac).is_err());
        assert!(super::check_send6(&p[..p.len() - 1], &mac).is_err());
        let ll = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 2);
        let f = super::frame6(&mac, ll, &p);
        assert_eq!(&f[..6], &[0x33, 0x33, 0, 1, 0, 2]);
        assert_eq!(&f[54..58], &[0x02, 0x22, 0x02, 0x23]);
        assert_eq!(&f[62..], &p[..]);
        let mut s: u32 = 17 + (f.len() - 54) as u32;
        for c in f[22..].chunks(2) {
            s += u16::from_be_bytes([c[0], *c.get(1).unwrap_or(&0)]) as u32;
        }
        while s > 0xffff {
            s = (s & 0xffff) + (s >> 16);
        }
        assert_eq!(s, 0xffff);
    }
}
