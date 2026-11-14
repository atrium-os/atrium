//! ZIP, as OOXML and ODF packages use it: a reader for hostile archives and a writer.
//!
//! The reader trusts nothing: the central directory is bounded ([`MAX_ENTRIES`], name
//! length), every offset is range-checked, an entry inflates to at most what its header
//! declares and at most [`MAX_ENTRY`] bytes, the whole archive to at most
//! [`MAX_TOTAL`] bytes, and a declared size more than [`MAX_RATIO`] times the
//! compressed size is refused (zip bombs). Only "stored" and "deflate" are read;
//! encryption is refused. ZIP64 is read (big sheets need it); the writer writes it when
//! a size or offset needs it.

use std::io::{self, Read, Write};

use miniz_oxide::inflate::stream::{inflate, InflateState};
use miniz_oxide::{DataFormat, MZFlush, MZStatus};

pub const MAX_ENTRIES: usize = 20_000;
pub const MAX_NAME: usize = 1024;
/// The most one entry may inflate to.
pub const MAX_ENTRY: u64 = 4 << 30;
/// The most a whole archive may inflate to.
pub const MAX_TOTAL: u64 = 16 << 30;
/// Declared size over compressed size above which an entry is refused.
pub const MAX_RATIO: u64 = 1000;

#[derive(Debug)]
pub enum ZipError {
    NotZip,
    Bad(&'static str),
    Unsupported(&'static str),
    TooBig(&'static str),
    Io(io::Error),
}

impl std::fmt::Display for ZipError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ZipError::NotZip => write!(f, "not a ZIP package"),
            ZipError::Bad(w) => write!(f, "damaged ZIP package: {w}"),
            ZipError::Unsupported(w) => write!(f, "unsupported ZIP feature: {w}"),
            ZipError::TooBig(w) => write!(f, "ZIP package too big: {w}"),
            ZipError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ZipError {}

impl From<io::Error> for ZipError {
    fn from(e: io::Error) -> Self {
        ZipError::Io(e)
    }
}

/// One file in the archive.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    method: u16,
    crc: u32,
    pub compressed: u64,
    pub size: u64,
    local: u64,
}

/// An archive in memory.
pub struct Archive<'a> {
    data: &'a [u8],
    pub entries: Vec<Entry>,
}

fn u16_at(d: &[u8], at: usize) -> Option<u16> {
    d.get(at..at + 2).map(|b| u16::from_le_bytes([b[0], b[1]]))
}
fn u32_at(d: &[u8], at: usize) -> Option<u32> {
    d.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}
fn u64_at(d: &[u8], at: usize) -> Option<u64> {
    d.get(at..at + 8).map(|b| u64::from_le_bytes(b.try_into().unwrap()))
}

const EOCD: u32 = 0x0605_4b50;
const EOCD64: u32 = 0x0606_4b50;
const EOCD64_LOC: u32 = 0x0706_4b50;
const CEN: u32 = 0x0201_4b50;
const LOC: u32 = 0x0403_4b50;

impl<'a> Archive<'a> {
    pub fn open(data: &'a [u8]) -> Result<Self, ZipError> {
        if data.len() < 22 || u32_at(data, 0) != Some(LOC) {
            return Err(ZipError::NotZip);
        }
        // The end record is in the last 64 KiB + 22 bytes (its comment is at most 64 KiB).
        let lo = data.len().saturating_sub(22 + 65_535);
        let eocd = (lo..=data.len() - 22).rev().find(|&i| u32_at(data, i) == Some(EOCD)).ok_or(ZipError::Bad("no end record"))?;
        let mut count = u16_at(data, eocd + 10).unwrap() as u64;
        let mut cd_size = u32_at(data, eocd + 12).unwrap() as u64;
        let mut cd_off = u32_at(data, eocd + 16).unwrap() as u64;
        if count == 0xFFFF || cd_size == 0xFFFF_FFFF || cd_off == 0xFFFF_FFFF {
            let loc = eocd.checked_sub(20).ok_or(ZipError::Bad("zip64 locator"))?;
            if u32_at(data, loc) != Some(EOCD64_LOC) {
                return Err(ZipError::Bad("zip64 locator"));
            }
            let at = u64_at(data, loc + 8).ok_or(ZipError::Bad("zip64 locator"))?;
            let at = usize::try_from(at).map_err(|_| ZipError::Bad("zip64 end"))?;
            if u32_at(data, at) != Some(EOCD64) {
                return Err(ZipError::Bad("zip64 end"));
            }
            count = u64_at(data, at + 32).ok_or(ZipError::Bad("zip64 end"))?;
            cd_size = u64_at(data, at + 40).ok_or(ZipError::Bad("zip64 end"))?;
            cd_off = u64_at(data, at + 48).ok_or(ZipError::Bad("zip64 end"))?;
        }
        if count as usize > MAX_ENTRIES {
            return Err(ZipError::TooBig("too many entries"));
        }
        let end = cd_off.checked_add(cd_size).ok_or(ZipError::Bad("directory"))?;
        if end > data.len() as u64 {
            return Err(ZipError::Bad("directory past the end"));
        }
        let mut entries = Vec::with_capacity(count as usize);
        let mut p = cd_off as usize;
        let mut total: u64 = 0;
        for _ in 0..count {
            if u32_at(data, p) != Some(CEN) {
                return Err(ZipError::Bad("directory entry"));
            }
            let flags = u16_at(data, p + 8).ok_or(ZipError::Bad("entry"))?;
            let method = u16_at(data, p + 10).ok_or(ZipError::Bad("entry"))?;
            let crc = u32_at(data, p + 16).ok_or(ZipError::Bad("entry"))?;
            let mut compressed = u32_at(data, p + 20).ok_or(ZipError::Bad("entry"))? as u64;
            let mut size = u32_at(data, p + 24).ok_or(ZipError::Bad("entry"))? as u64;
            let nlen = u16_at(data, p + 28).ok_or(ZipError::Bad("entry"))? as usize;
            let xlen = u16_at(data, p + 30).ok_or(ZipError::Bad("entry"))? as usize;
            let clen = u16_at(data, p + 32).ok_or(ZipError::Bad("entry"))? as usize;
            let mut local = u32_at(data, p + 42).ok_or(ZipError::Bad("entry"))? as u64;
            if flags & 1 != 0 {
                return Err(ZipError::Unsupported("encrypted entries"));
            }
            if nlen > MAX_NAME {
                return Err(ZipError::Bad("entry name too long"));
            }
            let name = data.get(p + 46..p + 46 + nlen).ok_or(ZipError::Bad("entry name"))?;
            let name = String::from_utf8_lossy(name).into_owned();
            // ZIP64 extra field: the sizes and offset that were 0xFFFFFFFF, in order.
            let extra = data.get(p + 46 + nlen..p + 46 + nlen + xlen).ok_or(ZipError::Bad("entry extra"))?;
            let mut x = 0;
            while x + 4 <= extra.len() {
                let id = u16_at(extra, x).unwrap();
                let len = u16_at(extra, x + 2).unwrap() as usize;
                let body = extra.get(x + 4..x + 4 + len).ok_or(ZipError::Bad("extra field"))?;
                if id == 1 {
                    let mut q = 0;
                    for v in [&mut size, &mut compressed, &mut local] {
                        if *v == 0xFFFF_FFFF {
                            *v = u64_at(body, q).ok_or(ZipError::Bad("zip64 field"))?;
                            q += 8;
                        }
                    }
                }
                x += 4 + len;
            }
            if size > MAX_ENTRY {
                return Err(ZipError::TooBig("an entry over 4 GiB"));
            }
            if size > compressed.max(1).saturating_mul(MAX_RATIO) {
                return Err(ZipError::TooBig("an entry compressed past any real ratio"));
            }
            total = total.saturating_add(size);
            if total > MAX_TOTAL {
                return Err(ZipError::TooBig("over 16 GiB inflated"));
            }
            entries.push(Entry { name, method, crc, compressed, size, local });
            p = p.checked_add(46 + nlen + xlen + clen).ok_or(ZipError::Bad("directory"))?;
            if p as u64 > end {
                return Err(ZipError::Bad("directory overruns"));
            }
        }
        Ok(Archive { data, entries })
    }

    pub fn find(&self, name: &str) -> Option<&Entry> {
        let name = name.trim_start_matches('/');
        self.entries.iter().find(|e| e.name == name || e.name.eq_ignore_ascii_case(name))
    }

    /// The entry's compressed bytes.
    fn raw(&self, e: &Entry) -> Result<&'a [u8], ZipError> {
        let l = usize::try_from(e.local).map_err(|_| ZipError::Bad("offset"))?;
        if u32_at(self.data, l) != Some(LOC) {
            return Err(ZipError::Bad("local header"));
        }
        let nlen = u16_at(self.data, l + 26).ok_or(ZipError::Bad("local header"))? as usize;
        let xlen = u16_at(self.data, l + 28).ok_or(ZipError::Bad("local header"))? as usize;
        let start = l + 30 + nlen + xlen;
        let len = usize::try_from(e.compressed).map_err(|_| ZipError::Bad("size"))?;
        self.data.get(start..start.checked_add(len).ok_or(ZipError::Bad("size"))?).ok_or(ZipError::Bad("entry past the end"))
    }

    /// A reader over the entry's content, checked against its size and CRC at the end.
    pub fn reader(&self, e: &Entry) -> Result<EntryReader<'a>, ZipError> {
        let raw = self.raw(e)?;
        let inner = match e.method {
            0 => {
                if e.compressed != e.size {
                    return Err(ZipError::Bad("stored entry size"));
                }
                Inner::Stored { data: raw, pos: 0 }
            }
            8 => Inner::Deflate { state: InflateState::new_boxed(DataFormat::Raw), input: raw, pos: 0, done: false },
            _ => return Err(ZipError::Unsupported("compression other than deflate")),
        };
        Ok(EntryReader { inner, out: 0, size: e.size, crc: Crc::new(), want_crc: e.crc })
    }

    /// The whole entry.
    pub fn read(&self, e: &Entry) -> Result<Vec<u8>, ZipError> {
        let mut v = Vec::with_capacity(e.size.min(256 << 20) as usize);
        self.reader(e)?.read_to_end(&mut v)?;
        Ok(v)
    }

    pub fn read_name(&self, name: &str) -> Result<Option<Vec<u8>>, ZipError> {
        match self.find(name) {
            Some(e) => {
                let e = e.clone();
                self.read(&e).map(Some)
            }
            None => Ok(None),
        }
    }
}

enum Inner<'a> {
    Stored { data: &'a [u8], pos: usize },
    Deflate { state: Box<InflateState>, input: &'a [u8], pos: usize, done: bool },
}

/// Streams one entry, inflating as it goes.
pub struct EntryReader<'a> {
    inner: Inner<'a>,
    out: u64,
    size: u64,
    crc: Crc,
    want_crc: u32,
}

fn bad(w: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, w)
}

impl Read for EntryReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let room = (self.size - self.out).min(buf.len() as u64) as usize;
        let n = match &mut self.inner {
            Inner::Stored { data, pos } => {
                let n = room.min(data.len() - *pos);
                buf[..n].copy_from_slice(&data[*pos..*pos + n]);
                *pos += n;
                n
            }
            Inner::Deflate { state, input, pos, done } => {
                if *done {
                    0
                } else if room == 0 {
                    // Declared size reached: the stream must end here.
                    let mut probe = [0u8; 1];
                    let r = inflate(state, &input[*pos..], &mut probe, MZFlush::Finish);
                    *pos += r.bytes_consumed;
                    if r.bytes_written > 0 {
                        return Err(bad("entry larger than declared"));
                    }
                    *done = true;
                    0
                } else {
                    let mut n = 0;
                    while n == 0 {
                        let r = inflate(state, &input[*pos..], &mut buf[..room], MZFlush::None);
                        *pos += r.bytes_consumed;
                        n = r.bytes_written;
                        match r.status {
                            Ok(MZStatus::StreamEnd) => {
                                *done = true;
                                break;
                            }
                            Ok(_) => {
                                if n == 0 && r.bytes_consumed == 0 {
                                    return Err(bad("truncated deflate stream"));
                                }
                            }
                            Err(_) => return Err(bad("bad deflate stream")),
                        }
                    }
                    n
                }
            }
        };
        self.out += n as u64;
        self.crc.update(&buf[..n]);
        if n == 0 || self.out == self.size {
            let ended = match &self.inner {
                Inner::Stored { .. } => true,
                Inner::Deflate { done, .. } => *done,
            };
            if n == 0 || ended {
                if self.out != self.size {
                    return Err(bad("entry shorter than declared"));
                }
                if self.crc.value() != self.want_crc {
                    return Err(bad("entry CRC mismatch"));
                }
            }
        }
        Ok(n)
    }
}

/// CRC-32 (IEEE), table-driven.
#[derive(Clone)]
struct Crc(u32);

fn crc_table() -> &'static [u32; 256] {
    static T: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0u32; 256];
        for (i, e) in t.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *e = c;
        }
        t
    })
}

impl Crc {
    fn new() -> Self {
        Crc(0xFFFF_FFFF)
    }
    fn update(&mut self, b: &[u8]) {
        let t = crc_table();
        let mut c = self.0;
        for &x in b {
            c = t[((c ^ x as u32) & 0xFF) as usize] ^ (c >> 8);
        }
        self.0 = c;
    }
    fn value(&self) -> u32 {
        !self.0
    }
}

pub fn crc32(b: &[u8]) -> u32 {
    let mut c = Crc::new();
    c.update(b);
    c.value()
}

// ---------- writer ----------

struct Written {
    name: String,
    crc: u32,
    compressed: u64,
    size: u64,
    local: u64,
    method: u16,
}

/// Writes an archive, one whole entry at a time.
pub struct ZipWriter<W: Write> {
    out: W,
    at: u64,
    done: Vec<Written>,
    /// miniz level: 1 is fast (the default: a big sheet saves in seconds), 6 the usual.
    pub level: u8,
}

impl<W: Write> ZipWriter<W> {
    pub fn new(out: W) -> Self {
        ZipWriter { out, at: 0, done: Vec::new(), level: 1 }
    }

    fn put(&mut self, b: &[u8]) -> io::Result<()> {
        self.out.write_all(b)?;
        self.at += b.len() as u64;
        Ok(())
    }

    /// Adds `name` with `data`, deflated (or stored when that is no smaller).
    pub fn add(&mut self, name: &str, data: &[u8]) -> io::Result<()> {
        self.add_with(name, data, true)
    }

    /// Adds `name` with `data` stored, never deflated (ODF's `mimetype`).
    pub fn add_stored(&mut self, name: &str, data: &[u8]) -> io::Result<()> {
        self.add_with(name, data, false)
    }

    fn add_with(&mut self, name: &str, data: &[u8], deflate: bool) -> io::Result<()> {
        let crc = crc32(data);
        let packed = if deflate { miniz_oxide::deflate::compress_to_vec(data, self.level) } else { Vec::new() };
        let (method, body): (u16, &[u8]) = if deflate && packed.len() < data.len() { (8, &packed) } else { (0, data) };
        let local = self.at;
        let big = data.len() as u64 >= 0xFFFF_FFFF || body.len() as u64 >= 0xFFFF_FFFF;
        let mut h = Vec::with_capacity(30 + name.len() + 20);
        h.extend_from_slice(&LOC.to_le_bytes());
        h.extend_from_slice(&(if big { 45u16 } else { 20 }).to_le_bytes());
        h.extend_from_slice(&0x0800u16.to_le_bytes()); // UTF-8 names
        h.extend_from_slice(&method.to_le_bytes());
        h.extend_from_slice(&0u16.to_le_bytes()); // time
        h.extend_from_slice(&0x21u16.to_le_bytes()); // date: 1980-01-01
        h.extend_from_slice(&crc.to_le_bytes());
        let (cs, us) = if big { (0xFFFF_FFFFu32, 0xFFFF_FFFFu32) } else { (body.len() as u32, data.len() as u32) };
        h.extend_from_slice(&cs.to_le_bytes());
        h.extend_from_slice(&us.to_le_bytes());
        h.extend_from_slice(&(name.len() as u16).to_le_bytes());
        h.extend_from_slice(&(if big { 20u16 } else { 0 }).to_le_bytes());
        h.extend_from_slice(name.as_bytes());
        if big {
            h.extend_from_slice(&1u16.to_le_bytes());
            h.extend_from_slice(&16u16.to_le_bytes());
            h.extend_from_slice(&(data.len() as u64).to_le_bytes());
            h.extend_from_slice(&(body.len() as u64).to_le_bytes());
        }
        self.put(&h)?;
        self.put(body)?;
        self.done.push(Written { name: name.to_string(), crc, compressed: body.len() as u64, size: data.len() as u64, local, method });
        Ok(())
    }

    /// Writes the central directory and returns the sink.
    pub fn finish(mut self) -> io::Result<W> {
        let cd_off = self.at;
        let done = std::mem::take(&mut self.done);
        for w in &done {
            let big = w.size >= 0xFFFF_FFFF || w.compressed >= 0xFFFF_FFFF || w.local >= 0xFFFF_FFFF;
            let mut h = Vec::with_capacity(46 + w.name.len() + 28);
            h.extend_from_slice(&CEN.to_le_bytes());
            h.extend_from_slice(&(if big { 45u16 } else { 20 }).to_le_bytes());
            h.extend_from_slice(&(if big { 45u16 } else { 20 }).to_le_bytes());
            h.extend_from_slice(&0x0800u16.to_le_bytes());
            h.extend_from_slice(&w.method.to_le_bytes());
            h.extend_from_slice(&0u16.to_le_bytes());
            h.extend_from_slice(&0x21u16.to_le_bytes());
            h.extend_from_slice(&w.crc.to_le_bytes());
            let m = |v: u64| if big { 0xFFFF_FFFFu32 } else { v as u32 };
            h.extend_from_slice(&m(w.compressed).to_le_bytes());
            h.extend_from_slice(&m(w.size).to_le_bytes());
            h.extend_from_slice(&(w.name.len() as u16).to_le_bytes());
            h.extend_from_slice(&(if big { 28u16 } else { 0 }).to_le_bytes());
            h.extend_from_slice(&0u16.to_le_bytes()); // comment
            h.extend_from_slice(&0u16.to_le_bytes()); // disk
            h.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
            h.extend_from_slice(&0u32.to_le_bytes()); // external attrs
            h.extend_from_slice(&m(w.local).to_le_bytes());
            h.extend_from_slice(w.name.as_bytes());
            if big {
                h.extend_from_slice(&1u16.to_le_bytes());
                h.extend_from_slice(&24u16.to_le_bytes());
                h.extend_from_slice(&w.size.to_le_bytes());
                h.extend_from_slice(&w.compressed.to_le_bytes());
                h.extend_from_slice(&w.local.to_le_bytes());
            }
            self.put(&h)?;
        }
        let cd_size = self.at - cd_off;
        let n = done.len() as u64;
        if n >= 0xFFFF || cd_off >= 0xFFFF_FFFF || cd_size >= 0xFFFF_FFFF {
            let at = self.at;
            let mut e = Vec::with_capacity(56 + 20);
            e.extend_from_slice(&EOCD64.to_le_bytes());
            e.extend_from_slice(&44u64.to_le_bytes());
            e.extend_from_slice(&45u16.to_le_bytes());
            e.extend_from_slice(&45u16.to_le_bytes());
            e.extend_from_slice(&0u32.to_le_bytes());
            e.extend_from_slice(&0u32.to_le_bytes());
            e.extend_from_slice(&n.to_le_bytes());
            e.extend_from_slice(&n.to_le_bytes());
            e.extend_from_slice(&cd_size.to_le_bytes());
            e.extend_from_slice(&cd_off.to_le_bytes());
            e.extend_from_slice(&EOCD64_LOC.to_le_bytes());
            e.extend_from_slice(&0u32.to_le_bytes());
            e.extend_from_slice(&at.to_le_bytes());
            e.extend_from_slice(&1u32.to_le_bytes());
            self.put(&e)?;
        }
        let mut e = Vec::with_capacity(22);
        e.extend_from_slice(&EOCD.to_le_bytes());
        e.extend_from_slice(&0u16.to_le_bytes());
        e.extend_from_slice(&0u16.to_le_bytes());
        let n16 = n.min(0xFFFF) as u16;
        e.extend_from_slice(&n16.to_le_bytes());
        e.extend_from_slice(&n16.to_le_bytes());
        e.extend_from_slice(&(cd_size.min(0xFFFF_FFFF) as u32).to_le_bytes());
        e.extend_from_slice(&(cd_off.min(0xFFFF_FFFF) as u32).to_le_bytes());
        e.extend_from_slice(&0u16.to_le_bytes());
        self.put(&e)?;
        Ok(self.out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn archive(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut z = ZipWriter::new(Vec::new());
        for (n, d) in files {
            z.add(n, d).unwrap();
        }
        z.finish().unwrap()
    }

    #[test]
    fn round_trip() {
        let big: Vec<u8> = (0..200_000u32).flat_map(|i| (i % 251).to_le_bytes()).collect();
        let z = archive(&[("a.txt", b"hello"), ("dir/b.xml", &big), ("empty", b"")]);
        let a = Archive::open(&z).unwrap();
        assert_eq!(a.entries.len(), 3);
        assert_eq!(a.read_name("a.txt").unwrap().unwrap(), b"hello");
        assert_eq!(a.read_name("/dir/b.xml").unwrap().unwrap(), big);
        assert_eq!(a.read_name("empty").unwrap().unwrap(), b"");
        assert!(a.read_name("nope").unwrap().is_none());
    }

    #[test]
    fn hostile_archives_fail_cleanly() {
        let z = archive(&[("a.txt", &[7u8; 100_000]), ("b", b"tail")]);
        assert!(matches!(Archive::open(b"PK\x03\x04 not really"), Err(_)));
        assert!(matches!(Archive::open(b"plain text, not a zip at all......"), Err(ZipError::NotZip)));
        // Every truncation and every single-byte flip: an error or the right bytes, never a panic.
        for cut in (0..z.len()).step_by(7) {
            if let Ok(a) = Archive::open(&z[..cut]) {
                for e in a.entries.clone() {
                    let _ = a.read(&e);
                }
            }
        }
        for i in 0..z.len() {
            let mut m = z.clone();
            m[i] ^= 0x5A;
            if let Ok(a) = Archive::open(&m) {
                for e in a.entries.clone() {
                    if let Ok(d) = a.read(&e) {
                        // A flip that survived the CRC must not have changed the content.
                        if e.name == "a.txt" {
                            assert_eq!(d, vec![7u8; 100_000], "flip at {i}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_lying_size_is_caught() {
        let mut z = archive(&[("a.txt", &[1u8; 5000])]);
        // Find the central record and halve its declared size.
        let cen = (0..z.len() - 4).rev().find(|&i| u32_at(&z, i) == Some(CEN)).unwrap();
        let size = u32_at(&z, cen + 24).unwrap();
        z[cen + 24..cen + 28].copy_from_slice(&(size / 2).to_le_bytes());
        let a = Archive::open(&z).unwrap();
        let e = a.entries[0].clone();
        assert!(a.read(&e).is_err());
    }
}
