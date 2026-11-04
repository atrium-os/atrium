//! Which addresses a fetch may connect to (threat model §2.1, attacker A4).
//!
//! ★ DEFAULT-DENY, CHECKED AFTER RESOLUTION. A name is only a claim about an
//! address; the address is what the socket reaches. So the check runs on what
//! the resolver ANSWERED, immediately before the connect, and a literal-IP URL
//! goes through the same function. IPv6 is an allowlist: only global unicast
//! (2000::/3) is admitted at all, and the carve-outs inside it are refused.
//! Every IPv6 form that EMBEDS an IPv4 address (IPv4-mapped, NAT64, 6to4,
//! IPv4-compatible) is judged by the IPv4 address it reaches, because that is
//! the address the packets go to.
//!
//! The private-network grant (`Policy::grant_private_network`) admits the
//! ranges a LAN or the local machine actually uses. It never admits the cloud
//! metadata endpoints, the unspecified address, multicast, broadcast, or the
//! documentation/benchmark/reserved ranges: none of those is ever a document,
//! and the metadata endpoint is the single most valuable thing A4 can reach.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Why an address is not public. `Display` names the range, so a refusal says
/// what was refused and why rather than just "refused".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    // ---- admitted by the private-network grant ----
    Loopback,
    /// RFC 1918: 10/8, 172.16/12, 192.168/16.
    Private,
    /// 169.254/16, fe80::/10.
    LinkLocal,
    /// RFC 6598 carrier-grade NAT, 100.64/10.
    SharedCgnat,
    /// fc00::/7.
    UniqueLocal,
    // ---- never admitted ----
    /// 169.254.169.254 and fd00:ec2::254: cloud instance metadata.
    Metadata,
    /// 0.0.0.0/8, ::.
    Unspecified,
    /// 224/4, ff00::/8.
    Multicast,
    /// 255.255.255.255.
    Broadcast,
    /// 192.0.2/24, 198.51.100/24, 203.0.113/24, 2001:db8::/32, 3fff::/20.
    Documentation,
    /// 198.18/15.
    Benchmarking,
    /// 192.0.0/24, 192.88.99/24, 240/4, 100::/64, 2001::/23, 64:ff9b:1::/48,
    /// ::/96, fec0::/10, and everything outside 2000::/3.
    Reserved,
}

impl Range {
    /// Ranges a private-network grant admits.
    pub fn is_private_network(self) -> bool {
        matches!(self, Range::Loopback | Range::Private | Range::LinkLocal
                     | Range::SharedCgnat | Range::UniqueLocal)
    }
}

impl std::fmt::Display for Range {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Range::Loopback => "loopback",
            Range::Private => "private (RFC 1918)",
            Range::LinkLocal => "link-local",
            Range::SharedCgnat => "carrier-grade NAT (RFC 6598)",
            Range::UniqueLocal => "unique local (fc00::/7)",
            Range::Metadata => "cloud metadata endpoint",
            Range::Unspecified => "unspecified",
            Range::Multicast => "multicast",
            Range::Broadcast => "broadcast",
            Range::Documentation => "documentation",
            Range::Benchmarking => "benchmarking (RFC 2544)",
            Range::Reserved => "reserved / special-purpose",
        })
    }
}

/// `None` for a public address; otherwise the range it falls in.
pub fn classify(ip: IpAddr) -> Option<Range> {
    match ip {
        IpAddr::V4(v4) => classify_v4(v4),
        IpAddr::V6(v6) => classify_v6(v6),
    }
}

fn in_v4(ip: u32, net: [u8; 4], bits: u32) -> bool {
    let net = u32::from_be_bytes(net);
    let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
    ip & mask == net & mask
}

pub fn classify_v4(ip: Ipv4Addr) -> Option<Range> {
    let a = u32::from(ip);
    // Most specific first: the metadata host sits inside link-local, the
    // broadcast address inside 240/4.
    if ip == Ipv4Addr::new(169, 254, 169, 254) { return Some(Range::Metadata) }
    if ip == Ipv4Addr::BROADCAST { return Some(Range::Broadcast) }
    let table: &[([u8; 4], u32, Range)] = &[
        ([0, 0, 0, 0], 8, Range::Unspecified),
        ([10, 0, 0, 0], 8, Range::Private),
        ([100, 64, 0, 0], 10, Range::SharedCgnat),
        ([127, 0, 0, 0], 8, Range::Loopback),
        ([169, 254, 0, 0], 16, Range::LinkLocal),
        ([172, 16, 0, 0], 12, Range::Private),
        ([192, 0, 0, 0], 24, Range::Reserved),
        ([192, 0, 2, 0], 24, Range::Documentation),
        ([192, 88, 99, 0], 24, Range::Reserved),
        ([192, 168, 0, 0], 16, Range::Private),
        ([198, 18, 0, 0], 15, Range::Benchmarking),
        ([198, 51, 100, 0], 24, Range::Documentation),
        ([203, 0, 113, 0], 24, Range::Documentation),
        ([224, 0, 0, 0], 4, Range::Multicast),
        ([240, 0, 0, 0], 4, Range::Reserved),
    ];
    table.iter().find(|(n, b, _)| in_v4(a, *n, *b)).map(|(_, _, r)| *r)
}

fn in_v6(ip: u128, net: u128, bits: u32) -> bool {
    let mask = if bits == 0 { 0 } else { u128::MAX << (128 - bits) };
    ip & mask == net & mask
}

/// The IPv4 address an IPv6 address carries to, when it is one of the forms
/// whose packets end up at an IPv4 host.
pub fn embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let a = u128::from(ip);
    let low32 = Ipv4Addr::from(a as u32);
    if in_v6(a, 0xffff << 32, 96) { return Some(low32) }                         // ::ffff:a.b.c.d
    if in_v6(a, 0x0064_ff9b << 96, 96) { return Some(low32) }                    // 64:ff9b::a.b.c.d
    if in_v6(a, 0x2002 << 112, 16) { return Some(Ipv4Addr::from((a >> 80) as u32)) } // 2002:aabb:ccdd::
    None
}

pub fn classify_v6(ip: Ipv6Addr) -> Option<Range> {
    let a = u128::from(ip);
    if ip.is_unspecified() { return Some(Range::Unspecified) }
    if ip.is_loopback() { return Some(Range::Loopback) }
    // ★ The embedded forms are judged by where they lead, NOT refused
    // wholesale and NOT admitted as "some IPv6 address": ::ffff:127.0.0.1 is
    // loopback, ::ffff:93.184.215.14 is a public host.
    if let Some(v4) = embedded_v4(ip) { return classify_v4(v4) }
    if ip == "fd00:ec2::254".parse::<Ipv6Addr>().unwrap() { return Some(Range::Metadata) }
    let table: &[(u128, u32, Range)] = &[
        (0, 96, Range::Reserved),                                   // ::/96 IPv4-compatible (deprecated)
        (0x0064_ff9b_0001 << 80, 48, Range::Reserved),              // 64:ff9b:1::/48 local-use NAT64
        (0x0100 << 112, 64, Range::Reserved),                       // 100::/64 discard
        (0x2001_0db8 << 96, 32, Range::Documentation),              // 2001:db8::/32
        (0x3fff << 112, 20, Range::Documentation),                  // 3fff::/20 (RFC 9637)
        (0x2001 << 112, 23, Range::Reserved),                       // 2001::/23 IETF (Teredo, ORCHID…)
        (0xfc00 << 112, 7, Range::UniqueLocal),
        (0xfe80 << 112, 10, Range::LinkLocal),
        (0xfec0 << 112, 10, Range::Reserved),                       // site-local (deprecated)
        (0xff00 << 112, 8, Range::Multicast),
    ];
    if let Some((_, _, r)) = table.iter().find(|(n, b, _)| in_v6(a, *n, *b)) { return Some(*r) }
    // Default-deny: global unicast is 2000::/3 and nothing else.
    if !in_v6(a, 0x2000 << 112, 3) { return Some(Range::Reserved) }
    None
}

/// Name of the environment variable that grants the private network. The
/// name is the grant: it should read as a decision someone made, not a knob.
pub const GRANT_PRIVATE_NETWORK_ENV: &str = "NAVIGATOR_FETCH_GRANT_PRIVATE_NETWORK";

/// What a fetch may reach. `Default` is the safe one: public addresses only.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Policy {
    /// ★ A DELIBERATE GRANT, off by default. Admits loopback, RFC 1918,
    /// link-local, CGNAT and ULA — the intranet. Never the metadata endpoint.
    /// The spec's shape for this is a per-origin grant bound to the network
    /// it was given on (threat model §5 q2); this is the process-wide stand-in
    /// until the broker has somewhere to keep grants.
    pub grant_private_network: bool,
}

impl Policy {
    /// `NAVIGATOR_FETCH_GRANT_PRIVATE_NETWORK=1` and nothing else turns it on;
    /// any other value, or none, leaves the default.
    pub fn from_env() -> Policy {
        Policy { grant_private_network: std::env::var(GRANT_PRIVATE_NETWORK_ENV).ok().as_deref() == Some("1") }
    }

    /// `Ok` if this policy lets a fetch connect to `ip`.
    pub fn admit(&self, ip: IpAddr) -> Result<(), Range> {
        match classify(ip) {
            None => Ok(()),
            Some(r) if self.grant_private_network && r.is_private_network() => Ok(()),
            Some(r) => Err(r),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v4(s: &str) -> Option<Range> { classify(s.parse().unwrap()) }

    #[test]
    fn every_ipv4_range_with_both_edges() {
        // (address, expected) — first and last address of each range, plus
        // the address just outside it, so an off-by-one prefix shows up.
        let cases: &[(&str, Option<Range>)] = &[
            ("0.0.0.0", Some(Range::Unspecified)), ("0.255.255.255", Some(Range::Unspecified)),
            ("1.0.0.0", None),
            ("9.255.255.255", None), ("10.0.0.0", Some(Range::Private)), ("10.255.255.255", Some(Range::Private)), ("11.0.0.0", None),
            ("100.63.255.255", None), ("100.64.0.0", Some(Range::SharedCgnat)), ("100.127.255.255", Some(Range::SharedCgnat)), ("100.128.0.0", None),
            ("126.255.255.255", None), ("127.0.0.1", Some(Range::Loopback)), ("127.255.255.255", Some(Range::Loopback)), ("128.0.0.0", None),
            ("169.253.255.255", None), ("169.254.0.0", Some(Range::LinkLocal)), ("169.254.255.255", Some(Range::LinkLocal)), ("169.255.0.0", None),
            ("169.254.169.254", Some(Range::Metadata)), ("169.254.169.253", Some(Range::LinkLocal)),
            ("172.15.255.255", None), ("172.16.0.0", Some(Range::Private)), ("172.31.255.255", Some(Range::Private)), ("172.32.0.0", None),
            ("192.0.0.0", Some(Range::Reserved)), ("192.0.0.255", Some(Range::Reserved)), ("192.0.1.0", None),
            ("192.0.2.0", Some(Range::Documentation)), ("192.0.2.255", Some(Range::Documentation)), ("192.0.3.0", None),
            ("192.88.99.1", Some(Range::Reserved)),
            ("192.167.255.255", None), ("192.168.0.0", Some(Range::Private)), ("192.168.255.255", Some(Range::Private)), ("192.169.0.0", None),
            ("198.17.255.255", None), ("198.18.0.0", Some(Range::Benchmarking)), ("198.19.255.255", Some(Range::Benchmarking)), ("198.20.0.0", None),
            ("198.51.100.7", Some(Range::Documentation)), ("203.0.113.9", Some(Range::Documentation)),
            ("223.255.255.255", None), ("224.0.0.0", Some(Range::Multicast)), ("239.255.255.255", Some(Range::Multicast)),
            ("240.0.0.0", Some(Range::Reserved)), ("255.255.255.254", Some(Range::Reserved)),
            ("255.255.255.255", Some(Range::Broadcast)),
            ("93.184.215.14", None), ("8.8.8.8", None), ("1.1.1.1", None),
        ];
        for (a, want) in cases { assert_eq!(v4(a), *want, "{a}") }
    }

    #[test]
    fn every_ipv6_range() {
        let cases: &[(&str, Option<Range>)] = &[
            ("::", Some(Range::Unspecified)), ("::1", Some(Range::Loopback)),
            ("::2", Some(Range::Reserved)), ("::a00:1", Some(Range::Reserved)), // IPv4-compatible
            ("64:ff9b:1::1", Some(Range::Reserved)),
            ("100::1", Some(Range::Reserved)), ("100:0:0:1::", Some(Range::Reserved)),
            ("2001:db8::1", Some(Range::Documentation)), ("2001:db8:ffff::", Some(Range::Documentation)),
            ("3fff::1", Some(Range::Documentation)), ("3fff:fff::", Some(Range::Documentation)), ("3fff:1000::", None),
            ("2001::1", Some(Range::Reserved)), ("2001:1ff::", Some(Range::Reserved)), ("2001:200::1", None),
            ("fc00::1", Some(Range::UniqueLocal)), ("fdff:ffff::1", Some(Range::UniqueLocal)),
            ("fd00:ec2::254", Some(Range::Metadata)),
            ("fe80::1", Some(Range::LinkLocal)), ("febf::1", Some(Range::LinkLocal)),
            ("fec0::1", Some(Range::Reserved)),
            ("ff02::1", Some(Range::Multicast)), ("ff05::2", Some(Range::Multicast)),
            ("1fff:ffff::", Some(Range::Reserved)), ("4000::1", Some(Range::Reserved)), ("e000::1", Some(Range::Reserved)),
            ("2606:2800:21f:cb07:6820:80da:af6b:8b2c", None), ("2a00:1450:4001::68", None),
        ];
        for (a, want) in cases { assert_eq!(v4(a), *want, "{a}") }
    }

    #[test]
    fn embedded_ipv4_forms_are_judged_by_the_host_they_reach() {
        for (a, want) in [
            ("::ffff:127.0.0.1", Some(Range::Loopback)),
            ("::ffff:10.1.2.3", Some(Range::Private)),
            ("::ffff:192.168.1.1", Some(Range::Private)),
            ("::ffff:169.254.169.254", Some(Range::Metadata)),
            ("::ffff:100.64.0.1", Some(Range::SharedCgnat)),
            ("::ffff:0.0.0.0", Some(Range::Unspecified)),
            ("::ffff:255.255.255.255", Some(Range::Broadcast)),
            ("::ffff:224.0.0.1", Some(Range::Multicast)),
            ("::ffff:93.184.215.14", None),
            ("64:ff9b::7f00:1", Some(Range::Loopback)),
            ("64:ff9b::a9fe:a9fe", Some(Range::Metadata)),
            ("64:ff9b::808:808", None),
            ("2002:7f00:1::", Some(Range::Loopback)),
            ("2002:c0a8:101::1", Some(Range::Private)),
            ("2002:808:808::1", None),
        ] { assert_eq!(v4(a), want, "{a}") }
    }

    #[test]
    fn the_grant_admits_the_intranet_and_nothing_else() {
        let open = Policy { grant_private_network: true };
        let shut = Policy::default();
        for a in ["127.0.0.1", "10.0.0.5", "172.16.1.1", "192.168.1.1", "169.254.1.1", "100.64.0.1",
                  "::1", "fd12::1", "fe80::1", "::ffff:192.168.1.1"] {
            let ip: IpAddr = a.parse().unwrap();
            assert!(open.admit(ip).is_ok(), "grant should admit {a}");
            assert!(shut.admit(ip).is_err(), "default should refuse {a}");
        }
        // ★ Never, grant or no grant.
        for a in ["169.254.169.254", "fd00:ec2::254", "::ffff:169.254.169.254", "0.0.0.0", "::",
                  "255.255.255.255", "224.0.0.1", "ff02::1", "192.0.2.1", "2001:db8::1", "240.0.0.1", "198.18.0.1"] {
            let ip: IpAddr = a.parse().unwrap();
            assert!(open.admit(ip).is_err(), "grant must still refuse {a}");
        }
        for a in ["93.184.215.14", "2606:2800:21f:cb07:6820:80da:af6b:8b2c"] {
            assert!(shut.admit(a.parse().unwrap()).is_ok(), "{a} is public");
        }
    }

    #[test]
    fn the_grant_is_on_only_for_exactly_one() {
        // Serialised through one test: the environment is process-global.
        for (v, want) in [(Some("1"), true), (Some("0"), false), (Some("true"), false), (Some(""), false), (None, false)] {
            match v { Some(v) => std::env::set_var(GRANT_PRIVATE_NETWORK_ENV, v), None => std::env::remove_var(GRANT_PRIVATE_NETWORK_ENV) }
            assert_eq!(Policy::from_env().grant_private_network, want, "{v:?}");
        }
        std::env::remove_var(GRANT_PRIVATE_NETWORK_ENV);
    }
}
