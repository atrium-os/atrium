//! The tunnel: an HTTP `CONNECT` over the TLS connection, the settings the
//! gateway pushes in its answer, then CSTP frames both ways (vpn.md §9.1,
//! §9.4). TLS only: we never offer DTLS (decision 13), nor compression.

use crate::http::Head;
use crypta_proto::net::Prefix4;
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;

pub const MAX_INCLUDES: usize = 1024;
pub const MAX_EXCLUDES: usize = 256;
pub const MAX_DNS: usize = 3;
pub const MAX_DOMAINS: usize = 64;
/// No CSTP frame we take carries more (an MTU of 1500 and room).
pub const MAX_PAYLOAD: usize = 4096;

pub const DATA: u8 = 0x00;
pub const DPD_REQ: u8 = 0x03;
pub const DPD_RESP: u8 = 0x04;
pub const DISCONNECT: u8 = 0x05;
pub const KEEPALIVE: u8 = 0x07;
pub const COMPRESSED: u8 = 0x08;
pub const TERMINATE: u8 = 0x09;

/// The `CONNECT` that opens the tunnel. `hostname` is the profile's
/// (vpn.md §9.5: a generic name by default), `mtu` what we can carry.
pub fn connect(host: &str, cookie: &str, hostname: &str, mtu: u16) -> Vec<u8> {
    let c = format!("webvpn={cookie}");
    let m = mtu.to_string();
    crate::http::request(
        "CONNECT",
        "/CSCOSSLC/tunnel",
        host,
        &[
            ("Cookie", c.as_str()),
            ("X-CSTP-Version", "1"),
            ("X-CSTP-Hostname", hostname),
            ("X-CSTP-Base-MTU", m.as_str()),
            ("X-CSTP-MTU", m.as_str()),
            ("X-CSTP-Address-Type", "IPv4"),
            ("X-CSTP-Full-IPv6-Capability", "false"),
            ("X-CSTP-License", "accept"),
        ],
        b"",
    )
}

/// What the gateway pushed (vpn.md §9.4), read and capped; atrium-net
/// checks it again before anything changes (§5.2).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Push {
    pub addr: Ipv4Addr,
    pub prefix: u8,
    pub mtu: u16,
    pub dns: Vec<Ipv4Addr>,
    pub domains: Vec<String>,
    pub includes: Vec<Prefix4>,
    pub excludes: Vec<Prefix4>,
    /// No includes: the gateway asked for all traffic.
    pub full: bool,
    pub dpd_s: u16,
    pub keepalive_s: u16,
    pub idle_s: Option<u32>,
    pub session_s: Option<u32>,
    pub banner: String,
    /// Headers dropped and logged: scripts, proxies, profiles, updates.
    pub ignored: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PushError {
    NoAddress,
    Bad(&'static str),
    TooMany(&'static str),
}

impl std::fmt::Display for PushError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PushError::NoAddress => f.write_str("the gateway gave no IPv4 address"),
            PushError::Bad(w) => write!(f, "the gateway pushed a bad {w}"),
            PushError::TooMany(w) => write!(f, "the gateway pushed too many {w}"),
        }
    }
}

fn netmask_len(m: &str) -> Option<u8> {
    let v = u32::from(m.parse::<Ipv4Addr>().ok()?);
    let n = v.leading_ones();
    (v.checked_shl(n).unwrap_or(0) == 0).then_some(n as u8)
}

/// `a.b.c.d/m.m.m.m` or `a.b.c.d/len`.
fn route(s: &str) -> Option<Prefix4> {
    let (a, m) = s.trim().split_once('/')?;
    let len = if m.contains('.') { netmask_len(m)? } else { m.parse::<u8>().ok().filter(|l| *l <= 32 && m.len() <= 2)? };
    Prefix4::new(a.parse().ok()?, len)
}

fn valid_domain(d: &str) -> bool {
    !d.is_empty() && d.len() <= 253 && d.split('.').all(|l| !l.is_empty() && l.len() <= 63 && l.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') && !l.starts_with('-') && !l.ends_with('-'))
}

fn secs(h: &Head, k: &str, lo: u32, hi: u32) -> Option<u32> {
    h.get(k).and_then(|v| v.trim().parse::<i64>().ok()).filter(|v| *v > 0).map(|v| (v.min(hi as i64) as u32).max(lo))
}

/// Read the gateway's answer to [`connect`].
pub fn push(h: &Head) -> Result<Push, PushError> {
    let addr: Ipv4Addr = h.get("x-cstp-address").ok_or(PushError::NoAddress)?.parse().map_err(|_| PushError::Bad("address"))?;
    if addr.is_unspecified() || addr.is_loopback() || addr.is_multicast() || addr.is_broadcast() || addr.is_link_local() {
        return Err(PushError::Bad("address"));
    }
    let prefix = match h.get("x-cstp-netmask") {
        Some(m) => netmask_len(m).filter(|l| *l >= 8).ok_or(PushError::Bad("netmask"))?,
        None => 32,
    };
    let mut p = Push {
        addr,
        prefix,
        mtu: h.get("x-cstp-mtu").and_then(|m| m.parse::<u32>().ok()).map(|m| m.clamp(1280, 1500) as u16).unwrap_or(1406),
        dns: Vec::new(),
        domains: Vec::new(),
        includes: Vec::new(),
        excludes: Vec::new(),
        full: false,
        dpd_s: secs(h, "x-cstp-dpd", 5, 300).unwrap_or(30) as u16,
        keepalive_s: secs(h, "x-cstp-keepalive", 5, 300).unwrap_or(20) as u16,
        idle_s: secs(h, "x-cstp-idle-timeout", 60, 7 * 86400),
        session_s: secs(h, "x-cstp-session-timeout", 60, 30 * 86400),
        banner: h.get("x-cstp-banner").map(crate::auth::clean).unwrap_or_default(),
        ignored: Vec::new(),
    };
    for (k, v) in &h.headers {
        match k.as_str() {
            "x-cstp-dns" => {
                if p.dns.len() >= MAX_DNS {
                    return Err(PushError::TooMany("DNS servers"));
                }
                let a: Ipv4Addr = v.parse().map_err(|_| PushError::Bad("DNS server"))?;
                if !p.dns.contains(&a) {
                    p.dns.push(a);
                }
            }
            "x-cstp-default-domain" | "x-cstp-split-dns" => {
                for d in v.split([' ', ',']).filter(|d| !d.is_empty()) {
                    let d = d.trim_end_matches('.').to_ascii_lowercase();
                    if !valid_domain(&d) {
                        return Err(PushError::Bad("DNS domain"));
                    }
                    if p.domains.len() >= MAX_DOMAINS {
                        return Err(PushError::TooMany("DNS domains"));
                    }
                    if !p.domains.contains(&d) {
                        p.domains.push(d);
                    }
                }
            }
            "x-cstp-split-include" => {
                if p.includes.len() >= MAX_INCLUDES {
                    return Err(PushError::TooMany("routes"));
                }
                p.includes.push(route(v).ok_or(PushError::Bad("route"))?);
            }
            "x-cstp-split-exclude" => {
                if p.excludes.len() >= MAX_EXCLUDES {
                    return Err(PushError::TooMany("excluded routes"));
                }
                p.excludes.push(route(v).ok_or(PushError::Bad("excluded route"))?);
            }
            // Things we never act on (vpn.md §9.4): logged, by name only.
            "x-cstp-post-auth-xml" | "x-cstp-proxy-pac" | "x-cstp-proxy" | "x-cstp-msie-proxy-server" | "x-cstp-msie-proxy-pac-url" | "x-cstp-smartcard-removal-disconnect" | "x-cstp-client-bypass-protocol" | "x-cstp-tunnel-all-dns" | "x-cstp-routing-filtering-ignore" | "x-cstp-quarantine" | "x-cstp-content-encoding" | "x-cstp-rekey-method" | "x-cstp-rekey-time" | "x-cstp-address-ip6" | "x-cstp-split-include-ip6" | "x-cstp-split-exclude-ip6" | "x-cstp-msie-proxy-lockdown" | "x-cstp-msie-proxy-exceptions" | "x-cstp-msie-proxy-option" | "x-transcend-version" | "x-cstp-dynamic-split-exclude-domains" | "x-cstp-dynamic-split-include-domains" | "x-cstp-lease-duration" | "x-cstp-disconnected-timeout" | "x-cstp-keep" | "x-cstp-session-timeout-alert-interval" | "x-cstp-session-timeout-remaining" | "x-cstp-fw-rule" => {
                if !p.ignored.contains(k) {
                    p.ignored.push(k.clone());
                }
            }
            _ => {}
        }
    }
    if h.get("x-cstp-content-encoding").is_some() {
        // We asked for none; a gateway that compresses anyway is not one
        // we can follow.
        return Err(PushError::Bad("compression"));
    }
    p.full = p.includes.is_empty();
    Ok(p)
}

/// A frame header: `STF`, version 1, length (big-endian), type, 0.
pub fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(8 + payload.len());
    f.extend_from_slice(&[b'S', b'T', b'F', 1]);
    f.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    f.push(kind);
    f.push(0);
    f.extend_from_slice(payload);
    f
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    Header,
    TooLong,
    Kind(u8),
}

/// Frames out of the TLS stream.
#[derive(Default)]
pub struct Deframer {
    buf: Vec<u8>,
}

impl Deframer {
    pub fn push(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    /// The next whole frame: its type and payload. An error ends the tunnel.
    pub fn next(&mut self) -> Result<Option<(u8, Vec<u8>)>, FrameError> {
        if self.buf.len() < 8 {
            return Ok(None);
        }
        if &self.buf[..4] != b"STF\x01" || self.buf[7] != 0 {
            return Err(FrameError::Header);
        }
        let n = u16::from_be_bytes([self.buf[4], self.buf[5]]) as usize;
        if n > MAX_PAYLOAD {
            return Err(FrameError::TooLong);
        }
        let kind = self.buf[6];
        if !matches!(kind, DATA | DPD_REQ | DPD_RESP | DISCONNECT | KEEPALIVE | TERMINATE) {
            // COMPRESSED included: never asked for.
            return Err(FrameError::Kind(kind));
        }
        if self.buf.len() < 8 + n {
            return Ok(None);
        }
        let p = self.buf[8..8 + n].to_vec();
        self.buf.drain(..8 + n);
        Ok(Some((kind, p)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(lines: &[(&str, &str)]) -> Head {
        Head { status: 200, headers: lines.iter().map(|(k, v)| (k.to_ascii_lowercase(), v.to_string())).collect() }
    }

    #[test]
    fn ocserv_push() {
        let h = head(&[
            ("X-CSTP-Version", "1"),
            ("X-CSTP-Address", "10.98.0.37"),
            ("X-CSTP-Netmask", "255.255.255.0"),
            ("X-CSTP-DNS", "10.98.0.1"),
            ("X-CSTP-Default-Domain", "corp.example lab.example"),
            ("X-CSTP-Split-Include", "10.98.0.0/255.255.255.0"),
            ("X-CSTP-Split-Include", "192.0.2.0/24"),
            ("X-CSTP-Split-Exclude", "10.98.0.128/255.255.255.128"),
            ("X-CSTP-MTU", "1434"),
            ("X-CSTP-DPD", "90"),
            ("X-CSTP-Keepalive", "32400"),
            ("X-CSTP-Idle-Timeout", "none"),
            ("X-CSTP-Banner", "Welcome\u{7}"),
            ("X-CSTP-Post-Auth-XML", "<x/>"),
            ("X-CSTP-MSIE-Proxy-PAC-URL", "http://evil/pac"),
        ]);
        let p = push(&h).unwrap();
        assert_eq!((p.addr, p.prefix, p.mtu, p.dpd_s, p.keepalive_s, p.idle_s), ("10.98.0.37".parse().unwrap(), 24, 1434, 90, 300, None));
        assert_eq!(p.domains, vec!["corp.example", "lab.example"]);
        assert_eq!(p.includes, vec![Prefix4::parse("10.98.0.0/24").unwrap(), Prefix4::parse("192.0.2.0/24").unwrap()]);
        assert_eq!(p.excludes, vec![Prefix4::parse("10.98.0.128/25").unwrap()]);
        assert!(!p.full);
        assert_eq!(p.banner, "Welcome");
        assert_eq!(p.ignored, vec!["x-cstp-post-auth-xml", "x-cstp-msie-proxy-pac-url"]);
        let full = push(&head(&[("X-CSTP-Address", "10.98.0.2")])).unwrap();
        assert!(full.full);
        assert_eq!(full.prefix, 32);
    }

    #[test]
    fn hostile_pushes() {
        let a = ("X-CSTP-Address", "10.98.0.2");
        assert_eq!(push(&head(&[])), Err(PushError::NoAddress));
        assert!(push(&head(&[("X-CSTP-Address", "127.0.0.1")])).is_err());
        assert!(push(&head(&[("X-CSTP-Address", "224.0.0.1")])).is_err());
        assert!(push(&head(&[a, ("X-CSTP-Netmask", "255.0.255.0")])).is_err());
        assert!(push(&head(&[a, ("X-CSTP-Split-Include", "10.0.0.0/255.0.255.0")])).is_err());
        assert!(push(&head(&[a, ("X-CSTP-Default-Domain", "a..b")])).is_err());
        assert!(push(&head(&[a, ("X-CSTP-DNS", "1.1.1.1"), ("X-CSTP-DNS", "1.1.1.2"), ("X-CSTP-DNS", "1.1.1.3"), ("X-CSTP-DNS", "1.1.1.4")])).is_err());
        assert!(push(&head(&[a, ("X-CSTP-Content-Encoding", "lzs")])).is_err());
        let many: Vec<(String, String)> = (0..=MAX_INCLUDES).map(|i| ("X-CSTP-Split-Include".to_string(), format!("10.{}.{}.0/24", i / 256, i % 256))).collect();
        let mut h = head(&[a]);
        h.headers.extend(many.into_iter().map(|(k, v)| (k.to_ascii_lowercase(), v)));
        assert_eq!(push(&h), Err(PushError::TooMany("routes")));
        // Clamped, not refused.
        let p = push(&head(&[a, ("X-CSTP-MTU", "60000"), ("X-CSTP-DPD", "1")])).unwrap();
        assert_eq!((p.mtu, p.dpd_s), (1500, 5));
    }

    #[test]
    fn frames() {
        let mut d = Deframer::default();
        let mut s = frame(DATA, b"abc");
        s.extend(frame(DPD_REQ, b""));
        for c in s.chunks(3) {
            d.push(c);
        }
        assert_eq!(d.next().unwrap(), Some((DATA, b"abc".to_vec())));
        assert_eq!(d.next().unwrap(), Some((DPD_REQ, vec![])));
        assert_eq!(d.next().unwrap(), None);
        let mut bad = Deframer::default();
        bad.push(&frame(COMPRESSED, b"x"));
        assert_eq!(bad.next(), Err(FrameError::Kind(COMPRESSED)));
        let mut bad = Deframer::default();
        bad.push(b"GET / HTTP/1.1\r\n");
        assert_eq!(bad.next(), Err(FrameError::Header));
        let mut big = Deframer::default();
        big.push(&[b'S', b'T', b'F', 1, 0xff, 0xff, 0, 0]);
        assert_eq!(big.next(), Err(FrameError::TooLong));
        let c = String::from_utf8(connect("gw.example", "abc", "atrium", 1406)).unwrap();
        assert!(c.starts_with("CONNECT /CSCOSSLC/tunnel HTTP/1.1\r\nHost: gw.example\r\n"));
        assert!(c.contains("Cookie: webvpn=abc\r\n") && c.ends_with("\r\n\r\n") && !c.contains("X-DTLS"));
    }
}
