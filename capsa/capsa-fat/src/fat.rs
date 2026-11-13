//! The allocation table: entry access through a bounded sector cache,
//! chains, free-space search. Writes go to every copy (or, on FAT32 with
//! mirroring off, the active one) when flushed.

use std::collections::BTreeMap;

use crate::bpb::{FatType, Geometry};
use crate::{BlockDev, Error, Result};

/// Cached FAT sectors before clean ones are dropped.
const CACHE_SECTORS: usize = 2048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    Free,
    Cluster(u32),
    End,
    Bad,
    /// Reserved values, or a cluster number outside the volume.
    Invalid,
}

#[derive(Debug, Default)]
pub struct Table {
    cache: BTreeMap<u64, (Vec<u8>, bool)>,
    /// Where the next free-cluster search starts.
    pub hint: u32,
}

impl Table {
    fn copy_offset(g: &Geometry) -> u64 {
        g.fat_offset + u64::from(g.active_fat.unwrap_or(0)) * g.fat_bytes
    }

    fn sector<'a, D: BlockDev>(&'a mut self, dev: &mut D, g: &Geometry, idx: u64) -> Result<&'a mut (Vec<u8>, bool)> {
        if !self.cache.contains_key(&idx) {
            if self.cache.len() >= CACHE_SECTORS {
                self.flush(dev, g)?;
                self.cache.clear();
            }
            let bps = u64::from(g.bytes_per_sector);
            let mut b = vec![0u8; g.bytes_per_sector as usize];
            dev.read_at(Self::copy_offset(g) + idx * bps, &mut b)?;
            self.cache.insert(idx, (b, false));
        }
        self.cache.get_mut(&idx).ok_or(Error::Corrupt("FAT cache"))
    }

    fn byte<D: BlockDev>(&mut self, dev: &mut D, g: &Geometry, off: u64) -> Result<u8> {
        if off >= g.fat_bytes {
            return Err(Error::Corrupt("FAT entry beyond the FAT"));
        }
        let bps = u64::from(g.bytes_per_sector);
        let (s, i) = (off / bps, (off % bps) as usize);
        Ok(self.sector(dev, g, s)?.0[i])
    }

    fn set_byte<D: BlockDev>(&mut self, dev: &mut D, g: &Geometry, off: u64, v: u8) -> Result<()> {
        if off >= g.fat_bytes {
            return Err(Error::Corrupt("FAT entry beyond the FAT"));
        }
        let bps = u64::from(g.bytes_per_sector);
        let (s, i) = (off / bps, (off % bps) as usize);
        let e = self.sector(dev, g, s)?;
        e.0[i] = v;
        e.1 = true;
        Ok(())
    }

    /// The raw entry for cluster `n` (any n the FAT has room for).
    pub fn raw<D: BlockDev>(&mut self, dev: &mut D, g: &Geometry, n: u32) -> Result<u32> {
        let n64 = u64::from(n);
        Ok(match g.fat_type {
            FatType::Fat12 => {
                let o = n64 + n64 / 2;
                let v = u32::from(self.byte(dev, g, o)?) | u32::from(self.byte(dev, g, o + 1)?) << 8;
                if n & 1 == 1 { v >> 4 } else { v & 0xFFF }
            }
            FatType::Fat16 => u32::from(self.byte(dev, g, n64 * 2)?) | u32::from(self.byte(dev, g, n64 * 2 + 1)?) << 8,
            FatType::Fat32 => {
                let mut v = 0u32;
                for k in 0..4 {
                    v |= u32::from(self.byte(dev, g, n64 * 4 + k)?) << (8 * k);
                }
                v & 0x0FFF_FFFF
            }
        })
    }

    pub fn next<D: BlockDev>(&mut self, dev: &mut D, g: &Geometry, n: u32) -> Result<Next> {
        if !g.valid_cluster(n) {
            return Ok(Next::Invalid);
        }
        let v = self.raw(dev, g, n)?;
        let (bad, eoc) = match g.fat_type {
            FatType::Fat12 => (0xFF7, 0xFF8),
            FatType::Fat16 => (0xFFF7, 0xFFF8),
            FatType::Fat32 => (0x0FFF_FFF7, 0x0FFF_FFF8),
        };
        Ok(match v {
            0 => Next::Free,
            v if v >= eoc => Next::End,
            v if v == bad => Next::Bad,
            v if g.valid_cluster(v) => Next::Cluster(v),
            _ => Next::Invalid,
        })
    }

    /// Set cluster `n`'s entry (`Next::Free`, `End` or `Cluster`).
    pub fn set<D: BlockDev>(&mut self, dev: &mut D, g: &Geometry, n: u32, to: Next) -> Result<()> {
        if !g.valid_cluster(n) {
            return Err(Error::Corrupt("set of an invalid cluster"));
        }
        let v = match (to, g.fat_type) {
            (Next::Free, _) => 0,
            (Next::End, FatType::Fat12) => 0xFFF,
            (Next::End, FatType::Fat16) => 0xFFFF,
            (Next::End, FatType::Fat32) => 0x0FFF_FFFF,
            (Next::Cluster(c), _) if g.valid_cluster(c) => c,
            _ => return Err(Error::Corrupt("bad FAT value")),
        };
        let n64 = u64::from(n);
        match g.fat_type {
            FatType::Fat12 => {
                let o = n64 + n64 / 2;
                let lo = u32::from(self.byte(dev, g, o)?);
                let hi = u32::from(self.byte(dev, g, o + 1)?);
                let old = lo | hi << 8;
                let new = if n & 1 == 1 { (old & 0x000F) | (v << 4) } else { (old & 0xF000) | v };
                self.set_byte(dev, g, o, new as u8)?;
                self.set_byte(dev, g, o + 1, (new >> 8) as u8)?;
            }
            FatType::Fat16 => {
                self.set_byte(dev, g, n64 * 2, v as u8)?;
                self.set_byte(dev, g, n64 * 2 + 1, (v >> 8) as u8)?;
            }
            FatType::Fat32 => {
                // The top four bits are reserved: keep them.
                let top = self.byte(dev, g, n64 * 4 + 3)? & 0xF0;
                let b = v.to_le_bytes();
                for k in 0..3 {
                    self.set_byte(dev, g, n64 * 4 + k as u64, b[k])?;
                }
                self.set_byte(dev, g, n64 * 4 + 3, top | (b[3] & 0x0F))?;
            }
        }
        Ok(())
    }

    /// The clusters of the chain starting at `first`, in order. A loop,
    /// a free or bad entry, or an out-of-range link is `Corrupt`.
    pub fn chain<D: BlockDev>(&mut self, dev: &mut D, g: &Geometry, first: u32) -> Result<Vec<u32>> {
        let mut out = Vec::new();
        let mut c = first;
        loop {
            if !g.valid_cluster(c) {
                return Err(Error::Corrupt("chain leaves the volume"));
            }
            if out.len() as u64 > u64::from(g.clusters) {
                return Err(Error::Corrupt("chain loops"));
            }
            out.push(c);
            match self.next(dev, g, c)? {
                Next::End => return Ok(out),
                Next::Cluster(n) => c = n,
                Next::Free => return Err(Error::Corrupt("chain runs into a free cluster")),
                Next::Bad => return Err(Error::Corrupt("chain runs into a bad cluster")),
                Next::Invalid => return Err(Error::Corrupt("chain link out of range")),
            }
        }
    }

    /// One free cluster, marked end-of-chain.
    pub fn alloc<D: BlockDev>(&mut self, dev: &mut D, g: &Geometry) -> Result<u32> {
        let start = if g.valid_cluster(self.hint) { self.hint } else { 2 };
        let mut c = start;
        for _ in 0..g.clusters {
            if self.next(dev, g, c)? == Next::Free {
                self.set(dev, g, c, Next::End)?;
                self.hint = if c + 1 < g.clusters + 2 { c + 1 } else { 2 };
                return Ok(c);
            }
            c = if c + 1 < g.clusters + 2 { c + 1 } else { 2 };
        }
        Err(Error::NoSpace)
    }

    pub fn count_free<D: BlockDev>(&mut self, dev: &mut D, g: &Geometry) -> Result<u32> {
        let mut n = 0;
        for c in 2..g.clusters + 2 {
            if self.next(dev, g, c)? == Next::Free {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Write dirty sectors to every FAT copy in use.
    pub fn flush<D: BlockDev>(&mut self, dev: &mut D, g: &Geometry) -> Result<()> {
        let bps = u64::from(g.bytes_per_sector);
        let copies: Vec<u8> = match g.active_fat {
            Some(a) => vec![a],
            None => (0..g.num_fats).collect(),
        };
        for (idx, (b, dirty)) in self.cache.iter_mut() {
            if *dirty {
                for &k in &copies {
                    dev.write_at(g.fat_offset + u64::from(k) * g.fat_bytes + idx * bps, b)?;
                }
                *dirty = false;
            }
        }
        Ok(())
    }
}
