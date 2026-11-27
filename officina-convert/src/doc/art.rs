//! A `.doc`'s OfficeArt drawing (`fcDggInfo` in the table stream): the
//! picture store, and the main document's shapes by id, which is what its
//! floating pictures need (officina.md §16.46).
//!
//! The content is the drawing group (`0xF000`, with its picture store
//! `0xF001` of `0xF007` entries), then one drawing per story kind: a
//! byte (0 the main document, 1 the headers) and its `0xF002` container.
//! A picture entry holds its picture, or names where it lies in the
//! WordDocument stream (`foDelay`).

use std::borrow::Cow;
use std::collections::HashMap;

use super::{find_blip, u16_at, u32_at};

/// The most shapes read from one drawing.
const MAX_SHAPES: usize = 1 << 14;

/// A shape the anchors name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Shape {
    /// Its picture: an index (from 1) into the picture store.
    pub pib: Option<u32>,
    /// A group's own shape.
    pub group: bool,
    /// Where it is placed, when the shape says (Word 2000 and later; the
    /// anchor's own `bx`/`by` then don't count): `posh`/`posv` (0 by
    /// offset, else aligned), `posrelh`/`posrelv` (what from).
    pub posh: Option<u32>,
    pub posrelh: Option<u32>,
    pub posv: Option<u32>,
    pub posrelv: Option<u32>,
    /// Behind the text (`fBehindDocument`, when it is set at all).
    pub behind: Option<bool>,
    /// Its outline (`MSOSPT`: 1 rectangle, 2 rounded, 3 oval, 20 line,
    /// 202 text box …).
    pub spt: u16,
    /// Fill and line colours (RGB) and whether each is on, when set; the
    /// line's width in EMUs.
    pub fill: Option<u32>,
    pub filled: Option<bool>,
    pub line: Option<u32>,
    pub lined: Option<bool>,
    pub line_w: Option<u32>,
}

/// An `OfficeArtCOLORREF` as RGB; `None` for palette and system colours.
fn colorref(v: u32) -> Option<u32> {
    (v >> 24 == 0).then(|| (v & 0xFF) << 16 | (v >> 8 & 0xFF) << 8 | (v >> 16 & 0xFF))
}

/// One picture store entry: where its picture lies.
#[derive(Debug, Clone, Copy)]
enum Entry {
    /// In the WordDocument stream at this offset, this long.
    Delay(usize, usize),
    /// Inside the drawing info itself, at this range.
    Here(usize, usize),
    None,
}

#[derive(Debug, Default)]
pub struct Art<'a> {
    dgg: &'a [u8],
    store: Vec<Entry>,
    /// The main document's shapes, by id, and the headers' and footers'.
    pub main: HashMap<u32, Shape>,
    pub hdr: HashMap<u32, Shape>,
}

/// The records in `b`: (version/instance, type, body offset, body length).
fn records(b: &[u8]) -> impl Iterator<Item = (u16, u16, usize, usize)> + '_ {
    let mut p = 0;
    std::iter::from_fn(move || {
        if p + 8 > b.len() {
            return None;
        }
        let vi = u16_at(b, p);
        let ty = u16_at(b, p + 2);
        let len = (u32_at(b, p + 4) as usize).min(b.len() - p - 8);
        let at = p + 8;
        p = at + len;
        Some((vi, ty, at, len))
    })
}

impl<'a> Art<'a> {
    pub fn read(dgg: &'a [u8]) -> Art<'a> {
        let mut art = Art { dgg, ..Art::default() };
        let mut p = 0;
        // The drawing group.
        if dgg.len() >= 8 && u16_at(dgg, 2) == 0xF000 {
            let len = (u32_at(dgg, 4) as usize).min(dgg.len() - 8);
            for (_, ty, at, len) in records(&dgg[8..8 + len]) {
                if ty == 0xF001 {
                    let base = 8 + at;
                    for (_, ty, at, len) in records(&dgg[base..base + len]).take(1 << 16) {
                        let (at, b) = (base + at, &dgg[base + at..base + at + len]);
                        art.store.push(if ty != 0xF007 || b.len() < 36 {
                            Entry::None
                        } else {
                            let (size, delay, name) = (u32_at(b, 20) as usize, u32_at(b, 28) as usize, b[33] as usize);
                            if b.len() > 36 + name {
                                Entry::Here(at + 36 + name, len - 36 - name)
                            } else if size > 0 {
                                Entry::Delay(delay, size)
                            } else {
                                Entry::None
                            }
                        });
                    }
                }
            }
            p = 8 + len;
        }
        // The drawings: a story byte, then its container.
        while p + 9 <= dgg.len() {
            let which = dgg[p];
            let b = &dgg[p + 1..];
            if u16_at(b, 2) != 0xF002 {
                break;
            }
            let len = (u32_at(b, 4) as usize).min(b.len() - 8);
            match which {
                0 if art.main.is_empty() => shapes(&b[8..8 + len], 0, &mut art.main),
                1 if art.hdr.is_empty() => shapes(&b[8..8 + len], 0, &mut art.hdr),
                _ => {}
            }
            p += 1 + 8 + len;
        }
        art
    }

    /// Picture `pib`'s file (its extension and bytes); `word` is the
    /// WordDocument stream, where most pictures lie.
    pub fn picture<'w>(&'w self, pib: u32, word: &'w [u8]) -> Result<(&'static str, Cow<'w, [u8]>), bool> {
        let entry = pib.checked_sub(1).and_then(|i| self.store.get(i as usize)).copied().unwrap_or(Entry::None);
        let b = match entry {
            Entry::Delay(at, n) => word.get(at..at.saturating_add(n).min(word.len())),
            Entry::Here(at, n) => self.dgg.get(at..at + n),
            Entry::None => None,
        };
        find_blip(b.ok_or(false)?, 0)
    }
}

/// The shapes in a drawing's records (groups and their members), by id.
fn shapes(b: &[u8], depth: usize, out: &mut HashMap<u32, Shape>) {
    if depth > 16 {
        return;
    }
    for (vi, ty, at, len) in records(b) {
        if out.len() >= MAX_SHAPES {
            return;
        }
        let body = &b[at..at + len];
        match ty {
            0xF004 => shape(body, out),
            _ if vi & 0xF == 0xF => shapes(body, depth + 1, out),
            _ => {}
        }
    }
}

/// One shape container: its id and flags (`0xF00A`) and picture (`0xF00B`
/// or `0xF122` property 0x0104, `pib`).
fn shape(b: &[u8], out: &mut HashMap<u32, Shape>) {
    let (mut id, mut s) = (None, Shape::default());
    for (vi, ty, at, len) in records(b) {
        let body = &b[at..at + len];
        match ty {
            0xF00A if body.len() >= 8 => {
                id = Some(u32_at(body, 0));
                s.spt = vi >> 4;
                // fGroup: a group's own shape (its members are drawn with it).
                s.group = u32_at(body, 4) & 1 != 0;
            }
            0xF00B | 0xF122 => {
                let n = (vi >> 4) as usize;
                for k in 0..n.min(len / 6) {
                    let pid = u16_at(body, k * 6);
                    if pid & 0x8000 != 0 {
                        continue;
                    }
                    let v = u32_at(body, k * 6 + 2);
                    match pid & 0x3FFF {
                        0x0104 => s.pib = Some(v),
                        0x038F => s.posh = Some(v),
                        0x0390 => s.posrelh = Some(v),
                        0x0391 => s.posv = Some(v),
                        0x0392 => s.posrelv = Some(v),
                        // Group shape booleans: fBehindDocument and its use bit.
                        0x03BF if v & (1 << 21) != 0 => s.behind = Some(v & (1 << 5) != 0),
                        0x0181 => s.fill = colorref(v),
                        0x01BF if v & (1 << 20) != 0 => s.filled = Some(v & (1 << 4) != 0),
                        0x01C0 => s.line = colorref(v),
                        0x01CB => s.line_w = Some(v),
                        0x01FF if v & (1 << 19) != 0 => s.lined = Some(v & (1 << 3) != 0),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(id) = id {
        out.entry(id).or_insert(s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(vi: u16, ty: u16, body: &[u8]) -> Vec<u8> {
        let mut r = Vec::new();
        r.extend(vi.to_le_bytes());
        r.extend(ty.to_le_bytes());
        r.extend((body.len() as u32).to_le_bytes());
        r.extend(body);
        r
    }

    fn sp(id: u32, flags: u32, pib: Option<u32>) -> Vec<u8> {
        let mut body = rec(0x0C02, 0xF00A, &[id.to_le_bytes(), flags.to_le_bytes()].concat());
        if let Some(p) = pib {
            body.extend(rec(0x0013, 0xF00B, &[&0x4104u16.to_le_bytes()[..], &p.to_le_bytes()].concat()));
        }
        rec(0x000F, 0xF004, &body)
    }

    #[test]
    fn pictures_by_shape_and_store() {
        // A PNG blip: header, one uid, tag byte, the file.
        let png = b"\x89PNG\r\n\x1a\nrest";
        let blip = rec(0x6E00, 0xF01E, &[&[7u8; 17][..], png].concat());
        // Entry 1 holds its picture; entry 2 names one in the WordDocument stream.
        let mut fbse = vec![0u8; 36];
        fbse[20..24].copy_from_slice(&(blip.len() as u32).to_le_bytes());
        let embedded = rec(0x0062, 0xF007, &[&fbse[..], &blip].concat());
        fbse[28..32].copy_from_slice(&5u32.to_le_bytes());
        let delayed = rec(0x0062, 0xF007, &fbse);
        let store = rec(0x002F, 0xF001, &[embedded, delayed].concat());
        let dgg = rec(0x000F, 0xF000, &store);
        // The main drawing: a picture shape, a group holding another, a plain shape.
        let group = rec(0x000F, 0xF003, &[sp(1024, 0b101, None), sp(1026, 0, Some(1))].concat());
        let shapes = [rec(0x000F, 0xF003, &[sp(1025, 0xA00, Some(1)), group, sp(1027, 0xA00, None)].concat())].concat();
        let dg = rec(0x000F, 0xF002, &shapes);
        let info = [dgg, vec![0], dg].concat();
        let art = Art::read(&info);
        assert_eq!(art.main.get(&1025), Some(&Shape { pib: Some(1), spt: 0xC0, ..Shape::default() }));
        assert!(art.main[&1024].group);
        assert_eq!(art.main[&1026].pib, Some(1));
        assert_eq!(art.main[&1027].pib, None);
        let (ext, b) = art.picture(1, &[]).unwrap();
        assert_eq!((ext, &b[..]), ("png", &png[..]));
        let word = [&[0u8; 5][..], &blip].concat();
        assert_eq!(art.picture(2, &word).unwrap().1.as_ref(), &png[..]);
        assert!(art.picture(0, &word).is_err() && art.picture(9, &word).is_err());
        // Damage anywhere: no panic.
        for cut in 0..info.len() {
            let a = Art::read(&info[..cut]);
            for k in 0..3 {
                let _ = a.picture(k, &word[..cut.min(word.len())]);
            }
        }
    }
}
