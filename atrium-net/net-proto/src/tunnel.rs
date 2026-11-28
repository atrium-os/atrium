//! A VPN tunnel as cryptad proposes it to atrium-net (vpn.md §5.1, §5.2),
//! the checks atrium-net runs before anything changes, and the pf anchor
//! `atrium/vpn` it writes (§5.4).
//!
//! cryptad is trusted more than atrium-lease (it is root, like atrium-net),
//! but what it proposes comes from a profile a VPN provider wrote and, for
//! corporate engines later, from a gateway's push. So the shape is checked
//! here the same way a lease is: addresses, routes, DNS and the gateway, all
//! bounded, before a route or a resolver changes.

use crate::{in_net, ordinary_unicast, APP_NET, HostView};
use aura_ipc::Line;
use std::net::Ipv4Addr;

pub const MAX_INCLUDES: usize = 1024;
pub const MAX_ADDRS: usize = 4;
pub const MAX_TUNNEL_DNS: usize = 3;
pub const MAX_DOMAINS: usize = 64;
pub const TUN_MTU_MIN: u16 = 1280;
pub const TUN_MTU_MAX: u16 = 1500;
/// pf tag jaild puts on a Crypta engine's flows (on its epair), which only
/// those may carry to a gateway's address and port out the physical
/// interface (§5.4).
pub const GLOBALPROTECT_ESP_PORT: u16 = 4501;
pub const OUTER_TAG: &str = "CRYPTA_OUTER";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Net4 {
    pub addr: Ipv4Addr,
    pub prefix: u8,
}

impl Net4 {
    pub fn parse(s: &str) -> Option<Net4> {
        let (a, p) = s.split_once('/')?;
        let prefix: u8 = p.parse().ok().filter(|p| *p <= 32 && !s.contains('+'))?;
        Some(Net4 { addr: a.parse().ok()?, prefix })
    }

    /// The network (host bits cleared).
    pub fn net(&self) -> Ipv4Addr {
        Ipv4Addr::from(u32::from(self.addr) & crate::mask(self.prefix))
    }

    pub fn contains(&self, a: Ipv4Addr) -> bool {
        in_net(a, self.addr, self.prefix)
    }

    fn covers(&self, o: &Net4) -> bool {
        o.prefix >= self.prefix && self.contains(o.addr)
    }

    fn overlaps(&self, o: &Net4) -> bool {
        self.covers(o) || o.covers(self)
    }
}

impl std::fmt::Display for Net4 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.prefix)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tunnel {
    /// The tunnel's own addresses (with their prefixes: a /24 also routes
    /// its network into the tunnel).
    pub addrs: Vec<Net4>,
    pub mtu: u16,
    /// Networks routed into the tunnel. `full` stands for 0.0.0.0/0.
    pub includes: Vec<Net4>,
    pub full: bool,
    pub dns: Vec<Ipv4Addr>,
    pub domains: Vec<String>,
    /// Mark the domains insecure for DNSSEC (internal zones under a signed
    /// parent); off unless the profile says so (§5.5).
    pub insecure: bool,
    /// The VPN server, pinned to the physical egress.
    pub gateway: Ipv4Addr,
    pub port: u16,
    /// The engine reaches the gateway over TCP (AnyConnect's TLS, §9),
    /// not UDP (WireGuard).
    pub tcp: bool,
    /// GlobalProtect also needs UDP/4501 to the same gateway.
    pub gp_esp: bool,
    /// Block when not connected (§5.4): only with `full`.
    pub block: bool,
    /// Persona scope (§5.3, §9.7): the routes go in the tunnel's own route
    /// domain (a FIB named by the tunnel's id), never the host's table, and
    /// the host's DNS is left alone.
    pub scoped: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum TunnelRefused {
    Address,
    Overlap,
    Routes,
    Dns,
    Domains,
    Gateway,
    Shape,
}

impl TunnelRefused {
    pub fn word(&self) -> &'static str {
        match self {
            TunnelRefused::Address => "address",
            TunnelRefused::Overlap => "overlap",
            TunnelRefused::Routes => "routes",
            TunnelRefused::Dns => "dns",
            TunnelRefused::Domains => "domains",
            TunnelRefused::Gateway => "gateway",
            TunnelRefused::Shape => "malformed",
        }
    }
}

fn list<'a>(l: &'a Line, k: &str) -> impl Iterator<Item = &'a str> {
    l.get(k).unwrap_or("").split(',').filter(|s| !s.is_empty())
}

/// Names a tunnel may never take over (§5.2): the host's own and the local
/// link's.
pub fn reserved_domain(d: &str) -> bool {
    let d = d.trim_end_matches('.');
    ["localhost", "local", "atrium", "home.arpa", "in-addr.arpa", "ip6.arpa"]
        .iter()
        .any(|r| d == *r || d.ends_with(&format!(".{r}")))
}

pub fn valid_domain(d: &str) -> bool {
    !d.is_empty()
        && d.len() <= 253
        && d.split('.').all(|l| !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
}

impl Tunnel {
    /// From `tunnel-apply`'s fields and the includes sent before it with
    /// `tunnel-routes`. Shape only; [`check_tunnel`] judges the values.
    pub fn from_line(l: &Line, includes: Vec<Net4>) -> Option<Tunnel> {
        let addrs: Option<Vec<Net4>> = list(l, "addr").map(Net4::parse).collect();
        let dns: Option<Vec<Ipv4Addr>> = list(l, "dns").map(|a| a.parse().ok()).collect();
        let flag = |k: &str| match l.get(k) {
            None | Some("0") => Some(false),
            Some("1") => Some(true),
            _ => None,
        };
        Some(Tunnel {
            addrs: addrs?,
            mtu: l.num::<u16>("mtu").unwrap_or(1420),
            includes,
            full: flag("full")?,
            dns: dns?,
            domains: list(l, "domains").map(str::to_string).collect(),
            insecure: flag("insecure")?,
            gateway: l.get("gw")?.parse().ok()?,
            port: l.num::<u16>("port")?,
            tcp: matches!(l.get("proto"), Some("tcp" | "gp")),
            gp_esp: l.get("proto") == Some("gp"),
            block: flag("block")?,
            scoped: flag("scoped")?,
        })
    }

    pub fn to_lines(&self, id: &str) -> Vec<Line> {
        let mut v: Vec<Line> = self
            .includes
            .chunks(128)
            .map(|c| Line::new("tunnel-routes").with("id", id).with("add", c.iter().map(Net4::to_string).collect::<Vec<_>>().join(",")))
            .collect();
        let join = |x: Vec<String>| x.join(",");
        let mut a = Line::new("tunnel-apply")
            .with("id", id)
            .with("addr", join(self.addrs.iter().map(Net4::to_string).collect()))
            .with("mtu", self.mtu)
            .with("full", self.full as u8)
            .with("gw", self.gateway)
            .with("port", self.port)
            .with("proto", if self.gp_esp { "gp" } else if self.tcp { "tcp" } else { "udp" })
            .with("block", self.block as u8)
            .with("insecure", self.insecure as u8)
            .with("scoped", self.scoped as u8);
        if !self.dns.is_empty() {
            a = a.with("dns", join(self.dns.iter().map(Ipv4Addr::to_string).collect()));
        }
        if !self.domains.is_empty() {
            a = a.with("domains", self.domains.join(","));
        }
        v.push(a);
        v
    }

    pub fn link_address_inside(&self, ip: std::net::IpAddr) -> bool {
        match ip.to_canonical() {
            std::net::IpAddr::V4(addr) => self.routes_to(addr),
            std::net::IpAddr::V6(_) => self.full,
        }
    }

    pub fn link_domain_inside(&self, host: &str) -> bool {
        self.domains.iter().any(|d| host == d || host.strip_suffix(d).is_some_and(|prefix| prefix.ends_with('.')))
    }

    /// Whether `a` goes into the tunnel by these routes.
    pub fn routes_to(&self, a: Ipv4Addr) -> bool {
        self.full || self.includes.iter().chain(&self.addrs).any(|n| n.contains(a))
    }
}

/// What may never be routed into a tunnel: loopback, link-local, multicast
/// and the class E range, and the app range (§5.2).
const FORBIDDEN: [(Ipv4Addr, u8); 5] = [
    (Ipv4Addr::new(127, 0, 0, 0), 8),
    (Ipv4Addr::new(169, 254, 0, 0), 16),
    (Ipv4Addr::new(224, 0, 0, 0), 4),
    (Ipv4Addr::new(240, 0, 0, 0), 4),
    (APP_NET.0, APP_NET.1),
];

/// The checks of vpn.md §5.2. `host`: the other interfaces' subnets (not
/// this tunnel's own interface). MTU is clamped, not refused.
pub fn check_tunnel(t: &mut Tunnel, host: &HostView) -> Result<(), TunnelRefused> {
    t.mtu = t.mtu.clamp(TUN_MTU_MIN, TUN_MTU_MAX);
    if t.addrs.is_empty() || t.addrs.len() > MAX_ADDRS {
        return Err(TunnelRefused::Address);
    }
    for a in &t.addrs {
        if !(8..=32).contains(&a.prefix) || !ordinary_unicast(a.addr) {
            return Err(TunnelRefused::Address);
        }
        // Not one of this host's other networks: a tunnel address inside
        // the LAN's subnet would take the LAN's traffic.
        if host.others.iter().any(|&(o, p)| a.overlaps(&Net4 { addr: o, prefix: p })) {
            return Err(TunnelRefused::Overlap);
        }
    }
    if t.includes.len() > MAX_INCLUDES {
        return Err(TunnelRefused::Routes);
    }
    for r in &t.includes {
        if r.prefix == 0 {
            // "Everything" is said with `full`, never as a route.
            return Err(TunnelRefused::Routes);
        }
        if FORBIDDEN.iter().any(|&(n, p)| r.overlaps(&Net4 { addr: n, prefix: p })) {
            return Err(TunnelRefused::Routes);
        }
        if host.others.iter().any(|&(o, p)| r.overlaps(&Net4 { addr: o, prefix: p })) {
            return Err(TunnelRefused::Overlap);
        }
    }
    // The gateway: an ordinary address, never inside an include (it would
    // be routed into its own tunnel; with `full` the pinned host route
    // keeps it out), never one of the tunnel's own networks.
    if !ordinary_unicast(t.gateway) || t.port == 0 || t.includes.iter().chain(&t.addrs).any(|n| n.contains(t.gateway)) {
        return Err(TunnelRefused::Gateway);
    }
    if t.dns.len() > MAX_TUNNEL_DNS || t.dns.iter().any(|&d| !ordinary_unicast(d) || !t.routes_to(d)) {
        return Err(TunnelRefused::Dns);
    }
    for (i, d) in t.dns.iter().enumerate() {
        if t.dns[..i].contains(d) {
            return Err(TunnelRefused::Dns);
        }
    }
    if t.domains.len() > MAX_DOMAINS || t.domains.iter().any(|d| !valid_domain(d) || reserved_domain(d)) {
        return Err(TunnelRefused::Domains);
    }
    // Domains with nowhere to send them would answer nothing: refused, so
    // the profile is fixed rather than silently broken.
    if !t.domains.is_empty() && t.dns.is_empty() {
        return Err(TunnelRefused::Domains);
    }
    if t.block && !t.full {
        return Err(TunnelRefused::Shape);
    }
    Ok(())
}

/// One applied tunnel as the anchor needs it.
pub struct AnchorTunnel<'a> {
    pub iface: &'a str,
    pub gateway: Ipv4Addr,
    pub port: u16,
    pub tcp: bool,
    /// GlobalProtect also needs UDP/4501 to the same gateway.
    pub gp_esp: bool,
    pub full: bool,
    pub block: bool,
    /// A persona-scoped tunnel's DNS (§5.5, V2): every lookup from an app in
    /// route domain `.0` (its addresses are in pf table `atrium_fib_<.0>`,
    /// kept by jaild) is redirected to `.1` on the app's gateway, so it
    /// never reaches the network's own servers or the machine's resolver.
    pub rdr: Option<(u32, Ipv4Addr)>,
}

/// The pf table jaild keeps of the app addresses in route domain `fib`.
pub fn domain_table(fib: u32) -> String {
    format!("atrium_fib_{fib}")
}

/// The `atrium/vpn` anchor (§5.4) for every applied tunnel, with `phys` the
/// physical egress (`None`: no egress yet, so nothing passes out at all
/// for a blocking tunnel; the rules then name the egress group).
///
/// Loaded with `pfctl -a atrium/vpn -f -`: its `nat` rules run from the
/// base ruleset's `nat-anchor "atrium/vpn"` (before the apps' NAT on the
/// egress), its filter rules from `anchor "atrium/*"`.
/// The tunnels' DNS zones as an unbound config file (vpn.md §5.5), so a
/// restart of local-unbound starts with them rather than sending a
/// tunnel's names to the network's servers until atrium-net sets them
/// again. Each entry: the zones applied, the tunnel's servers, whether
/// they are DNSSEC-insecure. A name is written once (the first tunnel's),
/// and only a valid, unreserved one: a duplicate zone or a stray byte
/// would stop unbound from starting at all.
pub fn render_zones(tunnels: &[(&[String], &[Ipv4Addr], bool)]) -> String {
    let mut out = String::from("# Written by atrium-net: the VPN tunnels' DNS zones (vpn.md §5.5). Do not edit.\n");
    let mut seen: Vec<&str> = Vec::new();
    let mut insecure = Vec::new();
    let mut zones = String::new();
    for (names, dns, ins) in tunnels {
        if dns.is_empty() {
            continue;
        }
        for d in names.iter() {
            if !valid_domain(d) || reserved_domain(d) || seen.contains(&d.as_str()) {
                continue;
            }
            seen.push(d);
            zones.push_str(&format!("forward-zone:\n    name: \"{d}.\"\n"));
            for a in dns.iter() {
                zones.push_str(&format!("    forward-addr: {a}\n"));
            }
            if *ins {
                insecure.push(d.as_str());
            }
        }
    }
    if !insecure.is_empty() {
        out.push_str("server:\n");
        for d in insecure {
            out.push_str(&format!("    domain-insecure: \"{d}.\"\n"));
        }
    }
    out.push_str(&zones);
    out
}

/// unbound's forward.conf: the root's forwarders as atrium-net set them
/// live, so a restart of unbound starts with them (vpn.md §5.5). `off` (a
/// full tunnel without DNS servers) leaves no root zone: unbound resolves
/// from the roots, through the tunnel. Entries with `@853#name` are DNS over
/// TLS, and then only TLS. Anything that isn't an address is skipped.
pub fn render_forward(forwarders: &[String]) -> String {
    let mut out = String::from("# Written by atrium-net: the root's DNS forwarders (vpn.md §5.5). Do not edit.\n");
    let ok = |x: &&String| {
        let (addr, rest) = x.split_once('@').map_or((x.as_str(), None), |(a, r)| (a, Some(r)));
        let host = addr.split('%').next().unwrap_or("");
        let scope = addr.split_once('%').map_or(true, |(_, i)| !i.is_empty() && i.bytes().all(|b| b.is_ascii_digit()));
        let tls = rest.map_or(true, |r| r.split_once('#').is_some_and(|(p, n)| p == "853" && valid_domain(n)));
        host.parse::<std::net::IpAddr>().is_ok() && scope && tls
    };
    let list: Vec<&String> = forwarders.iter().filter(ok).collect();
    if list.is_empty() {
        return out;
    }
    out.push_str("forward-zone:\n    name: \".\"\n");
    if list.iter().any(|x| x.contains('@')) {
        out.push_str("    forward-tls-upstream: yes\n");
    }
    for a in list {
        out.push_str(&format!("    forward-addr: {a}\n"));
    }
    out
}

pub fn render_anchor(tunnels: &[AnchorTunnel], phys: Option<&str>) -> String {
    let out = phys.unwrap_or("atrium_egress");
    let mut r = String::new();
    for t in tunnels {
        // From the base ruleset's `rdr-anchor "atrium/vpn"`. `pass`: the
        // filter's block of the host's own addresses never sees it.
        if let Some((fib, dns)) = t.rdr {
            r.push_str(&format!(
                "rdr pass on atrium inet proto {{ udp tcp }} from <{}> to any port 53 -> {dns} port 53\n",
                domain_table(fib)
            ));
        }
    }
    for t in tunnels {
        // Apps' traffic that the tunnel carries leaves as the tunnel's address.
        r.push_str(&format!("nat on {} inet from {}/{} to any -> ({})\n", t.iface, APP_NET.0, APP_NET.1, t.iface));
    }
    for t in tunnels {
        if let Some((_, dns)) = t.rdr {
            // Root's tunnel-link questions must stay on this interface
            // even if a more-specific physical route appears. Bind their
            // reply state to the tun so it cannot bypass the egress block.
            let iface = t.iface;
            r.push_str(&format!("pass out quick on {iface} inet proto udp from ({iface}) to {dns} port 53 user root keep state (if-bound)\n"));
            r.push_str(&format!("block out quick on ! {iface} inet proto udp from ({iface}) to {dns} port 53 user root\n"));
        }
        // ServerIP (§6): only the engine's own flows reach the gateway's
        // address outside the tunnel; anything else to it is dropped here
        // (with `full`, the pinned host route would otherwise carry it).
        r.push_str(&format!(
            "pass out quick on {out} inet proto {} from any to {} port {} tagged {OUTER_TAG} keep state\n",
            if t.tcp { "tcp" } else { "udp" },
            t.gateway,
            t.port
        ));
        if t.gp_esp {
            r.push_str(&format!("pass out quick on {out} inet proto udp from any to {} port {GLOBALPROTECT_ESP_PORT} tagged {OUTER_TAG} keep state\n", t.gateway));
        }
        r.push_str(&format!("block out quick on {out} inet from any to {}\n", t.gateway));
        // Nothing from the tunnel opens a connection here (§5.4): only
        // replies to flows we opened, which pf's states pass before rules.
        r.push_str(&format!("block in quick on {} all\n", t.iface));
    }
    if tunnels.iter().any(|t| t.full) {
        // A full tunnel carries no IPv6 in V1, so IPv6 must not go around it
        // (§5.4, decision 8): out the physical interface, only neighbour
        // discovery. DHCP goes by BPF and never meets pf.
        r.push_str(&format!(
            "pass out quick on {out} inet6 proto ipv6-icmp all icmp6-type {{ neighbrsol, neighbradv, routersol }} keep state\n"
        ));
        r.push_str(&format!("block out quick on {out} inet6 all\n"));
    }
    if tunnels.iter().any(|t| t.full && t.block) {
        // Block when not connected: nothing else but the engines' tagged
        // flows (above).
        r.push_str(&format!("block out quick on {out} all\n"));
    }
    r
}

#[cfg(test)]
mod zone_tests {
    use super::*;

    #[test]
    fn zones_are_written_once_each_and_only_valid() {
        let a = vec!["corp.example".to_string(), "lab.corp.example".to_string()];
        let b = vec!["corp.example".to_string(), "bad\"\ninclude: /etc".to_string(), "local".to_string()];
        let dns1 = [Ipv4Addr::new(10, 98, 0, 1), Ipv4Addr::new(10, 98, 0, 2)];
        let dns2 = [Ipv4Addr::new(10, 99, 0, 1)];
        let t = render_zones(&[(&a, &dns1, true), (&b, &dns2, false), (&a, &[], false)]);
        assert_eq!(t.matches("forward-zone:").count(), 2, "{t}");
        assert!(t.contains("name: \"corp.example.\"\n    forward-addr: 10.98.0.1\n    forward-addr: 10.98.0.2\n"), "{t}");
        assert!(t.contains("server:\n    domain-insecure: \"corp.example.\"\n    domain-insecure: \"lab.corp.example.\"\n"), "{t}");
        assert!(!t.contains("include") && !t.contains("10.99.0.1") && !t.contains("\"local."), "{t}");
        assert_eq!(render_zones(&[]).lines().count(), 1, "only the header");
    }

    #[test]
    fn the_roots_forwarders_are_written_as_set() {
        let v = |x: &[&str]| x.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let t = render_forward(&v(&["10.98.0.1", "fe80::1%2", "bad\ninclude: /etc", "fe80::2%vtnet0"]));
        assert!(t.ends_with("forward-zone:\n    name: \".\"\n    forward-addr: 10.98.0.1\n    forward-addr: fe80::1%2\n"), "{t}");
        let t = render_forward(&v(&["1.1.1.1@853#one.one.one.one", "9.9.9.9@853#x\"y"]));
        assert!(t.contains("    forward-tls-upstream: yes\n    forward-addr: 1.1.1.1@853#one.one.one.one\n") && !t.contains("9.9.9.9"), "{t}");
        assert_eq!(render_forward(&v(&["off"])).lines().count(), 1, "only the header");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tunnel() -> Tunnel {
        let l = Line::new("tunnel-apply")
            .with("id", "home")
            .with("addr", "10.99.0.2/24")
            .with("full", 1)
            .with("dns", "10.99.0.1")
            .with("domains", "corp.test")
            .with("gw", "198.51.100.51")
            .with("port", 51820)
            .with("block", 1);
        Tunnel::from_line(&l, vec![]).unwrap()
    }

    fn host() -> HostView {
        HostView { others: vec![(Ipv4Addr::new(10, 88, 0, 50), 24)] }
    }

    #[test]
    fn link_domains_and_addresses_follow_the_source_tunnel() {
        let mut t = tunnel();
        t.full = false;
        assert!(t.link_domain_inside("corp.test") && t.link_domain_inside("a.corp.test"));
        assert!(!t.link_domain_inside("notcorp.test"));
        assert!(t.link_address_inside("::ffff:10.99.0.19".parse().unwrap()));
        assert!(!t.link_address_inside("203.0.113.2".parse().unwrap()));
        t.full = true;
        assert!(t.link_address_inside("203.0.113.2".parse().unwrap()));
    }

    #[test]
    fn a_full_tunnel_passes_and_round_trips() {
        let mut t = tunnel();
        assert_eq!(check_tunnel(&mut t, &host()), Ok(()));
        let lines = t.to_lines("home");
        assert_eq!(lines.len(), 1);
        assert_eq!(Tunnel::from_line(&lines[0], vec![]).unwrap(), t);
        assert_eq!(t.mtu, 1420);
    }

    #[test]
    fn refusals() {
        let check = |f: &dyn Fn(&mut Tunnel)| {
            let mut t = tunnel();
            f(&mut t);
            check_tunnel(&mut t, &host())
        };
        let n = |s: &str| Net4::parse(s).unwrap();
        assert_eq!(check(&|t| t.addrs = vec![]), Err(TunnelRefused::Address));
        assert_eq!(check(&|t| t.addrs = vec![n("127.0.0.2/8")]), Err(TunnelRefused::Address));
        assert_eq!(check(&|t| t.addrs = vec![n("100.64.3.2/24")]), Err(TunnelRefused::Address));
        assert_eq!(check(&|t| t.addrs = vec![n("10.88.0.9/32")]), Err(TunnelRefused::Overlap));
        assert_eq!(check(&|t| t.addrs = vec![n("10.0.0.2/8")]), Err(TunnelRefused::Overlap));
        assert_eq!(check(&|t| t.includes = vec![n("0.0.0.0/0")]), Err(TunnelRefused::Routes));
        assert_eq!(check(&|t| t.includes = vec![n("100.64.0.0/10")]), Err(TunnelRefused::Routes));
        assert_eq!(check(&|t| t.includes = vec![n("96.0.0.0/3")]), Err(TunnelRefused::Routes));
        assert_eq!(check(&|t| t.includes = vec![n("224.0.0.0/8")]), Err(TunnelRefused::Routes));
        assert_eq!(check(&|t| t.includes = vec![n("169.254.0.0/24")]), Err(TunnelRefused::Routes));
        assert_eq!(check(&|t| t.includes = vec![n("10.88.0.0/16")]), Err(TunnelRefused::Overlap));
        assert_eq!(check(&|t| t.includes = (0..=MAX_INCLUDES).map(|_| n("10.1.0.0/16")).collect()), Err(TunnelRefused::Routes));
        assert_eq!(check(&|t| t.gateway = Ipv4Addr::new(10, 99, 0, 9)), Err(TunnelRefused::Gateway));
        assert_eq!(check(&|t| { t.full = false; t.block = false; t.includes = vec![n("198.51.100.0/24")]; t.dns = vec![]; t.domains = vec![] }), Err(TunnelRefused::Gateway));
        assert_eq!(check(&|t| t.port = 0), Err(TunnelRefused::Gateway));
        assert_eq!(check(&|t| t.dns = vec![Ipv4Addr::new(10, 99, 0, 1); 2]), Err(TunnelRefused::Dns));
        assert_eq!(check(&|t| t.dns = vec![Ipv4Addr::new(224, 0, 0, 1)]), Err(TunnelRefused::Dns));
        // A split tunnel's DNS server must be inside it.
        assert_eq!(check(&|t| { t.full = false; t.block = false; t.dns = vec![Ipv4Addr::new(8, 8, 8, 8)] }), Err(TunnelRefused::Dns));
        assert_eq!(check(&|t| { t.full = false; t.block = false; t.includes = vec![n("10.100.0.0/16")]; t.dns = vec![Ipv4Addr::new(10, 100, 0, 53)] }), Ok(()));
        assert_eq!(check(&|t| t.domains = vec!["localhost".into()]), Err(TunnelRefused::Domains));
        assert_eq!(check(&|t| t.domains = vec!["printer.local".into()]), Err(TunnelRefused::Domains));
        assert_eq!(check(&|t| t.domains = vec!["Bad_Name".into()]), Err(TunnelRefused::Domains));
        assert_eq!(check(&|t| t.dns = vec![]), Err(TunnelRefused::Domains));
        assert_eq!(check(&|t| t.full = false), Err(TunnelRefused::Shape));
        let mut t = tunnel();
        t.mtu = 9000;
        check_tunnel(&mut t, &host()).unwrap();
        assert_eq!(t.mtu, TUN_MTU_MAX);
    }

    #[test]
    fn globalprotect_allows_only_tagged_esp_to_its_gateway() {
        let gateway = Ipv4Addr::new(198, 51, 100, 52);
        let tunnel = AnchorTunnel {
            iface: "tun0", gateway, port: 443, tcp: true, gp_esp: true,
            full: true, block: true, rdr: None,
        };
        let rules = render_anchor(&[tunnel], Some("vtnet1"));
        let esp = "pass out quick on vtnet1 inet proto udp from any to 198.51.100.52 port 4501 tagged CRYPTA_OUTER keep state";
        let block = "block out quick on vtnet1 inet from any to 198.51.100.52";
        assert!(rules.find(esp).unwrap() < rules.find(block).unwrap());
    }

    #[test]
    fn the_anchor() {
        let t = AnchorTunnel { iface: "tun0", gateway: Ipv4Addr::new(198, 51, 100, 51), port: 51820, tcp: false, gp_esp: false, full: true, block: true, rdr: None };
        let a = render_anchor(&[t], Some("vtnet1"));
        let lines: Vec<&str> = a.lines().collect();
        assert_eq!(lines[0], "nat on tun0 inet from 100.64.0.0/16 to any -> (tun0)");
        assert_eq!(lines[1], "pass out quick on vtnet1 inet proto udp from any to 198.51.100.51 port 51820 tagged CRYPTA_OUTER keep state");
        assert_eq!(lines[2], "block out quick on vtnet1 inet from any to 198.51.100.51");
        assert_eq!(lines[3], "block in quick on tun0 all");
        assert_eq!(*lines.last().unwrap(), "block out quick on vtnet1 all");
        let split = AnchorTunnel { iface: "tun1", gateway: Ipv4Addr::new(198, 51, 100, 52), port: 51820, tcp: false, gp_esp: false, full: false, block: false, rdr: None };
        let a = render_anchor(&[split], Some("vtnet1"));
        assert!(!a.contains("block out quick on vtnet1 all"));
        // AnyConnect's engine: TCP to the gateway.
        let ac = AnchorTunnel { iface: "tun4", gateway: Ipv4Addr::new(198, 51, 100, 52), port: 443, tcp: true, gp_esp: false, full: false, block: false, rdr: None };
        assert!(render_anchor(&[ac], Some("vtnet1")).contains("pass out quick on vtnet1 inet proto tcp from any to 198.51.100.52 port 443 tagged CRYPTA_OUTER keep state"));
        assert!(!a.contains("inet6"));
        // A full tunnel without the block still keeps IPv6 from going around it.
        let open = AnchorTunnel { iface: "tun2", gateway: Ipv4Addr::new(198, 51, 100, 53), port: 51820, tcp: false, gp_esp: false, full: true, block: false, rdr: None };
        let a = render_anchor(&[open], None);
        assert!(a.contains("block out quick on atrium_egress inet6 all"));
        assert!(!a.contains("block out quick on atrium_egress all"));
        // A persona's tunnel: its apps' lookups go to its DNS, and it is
        // neither full nor blocking for the machine.
        let work = AnchorTunnel { iface: "tun3", gateway: Ipv4Addr::new(198, 51, 100, 54), port: 51820, tcp: false, gp_esp: false, full: false, block: false, rdr: Some((2, Ipv4Addr::new(10, 99, 0, 1))) };
        let a = render_anchor(&[work], Some("vtnet1"));
        assert_eq!(a.lines().next().unwrap(), "rdr pass on atrium inet proto { udp tcp } from <atrium_fib_2> to any port 53 -> 10.99.0.1 port 53");
        assert!(a.contains("from (tun3) to 10.99.0.1 port 53 user root keep state (if-bound)"));
        assert!(a.contains("block out quick on ! tun3 inet proto udp from (tun3) to 10.99.0.1 port 53 user root"));
        assert!(!a.contains("block out quick on vtnet1 all"));
        // AnyConnect's engine: TCP to the gateway.
        let ac = AnchorTunnel { iface: "tun4", gateway: Ipv4Addr::new(198, 51, 100, 52), port: 443, tcp: true, gp_esp: false, full: false, block: false, rdr: None };
        assert!(render_anchor(&[ac], Some("vtnet1")).contains("pass out quick on vtnet1 inet proto tcp from any to 198.51.100.52 port 443 tagged CRYPTA_OUTER keep state"));
    }
}
