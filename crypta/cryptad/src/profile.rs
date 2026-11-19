//! Profiles (vpn.md §7.1): what cryptad keeps about a tunnel. No secrets:
//! the private key and the preshared key are Arca items, named here by id.
//!
//! On disk, `<state>/<name>.profile` (0600): one `profile` line, then the
//! allowed prefixes in `allowed` lines of at most 128.

use aura_ipc::Line;
use crypta_proto::conf::{Conf, Endpoint};
use crypta_proto::net::Prefix4;
use net_proto::tunnel::{Net4, Tunnel};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    /// Arca item ids: the static key, and the preshared key if any.
    pub key: [u8; 16],
    pub public: [u8; 32],
    pub psk: Option<[u8; 16]>,
    pub peer: [u8; 32],
    pub endpoint: SocketAddrV4,
    pub keepalive: Option<u16>,
    /// Our tunnel addresses, each with its prefix (wg-quick's `Address`).
    pub addrs: Vec<Prefix4>,
    pub allowed: Vec<Prefix4>,
    pub dns: Vec<Ipv4Addr>,
    pub domains: Vec<String>,
    pub mtu: Option<u16>,
    /// Block when not connected (§5.4). Default: on for a full tunnel.
    pub block: bool,
    pub insecure: bool,
    /// Persona scope (vpn.md §5.3, §9.7): only the instances of personas
    /// whose `vpn` names this profile use it, through its route domain;
    /// the rest of the machine never sees it. Off: machine scope.
    pub scoped: bool,
}

/// A profile name: what the user calls the tunnel, as a file name and an
/// Arca account. Lowercase letters, digits and `-`, 1 to 32.
pub fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 32 && !n.starts_with('-') && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn hexa<const N: usize>(l: &Line, k: &str) -> Option<[u8; N]> {
    l.hex(k)?.try_into().ok()
}

fn join<T: ToString>(v: &[T]) -> String {
    v.iter().map(T::to_string).collect::<Vec<_>>().join(",")
}

fn list<'a>(l: &'a Line, k: &str) -> impl Iterator<Item = &'a str> {
    l.get(k).unwrap_or("").split(',').filter(|s| !s.is_empty())
}

/// What makes an imported file unusable in V1, in words for the user.
#[derive(Debug, PartialEq, Eq)]
pub enum Unusable {
    NoEndpoint,
    /// A name, not an address: resolving it waits for cryptad's resolver.
    NamedEndpoint,
    V6Endpoint,
    NoV4Address,
    NoV4Routes,
}

impl Unusable {
    pub fn word(&self) -> &'static str {
        match self {
            Unusable::NoEndpoint => "no-endpoint",
            Unusable::NamedEndpoint => "named-endpoint",
            Unusable::V6Endpoint => "v6-endpoint",
            Unusable::NoV4Address => "no-v4-address",
            Unusable::NoV4Routes => "no-v4-routes",
        }
    }
}

impl Profile {
    /// From an imported file, once its keys are in Arca (`key`, `psk`).
    pub fn from_conf(name: &str, c: &Conf, key: [u8; 16], public: [u8; 32], psk: Option<[u8; 16]>) -> Result<Profile, Unusable> {
        let endpoint = match &c.endpoint {
            None => return Err(Unusable::NoEndpoint),
            Some(Endpoint::Host(..)) => return Err(Unusable::NamedEndpoint),
            Some(Endpoint::Addr(SocketAddr::V6(_))) => return Err(Unusable::V6Endpoint),
            Some(Endpoint::Addr(SocketAddr::V4(a))) => *a,
        };
        if c.addresses4.is_empty() {
            return Err(Unusable::NoV4Address);
        }
        if c.allowed4.is_empty() {
            return Err(Unusable::NoV4Routes);
        }
        let full = c.allowed4.iter().any(|p| p.len == 0);
        Ok(Profile {
            name: name.into(),
            key,
            public,
            psk,
            peer: c.peer_public,
            endpoint,
            keepalive: c.keepalive,
            addrs: c.addresses4.clone(),
            allowed: c.allowed4.clone(),
            dns: c.dns_servers.iter().filter_map(|a| match a { IpAddr::V4(a) => Some(*a), _ => None }).take(3).collect(),
            domains: c.dns_domains.clone(),
            mtu: c.mtu,
            block: full,
            insecure: false,
            scoped: false,
        })
    }

    /// From a `create` request (vpn.md §7.3, "generate a key"): what the
    /// server's admin gave, with a key Arca makes; `key` and `public` are
    /// filled in once it has. Checked as an imported file is.
    pub fn from_create(l: &Line) -> Result<Profile, &'static str> {
        let name = l.get("name").filter(|n| valid_name(n)).ok_or("bad-name")?.to_string();
        let peer = l.get("peer").and_then(crypta_proto::b64::key).ok_or("bad-peer")?;
        let endpoint: SocketAddrV4 = l.get("endpoint").and_then(|e| e.parse().ok()).ok_or("bad-endpoint")?;
        let addrs: Option<Vec<Prefix4>> = list(l, "addrs")
            .map(|a| {
                let (ip, len) = a.split_once('/').unwrap_or((a, "32"));
                Some(Prefix4 { addr: ip.parse().ok()?, len: len.parse().ok().filter(|l| *l <= 32)? })
            })
            .collect();
        let addrs = addrs.filter(|a| !a.is_empty() && a.len() <= 8).ok_or(Unusable::NoV4Address.word())?;
        let allowed: Option<Vec<Prefix4>> = list(l, "allowed").map(Prefix4::parse).collect();
        let allowed = allowed.filter(|a| !a.is_empty()).ok_or(Unusable::NoV4Routes.word())?;
        let dns: Option<Vec<Ipv4Addr>> = list(l, "dns").map(|a| a.parse().ok()).collect();
        let dns = dns.filter(|d| d.len() <= 3).ok_or("bad-dns")?;
        let keepalive = match l.get("keepalive") {
            None => None,
            Some(k) => Some(k.parse::<u16>().ok().filter(|k| (1..=3600).contains(k)).ok_or("bad-keepalive")?),
        };
        let full = allowed.iter().any(|p| p.len == 0);
        Ok(Profile {
            name,
            key: [0; 16],
            public: [0; 32],
            psk: None,
            peer,
            endpoint,
            keepalive,
            addrs,
            allowed,
            dns,
            domains: Vec::new(),
            mtu: None,
            block: full,
            insecure: false,
            scoped: false,
        })
    }

    pub fn full(&self) -> bool {
        self.allowed.iter().any(|p| p.len == 0)
    }

    /// What atrium-net is asked to apply (vpn.md §5.1).
    pub fn proposal(&self) -> Tunnel {
        let full = self.full();
        Tunnel {
            addrs: self.addrs.iter().map(|p| Net4 { addr: p.addr, prefix: p.len }).collect(),
            mtu: self.mtu.unwrap_or(1420),
            includes: if full { Vec::new() } else { self.allowed.iter().map(|p| Net4 { addr: p.addr, prefix: p.len }).collect() },
            full,
            dns: self.dns.clone(),
            domains: self.domains.clone(),
            insecure: self.insecure,
            gateway: *self.endpoint.ip(),
            port: self.endpoint.port(),
            block: self.block && full,
            scoped: self.scoped,
        }
    }

    pub fn to_text(&self) -> String {
        let mut p = Line::new("profile")
            .with("name", &self.name)
            .with_hex("key", &self.key)
            .with_hex("pub", &self.public)
            .with_hex("peer", &self.peer)
            .with("endpoint", self.endpoint)
            .with("block", self.block as u8)
            .with("insecure", self.insecure as u8)
            .with("scoped", self.scoped as u8)
            .with("addr", join(&self.addrs.iter().map(|p| format!("{}/{}", p.addr, p.len)).collect::<Vec<_>>()));
        if let Some(i) = self.psk {
            p = p.with_hex("psk", &i);
        }
        if let Some(k) = self.keepalive {
            p = p.with("keepalive", k);
        }
        if let Some(m) = self.mtu {
            p = p.with("mtu", m);
        }
        if !self.dns.is_empty() {
            p = p.with("dns", join(&self.dns));
        }
        if !self.domains.is_empty() {
            p = p.with("domains", self.domains.join(","));
        }
        let mut out = String::from_utf8(p.encode().unwrap_or_default()).unwrap_or_default();
        for c in self.allowed.chunks(128) {
            let l = Line::new("allowed").with("add", join(c));
            out.push_str(&String::from_utf8(l.encode().unwrap_or_default()).unwrap_or_default());
        }
        out
    }

    pub fn from_text(s: &str) -> Option<Profile> {
        let mut lines = s.lines().map(Line::parse);
        let l = lines.next()??;
        if l.verb != "profile" {
            return None;
        }
        let flag = |k: &str| l.get(k) == Some("1");
        let mut allowed = Vec::new();
        for a in lines {
            let a = a?;
            if a.verb != "allowed" {
                return None;
            }
            for p in list(&a, "add") {
                allowed.push(Prefix4::parse(p)?);
            }
        }
        let addrs: Option<Vec<Prefix4>> = list(&l, "addr")
            .map(|a| {
                let (ip, len) = a.split_once('/')?;
                Some(Prefix4 { addr: ip.parse().ok()?, len: len.parse().ok().filter(|l| *l <= 32)? })
            })
            .collect();
        let dns: Option<Vec<Ipv4Addr>> = list(&l, "dns").map(|a| a.parse().ok()).collect();
        let name = l.get("name")?.to_string();
        valid_name(&name).then_some(())?;
        Some(Profile {
            name,
            key: hexa(&l, "key")?,
            public: hexa(&l, "pub")?,
            psk: match l.get("psk") {
                Some(_) => Some(hexa(&l, "psk")?),
                None => None,
            },
            peer: hexa(&l, "peer")?,
            endpoint: l.get("endpoint")?.parse().ok()?,
            keepalive: l.num("keepalive"),
            addrs: addrs?,
            allowed,
            dns: dns?,
            domains: list(&l, "domains").map(str::to_string).collect(),
            mtu: l.num("mtu"),
            block: flag("block"),
            insecure: flag("insecure"),
            scoped: flag("scoped"),
        })
    }

    pub fn path(dir: &Path, name: &str) -> PathBuf {
        dir.join(format!("{name}.profile"))
    }

    pub fn save(&self, dir: &Path) -> std::io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let tmp = dir.join(format!(".{}.tmp", self.name));
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        f.write_all(self.to_text().as_bytes())?;
        f.sync_all()?;
        std::fs::rename(&tmp, Self::path(dir, &self.name))
    }

    pub fn load(dir: &Path, name: &str) -> Option<Profile> {
        if !valid_name(name) {
            return None;
        }
        let p = Profile::from_text(&std::fs::read_to_string(Self::path(dir, name)).ok()?)?;
        (p.name == name).then_some(p)
    }

    pub fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .map(|rd| rd.flatten().filter_map(|e| e.file_name().to_str()?.strip_suffix(".profile").map(str::to_string)).filter(|n| valid_name(n)).collect())
            .unwrap_or_default();
        v.sort();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "[Interface]\nPrivateKey = yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=\nAddress = 10.99.0.2/24\nDNS = 10.99.0.1, corp.test\n\n[Peer]\nPublicKey = xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=\nAllowedIPs = 0.0.0.0/0\nEndpoint = 198.51.100.51:51820\nPersistentKeepalive = 25\n";

    #[test]
    fn a_full_tunnel_profile_round_trips_and_proposes() {
        let c = crypta_proto::conf::parse(FILE.as_bytes()).unwrap();
        let p = Profile::from_conf("home", &c, [1; 16], [2; 32], Some([3; 16])).unwrap();
        assert!(p.block, "a full tunnel blocks when not connected by default");
        let back = Profile::from_text(&p.to_text()).unwrap();
        assert_eq!(back, p);
        let mut t = p.proposal();
        assert!(t.full && t.block && t.includes.is_empty());
        assert_eq!(t.addrs, vec![Net4 { addr: "10.99.0.2".parse().unwrap(), prefix: 24 }]);
        assert_eq!((t.gateway, t.port), ("198.51.100.51".parse().unwrap(), 51820));
        let host = net_proto::HostView { others: vec![("10.88.0.50".parse().unwrap(), 24)] };
        assert_eq!(net_proto::tunnel::check_tunnel(&mut t, &host), Ok(()));
    }

    #[test]
    fn a_created_profile() {
        let l = Line::parse("create name=acme peer=xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg= endpoint=198.51.100.51:51820 addrs=10.99.0.2/24 allowed=0.0.0.0/0 dns=10.99.0.1 keepalive=25").unwrap();
        let p = Profile::from_create(&l).unwrap();
        assert!(p.full() && p.block && !p.scoped);
        assert_eq!(p.addrs, vec![Prefix4 { addr: "10.99.0.2".parse().unwrap(), len: 24 }]);
        assert_eq!((p.keepalive, p.dns.len()), (Some(25), 1));
        let split = Line::parse("create name=w peer=xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg= endpoint=198.51.100.51:51820 addrs=10.99.0.2 allowed=10.99.0.0/24,10.100.0.0/16").unwrap();
        let p = Profile::from_create(&split).unwrap();
        assert!(!p.block && p.allowed.len() == 2 && p.addrs[0].len == 32);
        for (bad, why) in [("peer=short", "bad-peer"), ("addrs=", "no-v4-address"), ("allowed=10.0.0.0/33", "no-v4-routes"), ("keepalive=0", "bad-keepalive"), ("endpoint=x", "bad-endpoint")] {
            let (k, _) = bad.split_once('=').unwrap();
            let text = l.fields.iter().filter(|(kk, _)| kk != k).map(|(k, v)| format!(" {k}={v}")).collect::<String>();
            let b = Line::parse(&format!("create{text} {bad}")).unwrap();
            assert_eq!(Profile::from_create(&b), Err(why), "{bad}");
        }
    }

    #[test]
    fn a_split_tunnel_and_what_v1_cannot_use() {
        let split = FILE.replace("AllowedIPs = 0.0.0.0/0", "AllowedIPs = 10.99.0.0/24, 10.100.0.0/16");
        let c = crypta_proto::conf::parse(split.as_bytes()).unwrap();
        let p = Profile::from_conf("work", &c, [1; 16], [2; 32], None).unwrap();
        assert!(!p.block);
        let t = p.proposal();
        assert!(!t.full && t.includes.len() == 2);
        let named = FILE.replace("198.51.100.51:51820", "vpn.example.com:51820");
        let c = crypta_proto::conf::parse(named.as_bytes()).unwrap();
        assert_eq!(Profile::from_conf("x", &c, [1; 16], [2; 32], None), Err(Unusable::NamedEndpoint));
        let v6 = FILE.replace("Address = 10.99.0.2/24", "Address = fd00::2/64");
        let c = crypta_proto::conf::parse(v6.as_bytes()).unwrap();
        assert_eq!(Profile::from_conf("x", &c, [1; 16], [2; 32], None), Err(Unusable::NoV4Address));
    }

    #[test]
    fn many_allowed_prefixes_fit_on_disk() {
        let many: String = (0..1000)
            .map(|i| format!("10.{}.{}.0/24", 100 + i / 256, i % 256))
            .collect::<Vec<_>>()
            .chunks(100)
            .map(|c| format!("AllowedIPs = {}\n", c.join(", ")))
            .collect();
        let f = FILE.replace("AllowedIPs = 0.0.0.0/0\n", &many);
        let c = crypta_proto::conf::parse(f.as_bytes()).unwrap();
        let p = Profile::from_conf("big", &c, [1; 16], [2; 32], None).unwrap();
        let d = tempfile::tempdir().unwrap();
        p.save(d.path()).unwrap();
        assert_eq!(Profile::load(d.path(), "big").unwrap(), p);
        assert_eq!(Profile::names(d.path()), vec!["big".to_string()]);
        assert!(Profile::load(d.path(), "../big").is_none());
    }

    #[test]
    fn names() {
        assert!(valid_name("home-1"));
        assert!(!valid_name("Home"));
        assert!(!valid_name("a/b"));
        assert!(!valid_name(""));
        assert!(!valid_name(&"a".repeat(33)));
    }
}
