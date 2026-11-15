//! Compound files ([MS-CFB]): the container of encrypted OOXML packages
//! (O7a) and of the legacy binary formats (O7b–O7d).
//!
//! The reader is bounded: every sector number is checked against the file,
//! every chain against loops, every size against the file's length, before
//! anything is allocated from it. Only the streams are read (storages are
//! walked for their names); nothing is written back in place.
//!
//! The writer makes a version 3 file (512-byte sectors) holding streams at
//! the top level, small ones in the mini stream, as Office does.

use std::collections::HashSet;

const SIGNATURE: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
const FREESECT: u32 = 0xFFFF_FFFF;
const ENDOFCHAIN: u32 = 0xFFFF_FFFE;
const FATSECT: u32 = 0xFFFF_FFFD;
const DIFSECT: u32 = 0xFFFF_FFFC;
const NOSTREAM: u32 = 0xFFFF_FFFF;
const MINI_CUTOFF: u64 = 4096;
/// Directory entries read at most.
const MAX_ENTRIES: usize = 1 << 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CfbError(pub String);

impl std::fmt::Display for CfbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "compound file: {}", self.0)
    }
}

fn bad<T>(why: &str) -> Result<T, CfbError> {
    Err(CfbError(why.into()))
}

/// Whether `b` starts like a compound file.
pub fn is_cfb(b: &[u8]) -> bool {
    b.len() >= 512 && b[..8] == SIGNATURE
}

#[derive(Debug, Clone)]
pub struct Entry {
    /// The path from the root, names joined with '/'.
    pub path: String,
    /// 1 storage, 2 stream, 5 root.
    pub kind: u8,
    start: u32,
    pub size: u64,
}

pub struct Cfb<'a> {
    data: &'a [u8],
    shift: u32,
    fat: Vec<u32>,
    minifat: Vec<u32>,
    /// The mini stream (the root's stream), read whole.
    mini: Vec<u8>,
    pub entries: Vec<Entry>,
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], o: usize) -> u64 {
    u64::from_le_bytes(b[o..o + 8].try_into().unwrap())
}

impl<'a> Cfb<'a> {
    pub fn open(data: &'a [u8]) -> Result<Cfb<'a>, CfbError> {
        if !is_cfb(data) {
            return bad("not a compound file");
        }
        let major = u16_at(data, 0x1A);
        let shift = u16_at(data, 0x1E) as u32;
        if u16_at(data, 0x1C) != 0xFFFE || !matches!((major, shift), (3, 9) | (4, 12)) || u16_at(data, 0x20) != 6 {
            return bad("header");
        }
        let ssz = 1usize << shift;
        let nsect = (data.len().saturating_sub(ssz) / ssz) as u64;
        let sector = |n: u32| -> Result<&'a [u8], CfbError> {
            if n as u64 >= nsect {
                return bad("sector past the end");
            }
            let o = (n as usize + 1) << shift;
            Ok(&data[o..o + ssz])
        };
        // DIFAT: 109 in the header, then a chain of DIFAT sectors.
        let nfat = u32_at(data, 0x2C) as u64;
        if nfat > nsect {
            return bad("FAT size");
        }
        let mut fat_sectors: Vec<u32> = (0..109).map(|i| u32_at(data, 0x4C + 4 * i)).filter(|&s| s != FREESECT).collect();
        let (mut d, mut nd) = (u32_at(data, 0x44), u32_at(data, 0x48));
        let mut seen = HashSet::new();
        while d != ENDOFCHAIN && d != FREESECT && nd > 0 {
            if !seen.insert(d) {
                return bad("DIFAT loop");
            }
            let s = sector(d)?;
            let per = ssz / 4 - 1;
            fat_sectors.extend((0..per).map(|i| u32_at(s, 4 * i)).filter(|&x| x != FREESECT));
            d = u32_at(s, ssz - 4);
            nd -= 1;
        }
        fat_sectors.truncate(nfat as usize);
        let mut fat = Vec::with_capacity(fat_sectors.len() * ssz / 4);
        for &f in &fat_sectors {
            let s = sector(f)?;
            fat.extend((0..ssz / 4).map(|i| u32_at(s, 4 * i)));
        }
        let mut c = Cfb { data, shift, fat, minifat: Vec::new(), mini: Vec::new(), entries: Vec::new() };
        // Directory.
        let dir = c.chain(u32_at(data, 0x30), None)?;
        let n = (dir.len() / 128).min(MAX_ENTRIES);
        struct Raw {
            name: String,
            kind: u8,
            left: u32,
            right: u32,
            child: u32,
            start: u32,
            size: u64,
        }
        let mut raw = Vec::with_capacity(n);
        for i in 0..n {
            let e = &dir[i * 128..(i + 1) * 128];
            let len = (u16_at(e, 0x40) as usize).min(64);
            let units: Vec<u16> = (0..len / 2).map(|k| u16_at(e, 2 * k)).take_while(|&u| u != 0).collect();
            let size = if major == 3 { u32_at(e, 0x78) as u64 } else { u64_at(e, 0x78) };
            raw.push(Raw { name: String::from_utf16_lossy(&units), kind: e[0x42], left: u32_at(e, 0x44), right: u32_at(e, 0x48), child: u32_at(e, 0x4C), start: u32_at(e, 0x74), size });
        }
        if raw.first().is_none_or(|r| r.kind != 5) {
            return bad("no root entry");
        }
        // Walk the tree from the root, each entry once.
        let mut visited = vec![false; raw.len()];
        let mut stack = vec![(raw[0].child, String::new())];
        visited[0] = true;
        while let Some((i, parent)) = stack.pop() {
            if i == NOSTREAM {
                continue;
            }
            let i = i as usize;
            if i >= raw.len() || visited[i] {
                return bad("directory tree");
            }
            visited[i] = true;
            let r = &raw[i];
            let path = if parent.is_empty() { r.name.clone() } else { format!("{parent}/{}", r.name) };
            stack.push((r.left, parent.clone()));
            stack.push((r.right, parent.clone()));
            if r.kind == 1 {
                stack.push((r.child, path.clone()));
            }
            if matches!(r.kind, 1 | 2) {
                c.entries.push(Entry { path, kind: r.kind, start: r.start, size: r.size });
            }
        }
        // The mini stream and its FAT.
        let root_size = raw[0].size;
        if root_size > data.len() as u64 {
            return bad("mini stream size");
        }
        c.mini = c.chain(raw[0].start, Some(root_size))?;
        let nmini = u32_at(data, 0x40) as u64;
        if nmini > nsect {
            return bad("mini FAT size");
        }
        let mf = c.chain(u32_at(data, 0x3C), Some(nmini * ssz as u64))?;
        c.minifat = (0..mf.len() / 4).map(|i| u32_at(&mf, 4 * i)).collect();
        Ok(c)
    }

    /// A regular chain from `start`, `len` bytes of it (all of it if `None`).
    fn chain(&self, start: u32, len: Option<u64>) -> Result<Vec<u8>, CfbError> {
        let ssz = 1usize << self.shift;
        let mut out = Vec::new();
        let mut s = start;
        let mut steps = 0usize;
        let want = len.map(|l| l as usize);
        while s != ENDOFCHAIN && want.is_none_or(|w| out.len() < w) {
            if s == FREESECT && want == Some(0) {
                break;
            }
            steps += 1;
            if steps > self.fat.len() + 1 {
                return bad("sector chain loops");
            }
            let o = (s as usize).checked_add(1).map(|v| v << self.shift).ok_or(CfbError("sector".into()))?;
            if s >= FATSECT.min(DIFSECT) || o + ssz > self.data.len() {
                return bad("sector chain past the end");
            }
            out.extend_from_slice(&self.data[o..o + ssz]);
            s = *self.fat.get(s as usize).ok_or(CfbError("sector not in the FAT".into()))?;
        }
        if let Some(w) = want {
            if out.len() < w {
                return bad("stream shorter than its size");
            }
            out.truncate(w);
        }
        Ok(out)
    }

    fn mini_chain(&self, start: u32, len: u64) -> Result<Vec<u8>, CfbError> {
        let mut out = Vec::with_capacity(len as usize);
        let mut s = start;
        let mut steps = 0usize;
        while (out.len() as u64) < len {
            steps += 1;
            if steps > self.minifat.len() + 1 {
                return bad("mini chain loops");
            }
            let o = s as usize * 64;
            if s == ENDOFCHAIN || o + 64 > self.mini.len() {
                return bad("mini chain past the end");
            }
            out.extend_from_slice(&self.mini[o..o + 64]);
            s = *self.minifat.get(s as usize).ok_or(CfbError("sector not in the mini FAT".into()))?;
        }
        out.truncate(len as usize);
        Ok(out)
    }

    /// The stream at `path` ('/'-separated, names compared without case).
    pub fn stream(&self, path: &str) -> Result<Vec<u8>, CfbError> {
        let e = self.entries.iter().find(|e| e.kind == 2 && e.path.eq_ignore_ascii_case(path)).ok_or_else(|| CfbError(format!("no stream {path}")))?;
        if e.size > self.data.len() as u64 {
            return bad("stream size past the file");
        }
        if e.size < MINI_CUTOFF {
            self.mini_chain(e.start, e.size)
        } else {
            self.chain(e.start, Some(e.size))
        }
    }

    pub fn has(&self, path: &str) -> bool {
        self.entries.iter().any(|e| e.kind == 2 && e.path.eq_ignore_ascii_case(path))
    }
}

/// A version 3 compound file of `streams` at the top level.
pub fn write(streams: &[(&str, &[u8])]) -> Vec<u8> {
    const SSZ: usize = 512;
    let per = SSZ / 4;
    // Order the names as the directory compares them: length, then upper case.
    let key = |n: &str| (n.encode_utf16().count(), n.to_uppercase());
    let mut order: Vec<usize> = (0..streams.len()).collect();
    order.sort_by_key(|&i| key(streams[i].0));
    // The mini stream: small streams, 64-byte mini sectors.
    let mut mini = Vec::new();
    let mut minifat: Vec<u32> = Vec::new();
    let mut starts = vec![ENDOFCHAIN; streams.len()];
    for (i, (_, d)) in streams.iter().enumerate() {
        if (d.len() as u64) < MINI_CUTOFF && !d.is_empty() {
            let first = minifat.len() as u32;
            let n = d.len().div_ceil(64);
            for k in 0..n {
                minifat.push(if k + 1 == n { ENDOFCHAIN } else { first + k as u32 + 1 });
            }
            mini.extend_from_slice(d);
            mini.resize(minifat.len() * 64, 0);
            starts[i] = first;
        }
    }
    // Regular sectors: big streams, the mini stream, the mini FAT, the
    // directory, then the FAT itself.
    let mut sectors: Vec<Vec<u8>> = Vec::new();
    let mut fat: Vec<u32> = Vec::new();
    let put = |bytes: &[u8], fat: &mut Vec<u32>, sectors: &mut Vec<Vec<u8>>| -> u32 {
        if bytes.is_empty() {
            return ENDOFCHAIN;
        }
        let first = sectors.len() as u32;
        let n = bytes.len().div_ceil(SSZ);
        for k in 0..n {
            let mut s = bytes[k * SSZ..((k + 1) * SSZ).min(bytes.len())].to_vec();
            s.resize(SSZ, 0);
            sectors.push(s);
            fat.push(if k + 1 == n { ENDOFCHAIN } else { first + k as u32 + 1 });
        }
        first
    };
    for (i, (_, d)) in streams.iter().enumerate() {
        if d.len() as u64 >= MINI_CUTOFF {
            starts[i] = put(d, &mut fat, &mut sectors);
        }
    }
    let mini_start = put(&mini, &mut fat, &mut sectors);
    let mf_bytes: Vec<u8> = minifat.iter().flat_map(|v| v.to_le_bytes()).collect();
    let minifat_start = put(&mf_bytes, &mut fat, &mut sectors);
    // Directory: the root, then the streams in a balanced tree, all black.
    let mut dir = vec![0u8; (streams.len() + 1).div_ceil(4) * 4 * 128];
    let entry = |dir: &mut Vec<u8>, at: usize, name: &str, kind: u8, left: u32, right: u32, child: u32, start: u32, size: u64| {
        let e = &mut dir[at * 128..(at + 1) * 128];
        let units: Vec<u16> = name.encode_utf16().take(31).collect();
        for (k, u) in units.iter().enumerate() {
            e[2 * k..2 * k + 2].copy_from_slice(&u.to_le_bytes());
        }
        e[0x40..0x42].copy_from_slice(&(((units.len() + 1) * 2) as u16).to_le_bytes());
        e[0x42] = kind;
        e[0x43] = 1; // black
        e[0x44..0x48].copy_from_slice(&left.to_le_bytes());
        e[0x48..0x4C].copy_from_slice(&right.to_le_bytes());
        e[0x4C..0x50].copy_from_slice(&child.to_le_bytes());
        e[0x74..0x78].copy_from_slice(&start.to_le_bytes());
        e[0x78..0x80].copy_from_slice(&size.to_le_bytes());
    };
    fn tree(order: &[usize], links: &mut Vec<(u32, u32)>) -> u32 {
        if order.is_empty() {
            return NOSTREAM;
        }
        let m = order.len() / 2;
        let (l, r) = (tree(&order[..m], links), tree(&order[m + 1..], links));
        links[order[m]] = (l, r);
        order[m] as u32 + 1
    }
    let mut links = vec![(NOSTREAM, NOSTREAM); streams.len()];
    let top = tree(&order, &mut links);
    // Unused entries: empty, no siblings.
    for k in streams.len() + 1..dir.len() / 128 {
        entry(&mut dir, k, "", 0, NOSTREAM, NOSTREAM, NOSTREAM, 0, 0);
        dir[k * 128 + 0x40] = 0;
        dir[k * 128 + 0x41] = 0;
        dir[k * 128 + 0x43] = 0;
    }
    entry(&mut dir, 0, "Root Entry", 5, NOSTREAM, NOSTREAM, top, if mini.is_empty() { ENDOFCHAIN } else { mini_start }, mini.len() as u64);
    for (i, (name, d)) in streams.iter().enumerate() {
        let start = if d.is_empty() { ENDOFCHAIN } else { starts[i] };
        entry(&mut dir, i + 1, name, 2, links[i].0, links[i].1, NOSTREAM, start, d.len() as u64);
    }
    let dir_start = put(&dir, &mut fat, &mut sectors);
    // The FAT: enough sectors to cover every sector, its own and the
    // DIFAT's included (the header lists 109 FAT sectors; DIFAT sectors list
    // the rest, `per - 1` each).
    let mut nfat = 0usize;
    let mut ndifat;
    loop {
        ndifat = nfat.saturating_sub(109).div_ceil(per - 1);
        if nfat * per >= sectors.len() + nfat + ndifat {
            break;
        }
        nfat += 1;
    }
    let fat_first = sectors.len() as u32;
    for _ in 0..nfat {
        fat.push(FATSECT);
    }
    let difat_first = fat_first + nfat as u32;
    for _ in 0..ndifat {
        fat.push(DIFSECT);
    }
    fat.resize(nfat * per, FREESECT);
    for k in 0..nfat {
        sectors.push(fat[k * per..(k + 1) * per].iter().flat_map(|v| v.to_le_bytes()).collect());
    }
    for d in 0..ndifat {
        let mut v: Vec<u32> = (0..per - 1).map(|i| 109 + d * (per - 1) + i).map(|k| if k < nfat { fat_first + k as u32 } else { FREESECT }).collect();
        v.push(if d + 1 < ndifat { difat_first + d as u32 + 1 } else { ENDOFCHAIN });
        sectors.push(v.iter().flat_map(|x| x.to_le_bytes()).collect());
    }
    let mut h = vec![0u8; SSZ];
    h[..8].copy_from_slice(&SIGNATURE);
    h[0x18..0x1A].copy_from_slice(&0x3Eu16.to_le_bytes());
    h[0x1A..0x1C].copy_from_slice(&3u16.to_le_bytes());
    h[0x1C..0x1E].copy_from_slice(&0xFFFEu16.to_le_bytes());
    h[0x1E..0x20].copy_from_slice(&9u16.to_le_bytes());
    h[0x20..0x22].copy_from_slice(&6u16.to_le_bytes());
    h[0x2C..0x30].copy_from_slice(&(nfat as u32).to_le_bytes());
    h[0x30..0x34].copy_from_slice(&dir_start.to_le_bytes());
    h[0x38..0x3C].copy_from_slice(&(MINI_CUTOFF as u32).to_le_bytes());
    h[0x3C..0x40].copy_from_slice(&(if minifat.is_empty() { ENDOFCHAIN } else { minifat_start }).to_le_bytes());
    h[0x40..0x44].copy_from_slice(&(mf_bytes.len().div_ceil(SSZ) as u32).to_le_bytes());
    h[0x44..0x48].copy_from_slice(&(if ndifat > 0 { difat_first } else { ENDOFCHAIN }).to_le_bytes());
    h[0x48..0x4C].copy_from_slice(&(ndifat as u32).to_le_bytes());
    for k in 0..109 {
        let v = if k < nfat { fat_first + k as u32 } else { FREESECT };
        h[0x4C + 4 * k..0x50 + 4 * k].copy_from_slice(&v.to_le_bytes());
    }
    let mut out = h;
    for s in sectors {
        out.extend_from_slice(&s);
    }
    out
}
