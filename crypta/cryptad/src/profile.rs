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
    /// An AnyConnect profile (vpn.md §9, V4); the WireGuard fields above
    /// are then unused.
    pub ac: Option<Ac>,
}

/// An AnyConnect gateway and how to sign in to it. No secret: a saved
/// password, the session cookie and the client key are Arca items of the
/// crypta service, named by account (`<profile>/password`, `/session`,
/// `/tls`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ac {
    /// Direct GlobalProtect gateway, sharing the corporate profile fields.
    pub globalprotect: bool,
    pub gp_portal: bool,
    /// The gateway's name (TLS and `Host`), resolved at each connect.
    pub host: String,
    pub port: u16,
    /// `/` or a group's path.
    pub path: String,
    /// CA certificates the profile trusts besides the system's (DER).
    pub cas: Vec<Vec<u8>>,
    /// Or the gateway's own certificate, pinned by its SHA-256.
    pub pin: Option<[u8; 32]>,
    /// Remembered after a successful sign-in, offered next time.
    pub username: Option<String>,
    pub group: Option<String>,
    /// What the gateway is told this machine is called (§9.5).
    pub hostname: String,
    /// The client certificate chain (DER, its own first); its key is the
    /// Arca item `<profile>/tls`.
    pub cert_chain: Vec<Vec<u8>>,
    /// The client key's public point (SEC1), once made.
    pub cert_key: Option<Vec<u8>>,
}

impl Ac {
    pub fn kind(&self) -> &'static str {
        if self.globalprotect { "gp" } else { "ac" }
    }
}

/// A gateway name: a DNS name (letters, digits, `-`, dots), at most 253.
pub fn valid_host(h: &str) -> bool {
    !h.is_empty() && h.len() <= 253 && h.split('.').all(|l| !l.is_empty() && l.len() <= 63 && l.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') && !l.starts_with('-'))
}

/// A gateway path: `/` and URL-safe characters only.
pub fn valid_path(p: &str) -> bool {
    p.starts_with('/') && !p.starts_with("//") && p.len() <= 128 && p.bytes().all(|c| c.is_ascii_alphanumeric() || b"/-_.~".contains(&c))
}

/// The hostname the gateway is told by default (§9.5): generic.
pub const HOSTNAME: &str = "atrium";

/// A PEM file's certificates (DER), or one base64 body.
pub fn certs_of(text: &str) -> Option<Vec<Vec<u8>>> {
    if !text.contains("-----BEGIN") {
        return crypta_proto::b64::decode(text.trim()).filter(|d| !d.is_empty()).map(|d| vec![d]);
    }
    let mut out = Vec::new();
    for blk in text.split("-----BEGIN CERTIFICATE-----").skip(1) {
        let body: String = blk.split("-----END").next()?.lines().map(str::trim).collect();
        out.push(crypta_proto::b64::decode(&body)?);
    }
    (!out.is_empty()).then_some(out)
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
            ac: None,
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
            ac: None,
        })
    }

    /// From a `create-ac` request (vpn.md §7.3, V4): an AnyConnect
    /// gateway, `host=` (`name[:port]`), with optional `path=`, `group=`,
    /// `user=`, `pin=` (hex SHA-256 of the gateway's certificate) and `ca=`
    /// (base64 DER of a CA to trust).
    pub fn from_create_ac(l: &Line) -> Result<Profile, &'static str> {
        let name = l.get("name").filter(|n| valid_name(n)).ok_or("bad-name")?.to_string();
        let hp = l.get("host").ok_or("bad-host")?;
        let (host, port) = match hp.rsplit_once(':') {
            Some((h, p)) => (h, p.parse::<u16>().ok().filter(|p| *p > 0).ok_or("bad-host")?),
            None => (hp, 443),
        };
        let host = host.to_ascii_lowercase();
        if !valid_host(&host) {
            return Err("bad-host");
        }
        let globalprotect = l.verb == "create-gp";
        let gp_portal = match l.get("portal") {
            None | Some("0") => false,
            Some("1") if globalprotect => true,
            _ => return Err("bad-portal"),
        };
        if globalprotect && (l.get("path").is_some() || l.get("group").is_some()) {
            return Err("globalprotect-uses-direct-gateway");
        }
        let path = l.get("path").unwrap_or("/").to_string();
        if !valid_path(&path) {
            return Err("bad-path");
        }
        let text = |k: &str| -> Result<Option<String>, &'static str> {
            match l.hex(k) {
                None if l.get(k).is_none() => Ok(None),
                Some(b) => String::from_utf8(b).ok().filter(|s| !s.is_empty() && s.len() <= 128 && !s.chars().any(char::is_control)).map(Some).ok_or("bad-text"),
                None => Err("bad-text"),
            }
        };
        let pin = match l.get("pin") {
            None => None,
            Some(_) => Some(hexa::<32>(l, "pin").ok_or("bad-pin")?),
        };
        let cas = match l.get("ca") {
            None => Vec::new(),
            Some(c) => certs_of(c).ok_or("bad-ca")?,
        };
        Ok(Profile {
            name,
            key: [0; 16],
            public: [0; 32],
            psk: None,
            peer: [0; 32],
            endpoint: SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port),
            keepalive: None,
            addrs: Vec::new(),
            allowed: Vec::new(),
            dns: Vec::new(),
            domains: Vec::new(),
            mtu: None,
            block: false,
            // Pushed names under a signed parent often fail DNSSEC (§5.5).
            insecure: true,
            // A person's corporate VPN carries their work apps (§9.6).
            scoped: l.get("scoped") != Some("0"),
            ac: Some(Ac {
                globalprotect,
                gp_portal,
                host,
                port,
                path,
                cas,
                pin,
                username: text("user")?,
                group: text("group")?,
                hostname: HOSTNAME.into(),
                cert_chain: Vec::new(),
                cert_key: None,
            }),
        })
    }

    /// The tunnel for atrium-net from what an AnyConnect gateway pushed,
    /// to the address cryptad resolved.
    pub fn ac_proposal(&self, p: &crypta_proto::engine::Pushed, gw: SocketAddrV4) -> Tunnel {
        Tunnel {
            addrs: vec![Net4 { addr: p.addr, prefix: p.prefix }],
            mtu: p.mtu,
            includes: if p.full { Vec::new() } else { p.includes.iter().map(|x| Net4 { addr: x.addr, prefix: x.len }).collect() },
            full: p.full,
            dns: p.dns.clone(),
            domains: p.domains.clone(),
            insecure: self.insecure,
            gateway: *gw.ip(),
            port: gw.port(),
            tcp: true,
            block: self.block && p.full,
            scoped: self.scoped,
        }
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
            tcp: false,
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
        if let Some(a) = &self.ac {
            p = p.with("kind", a.kind()).with("host", &a.host).with("port", a.port).with("path", &a.path).with_hex("hostname", a.hostname.as_bytes());
            if a.gp_portal {
                p = p.with("portal", 1);
            }
            if let Some(u) = &a.username {
                p = p.with_hex("user", u.as_bytes());
            }
            if let Some(g) = &a.group {
                p = p.with_hex("group", g.as_bytes());
            }
            if let Some(x) = &a.pin {
                p = p.with_hex("pin", x);
            }
            if let Some(k) = &a.cert_key {
                p = p.with_hex("certkey", k);
            }
        }
        let mut out = String::from_utf8(p.encode().unwrap_or_default()).unwrap_or_default();
        if let Some(a) = &self.ac {
            for (verb, ders) in [("ca", &a.cas), ("certificate", &a.cert_chain)] {
                for d in ders.iter() {
                    let l = Line::new(verb).with("der", crypta_proto::b64::encode(d));
                    out.push_str(&String::from_utf8(l.encode().unwrap_or_default()).unwrap_or_default());
                }
            }
        }
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
        let (mut cas, mut chain) = (Vec::new(), Vec::new());
        for a in lines {
            let a = a?;
            match a.verb.as_str() {
                "allowed" => {
                    for p in list(&a, "add") {
                        allowed.push(Prefix4::parse(p)?);
                    }
                }
                "ca" => cas.push(crypta_proto::b64::decode(a.get("der")?)?),
                "certificate" => chain.push(crypta_proto::b64::decode(a.get("der")?)?),
                _ => return None,
            }
        }
        let utf8 = |k: &str| l.hex(k).and_then(|b| String::from_utf8(b).ok());
        let ac = match l.get("kind") {
            Some("ac" | "gp") => Some(Ac {
                globalprotect: l.get("kind") == Some("gp"),
                gp_portal: l.get("kind") == Some("gp") && flag("portal"),
                host: l.get("host").filter(|h| valid_host(h))?.to_string(),
                port: l.num("port")?,
                path: l.get("path").filter(|p| valid_path(p))?.to_string(),
                cas,
                pin: match l.get("pin") {
                    Some(_) => Some(hexa(&l, "pin")?),
                    None => None,
                },
                username: utf8("user"),
                group: utf8("group"),
                hostname: utf8("hostname").unwrap_or_else(|| HOSTNAME.into()),
                cert_chain: chain,
                cert_key: l.hex("certkey"),
            }),
            None => None,
            Some(_) => return None,
        };
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
            ac,
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
    fn a_globalprotect_profile_round_trips() {
        let line = Line::parse("create-gp name=work host=gw.example:8443").unwrap();
        let p = Profile::from_create_ac(&line).unwrap();
        assert_eq!(p.ac.as_ref().unwrap().kind(), "gp");
        assert_eq!(Profile::from_text(&p.to_text()), Some(p));
        let portal = Line::parse("create-gp name=work host=gw.example portal=1").unwrap();
        let p = Profile::from_create_ac(&portal).unwrap();
        assert!(p.ac.as_ref().unwrap().gp_portal);
        assert_eq!(Profile::from_text(&p.to_text()), Some(p));
        let portal = Line::parse("create-gp name=work host=gw.example path=/portal").unwrap();
        assert!(Profile::from_create_ac(&portal).is_err());
    }

    #[test]
    fn an_anyconnect_profile() {
        let ca = crypta_proto::b64::encode(&[0x30, 0x03, 0x02, 0x01, 0x01]);
        let l = Line::parse(&format!("create-ac name=work host=GW.corp.example:8443 path=/staff user={} group={} ca={ca}", aura_ipc::hex(b"ann smith"), aura_ipc::hex(b"Staff"))).unwrap();
        let mut p = Profile::from_create_ac(&l).unwrap();
        let a = p.ac.clone().unwrap();
        assert_eq!((a.host.as_str(), a.port, a.path.as_str(), a.username.as_deref(), a.group.as_deref()), ("gw.corp.example", 8443, "/staff", Some("ann smith"), Some("Staff")));
        assert!(p.scoped && p.insecure && !p.block, "a corporate VPN carries a persona's apps by default");
        p.ac.as_mut().unwrap().cert_chain = vec![vec![1, 2, 3]];
        p.ac.as_mut().unwrap().cert_key = Some(vec![4; 65]);
        let back = Profile::from_text(&p.to_text()).unwrap();
        assert_eq!(back, p);
        let pushed = crypta_proto::engine::Pushed {
            addr: "10.98.0.50".parse().unwrap(),
            prefix: 24,
            mtu: 1378,
            dns: vec!["10.98.0.1".parse().unwrap()],
            domains: vec!["corp.crypta.test".into()],
            includes: vec![Prefix4::parse("10.98.0.0/24").unwrap()],
            excludes: vec![],
            full: false,
            banner: String::new(),
            ignored: vec![],
        };
        let mut t = p.ac_proposal(&pushed, "198.51.100.52:443".parse().unwrap());
        assert!(t.tcp && !t.full && t.scoped && t.includes.len() == 1);
        assert_eq!(t.addrs, vec![Net4 { addr: "10.98.0.50".parse().unwrap(), prefix: 24 }]);
        let host = net_proto::HostView { others: vec![("10.88.0.50".parse().unwrap(), 24)] };
        assert_eq!(net_proto::tunnel::check_tunnel(&mut t, &host), Ok(()));
        for bad in ["host=-x", "host=a:0", "host=a/b", "path=//x", "pin=12", "ca=!!", "user=zz"] {
            let (k, _) = bad.split_once('=').unwrap();
            let text = l.fields.iter().filter(|(kk, _)| kk != k).map(|(k, v)| format!(" {k}={v}")).collect::<String>();
            assert!(Profile::from_create_ac(&Line::parse(&format!("create-ac{text} {bad}")).unwrap()).is_err(), "{bad}");
        }
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
