//! The pipe between aura-keys and its EAP helper (`atrium-aura-eap`, one
//! per join, in capability mode): frames of `tag, length (u32 LE), bytes`.
//!
//! To the helper: `c` the profile (once, first), `p` an EAP packet from
//! the AP. From it: `s` an EAP packet to send, `d` the MSK (the end),
//! `f` why it failed (the end), `t` the certificate the user has to decide
//! about (the end), `x` a packet it dropped and why (logged), `g` a
//! SHA-256 digest to sign with the EAP-TLS key, answered with `G` and the
//! DER signature (empty when Arca would not).

use std::io::{self, Write};
use zeroize::Zeroizing;

pub const CONFIG: u8 = b'c';
pub const PACKET: u8 = b'p';
pub const SEND: u8 = b's';
pub const DONE: u8 = b'd';
pub const FAILED: u8 = b'f';
pub const TRUST: u8 = b't';
pub const DROPPED: u8 = b'x';
pub const SIGN: u8 = b'g';
pub const SIGNED: u8 = b'G';

pub const MAX_FRAME: usize = 64 * 1024;

pub fn write(w: &mut impl Write, tag: u8, data: &[u8]) -> io::Result<()> {
    let mut f = Zeroizing::new(Vec::with_capacity(5 + data.len()));
    f.push(tag);
    f.extend_from_slice(&(data.len() as u32).to_le_bytes());
    f.extend_from_slice(data);
    w.write_all(&f)
}

/// Collects bytes and hands out whole frames.
#[derive(Default)]
pub struct Reader {
    buf: Zeroizing<Vec<u8>>,
}

impl Reader {
    pub fn push(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    /// The next whole frame; `Err` when the stream is not frames.
    pub fn next(&mut self) -> Result<Option<(u8, Zeroizing<Vec<u8>>)>, ()> {
        if self.buf.len() < 5 {
            return Ok(None);
        }
        let n = u32::from_le_bytes(self.buf[1..5].try_into().unwrap()) as usize;
        if n > MAX_FRAME {
            return Err(());
        }
        if self.buf.len() < 5 + n {
            return Ok(None);
        }
        let tag = self.buf[0];
        let data = Zeroizing::new(self.buf[5..5 + n].to_vec());
        let rest = self.buf[5 + n..].to_vec();
        *self.buf = rest;
        Ok(Some((tag, data)))
    }
}

/// A `t` frame: the server's DNS names and its chain (end entity first).
pub fn encode_seen(names: &[String], chain: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for (tag, items) in [(b'n', names.iter().map(|n| n.as_bytes()).collect::<Vec<_>>()), (b'c', chain.iter().map(|c| c.as_slice()).collect())] {
        for i in items {
            out.push(tag);
            out.extend_from_slice(&(i.len() as u32).to_le_bytes());
            out.extend_from_slice(i);
        }
    }
    out
}

pub fn decode_seen(mut b: &[u8]) -> Option<(Vec<String>, Vec<Vec<u8>>)> {
    let (mut names, mut chain) = (Vec::new(), Vec::new());
    while !b.is_empty() {
        let n = u32::from_le_bytes(b.get(1..5)?.try_into().ok()?) as usize;
        let v = b.get(5..5 + n)?;
        match b[0] {
            b'n' => names.push(String::from_utf8(v.to_vec()).ok()?),
            b'c' => chain.push(v.to_vec()),
            _ => return None,
        }
        b = &b[5 + n..];
    }
    (!chain.is_empty()).then_some((names, chain))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames() {
        let mut v = Vec::new();
        write(&mut v, SEND, b"abc").unwrap();
        write(&mut v, DONE, &[]).unwrap();
        let mut r = Reader::default();
        r.push(&v[..4]);
        assert_eq!(r.next(), Ok(None));
        r.push(&v[4..]);
        assert_eq!(r.next().unwrap().map(|(t, d)| (t, d.to_vec())), Some((SEND, b"abc".to_vec())));
        assert_eq!(r.next().unwrap().map(|(t, d)| (t, d.to_vec())), Some((DONE, vec![])));
        assert_eq!(r.next(), Ok(None));
        r.push(&[b's', 0xff, 0xff, 0xff, 0xff]);
        assert_eq!(r.next(), Err(()));
        let s = encode_seen(&["a.example".into()], &[vec![1, 2], vec![3]]);
        assert_eq!(decode_seen(&s), Some((vec!["a.example".into()], vec![vec![1, 2], vec![3]])));
        assert_eq!(decode_seen(&s[..s.len() - 1]), None);
    }
}
