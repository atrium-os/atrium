//! IPv6 router advertisements (RFC 4861 §4.2), read only for their DNS
//! servers (RDNSS, RFC 8106). The kernel does SLAAC and default routes from
//! the same RAs (docs/spec/atrium-net.md §5.3); this side turns an
//! attacker-chosen frame into at most three server addresses with lifetimes,
//! which atrium-net checks again before unbound sees them.
//!
//! DNSSL (search domains) is not read: nothing here uses search domains, and
//! names from the network are a parser we do not need.

use std::net::Ipv6Addr;

pub const ICMP6: u8 = 58;
pub const RA: u8 = 134;
const OPT_RDNSS: u8 = 25;
/// RFC 8106 §5.1 allows any number; more than this is not a real network.
pub const MAX_RDNSS: usize = 3;
/// Lifetimes are capped (RFC 8106 allows "infinity"): the router says again.
pub const MAX_LIFE_S: u32 = 3 * 3600;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ra {
    pub router: Ipv6Addr,
    /// Router lifetime, seconds (0: not a default router).
    pub router_life: u16,
    /// (server, lifetime s); lifetime 0 withdraws the server.
    pub dns: Vec<(Ipv6Addr, u32)>,
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

fn addr(b: &[u8]) -> Ipv6Addr {
    let a: [u8; 16] = b.try_into().unwrap();
    Ipv6Addr::from(a)
}

/// An RA in an Ethernet frame, validated as RFC 4861 §6.1.2 asks (hop limit
/// 255, link-local source, code 0, checksum, well-formed options), or `None`.
/// Extension headers are refused: an RA has no reason to carry any.
pub fn ra_from_frame(f: &[u8]) -> Option<Ra> {
    if f.len() < 14 + 40 + 16 || f[12..14] != [0x86, 0xdd] {
        return None;
    }
    let ip = &f[14..];
    if ip[0] >> 4 != 6 || ip[6] != ICMP6 || ip[7] != 255 {
        return None;
    }
    let plen = u16::from_be_bytes([ip[4], ip[5]]) as usize;
    if plen < 16 || 40 + plen > ip.len() {
        return None;
    }
    let src = addr(&ip[8..24]);
    let dst = addr(&ip[24..40]);
    if src.segments()[0] & 0xffc0 != 0xfe80 {
        return None;
    }
    // To all nodes, or to us (a reply to our solicitation may be unicast).
    if !(dst == Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1) || dst.segments()[0] & 0xffc0 == 0xfe80) {
        return None;
    }
    let m = &ip[40..40 + plen];
    if m[0] != RA || m[1] != 0 {
        return None;
    }
    let mut pseudo = 0u32;
    for c in ip[8..40].chunks(2) {
        pseudo += u16::from_be_bytes([c[0], c[1]]) as u32;
    }
    pseudo += plen as u32 + ICMP6 as u32;
    if csum(m, pseudo) != 0 {
        return None;
    }
    let router_life = u16::from_be_bytes([m[6], m[7]]);
    let mut dns = Vec::new();
    let mut i = 16;
    while i < m.len() {
        let typ = m[i];
        let len = *m.get(i + 1)? as usize * 8;
        if len == 0 || i + len > m.len() {
            return None;
        }
        let o = &m[i..i + len];
        if typ == OPT_RDNSS {
            // Length 1 + 2n (units of 8 octets), n >= 1.
            if len < 24 || (len - 8) % 16 != 0 {
                return None;
            }
            let life = u32::from_be_bytes(o[4..8].try_into().ok()?).min(MAX_LIFE_S);
            for a in o[8..].chunks_exact(16) {
                if dns.len() == MAX_RDNSS * 4 {
                    return None;
                }
                dns.push((addr(a), life));
            }
        }
        i += len;
    }
    Some(Ra { router: src, router_life, dns })
}

/// DNS servers learned from RAs, per router, with expiry.
#[derive(Default)]
pub struct Rdnss {
    /// (router, server, expires at ms)
    have: Vec<(Ipv6Addr, Ipv6Addr, u64)>,
    told: Vec<Ipv6Addr>,
}

impl Rdnss {
    /// Take an RA in; `Some(servers)` when the usable set changed.
    pub fn rx(&mut self, ra: &Ra, now: u64) -> Option<Vec<Ipv6Addr>> {
        // A router's RDNSS list replaces what it said before; an RA without
        // RDNSS says nothing about DNS (RFC 8106 §5.1 keeps old entries).
        if !ra.dns.is_empty() {
            self.have.retain(|(r, _, _)| *r != ra.router);
            // Unusable servers are dropped here, so one bad entry from a
            // rogue router cannot get the whole list refused by atrium-net.
            for &(s, life) in &ra.dns {
                if life > 0 && net_proto::ordinary_unicast6(s) && !self.have.iter().any(|(_, x, _)| *x == s) {
                    self.have.push((ra.router, s, now + life as u64 * 1000));
                }
            }
            self.have.truncate(MAX_RDNSS * 2);
        }
        self.tick(now)
    }

    pub fn tick(&mut self, now: u64) -> Option<Vec<Ipv6Addr>> {
        self.have.retain(|(_, _, t)| *t > now);
        let cur: Vec<Ipv6Addr> = self.have.iter().map(|(_, s, _)| *s).take(MAX_RDNSS).collect();
        (cur != self.told).then(|| {
            self.told = cur.clone();
            cur
        })
    }

    pub fn deadline(&self) -> Option<u64> {
        self.have.iter().map(|(_, _, t)| *t).min()
    }

    /// Off the network: forget everything (the next network says its own).
    pub fn clear(&mut self) -> Option<Vec<Ipv6Addr>> {
        self.have.clear();
        self.tick(0)
    }
}

/// Build an RA frame (tests, fuzzing).
pub fn ra_for_test(src: Ipv6Addr, router_life: u16, opts: &[u8]) -> Vec<u8> {
    let mut m = vec![RA, 0, 0, 0, 64, 0];
    m.extend_from_slice(&router_life.to_be_bytes());
    m.extend_from_slice(&[0; 8]);
    m.extend_from_slice(opts);
    let dst = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 1);
    let mut f = vec![0x33, 0x33, 0, 0, 0, 1, 2, 0, 0, 0, 0, 0xfe, 0x86, 0xdd];
    f.extend_from_slice(&[0x60, 0, 0, 0]);
    f.extend_from_slice(&(m.len() as u16).to_be_bytes());
    f.extend_from_slice(&[ICMP6, 255]);
    f.extend_from_slice(&src.octets());
    f.extend_from_slice(&dst.octets());
    let mut pseudo = 0u32;
    for c in f[22..54].chunks(2) {
        pseudo += u16::from_be_bytes([c[0], c[1]]) as u32;
    }
    pseudo += m.len() as u32 + ICMP6 as u32;
    let c = csum(&m, pseudo);
    m[2..4].copy_from_slice(&c.to_be_bytes());
    f.extend_from_slice(&m);
    f
}

/// An RDNSS option (tests, fuzzing).
pub fn rdnss_opt(life: u32, servers: &[Ipv6Addr]) -> Vec<u8> {
    let mut o = vec![OPT_RDNSS, (1 + 2 * servers.len()) as u8, 0, 0];
    o.extend_from_slice(&life.to_be_bytes());
    for s in servers {
        o.extend_from_slice(&s.octets());
    }
    o
}
