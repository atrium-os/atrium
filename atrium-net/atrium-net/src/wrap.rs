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
