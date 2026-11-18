//! cryptad ↔ engine (vpn.md §3): one socketpair, length-prefixed postcard
//! frames both ways (`u32` little-endian length, then the body).
//!
//! The engine gets its tun as a descriptor (fd 3 at start), never a device
//! node, and its static key only as answers to [`FromEngine::Dh`] for the
//! one profile cryptad started it for.

use crate::net::Prefix4;
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::net::SocketAddrV4;

/// No frame is longer: [`Start`] with [`MAX_ALLOWED`] prefixes fits.
pub const MAX_FRAME: usize = 16 * 1024;
pub const MAX_ALLOWED: usize = 1024;

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Start {
    pub our_public: [u8; 32],
    pub peer_public: [u8; 32],
    pub psk: Option<[u8; 32]>,
    pub endpoint: SocketAddrV4,
    pub keepalive_s: Option<u16>,
    /// Packets from the tunnel are delivered only from these sources, and
    /// packets to the tunnel sent only to these destinations (WireGuard's
    /// cryptokey routing for one peer).
    pub allowed: Vec<Prefix4>,
}

impl std::fmt::Debug for Start {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Start").field("endpoint", &self.endpoint).field("keepalive_s", &self.keepalive_s).field("psk", &self.psk.is_some()).field("allowed", &self.allowed.len()).finish()
    }
}

impl Drop for Start {
    fn drop(&mut self) {
        if let Some(p) = &mut self.psk {
            zeroize::Zeroize::zeroize(p);
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ToEngine {
    Start(Start),
    /// The answer to [`FromEngine::Dh`]; `None`: refused or unavailable.
    Dh(Option<DhShared>),
    /// The network changed: send from the new path (vpn.md §5.6).
    Rebind,
    Stop,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DhShared(pub [u8; 32]);

impl std::fmt::Debug for DhShared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "DhShared(..)")
    }
}

impl Drop for DhShared {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.0);
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum FromEngine {
    /// X25519(our static key, point), please.
    Dh { point: [u8; 32] },
    /// A handshake completed: the tunnel carries traffic.
    Up,
    /// Handshakes stopped getting answers (90 s), or keys were erased.
    Down { why: String },
    /// Every 10 s while running.
    Stats { rx: u64, tx: u64, dropped: u64 },
}

pub fn write<T: Serialize>(w: &mut impl Write, m: &T) -> io::Result<()> {
    let b = postcard::to_stdvec(m).map_err(|_| io::Error::other("encode"))?;
    if b.len() > MAX_FRAME {
        return Err(io::Error::other("frame too large"));
    }
    let mut f = Vec::with_capacity(4 + b.len());
    f.extend_from_slice(&(b.len() as u32).to_le_bytes());
    f.extend_from_slice(&b);
    w.write_all(&f)
}

/// One frame; `Ok(None)` at a clean end before any byte.
pub fn read<T: for<'a> Deserialize<'a>>(r: &mut impl Read) -> io::Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read(&mut len[..1])? {
        0 => return Ok(None),
        _ => r.read_exact(&mut len[1..])?,
    }
    let n = u32::from_le_bytes(len) as usize;
    if n > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut b = zeroize::Zeroizing::new(vec![0u8; n]);
    r.read_exact(&mut b)?;
    decode(&b).map(Some)
}

pub fn decode<T: for<'a> Deserialize<'a>>(b: &[u8]) -> io::Result<T> {
    match postcard::take_from_bytes::<T>(b) {
        Ok((m, rest)) if rest.is_empty() => Ok(m),
        _ => Err(io::Error::new(io::ErrorKind::InvalidData, "bad frame")),
    }
}

/// Frames out of a byte stream read in pieces (a non-blocking socket).
#[derive(Default)]
pub struct Reader {
    buf: Vec<u8>,
}

impl Reader {
    pub fn push(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    /// The next whole frame, if one is here. An error ends the stream.
    pub fn next<T: for<'a> Deserialize<'a>>(&mut self) -> io::Result<Option<T>> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let n = u32::from_le_bytes(self.buf[..4].try_into().unwrap()) as usize;
        if n > MAX_FRAME {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
        }
        if self.buf.len() < 4 + n {
            return Ok(None);
        }
        let m = decode(&self.buf[4..4 + n]);
        zeroize::Zeroize::zeroize(&mut self.buf[..4 + n]);
        self.buf.drain(..4 + n);
        m.map(Some)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_round_trip_and_bad_ones_end_the_stream() {
        let s = ToEngine::Start(Start {
            our_public: [1; 32],
            peer_public: [2; 32],
            psk: Some([3; 32]),
            endpoint: "198.51.100.51:51820".parse().unwrap(),
            keepalive_s: Some(25),
            allowed: (0..MAX_ALLOWED).map(|i| Prefix4::new(std::net::Ipv4Addr::from(0x0a00_0000 | (i as u32) << 8), 24).unwrap()).collect(),
        });
        let mut b = Vec::new();
        write(&mut b, &s).unwrap();
        write(&mut b, &FromEngine::Dh { point: [9; 32] }).unwrap();
        let mut r = &b[..];
        assert_eq!(read::<ToEngine>(&mut r).unwrap(), Some(s.clone()));
        let mut rd = Reader::default();
        for c in b.chunks(7) {
            rd.push(c);
        }
        assert_eq!(rd.next::<ToEngine>().unwrap(), Some(s));
        assert_eq!(rd.next::<FromEngine>().unwrap(), Some(FromEngine::Dh { point: [9; 32] }));
        assert_eq!(rd.next::<FromEngine>().unwrap(), None);
        let mut big = Reader::default();
        big.push(&((MAX_FRAME + 1) as u32).to_le_bytes());
        assert!(big.next::<FromEngine>().is_err());
        let mut junk = Reader::default();
        junk.push(&[2, 0, 0, 0, 0xff, 0xff]);
        assert!(junk.next::<FromEngine>().is_err());
    }
}
