//! TrueType subsets for embedding: the glyphs a document uses (and the
//! components they are built from) are kept at their ids; every other glyph is
//! emptied. Ids stay put, so the PDF uses them as CIDs directly (Identity).

use std::collections::BTreeSet;

/// What a face file says about itself, for the font descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FaceInfo {
    pub upem: u16,
    pub ascent: i16,
    pub descent: i16,
    pub bbox: [i16; 4],
    pub glyphs: u16,
    pub italic_angle: i32,
}

pub struct Face<'a> {
    data: &'a [u8],
    tables: Vec<([u8; 4], usize, usize)>,
    long_loca: bool,
    pub info: FaceInfo,
    hmetrics: u16,
}

fn u16_at(b: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?]))
}
fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_be_bytes([*b.get(i)?, *b.get(i + 1)?, *b.get(i + 2)?, *b.get(i + 3)?]))
}

impl<'a> Face<'a> {
    /// A face from a TrueType (`glyf`) file; None for anything else.
    pub fn parse(data: &'a [u8]) -> Option<Face<'a>> {
        let n = u16_at(data, 4)? as usize;
        let mut tables = Vec::with_capacity(n);
        for k in 0..n {
            let r = 12 + 16 * k;
            let tag: [u8; 4] = data.get(r..r + 4)?.try_into().ok()?;
            let off = u32_at(data, r + 8)? as usize;
            let len = u32_at(data, r + 12)? as usize;
            data.get(off..off.checked_add(len)?)?;
            tables.push((tag, off, len));
        }
        let mut f = Face { data, tables, long_loca: false, info: FaceInfo { upem: 1000, ascent: 0, descent: 0, bbox: [0; 4], glyphs: 0, italic_angle: 0 }, hmetrics: 0 };
        let head = f.table(b"head")?;
        let hhea = f.table(b"hhea")?;
        let maxp = f.table(b"maxp")?;
        f.table(b"glyf")?;
        f.table(b"loca")?;
        f.table(b"hmtx")?;
        f.long_loca = u16_at(head, 50)? == 1;
        let s = |b: &[u8], i| u16_at(b, i).map(|v| v as i16);
        f.info.upem = u16_at(head, 18)?.max(16);
        f.info.bbox = [s(head, 36)?, s(head, 38)?, s(head, 40)?, s(head, 42)?];
        f.info.ascent = s(hhea, 4)?;
        f.info.descent = s(hhea, 6)?;
        f.hmetrics = u16_at(hhea, 34)?.max(1);
        f.info.glyphs = u16_at(maxp, 4)?;
        if let Some(post) = f.table(b"post") {
            f.info.italic_angle = u32_at(post, 4).unwrap_or(0) as i32;
        }
        Some(f)
    }

    fn table(&self, tag: &[u8; 4]) -> Option<&'a [u8]> {
        self.tables.iter().find(|t| &t.0 == tag).map(|&(_, o, l)| &self.data[o..o + l])
    }

    /// Glyph `g`'s bytes in `glyf`.
    fn glyph(&self, g: u16) -> &'a [u8] {
        let (Some(loca), Some(glyf)) = (self.table(b"loca"), self.table(b"glyf")) else { return &[] };
        let at = |i: usize| -> Option<usize> { if self.long_loca { u32_at(loca, i * 4).map(|v| v as usize) } else { u16_at(loca, i * 2).map(|v| v as usize * 2) } };
        match (at(g as usize), at(g as usize + 1)) {
            (Some(s), Some(e)) if s <= e && e <= glyf.len() => &glyf[s..e],
            _ => &[],
        }
    }

    /// Glyph `g`'s advance, font units.
    pub fn advance(&self, g: u16) -> u16 {
        let Some(hmtx) = self.table(b"hmtx") else { return 0 };
        let i = g.min(self.hmetrics - 1) as usize;
        u16_at(hmtx, i * 4).unwrap_or(0)
    }

    /// The components of a composite glyph.
    fn components(&self, g: u16) -> Vec<u16> {
        let b = self.glyph(g);
        let mut out = Vec::new();
        if b.len() < 10 || (u16_at(b, 0).unwrap_or(0) as i16) >= 0 {
            return out;
        }
        let mut i = 10;
        loop {
            let (Some(flags), Some(id)) = (u16_at(b, i), u16_at(b, i + 2)) else { break };
            out.push(id);
            i += 4 + if flags & 1 != 0 { 4 } else { 2 };
            i += if flags & 0x8 != 0 {
                2
            } else if flags & 0x40 != 0 {
                4
            } else if flags & 0x80 != 0 {
                8
            } else {
                0
            };
            if flags & 0x20 == 0 || out.len() > 1024 {
                break;
            }
        }
        out
    }

    /// The subset file keeping `used` (glyph 0 always) and what they are made of.
    pub fn subset(&self, used: &BTreeSet<u16>) -> Vec<u8> {
        let n = self.info.glyphs;
        let mut keep: BTreeSet<u16> = BTreeSet::new();
        let mut todo: Vec<u16> = used.iter().copied().chain([0]).filter(|&g| g < n).collect();
        while let Some(g) = todo.pop() {
            if keep.insert(g) {
                todo.extend(self.components(g).into_iter().filter(|&c| c < n && !keep.contains(&c)));
            }
        }
        let mut glyf = Vec::new();
        let mut loca = Vec::with_capacity(4 * (n as usize + 1));
        for g in 0..n {
            loca.extend_from_slice(&(glyf.len() as u32).to_be_bytes());
            if keep.contains(&g) {
                glyf.extend_from_slice(self.glyph(g));
                while glyf.len() % 4 != 0 {
                    glyf.push(0);
                }
            }
        }
        loca.extend_from_slice(&(glyf.len() as u32).to_be_bytes());
        let mut head = self.table(b"head").unwrap_or_default().to_vec();
        if head.len() >= 54 {
            head[8..12].copy_from_slice(&[0; 4]);
            head[50..52].copy_from_slice(&1u16.to_be_bytes());
        }
        let mut tables: Vec<(&[u8; 4], Vec<u8>)> = vec![(b"glyf", glyf), (b"head", head), (b"loca", loca)];
        for tag in [b"cvt ", b"fpgm", b"hhea", b"hmtx", b"maxp", b"prep"] {
            if let Some(t) = self.table(tag) {
                tables.push((tag, t.to_vec()));
            }
        }
        tables.sort_by_key(|t| *t.0);
        write_sfnt(&tables)
    }
}

fn checksum(b: &[u8]) -> u32 {
    let mut s = 0u32;
    for c in b.chunks(4) {
        let mut w = [0u8; 4];
        w[..c.len()].copy_from_slice(c);
        s = s.wrapping_add(u32::from_be_bytes(w));
    }
    s
}

fn write_sfnt(tables: &[(&[u8; 4], Vec<u8>)]) -> Vec<u8> {
    let n = tables.len() as u16;
    let mut p = 1u16;
    let mut e = 0u16;
    while p * 2 <= n {
        p *= 2;
        e += 1;
    }
    let mut out = Vec::new();
    out.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    for v in [n, p * 16, e, n * 16 - p * 16] {
        out.extend_from_slice(&v.to_be_bytes());
    }
    let mut off = 12 + 16 * tables.len();
    let mut head_at = None;
    for (tag, data) in tables {
        out.extend_from_slice(*tag);
        out.extend_from_slice(&checksum(data).to_be_bytes());
        out.extend_from_slice(&(off as u32).to_be_bytes());
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        if *tag == b"head" {
            head_at = Some(off);
        }
        off += (data.len() + 3) & !3;
    }
    for (_, data) in tables {
        out.extend_from_slice(data);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    }
    if let Some(h) = head_at.filter(|h| h + 12 <= out.len()) {
        let adj = 0xB1B0_AFBAu32.wrapping_sub(checksum(&out));
        out[h + 8..h + 12].copy_from_slice(&adj.to_be_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subsets_keep_ids_and_parse() {
        let set = officina_text::layout::PinnedFonts::new().unwrap();
        let bytes = set.face_bytes(0);
        let f = Face::parse(bytes).unwrap();
        let tt = ttf_parser_face(bytes);
        let a = tt.0;
        let used: BTreeSet<u16> = [a].into_iter().collect();
        let sub = f.subset(&used);
        assert!(sub.len() < bytes.len() / 4, "{} of {}", sub.len(), bytes.len());
        let g = Face::parse(&sub).unwrap();
        assert_eq!(g.info.glyphs, f.info.glyphs);
        assert_eq!(g.glyph(a), f.glyph(a));
        assert_eq!(g.advance(a), f.advance(a));
        assert!(g.glyph(a + 1).is_empty() || f.glyph(a + 1).is_empty() || a + 1 == 0);
        assert_eq!(checksum(&sub), 0xB1B0_AFBA);
    }

    /// The glyph id of 'A' read from the cmap the slow way (format 4).
    fn ttf_parser_face(b: &[u8]) -> (u16,) {
        let f = Face::parse(b).unwrap();
        let cmap = f.table(b"cmap").unwrap();
        let n = u16_at(cmap, 2).unwrap() as usize;
        for k in 0..n {
            let off = u32_at(cmap, 4 + 8 * k + 4).unwrap() as usize;
            let t = &cmap[off..];
            if u16_at(t, 0) == Some(4) {
                let seg = u16_at(t, 6).unwrap() as usize / 2;
                for s in 0..seg {
                    let end = u16_at(t, 14 + 2 * s).unwrap();
                    let start = u16_at(t, 16 + 2 * seg + 2 * s).unwrap();
                    if (start..=end).contains(&0x41) {
                        let delta = u16_at(t, 16 + 4 * seg + 2 * s).unwrap();
                        let ro = u16_at(t, 16 + 6 * seg + 2 * s).unwrap() as usize;
                        if ro == 0 {
                            return (0x41u16.wrapping_add(delta),);
                        }
                        let at = 16 + 6 * seg + 2 * s + ro + 2 * (0x41 - start as usize);
                        return (u16_at(t, at).unwrap().wrapping_add(delta),);
                    }
                }
            }
        }
        panic!("no cmap 4");
    }
}
