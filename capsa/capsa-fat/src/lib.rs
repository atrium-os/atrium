//! FAT12/16/32 for removable media (docs/spec/device-assignment.md §4.6).
//!
//! A USB stick or SD card is untrusted: its bytes are whatever its last
//! writer, or an attacker, put there. This crate is what parses them, inside
//! a jailed FUSE server (never the kernel's `msdosfs`). So:
//!
//! - no input makes it panic, loop, or allocate without bound: every
//!   offset is checked arithmetic, every chain walk is bounded by the
//!   cluster count, every directory by the FAT limit of 65536 entries;
//! - names that cannot be file names here (`/`, NUL, `.`, `..`, empty) are
//!   skipped, never passed up;
//! - structural damage is an error for the one operation, not for the
//!   volume.
//!
//! Device access goes through [`BlockDev`], byte-addressed; [`Aligned`]
//! turns a device that wants whole sectors (FreeBSD's `/dev/daNpM`) into one.

mod bpb;
mod dir;
mod fat;
mod time;
mod volume;
mod write;

pub use bpb::{FatType, Geometry};
pub use dir::{Entry, Loc, ATTR_ARCHIVE, ATTR_DIRECTORY, ATTR_HIDDEN, ATTR_READ_ONLY, ATTR_SYSTEM, ATTR_VOLUME_ID};
pub use volume::{DirRef, FsStat, Volume};

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The device failed (or the volume claims more than the device holds).
    Io(String),
    /// Not a FAT volume we accept; why.
    NotFat(&'static str),
    /// A structure on the volume is damaged; which.
    Corrupt(&'static str),
    NotFound,
    NotDir,
    IsDir,
    Exists,
    NotEmpty,
    NoSpace,
    InvalidName,
    NameTooLong,
    TooBig,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(m) => write!(f, "I/O: {m}"),
            Error::NotFat(m) => write!(f, "not a FAT volume: {m}"),
            Error::Corrupt(m) => write!(f, "damaged volume: {m}"),
            e => write!(f, "{e:?}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// A byte-addressed device: a partition, an image file, memory.
pub trait BlockDev {
    /// Fill `buf` from `off`; short reads are errors.
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<()>;
    fn write_at(&mut self, off: u64, buf: &[u8]) -> Result<()>;
    fn len(&self) -> u64;
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

fn range(len: u64, off: u64, n: usize) -> Result<(usize, usize)> {
    let end = off.checked_add(n as u64).filter(|&e| e <= len).ok_or_else(|| Error::Io(format!("{n} bytes at {off} beyond the device ({len})")))?;
    Ok((off as usize, end as usize))
}

impl BlockDev for Vec<u8> {
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<()> {
        let (a, b) = range(self.len() as u64, off, buf.len())?;
        buf.copy_from_slice(&self[a..b]);
        Ok(())
    }
    fn write_at(&mut self, off: u64, buf: &[u8]) -> Result<()> {
        let (a, b) = range(self.len() as u64, off, buf.len())?;
        self[a..b].copy_from_slice(buf);
        Ok(())
    }
    fn len(&self) -> u64 {
        Vec::len(self) as u64
    }
}

/// A read-only view of bytes (fuzzing, tests): writes fail.
pub struct ReadOnly<'a>(pub &'a [u8]);

impl BlockDev for ReadOnly<'_> {
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<()> {
        let (a, b) = range(self.0.len() as u64, off, buf.len())?;
        buf.copy_from_slice(&self.0[a..b]);
        Ok(())
    }
    fn write_at(&mut self, _: u64, _: &[u8]) -> Result<()> {
        Err(Error::Io("read-only".into()))
    }
    fn len(&self) -> u64 {
        self.0.len() as u64
    }
}

impl BlockDev for std::fs::File {
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<()> {
        use std::os::unix::fs::FileExt;
        FileExt::read_exact_at(self, buf, off).map_err(|e| Error::Io(e.to_string()))
    }
    fn write_at(&mut self, off: u64, buf: &[u8]) -> Result<()> {
        use std::os::unix::fs::FileExt;
        FileExt::write_all_at(self, buf, off).map_err(|e| Error::Io(e.to_string()))
    }
    fn len(&self) -> u64 {
        self.metadata().map(|m| m.len()).unwrap_or(0)
    }
    fn flush(&mut self) -> Result<()> {
        self.sync_data().map_err(|e| Error::Io(e.to_string()))
    }
}

/// Whole-sector I/O over a device that refuses anything else (FreeBSD's
/// disk devices): partial sectors are read, patched and written back.
pub struct Aligned<D> {
    inner: D,
    sector: u64,
    len: u64,
}

impl<D: BlockDev> Aligned<D> {
    /// `len`: the device's size (a disk device's `metadata().len()` is 0;
    /// the caller asks `DIOCGMEDIASIZE`). `sector`: a power of two, 512..=65536.
    pub fn new(inner: D, sector: u64, len: u64) -> Result<Self> {
        if !sector.is_power_of_two() || !(512..=65536).contains(&sector) {
            return Err(Error::Io(format!("sector size {sector}")));
        }
        Ok(Self { inner, sector, len })
    }

    pub fn into_inner(self) -> D {
        self.inner
    }

    fn span(&self, off: u64, n: usize) -> Result<(u64, usize)> {
        let end = off.checked_add(n as u64).filter(|&e| e <= self.len).ok_or_else(|| Error::Io("beyond the device".into()))?;
        let a = off / self.sector * self.sector;
        let b = end.div_ceil(self.sector) * self.sector;
        Ok((a, usize::try_from(b - a).map_err(|_| Error::TooBig)?))
    }
}

impl<D: BlockDev> BlockDev for Aligned<D> {
    fn read_at(&mut self, off: u64, buf: &mut [u8]) -> Result<()> {
        if off % self.sector == 0 && buf.len() as u64 % self.sector == 0 {
            return self.inner.read_at(off, buf);
        }
        let (a, n) = self.span(off, buf.len())?;
        let mut tmp = vec![0u8; n];
        self.inner.read_at(a, &mut tmp)?;
        let s = (off - a) as usize;
        buf.copy_from_slice(&tmp[s..s + buf.len()]);
        Ok(())
    }
    fn write_at(&mut self, off: u64, buf: &[u8]) -> Result<()> {
        if off % self.sector == 0 && buf.len() as u64 % self.sector == 0 {
            return self.inner.write_at(off, buf);
        }
        let (a, n) = self.span(off, buf.len())?;
        let mut tmp = vec![0u8; n];
        self.inner.read_at(a, &mut tmp)?;
        let s = (off - a) as usize;
        tmp[s..s + buf.len()].copy_from_slice(buf);
        self.inner.write_at(a, &tmp)
    }
    fn len(&self) -> u64 {
        self.len
    }
    fn flush(&mut self) -> Result<()> {
        self.inner.flush()
    }
}

pub(crate) fn le16(b: &[u8], o: usize) -> u16 {
    match b.get(o..o + 2) {
        Some(s) => u16::from_le_bytes([s[0], s[1]]),
        None => 0,
    }
}

pub(crate) fn le32(b: &[u8], o: usize) -> u32 {
    match b.get(o..o + 4) {
        Some(s) => u32::from_le_bytes([s[0], s[1], s[2], s[3]]),
        None => 0,
    }
}
