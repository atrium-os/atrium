//! Aura's local service protocol (docs/spec/wifi.md §3.4, §3.5).
//!
//! Over Unix stream sockets: `aura-mlme` listens on `mlme.sock` for
//! atrium-net (scan, join, leave, status, and state events back);
//! `aura-keys` listens on `keys.sock` for `aura-mlme` (association
//! details in, authorized/failed out) and for atrium-net (remember, forget,
//! prepare a credential). Each message is one line, at most [`MAX_LINE`]
//! bytes: a verb, then `key=value` fields. Values are printable ASCII
//! without spaces; anything binary (SSIDs, RSNEs, secrets) is hex. Peers
//! are told apart by their uid (getpeereid), never by what they say.

#![deny(unsafe_code)]

pub mod sock;

use std::io::{self, Read, Write};
use zeroize::Zeroize;

pub const MAX_LINE: usize = 4096;

#[derive(Clone, PartialEq, Eq)]
pub struct Line {
    pub verb: String,
    pub fields: Vec<(String, String)>,
}

impl Drop for Line {
    fn drop(&mut self) {
        // Secrets travel as field values: never leave them in freed memory.
        for (_, v) in &mut self.fields {
            v.zeroize();
        }
    }
}

impl std::fmt::Debug for Line {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.verb)?;
        for (k, v) in &self.fields {
            if k == "secret" {
                write!(f, " {k}=..")?;
            } else {
                write!(f, " {k}={v}")?;
            }
        }
        Ok(())
    }
}

fn token_ok(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_graphic() && b != b'=')
}

fn value_ok(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_graphic())
}

impl Line {
    pub fn new(verb: &str) -> Line {
        Line { verb: verb.into(), fields: Vec::new() }
    }

    pub fn with(mut self, k: &str, v: impl ToString) -> Line {
        self.fields.push((k.into(), v.to_string()));
        self
    }

    pub fn with_hex(self, k: &str, v: &[u8]) -> Line {
        self.with(k, hex(v))
    }

    pub fn get(&self, k: &str) -> Option<&str> {
        self.fields.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.as_str())
    }

    pub fn hex(&self, k: &str) -> Option<Vec<u8>> {
        unhex(self.get(k)?)
    }

    pub fn num<T: std::str::FromStr>(&self, k: &str) -> Option<T> {
        self.get(k)?.parse().ok()
    }

    pub fn mac(&self, k: &str) -> Option<[u8; 6]> {
        parse_mac(self.get(k)?)
    }

    /// Parses one line (without the newline).
    pub fn parse(s: &str) -> Option<Line> {
        let mut it = s.split(' ');
        let verb = it.next()?;
        if !token_ok(verb) {
            return None;
        }
        let mut l = Line::new(verb);
        for f in it {
            let (k, v) = f.split_once('=')?;
            if !token_ok(k) || !value_ok(v) || l.get(k).is_some() {
                return None;
            }
            l.fields.push((k.into(), v.into()));
        }
        Some(l)
    }

    pub fn encode(&self) -> Option<Vec<u8>> {
        if !token_ok(&self.verb) {
            return None;
        }
        let mut s = self.verb.clone();
        for (k, v) in &self.fields {
            if !token_ok(k) || !value_ok(v) {
                return None;
            }
            s.push(' ');
            s.push_str(k);
            s.push('=');
            s.push_str(v);
        }
        s.push('\n');
        (s.len() <= MAX_LINE).then(|| s.into_bytes())
    }

    pub fn send(&self, w: &mut impl Write) -> io::Result<()> {
        let b = self.encode().ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "bad line"))?;
        w.write_all(&b)
    }
}

pub fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push_str(&format!("{x:02x}"));
    }
    if s.is_empty() {
        s.push('-');
    }
    s
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if s == "-" {
        return Some(Vec::new());
    }
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

pub fn fmt_mac(m: &[u8; 6]) -> String {
    m.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let v: Vec<u8> = s.split(':').map(|p| u8::from_str_radix(p, 16).ok()).collect::<Option<_>>()?;
    v.try_into().ok()
}

/// Splits a byte stream into lines, bounded: a peer that sends a line
/// longer than [`MAX_LINE`] is cut off.
#[derive(Default)]
pub struct Reader {
    buf: Vec<u8>,
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.buf.zeroize();
    }
}

pub enum Got {
    Line(Line),
    /// A line that does not parse; the connection may continue.
    Bad,
}

impl Reader {
    /// Reads what is available (non-blocking or blocking stream) and returns
    /// the complete lines. `Err` on EOF, an error, or an over-long line.
    pub fn fill(&mut self, r: &mut impl Read) -> io::Result<Vec<Got>> {
        let mut tmp = [0u8; 2048];
        let n = match r.read(&mut tmp) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => 0,
            Err(e) => return Err(e),
        };
        self.buf.extend_from_slice(&tmp[..n]);
        tmp.zeroize();
        let mut out = Vec::new();
        while let Some(p) = self.buf.iter().position(|&b| b == b'\n') {
            let mut l: Vec<u8> = self.buf.drain(..=p).collect();
            let got = std::str::from_utf8(&l[..p]).ok().and_then(Line::parse);
            l.zeroize();
            out.push(got.map(Got::Line).unwrap_or(Got::Bad));
        }
        if self.buf.len() > MAX_LINE {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let l = Line::new("assoc").with_hex("ssid", b"aura test").with("bssid", fmt_mac(&[2, 0, 0, 0, 0, 1])).with("akm", 6);
        let b = l.encode().unwrap();
        let p = Line::parse(std::str::from_utf8(&b[..b.len() - 1]).unwrap()).unwrap();
        assert_eq!(p, l);
        assert_eq!(p.hex("ssid").unwrap(), b"aura test");
        assert_eq!(p.mac("bssid").unwrap(), [2, 0, 0, 0, 0, 1]);
        assert_eq!(p.num::<u32>("akm"), Some(6));
        assert!(Line::parse("x a=1 a=2").is_none());
        assert!(Line::parse("x a").is_none());
        assert!(Line::parse("").is_none());
        assert!(Line::new("x").with("k", "has space").encode().is_none());
        assert_eq!(unhex("-").unwrap(), b"");
        assert!(unhex("abc").is_none());
        let mut r = Reader::default();
        let mut src: &[u8] = b"a k=1\nb\nbad line=\xff\n";
        let got = r.fill(&mut src).unwrap();
        assert_eq!(got.len(), 3);
        assert!(matches!(&got[2], Got::Bad));
        let long = vec![b'x'; MAX_LINE + 10];
        let mut r = Reader::default();
        assert!(r.fill(&mut &long[..2048]).is_ok());
        assert!(r.fill(&mut &long[2048..4096]).is_ok());
        assert!(r.fill(&mut &long[4096..]).is_err());
    }
}
