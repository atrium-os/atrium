//! exFAT, write side: create, write, truncate, unlink, rmdir, rename,
//! times.
//!
//! - **Allocation** is the bitmap's: a cluster is taken by setting its bit,
//!   never by reading the FAT (a contiguous file's FAT entries mean
//!   nothing). Every file this side grows gets a FAT chain; a contiguous
//!   (`NoFatChain`) file is given one first.
//! - **Entry sets** (File, Stream, Names) are written whole with their
//!   checksum and the name hash, which goes through the volume's up-case
//!   table, as Windows looks names up by it.
//! - **Directories** have no `.`/`..` entries; one grows by a zeroed
//!   cluster and its stream entry's length follows.
//! - **VolumeDirty** is set in the boot sector at the first change and
//!   cleared by [`ExVolume::sync`] once everything is written; the boot
//!   checksum leaves that field (and PercentInUse) out.
//!
//! The same distrust as reading: every length, cluster and index is
//! checked; a damaged structure fails the one operation.

use std::collections::BTreeSet;

use crate::exfat::{entry_set_checksum, ExDir, ExEntry, ExVolume};
use crate::fat::Next;
use crate::time::from_unix;
use crate::{le16, BlockDev, Error, Result};

const ATTR_DIRECTORY: u16 = 0x10;
const ATTR_ARCHIVE: u16 = 0x20;
/// Stream flags: AllocationPossible, NoFatChain.
const ALLOC_POSSIBLE: u8 = 0x01;
const NO_FAT_CHAIN: u8 = 0x02;
/// Boot sector VolumeFlags: VolumeDirty.
const VOLUME_DIRTY: u16 = 0x02;
/// Characters an exFAT name may not hold (besides controls).
const NAME_BAD: &[char] = &['"', '*', '/', ':', '<', '>', '?', '\\', '|'];
/// Largest file (the format allows more; the heap never holds more).
const MAX_FILE: u64 = 1 << 44;
/// Largest directory this side grows (the format's limit is 256 MiB).
const MAX_DIR: u64 = crate::exfat::MAX_DIR_BYTES;

/// The allocation bitmap, in memory once anything is written.
#[derive(Debug)]
pub(crate) struct Bits {
    data: Vec<u8>,
    /// The bitmap's own clusters.
    at: Vec<u32>,
    /// Dirty chunks (of `chunk` bytes).
    dirty: BTreeSet<usize>,
    chunk: usize,
    pub(crate) free: u32,
    hint: u32,
}

#[derive(Debug, Default)]
pub(crate) struct State {
    pub(crate) bits: Option<Bits>,
    /// Up-case table, by UTF-16 unit (identity where absent).
    pub(crate) upcase: Option<Vec<u16>>,
    dirty_set: bool,
    /// The last file's chain: (first cluster, clusters), so a long write
    /// does not walk the FAT for every block.
    chain: Option<(u32, Vec<u32>)>,
    /// Directory (first cluster) -> its entry (parent, index).
    parents: std::collections::HashMap<u32, (ExDir, u32)>,
}

/// A name this volume can hold.
pub fn creatable(name: &str) -> Result<()> {
    if !crate::dir::name_ok(name) || name.chars().any(|c| NAME_BAD.contains(&c)) {
        return Err(Error::InvalidName);
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err(Error::InvalidName);
    }
    if name.encode_utf16().count() > 255 {
        return Err(Error::NameTooLong);
    }
    Ok(())
}

/// The up-case table as stored: `0xFFFF, n` skips `n` identity entries.
fn decompress_upcase(raw: &[u8]) -> Vec<u16> {
    let mut t: Vec<u16> = (0..=u16::MAX).collect();
    let units: Vec<u16> = raw.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let mut i = 0usize;
    let mut k = 0usize;
    while i < units.len() && k < t.len() {
        if units[i] == 0xFFFF && i + 1 < units.len() {
            k = k.saturating_add(usize::from(units[i + 1]));
            i += 2;
        } else {
            t[k] = units[i];
            k += 1;
            i += 1;
        }
    }
    t
}

fn ascii_upcase() -> Vec<u16> {
    (0..=u16::MAX).map(|c| if (0x61..=0x7A).contains(&c) { c - 0x20 } else { c }).collect()
}

/// exFAT's timestamp: (date << 16 | time, 10 ms increment, UTC offset byte).
fn stamp(t: i64) -> (u32, u8, u8) {
    let (d, h) = from_unix(t);
    (u32::from(d) << 16 | u32::from(h), if t.rem_euclid(2) == 1 { 100 } else { 0 }, 0x80)
}

impl<D: BlockDev> ExVolume<D> {
    /// Load the up-case table (at open); ASCII when it is missing or broken.
    pub(crate) fn load_upcase(&mut self) {
        let t = self
            .upcase_at
            .filter(|&(_, len)| len >= 2 && len <= 256 * 1024)
            .and_then(|(first, len)| {
                let cl = self.clusters(first, len, false).or_else(|_| self.clusters(first, len, true)).ok()?;
                let cs = self.g.cluster_size as usize;
                let mut raw = vec![0u8; len as usize];
                for (i, &c) in cl.iter().enumerate() {
                    let a = i * cs;
                    if a >= raw.len() {
                        break;
                    }
                    let end = (a + cs).min(raw.len());
                    let o = self.g.cluster_offset(c)?;
                    self.dev.read_at(o, &mut raw[a..end]).ok()?;
                }
                Some(decompress_upcase(&raw))
            });
        self.w.upcase = Some(t.unwrap_or_else(ascii_upcase));
    }

    fn up(&self, u: u16) -> u16 {
        match &self.w.upcase {
            Some(t) => t[usize::from(u)],
            None => if (0x61..=0x7A).contains(&u) { u - 0x20 } else { u },
        }
    }

    /// `name` through the up-case table, for comparing names.
    pub fn upcased(&self, name: &str) -> Vec<u16> {
        name.encode_utf16().map(|u| self.up(u)).collect()
    }

    fn name_hash(&self, name: &str) -> u16 {
        let mut h = 0u16;
        for u in name.encode_utf16() {
            for b in self.up(u).to_le_bytes() {
                h = (h >> 1 | h << 15).wrapping_add(u16::from(b));
            }
        }
        h
    }

    // ---- the allocation bitmap ----

    fn bits(&mut self) -> Result<&mut Bits> {
        if self.w.bits.is_none() {
            let (first, len) = self.bitmap.ok_or(Error::Corrupt("no allocation bitmap"))?;
            let need = u64::from(self.g.clusters).div_ceil(8);
            if len < need {
                return Err(Error::Corrupt("allocation bitmap too small"));
            }
            let at = self.clusters(first, need, false).or_else(|_| self.clusters(first, need, true))?;
            let cs = self.g.cluster_size as usize;
            let mut data = vec![0u8; need as usize];
            for (i, &c) in at.iter().enumerate() {
                let a = i * cs;
                if a >= data.len() {
                    break;
                }
                let end = (a + cs).min(data.len());
                let o = self.g.cluster_offset(c).ok_or(Error::Corrupt("bitmap cluster"))?;
                self.dev.read_at(o, &mut data[a..end])?;
            }
            // Bits past the last cluster are not clusters.
            let n = self.g.clusters as usize;
            if n % 8 != 0 {
                if let Some(l) = data.last_mut() {
                    *l &= (1u8 << (n % 8)) - 1;
                }
            }
            let used: u64 = data.iter().map(|b| u64::from(b.count_ones())).sum();
            let free = u32::try_from(u64::from(self.g.clusters).saturating_sub(used)).unwrap_or(0);
            let chunk = cs.min(4096);
            self.w.bits = Some(Bits { data, at, dirty: BTreeSet::new(), chunk, free, hint: 2 });
        }
        self.w.bits.as_mut().ok_or(Error::Corrupt("bitmap"))
    }

    fn bit(&mut self, c: u32) -> Result<bool> {
        if !self.g.valid_cluster(c) {
            return Err(Error::Corrupt("cluster out of range"));
        }
        let b = self.bits()?;
        let i = (c - 2) as usize;
        Ok(b.data[i / 8] & (1 << (i % 8)) != 0)
    }

    fn set_bit(&mut self, c: u32, on: bool) -> Result<()> {
        if !self.g.valid_cluster(c) {
            return Err(Error::Corrupt("cluster out of range"));
        }
        let b = self.bits()?;
        let i = (c - 2) as usize;
        let was = b.data[i / 8] & (1 << (i % 8)) != 0;
        if was == on {
            return Ok(());
        }
        if on {
            b.data[i / 8] |= 1 << (i % 8);
            b.free = b.free.saturating_sub(1);
        } else {
            b.data[i / 8] &= !(1 << (i % 8));
            b.free += 1;
        }
        b.dirty.insert(i / 8 / b.chunk);
        Ok(())
    }

    /// One free cluster, marked used (not linked).
    fn take_cluster(&mut self) -> Result<u32> {
        let n = self.g.clusters;
        let start = {
            let b = self.bits()?;
            if b.free == 0 {
                return Err(Error::NoSpace);
            }
            b.hint
        };
        let mut c = if self.g.valid_cluster(start) { start } else { 2 };
        for _ in 0..n {
            if !self.bit(c)? {
                self.set_bit(c, true)?;
                let next = if c + 1 < n + 2 { c + 1 } else { 2 };
                self.bits()?.hint = next;
                return Ok(c);
            }
            c = if c + 1 < n + 2 { c + 1 } else { 2 };
        }
        Err(Error::NoSpace)
    }

    fn flush_bits(&mut self) -> Result<()> {
        let cs = self.g.cluster_size as usize;
        let Some(b) = self.w.bits.as_mut() else { return Ok(()) };
        for k in std::mem::take(&mut b.dirty) {
            let a = k * b.chunk;
            let end = (a + b.chunk).min(b.data.len());
            let ci = a / cs;
            let c = *b.at.get(ci).ok_or(Error::Corrupt("bitmap shorter than its clusters"))?;
            let o = self.g.cluster_offset(c).ok_or(Error::Corrupt("bitmap cluster"))? + (a % cs) as u64;
            self.dev.write_at(o, &b.data[a..end])?;
        }
        Ok(())
    }

    // ---- the boot sector's dirty flag ----

    fn mark_dirty(&mut self) -> Result<()> {
        if self.w.dirty_set {
            return Ok(());
        }
        let mut f = [0u8; 2];
        self.dev.read_at(106, &mut f)?;
        let v = u16::from_le_bytes(f) | VOLUME_DIRTY;
        self.dev.write_at(106, &v.to_le_bytes())?;
        self.dev.flush()?;
        self.w.dirty_set = true;
        Ok(())
    }

    /// Write the FAT and bitmap, then clear VolumeDirty and set PercentInUse.
    pub fn sync(&mut self) -> Result<()> {
        if !self.w.dirty_set {
            return Ok(());
        }
        self.fat.flush(&mut self.dev, &self.g)?;
        self.flush_bits()?;
        self.dev.flush()?;
        let used = self.w.bits.as_ref().map(|b| self.g.clusters - b.free.min(self.g.clusters));
        let pct = used.map_or(0xFF, |u| (u64::from(u) * 100 / u64::from(self.g.clusters.max(1))) as u8);
        let mut f = [0u8; 2];
        self.dev.read_at(106, &mut f)?;
        let v = u16::from_le_bytes(f) & !VOLUME_DIRTY;
        self.dev.write_at(106, &v.to_le_bytes())?;
        self.dev.write_at(112, &[pct])?;
        self.dev.flush()?;
        self.w.dirty_set = false;
        Ok(())
    }

    // ---- clusters of a file ----

    /// A file's (or directory's) clusters, for `alloc` bytes.
    fn chain_of(&mut self, first: u32, alloc: u64, contiguous: bool) -> Result<Vec<u32>> {
        if first == 0 {
            return Ok(Vec::new());
        }
        if !contiguous {
            if let Some((f, c)) = &self.w.chain {
                if *f == first {
                    return Ok(c.clone());
                }
            }
        }
        let c = self.clusters(first, alloc, contiguous)?;
        let cs = u64::from(self.g.cluster_size);
        let want = alloc.div_ceil(cs) as usize;
        if c.len() < want {
            return Err(Error::Corrupt("chain shorter than the data"));
        }
        if !contiguous {
            self.w.chain = Some((first, c.clone()));
        }
        Ok(c)
    }

    /// Give a contiguous run a FAT chain, so it can grow anywhere.
    fn chain_run(&mut self, run: &[u32]) -> Result<()> {
        for w in run.windows(2) {
            self.fat.set(&mut self.dev, &self.g, w[0], Next::Cluster(w[1]))?;
        }
        if let Some(&l) = run.last() {
            self.fat.set(&mut self.dev, &self.g, l, Next::End)?;
        }
        Ok(())
    }

    /// Grow `chain` to `n` clusters (linked); new clusters are zeroed when
    /// `zero` (directories).
    fn grow_to(&mut self, chain: &mut Vec<u32>, n: usize, zero: bool) -> Result<()> {
        let cs = self.g.cluster_size as usize;
        while chain.len() < n {
            let c = match self.take_cluster() {
                Ok(c) => c,
                Err(e) => {
                    self.w.chain = None;
                    return Err(e);
                }
            };
            self.fat.set(&mut self.dev, &self.g, c, Next::End)?;
            if let Some(&l) = chain.last() {
                self.fat.set(&mut self.dev, &self.g, l, Next::Cluster(c))?;
            }
            if zero {
                let o = self.g.cluster_offset(c).ok_or(Error::Corrupt("cluster"))?;
                self.dev.write_at(o, &vec![0u8; cs])?;
            }
            chain.push(c);
        }
        Ok(())
    }

    fn free_run(&mut self, clusters: &[u32], chained: bool) -> Result<()> {
        for &c in clusters {
            self.set_bit(c, false)?;
            if chained {
                self.fat.set(&mut self.dev, &self.g, c, Next::Free)?;
            }
        }
        self.w.chain = None;
        Ok(())
    }

    /// Write `data` at byte `off` of the clusters `chain`.
    fn put(&mut self, chain: &[u32], off: u64, data: &[u8]) -> Result<()> {
        let cs = u64::from(self.g.cluster_size);
        let mut done = 0usize;
        while done < data.len() {
            let p = off + done as u64;
            let c = *chain.get((p / cs) as usize).ok_or(Error::Corrupt("write past the chain"))?;
            let within = p % cs;
            let take = ((cs - within) as usize).min(data.len() - done);
            let o = self.g.cluster_offset(c).ok_or(Error::Corrupt("cluster"))?;
            self.dev.write_at(o + within, &data[done..done + take])?;
            done += take;
        }
        Ok(())
    }

    // ---- entry sets ----

    /// The entry set at `index` of `dir`, as bytes, with where each of its
    /// entries lives on the device.
    fn read_set(&mut self, dir: ExDir, index: u32) -> Result<(Vec<u8>, Vec<u64>)> {
        let cl = self.dir_clusters(dir)?;
        let cs = u64::from(self.g.cluster_size);
        let at = |k: u64| -> Result<u64> {
            let p = k * 32;
            let c = *cl.get((p / cs) as usize).ok_or(Error::Corrupt("entry beyond the directory"))?;
            Ok(self.g.cluster_offset(c).ok_or(Error::Corrupt("cluster"))? + p % cs)
        };
        let mut first = [0u8; 32];
        let o0 = at(u64::from(index))?;
        self.dev.read_at(o0, &mut first)?;
        if first[0] != 0x85 || !(2..=18).contains(&first[1]) {
            return Err(Error::NotFound);
        }
        let n = 1 + usize::from(first[1]);
        let mut set = first.to_vec();
        let mut offs = vec![o0];
        for k in 1..n as u64 {
            let o = at(u64::from(index) + k)?;
            let mut e = [0u8; 32];
            self.dev.read_at(o, &mut e)?;
            set.extend_from_slice(&e);
            offs.push(o);
        }
        if le16(&set, 2) != entry_set_checksum(&set) {
            return Err(Error::Corrupt("entry set checksum"));
        }
        Ok((set, offs))
    }

    fn write_set(&mut self, set: &mut [u8], offs: &[u64]) -> Result<()> {
        let sum = entry_set_checksum(set);
        set[2..4].copy_from_slice(&sum.to_le_bytes());
        for (k, o) in offs.iter().enumerate() {
            self.dev.write_at(*o, &set[k * 32..k * 32 + 32])?;
        }
        Ok(())
    }

    /// A new entry set for `name`.
    fn new_set(&self, name: &str, attr: u16, first: u32, len: u64, valid: u64, flags: u8, now: i64) -> Vec<u8> {
        let units: Vec<u16> = name.encode_utf16().collect();
        let names = units.len().div_ceil(15);
        let mut s = vec![0u8; (2 + names) * 32];
        s[0] = 0x85;
        s[1] = (1 + names) as u8;
        s[4..6].copy_from_slice(&attr.to_le_bytes());
        let (t, ms, utc) = stamp(now);
        for (o, m, u) in [(8, 20, 22), (12, 21, 23), (16, usize::MAX, 24)] {
            s[o..o + 4].copy_from_slice(&t.to_le_bytes());
            if m != usize::MAX {
                s[m] = ms;
            }
            s[u] = utc;
        }
        let st = &mut s[32..64];
        st[0] = 0xC0;
        st[1] = flags;
        st[3] = units.len() as u8;
        st[4..6].copy_from_slice(&self.name_hash(name).to_le_bytes());
        st[8..16].copy_from_slice(&valid.to_le_bytes());
        st[20..24].copy_from_slice(&first.to_le_bytes());
        st[24..32].copy_from_slice(&len.to_le_bytes());
        for (k, chunk) in units.chunks(15).enumerate() {
            let e = &mut s[(2 + k) * 32..(3 + k) * 32];
            e[0] = 0xC1;
            for (j, u) in chunk.iter().enumerate() {
                e[2 + 2 * j..4 + 2 * j].copy_from_slice(&u.to_le_bytes());
            }
        }
        s
    }

    /// Room for `n` entries in `dir`: the first free run, growing the
    /// directory when there is none. Its index and each entry's offset.
    fn place(&mut self, dir: ExDir, n: usize) -> Result<(u32, Vec<u64>, ExDir)> {
        let cs = u64::from(self.g.cluster_size);
        let mut d = dir;
        loop {
            let data = self.dir_bytes(d)?;
            let total = data.len() / 32;
            let mut run = 0usize;
            let mut found = None;
            for i in 0..total {
                if data[i * 32] & 0x80 == 0 {
                    run += 1;
                    if run == n {
                        found = Some(i + 1 - n);
                        break;
                    }
                } else {
                    run = 0;
                }
            }
            if let Some(start) = found {
                let cl = self.dir_clusters(d)?;
                let offs = (start..start + n)
                    .map(|k| {
                        let p = k as u64 * 32;
                        let c = cl[(p / cs) as usize];
                        self.g.cluster_offset(c).map(|o| o + p % cs).ok_or(Error::Corrupt("cluster"))
                    })
                    .collect::<Result<Vec<u64>>>()?;
                return Ok((start as u32, offs, d));
            }
            d = self.grow_dir(d)?;
        }
    }

    /// One more zeroed cluster for `dir`; its stream entry follows (the
    /// root has none).
    fn grow_dir(&mut self, dir: ExDir) -> Result<ExDir> {
        let cs = u64::from(self.g.cluster_size);
        let mut cl = self.dir_clusters(dir)?;
        if (cl.len() as u64 + 1) * cs > MAX_DIR {
            return Err(Error::NoSpace);
        }
        if dir.contiguous {
            self.chain_run(&cl)?;
        }
        let n = cl.len() + 1;
        self.grow_to(&mut cl, n, true)?;
        self.w.chain = None;
        let len = cl.len() as u64 * cs;
        let grown = ExDir { first: dir.first, len, contiguous: false };
        if dir.first != self.g.root_cluster {
            let e = self.find_dir_entry(dir)?;
            let (mut set, offs) = self.read_set(e.dir, e.index)?;
            set[33] &= !NO_FAT_CHAIN;
            set[40..48].copy_from_slice(&len.to_le_bytes());
            set[56..64].copy_from_slice(&len.to_le_bytes());
            self.write_set(&mut set, &offs)?;
        }
        Ok(grown)
    }

    /// The entry of directory `d` (not the root), found from the root.
    /// exFAT keeps no parent links; this side remembers them instead.
    fn find_dir_entry(&mut self, d: ExDir) -> Result<ExEntry> {
        if let Some((pd, i)) = self.w.parents_get(d.first) {
            if let Ok(Some(x)) = self.read_dir(pd).map(|v| v.into_iter().find(|x| x.index == i && x.first == d.first)) {
                return Ok(x);
            }
        }
        // Breadth-first from the root, bounded.
        let mut queue = vec![self.root()];
        let mut seen = 0usize;
        while let Some(q) = queue.pop() {
            for x in self.read_dir(q)? {
                if x.is_dir() {
                    seen += 1;
                    if seen > 100_000 {
                        return Err(Error::Corrupt("too many directories"));
                    }
                    if x.first == d.first {
                        return Ok(x);
                    }
                    queue.push(x.as_dir()?);
                }
            }
        }
        Err(Error::NotFound)
    }

    /// The entry at `(dir, index)` as it is now.
    pub fn entry_at(&mut self, dir: ExDir, index: u32) -> Result<ExEntry> {
        self.read_dir(dir)?.into_iter().find(|e| e.index == index).ok_or(Error::NotFound)
    }

    /// Update `e`'s stream and times on disk from its fields.
    fn rewrite(&mut self, e: &ExEntry, mtime: Option<i64>) -> Result<()> {
        let (mut set, offs) = self.read_set(e.dir, e.index)?;
        set[4..6].copy_from_slice(&e.attr.to_le_bytes());
        if let Some(t) = mtime {
            let (v, ms, utc) = stamp(t);
            set[12..16].copy_from_slice(&v.to_le_bytes());
            set[21] = ms;
            set[23] = utc;
        }
        let flags = ALLOC_POSSIBLE | if e.contiguous { NO_FAT_CHAIN } else { 0 };
        set[33] = (set[33] & !(ALLOC_POSSIBLE | NO_FAT_CHAIN)) | flags;
        set[40..48].copy_from_slice(&e.valid.to_le_bytes());
        set[52..56].copy_from_slice(&e.first.to_le_bytes());
        set[56..64].copy_from_slice(&e.len.to_le_bytes());
        self.write_set(&mut set, &offs)
    }

    fn delete_set(&mut self, dir: ExDir, index: u32) -> Result<()> {
        let (set, offs) = self.read_set(dir, index)?;
        for (k, o) in offs.iter().enumerate() {
            self.dev.write_at(*o, &[set[k * 32] & 0x7F])?;
        }
        Ok(())
    }

    /// The allocation `e` has, in bytes (whole clusters).
    fn alloc_of(&self, e: &ExEntry) -> u64 {
        let cs = u64::from(self.g.cluster_size);
        if e.first == 0 { 0 } else { e.len.div_ceil(cs) * cs }
    }

    /// `e`'s clusters, as a chain (a contiguous file gets one).
    fn chained(&mut self, e: &mut ExEntry) -> Result<Vec<u32>> {
        let alloc = self.alloc_of(e);
        let cl = self.chain_of(e.first, alloc, e.contiguous)?;
        if e.contiguous && !cl.is_empty() {
            self.chain_run(&cl)?;
            e.contiguous = false;
            self.w.chain = Some((e.first, cl.clone()));
        }
        Ok(cl)
    }

    // ---- operations ----

    pub fn create(&mut self, dir: ExDir, name: &str, is_dir: bool, now: i64) -> Result<ExEntry> {
        creatable(name)?;
        match self.lookup(dir, name) {
            Ok(_) => return Err(Error::Exists),
            Err(Error::NotFound) => {}
            Err(e) => return Err(e),
        }
        self.mark_dirty()?;
        let (first, len) = if is_dir {
            let mut c = Vec::new();
            self.grow_to(&mut c, 1, true)?;
            (c[0], u64::from(self.g.cluster_size))
        } else {
            (0, 0)
        };
        let attr = if is_dir { ATTR_DIRECTORY } else { ATTR_ARCHIVE };
        let mut set = self.new_set(name, attr, first, len, len, ALLOC_POSSIBLE, now);
        let (index, offs, d) = match self.place(dir, set.len() / 32) {
            Ok(p) => p,
            Err(e) => {
                if first != 0 {
                    self.free_run(&[first], true)?;
                }
                return Err(e);
            }
        };
        self.write_set(&mut set, &offs)?;
        self.entry_at(d, index)
    }

    /// Write `data` at `off`, growing the file; a gap past the valid data
    /// is zeroed on disk first.
    pub fn write(&mut self, e: &mut ExEntry, off: u64, data: &[u8], now: i64) -> Result<usize> {
        if e.is_dir() {
            return Err(Error::IsDir);
        }
        if data.is_empty() {
            return Ok(0);
        }
        let end = off.checked_add(data.len() as u64).filter(|&x| x <= MAX_FILE).ok_or(Error::TooBig)?;
        self.mark_dirty()?;
        let cs = u64::from(self.g.cluster_size);
        let mut chain = self.chained(e)?;
        let need = end.max(e.len).div_ceil(cs) as usize;
        if chain.len() < need {
            self.grow_to(&mut chain, need, false)?;
            e.first = chain[0];
            self.w.chain = Some((e.first, chain.clone()));
        }
        if off > e.valid {
            let zeros = vec![0u8; 64 * 1024];
            let mut p = e.valid;
            while p < off {
                let n = ((off - p) as usize).min(zeros.len());
                self.put(&chain, p, &zeros[..n])?;
                p += n as u64;
            }
        }
        self.put(&chain, off, data)?;
        e.len = e.len.max(end);
        e.valid = e.valid.max(end);
        e.attr |= ATTR_ARCHIVE;
        e.mtime = now;
        self.rewrite(e, Some(now))?;
        Ok(data.len())
    }

    /// Set the file's length; growing reads as zeros (past ValidDataLength).
    pub fn truncate(&mut self, e: &mut ExEntry, len: u64, now: i64) -> Result<()> {
        if e.is_dir() {
            return Err(Error::IsDir);
        }
        if len > MAX_FILE {
            return Err(Error::TooBig);
        }
        self.mark_dirty()?;
        let cs = u64::from(self.g.cluster_size);
        let need = len.div_ceil(cs) as usize;
        let mut chain = self.chained(e)?;
        if need > chain.len() {
            self.grow_to(&mut chain, need, false)?;
            e.first = chain[0];
        } else if need < chain.len() {
            let gone = chain.split_off(need);
            self.free_run(&gone, true)?;
            match chain.last() {
                Some(&l) => self.fat.set(&mut self.dev, &self.g, l, Next::End)?,
                None => e.first = 0,
            }
        }
        self.w.chain = (e.first != 0).then(|| (e.first, chain));
        e.len = len;
        e.valid = e.valid.min(len);
        e.mtime = now;
        self.rewrite(e, Some(now))
    }

    pub fn set_mtime(&mut self, e: &mut ExEntry, t: i64) -> Result<()> {
        self.mark_dirty()?;
        e.mtime = t;
        self.rewrite(e, Some(t))
    }

    pub fn unlink(&mut self, e: &ExEntry) -> Result<()> {
        if e.is_dir() {
            return Err(Error::IsDir);
        }
        self.mark_dirty()?;
        let alloc = self.alloc_of(e);
        let cl = self.chain_of(e.first, alloc, e.contiguous)?;
        self.delete_set(e.dir, e.index)?;
        self.free_run(&cl, !e.contiguous)
    }

    pub fn rmdir(&mut self, e: &ExEntry) -> Result<()> {
        let d = e.as_dir()?;
        if !self.read_dir(d)?.is_empty() {
            return Err(Error::NotEmpty);
        }
        self.mark_dirty()?;
        let cl = self.dir_clusters(d)?;
        self.delete_set(e.dir, e.index)?;
        self.w.parents_forget(e.first);
        self.free_run(&cl, !d.contiguous)
    }

    /// Move `e` to `name` in `to`, replacing a file there (or an empty
    /// directory, for a directory).
    pub fn rename(&mut self, e: &ExEntry, to: ExDir, name: &str, now: i64) -> Result<ExEntry> {
        creatable(name)?;
        if e.is_dir() && to.first != e.dir.first {
            // Not into itself or below it.
            let me = e.as_dir()?;
            if to.first == me.first || self.below(me, to.first)? {
                return Err(Error::InvalidName);
            }
        }
        match self.lookup(to, name) {
            Ok(t) if t.dir == e.dir && t.index == e.index => {}
            Ok(t) => match (e.is_dir(), t.is_dir()) {
                (false, false) => self.unlink(&t)?,
                (true, true) => self.rmdir(&t)?,
                (true, false) => return Err(Error::NotDir),
                (false, true) => return Err(Error::IsDir),
            },
            Err(Error::NotFound) => {}
            Err(x) => return Err(x),
        }
        self.mark_dirty()?;
        let (old, _) = self.read_set(e.dir, e.index)?;
        let flags = old[33] & (ALLOC_POSSIBLE | NO_FAT_CHAIN);
        let mut set = self.new_set(name, e.attr, e.first, e.len, e.valid, flags, now);
        // Keep the times.
        set[8..20].copy_from_slice(&old[8..20]);
        set[20..25].copy_from_slice(&old[20..25]);
        // Placing may grow `to`; with the old set still there, the old
        // index stays valid.
        let (index, offs, d) = self.place(to, set.len() / 32)?;
        self.write_set(&mut set, &offs)?;
        self.delete_set(e.dir, e.index)?;
        self.entry_at(d, index)
    }

    /// Is directory `first` somewhere below `d`? (Bounded.)
    fn below(&mut self, d: ExDir, first: u32) -> Result<bool> {
        let mut queue = vec![d];
        let mut seen = 0usize;
        while let Some(q) = queue.pop() {
            for x in self.read_dir(q)? {
                if x.is_dir() {
                    if x.first == first {
                        return Ok(true);
                    }
                    seen += 1;
                    if seen > 100_000 {
                        return Err(Error::Corrupt("too many directories"));
                    }
                    queue.push(x.as_dir()?);
                }
            }
        }
        Ok(false)
    }
}

/// Where each directory's entry lives, learnt as directories are read
/// (exFAT keeps no parent links).
impl State {
    fn parents_get(&self, first: u32) -> Option<(ExDir, u32)> {
        self.parents.get(&first).copied()
    }
    pub(crate) fn parents_put(&mut self, first: u32, e: &ExEntry) {
        if self.parents.len() < 1 << 20 {
            self.parents.insert(first, (e.dir, e.index));
        }
    }
    fn parents_forget(&mut self, first: u32) {
        self.parents.remove(&first);
    }
}
