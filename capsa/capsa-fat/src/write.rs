//! Changing a volume: create, write, truncate, unlink, rmdir, rename,
//! times. Every change goes through the same checks as reading; the FAT
//! cache is written to every FAT copy on [`Volume::sync`].
//!
//! Slots of a directory are never compacted, so an entry's [`Loc`] stays
//! valid until that entry itself is removed or renamed.

use crate::bpb::FatType;
use crate::dir::{checksum, name_ok, Assembler, Entry, Loc, Slot, ATTR_ARCHIVE, ATTR_DIRECTORY};
use crate::fat::Next;
use crate::time::from_unix;
use crate::volume::{DirRef, Volume, MAX_DIR_SLOTS};
use crate::{BlockDev, Error, Result};

/// Characters a long name may not hold (besides controls).
const LFN_BAD: &[char] = &['"', '*', '/', ':', '<', '>', '?', '\\', '|'];
/// Characters an 8.3 name may not hold, besides those.
const SFN_BAD: &[u8] = b"+,;=[] .";

/// A name this volume can hold as a long name.
pub fn creatable(name: &str) -> Result<()> {
    if !name_ok(name) || name.chars().any(|c| LFN_BAD.contains(&c)) {
        return Err(Error::InvalidName);
    }
    // Windows strips these, so such a name would not round-trip.
    if name.ends_with('.') || name.ends_with(' ') {
        return Err(Error::InvalidName);
    }
    if name.encode_utf16().count() > 255 {
        return Err(Error::NameTooLong);
    }
    Ok(())
}

/// The name as a plain 8.3 name, with NT case bits, when it is one exactly.
fn exact_short(name: &str) -> Option<([u8; 11], u8)> {
    let (base, ext) = match name.rsplit_once('.') {
        Some((b, e)) => (b, Some(e)),
        None => (name, None),
    };
    let ok = |s: &str, max: usize| !s.is_empty() && s.len() <= max && s.bytes().all(|b| b.is_ascii_graphic() && !SFN_BAD.contains(&b) && !b"\"*/:<>?\\|".contains(&b));
    if !ok(base, 8) || ext.is_some_and(|e| !ok(e, 3)) {
        return None;
    }
    // One case per part, or it needs a long name.
    let case = |s: &str| -> Option<bool> {
        let lower = s.bytes().any(|b| b.is_ascii_lowercase());
        let upper = s.bytes().any(|b| b.is_ascii_uppercase());
        if lower && upper { None } else { Some(lower) }
    };
    let (bl, el) = (case(base)?, ext.map_or(Some(false), case)?);
    let mut raw = [b' '; 11];
    for (i, b) in base.bytes().enumerate() {
        raw[i] = b.to_ascii_uppercase();
    }
    for (i, b) in ext.unwrap_or("").bytes().enumerate() {
        raw[8 + i] = b.to_ascii_uppercase();
    }
    if raw[0] == 0xE5 {
        raw[0] = 0x05;
    }
    Some((raw, if bl { 0x08 } else { 0 } | if el { 0x10 } else { 0 }))
}

/// `BASE~N.EXT` for a name that needs a long name, unique among `taken`.
fn generated_short(name: &str, taken: &[[u8; 11]]) -> Result<[u8; 11]> {
    let clean = |s: &str| -> Vec<u8> {
        s.chars()
            .filter(|c| *c != ' ' && *c != '.')
            .map(|c| if c.is_ascii() && !SFN_BAD.contains(&(c as u8)) && !LFN_BAD.contains(&c) && c.is_ascii_graphic() { c.to_ascii_uppercase() as u8 } else { b'_' })
            .collect()
    };
    let (b, e) = match name.trim_start_matches('.').rsplit_once('.') {
        Some((b, e)) if !b.is_empty() => (b, e),
        _ => (name, ""),
    };
    let base = clean(b);
    let ext = clean(e);
    let base: Vec<u8> = if base.is_empty() { b"_".to_vec() } else { base };
    for n in 1..1_000_000u32 {
        let tail = format!("~{n}");
        let keep = 8usize.saturating_sub(tail.len()).min(base.len());
        let mut raw = [b' '; 11];
        raw[..keep].copy_from_slice(&base[..keep]);
        raw[keep..keep + tail.len()].copy_from_slice(tail.as_bytes());
        for (i, &c) in ext.iter().take(3).enumerate() {
            raw[8 + i] = c;
        }
        if !taken.contains(&raw) {
            return Ok(raw);
        }
    }
    Err(Error::NoSpace)
}

fn lfn_slots(name: &str, sum: u8) -> Vec<[u8; 32]> {
    let mut units: Vec<u16> = name.encode_utf16().collect();
    if units.len() % 13 != 0 {
        units.push(0);
        while units.len() % 13 != 0 {
            units.push(0xFFFF);
        }
    }
    let n = units.len() / 13;
    let offs = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
    (0..n)
        .rev()
        .map(|k| {
            let mut s = [0u8; 32];
            s[0] = (k as u8 + 1) | if k == n - 1 { 0x40 } else { 0 };
            s[11] = 0x0F;
            s[13] = sum;
            for (i, o) in offs.iter().enumerate() {
                s[*o..*o + 2].copy_from_slice(&units[k * 13 + i].to_le_bytes());
            }
            s
        })
        .collect()
}

fn sfn_slot(short: &[u8; 11], nt: u8, attr: u8, cluster: u32, size: u32, mtime: i64) -> [u8; 32] {
    let mut s = [0u8; 32];
    s[..11].copy_from_slice(short);
    s[11] = attr;
    s[12] = nt;
    let (d, t) = from_unix(mtime);
    s[14..16].copy_from_slice(&t.to_le_bytes());
    s[16..18].copy_from_slice(&d.to_le_bytes());
    s[18..20].copy_from_slice(&d.to_le_bytes());
    s[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
    s[22..24].copy_from_slice(&t.to_le_bytes());
    s[24..26].copy_from_slice(&d.to_le_bytes());
    s[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
    s[28..32].copy_from_slice(&size.to_le_bytes());
    s
}

impl<D: BlockDev> Volume<D> {
    fn dirty_free(&mut self, delta: i64) {
        if let Some(f) = self.free.as_mut() {
            *f = (i64::from(*f) + delta).clamp(0, i64::from(u32::MAX)) as u32;
        }
    }

    fn alloc_cluster(&mut self, zero: bool) -> Result<u32> {
        let c = self.fat.alloc(&mut self.dev, &self.g)?;
        self.dirty_free(-1);
        if zero {
            let o = self.g.cluster_offset(c).ok_or(Error::Corrupt("cluster"))?;
            self.dev.write_at(o, &vec![0u8; self.g.cluster_size as usize])?;
        }
        Ok(c)
    }

    fn free_chain(&mut self, first: u32) -> Result<()> {
        if first == 0 {
            return Ok(());
        }
        for c in self.fat.chain(&mut self.dev, &self.g, first)? {
            self.fat.set(&mut self.dev, &self.g, c, Next::Free)?;
            self.dirty_free(1);
        }
        Ok(())
    }

    fn write_slots(&mut self, dir: DirRef, first: u32, slots: &[[u8; 32]]) -> Result<()> {
        let (offs, _) = self.dir_data(dir)?;
        for (k, s) in slots.iter().enumerate() {
            let o = *offs.get(first as usize + k).ok_or(Error::Corrupt("slot beyond the directory"))?;
            self.dev.write_at(o, s)?;
        }
        Ok(())
    }

    fn mark_free(&mut self, loc: Loc) -> Result<()> {
        let (offs, _) = self.dir_data(loc.dir)?;
        for i in loc.lfn_start..=loc.index {
            let o = *offs.get(i as usize).ok_or(Error::Corrupt("slot beyond the directory"))?;
            self.dev.write_at(o, &[0xE5])?;
        }
        Ok(())
    }

    /// `n` consecutive free slots in `dir`, growing a chained directory.
    fn free_run(&mut self, dir: DirRef, n: u32) -> Result<u32> {
        loop {
            let (_, data) = self.dir_data(dir)?;
            let mut run = 0u32;
            for (i, e) in data.chunks_exact(32).enumerate() {
                if e[0] == 0x00 {
                    // Everything after the end marker is free too.
                    let left = (data.len() / 32 - i) as u32;
                    if left >= n - run {
                        return Ok(i as u32 - run);
                    }
                    break;
                }
                if e[0] == 0xE5 {
                    run += 1;
                    if run == n {
                        return Ok(i as u32 + 1 - n);
                    }
                } else {
                    run = 0;
                }
            }
            let DirRef::Chain(first) = dir else { return Err(Error::NoSpace) };
            let chain = self.fat.chain(&mut self.dev, &self.g, first)?;
            let per = self.g.cluster_size / 32;
            if (chain.len() as u32 + 1).saturating_mul(per) > MAX_DIR_SLOTS {
                return Err(Error::NoSpace);
            }
            let c = self.alloc_cluster(true)?;
            let last = *chain.last().ok_or(Error::Corrupt("empty chain"))?;
            self.fat.set(&mut self.dev, &self.g, last, Next::Cluster(c))?;
        }
    }

    /// The entry at `loc` as it is now.
    pub fn entry_at(&mut self, loc: Loc) -> Result<Entry> {
        let (_, data) = self.dir_data(loc.dir)?;
        let mut a = Assembler::default();
        for (i, e) in data.chunks_exact(32).enumerate().take(loc.index as usize + 1) {
            if let Slot::Entry(mut x) = a.feed(loc.dir, i as u32, e) {
                if i as u32 == loc.index {
                    if self.g.fat_type != FatType::Fat32 {
                        x.cluster &= 0xFFFF;
                    }
                    return Ok(x);
                }
            }
        }
        Err(Error::NotFound)
    }

    fn rewrite(&mut self, e: &Entry) -> Result<()> {
        let (offs, data) = self.dir_data(e.loc.dir)?;
        let i = e.loc.index as usize;
        let o = *offs.get(i).ok_or(Error::Corrupt("slot"))?;
        let old = data.get(i * 32..i * 32 + 32).ok_or(Error::Corrupt("slot"))?;
        let mut s = sfn_slot(&e.short, old[12], e.attr, e.cluster, e.size, e.mtime);
        // Keep the creation time.
        s[13..18].copy_from_slice(&old[13..18]);
        self.dev.write_at(o, &s)
    }

    /// Create a file or directory `name` in `dir`.
    pub fn create(&mut self, dir: DirRef, name: &str, is_dir: bool, now: i64) -> Result<Entry> {
        creatable(name)?;
        let existing = self.read_dir(dir)?;
        let lname = name.to_lowercase();
        if existing.iter().any(|e| e.name.to_lowercase() == lname || crate::dir::short_name(&e.short, 0).to_lowercase() == lname) {
            return Err(Error::Exists);
        }
        let taken: Vec<[u8; 11]> = existing.iter().map(|e| e.short).collect();
        let (short, nt, long) = match exact_short(name) {
            Some((s, nt)) if !taken.contains(&s) => (s, nt, false),
            _ => (generated_short(name, &taken)?, 0, true),
        };
        let mut slots = if long { lfn_slots(name, checksum(&short)) } else { Vec::new() };
        let cluster = if is_dir {
            let c = self.alloc_cluster(true)?;
            let parent = match dir {
                DirRef::Chain(p) if p != self.g.root_cluster || self.g.fat_type != FatType::Fat32 => p,
                _ => 0,
            };
            let dot = sfn_slot(b".          ", 0, ATTR_DIRECTORY, c, 0, now);
            let dotdot = sfn_slot(b"..         ", 0, ATTR_DIRECTORY, parent, 0, now);
            let o = self.g.cluster_offset(c).ok_or(Error::Corrupt("cluster"))?;
            self.dev.write_at(o, &dot)?;
            self.dev.write_at(o + 32, &dotdot)?;
            c
        } else {
            0
        };
        let attr = if is_dir { ATTR_DIRECTORY } else { ATTR_ARCHIVE };
        slots.push(sfn_slot(&short, nt, attr, cluster, 0, now));
        let at = match self.free_run(dir, slots.len() as u32) {
            Ok(a) => a,
            Err(e) => {
                if is_dir {
                    self.free_chain(cluster)?;
                }
                return Err(e);
            }
        };
        self.write_slots(dir, at, &slots)?;
        self.entry_at(Loc { dir, index: at + slots.len() as u32 - 1, lfn_start: at })
    }

    /// Make the file's chain at least `need` clusters long; returns it.
    fn grow(&mut self, e: &mut Entry, need: u64) -> Result<Vec<u32>> {
        let mut chain = if e.cluster == 0 { Vec::new() } else { self.fat.chain(&mut self.dev, &self.g, e.cluster)? };
        if need > u64::from(self.g.clusters) {
            return Err(Error::NoSpace);
        }
        while (chain.len() as u64) < need {
            let c = self.alloc_cluster(false)?;
            match chain.last() {
                Some(&l) => self.fat.set(&mut self.dev, &self.g, l, Next::Cluster(c))?,
                None => e.cluster = c,
            }
            chain.push(c);
        }
        Ok(chain)
    }

    fn put(&mut self, chain: &[u32], off: u64, data: &[u8]) -> Result<()> {
        let cs = u64::from(self.g.cluster_size);
        let mut done = 0usize;
        while done < data.len() {
            let pos = off + done as u64;
            let c = *chain.get((pos / cs) as usize).ok_or(Error::Corrupt("chain too short"))?;
            let within = pos % cs;
            let take = ((cs - within) as usize).min(data.len() - done);
            let o = self.g.cluster_offset(c).ok_or(Error::Corrupt("cluster"))?;
            self.dev.write_at(o + within, &data[done..done + take])?;
            done += take;
        }
        Ok(())
    }

    /// Write `data` at `off`, growing the file (the gap reads as zeros).
    pub fn write(&mut self, e: &mut Entry, off: u64, data: &[u8], now: i64) -> Result<usize> {
        if e.is_dir() {
            return Err(Error::IsDir);
        }
        if data.is_empty() {
            return Ok(0);
        }
        let end = off.checked_add(data.len() as u64).filter(|&x| x <= u64::from(u32::MAX)).ok_or(Error::TooBig)?;
        let cs = u64::from(self.g.cluster_size);
        let old = u64::from(e.size);
        let chain = self.grow(e, end.max(old).div_ceil(cs))?;
        if off > old {
            // FAT has no holes: zero what lies between the old end and off.
            let mut p = old;
            let zeros = vec![0u8; 64 * 1024];
            while p < off {
                let n = ((off - p) as usize).min(zeros.len());
                self.put(&chain, p, &zeros[..n])?;
                p += n as u64;
            }
        }
        self.put(&chain, off, data)?;
        e.size = end.max(old) as u32;
        e.mtime = now;
        e.attr |= ATTR_ARCHIVE;
        self.rewrite(e)?;
        Ok(data.len())
    }

    /// Set the file's length; growing reads as zeros.
    pub fn truncate(&mut self, e: &mut Entry, len: u64, now: i64) -> Result<()> {
        if e.is_dir() {
            return Err(Error::IsDir);
        }
        if len > u64::from(u32::MAX) {
            return Err(Error::TooBig);
        }
        let old = u64::from(e.size);
        if len > old {
            let mut p = old;
            let zeros = vec![0u8; 64 * 1024];
            while p < len {
                let n = ((len - p) as usize).min(zeros.len());
                self.write(e, p, &zeros[..n], now)?;
                p += n as u64;
            }
            return Ok(());
        }
        let cs = u64::from(self.g.cluster_size);
        let keep = len.div_ceil(cs) as usize;
        if e.cluster != 0 {
            let chain = self.fat.chain(&mut self.dev, &self.g, e.cluster)?;
            if keep == 0 {
                self.free_chain(e.cluster)?;
                e.cluster = 0;
            } else if keep < chain.len() {
                self.fat.set(&mut self.dev, &self.g, chain[keep - 1], Next::End)?;
                self.free_chain(chain[keep])?;
            }
        }
        e.size = len as u32;
        e.mtime = now;
        self.rewrite(e)
    }

    pub fn set_mtime(&mut self, e: &mut Entry, t: i64) -> Result<()> {
        e.mtime = t;
        self.rewrite(e)
    }

    /// Remove a file.
    pub fn unlink(&mut self, e: &Entry) -> Result<()> {
        if e.is_dir() {
            return Err(Error::IsDir);
        }
        self.mark_free(e.loc)?;
        self.free_chain(e.cluster)
    }

    /// Remove an empty directory.
    pub fn rmdir(&mut self, e: &Entry) -> Result<()> {
        let d = self.dir_of(e)?;
        if !self.read_dir(d)?.is_empty() {
            return Err(Error::NotEmpty);
        }
        self.mark_free(e.loc)?;
        self.free_chain(e.cluster)
    }

    /// The directory holding `dir` (from its `..`); `None` for the root.
    fn parent(&mut self, dir: DirRef) -> Result<Option<DirRef>> {
        if dir == self.root() {
            return Ok(None);
        }
        let (_, data) = self.dir_data(dir)?;
        let s = data.get(32..64).ok_or(Error::Corrupt("directory without .."))?;
        if &s[..2] != b".." {
            return Err(Error::Corrupt("directory without .."));
        }
        let c = (u32::from(crate::le16(s, 20)) << 16 | u32::from(crate::le16(s, 26))) & 0x0FFF_FFFF;
        Ok(Some(if c == 0 { self.root() } else { DirRef::Chain(c) }))
    }

    /// Move `e` to `name` in `to`, replacing a file (or an empty directory,
    /// for a directory) of that name.
    pub fn rename(&mut self, e: &Entry, to: DirRef, name: &str, now: i64) -> Result<Entry> {
        creatable(name)?;
        if e.is_dir() {
            // Not into itself or below it.
            let me = self.dir_of(e)?;
            let mut d = Some(to);
            for _ in 0..4096 {
                match d {
                    Some(x) if x == me => return Err(Error::InvalidName),
                    Some(x) => d = self.parent(x)?,
                    None => break,
                }
            }
        }
        match self.lookup(to, name) {
            Ok(t) if t.loc == e.loc => {
                // A change of case only: rewrite the name in place below.
            }
            Ok(t) => match (e.is_dir(), t.is_dir()) {
                (false, false) => self.unlink(&t)?,
                (true, true) => self.rmdir(&t)?,
                (true, false) => return Err(Error::NotDir),
                (false, true) => return Err(Error::IsDir),
            },
            Err(Error::NotFound) => {}
            Err(x) => return Err(x),
        }
        // Free the old slots first so a same-directory rename can reuse them.
        self.mark_free(e.loc)?;
        let made = self.create(to, name, false, now);
        let mut n = match made {
            Ok(n) => n,
            Err(err) => {
                // Put the old slots back as they were.
                return Err(self.restore(e).err().unwrap_or(err));
            }
        };
        n.attr = e.attr;
        n.cluster = e.cluster;
        n.size = e.size;
        n.mtime = e.mtime;
        self.rewrite(&n)?;
        if e.is_dir() && e.loc.dir != to {
            let me = self.dir_of(e)?;
            let parent = match to {
                DirRef::Chain(p) if !(self.g.fat_type == FatType::Fat32 && p == self.g.root_cluster) => p,
                _ => 0,
            };
            let (offs, data) = self.dir_data(me)?;
            let mut s = [0u8; 32];
            s.copy_from_slice(data.get(32..64).ok_or(Error::Corrupt("directory without .."))?);
            s[20..22].copy_from_slice(&((parent >> 16) as u16).to_le_bytes());
            s[26..28].copy_from_slice(&(parent as u16).to_le_bytes());
            self.dev.write_at(*offs.get(1).ok_or(Error::Corrupt("directory without .."))?, &s)?;
        }
        self.entry_at(n.loc)
    }

    /// Undo `mark_free` on a failed rename: rewrite the long name and 8.3
    /// slots from the entry.
    fn restore(&mut self, e: &Entry) -> Result<()> {
        let mut slots = if e.loc.lfn_start < e.loc.index { lfn_slots(&e.name, checksum(&e.short)) } else { Vec::new() };
        slots.push(sfn_slot(&e.short, 0, e.attr, e.cluster, e.size, e.mtime));
        if slots.len() as u32 != e.loc.index - e.loc.lfn_start + 1 {
            return Err(Error::Corrupt("could not restore a renamed entry"));
        }
        self.write_slots(e.loc.dir, e.loc.lfn_start, &slots)
    }

    /// Write the FAT to every copy, mark FSInfo's counts unknown, flush.
    pub fn sync(&mut self) -> Result<()> {
        self.fat.flush(&mut self.dev, &self.g)?;
        if let Some(o) = self.g.fsinfo_offset {
            let mut b = vec![0u8; 512];
            self.dev.read_at(o, &mut b)?;
            if crate::le32(&b, 0) == 0x4161_5252 && crate::le32(&b, 484) == 0x6141_7272 {
                let free = self.free.unwrap_or(u32::MAX);
                b[488..492].copy_from_slice(&free.to_le_bytes());
                b[492..496].copy_from_slice(&u32::MAX.to_le_bytes());
                self.dev.write_at(o, &b)?;
            }
        }
        self.dev.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names() {
        assert_eq!(exact_short("README.TXT").map(|x| x.1), Some(0));
        assert_eq!(exact_short("readme.txt").map(|x| x.1), Some(0x18));
        assert_eq!(exact_short("ReadMe.txt"), None);
        assert_eq!(exact_short("a b.txt"), None);
        assert_eq!(exact_short("toolongname.txt"), None);
        assert_eq!(exact_short("a.b.c"), None);
        let g = generated_short("A long file name.text", &[]).unwrap();
        assert_eq!(&g, b"ALONGF~1TEX");
        let g2 = generated_short("A long file name.text", &[g]).unwrap();
        assert_eq!(&g2, b"ALONGF~2TEX");
        assert_eq!(&generated_short(".bashrc", &[]).unwrap(), b"BASHRC~1   ");
        assert_eq!(&generated_short("文件.bin", &[]).unwrap(), b"__~1    BIN");
        for bad in ["a:b", "a*", "trail.", "trail ", "", "..", "a/b", "q?"] {
            assert!(creatable(bad).is_err(), "{bad}");
        }
        assert_eq!(creatable(&"x".repeat(256)), Err(Error::InvalidName));
    }
}
