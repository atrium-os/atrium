//! What the server needs from a filesystem, implemented for capsa-fat's
//! FAT volumes (read-write) and exFAT volumes (read-only so far).

use std::hash::Hash;

use capsa_fat::exfat::{ExDir, ExEntry, ExVolume};
use capsa_fat::{BlockDev, DirRef, Entry, Error, Loc, Result, Volume, ATTR_READ_ONLY};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Attr {
    pub size: u64,
    pub dir: bool,
    pub read_only: bool,
    pub mtime: i64,
    pub ctime: i64,
    pub atime: i64,
}

pub trait Backend {
    type Dir: Copy + Eq + Hash + std::fmt::Debug;
    type Node: Clone;
    /// Where a node lives; stable until it is removed or renamed.
    type Key: Copy + Eq + Hash + std::fmt::Debug;

    fn root(&self) -> Self::Dir;
    fn key(n: &Self::Node) -> Self::Key;
    fn name(n: &Self::Node) -> &str;
    fn attr(n: &Self::Node) -> Attr;
    fn as_dir(&self, n: &Self::Node) -> Result<Self::Dir>;
    fn list(&mut self, d: Self::Dir) -> Result<Vec<Self::Node>>;
    fn lookup(&mut self, d: Self::Dir, name: &str) -> Result<Self::Node>;
    /// The node at `k` as it is now.
    fn refresh(&mut self, k: Self::Key) -> Result<Self::Node>;
    fn read(&mut self, n: &Self::Node, off: u64, buf: &mut [u8]) -> Result<usize>;
    /// (cluster size, clusters, free clusters).
    fn statfs(&mut self) -> Result<(u32, u64, u64)>;
    fn writable(&self) -> bool {
        false
    }
    fn create(&mut self, _d: Self::Dir, _name: &str, _dir: bool, _now: i64) -> Result<Self::Node> {
        Err(Error::Io("read-only".into()))
    }
    fn write(&mut self, _n: &mut Self::Node, _off: u64, _data: &[u8], _now: i64) -> Result<usize> {
        Err(Error::Io("read-only".into()))
    }
    fn truncate(&mut self, _n: &mut Self::Node, _len: u64, _now: i64) -> Result<()> {
        Err(Error::Io("read-only".into()))
    }
    fn set_mtime(&mut self, _n: &mut Self::Node, _t: i64) -> Result<()> {
        Err(Error::Io("read-only".into()))
    }
    fn unlink(&mut self, _n: &Self::Node) -> Result<()> {
        Err(Error::Io("read-only".into()))
    }
    fn rmdir(&mut self, _n: &Self::Node) -> Result<()> {
        Err(Error::Io("read-only".into()))
    }
    fn rename(&mut self, _n: &Self::Node, _to: Self::Dir, _name: &str, _now: i64) -> Result<Self::Node> {
        Err(Error::Io("read-only".into()))
    }
    fn sync(&mut self) -> Result<()> {
        Ok(())
    }
}

impl<D: BlockDev> Backend for Volume<D> {
    type Dir = DirRef;
    type Node = Entry;
    type Key = Loc;

    fn root(&self) -> DirRef {
        Volume::root(self)
    }
    fn key(n: &Entry) -> Loc {
        n.loc
    }
    fn name(n: &Entry) -> &str {
        &n.name
    }
    fn attr(n: &Entry) -> Attr {
        Attr { size: u64::from(n.size), dir: n.is_dir(), read_only: n.attr & ATTR_READ_ONLY != 0, mtime: n.mtime, ctime: n.ctime, atime: n.atime }
    }
    fn as_dir(&self, n: &Entry) -> Result<DirRef> {
        self.dir_of(n)
    }
    fn list(&mut self, d: DirRef) -> Result<Vec<Entry>> {
        self.read_dir(d)
    }
    fn lookup(&mut self, d: DirRef, name: &str) -> Result<Entry> {
        Volume::lookup(self, d, name)
    }
    fn refresh(&mut self, k: Loc) -> Result<Entry> {
        self.entry_at(k)
    }
    fn read(&mut self, n: &Entry, off: u64, buf: &mut [u8]) -> Result<usize> {
        Volume::read(self, n, off, buf)
    }
    fn statfs(&mut self) -> Result<(u32, u64, u64)> {
        let s = self.stat()?;
        Ok((s.cluster_size, u64::from(s.clusters), u64::from(s.free)))
    }
    fn writable(&self) -> bool {
        true
    }
    fn create(&mut self, d: DirRef, name: &str, dir: bool, now: i64) -> Result<Entry> {
        Volume::create(self, d, name, dir, now)
    }
    fn write(&mut self, n: &mut Entry, off: u64, data: &[u8], now: i64) -> Result<usize> {
        Volume::write(self, n, off, data, now)
    }
    fn truncate(&mut self, n: &mut Entry, len: u64, now: i64) -> Result<()> {
        Volume::truncate(self, n, len, now)
    }
    fn set_mtime(&mut self, n: &mut Entry, t: i64) -> Result<()> {
        Volume::set_mtime(self, n, t)
    }
    fn unlink(&mut self, n: &Entry) -> Result<()> {
        Volume::unlink(self, n)
    }
    fn rmdir(&mut self, n: &Entry) -> Result<()> {
        Volume::rmdir(self, n)
    }
    fn rename(&mut self, n: &Entry, to: DirRef, name: &str, now: i64) -> Result<Entry> {
        Volume::rename(self, n, to, name, now)
    }
    fn sync(&mut self) -> Result<()> {
        Volume::sync(self)
    }
}

impl<D: BlockDev> Backend for ExVolume<D> {
    type Dir = ExDir;
    type Node = ExEntry;
    type Key = (ExDir, u32);

    fn root(&self) -> ExDir {
        ExVolume::root(self)
    }
    fn key(n: &ExEntry) -> (ExDir, u32) {
        (n.dir, n.index)
    }
    fn name(n: &ExEntry) -> &str {
        &n.name
    }
    fn attr(n: &ExEntry) -> Attr {
        Attr { size: n.len, dir: n.is_dir(), read_only: n.attr & 1 != 0, mtime: n.mtime, ctime: n.ctime, atime: n.atime }
    }
    fn as_dir(&self, n: &ExEntry) -> Result<ExDir> {
        n.as_dir()
    }
    fn list(&mut self, d: ExDir) -> Result<Vec<ExEntry>> {
        self.read_dir(d)
    }
    fn lookup(&mut self, d: ExDir, name: &str) -> Result<ExEntry> {
        ExVolume::lookup(self, d, name)
    }
    fn refresh(&mut self, (d, i): (ExDir, u32)) -> Result<ExEntry> {
        self.read_dir(d)?.into_iter().find(|e| e.index == i).ok_or(Error::NotFound)
    }
    fn read(&mut self, n: &ExEntry, off: u64, buf: &mut [u8]) -> Result<usize> {
        ExVolume::read(self, n, off, buf)
    }
    fn statfs(&mut self) -> Result<(u32, u64, u64)> {
        let g = self.geometry().clone();
        let free = self.free_clusters().unwrap_or(0);
        Ok((g.cluster_size, u64::from(g.clusters), u64::from(free)))
    }
}
