//! Text strings (ISO 32000-2 §7.9.2.2): UTF-16BE or UTF-8 with a byte
//! order mark, else PDFDocEncoding.

/// PDFDocEncoding 0x18–0x1F and 0x80–0xA0 where they differ from Latin-1.
fn pdfdoc(b: u8) -> char {
    const LOW: [char; 8] = ['\u{02D8}', '\u{02C7}', '\u{02C6}', '\u{02D9}', '\u{02DD}', '\u{02DB}', '\u{02DA}', '\u{02DC}'];
    const HIGH: [char; 33] = [
        '\u{2022}', '\u{2020}', '\u{2021}', '\u{2026}', '\u{2014}', '\u{2013}', '\u{0192}', '\u{2044}', '\u{2039}', '\u{203A}',
        '\u{2212}', '\u{2030}', '\u{201E}', '\u{201C}', '\u{201D}', '\u{2018}', '\u{2019}', '\u{201A}', '\u{2122}', '\u{FB01}',
        '\u{FB02}', '\u{0141}', '\u{0152}', '\u{0160}', '\u{0178}', '\u{017D}', '\u{0131}', '\u{0142}', '\u{0153}', '\u{0161}',
        '\u{017E}', '\u{FFFD}', '\u{20AC}',
    ];
    match b {
        0x18..=0x1F => LOW[(b - 0x18) as usize],
        0x80..=0xA0 => HIGH[(b - 0x80) as usize],
        _ => b as char,
    }
}

pub fn decode(s: &[u8]) -> String {
    if s.starts_with(&[0xFE, 0xFF]) {
        let u: Vec<u16> = s[2..].chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])).collect();
        return strip_escapes(String::from_utf16_lossy(&u));
    }
    if s.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(&s[3..]).into_owned();
    }
    s.iter().map(|&b| pdfdoc(b)).collect()
}

/// UTF-16 text strings may hold language escapes, U+001B … U+001B.
fn strip_escapes(s: String) -> String {
    if !s.contains('\u{1B}') {
        return s;
    }
    let mut out = String::with_capacity(s.len());
    let mut skip = false;
    for c in s.chars() {
        if c == '\u{1B}' {
            skip = !skip;
        } else if !skip {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn kinds() {
        assert_eq!(super::decode(b"Caf\xe9 \x80"), "Café •");
        assert_eq!(super::decode(&[0xFE, 0xFF, 0, b'H', 0x20, 0xAC]), "H€");
        assert_eq!(super::decode(&[0xEF, 0xBB, 0xBF, b'o', b'k']), "ok");
    }
}
