//! Directory entries: 8.3 names with their case bits, long names (VFAT
//! LFN, checksummed), attributes, times.

use crate::time::to_unix;
use crate::{le16, le32};

pub const ATTR_READ_ONLY: u8 = 0x01;
pub const ATTR_HIDDEN: u8 = 0x02;
pub const ATTR_SYSTEM: u8 = 0x04;
pub const ATTR_VOLUME_ID: u8 = 0x08;
pub const ATTR_DIRECTORY: u8 = 0x10;
pub const ATTR_ARCHIVE: u8 = 0x20;
pub const ATTR_LFN: u8 = 0x0F;

/// Where an entry lives: its directory and the index of its 8.3 slot, and
/// the first of its long-name slots (`== index` when it has none).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Loc {
    pub dir: crate::DirRef,
    pub index: u32,
    pub lfn_start: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The long name if it has a valid one, else the 8.3 name.
    pub name: String,
    pub short: [u8; 11],
    pub attr: u8,
    pub cluster: u32,
    pub size: u32,
    pub mtime: i64,
    pub ctime: i64,
    pub atime: i64,
    pub loc: Loc,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.attr & ATTR_DIRECTORY != 0
    }
}

/// CP437's upper half, which is what DOS wrote into 8.3 names.
const CP437: [char; 128] = [
    'Ç', 'ü', 'é', 'â', 'ä', 'à', 'å', 'ç', 'ê', 'ë', 'è', 'ï', 'î', 'ì', 'Ä', 'Å', 'É', 'æ', 'Æ', 'ô', 'ö', 'ò', 'û', 'ù', 'ÿ', 'Ö', 'Ü', '¢', '£', '¥', '₧', 'ƒ',
    'á', 'í', 'ó', 'ú', 'ñ', 'Ñ', 'ª', 'º', '¿', '⌐', '¬', '½', '¼', '¡', '«', '»', '░', '▒', '▓', '│', '┤', '╡', '╢', '╖', '╕', '╣', '║', '╗', '╝', '╜', '╛', '┐',
    '└', '┴', '┬', '├', '─', '┼', '╞', '╟', '╚', '╔', '╩', '╦', '╠', '═', '╬', '╧', '╨', '╤', '╥', '╙', '╘', '╒', '╓', '╫', '╪', '┘', '┌', '█', '▄', '▌', '▐', '▀',
    'α', 'ß', 'Γ', 'π', 'Σ', 'σ', 'µ', 'τ', 'Φ', 'Θ', 'Ω', 'δ', '∞', 'φ', 'ε', '∩', '≡', '±', '≥', '≤', '⌠', '⌡', '÷', '≈', '°', '∙', '·', '√', 'ⁿ', '²', '■', ' ',
];

fn oem(b: u8) -> char {
    if b >= 0x80 {
        CP437[usize::from(b - 0x80)]
    } else {
        char::from(b)
    }
}

/// A name we will hand to the OS: nonempty, no `/` or NUL or control
/// characters, not `.` or `..`, at most 255 UTF-8 bytes.
pub fn name_ok(n: &str) -> bool {
    !n.is_empty() && n != "." && n != ".." && n.len() <= 255 && !n.chars().any(|c| c == '/' || c.is_control())
}

/// The 8.3 name as text, with NT's lowercase bits (0x08 base, 0x10 ext).
pub fn short_name(raw: &[u8; 11], nt: u8) -> String {
    let mut base: Vec<u8> = raw[..8].to_vec();
    if base[0] == 0x05 {
        base[0] = 0xE5;
    }
    let base = trim(&base);
    let ext = trim(&raw[8..]);
    let conv = |s: &[u8], lower: bool| -> String {
        s.iter().map(|&b| if lower { oem(b).to_ascii_lowercase() } else { oem(b) }).collect()
    };
    let mut s = conv(base, nt & 0x08 != 0);
    if !ext.is_empty() {
        s.push('.');
        s.push_str(&conv(ext, nt & 0x10 != 0));
    }
    s
}

fn trim(b: &[u8]) -> &[u8] {
    let end = b.iter().rposition(|&c| c != b' ').map_or(0, |p| p + 1);
    &b[..end]
}

/// The volume label from a boot sector or a label entry.
pub fn label(raw: &[u8]) -> Option<String> {
    let t: String = trim(raw).iter().map(|&b| oem(b)).collect();
    (!t.is_empty() && t != "NO NAME").then_some(t)
}

pub fn checksum(short: &[u8; 11]) -> u8 {
    short.iter().fold(0u8, |s, &b| (s >> 1 | s << 7).wrapping_add(b))
}

/// The 13 UTF-16 units of one LFN slot.
pub fn lfn_units(e: &[u8]) -> [u16; 13] {
    let mut u = [0u16; 13];
    let offs = [1, 3, 5, 7, 9, 14, 16, 18, 20, 22, 24, 28, 30];
    for (i, o) in offs.iter().enumerate() {
        u[i] = le16(e, *o);
    }
    u
}

/// Folds 32-byte slots into entries. Feed slots in order with their index;
/// it keeps the pending long name and checks it against the 8.3 slot.
#[derive(Default)]
pub struct Assembler {
    /// Units by slot order (1-based), the checksum, how many slots, and
    /// where the long name started.
    parts: Vec<[u16; 13]>,
    expect: u8,
    sum: u8,
    start: u32,
    /// Slots skipped for bad names or broken long-name runs (for logs).
    pub skipped: u32,
}

pub enum Slot {
    End,
    Free,
    Entry(Entry),
    /// A long-name part, label, `.`/`..` or a skipped entry.
    Other,
}

impl Assembler {
    pub fn feed(&mut self, dir: crate::DirRef, index: u32, e: &[u8]) -> Slot {
        if e.len() < 32 {
            return Slot::End;
        }
        let first = e[0];
        if first == 0x00 {
            return Slot::End;
        }
        if first == 0xE5 {
            self.parts.clear();
            return Slot::Free;
        }
        let attr = e[11];
        if attr & 0x3F == ATTR_LFN {
            let ord = first & 0x1F;
            if first & 0x40 != 0 {
                // The last part comes first and says how many there are.
                if ord == 0 || ord > 20 {
                    self.parts.clear();
                    self.skipped += 1;
                    return Slot::Other;
                }
                self.parts = vec![[0u16; 13]; usize::from(ord)];
                self.expect = ord;
                self.sum = e[13];
                self.start = index;
            } else if self.parts.is_empty() || ord == 0 || ord != self.expect || e[13] != self.sum {
                self.parts.clear();
                self.skipped += 1;
                return Slot::Other;
            }
            self.parts[usize::from(ord) - 1] = lfn_units(e);
            self.expect = ord - 1;
            return Slot::Other;
        }
        let mut short = [0u8; 11];
        short.copy_from_slice(&e[..11]);
        let long = std::mem::take(&mut self.parts);
        let complete = !long.is_empty() && self.expect == 0 && self.sum == checksum(&short);
        if attr & ATTR_VOLUME_ID != 0 {
            return Slot::Other;
        }
        if short[0] == b'.' && (short[1..] == [b' '; 10] || (short[1] == b'.' && short[2..] == [b' '; 9])) {
            return Slot::Other;
        }
        let name = complete.then(|| decode_lfn(&long)).flatten().filter(|n| name_ok(n)).unwrap_or_else(|| short_name(&short, e[12]));
        if !name_ok(&name) {
            self.skipped += 1;
            return Slot::Other;
        }
        let hi = u32::from(le16(e, 20));
        Slot::Entry(Entry {
            name,
            short,
            attr,
            cluster: (hi << 16 | u32::from(le16(e, 26))) & 0x0FFF_FFFF,
            size: le32(e, 28),
            mtime: to_unix(le16(e, 24), le16(e, 22)),
            ctime: to_unix(le16(e, 16), le16(e, 14)),
            atime: to_unix(le16(e, 18), 0),
            loc: Loc { dir, index, lfn_start: if complete { self.start } else { index } },
        })
    }
}

fn decode_lfn(parts: &[[u16; 13]]) -> Option<String> {
    let mut units: Vec<u16> = parts.iter().flatten().copied().collect();
    if let Some(p) = units.iter().position(|&u| u == 0) {
        // After the NUL only 0xFFFF padding is allowed.
        if units[p + 1..].iter().any(|&u| u != 0xFFFF) {
            return None;
        }
        units.truncate(p);
    } else if units.iter().any(|&u| u == 0xFFFF) {
        return None;
    }
    if units.is_empty() || units.len() > 255 {
        return None;
    }
    String::from_utf16(&units).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(short_name(b"README  TXT", 0), "README.TXT");
        assert_eq!(short_name(b"README  TXT", 0x18), "readme.txt");
        assert_eq!(short_name(b"\x05BC        ", 0).chars().next(), Some('σ'));
        assert_eq!(short_name(b"NOEXT      ", 0), "NOEXT");
        for bad in ["", ".", "..", "a/b", "a\0b", "a\u{1}"] {
            assert!(!name_ok(bad), "{bad:?}");
        }
        assert_eq!(checksum(b"README  TXT"), checksum(b"README  TXT"));
    }
}
