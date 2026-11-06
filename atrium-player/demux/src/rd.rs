//! A bounds-checked big-endian reader over a byte slice.
use crate::{Error, Result};

pub struct Rd<'a> {
    b: &'a [u8],
    p: usize,
}

const SHORT: Error = Error::Malformed("truncated");

impl<'a> Rd<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Rd { b, p: 0 }
    }
    pub fn left(&self) -> usize {
        self.b.len() - self.p
    }
    pub fn rest(&self) -> &'a [u8] {
        &self.b[self.p..]
    }
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.left() {
            return Err(SHORT);
        }
        let s = &self.b[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }
    pub fn skip(&mut self, n: usize) -> Result<()> {
        self.bytes(n).map(|_| ())
    }
    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.bytes(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16> {
        let b = self.bytes(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    pub fn u32(&mut self) -> Result<u32> {
        let b = self.bytes(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    pub fn u64(&mut self) -> Result<u64> {
        let b = self.bytes(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_be_bytes(a))
    }
}

/// A big-endian unsigned integer of 0 to 8 bytes (EBML uint).
pub fn be_uint(b: &[u8]) -> Result<u64> {
    if b.len() > 8 {
        return Err(Error::Malformed("integer over 8 bytes"));
    }
    Ok(b.iter().fold(0u64, |a, &x| (a << 8) | x as u64))
}

/// An EBML float: 0, 4 or 8 bytes.
pub fn be_float(b: &[u8]) -> Result<f64> {
    match b.len() {
        0 => Ok(0.0),
        4 => Ok(f32::from_bits(be_uint(b)? as u32) as f64),
        8 => Ok(f64::from_bits(be_uint(b)?)),
        _ => Err(Error::Malformed("float size")),
    }
}
