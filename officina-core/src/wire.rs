//! Bounded binary encoding for Officina's files and journals.
//!
//! Little-endian fixed-width integers, LEB128 varints for lengths and counts, UTF-8
//! strings with a length prefix. A [`Reader`] never allocates more than the caller's
//! limit for one item and never reads past its slice; every failure is a [`WireError`],
//! never a panic.

use std::fmt;

/// Why decoding stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    /// The bytes ended in the middle of an item.
    Truncated,
    /// A length or count is over the limit for that item.
    TooLong { what: &'static str, len: u64, max: u64 },
    /// A tag or value that isn't defined.
    Invalid(&'static str),
    /// A string that isn't UTF-8.
    Utf8,
    /// A varint longer than 10 bytes or overflowing 64 bits.
    Varint,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WireError::Truncated => write!(f, "truncated"),
            WireError::TooLong { what, len, max } => write!(f, "{what}: {len} is over the limit {max}"),
            WireError::Invalid(what) => write!(f, "invalid {what}"),
            WireError::Utf8 => write!(f, "string is not UTF-8"),
            WireError::Varint => write!(f, "bad varint"),
        }
    }
}

impl std::error::Error for WireError {}

/// Appends encoded items to a byte vector.
#[derive(Default, Debug, Clone)]
pub struct Writer {
    pub buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
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
    pub fn f64(&mut self, v: f64) {
        self.buf.extend_from_slice(&v.to_bits().to_le_bytes());
    }
    pub fn var(&mut self, mut v: u64) {
        loop {
            let b = (v & 0x7F) as u8;
            v >>= 7;
            if v == 0 {
                self.buf.push(b);
                return;
            }
            self.buf.push(b | 0x80);
        }
    }
    /// A signed varint (zigzag).
    pub fn ivar(&mut self, v: i64) {
        self.var(((v << 1) ^ (v >> 63)) as u64);
    }
    pub fn bytes(&mut self, b: &[u8]) {
        self.var(b.len() as u64);
        self.buf.extend_from_slice(b);
    }
    pub fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

/// Reads encoded items from a slice, with limits.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Reader { data, pos: 0 }
    }
    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }
    pub fn pos(&self) -> usize {
        self.pos
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        if self.remaining() < n {
            return Err(WireError::Truncated);
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    pub fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.take(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16, WireError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    pub fn u32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    pub fn u64(&mut self) -> Result<u64, WireError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    pub fn f64(&mut self) -> Result<f64, WireError> {
        Ok(f64::from_bits(self.u64()?))
    }
    pub fn var(&mut self) -> Result<u64, WireError> {
        let mut v: u64 = 0;
        for i in 0..10 {
            let b = self.u8()?;
            let part = (b & 0x7F) as u64;
            if i == 9 && part > 1 {
                return Err(WireError::Varint);
            }
            v |= part << (7 * i);
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(WireError::Varint)
    }
    pub fn ivar(&mut self) -> Result<i64, WireError> {
        let u = self.var()?;
        Ok(((u >> 1) as i64) ^ -((u & 1) as i64))
    }
    /// A count or length, refused over `max` before anything is allocated.
    pub fn len(&mut self, what: &'static str, max: u64) -> Result<usize, WireError> {
        let n = self.var()?;
        if n > max {
            return Err(WireError::TooLong { what, len: n, max });
        }
        Ok(n as usize)
    }
    /// A count of items each at least `min_item` bytes long: also refused when the
    /// remaining bytes can't hold that many, so a count can't make the caller reserve
    /// more than the input could fill.
    pub fn count(&mut self, what: &'static str, max: u64, min_item: usize) -> Result<usize, WireError> {
        let n = self.len(what, max)?;
        if min_item > 0 && n > self.remaining() / min_item {
            return Err(WireError::Truncated);
        }
        Ok(n)
    }
    pub fn bytes(&mut self, what: &'static str, max: u64) -> Result<&'a [u8], WireError> {
        let n = self.len(what, max)?;
        self.take(n)
    }
    pub fn str(&mut self, what: &'static str, max: u64) -> Result<&'a str, WireError> {
        std::str::from_utf8(self.bytes(what, max)?).map_err(|_| WireError::Utf8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut w = Writer::new();
        w.u8(7);
        w.u16(0xBEEF);
        w.u32(123456);
        w.f64(-2.5);
        w.var(300);
        w.ivar(-5);
        w.ivar(i64::MIN);
        w.str("héllo");
        let b = w.into_bytes();
        let mut r = Reader::new(&b);
        assert_eq!(r.u8().unwrap(), 7);
        assert_eq!(r.u16().unwrap(), 0xBEEF);
        assert_eq!(r.u32().unwrap(), 123456);
        assert_eq!(r.f64().unwrap(), -2.5);
        assert_eq!(r.var().unwrap(), 300);
        assert_eq!(r.ivar().unwrap(), -5);
        assert_eq!(r.ivar().unwrap(), i64::MIN);
        assert_eq!(r.str("s", 16).unwrap(), "héllo");
        assert!(r.is_empty());
    }

    #[test]
    fn limits() {
        let mut w = Writer::new();
        w.str("too long for the limit");
        let b = w.into_bytes();
        assert!(matches!(Reader::new(&b).str("s", 4), Err(WireError::TooLong { .. })));
        // A count bigger than the input could hold is refused before allocating.
        let mut w = Writer::new();
        w.var(1 << 40);
        let b = w.into_bytes();
        assert!(Reader::new(&b).count("n", u64::MAX, 8).is_err());
        assert_eq!(Reader::new(&[0xFF; 11]).var(), Err(WireError::Varint));
        assert_eq!(Reader::new(&[0x80]).var(), Err(WireError::Truncated));
    }
}
