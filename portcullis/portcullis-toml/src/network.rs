//! The `network` capability: a mode string, or a table of finer grants
//! (docs/spec/network.md §0.1).
//!
//! ```toml
//! network = "full"                      # or "none", "loopback"
//!
//! [capabilities.network]                # grants LESS than "full"
//! outbound = ["github.com:443", "1.1.1.1:53/udp", "10.0.0.0/8"]   # or "any"
//! peers    = ["org.atrium.db:5432"]
//! inbound  = [8080]
//! listen   = [{ port = 8080, proto = "tcp", from = "local-network" }]
//! ```
//!
//! `listen` asks for reach from outside the app's own stack
//! (background-apps.md §4): the person still has to allow it, per network.
//!
//! ★ What cannot be enforced yet is REFUSED at parse, never ignored: an app
//! asking for `lan_alias`, `expose` or `mdns` fails to load with the reason,
//! rather than running believing it has them.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::schema::NetworkCap;

/// The `network` capability as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkSpec {
    /// `network = "none" | "loopback" | "full"`.
    Mode(NetworkCap),
    /// `[capabilities.network]` — an own routed stack with only these grants.
    Grants(NetworkGrants),
}

impl NetworkSpec {
    /// The stack the app gets. The table form is a routed stack (`Full`)
    /// whose reach is then narrowed by its grants.
    pub fn mode(&self) -> NetworkCap {
        match self {
            NetworkSpec::Mode(m) => *m,
            NetworkSpec::Grants(_) => NetworkCap::Full,
        }
    }

    /// The finer grants, if any. `Mode(Full)` has none: it is outbound "any".
    pub fn grants(&self) -> Option<&NetworkGrants> {
        match self { NetworkSpec::Grants(g) => Some(g), _ => None }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NetworkGrants {
    pub outbound: Outbound,
    /// Apps this one may dial — honoured only where the target lists the
    /// port in its own `inbound` (mutual consent).
    pub peers:    Vec<Peer>,
    /// Ports other apps may dial on this one.
    pub inbound:  Vec<u16>,
    /// Ports the local network or this computer may reach — a request only;
    /// nothing forwards until the person allows it (background-apps.md §4.2).
    pub listen:   Vec<Listen>,
}

/// At most this many `listen` entries per app (background-apps.md §4.1).
pub const MAX_LISTEN: usize = 4;

/// Who a `listen` port is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenFrom {
    /// Only the person's own processes on this machine.
    ThisComputer,
    /// Devices on a network the person allowed it on.
    LocalNetwork,
}

impl ListenFrom {
    pub fn as_str(self) -> &'static str {
        match self { ListenFrom::ThisComputer => "this-computer", ListenFrom::LocalNetwork => "local-network" }
    }
}

/// One `listen` entry: `{ port = 8080, proto = "tcp", from = "local-network" }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Listen {
    /// 1024-65535: nothing below 1024 in v1 (background-apps.md §8).
    pub port:  u16,
    pub proto: Proto,
    pub from:  ListenFrom,
}

impl Listen {
    fn parse(v: &toml::Value) -> Result<Self, String> {
        let t = v.as_table().ok_or("network.listen entries are tables: { port, proto, from }")?;
        let mut port = None;
        let mut proto = Proto::Tcp;
        let mut from = None;
        for (k, v) in t {
            match k.as_str() {
                "port" => port = Some(v.as_integer().filter(|p| (1024..=65535).contains(p))
                    .ok_or("network.listen port must be 1024-65535 (below 1024 is not offered)")? as u16),
                "proto" => proto = match v.as_str() {
                    Some("tcp") => Proto::Tcp,
                    Some("udp") => Proto::Udp,
                    _ => return Err("network.listen proto is \"tcp\" or \"udp\"".into()),
                },
                "from" => from = Some(match v.as_str() {
                    Some("this-computer") => ListenFrom::ThisComputer,
                    Some("local-network") => ListenFrom::LocalNetwork,
                    _ => return Err("network.listen from is \"this-computer\" or \"local-network\" \
                                     (there is no internet reach)".into()),
                }),
                other => return Err(format!("network.listen: unknown key {other:?} (port, proto, from)")),
            }
        }
        Ok(Listen {
            port: port.ok_or("network.listen entry has no port")?,
            proto,
            from: from.ok_or("network.listen entry has no from")?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outbound {
    /// Anywhere (still never the host, never another app).
    Any,
    /// Only these destinations. Empty = no outbound at all.
    List(Vec<Dest>),
}

impl Default for Outbound {
    fn default() -> Self { Outbound::List(Vec::new()) }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proto { Tcp, Udp }

/// One outbound destination: `host[:port][/tcp|/udp]` or `cidr[:port][/proto]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dest {
    /// A hostname, an IPv4 address, or an IPv4 CIDR. Hostnames are resolved
    /// by the launcher at launch.
    pub host:  String,
    /// `None` = every port.
    pub port:  Option<u16>,
    pub proto: Proto,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub app_id: String,
    pub port:   u16,
}

impl Dest {
    pub fn parse(s: &str) -> Result<Self, String> {
        let (rest, proto) = match s.rsplit_once('/') {
            Some((r, "tcp")) => (r, Proto::Tcp),
            Some((r, "udp")) => (r, Proto::Udp),
            _ => (s, Proto::Tcp),
        };
        if rest.matches(':').count() > 1 {
            return Err(format!("{s:?}: IPv6 destinations are not supported yet"));
        }
        let (host, port) = match rest.split_once(':') {
            Some((h, p)) => (h, Some(p.parse::<u16>().ok().filter(|p| *p != 0)
                .ok_or_else(|| format!("{s:?}: port {p:?} is not 1-65535"))?)),
            None => (rest, None),
        };
        if !valid_host(host) {
            return Err(format!("{s:?}: {host:?} is not a hostname, IPv4 address or IPv4 CIDR"));
        }
        Ok(Dest { host: host.to_string(), port, proto })
    }

    fn render(&self) -> String {
        let mut s = self.host.clone();
        if let Some(p) = self.port { s.push_str(&format!(":{p}")) }
        if self.proto == Proto::Udp { s.push_str("/udp") }
        s
    }
}

impl Peer {
    pub fn parse(s: &str) -> Result<Self, String> {
        let (id, port) = s.rsplit_once(':')
            .ok_or_else(|| format!("{s:?}: a peer is app-id:port"))?;
        let port = port.parse::<u16>().ok().filter(|p| *p != 0)
            .ok_or_else(|| format!("{s:?}: port {port:?} is not 1-65535"))?;
        let ok = !id.is_empty() && id.contains('.')
            && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_');
        if !ok {
            return Err(format!("{s:?}: {id:?} is not an app id — peers are named, never globbed"));
        }
        Ok(Peer { app_id: id.to_string(), port })
    }
}

/// hostname, IPv4, or IPv4/prefix.
fn valid_host(h: &str) -> bool {
    if let Some((ip, len)) = h.split_once('/') {
        return ip.parse::<std::net::Ipv4Addr>().is_ok()
            && len.parse::<u8>().map(|l| l <= 32).unwrap_or(false);
    }
    if h.parse::<std::net::Ipv4Addr>().is_ok() { return true }
    !h.is_empty() && h.len() <= 253 && !h.starts_with(['-', '.'])
        && h.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
}

/// Keys the table form refuses outright (§0.1) — they cannot be enforced yet.
const REFUSED: &[(&str, &str)] = &[
    ("lan_alias", "a LAN-visible address is not supported on per-app stacks yet"),
    ("expose",    "use `listen = [{ port, proto, from }]` instead (background-apps.md §4)"),
    ("mdns",      "mDNS participation is reserved for V2"),
];

impl<'de> Deserialize<'de> for NetworkSpec {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let v = toml::Value::deserialize(d)?;
        match v {
            toml::Value::String(s) => match s.as_str() {
                "none" => Ok(NetworkSpec::Mode(NetworkCap::None)),
                "loopback" => Ok(NetworkSpec::Mode(NetworkCap::Loopback)),
                "full" => Ok(NetworkSpec::Mode(NetworkCap::Full)),
                other => Err(D::Error::custom(format!(
                    "network = {other:?}: expected \"none\", \"loopback\", \"full\" or a table"))),
            },
            toml::Value::Table(t) => {
                let mut g = NetworkGrants::default();
                for (k, v) in &t {
                    if let Some((_, why)) = REFUSED.iter().find(|(r, _)| r == k) {
                        return Err(D::Error::custom(format!("network.{k}: {why}")));
                    }
                    match (k.as_str(), v) {
                        ("outbound", toml::Value::String(s)) if s == "any" => g.outbound = Outbound::Any,
                        ("outbound", toml::Value::Array(a)) => {
                            let mut list = Vec::new();
                            for e in a {
                                let s = e.as_str().ok_or_else(|| D::Error::custom(
                                    "network.outbound entries are strings"))?;
                                list.push(Dest::parse(s).map_err(D::Error::custom)?);
                            }
                            g.outbound = Outbound::List(list);
                        }
                        ("outbound", _) => return Err(D::Error::custom(
                            "network.outbound is \"any\" or a list of destinations")),
                        ("peers", toml::Value::Array(a)) => for e in a {
                            let s = e.as_str().ok_or_else(|| D::Error::custom(
                                "network.peers entries are \"app-id:port\" strings"))?;
                            g.peers.push(Peer::parse(s).map_err(D::Error::custom)?);
                        },
                        ("inbound", toml::Value::Array(a)) => for e in a {
                            let p = e.as_integer().filter(|p| (1..=65535).contains(p))
                                .ok_or_else(|| D::Error::custom("network.inbound entries are ports 1-65535"))?;
                            g.inbound.push(p as u16);
                        },
                        ("listen", toml::Value::Array(a)) => {
                            if a.len() > MAX_LISTEN {
                                return Err(D::Error::custom(format!(
                                    "network.listen: at most {MAX_LISTEN} entries")));
                            }
                            for e in a {
                                let l = Listen::parse(e).map_err(D::Error::custom)?;
                                if g.listen.iter().any(|o| o.port == l.port && o.proto == l.proto) {
                                    return Err(D::Error::custom(format!(
                                        "network.listen: port {} is listed twice", l.port)));
                                }
                                g.listen.push(l);
                            }
                        }
                        (k, _) => return Err(D::Error::custom(format!(
                            "network.{k}: unknown or malformed key (outbound, peers, inbound, listen)"))),
                    }
                }
                Ok(NetworkSpec::Grants(g))
            }
            _ => Err(D::Error::custom("network is a string or a table")),
        }
    }
}

impl Serialize for NetworkSpec {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            NetworkSpec::Mode(m) => m.serialize(s),
            NetworkSpec::Grants(g) => {
                use serde::ser::SerializeMap;
                let mut m = s.serialize_map(None)?;
                match &g.outbound {
                    Outbound::Any => m.serialize_entry("outbound", "any")?,
                    Outbound::List(l) => m.serialize_entry("outbound",
                        &l.iter().map(Dest::render).collect::<Vec<_>>())?,
                }
                m.serialize_entry("peers",
                    &g.peers.iter().map(|p| format!("{}:{}", p.app_id, p.port)).collect::<Vec<_>>())?;
                m.serialize_entry("inbound", &g.inbound)?;
                if !g.listen.is_empty() {
                    let l: Vec<toml::Value> = g.listen.iter().map(|l| {
                        let mut t = toml::map::Map::new();
                        t.insert("port".into(), toml::Value::Integer(l.port.into()));
                        t.insert("proto".into(), toml::Value::String(
                            if l.proto == Proto::Udp { "udp" } else { "tcp" }.into()));
                        t.insert("from".into(), toml::Value::String(l.from.as_str().into()));
                        toml::Value::Table(t)
                    }).collect();
                    m.serialize_entry("listen", &l)?;
                }
                m.end()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize, Serialize)]
    struct W { network: NetworkSpec }

    fn parse(t: &str) -> Result<NetworkSpec, String> {
        toml::from_str::<W>(t).map(|w| w.network).map_err(|e| e.to_string())
    }

    #[test]
    fn the_string_form_is_unchanged() {
        assert_eq!(parse(r#"network = "full""#).unwrap(), NetworkSpec::Mode(NetworkCap::Full));
        assert_eq!(parse(r#"network = "none""#).unwrap().mode(), NetworkCap::None);
        assert!(parse(r#"network = "everything""#).is_err());
    }

    #[test]
    fn the_table_form_parses_destinations_peers_and_inbound() {
        let n = parse(r#"
            [network]
            outbound = ["github.com:443", "1.1.1.1:53/udp", "10.0.0.0/8"]
            peers = ["org.atrium.db:5432"]
            inbound = [8080]
        "#).unwrap();
        assert_eq!(n.mode(), NetworkCap::Full);
        let g = n.grants().unwrap();
        let Outbound::List(l) = &g.outbound else { panic!("list") };
        assert_eq!(l[0], Dest { host: "github.com".into(), port: Some(443), proto: Proto::Tcp });
        assert_eq!(l[1], Dest { host: "1.1.1.1".into(), port: Some(53), proto: Proto::Udp });
        assert_eq!(l[2], Dest { host: "10.0.0.0/8".into(), port: None, proto: Proto::Tcp });
        assert_eq!(g.peers, vec![Peer { app_id: "org.atrium.db".into(), port: 5432 }]);
        assert_eq!(g.inbound, vec![8080]);
    }

    #[test]
    fn outbound_any_and_an_empty_table_mean_what_they_say() {
        assert_eq!(parse("[network]\noutbound = \"any\"").unwrap().grants().unwrap().outbound, Outbound::Any);
        // A table with no outbound grants NO outbound — the default is deny.
        assert_eq!(parse("[network]\ninbound = [80]").unwrap().grants().unwrap().outbound,
                   Outbound::List(vec![]));
    }

    /// ★ Refused, never ignored.
    #[test]
    fn unenforceable_keys_are_refused_with_the_reason() {
        for (k, v) in [("lan_alias", "true"), ("expose", "[]"), ("mdns", "true")] {
            let e = parse(&format!("[network]\n{k} = {v}")).unwrap_err();
            assert!(e.contains(k), "{e}");
        }
        assert!(parse("[network]\nexpose = []").unwrap_err().contains("listen"));
        assert!(parse("[network]\nwhatever = 1").unwrap_err().contains("unknown"));
    }

    #[test]
    fn malformed_entries_are_refused() {
        for bad in [r#"["github.com:0"]"#, r#"["github.com:99999"]"#, r#"["::1:443"]"#,
                    r#"["-bad.example"]"#, r#"["10.0.0.0/40"]"#] {
            assert!(parse(&format!("[network]\noutbound = {bad}")).is_err(), "{bad}");
        }
        for bad in [r#"["dev-*:22"]"#, r#"["org.x.db"]"#, r#"["nodot:22"]"#] {
            assert!(parse(&format!("[network]\npeers = {bad}")).is_err(), "{bad}");
        }
        assert!(parse("[network]\ninbound = [0]").is_err());
    }

    #[test]
    fn listen_parses_and_refuses_what_v1_does_not_offer() {
        let g = parse(r#"
            [network]
            listen = [{ port = 8080, proto = "tcp", from = "local-network" },
                      { port = 5353, proto = "udp", from = "this-computer" }]
        "#).unwrap();
        assert_eq!(g.grants().unwrap().listen, vec![
            Listen { port: 8080, proto: Proto::Tcp, from: ListenFrom::LocalNetwork },
            Listen { port: 5353, proto: Proto::Udp, from: ListenFrom::ThisComputer },
        ]);
        for (bad, why) in [
            (r#"[{ port = 80, from = "local-network" }]"#, "1024"),
            (r#"[{ port = 8080, from = "internet" }]"#, "internet"),
            (r#"[{ port = 8080 }]"#, "no from"),
            (r#"[{ from = "this-computer" }]"#, "no port"),
            (r#"[{ port = 8080, from = "this-computer", proto = "sctp" }]"#, "proto"),
            (r#"[{ port = 8080, from = "this-computer", bind = "0.0.0.0" }]"#, "unknown key"),
            (r#"[8080]"#, "tables"),
            (r#"[{ port = 8080, from = "this-computer" }, { port = 8080, from = "local-network" }]"#, "twice"),
            (r#"[{ port = 2001, from = "this-computer" }, { port = 2002, from = "this-computer" },
                 { port = 2003, from = "this-computer" }, { port = 2004, from = "this-computer" },
                 { port = 2005, from = "this-computer" }]"#, "at most 4"),
        ] {
            let e = parse(&format!("[network]\nlisten = {bad}")).unwrap_err();
            assert!(e.contains(why), "{bad}: {e}");
        }
    }

    /// Serialize → parse is a fixed point (the manifest round-trips).
    #[test]
    fn serialize_then_parse_is_a_fixed_point() {
        let src = parse(r#"
            [network]
            outbound = ["github.com:443", "1.1.1.1:53/udp", "10.0.0.0/8"]
            peers = ["org.atrium.db:5432"]
            inbound = [8080]
            listen = [{ port = 8080, proto = "tcp", from = "local-network" }]
        "#).unwrap();
        let text = toml::to_string(&W { network: src.clone() }).unwrap();
        assert_eq!(parse(&text).unwrap(), src, "{text}");
    }
}
