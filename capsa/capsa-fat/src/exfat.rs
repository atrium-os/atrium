//! exFAT, read side (most SD cards and sticks over 32 GB ship with it).
//!
//! Same rules as the FAT side: checked boot region (including its
//! checksum), bounded chain walks and directory sizes, entry sets used only
//! when their checksum, stream entry and name entries agree, names that
//! cannot be file names skipped. Files flagged `NoFatChain` are contiguous
//! runs checked against the cluster heap; bytes past `ValidDataLength`
//! read as zeros.

use crate::bpb::{FatType, Geometry};
use crate::dir::name_ok;
use crate::fat::{Next, Table};
use crate::{le16, le32, BlockDev, Error, Result};

/// Largest directory read, in bytes (16 MiB: 524288 entries).
pub const MAX_DIR_BYTES: u64 = 16 << 20;

/// A directory, known by its first cluster: its length and layout change
/// as it grows, its identity does not.
#[derive(Debug, Clone, Copy)]
pub struct ExDir {
    pub first: u32,
    /// Bytes; `0` for the root, whose size is its chain's. A chained
    /// directory is read to the end of its chain.
    pub len: u64,
    pub contiguous: bool,
}

impl PartialEq for ExDir {
    fn eq(&self, o: &Self) -> bool {
        self.first == o.first
    }
}
impl Eq for ExDir {}
impl std::hash::Hash for ExDir {
    fn hash<H: std::hash::Hasher>(&self, h: &mut H) {
        self.first.hash(h)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExEntry {
    pub name: String,
    /// FAT attribute bits (read-only 0x01, hidden 0x02, system 0x04,
    /// directory 0x10, archive 0x20).
    pub attr: u16,
    pub first: u32,
    pub len: u64,
    pub valid: u64,
    pub contiguous: bool,
    pub mtime: i64,
    pub ctime: i64,
    pub atime: i64,
    /// The directory holding it and the index of its File entry.
    pub dir: ExDir,
    pub index: u32,
}

impl ExEntry {
    pub fn is_dir(&self) -> bool {
        self.attr & 0x10 != 0
    }
    pub fn as_dir(&self) -> Result<ExDir> {
        if !self.is_dir() {
            return Err(Error::NotDir);
        }
        Ok(ExDir { first: self.first, len: self.len, contiguous: self.contiguous })
    }
}

pub struct ExVolume<D> {
    pub(crate) dev: D,
    pub(crate) g: Geometry,
    pub(crate) fat: Table,
    pub label: Option<String>,
    pub(crate) bitmap: Option<(u32, u64)>,
    /// The up-case table's (first cluster, bytes), for name hashes.
    pub(crate) upcase_at: Option<(u32, u64)>,
    pub(crate) w: crate::exwrite::State,
}

/// An exFAT timestamp (and its 10 ms and UTC-offset bytes) as Unix seconds.
fn ts(v: u32, utc: u8) -> i64 {
    if v == 0 {
        return 0;
    }
    let date = (v >> 16) as u16;
    let time = v as u16;
    let mut t = crate::time::to_unix(date, time);
    if utc & 0x80 != 0 {
        // 7-bit two's complement, 15-minute steps east of UTC.
        let off = i64::from(((utc & 0x7F) << 1) as i8 >> 1) * 15 * 60;
        t -= off;
    }
    t
}

fn boot_checksum(sectors: &[u8], bps: usize) -> u32 {
    let mut c = 0u32;
    for (i, &b) in sectors[..11 * bps].iter().enumerate() {
        if i == 106 || i == 107 || i == 112 {
            continue;
        }
        c = (c >> 1 | c << 31).wrapping_add(u32::from(b));
    }
    c
}

pub fn entry_set_checksum(set: &[u8]) -> u16 {
    let mut c = 0u16;
    for (i, &b) in set.iter().enumerate() {
        if i == 2 || i == 3 {
            continue;
        }
        c = (c >> 1 | c << 15).wrapping_add(u16::from(b));
    }
    c
}

impl<D: BlockDev> ExVolume<D> {
    pub fn open(mut dev: D) -> Result<Self> {
        let mut bs = vec![0u8; 512];
        dev.read_at(0, &mut bs)?;
        if bs.get(3..11) != Some(b"EXFAT   ") || bs[510] != 0x55 || bs[511] != 0xAA {
            return Err(Error::NotFat("not exFAT"));
        }
        if bs[11..64].iter().any(|&b| b != 0) {
            return Err(Error::NotFat("exFAT MustBeZero field"));
        }
        let bps_shift = bs[108];
        let spc_shift = bs[109];
        if !(9..=12).contains(&bps_shift) || u32::from(spc_shift) > 25 - u32::from(bps_shift) {
            return Err(Error::NotFat("exFAT sector or cluster size"));
        }
        let bps = 1u64 << bps_shift;
        let cluster_size = 1u64 << (bps_shift + spc_shift);
        if cluster_size > 32 << 20 {
            return Err(Error::NotFat("cluster size"));
        }
        if le16(&bs, 104) >> 8 != 1 {
            return Err(Error::NotFat("exFAT revision"));
        }
        // The boot region's checksum (sector 11 repeats it).
        let mut region = vec![0u8; 12 * bps as usize];
        dev.read_at(0, &mut region)?;
        let sum = boot_checksum(&region, bps as usize);
        let stored = &region[11 * bps as usize..];
        if stored.chunks_exact(4).any(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]) != sum) {
            return Err(Error::NotFat("exFAT boot checksum"));
        }
        let vol_sectors = u64::from_le_bytes(bs[72..80].try_into().map_err(|_| Error::NotFat("length"))?);
        let fat_off = u64::from(le32(&bs, 80));
        let fat_len = u64::from(le32(&bs, 84));
        let heap = u64::from(le32(&bs, 88));
        let count = le32(&bs, 92);
        let root = le32(&bs, 96);
        let flags = le16(&bs, 106);
        let nfats = bs[110];
        if !(1..=2).contains(&nfats) || fat_off < 24 || fat_len == 0 || count == 0 || count >= 0x0FFF_FFF0 {
            return Err(Error::NotFat("exFAT layout"));
        }
        if fat_len.checked_mul(bps).map_or(true, |b| b < (u64::from(count) + 2) * 4) {
            return Err(Error::NotFat("exFAT FAT too small"));
        }
        if heap < fat_off + fat_len * u64::from(nfats) {
            return Err(Error::NotFat("exFAT heap overlaps the FAT"));
        }
        let vol_bytes = vol_sectors.checked_mul(bps).ok_or(Error::NotFat("length"))?;
        let heap_end = heap.checked_mul(bps).and_then(|h| h.checked_add(u64::from(count) * cluster_size)).ok_or(Error::NotFat("heap"))?;
        if heap_end > vol_bytes || vol_bytes > dev.len() {
            return Err(Error::NotFat("exFAT volume larger than the device"));
        }
        let g = Geometry {
            fat_type: FatType::Fat32,
            bytes_per_sector: bps as u32,
            cluster_size: cluster_size as u32,
            clusters: count,
            fat_offset: fat_off * bps,
            fat_bytes: fat_len * bps,
            num_fats: nfats,
            active_fat: Some(if nfats == 2 { (flags & 1) as u8 } else { 0 }),
            root_offset: 0,
            root_entries: 0,
            root_cluster: root,
            data_offset: heap * bps,
            fsinfo_offset: None,
            volume_bytes: vol_bytes,
            label: None,
        };
        if !g.valid_cluster(root) {
            return Err(Error::NotFat("exFAT root cluster"));
        }
        let mut fat = Table::default();
        fat.exfat = true;
        let mut v = Self { dev, g, fat, label: None, bitmap: None, upcase_at: None, w: Default::default() };
        // Label and allocation bitmap live in the root.
        let r = v.root();
        let data = v.dir_bytes(r)?;
        for e in data.chunks_exact(32) {
            match e[0] {
                0x00 => break,
                0x83 => {
                    let n = usize::from(e[1]).min(11);
                    let units: Vec<u16> = (0..n).map(|i| le16(e, 2 + 2 * i)).collect();
                    v.label = String::from_utf16(&units).ok().filter(|s| !s.is_empty());
                }
                0x81 if v.bitmap.is_none() => v.bitmap = Some((le32(e, 20), u64::from_le_bytes(e[24..32].try_into().unwrap_or([0; 8])))),
                0x82 if v.upcase_at.is_none() => v.upcase_at = Some((le32(e, 20), u64::from_le_bytes(e[24..32].try_into().unwrap_or([0; 8])))),
                _ => {}
            }
        }
        v.load_upcase();
        Ok(v)
    }

    pub fn root(&self) -> ExDir {
        ExDir { first: self.g.root_cluster, len: 0, contiguous: false }
    }

    pub fn geometry(&self) -> &Geometry {
        &self.g
    }

    /// The clusters of `first`, for `len` bytes (or the whole chain).
    pub(crate) fn clusters(&mut self, first: u32, len: u64, contiguous: bool) -> Result<Vec<u32>> {
        let cs = u64::from(self.g.cluster_size);
        if contiguous {
            let n = len.div_ceil(cs);
            if n > u64::from(self.g.clusters) || !self.g.valid_cluster(first) || u64::from(first) + n > u64::from(self.g.clusters) + 2 {
                return Err(Error::Corrupt("contiguous run outside the heap"));
            }
            return Ok((0..n as u32).map(|k| first + k).collect());
        }
        let chain = self.fat.chain(&mut self.dev, &self.g, first)?;
        if len > 0 && (chain.len() as u64) < len.div_ceil(cs) {
            return Err(Error::Corrupt("chain shorter than the data"));
        }
        Ok(chain)
    }

    pub(crate) fn dir_bytes(&mut self, d: ExDir) -> Result<Vec<u8>> {
        let cl = self.dir_clusters(d)?;
        let cs = self.g.cluster_size as usize;
        let total = cl.len() as u64 * cs as u64;
        if total > MAX_DIR_BYTES {
            return Err(Error::Corrupt("directory too large"));
        }
        let mut b = vec![0u8; total as usize];
        for (i, &c) in cl.iter().enumerate() {
            let a = i * cs;
            if a >= b.len() {
                break;
            }
            let end = (a + cs).min(b.len());
            let o = self.g.cluster_offset(c).ok_or(Error::Corrupt("cluster"))?;
            self.dev.read_at(o, &mut b[a..end])?;
        }
        Ok(b)
    }

    /// A directory's clusters: a contiguous one's run, a chained one's
    /// whole chain (it only grows by whole clusters).
    pub(crate) fn dir_clusters(&mut self, d: ExDir) -> Result<Vec<u32>> {
        if d.contiguous {
            self.clusters(d.first, d.len, true)
        } else {
            self.clusters(d.first, 0, false)
        }
    }

    pub fn read_dir(&mut self, d: ExDir) -> Result<Vec<ExEntry>> {
        let data = self.dir_bytes(d)?;
        let n = data.len() / 32;
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < n {
            let e = &data[i * 32..i * 32 + 32];
            match e[0] {
                0x00 => break,
                0x85 => {
                    let sec = usize::from(e[1]);
                    if (2..=18).contains(&sec) && i + sec < n {
                        if let Some(x) = self.entry_set(d, i as u32, &data[i * 32..(i + sec + 1) * 32]) {
                            if x.is_dir() {
                                self.w.parents_put(x.first, &x);
                            }
                            out.push(x);
                        }
                        i += sec + 1;
                        continue;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        Ok(out)
    }

    fn entry_set(&self, dir: ExDir, index: u32, set: &[u8]) -> Option<ExEntry> {
        if le16(set, 2) != entry_set_checksum(set) {
            return None;
        }
        let stream = &set[32..64];
        if stream[0] != 0xC0 {
            return None;
        }
        let name_len = usize::from(stream[3]);
        let need = name_len.div_ceil(15);
        if name_len == 0 || set.len() / 32 < 2 + need {
            return None;
        }
        let mut units = Vec::with_capacity(name_len);
        for k in 0..need {
            let ne = &set[(2 + k) * 32..(3 + k) * 32];
            if ne[0] != 0xC1 {
                return None;
            }
            units.extend((0..15).map(|j| le16(ne, 2 + 2 * j)));
        }
        units.truncate(name_len);
        let name = String::from_utf16(&units).ok().filter(|n| name_ok(n))?;
        let len = u64::from_le_bytes(stream[24..32].try_into().ok()?);
        let valid = u64::from_le_bytes(stream[8..16].try_into().ok()?).min(len);
        let f = &set[..32];
        Some(ExEntry {
            name,
            attr: le16(f, 4),
            first: le32(stream, 20),
            len,
            valid,
            contiguous: stream[1] & 0x02 != 0,
            ctime: ts(le32(f, 8), f[22]),
            mtime: ts(le32(f, 12), f[23]),
            atime: ts(le32(f, 16), f[24]),
            dir,
            index,
        })
    }

    /// By name, as exFAT compares them: through the volume's up-case table.
    pub fn lookup(&mut self, d: ExDir, name: &str) -> Result<ExEntry> {
        let want = self.upcased(name);
        let all = self.read_dir(d)?;
        all.into_iter().find(|e| self.upcased(&e.name) == want).ok_or(Error::NotFound)
    }

    pub fn resolve(&mut self, path: &str) -> Result<Option<ExEntry>> {
        let mut d = self.root();
        let mut cur: Option<ExEntry> = None;
        for p in path.split('/').filter(|p| !p.is_empty()) {
            if let Some(e) = &cur {
                d = e.as_dir()?;
            }
            cur = Some(self.lookup(d, p)?);
        }
        Ok(cur)
    }

    pub fn read(&mut self, e: &ExEntry, off: u64, buf: &mut [u8]) -> Result<usize> {
        if e.is_dir() {
            return Err(Error::IsDir);
        }
        if off >= e.len || buf.is_empty() {
            return Ok(0);
        }
        let n = (e.len - off).min(buf.len() as u64) as usize;
        let cs = u64::from(self.g.cluster_size);
        let mut done = 0usize;
        // Walk to the first cluster needed (a chain is bounded by `chain`).
        let mut c = e.first;
        let skip = off / cs;
        if skip > u64::from(self.g.clusters) {
            return Err(Error::Corrupt("file larger than the volume"));
        }
        if e.contiguous {
            self.clusters(e.first, e.len, true)?;
            c = e.first + skip as u32;
        } else {
            if !self.g.valid_cluster(c) {
                return Err(Error::Corrupt("file cluster out of range"));
            }
            for _ in 0..skip {
                c = self.follow(c)?;
            }
        }
        let mut pos = off;
        while done < n {
            let within = pos % cs;
            let take = ((cs - within) as usize).min(n - done);
            let dst = &mut buf[done..done + take];
            if pos >= e.valid {
                dst.fill(0);
            } else {
                let o = self.g.cluster_offset(c).ok_or(Error::Corrupt("cluster"))?;
                self.dev.read_at(o + within, dst)?;
                let keep = (e.valid - pos).min(take as u64) as usize;
                dst[keep..].fill(0);
            }
            done += take;
            pos += take as u64;
            if done < n {
                c = if e.contiguous { c + 1 } else { self.follow(c)? };
            }
        }
        Ok(n)
    }

    pub(crate) fn follow(&mut self, c: u32) -> Result<u32> {
        match self.fat.next(&mut self.dev, &self.g, c)? {
            Next::Cluster(n) => Ok(n),
            Next::End => Err(Error::Corrupt("file shorter than its size")),
            _ => Err(Error::Corrupt("broken cluster chain")),
        }
    }

    /// Free clusters, from the allocation bitmap.
    pub fn free_clusters(&mut self) -> Result<u32> {
        if let Some(b) = &self.w.bits {
            return Ok(b.free);
        }
        let (first, len) = self.bitmap.ok_or(Error::Corrupt("no allocation bitmap"))?;
        let need = u64::from(self.g.clusters).div_ceil(8);
        if len < need {
            return Err(Error::Corrupt("allocation bitmap too small"));
        }
        let cl = self.clusters(first, need, false).or_else(|_| self.clusters(first, need, true))?;
        let cs = self.g.cluster_size as usize;
        let mut used = 0u64;
        let mut left = need as usize;
        let mut b = vec![0u8; cs];
        for c in cl {
            if left == 0 {
                break;
            }
            let o = self.g.cluster_offset(c).ok_or(Error::Corrupt("cluster"))?;
            let take = left.min(cs);
            self.dev.read_at(o, &mut b[..take])?;
            used += b[..take].iter().map(|x| u64::from(x.count_ones())).sum::<u64>();
            left -= take;
        }
        Ok(u32::try_from(u64::from(self.g.clusters).saturating_sub(used)).unwrap_or(0))
    }
}
