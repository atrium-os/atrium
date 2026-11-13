//! A mounted volume: directories, lookup, file reads.

use crate::bpb::{FatType, Geometry};
use crate::dir::{Assembler, Entry, Slot};
use crate::fat::{Next, Table};
use crate::{BlockDev, Error, Result};

/// The FAT limit on directory size, in 32-byte slots.
pub const MAX_DIR_SLOTS: u32 = 65536;

/// A directory: the fixed FAT12/16 root, or a cluster chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DirRef {
    FixedRoot,
    Chain(u32),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FsStat {
    pub cluster_size: u32,
    pub clusters: u32,
    pub free: u32,
    pub label: Option<String>,
    pub fat_type: FatType,
}

pub struct Volume<D> {
    pub(crate) dev: D,
    pub(crate) g: Geometry,
    pub(crate) fat: Table,
    free: Option<u32>,
}

impl<D: BlockDev> Volume<D> {
    pub fn open(mut dev: D) -> Result<Self> {
        let mut bs = vec![0u8; 512];
        dev.read_at(0, &mut bs)?;
        let g = Geometry::parse(&bs, dev.len())?;
        Ok(Self { dev, g, fat: Table::default(), free: None })
    }

    pub fn geometry(&self) -> &Geometry {
        &self.g
    }

    pub fn into_inner(self) -> D {
        self.dev
    }

    pub fn root(&self) -> DirRef {
        match self.g.fat_type {
            FatType::Fat32 => DirRef::Chain(self.g.root_cluster),
            _ => DirRef::FixedRoot,
        }
    }

    /// The byte offset of each slot of `dir`, and the slots' bytes.
    pub(crate) fn dir_data(&mut self, dir: DirRef) -> Result<(Vec<u64>, Vec<u8>)> {
        match dir {
            DirRef::FixedRoot => {
                if self.g.fat_type == FatType::Fat32 {
                    return Err(Error::Corrupt("fixed root on FAT32"));
                }
                let n = self.g.root_entries.min(MAX_DIR_SLOTS);
                let mut b = vec![0u8; n as usize * 32];
                self.dev.read_at(self.g.root_offset, &mut b)?;
                let offs = (0..u64::from(n)).map(|i| self.g.root_offset + i * 32).collect();
                Ok((offs, b))
            }
            DirRef::Chain(c) => {
                let chain = self.fat.chain(&mut self.dev, &self.g, c)?;
                let cs = self.g.cluster_size as usize;
                let per = (cs / 32) as u64;
                if chain.len() as u64 * per > u64::from(MAX_DIR_SLOTS) {
                    return Err(Error::Corrupt("directory over 65536 entries"));
                }
                let mut b = vec![0u8; chain.len() * cs];
                let mut offs = Vec::with_capacity(chain.len() * cs / 32);
                for (i, &cl) in chain.iter().enumerate() {
                    let o = self.g.cluster_offset(cl).ok_or(Error::Corrupt("cluster"))?;
                    self.dev.read_at(o, &mut b[i * cs..(i + 1) * cs])?;
                    offs.extend((0..per).map(|k| o + k * 32));
                }
                Ok((offs, b))
            }
        }
    }

    pub fn read_dir(&mut self, dir: DirRef) -> Result<Vec<Entry>> {
        let (_, data) = self.dir_data(dir)?;
        let mut a = Assembler::default();
        let mut out = Vec::new();
        for (i, e) in data.chunks_exact(32).enumerate() {
            match a.feed(dir, i as u32, e) {
                Slot::End => break,
                Slot::Entry(mut x) => {
                    if self.g.fat_type != FatType::Fat32 {
                        x.cluster &= 0xFFFF;
                    }
                    out.push(x);
                }
                Slot::Free | Slot::Other => {}
            }
        }
        Ok(out)
    }

    /// `name` in `dir`, case-insensitively, by long or 8.3 name.
    pub fn lookup(&mut self, dir: DirRef, name: &str) -> Result<Entry> {
        let want = name.to_lowercase();
        self.read_dir(dir)?
            .into_iter()
            .find(|e| e.name.to_lowercase() == want || crate::dir::short_name(&e.short, 0).to_lowercase() == want)
            .ok_or(Error::NotFound)
    }

    /// A `/`-separated path from the root.
    pub fn resolve(&mut self, path: &str) -> Result<Option<Entry>> {
        let mut dir = self.root();
        let mut cur = None;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            if let Some(e) = &cur {
                dir = self.dir_of(e)?;
            }
            cur = Some(self.lookup(dir, part)?);
        }
        Ok(cur)
    }

    /// The directory an entry is.
    pub fn dir_of(&self, e: &Entry) -> Result<DirRef> {
        if !e.is_dir() {
            return Err(Error::NotDir);
        }
        if e.cluster == 0 {
            return Err(Error::Corrupt("directory with no cluster"));
        }
        if !self.g.valid_cluster(e.cluster) {
            return Err(Error::Corrupt("directory cluster out of range"));
        }
        Ok(DirRef::Chain(e.cluster))
    }

    /// Read file data at `off`; returns the bytes read (0 at end of file).
    pub fn read(&mut self, e: &Entry, off: u64, buf: &mut [u8]) -> Result<usize> {
        if e.is_dir() {
            return Err(Error::IsDir);
        }
        let size = u64::from(e.size);
        if off >= size || buf.is_empty() {
            return Ok(0);
        }
        let n = (size - off).min(buf.len() as u64) as usize;
        let cs = u64::from(self.g.cluster_size);
        let mut c = e.cluster;
        if !self.g.valid_cluster(c) {
            return Err(Error::Corrupt("file cluster out of range"));
        }
        // Skip whole clusters; a chain walk is bounded by the cluster count.
        let skip = off / cs;
        if skip > u64::from(self.g.clusters) {
            return Err(Error::Corrupt("file larger than the volume"));
        }
        for _ in 0..skip {
            c = self.follow(c)?;
        }
        let mut done = 0usize;
        let mut within = off % cs;
        while done < n {
            let o = self.g.cluster_offset(c).ok_or(Error::Corrupt("cluster"))?;
            let take = ((cs - within) as usize).min(n - done);
            self.dev.read_at(o + within, &mut buf[done..done + take])?;
            done += take;
            within = 0;
            if done < n {
                c = self.follow(c)?;
            }
        }
        Ok(n)
    }

    fn follow(&mut self, c: u32) -> Result<u32> {
        match self.fat.next(&mut self.dev, &self.g, c)? {
            Next::Cluster(n) => Ok(n),
            Next::End => Err(Error::Corrupt("file shorter than its size")),
            _ => Err(Error::Corrupt("broken cluster chain")),
        }
    }

    pub fn stat(&mut self) -> Result<FsStat> {
        let free = match self.free {
            Some(f) => f,
            None => {
                let f = self.fat.count_free(&mut self.dev, &self.g)?;
                self.free = Some(f);
                f
            }
        };
        Ok(FsStat { cluster_size: self.g.cluster_size, clusters: self.g.clusters, free, label: self.g.label.clone(), fat_type: self.g.fat_type })
    }
}
