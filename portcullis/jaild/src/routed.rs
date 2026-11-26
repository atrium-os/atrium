//! A networked jail's own stack: a point-to-point epair (network.md §0).
//!
//! Every jail granted network gets `vnet=new` plus one end of an epair, on a
//! /30 of its own from `100.64.0.0/16`: host end `.1` (in interface group
//! `atrium`), app end `.2` with a per-app derived MAC, default route via `.1`.
//! Point-to-point rather than a bridge, so jails share no layer 2 and all
//! traffic between them is routed through the host, where pf sees it.
//!
//! ★★ pf's base rules are what keep a jail off the host and off other jails —
//! measured: without them a jail reached the host's sshd through its gateway.
//! So [`isolation_loaded`] is checked before any networked jail is created, the
//! same fail-closed shape as the devfs rulesets (portcullis.md §9.1b).
//!
//! Shelled (`ifconfig`, `route`, `pfctl`) in the audited style this crate
//! already uses for `ifconfig` aliases and `rctl`: the interface ioctls are
//! stable but wide, and the jail-scoped forms (`-j`) do the vnet switch for us.

use std::io;
use std::process::Command;

/// /30 slots in 100.64.0.0/16.
pub const SLOTS: u32 = 16384;

/// Host-end and app-end addresses of `slot`.
/// The /30's network address.
pub fn slot_net(slot: u32) -> String {
    let base = slot * 4;
    format!("100.64.{}.{}", base / 256, base % 256)
}

pub fn slot_addrs(slot: u32) -> (String, String) {
    let base = slot * 4;
    let (o3, o4) = (base / 256, base % 256);
    (format!("100.64.{o3}.{}", o4 + 1), format!("100.64.{o3}.{}", o4 + 2))
}

/// Whether a MAC is a plain locally-administered unicast address — the only
/// kind a derived identity produces (portcullis-identity).
pub fn valid_mac(mac: &str) -> bool {
    let parts: Vec<&str> = mac.split(':').collect();
    if parts.len() != 6 || !parts.iter().all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())) {
        return false;
    }
    let b0 = u8::from_str_radix(parts[0], 16).unwrap_or(1);
    b0 & 0x01 == 0 && b0 & 0x02 == 0x02
}

fn sh(cmd: &str, args: &[&str]) -> io::Result<String> {
    let out = Command::new(cmd).args(args).output()?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    Err(io::Error::other(format!("{cmd} {}: {}", args.join(" "),
        String::from_utf8_lossy(&out.stderr).trim())))
}

/// ★★ The isolation a networked jail depends on is in force: pf enabled, both
/// block rules on group `atrium` loaded, and forwarding on (without it the jail
/// has no route out — not a leak, but a jail that silently cannot work).
pub fn isolation_loaded() -> Result<(), String> {
    let info = sh("pfctl", &["-s", "info"]).map_err(|e| format!("pf: {e}"))?;
    if !info.lines().any(|l| l.trim_start().starts_with("Status: Enabled")) {
        return Err("pf is not enabled".into());
    }
    let rules = sh("pfctl", &["-s", "rules"]).map_err(|e| format!("pf rules: {e}"))?;
    // Base order (network.md §0.1): block app->host, the per-app anchors, then
    // block everything else from apps. All three must be there.
    let host_block = rules.lines().any(|l| l.contains("block drop in quick on atrium") && l.contains("to (self)"));
    let anchors = rules.lines().any(|l| l.trim_start().starts_with("anchor \"atrium/*\""));
    let final_block = rules.lines().any(|l| l.trim() == "block drop in quick on atrium all");
    if !(host_block && anchors && final_block) {
        return Err("pf is missing the atrium isolation rules (app->host block, per-app anchors, \
                    final app block); load etc/atrium.pf".into());
    }
    let fwd = sh("sysctl", &["-n", "net.inet.ip.forwarding"]).map_err(|e| e.to_string())?;
    if fwd.trim() != "1" {
        return Err("net.inet.ip.forwarding is 0; a networked jail would have no route out".into());
    }
    Ok(())
}

/// Create the epair and configure the host end. Returns (a, b).
///
/// With `fib`, the host end is in that routing table: ★ set BEFORE the
/// address, so the /30's routes are made there (measured on 16-CURRENT:
/// moving an addressed interface to another FIB left a dead route to the
/// /30 in FIB 0 after the epair was destroyed, and the address could not be
/// added again). Then a route to the /30 in FIB 0 as well, through the
/// epair, so replies to the app (un-NATed on the egress or the tunnel, both
/// in FIB 0) find their way back. Both go with the epair.
pub fn create_host_end(slot: u32, fib: Option<u32>) -> io::Result<(String, String)> {
    let a = sh("ifconfig", &["epair", "create"])?.trim().to_string();
    let b = format!("{}b", a.strip_suffix('a').unwrap_or(&a));
    let (host, _) = slot_addrs(slot);
    let made = (|| {
        if let Some(f) = fib {
            sh("ifconfig", &[&a, "fib", &f.to_string()])?;
        }
        sh("ifconfig", &[&a, "inet", &format!("{host}/30"), "up", "group", "atrium"])?;
        if fib.is_some() {
            let net = slot_net(slot);
            sh("route", &["-q", "-n", "add", "-net", &format!("{net}/30"), "-iface", &a])?;
        }
        Ok(())
    })();
    if let Err(e) = made {
        let _ = destroy(&a);
        return Err(e);
    }
    Ok((a, b))
}

/// The pf table of route domain `fib`'s app addresses: atrium-net's
/// `atrium/vpn` redirects their DNS to the domain's tunnel (vpn.md §5.5).
pub fn domain_table(fib: u32) -> String {
    format!("atrium_fib_{fib}")
}

/// Put slot `slot`'s app address in its route domain's table. Returns the
/// table, which [`release_grants`] empties of it again.
pub fn join_domain(slot: u32, fib: u32) -> io::Result<String> {
    let (_, app) = slot_addrs(slot);
    let t = domain_table(fib);
    sh("pfctl", &["-t", &t, "-T", "add", &app])?;
    Ok(t)
}

/// Whether `fib` names a routing table a jail may be put in: 1..=MAX_FIB,
/// and one the kernel has.
pub fn valid_fib(fib: u32) -> Result<(), String> {
    if fib == 0 || fib > crate::protocol::MAX_FIB {
        return Err(format!("fib {fib} is not 1..={}", crate::protocol::MAX_FIB));
    }
    let have: u32 = sh("sysctl", &["-n", "net.fibs"]).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(1);
    if fib >= have {
        return Err(format!("fib {fib}: the kernel has {have} (atrium-net raises net.fibs)"));
    }
    Ok(())
}

/// Move `b` into the jail's vnet and configure the app end: the derived MAC,
/// the /30 address, its own loopback, the default route. Done before anything
/// runs in the jail.
pub fn configure_app_end(jail: &str, b: &str, mac: &str, slot: u32) -> io::Result<()> {
    let (host, app) = slot_addrs(slot);
    sh("ifconfig", &[b, "vnet", jail])?;
    sh("ifconfig", &["-j", jail, b, "ether", mac])?;
    sh("ifconfig", &["-j", jail, b, "inet", &format!("{app}/30"), "up"])?;
    sh("ifconfig", &["-j", jail, "lo0", "inet", "127.0.0.1/8", "up"])?;
    sh("route", &["-q", "-j", jail, "add", "default", &host])?;
    // ★ A 1 s MSL in the APP's stack only (net.inet.tcp.msl is per-vnet; the
    // host's stays 30 s). The stack is destroyed when the app exits, so TIME_WAIT
    // after that protects nothing — and while it lasts it keeps the jail dying
    // and its root pinned: 2×30 s, measured. 2×1 s bounds that.
    sh("sysctl", &["-j", jail, "net.inet.tcp.msl=1000"])?;
    Ok(())
}

/// Bring up a jail's OWN loopback (127.0.0.1/8 and ::1) — the whole network
/// of a `loopback`-capability jail. Short MSL for the same reason as a routed
/// stack: its TIME_WAIT would only keep the dying jail pinned.
pub fn configure_loopback(jail: &str) -> io::Result<()> {
    sh("ifconfig", &["-j", jail, "lo0", "inet", "127.0.0.1/8", "up"])?;
    sh("ifconfig", &["-j", jail, "lo0", "inet6", "::1/128"])?;
    sh("sysctl", &["-j", jail, "net.inet.tcp.msl=1000"])?;
    Ok(())
}

/// Whether a consent key is 16 lowercase hex (portcullis_identity::consent_key).
fn valid_key(k: &str) -> bool {
    k.len() == 16 && k.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

fn valid_cidr(c: &str) -> bool {
    let Some((ip, len)) = c.split_once('/') else { return false };
    ip.parse::<std::net::Ipv4Addr>().is_ok() && len.parse::<u8>().map(|l| l <= 32).unwrap_or(false)
}

/// Shape-check grants before anything is rendered. What they may REACH is
/// bounded by the anchor's structure (below), not by trusting these values.
pub fn validate_grants(g: &crate::protocol::NetGrants) -> Result<(), String> {
    if !valid_key(&g.app_key) { return Err(format!("app_key {:?} is not 16 lowercase hex", g.app_key)) }
    for d in &g.outbound {
        if !valid_cidr(&d.cidr) { return Err(format!("outbound {:?} is not an IPv4 CIDR", d.cidr)) }
        if d.port == Some(0) { return Err("outbound port 0".into()) }
    }
    for p in &g.peers {
        if !valid_key(&p.app_key) || p.port == 0 { return Err(format!("peer {p:?} is malformed")) }
    }
    if g.inbound.contains(&0) { return Err("inbound port 0".into()) }
    validate_listen(&g.listen)?;
    if let Some(t) = &g.tag {
        if t.is_empty() || t.len() > 31 || !t.bytes().all(|b| b.is_ascii_uppercase() || b == b'_') {
            return Err(format!("tag {t:?} is not 1-31 of A-Z and _"));
        }
    }
    Ok(())
}

/// The most ports one app may serve (portcullis-toml's `MAX_LISTEN`).
pub const MAX_LISTEN: usize = 4;

/// An interface name pf and ifconfig take: a letter, then letters and
/// digits, at most 15 (IFNAMSIZ - 1).
fn valid_iface(i: &str) -> bool {
    !i.is_empty() && i.len() <= 15 && i.as_bytes()[0].is_ascii_lowercase()
        && i.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// A person's login name as it reaches the registry: plain and short.
fn valid_owner(o: &str) -> bool {
    !o.is_empty() && o.len() <= 32
        && o.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
}

pub fn validate_listen(l: &[crate::protocol::NetListen]) -> Result<(), String> {
    use crate::protocol::ListenFrom;
    if l.len() > MAX_LISTEN {
        return Err(format!("{} listen ports; at most {MAX_LISTEN}", l.len()));
    }
    for (i, x) in l.iter().enumerate() {
        if x.port < 1024 {
            return Err(format!("listen port {} is below 1024", x.port));
        }
        if l[..i].iter().any(|y| y.port == x.port && y.udp == x.udp) {
            return Err(format!("listen port {} twice", x.port));
        }
        match &x.from {
            ListenFrom::Lan { iface, subnet } => {
                if !valid_iface(iface) {
                    return Err(format!("listen iface {iface:?} is not an interface name"));
                }
                // A network's own subnet, never "anywhere".
                let len = subnet.split_once('/').and_then(|(_, n)| n.parse::<u8>().ok());
                if !valid_cidr(subnet) || len.is_none_or(|n| n < 8) {
                    return Err(format!("listen subnet {subnet:?} is not an IPv4 network of /8 or narrower"));
                }
            }
            ListenFrom::ThisComputer { owner_uid, owner } => {
                if x.udp {
                    return Err("listen from this computer is TCP only".into());
                }
                if *owner_uid == 0 || !valid_owner(owner) {
                    return Err(format!("listen owner {owner:?} ({owner_uid}) is not a person"));
                }
            }
        }
    }
    Ok(())
}

/// The consent table for (app, inbound port). pf table names are at most 31
/// characters: `atrium_p_` + 16 hex + `_` + port fits.
pub fn consent_table(app_key: &str, port: u16) -> String {
    format!("atrium_p_{app_key}_{port}")
}

/// The resolvers an app with a restricted outbound gets automatically (port
/// 53, udp + tcp) — read from the HOST's resolv.conf, never from the caller.
fn host_resolvers() -> Vec<String> {
    std::fs::read_to_string("/etc/resolv.conf").unwrap_or_default().lines()
        .filter_map(|l| l.strip_prefix("nameserver"))
        .map(|a| a.trim().to_string())
        .filter(|a| a.parse::<std::net::Ipv4Addr>().is_ok())
        .collect()
}

/// Render the app's anchor. ★ Its STRUCTURE is what bounds it, whatever the
/// grants say: peers can only name consent tables; then every other packet
/// to 100.64.0.0/16 (another app) is blocked; only then do outbound passes
/// apply — so even `0.0.0.0/0` cannot reach an app. The host is blocked by the
/// base ruleset before any anchor is evaluated. (Proven in the VM with a
/// listening peer: consented port allowed, unconsented port blocked while
/// 0/0 outbound worked.)
pub fn render_anchor(ea: &str, app: &str, g: Option<&crate::protocol::NetGrants>) -> String {
    let mut r = String::new();
    // Translation first (pf wants it before filtering): devices on an
    // allowed network reach the app's port on the host's own address there.
    // `rdr`, not `rdr pass`, so the pass below still applies.
    let listen = g.map(|g| g.listen.as_slice()).unwrap_or_default();
    for l in listen {
        if let crate::protocol::ListenFrom::Lan { iface, subnet } = &l.from {
            let p = if l.udp { "udp" } else { "tcp" };
            r.push_str(&format!("rdr on {iface} inet proto {p} from {subnet} to ({iface}) port {port} -> {app} port {port}\n",
                port = l.port));
        }
    }
    let tag = g.and_then(|g| g.tag.as_deref()).map(|t| format!(" tag {t}")).unwrap_or_default();
    let pass = |proto: &str, to: &str, port: Option<u16>| {
        let proto = if proto.is_empty() { String::new() } else { format!(" proto {proto}") };
        let port = port.map(|p| format!(" port {p}")).unwrap_or_default();
        format!("pass in quick on {ea} inet{proto} from {app} to {to}{port}{tag} keep state\n")
    };
    if let Some(g) = g {
        for p in &g.peers {
            r.push_str(&pass("tcp", &format!("<{}>", consent_table(&p.app_key, p.port)), Some(p.port)));
        }
    }
    r.push_str(&format!("block in quick on {ea} inet from any to 100.64.0.0/16\n"));
    match g {
        None => r.push_str(&pass("", "any", None)),
        Some(g) if g.outbound_any => r.push_str(&pass("", "any", None)),
        Some(g) => {
            for d in &g.outbound {
                r.push_str(&pass(if d.udp { "udp" } else { "tcp" }, &d.cidr, d.port));
            }
            if !g.outbound.is_empty() && !g.no_resolver {
                for ns in host_resolvers() {
                    r.push_str(&pass("udp", &format!("{ns}/32"), Some(53)));
                    r.push_str(&pass("tcp", &format!("{ns}/32"), Some(53)));
                }
            }
        }
    }
    for l in listen {
        match &l.from {
            crate::protocol::ListenFrom::Lan { iface, subnet } => {
                let port = l.port;
                if l.udp {
                    r.push_str(&format!("pass in quick on {iface} inet proto udp from {subnet} to {app} port {port} keep state (max 256)\n"));
                } else {
                    r.push_str(&format!("pass in quick on {iface} inet proto tcp from {subnet} to {app} port {port} flags S/SA \
                                         keep state (max-src-conn 16, max-src-conn-rate 30/10, max 256)\n"));
                }
            }
            // From this computer, only jaild's own forwarder (root) may
            // reach the port; any other local process is refused here.
            crate::protocol::ListenFrom::ThisComputer { .. } => {
                let port = l.port;
                r.push_str(&format!("pass out quick on {ea} inet proto tcp from (self) to {app} port {port} user root keep state\n"));
                r.push_str(&format!("block out quick on {ea} inet proto tcp from (self) to {app} port {port}\n"));
            }
        }
    }
    r
}

/// Load the app's anchor and publish its address in its consent tables.
/// Returns the tables written, for release.
pub fn apply_grants(jail: &str, ea: &str, slot: u32, g: Option<&crate::protocol::NetGrants>)
    -> io::Result<Vec<String>>
{
    load_anchor(jail, ea, slot, g)?;
    let (_, app) = slot_addrs(slot);
    let mut tables = Vec::new();
    if let Some(g) = g {
        for p in &g.inbound {
            let t = consent_table(&g.app_key, *p);
            sh("pfctl", &["-t", &t, "-T", "add", &app])?;
            tables.push(t);
        }
        crate::listen::start(jail, &app, &g.listen);
    }
    Ok(tables)
}

/// Load (or replace) the app's anchor from `g`. pfctl -f swaps the anchor's
/// rules in one transaction.
pub fn load_anchor(jail: &str, ea: &str, slot: u32, g: Option<&crate::protocol::NetGrants>) -> io::Result<()> {
    let (_, app) = slot_addrs(slot);
    let text = render_anchor(ea, &app, g);
    let mut child = Command::new("pfctl").args(["-a", &format!("atrium/{jail}"), "-f", "-"])
        .stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped()).spawn()?;
    {
        use std::io::Write;
        child.stdin.take().expect("piped").write_all(text.as_bytes())?;
    }
    let out = child.wait_with_output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!("pfctl anchor atrium/{jail}: {}",
            String::from_utf8_lossy(&out.stderr).trim())));
    }
    Ok(())
}

/// Kill the states of flows TO the app from anywhere (a dropped listen
/// rule must not leave its connections open). The app's own outbound
/// states are keyed from its address and stay.
pub fn kill_inbound_states(slot: u32) {
    let (_, app) = slot_addrs(slot);
    let _ = sh("pfctl", &["-k", "0.0.0.0/0", "-k", &app]);
}

/// pf's established flows to `app` on `port` from a network (`pfctl -ss`
/// lines such as `all tcp 100.64.0.2:8080 (192.168.77.1:8080) <- 192.168.77.2:5012 ESTABLISHED:ESTABLISHED`).
pub fn count_states(states: &str, app: &str, port: u16) -> usize {
    let to = format!(" {app}:{port} (");
    states.lines().filter(|l| l.contains(&to) && l.contains(" <- ") && l.contains("ESTABLISHED:ESTABLISHED")).count()
}

pub fn lan_connections(slot: u32, ports: &[u16]) -> usize {
    if ports.is_empty() {
        return 0;
    }
    let (_, app) = slot_addrs(slot);
    let Ok(s) = sh("pfctl", &["-ss"]) else { return 0 };
    ports.iter().map(|p| count_states(&s, &app, *p)).sum()
}

/// Whether the base ruleset evaluates the per-app anchors' translation
/// rules (`rdr-anchor "atrium/*"`): without it a `Lan` listen would be a
/// pass with no redirect to use it.
pub fn listen_loaded() -> Result<(), String> {
    let nat = sh("pfctl", &["-s", "nat"]).map_err(|e| format!("pfctl -s nat: {e}"))?;
    if nat.lines().any(|l| l.trim() == "rdr-anchor \"atrium/*\" all") {
        Ok(())
    } else {
        Err("pf has no rdr-anchor \"atrium/*\"; load etc/atrium.pf".into())
    }
}

/// Undo [`apply_grants`]: withdraw the address from every consent table,
/// flush the anchor, and KILL the app's pf states.
///
/// ★ States, because a flushed anchor is not freed while any state created by
/// its rules lives — measured: an app's anchor outlived it by exactly its
/// states' ~90 s TCP close timeouts. More importantly, the /30 goes back to
/// the pool, and states keyed on the old app's address would otherwise match
/// the next app given that address. Best-effort; the jail is gone either way.
pub fn release_grants(jail: &str, slot: u32, tables: &[String]) {
    crate::listen::stop(jail);
    let (_, app) = slot_addrs(slot);
    for t in tables {
        let _ = sh("pfctl", &["-t", t, "-T", "delete", &app]);
    }
    let _ = sh("pfctl", &["-a", &format!("atrium/{jail}"), "-F", "all"]);
    let _ = sh("pfctl", &["-k", &app]);                  // states FROM the app
    let _ = sh("pfctl", &["-k", "0.0.0.0/0", "-k", &app]); // states TO it
}

/// Destroy the pair (destroying either end destroys both).
pub fn destroy(a: &str) -> io::Result<()> {
    sh("ifconfig", &[a, "destroy"]).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_are_disjoint_slash30s() {
        assert_eq!(slot_addrs(0), ("100.64.0.1".into(), "100.64.0.2".into()));
        assert_eq!(slot_addrs(1), ("100.64.0.5".into(), "100.64.0.6".into()));
        assert_eq!(slot_addrs(64), ("100.64.1.1".into(), "100.64.1.2".into()));
        assert_eq!(slot_addrs(SLOTS - 1), ("100.64.255.253".into(), "100.64.255.254".into()));
    }

    fn grants() -> crate::protocol::NetGrants {
        crate::protocol::NetGrants {
            app_key: "0123456789abcdef".into(),
            outbound_any: false,
            outbound: vec![crate::protocol::NetDest { cidr: "140.82.112.3/32".into(), port: Some(443), udp: false }],
            peers: vec![crate::protocol::NetPeer { app_key: "fedcba9876543210".into(), port: 5432 }],
            inbound: vec![8080],
            no_resolver: false,
            tag: None,
            listen: Vec::new(),
        }
    }

    fn lan(port: u16) -> crate::protocol::NetListen {
        crate::protocol::NetListen { port, udp: false,
            from: crate::protocol::ListenFrom::Lan { iface: "vtnet0".into(), subnet: "192.168.1.0/24".into() } }
    }

    /// The redirect comes first and only from the allowed network's subnet;
    /// its pass is rate-limited; nothing else moves.
    #[test]
    fn a_lan_listen_is_an_rdr_from_its_subnet_and_a_limited_pass() {
        let mut g = grants(); g.listen = vec![lan(8080)];
        let a = render_anchor("epair3a", "100.64.0.14", Some(&g));
        let lines: Vec<&str> = a.lines().collect();
        assert_eq!(lines[0], "rdr on vtnet0 inet proto tcp from 192.168.1.0/24 to (vtnet0) port 8080 -> 100.64.0.14 port 8080");
        let last = lines.last().unwrap();
        assert!(last.starts_with("pass in quick on vtnet0 inet proto tcp from 192.168.1.0/24 to 100.64.0.14 port 8080 flags S/SA"), "{a}");
        assert!(last.contains("max-src-conn-rate"), "{a}");
        assert!(!a.contains("rdr pass"));
        let without = render_anchor("epair3a", "100.64.0.14", Some(&grants()));
        assert_eq!(a.lines().filter(|l| l.contains("epair3a")).collect::<Vec<_>>(),
                   without.lines().collect::<Vec<_>>(), "the app's own rules are unchanged");
    }

    #[test]
    fn established_lan_flows_are_counted_per_app_and_port() {
        let s = "all tcp 100.64.0.2:8080 (192.168.77.1:8080) <- 192.168.77.2:5012       ESTABLISHED:ESTABLISHED\n\
                 all tcp 100.64.0.2:8080 (192.168.77.1:8080) <- 192.168.77.2:5013       FIN_WAIT_2:FIN_WAIT_2\n\
                 all tcp 100.64.0.6:8080 (192.168.77.1:8080) <- 192.168.77.2:5014       ESTABLISHED:ESTABLISHED\n\
                 all tcp 100.64.0.2:51000 -> 140.82.112.3:443       ESTABLISHED:ESTABLISHED\n";
        assert_eq!(count_states(s, "100.64.0.2", 8080), 1);
        assert_eq!(count_states(s, "100.64.0.2", 443), 0, "its own outbound flows aren't connections to it");
    }

    #[test]
    fn this_computer_lets_only_the_forwarder_through() {
        let mut g = grants();
        g.listen = vec![crate::protocol::NetListen { port: 9000, udp: false,
            from: crate::protocol::ListenFrom::ThisComputer { owner_uid: 1077, owner: "alice".into() } }];
        let a = render_anchor("epair3a", "100.64.0.14", Some(&g));
        assert!(!a.contains("rdr"), "{a}");
        assert!(a.contains("pass out quick on epair3a inet proto tcp from (self) to 100.64.0.14 port 9000 user root keep state\n\
                            block out quick on epair3a inet proto tcp from (self) to 100.64.0.14 port 9000\n"), "{a}");
    }

    #[test]
    fn listen_values_are_tokens_or_refused() {
        let ok = |l: Vec<crate::protocol::NetListen>| { let mut g = grants(); g.listen = l; validate_grants(&g) };
        assert!(ok(vec![lan(8080)]).is_ok());
        assert!(ok(vec![lan(80)]).is_err(), "low port");
        assert!(ok(vec![lan(8080), lan(8080)]).is_err(), "twice");
        assert!(ok(vec![lan(8080), lan(8081), lan(8082), lan(8083), lan(8084)]).is_err(), "too many");
        let mut bad = lan(8080);
        for (iface, subnet) in [("vtnet0; pass", "192.168.1.0/24"), ("vtnet0", "0.0.0.0/0"),
                                ("vtnet0", "192.168.1.0/24 port 22"), ("", "10.0.0.0/8"), ("Vtnet0", "10.0.0.0/8")] {
            bad.from = crate::protocol::ListenFrom::Lan { iface: iface.into(), subnet: subnet.into() };
            assert!(ok(vec![bad.clone()]).is_err(), "{iface:?} {subnet:?}");
        }
        let me = |uid, owner: &str, udp| crate::protocol::NetListen { port: 9000, udp,
            from: crate::protocol::ListenFrom::ThisComputer { owner_uid: uid, owner: owner.into() } };
        assert!(ok(vec![me(1077, "alice", false)]).is_ok());
        assert!(ok(vec![me(0, "root", false)]).is_err());
        assert!(ok(vec![me(1077, "alice", true)]).is_err(), "udp");
        assert!(ok(vec![me(1077, "al ice", false)]).is_err());
    }

    /// ★ Structure, not trust: peers first (tables only), then the app->app
    /// block, then outbound — so no outbound grant can reach another app.
    #[test]
    fn the_anchor_puts_the_app_block_between_peers_and_outbound() {
        let a = render_anchor("epair3a", "100.64.0.14", Some(&grants()));
        let lines: Vec<&str> = a.lines().collect();
        assert!(lines[0].contains("to <atrium_p_fedcba9876543210_5432> port 5432"), "{a}");
        assert_eq!(lines[1], "block in quick on epair3a inet from any to 100.64.0.0/16");
        assert!(lines[2].contains("proto tcp from 100.64.0.14 to 140.82.112.3/32 port 443"), "{a}");
        assert!(a.lines().all(|l| l.contains("on epair3a")), "every rule is bound to this app's interface");
    }

    #[test]
    fn no_resolver_leaves_only_the_granted_destinations() {
        let mut g = grants(); g.peers.clear(); g.no_resolver = true;
        let a = render_anchor("epair3a", "100.64.0.14", Some(&g));
        assert_eq!(a, "block in quick on epair3a inet from any to 100.64.0.0/16\n\
                       pass in quick on epair3a inet proto tcp from 100.64.0.14 to 140.82.112.3/32 port 443 keep state\n");
    }

    #[test]
    fn a_tag_rides_on_every_outbound_pass() {
        let mut g = grants(); g.peers.clear(); g.no_resolver = true; g.tag = Some("CRYPTA_OUTER".into());
        g.outbound = vec![crate::protocol::NetDest { cidr: "198.51.100.51/32".into(), port: Some(51820), udp: true }];
        let a = render_anchor("epair3a", "100.64.0.14", Some(&g));
        assert_eq!(a, "block in quick on epair3a inet from any to 100.64.0.0/16\n\
                       pass in quick on epair3a inet proto udp from 100.64.0.14 to 198.51.100.51/32 port 51820 tag CRYPTA_OUTER keep state\n");
        g.tag = Some("bad tag".into());
        assert!(validate_grants(&g).is_err());
    }

    #[test]
    fn full_without_grants_is_outbound_any_behind_the_app_block() {
        let a = render_anchor("epair3a", "100.64.0.14", None);
        assert_eq!(a, "block in quick on epair3a inet from any to 100.64.0.0/16\n\
                       pass in quick on epair3a inet from 100.64.0.14 to any keep state\n");
    }

    #[test]
    fn malformed_grants_are_refused() {
        let mut g = grants(); g.app_key = "UPPER".into();
        assert!(validate_grants(&g).is_err());
        let mut g = grants(); g.outbound[0].cidr = "github.com".into();
        assert!(validate_grants(&g).is_err(), "names must be resolved by the launcher, not passed through");
        let mut g = grants(); g.peers[0].app_key = "x".into();
        assert!(validate_grants(&g).is_err());
        assert!(validate_grants(&grants()).is_ok());
    }

    #[test]
    fn consent_table_names_fit_pf() {
        assert!(consent_table("0123456789abcdef", 65535).len() <= 31);
    }

    #[test]
    fn only_locally_administered_unicast_macs_are_accepted() {
        assert!(valid_mac("02:a7:10:00:00:01"));
        assert!(!valid_mac("00:1b:21:12:34:56"), "a vendor (universally administered) MAC");
        // ★ QEMU's 52:54:00 prefix is itself LOCALLY administered, so this is a
        // SHAPE check only: it cannot tell a derived MAC from a VM's real one.
        // What keeps the real MAC out is that callers derive it from the
        // machine secret (portcullis-identity), never read it from a NIC.
        assert!(valid_mac("52:54:00:12:34:56"));
        assert!(!valid_mac("03:00:00:00:00:01"), "multicast");
        assert!(!valid_mac("02:A7:10:00:00:01"), "uppercase");
        assert!(!valid_mac("02:a7:10:00:00"), "short");
    }
}
