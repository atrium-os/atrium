//! atrium-net puts atrium-lease's DHCP payloads on the wire itself, after
//! checking them: a compromised lease process must not be able to send
//! arbitrary frames, only DHCP requests for this interface's MAC.

use std::net::Ipv4Addr;

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
