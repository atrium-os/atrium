//! The native container (docs/spec/officina.md §5.1): typed chunks, each with a CRC-32,
//! and an index at the end so a reader can find any chunk without reading the others.
//!
//! ```text
//! header   "OFFICINA" | version u16 | doc kind u16 | reserved u32 (0)
//! chunk*   kind u16 | flags u16 (0) | len u32 | crc32 u32 | payload[len]
//! index    a chunk of kind INDEX: count var, then per chunk: kind u16, offset u64, len u32
//! trailer  index offset u64 | "OFCNEND\0"
//! ```
//!
//! Limits ([`Limits`]) bound every chunk and the chunk count; the reader checks each index
//! entry against the file and the chunk header it points at before handing out a byte,
//! and checks a chunk's CRC when the chunk is read.

use crate::crc32::crc32;
use crate::wire::{Reader, WireError, Writer};
use std::io::{self, Write};

pub const MAGIC: &[u8; 8] = b"OFFICINA";
pub const END_MAGIC: &[u8; 8] = b"OFCNEND\0";
pub const VERSION: u16 = 0;
const HEADER: usize = 16;
const CHUNK_HEADER: usize = 12;
const TRAILER: usize = 16;
/// The index chunk's kind; not available to engines.
pub const INDEX: u16 = 0xFFFF;

/// Which app's document this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum DocKind {
    Sheets = 1,
    Write = 2,
    Slides = 3,
}

impl DocKind {
    fn from_u16(v: u16) -> Option<Self> {
        match v {
            1 => Some(DocKind::Sheets),
            2 => Some(DocKind::Write),
            3 => Some(DocKind::Slides),
            _ => None,
        }
    }
}

/// Reader bounds.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_chunk: u32,
    pub max_chunks: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { max_chunk: 64 << 20, max_chunks: 1 << 22 }
    }
}

#[derive(Debug)]
pub enum ContainerError {
    NotOfficina,
    Version(u16),
    Kind(u16),
    Wire(WireError),
    /// An index entry or chunk header that doesn't fit the file or disagrees with itself.
    Bad(&'static str),
    Crc { chunk: usize },
    Io(io::Error),
}

impl std::fmt::Display for ContainerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContainerError::NotOfficina => write!(f, "not an Officina file"),
            ContainerError::Version(v) => write!(f, "unsupported version {v}"),
            ContainerError::Kind(k) => write!(f, "unknown document kind {k}"),
            ContainerError::Wire(e) => write!(f, "index: {e}"),
            ContainerError::Bad(w) => write!(f, "damaged file: {w}"),
            ContainerError::Crc { chunk } => write!(f, "damaged file: chunk {chunk} fails its checksum"),
            ContainerError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ContainerError {}

impl From<WireError> for ContainerError {
    fn from(e: WireError) -> Self {
        ContainerError::Wire(e)
    }
}

impl From<io::Error> for ContainerError {
    fn from(e: io::Error) -> Self {
        ContainerError::Io(e)
    }
}

/// One chunk's place in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub kind: u16,
    pub offset: u64,
    pub len: u32,
}

/// Writes a container to any `Write`, streaming: chunks go out as they are added, the
/// index and trailer at [`ContainerWriter::finish`].
pub struct ContainerWriter<W: Write> {
    out: W,
    pos: u64,
    index: Vec<Entry>,
}

impl<W: Write> ContainerWriter<W> {
    pub fn new(mut out: W, kind: DocKind) -> io::Result<Self> {
        let mut h = Vec::with_capacity(HEADER);
        h.extend_from_slice(MAGIC);
        h.extend_from_slice(&VERSION.to_le_bytes());
        h.extend_from_slice(&(kind as u16).to_le_bytes());
        h.extend_from_slice(&0u32.to_le_bytes());
        out.write_all(&h)?;
        Ok(ContainerWriter { out, pos: HEADER as u64, index: Vec::new() })
    }

    fn put(&mut self, kind: u16, payload: &[u8]) -> io::Result<Entry> {
        let len = u32::try_from(payload.len()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "chunk over 4 GiB"))?;
        let mut h = [0u8; CHUNK_HEADER];
        h[0..2].copy_from_slice(&kind.to_le_bytes());
        h[4..8].copy_from_slice(&len.to_le_bytes());
        h[8..12].copy_from_slice(&crc32(payload).to_le_bytes());
        self.out.write_all(&h)?;
        self.out.write_all(payload)?;
        let e = Entry { kind, offset: self.pos, len };
        self.pos += (CHUNK_HEADER + payload.len()) as u64;
        Ok(e)
    }

    /// Adds a chunk; returns its index number.
    pub fn add(&mut self, kind: u16, payload: &[u8]) -> io::Result<usize> {
        assert_ne!(kind, INDEX, "INDEX is the container's own kind");
        let e = self.put(kind, payload)?;
        self.index.push(e);
        Ok(self.index.len() - 1)
    }

    pub fn finish(mut self) -> io::Result<W> {
        let mut w = Writer::new();
        w.var(self.index.len() as u64);
        for e in &self.index {
            w.u16(e.kind);
            w.u64(e.offset);
            w.u32(e.len);
        }
        let idx = self.put(INDEX, &w.buf)?;
        let mut t = Vec::with_capacity(TRAILER);
        t.extend_from_slice(&idx.offset.to_le_bytes());
        t.extend_from_slice(END_MAGIC);
        self.out.write_all(&t)?;
        self.out.flush()?;
        Ok(self.out)
    }
}

/// Reads a container held in memory (a mapped or read file).
#[derive(Debug)]
pub struct ContainerReader<'a> {
    data: &'a [u8],
    pub kind: DocKind,
    entries: Vec<Entry>,
}

impl<'a> ContainerReader<'a> {
    pub fn open(data: &'a [u8], limits: Limits) -> Result<Self, ContainerError> {
        if data.len() < HEADER + TRAILER + CHUNK_HEADER || &data[..8] != MAGIC {
            return Err(ContainerError::NotOfficina);
        }
        let version = u16::from_le_bytes([data[8], data[9]]);
        if version != VERSION {
            return Err(ContainerError::Version(version));
        }
        let k = u16::from_le_bytes([data[10], data[11]]);
        let kind = DocKind::from_u16(k).ok_or(ContainerError::Kind(k))?;
        if data[12..16] != [0; 4] {
            return Err(ContainerError::Bad("reserved header bytes"));
        }
        let t = data.len() - TRAILER;
        if &data[t + 8..] != END_MAGIC {
            return Err(ContainerError::Bad("trailer"));
        }
        let idx_off = u64::from_le_bytes(data[t..t + 8].try_into().unwrap());
        let idx = Self::header_at(data, idx_off, t as u64, limits)?;
        if idx.kind != INDEX || idx.offset + CHUNK_HEADER as u64 + idx.len as u64 != t as u64 {
            return Err(ContainerError::Bad("index place"));
        }
        let payload = Self::payload(data, &idx, usize::MAX)?;
        let mut r = Reader::new(payload);
        let n = r.count("chunk count", limits.max_chunks, 14)?;
        let mut entries = Vec::with_capacity(n);
        let mut prev_end = HEADER as u64;
        for _ in 0..n {
            let e = Entry { kind: r.u16()?, offset: r.u64()?, len: r.u32()? };
            // Chunks are in file order, don't overlap, and end before the index.
            if e.offset < prev_end || e.kind == INDEX {
                return Err(ContainerError::Bad("index entry order"));
            }
            let h = Self::header_at(data, e.offset, idx.offset, limits)?;
            if h != e {
                return Err(ContainerError::Bad("index entry disagrees with its chunk"));
            }
            prev_end = e.offset + CHUNK_HEADER as u64 + e.len as u64;
            entries.push(e);
        }
        if !r.is_empty() {
            return Err(ContainerError::Bad("index trailing bytes"));
        }
        Ok(ContainerReader { data, kind, entries })
    }

    /// The chunk header at `off`, which must end (with its payload) by `end`.
    fn header_at(data: &[u8], off: u64, end: u64, limits: Limits) -> Result<Entry, ContainerError> {
        let o = usize::try_from(off).map_err(|_| ContainerError::Bad("offset"))?;
        if off < HEADER as u64 || off.checked_add(CHUNK_HEADER as u64).map_or(true, |x| x > end) {
            return Err(ContainerError::Bad("chunk offset"));
        }
        let h = &data[o..o + CHUNK_HEADER];
        let kind = u16::from_le_bytes([h[0], h[1]]);
        if h[2..4] != [0, 0] {
            return Err(ContainerError::Bad("chunk flags"));
        }
        let len = u32::from_le_bytes(h[4..8].try_into().unwrap());
        if kind != INDEX && len > limits.max_chunk {
            return Err(ContainerError::Bad("chunk over the size limit"));
        }
        if off + CHUNK_HEADER as u64 + len as u64 > end {
            return Err(ContainerError::Bad("chunk runs past its end"));
        }
        Ok(Entry { kind, offset: off, len })
    }

    fn payload(data: &'a [u8], e: &Entry, i: usize) -> Result<&'a [u8], ContainerError> {
        let o = e.offset as usize;
        let crc = u32::from_le_bytes(data[o + 8..o + 12].try_into().unwrap());
        let p = &data[o + CHUNK_HEADER..o + CHUNK_HEADER + e.len as usize];
        if crc32(p) != crc {
            return Err(ContainerError::Crc { chunk: i });
        }
        Ok(p)
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Chunk `i`'s payload, checked against its CRC.
    pub fn chunk(&self, i: usize) -> Result<&'a [u8], ContainerError> {
        let e = self.entries.get(i).ok_or(ContainerError::Bad("no such chunk"))?;
        Self::payload(self.data, e, i)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        let mut w = ContainerWriter::new(Vec::new(), DocKind::Sheets).unwrap();
        w.add(1, b"first").unwrap();
        w.add(2, b"").unwrap();
        w.add(1, &vec![7u8; 5000]).unwrap();
        w.finish().unwrap()
    }

    #[test]
    fn round_trip() {
        let b = sample();
        let r = ContainerReader::open(&b, Limits::default()).unwrap();
        assert_eq!(r.kind, DocKind::Sheets);
        assert_eq!(r.entries().len(), 3);
        assert_eq!(r.chunk(0).unwrap(), b"first");
        assert_eq!(r.chunk(1).unwrap(), b"");
        assert_eq!(r.chunk(2).unwrap().len(), 5000);
    }

    #[test]
    fn damage_is_caught() {
        let b = sample();
        // A flipped payload byte fails that chunk's CRC only.
        let mut d = b.clone();
        d[HEADER + CHUNK_HEADER] ^= 1;
        let r = ContainerReader::open(&d, Limits::default()).unwrap();
        assert!(matches!(r.chunk(0), Err(ContainerError::Crc { chunk: 0 })));
        assert!(r.chunk(2).is_ok());
        // Every single-byte truncation and every byte flip outside payloads is an error,
        // never a panic.
        for n in 0..b.len() {
            let _ = ContainerReader::open(&b[..n], Limits::default());
        }
        for i in 0..b.len() {
            for bit in [1u8, 0x80] {
                let mut d = b.clone();
                d[i] ^= bit;
                if let Ok(r) = ContainerReader::open(&d, Limits::default()) {
                    for c in 0..r.entries().len() {
                        let _ = r.chunk(c);
                    }
                }
            }
        }
        // The chunk size limit applies.
        let tight = Limits { max_chunk: 100, max_chunks: 10 };
        assert!(ContainerReader::open(&b, tight).is_err());
    }
}
