//! The bounded binary codec for Arca's own files.
//!
//! Little-endian, length-prefixed, no recursion and no optional grammar.
//! Every length is checked against what remains and against a hard cap
//! before anything is allocated, so a corrupted or hostile file can make
//! a read fail but never allocate more than its own size.

use crate::Error;

pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Writer {
        Writer { buf: Vec::new() }
    }
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    pub fn raw(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }
    /// A u32 length, then the bytes.
    pub fn bytes(&mut self, b: &[u8]) {
        self.u32(b.len() as u32);
        self.raw(b);
    }
    pub fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
}

impl Default for Writer {
    fn default() -> Self {
        Writer::new()
    }
}

pub struct Reader<'a> {
    b: &'a [u8],
}

impl<'a> Reader<'a> {
    pub fn new(b: &'a [u8]) -> Reader<'a> {
        Reader { b }
    }
    pub fn remaining(&self) -> usize {
        self.b.len()
    }
    pub fn raw(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if n > self.b.len() {
            return Err(Error::Corrupt("truncated"));
        }
        let (h, t) = self.b.split_at(n);
        self.b = t;
        Ok(h)
    }
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let mut a = [0u8; N];
        a.copy_from_slice(self.raw(N)?);
        Ok(a)
    }
    pub fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.raw(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_le_bytes(self.array()?))
    }
    pub fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    pub fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    /// A u32-length-prefixed byte string of at most `cap` bytes.
    pub fn bytes(&mut self, cap: usize) -> Result<&'a [u8], Error> {
        let n = self.u32()? as usize;
        if n > cap {
            return Err(Error::Corrupt("field over its cap"));
        }
        self.raw(n)
    }
    pub fn str(&mut self, cap: usize) -> Result<&'a str, Error> {
        std::str::from_utf8(self.bytes(cap)?).map_err(|_| Error::Corrupt("not utf-8"))
    }
    pub fn end(&self) -> Result<(), Error> {
        if self.b.is_empty() { Ok(()) } else { Err(Error::Corrupt("trailing bytes")) }
    }
}
