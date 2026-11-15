//! CMaps: the ToUnicode kind (bfchar, bfrange) and the code ranges of an
//! embedded CMap for Type 0 fonts (ISO 32000-2 §9.7.5, §9.10.3). The
//! predefined CJK CMaps come in P4; Identity-H and Identity-V are here.

use std::collections::HashMap;

use folium_syntax::lexer::{Lexer, Token};

/// Entries read, so a hostile CMap stays small.
pub const CMAP_ENTRIES_MAX: usize = 1 << 18;

#[derive(Debug, Clone, Default)]
pub struct ToUnicode {
    map: HashMap<u32, String>,
}

impl ToUnicode {
    pub fn get(&self, code: u32) -> Option<&str> { self.map.get(&code).map(String::as_str) }
    pub fn len(&self) -> usize { self.map.len() }
    pub fn is_empty(&self) -> bool { self.map.is_empty() }

    pub fn parse(data: &[u8]) -> ToUnicode {
        let mut m = HashMap::new();
        let mut lx = Lexer::new(data, 0);
        let utf16 = |b: &[u8]| -> String {
            let u: Vec<u16> = b.chunks(2).map(|c| if c.len() == 2 { u16::from_be_bytes([c[0], c[1]]) } else { c[0] as u16 }).collect();
            String::from_utf16_lossy(&u)
        };
        let code = |b: &[u8]| b.iter().take(4).fold(0u32, |a, &x| a << 8 | x as u32);
        loop {
            let t = match lx.next() {
                Ok(Some(t)) => t,
                Ok(None) => break,
                Err(_) => {
                    lx.pos += 1;
                    continue;
                }
            };
            match t {
                Token::Keyword(b"beginbfchar") => loop {
                    let Ok(Some(Token::String(src))) = lx.next() else { break };
                    match lx.next() {
                        Ok(Some(Token::String(dst))) => {
                            m.insert(code(&src), utf16(&dst));
                        }
                        Ok(Some(Token::Name(n))) => {
                            if let Some(c) = crate::encoding::unicode_of(&String::from_utf8_lossy(n.as_bytes())) {
                                m.insert(code(&src), c.to_string());
                            }
                        }
                        _ => break,
                    }
                    if m.len() > CMAP_ENTRIES_MAX {
                        break;
                    }
                },
                Token::Keyword(b"beginbfrange") => loop {
                    let Ok(Some(Token::String(lo))) = lx.next() else { break };
                    let Ok(Some(Token::String(hi))) = lx.next() else { break };
                    let (lo, hi) = (code(&lo), code(&hi));
                    if hi < lo || hi - lo > 0xFFFF {
                        let _ = lx.next();
                        continue;
                    }
                    match lx.next() {
                        Ok(Some(Token::String(dst))) => {
                            let base = utf16(&dst);
                            for (i, c) in (lo..=hi).enumerate() {
                                if m.len() > CMAP_ENTRIES_MAX {
                                    break;
                                }
                                // The last UTF-16 unit counts up.
                                let mut s: Vec<char> = base.chars().collect();
                                if let Some(l) = s.last_mut() {
                                    *l = char::from_u32(*l as u32 + i as u32).unwrap_or('\u{FFFD}');
                                }
                                m.insert(c, s.into_iter().collect());
                            }
                        }
                        Ok(Some(Token::ArrayOpen)) => {
                            let mut c = lo;
                            loop {
                                match lx.next() {
                                    Ok(Some(Token::String(d))) => {
                                        if c <= hi {
                                            m.insert(c, utf16(&d));
                                        }
                                        c += 1;
                                    }
                                    Ok(Some(Token::ArrayClose)) | Ok(None) | Err(_) => break,
                                    _ => {}
                                }
                            }
                        }
                        _ => break,
                    }
                    if m.len() > CMAP_ENTRIES_MAX {
                        break;
                    }
                },
                _ => {}
            }
        }
        ToUnicode { map: m }
    }
}

/// How a Type 0 font's string bytes split into codes, and code to CID.
#[derive(Debug, Clone)]
pub enum Cmap {
    /// Identity-H/V: two bytes, the code is the CID.
    Identity { vertical: bool },
    /// An embedded CMap: code space ranges and cidrange/cidchar mappings.
    Embedded { ranges: Vec<(Vec<u8>, Vec<u8>)>, cids: Vec<(u32, u32, u32)>, vertical: bool },
}

impl Cmap {
    pub fn vertical(&self) -> bool {
        match self {
            Cmap::Identity { vertical } | Cmap::Embedded { vertical, .. } => *vertical,
        }
    }

    /// The next code at `s`: (code, bytes used).
    pub fn next_code(&self, s: &[u8]) -> (u32, usize) {
        match self {
            Cmap::Identity { .. } => {
                if s.len() >= 2 {
                    ((s[0] as u32) << 8 | s[1] as u32, 2)
                } else {
                    (s[0] as u32, 1)
                }
            }
            Cmap::Embedded { ranges, .. } => {
                for n in 1..=4usize.min(s.len()) {
                    for (lo, hi) in ranges {
                        if lo.len() == n && (0..n).all(|i| s[i] >= lo[i] && s[i] <= hi[i]) {
                            return (s[..n].iter().fold(0u32, |a, &x| a << 8 | x as u32), n);
                        }
                    }
                }
                // No range matches: one byte, as other readers do.
                (s[0] as u32, 1)
            }
        }
    }

    pub fn cid(&self, code: u32) -> u32 {
        match self {
            Cmap::Identity { .. } => code,
            Cmap::Embedded { cids, .. } => {
                let i = cids.partition_point(|r| r.1 < code);
                match cids.get(i) {
                    Some(&(lo, hi, cid)) if code >= lo && code <= hi => cid + (code - lo),
                    _ => 0,
                }
            }
        }
    }

    pub fn parse(data: &[u8]) -> Cmap {
        let mut ranges = Vec::new();
        let mut cids = Vec::new();
        let mut vertical = false;
        let mut lx = Lexer::new(data, 0);
        let code = |b: &[u8]| b.iter().take(4).fold(0u32, |a, &x| a << 8 | x as u32);
        let mut last_name = None;
        loop {
            let t = match lx.next() {
                Ok(Some(t)) => t,
                Ok(None) => break,
                Err(_) => {
                    lx.pos += 1;
                    continue;
                }
            };
            match t {
                Token::Name(n) => last_name = Some(n),
                Token::Int(1) if last_name.as_ref().is_some_and(|n| n.is("WMode")) => vertical = true,
                Token::Keyword(b"begincodespacerange") => loop {
                    let Ok(Some(Token::String(lo))) = lx.next() else { break };
                    let Ok(Some(Token::String(hi))) = lx.next() else { break };
                    if lo.len() == hi.len() && (1..=4).contains(&lo.len()) && ranges.len() < 256 {
                        ranges.push((lo, hi));
                    }
                },
                Token::Keyword(b"begincidrange") => loop {
                    let Ok(Some(Token::String(lo))) = lx.next() else { break };
                    let Ok(Some(Token::String(hi))) = lx.next() else { break };
                    let Ok(Some(Token::Int(c))) = lx.next() else { break };
                    if cids.len() < CMAP_ENTRIES_MAX && c >= 0 {
                        cids.push((code(&lo), code(&hi), c as u32));
                    }
                },
                Token::Keyword(b"begincidchar") => loop {
                    let Ok(Some(Token::String(lo))) = lx.next() else { break };
                    let Ok(Some(Token::Int(c))) = lx.next() else { break };
                    if cids.len() < CMAP_ENTRIES_MAX && c >= 0 {
                        cids.push((code(&lo), code(&lo), c as u32));
                    }
                },
                _ => {}
            }
        }
        cids.sort_by_key(|r| r.1);
        if ranges.is_empty() {
            ranges.push((vec![0, 0], vec![0xff, 0xff]));
        }
        Cmap::Embedded { ranges, cids, vertical }
    }
}

/// A ToUnicode CMap for two-byte codes (Identity-H CIDs): each code to the
/// text it shows. Codes with no text are left out; a text is cut at 256
/// UTF-16 units.
pub fn write_to_unicode(map: &std::collections::BTreeMap<u16, String>) -> Vec<u8> {
    use std::fmt::Write as _;
    let mut s = String::from("/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n/CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n");
    let with_text: Vec<(&u16, &String)> = map.iter().filter(|(_, t)| !t.is_empty()).collect();
    for chunk in with_text.chunks(100) {
        let _ = writeln!(s, "{} beginbfchar", chunk.len());
        for (g, t) in chunk {
            let mut hex = String::new();
            for u in t.encode_utf16().take(256) {
                let _ = write!(hex, "{u:04X}");
            }
            let _ = writeln!(s, "<{g:04X}> <{hex}>");
        }
        s.push_str("endbfchar\n");
    }
    s.push_str("endcmap\nCMapName currentdict /CMap defineresource pop\nend\nend\n");
    s.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_unicode() {
        let t = ToUnicode::parse(
            b"/CIDInit /ProcSet findresource begin 2 beginbfchar <0003> <0020> <0024> <00660069> endbfchar
              1 beginbfrange <0010> <0012> <0041> endbfrange 1 beginbfrange <20> <21> [<0078> <0079>] endbfrange end",
        );
        assert_eq!(t.get(3), Some(" "));
        assert_eq!(t.get(0x24), Some("fi"));
        assert_eq!(t.get(0x11), Some("B"));
        assert_eq!(t.get(0x21), Some("y"));
    }

    #[test]
    fn embedded_cmap() {
        let c = Cmap::parse(b"1 begincodespacerange <00> <80> <8140> <9FFC> endcodespacerange 1 begincidrange <8140> <817E> 633 endcidrange 1 begincidchar <41> 34 endcidchar");
        assert_eq!(c.next_code(&[0x41, 0x81]), (0x41, 1));
        assert_eq!(c.next_code(&[0x81, 0x41]), (0x8141, 2));
        assert_eq!(c.cid(0x8141), 634);
        assert_eq!(c.cid(0x41), 34);
    }

    #[test]
    fn written_to_unicode_reads_back() {
        let mut m = std::collections::BTreeMap::new();
        m.insert(3u16, "fi".to_string());
        m.insert(0x41, "é".to_string());
        m.insert(9, String::new());
        let t = ToUnicode::parse(&write_to_unicode(&m));
        assert_eq!(t.get(3), Some("fi"));
        assert_eq!(t.get(0x41), Some("é"));
        assert_eq!(t.get(9), None);
    }
}
