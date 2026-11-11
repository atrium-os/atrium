//! The decode worker ↔ `hospes-codec` channel (docs/spec/hospes.md §9).
//!
//! A stream socket carrying messages of `kind: u32, len: u32, payload[len]`, little-endian.
//! The worker sends one coded access unit at a time and reads pictures until `DONE`; the
//! codec answers nothing it was not asked. Everything the codec sends is hostile to the
//! worker (it runs a closed library on attacker bitstreams), so [`Picture::parse`] checks
//! sizes against the worker's limits before the worker touches a byte.
#![no_std]

#[cfg(feature = "std")]
extern crate std;

pub const VERSION: u32 = 1;

// worker → codec
pub const DECODE: u32 = 1;
pub const DRAIN: u32 = 2;
pub const RESET: u32 = 3;
// codec → worker
pub const READY: u32 = 0x81;
pub const PICTURE: u32 = 0x82;
pub const DONE: u32 = 0x83;
pub const FAIL: u32 = 0x84;

/// Largest message either side accepts (an 8K I420 picture is ~50 MB).
pub const MAX_LEN: u32 = 64 << 20;
/// Largest coded access unit the worker sends.
pub const MAX_AU: u32 = 16 << 20;

pub const PIC_HEADER: usize = 16;

pub fn header(kind: u32, len: u32) -> [u8; 8] {
    let mut h = [0u8; 8];
    h[..4].copy_from_slice(&kind.to_le_bytes());
    h[4..].copy_from_slice(&len.to_le_bytes());
    h
}

pub fn parse_header(h: &[u8; 8]) -> (u32, u32) {
    (u32::from_le_bytes([h[0], h[1], h[2], h[3]]), u32::from_le_bytes([h[4], h[5], h[6], h[7]]))
}

/// `PICTURE` payload: `pts: i64, width: u32, height: u32`, then I420 planes packed (Y
/// `w*h`, U and V `ceil(w/2)*ceil(h/2)` each).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Picture<'a> {
    pub pts: i64,
    pub width: u32,
    pub height: u32,
    pub y: &'a [u8],
    pub u: &'a [u8],
    pub v: &'a [u8],
}

pub fn i420_size(w: u32, h: u32) -> Option<usize> {
    let (w, h) = (w as usize, h as usize);
    let c = w.div_ceil(2).checked_mul(h.div_ceil(2))?;
    w.checked_mul(h)?.checked_add(c.checked_mul(2)?)
}

impl<'a> Picture<'a> {
    pub fn header(pts: i64, w: u32, h: u32) -> [u8; PIC_HEADER] {
        let mut b = [0u8; PIC_HEADER];
        b[..8].copy_from_slice(&pts.to_le_bytes());
        b[8..12].copy_from_slice(&w.to_le_bytes());
        b[12..].copy_from_slice(&h.to_le_bytes());
        b
    }

    /// Check a `PICTURE` payload against the worker's size limits.
    pub fn parse(p: &'a [u8], max_w: u32, max_h: u32) -> Option<Picture<'a>> {
        if p.len() < PIC_HEADER {
            return None;
        }
        let pts = i64::from_le_bytes(p[..8].try_into().ok()?);
        let w = u32::from_le_bytes(p[8..12].try_into().ok()?);
        let h = u32::from_le_bytes(p[12..16].try_into().ok()?);
        if w == 0 || h == 0 || w > max_w || h > max_h {
            return None;
        }
        let body = &p[PIC_HEADER..];
        if body.len() != i420_size(w, h)? {
            return None;
        }
        let ys = (w * h) as usize;
        let cs = (w.div_ceil(2) * h.div_ceil(2)) as usize;
        Some(Picture { pts, width: w, height: h, y: &body[..ys], u: &body[ys..ys + cs], v: &body[ys + cs..] })
    }
}

#[cfg(feature = "std")]
pub mod client {
    //! The worker's side.
    use super::*;
    use std::io::{self, Read, Write};
    use std::os::unix::net::UnixStream;
    use std::vec::Vec;

    pub struct Client {
        s: UnixStream,
        buf: Vec<u8>,
        pub max_w: u32,
        pub max_h: u32,
    }

    #[derive(Debug)]
    pub enum Error {
        Io(io::Error),
        /// The codec said something malformed or out of limits.
        Protocol(&'static str),
        /// The codec reported a failure (its code).
        Codec(u32),
    }

    impl From<io::Error> for Error {
        fn from(e: io::Error) -> Self {
            Error::Io(e)
        }
    }

    impl Client {
        /// Wait for the codec's `READY` (it has loaded the library).
        pub fn new(s: UnixStream, max_w: u32, max_h: u32) -> Result<Client, Error> {
            let mut c = Client { s, buf: Vec::new(), max_w, max_h };
            let (k, _) = c.read()?;
            match k {
                READY => Ok(c),
                FAIL => Err(Error::Codec(c.code())),
                _ => Err(Error::Protocol("expected READY")),
            }
        }

        fn code(&self) -> u32 {
            self.buf.get(..4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()))
        }

        fn read(&mut self) -> Result<(u32, usize), Error> {
            let mut h = [0u8; 8];
            self.s.read_exact(&mut h)?;
            let (k, n) = parse_header(&h);
            if n > MAX_LEN {
                return Err(Error::Protocol("message too long"));
            }
            self.buf.resize(n as usize, 0);
            self.s.read_exact(&mut self.buf)?;
            Ok((k, n as usize))
        }

        fn call(&mut self, kind: u32, head: &[u8], body: &[u8], f: &mut dyn FnMut(Picture<'_>)) -> Result<u32, Error> {
            let len = head.len() + body.len();
            if len > MAX_AU as usize {
                return Err(Error::Protocol("access unit too large"));
            }
            self.s.write_all(&header(kind, len as u32))?;
            self.s.write_all(head)?;
            self.s.write_all(body)?;
            loop {
                let (k, _) = self.read()?;
                match k {
                    PICTURE => {
                        let p = Picture::parse(&self.buf, self.max_w, self.max_h).ok_or(Error::Protocol("bad picture"))?;
                        f(p);
                    }
                    DONE => return Ok(self.code()),
                    FAIL => return Err(Error::Codec(self.code())),
                    _ => return Err(Error::Protocol("unexpected message")),
                }
            }
        }

        /// Decode one access unit; `f` sees each picture it releases. Returns the decoder's
        /// status for the unit (0: no error).
        pub fn decode(&mut self, au: &[u8], pts: i64, f: &mut dyn FnMut(Picture<'_>)) -> Result<u32, Error> {
            self.call(DECODE, &pts.to_le_bytes(), au, f)
        }
        /// End of stream: release every held picture.
        pub fn drain(&mut self, f: &mut dyn FnMut(Picture<'_>)) -> Result<u32, Error> {
            self.call(DRAIN, &[], &[], f)
        }
        /// Forget everything (a seek). The next unit must start with parameter sets.
        pub fn reset(&mut self) -> Result<u32, Error> {
            self.call(RESET, &[], &[], &mut |_| {})
        }
    }
}

/// Split an Annex-B stream into access units the way the H0 reference host does: NAL units
/// up to and including the next VCL NAL (types 1 and 5). Yields byte ranges.
pub fn split_annexb(buf: &[u8]) -> AnnexB<'_> {
    AnnexB { buf, i: 0 }
}

pub struct AnnexB<'a> {
    buf: &'a [u8],
    i: usize,
}

impl Iterator for AnnexB<'_> {
    type Item = core::ops::Range<usize>;
    fn next(&mut self) -> Option<Self::Item> {
        let (b, n) = (self.buf, self.buf.len());
        if self.i >= n {
            return None;
        }
        let start = self.i;
        let mut j = start;
        loop {
            let mut s = j;
            while s + 3 < n && !(b[s] == 0 && b[s + 1] == 0 && b[s + 2] == 1) {
                s += 1;
            }
            let ty = if s + 3 < n { b[s + 3] & 0x1f } else { 0 };
            let mut e = s + 3;
            while e + 3 < n && !(b[e] == 0 && b[e + 1] == 0 && (b[e + 2] == 1 || (b[e + 2] == 0 && b[e + 3] == 1))) {
                e += 1;
            }
            if e + 3 >= n {
                e = n;
            }
            j = e;
            if ty == 1 || ty == 5 || j >= n {
                break;
            }
        }
        self.i = j;
        Some(start..j)
    }
}
