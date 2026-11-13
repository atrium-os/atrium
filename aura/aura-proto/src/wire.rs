//! Bounds-checked little-endian reader and writer. Every access returns
//! `Err` instead of panicking.

use crate::Error;

pub struct Rd<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Rd<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Rd { b, p: 0 }
    }
    pub fn is_empty(&self) -> bool {
        self.p == self.b.len()
    }
    pub fn bytes(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.p.checked_add(n).ok_or(Error::Short)?;
        let s = self.b.get(self.p..end).ok_or(Error::Short)?;
        self.p = end;
        Ok(s)
    }
    /// Everything not yet read.
    pub fn rest(&mut self) -> &'a [u8] {
        let s = &self.b[self.p..];
        self.p = self.b.len();
        s
    }
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        let mut a = [0u8; N];
        a.copy_from_slice(self.bytes(N)?);
        Ok(a)
    }
    pub fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.array::<1>()?[0])
    }
    pub fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_le_bytes(self.array()?))
    }
    pub fn i16(&mut self) -> Result<i16, Error> {
        Ok(i16::from_le_bytes(self.array()?))
    }
    pub fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    pub fn i32(&mut self) -> Result<i32, Error> {
        Ok(i32::from_le_bytes(self.array()?))
    }
    pub fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_le_bytes(self.array()?))
    }
}

pub struct Wr<'a> {
    b: &'a mut [u8],
    p: usize,
}

impl<'a> Wr<'a> {
    pub fn new(b: &'a mut [u8]) -> Self {
        Wr { b, p: 0 }
    }
    pub fn pos(&self) -> usize {
        self.p
    }
    pub fn bytes(&mut self, s: &[u8]) -> Result<(), Error> {
        let end = self.p.checked_add(s.len()).ok_or(Error::BufTooSmall)?;
        self.b.get_mut(self.p..end).ok_or(Error::BufTooSmall)?.copy_from_slice(s);
        self.p = end;
        Ok(())
    }
    pub fn u8(&mut self, v: u8) -> Result<(), Error> {
        self.bytes(&[v])
    }
    pub fn u16(&mut self, v: u16) -> Result<(), Error> {
        self.bytes(&v.to_le_bytes())
    }
    pub fn i16(&mut self, v: i16) -> Result<(), Error> {
        self.bytes(&v.to_le_bytes())
    }
    pub fn u32(&mut self, v: u32) -> Result<(), Error> {
        self.bytes(&v.to_le_bytes())
    }
    pub fn i32(&mut self, v: i32) -> Result<(), Error> {
        self.bytes(&v.to_le_bytes())
    }
    pub fn u64(&mut self, v: u64) -> Result<(), Error> {
        self.bytes(&v.to_le_bytes())
    }
}
