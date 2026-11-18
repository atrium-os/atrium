//! Prefixes, as both sides write and check them.

use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;

/// An IPv4 prefix whose host bits are zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prefix4 {
    pub addr: Ipv4Addr,
    pub len: u8,
}

pub fn mask(len: u8) -> u32 {
    if len == 0 { 0 } else { u32::MAX << (32 - u32::from(len.min(32))) }
}

impl Prefix4 {
    /// `a/len` with its host bits cleared; `None` past /32.
    pub fn new(a: Ipv4Addr, len: u8) -> Option<Prefix4> {
        (len <= 32).then(|| Prefix4 { addr: Ipv4Addr::from(u32::from(a) & mask(len)), len })
    }

    pub fn contains(&self, a: Ipv4Addr) -> bool {
        u32::from(a) & mask(self.len) == u32::from(self.addr)
    }

    /// Whether every address of `o` is in `self`.
    pub fn covers(&self, o: &Prefix4) -> bool {
        o.len >= self.len && self.contains(o.addr)
    }

    pub fn overlaps(&self, o: &Prefix4) -> bool {
        self.covers(o) || o.covers(self)
    }

    /// `a.b.c.d/n`; a bare address is a /32.
    pub fn parse(s: &str) -> Option<Prefix4> {
        let (a, l) = match s.split_once('/') {
            Some((a, l)) => (a, l.parse::<u8>().ok().filter(|_| !l.starts_with('+') && l.len() <= 2)?),
            None => (s, 32),
        };
        Prefix4::new(a.parse().ok()?, l)
    }
}

impl std::fmt::Display for Prefix4 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", self.addr, self.len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes() {
        let p = Prefix4::parse("10.99.0.7/24").unwrap();
        assert_eq!(p.to_string(), "10.99.0.0/24");
        assert!(p.contains("10.99.0.200".parse().unwrap()));
        assert!(!p.contains("10.99.1.1".parse().unwrap()));
        assert!(Prefix4::parse("0.0.0.0/0").unwrap().covers(&p));
        assert_eq!(Prefix4::parse("1.2.3.4").unwrap().len, 32);
        assert_eq!(Prefix4::parse("1.2.3.4/33"), None);
        assert_eq!(Prefix4::parse("1.2.3.4/+8"), None);
        assert_eq!(Prefix4::parse("1.2.3/8"), None);
    }
}
