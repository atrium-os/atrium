//! The autosave journal (docs/spec/officina.md §4.4).
//!
//! Records are appended to a file in the document's private autosave folder:
//! `len u32 | crc32 u32 | payload`. After a crash the journal is replayed over the last
//! saved file; a torn last record (a crash in the middle of a write) is where replay
//! stops, and everything before it is kept.

use crate::crc32::crc32;
use std::io::{self, Write};

/// The largest record the reader accepts.
pub const MAX_RECORD: u32 = 64 << 20;

/// Appends records.
pub struct JournalWriter<W: Write> {
    out: W,
}

impl<W: Write> JournalWriter<W> {
    pub fn new(out: W) -> Self {
        JournalWriter { out }
    }
    pub fn append(&mut self, payload: &[u8]) -> io::Result<()> {
        let len = u32::try_from(payload.len()).ok().filter(|&l| l <= MAX_RECORD).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "journal record too big"))?;
        let mut h = [0u8; 8];
        h[..4].copy_from_slice(&len.to_le_bytes());
        h[4..].copy_from_slice(&crc32(payload).to_le_bytes());
        self.out.write_all(&h)?;
        self.out.write_all(payload)
    }
    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
    pub fn into_inner(self) -> W {
        self.out
    }
}

/// What replay found.
#[derive(Debug, PartialEq, Eq)]
pub struct Replay<'a> {
    pub records: Vec<&'a [u8]>,
    /// Bytes after the last whole record (a torn write), not replayed.
    pub torn_bytes: usize,
}

/// Splits a journal into its whole records.
pub fn read(data: &[u8]) -> Replay<'_> {
    let mut records = Vec::new();
    let mut pos = 0;
    while data.len() - pos >= 8 {
        let len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
        let crc = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap());
        if len > MAX_RECORD || data.len() - pos - 8 < len as usize {
            break;
        }
        let p = &data[pos + 8..pos + 8 + len as usize];
        if crc32(p) != crc {
            break;
        }
        records.push(p);
        pos += 8 + len as usize;
    }
    Replay { records, torn_bytes: data.len() - pos }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn torn_tail() {
        let mut w = JournalWriter::new(Vec::new());
        w.append(b"one").unwrap();
        w.append(b"two").unwrap();
        w.append(b"three").unwrap();
        let b = w.into_inner();
        assert_eq!(read(&b).records, vec![&b"one"[..], b"two", b"three"]);
        for cut in 0..b.len() {
            let r = read(&b[..cut]);
            assert!(r.records.len() <= 3);
            assert_eq!(r.torn_bytes + r.records.iter().map(|x| x.len() + 8).sum::<usize>(), cut);
        }
        let mut d = b.clone();
        d[8 + 3 + 8] ^= 1; // first byte of "two"
        assert_eq!(read(&d).records, vec![&b"one"[..]]);
    }
}
