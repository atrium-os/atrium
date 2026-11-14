//! What a jailed `atrium-lease` may ask of `atrium-net`, and the checks
//! `atrium-net` runs before it touches the host (docs/spec/atrium-net.md §5.2).
//!
//! atrium-lease parses DHCP and router advertisements; a compromised one can
//! send any proposal it likes. Everything here assumes that: a proposal is
//! applied only if [`check_v4`] accepts it, and the interface otherwise keeps
//! its last good state.

use aura_ipc::Line;
use std::net::Ipv4Addr;

/// The app address range (network.md §0). Nothing from the wire may claim it.
pub const APP_NET: (Ipv4Addr, u8) = (Ipv4Addr::new(100, 64, 0, 0), 16);
pub const MAX_ROUTES: usize = 16;
pub const MAX_DNS: usize = 3;
pub const MIN_LEASE_S: u32 = 60;
pub const MAX_LEASE_S: u32 = 7 * 24 * 3600;
pub const MTU_MIN: u16 = 1280;
pub const MTU_MAX: u16 = 9000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Route {
    pub dst: Ipv4Addr,
    pub prefix: u8,
    pub gw: Ipv4Addr,
}

/// An IPv4 lease as atrium-lease proposes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseV4 {
    pub addr: Ipv4Addr,
    pub prefix: u8,
    pub router: Option<Ipv4Addr>,
    /// Classless static routes (RFC 3442); when present they replace `router`.
    pub routes: Vec<Route>,
    pub dns: Vec<Ipv4Addr>,
    pub mtu: Option<u16>,
    pub lease_s: u32,
    pub server: Ipv4Addr,
}

/// What the rest of the host already uses, so a lease cannot overlap it.
#[derive(Clone, Debug, Default)]
pub struct HostView {
    /// (address, prefix) of every other interface.
    pub others: Vec<(Ipv4Addr, u8)>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    Prefix,
    Address,
    Overlap,
    Router,
    Routes,
    Dns,
    Lease,
    Mtu,
}

pub fn mask(prefix: u8) -> u32 {
    if prefix == 0 { 0 } else { u32::MAX << (32 - prefix as u32) }
}

pub fn in_net(a: Ipv4Addr, net: Ipv4Addr, prefix: u8) -> bool {
    (u32::from(a) & mask(prefix)) == (u32::from(net) & mask(prefix))
}

fn nets_overlap(a: Ipv4Addr, ap: u8, b: Ipv4Addr, bp: u8) -> bool {
    let p = ap.min(bp);
    in_net(a, b, p)
}

/// Unicast and none of the special ranges a DHCP server has no business
/// handing out (§5.2).
pub fn ordinary_unicast(a: Ipv4Addr) -> bool {
    let o = a.octets();
    !(a.is_unspecified()
        || o[0] == 0
        || a.is_loopback()
        || a.is_link_local()
        || a.is_multicast()
        || a.is_broadcast()
        || o[0] >= 240
        || in_net(a, APP_NET.0, APP_NET.1))
}

pub fn check_v4(l: &LeaseV4, host: &HostView) -> Result<(), Refused> {
    if !(8..=30).contains(&l.prefix) {
        return Err(Refused::Prefix);
    }
    let m = mask(l.prefix);
    let a = u32::from(l.addr);
    // Not the network or broadcast address of its own subnet.
    if !ordinary_unicast(l.addr) || a & !m == 0 || a & !m == !m {
        return Err(Refused::Address);
    }
    if nets_overlap(l.addr, l.prefix, APP_NET.0, APP_NET.1) {
        return Err(Refused::Overlap);
    }
    if host.others.iter().any(|&(o, p)| nets_overlap(l.addr, l.prefix, o, p)) {
        return Err(Refused::Overlap);
    }
    if let Some(r) = l.router {
        if !ordinary_unicast(r) || r == l.addr || !in_net(r, l.addr, l.prefix) {
            return Err(Refused::Router);
        }
    }
    if l.routes.len() > MAX_ROUTES {
        return Err(Refused::Routes);
    }
    for r in &l.routes {
        if r.prefix > 32 || u32::from(r.dst) & !mask(r.prefix) != 0 {
            return Err(Refused::Routes);
        }
        // A route may never cover the app range or loopback.
        if nets_overlap(r.dst, r.prefix, APP_NET.0, APP_NET.1)
            || nets_overlap(r.dst, r.prefix, Ipv4Addr::new(127, 0, 0, 0), 8)
        {
            return Err(Refused::Routes);
        }
        // Gateway 0.0.0.0 means on-link (RFC 3442); otherwise on our subnet.
        if !r.gw.is_unspecified() && (!ordinary_unicast(r.gw) || r.gw == l.addr || !in_net(r.gw, l.addr, l.prefix)) {
            return Err(Refused::Routes);
        }
    }
    if l.dns.len() > MAX_DNS || l.dns.iter().any(|&d| !ordinary_unicast(d)) {
        return Err(Refused::Dns);
    }
    if l.lease_s < MIN_LEASE_S {
        return Err(Refused::Lease);
    }
    if let Some(mtu) = l.mtu {
        if !(MTU_MIN..=MTU_MAX).contains(&mtu) {
            return Err(Refused::Mtu);
        }
    }
    Ok(())
}

impl LeaseV4 {
    /// `lease4 addr=A/P router=R routes=D/P@G,.. dns=A,B mtu=N secs=S server=S`
    pub fn to_line(&self) -> Line {
        let mut l = Line::new("lease4")
            .with("addr", format!("{}/{}", self.addr, self.prefix))
            .with("secs", self.lease_s.min(MAX_LEASE_S))
            .with("server", self.server);
        if let Some(r) = self.router {
            l = l.with("router", r);
        }
        if !self.routes.is_empty() {
            let v: Vec<String> = self.routes.iter().map(|r| format!("{}/{}@{}", r.dst, r.prefix, r.gw)).collect();
            l = l.with("routes", v.join(","));
        }
        if !self.dns.is_empty() {
            let v: Vec<String> = self.dns.iter().map(|d| d.to_string()).collect();
            l = l.with("dns", v.join(","));
        }
        if let Some(m) = self.mtu {
            l = l.with("mtu", m);
        }
        l
    }

    pub fn from_line(l: &Line) -> Option<LeaseV4> {
        if l.verb != "lease4" {
            return None;
        }
        let (addr, prefix) = cidr(l.get("addr")?)?;
        let routes = match l.get("routes") {
            None => Vec::new(),
            Some(s) => s
                .split(',')
                .map(|r| {
                    let (net, gw) = r.split_once('@')?;
                    let (dst, prefix) = cidr(net)?;
                    Some(Route { dst, prefix, gw: gw.parse().ok()? })
                })
                .collect::<Option<Vec<_>>>()?,
        };
        let dns = match l.get("dns") {
            None => Vec::new(),
            Some(s) => s.split(',').map(|d| d.parse().ok()).collect::<Option<Vec<_>>>()?,
        };
        Some(LeaseV4 {
            addr,
            prefix,
            router: match l.get("router") { Some(r) => Some(r.parse().ok()?), None => None },
            routes,
            dns,
            mtu: match l.get("mtu") { Some(m) => Some(m.parse().ok()?), None => None },
            lease_s: l.num("secs")?,
            server: l.get("server")?.parse().ok()?,
        })
    }
}

fn cidr(s: &str) -> Option<(Ipv4Addr, u8)> {
    let (a, p) = s.split_once('/')?;
    let p: u8 = p.parse().ok()?;
    (p <= 32).then_some(())?;
    Some((a.parse().ok()?, p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good() -> LeaseV4 {
        LeaseV4 {
            addr: Ipv4Addr::new(192, 168, 1, 50),
            prefix: 24,
            router: Some(Ipv4Addr::new(192, 168, 1, 1)),
            routes: vec![],
            dns: vec![Ipv4Addr::new(192, 168, 1, 1)],
            mtu: Some(1500),
            lease_s: 3600,
            server: Ipv4Addr::new(192, 168, 1, 1),
        }
    }

    #[test]
    fn accepts_ordinary_lease_and_roundtrips() {
        let l = good();
        assert_eq!(check_v4(&l, &HostView::default()), Ok(()));
        let line = Line::parse(std::str::from_utf8(&l.to_line().encode().unwrap()).unwrap().trim_end()).unwrap();
        assert_eq!(LeaseV4::from_line(&line), Some(l));
    }

    #[test]
    fn refuses_hostile_leases() {
        let h = HostView { others: vec![(Ipv4Addr::new(10, 0, 2, 15), 24)] };
        let cases: Vec<(Box<dyn Fn(&mut LeaseV4)>, Refused)> = vec![
            (Box::new(|l| l.prefix = 31), Refused::Prefix),
            (Box::new(|l| l.prefix = 4), Refused::Prefix),
            (Box::new(|l| l.addr = Ipv4Addr::new(127, 0, 0, 2)), Refused::Address),
            (Box::new(|l| l.addr = Ipv4Addr::new(224, 0, 0, 5)), Refused::Address),
            (Box::new(|l| l.addr = Ipv4Addr::new(192, 168, 1, 255)), Refused::Address),
            (Box::new(|l| l.addr = Ipv4Addr::new(192, 168, 1, 0)), Refused::Address),
            (Box::new(|l| { l.addr = Ipv4Addr::new(100, 64, 3, 2); l.router = None }), Refused::Address),
            (Box::new(|l| { l.addr = Ipv4Addr::new(100, 0, 0, 5); l.prefix = 8; l.router = None }), Refused::Overlap),
            (Box::new(|l| { l.addr = Ipv4Addr::new(10, 0, 2, 99); l.router = None }), Refused::Overlap),
            (Box::new(|l| l.router = Some(Ipv4Addr::new(8, 8, 8, 8))), Refused::Router),
            (Box::new(|l| l.router = Some(l.addr)), Refused::Router),
            (Box::new(|l| l.routes = vec![Route { dst: Ipv4Addr::new(100, 64, 0, 0), prefix: 10, gw: Ipv4Addr::new(192, 168, 1, 1) }]), Refused::Routes),
            (Box::new(|l| l.routes = vec![Route { dst: Ipv4Addr::new(0, 0, 0, 0), prefix: 1, gw: Ipv4Addr::new(192, 168, 1, 1) }]), Refused::Routes),
            (Box::new(|l| l.routes = vec![Route { dst: Ipv4Addr::new(10, 1, 0, 0), prefix: 16, gw: Ipv4Addr::new(10, 9, 9, 9) }]), Refused::Routes),
            (Box::new(|l| l.routes = vec![Route { dst: Ipv4Addr::new(10, 1, 0, 1), prefix: 16, gw: Ipv4Addr::new(192, 168, 1, 1) }]), Refused::Routes),
            (Box::new(|l| l.routes = vec![Route { dst: Ipv4Addr::new(10, 1, 0, 0), prefix: 16, gw: Ipv4Addr::new(192, 168, 1, 1) }; 17]), Refused::Routes),
            (Box::new(|l| l.dns = vec![Ipv4Addr::new(127, 0, 0, 1)]), Refused::Dns),
            (Box::new(|l| l.dns = vec![Ipv4Addr::new(1, 1, 1, 1); 4]), Refused::Dns),
            (Box::new(|l| l.lease_s = 5), Refused::Lease),
            (Box::new(|l| l.mtu = Some(576)), Refused::Mtu),
        ];
        for (i, (f, want)) in cases.into_iter().enumerate() {
            let mut l = good();
            f(&mut l);
            assert_eq!(check_v4(&l, &h), Err(want), "case {i}: {l:?}");
        }
        // The 0/1 route case above covers half the internet; that is fine on
        // its own (VPN-style), so it is refused only because it covers 100.64.
        let mut l = good();
        l.routes = vec![Route { dst: Ipv4Addr::new(128, 0, 0, 0), prefix: 1, gw: Ipv4Addr::new(192, 168, 1, 1) }];
        assert_eq!(check_v4(&l, &h), Ok(()));
    }

    #[test]
    fn hostile_lines_do_not_parse() {
        for s in ["lease4 addr=1.2.3.4/33 secs=60 server=1.1.1.1", "lease4 addr=1.2.3.4 secs=60 server=1.1.1.1", "lease4 addr=1.2.3.4/24 secs=x server=1.1.1.1", "lease4 addr=1.2.3.4/24 secs=60 server=1.1.1.1 routes=1.0.0.0/8"] {
            assert_eq!(LeaseV4::from_line(&Line::parse(s).unwrap()), None, "{s}");
        }
    }
}
