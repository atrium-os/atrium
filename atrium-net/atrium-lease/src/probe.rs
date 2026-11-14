//! The connectivity probe's wire handling (docs/spec/atrium-net.md §4.2):
//! one DNS A query and one plain-HTTP GET, both parsed here, in the
//! sandboxed `atrium-probe`, never in atrium-net. Pure functions, so they are
//! fuzzed in host tests.

use std::net::Ipv4Addr;

/// Longest reply read from the HTTP server.
pub const HTTP_MAX: usize = 8192;
/// Longest Location reported (a captive portal's sign-in page).
pub const LOCATION_MAX: usize = 256;

pub use net_proto::{valid_host_name as valid_name, valid_http_path as valid_path};

pub fn dns_query(id: u16, name: &str) -> Vec<u8> {
    let mut q = Vec::with_capacity(18 + name.len());
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
    for l in name.split('.') {
        q.push(l.len() as u8);
        q.extend_from_slice(l.as_bytes());
    }
    q.extend_from_slice(&[0, 0, 1, 0, 1]);
    q
}

#[derive(Debug, PartialEq, Eq)]
pub enum DnsErr {
    Short,
    NotOurs,
    Rcode(u8),
    NoAnswer,
    Malformed,
}

/// Skip a (possibly compressed) name at `i`; the index after it.
fn skip_name(b: &[u8], mut i: usize) -> Result<usize, DnsErr> {
    for _ in 0..128 {
        let l = *b.get(i).ok_or(DnsErr::Malformed)? as usize;
        if l == 0 {
            return Ok(i + 1);
        }
        if l & 0xc0 == 0xc0 {
            b.get(i + 1).ok_or(DnsErr::Malformed)?;
            return Ok(i + 2);
        }
        if l & 0xc0 != 0 {
            return Err(DnsErr::Malformed);
        }
        i += 1 + l;
    }
    Err(DnsErr::Malformed)
}

/// The first A record of a reply to `query`.
pub fn dns_answer(query: &[u8], r: &[u8]) -> Result<Ipv4Addr, DnsErr> {
    if r.len() < 12 {
        return Err(DnsErr::Short);
    }
    if r[..2] != query[..2] || r[2] & 0x80 == 0 {
        return Err(DnsErr::NotOurs);
    }
    if r[3] & 0x0f != 0 {
        return Err(DnsErr::Rcode(r[3] & 0x0f));
    }
    let qd = u16::from_be_bytes([r[4], r[5]]);
    let an = u16::from_be_bytes([r[6], r[7]]);
    // The question must be ours, byte for byte (names compared case-blind).
    let q = &query[12..];
    if qd != 1 || r.len() < 12 + q.len() || !r[12..12 + q.len()].eq_ignore_ascii_case(q) {
        return Err(DnsErr::NotOurs);
    }
    let mut i = 12 + q.len();
    for _ in 0..an.min(32) {
        i = skip_name(r, i)?;
        let h = r.get(i..i + 10).ok_or(DnsErr::Malformed)?;
        let (typ, class) = (u16::from_be_bytes([h[0], h[1]]), u16::from_be_bytes([h[2], h[3]]));
        let len = u16::from_be_bytes([h[8], h[9]]) as usize;
        let data = r.get(i + 10..i + 10 + len).ok_or(DnsErr::Malformed)?;
        if typ == 1 && class == 1 && len == 4 {
            return Ok(Ipv4Addr::new(data[0], data[1], data[2], data[3]));
        }
        i += 10 + len;
    }
    Err(DnsErr::NoAnswer)
}

pub fn http_request(host: &str, path: &str) -> Vec<u8> {
    format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: atrium-probe\r\nAccept: */*\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n").into_bytes()
}

#[derive(Debug, PartialEq, Eq)]
pub struct Http {
    pub status: u16,
    /// Printable ASCII only, at most LOCATION_MAX bytes; else dropped.
    pub location: Option<String>,
    /// The body contains the expected text (when one was asked for).
    pub matched: bool,
}

/// Parse what the server sent (all of it, up to HTTP_MAX).
pub fn http_reply(b: &[u8], expect: Option<&[u8]>) -> Option<Http> {
    let b = &b[..b.len().min(HTTP_MAX)];
    let head_end = b.windows(4).position(|w| w == b"\r\n\r\n");
    let head = &b[..head_end.unwrap_or(b.len())];
    let mut lines = head.split(|&c| c == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l));
    let first = lines.next()?;
    let rest = first.strip_prefix(b"HTTP/1.")?;
    if rest.len() < 5 || !matches!(rest[0], b'0' | b'1') || rest[1] != b' ' {
        return None;
    }
    let code = &rest[2..5];
    if !code.iter().all(u8::is_ascii_digit) || rest.get(5).is_some_and(|&c| c != b' ') {
        return None;
    }
    let status = std::str::from_utf8(code).ok()?.parse().ok()?;
    let mut location = None;
    for l in lines {
        if l.len() > 9 && l[..9].eq_ignore_ascii_case(b"location:") {
            let v = l[9..].trim_ascii();
            if !v.is_empty() && v.len() <= LOCATION_MAX && v.iter().all(|&c| (0x21..0x7f).contains(&c)) {
                location = Some(String::from_utf8_lossy(v).into_owned());
            }
        }
    }
    let body = head_end.map_or(&b[..0], |e| &b[e + 4..]);
    let matched = expect.is_some_and(|x| !x.is_empty() && body.windows(x.len()).any(|w| w == x));
    Some(Http { status, location, matched })
}
