//! SSH wire encoding (RFC 4251 §5): bounded reader and a writer.

use crate::Error;

/// Reads SSH data types from a message, never past its end.
pub struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(b: &'a [u8]) -> Self {
        Reader { b, pos: 0 }
    }
    pub fn rest(&self) -> &'a [u8] {
        &self.b[self.pos..]
    }
    pub fn done(&self) -> bool {
        self.pos == self.b.len()
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.pos.checked_add(n).ok_or(Error::Protocol("length overflow"))?;
        if end > self.b.len() {
            return Err(Error::Protocol("message shorter than its fields"));
        }
        let s = &self.b[self.pos..end];
        self.pos = end;
        Ok(s)
    }
    pub fn byte(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }
    pub fn bool(&mut self) -> Result<bool, Error> {
        Ok(self.byte()? != 0)
    }
    pub fn u32(&mut self) -> Result<u32, Error> {
        let s = self.take(4)?;
        Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
    }
    pub fn u64(&mut self) -> Result<u64, Error> {
        let s = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(s);
        Ok(u64::from_be_bytes(a))
    }
    pub fn string(&mut self) -> Result<&'a [u8], Error> {
        let n = self.u32()? as usize;
        self.take(n)
    }
    pub fn utf8(&mut self) -> Result<&'a str, Error> {
        std::str::from_utf8(self.string()?).map_err(|_| Error::Protocol("string is not UTF-8"))
    }
    /// An mpint's magnitude, positive only, leading zero stripped.
    pub fn mpint_pos(&mut self) -> Result<&'a [u8], Error> {
        let s = self.string()?;
        if s.first().is_some_and(|b| b & 0x80 != 0) {
            return Err(Error::Protocol("negative mpint"));
        }
        Ok(if s.first() == Some(&0) { &s[1..] } else { s })
    }
    pub fn name_list(&mut self) -> Result<Vec<&'a str>, Error> {
        let s = self.utf8()?;
        Ok(if s.is_empty() { Vec::new() } else { s.split(',').collect() })
    }
}

/// Builds an SSH message.
#[derive(Default)]
pub struct Writer(pub Vec<u8>);

impl Writer {
    pub fn new(msg: u8) -> Self {
        Writer(vec![msg])
    }
    pub fn byte(&mut self, b: u8) -> &mut Self {
        self.0.push(b);
        self
    }
    pub fn bool(&mut self, b: bool) -> &mut Self {
        self.byte(b as u8)
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.0.extend_from_slice(&v.to_be_bytes());
        self
    }
    pub fn string(&mut self, s: &[u8]) -> &mut Self {
        self.u32(s.len() as u32);
        self.0.extend_from_slice(s);
        self
    }
    pub fn raw(&mut self, s: &[u8]) -> &mut Self {
        self.0.extend_from_slice(s);
        self
    }
    /// An unsigned big-endian magnitude as an mpint.
    pub fn mpint(&mut self, mag: &[u8]) -> &mut Self {
        let mut m = mag;
        while m.first() == Some(&0) {
            m = &m[1..];
        }
        if m.first().is_some_and(|b| b & 0x80 != 0) {
            self.u32(m.len() as u32 + 1);
            self.0.push(0);
            self.0.extend_from_slice(m);
        } else {
            self.string(m);
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mpint_matches_rfc4251_examples() {
        let mut w = Writer::default();
        w.mpint(&[]);
        assert_eq!(w.0, [0, 0, 0, 0]);
        let mut w = Writer::default();
        w.mpint(&[0x00, 0x80]);
        assert_eq!(w.0, [0, 0, 0, 2, 0, 0x80]);
        let mut w = Writer::default();
        w.mpint(&[0x09, 0xa3, 0x78, 0xf9, 0xb2, 0xe3, 0x32, 0xa7]);
        assert_eq!(w.0, [0, 0, 0, 8, 0x09, 0xa3, 0x78, 0xf9, 0xb2, 0xe3, 0x32, 0xa7]);
    }

    #[test]
    fn reader_refuses_lengths_past_the_end() {
        let mut r = Reader::new(&[0, 0, 0, 9, 1, 2]);
        assert!(r.string().is_err());
        let mut r = Reader::new(&[0xff, 0xff, 0xff, 0xff]);
        assert!(r.string().is_err());
        let mut r = Reader::new(&[0, 0, 0, 1, 0x80]);
        assert!(r.mpint_pos().is_err());
    }
}
