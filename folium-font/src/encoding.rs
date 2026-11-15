//! Simple-font encodings (ISO 32000-2 §9.6.5) and glyph names.

use std::sync::Arc;

use folium_syntax::{Dict, Document, Object};

use crate::tables::{AGL, MAC_ROMAN, STANDARD, WIN_ANSI};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Base {
    Standard,
    WinAnsi,
    MacRoman,
    /// The font program's own encoding (symbolic fonts, Type 1 built-in).
    Builtin,
}

/// Code to glyph name for a simple font: the base, then `/Differences`.
#[derive(Debug, Clone)]
pub struct Encoding {
    pub base: Base,
    /// `true` when the PDF named a base or gave differences, i.e. the
    /// names override the font program's own encoding.
    pub explicit: bool,
    pub names: Vec<Option<Arc<str>>>,
}

pub fn base_table(b: Base) -> Option<&'static [Option<&'static str>; 256]> {
    match b {
        Base::Standard => Some(&STANDARD),
        Base::WinAnsi => Some(&WIN_ANSI),
        Base::MacRoman => Some(&MAC_ROMAN),
        Base::Builtin => None,
    }
}

impl Encoding {
    /// From a font's `/Encoding`; `default` is used when it names none.
    pub fn read(doc: &Document, enc: Option<&Object>, default: Base) -> Encoding {
        let enc = enc.and_then(|o| doc.resolve(o).ok());
        let base_of = |n: &folium_syntax::Name| {
            if n.is("WinAnsiEncoding") {
                Some(Base::WinAnsi)
            } else if n.is("MacRomanEncoding") {
                Some(Base::MacRoman)
            } else if n.is("StandardEncoding") {
                Some(Base::Standard)
            } else {
                None
            }
        };
        let (base, diffs, explicit) = match &enc {
            Some(Object::Name(n)) => match base_of(n) {
                Some(b) => (b, None, true),
                None => (default, None, false),
            },
            Some(Object::Dict(d)) => {
                let b = d.name("BaseEncoding").and_then(base_of);
                let diffs = d.get("Differences").and_then(|o| doc.resolve(o).ok());
                (b.unwrap_or(default), diffs, b.is_some() || d.get("Differences").is_some())
            }
            _ => (default, None, false),
        };
        let mut names: Vec<Option<Arc<str>>> = match base_table(base) {
            Some(t) => t.iter().map(|n| n.map(Arc::from)).collect(),
            None => vec![None; 256],
        };
        if let Some(Object::Array(a)) = diffs {
            let mut code: i64 = 0;
            for o in a.iter().take(4096) {
                match o {
                    Object::Int(_) | Object::Real(_) => code = o.as_int().unwrap_or(0),
                    Object::Name(n) => {
                        if (0..256).contains(&code) {
                            names[code as usize] = Some(Arc::from(String::from_utf8_lossy(n.as_bytes()).as_ref()));
                        }
                        code += 1;
                    }
                    _ => {}
                }
            }
        }
        Encoding { base, explicit, names }
    }

    pub fn name(&self, code: u8) -> Option<&str> { self.names[code as usize].as_deref() }
}

/// A glyph name's Unicode value: the AGL, then `uniXXXX`, `uXXXX[XX]`.
pub fn unicode_of(name: &str) -> Option<char> {
    let base = name.split('.').next().unwrap_or(name);
    if let Ok(i) = AGL.binary_search_by(|(n, _)| (*n).cmp(base)) {
        return char::from_u32(AGL[i].1);
    }
    let hex = |s: &str| u32::from_str_radix(s, 16).ok().and_then(char::from_u32);
    if let Some(h) = base.strip_prefix("uni") {
        if h.len() >= 4 {
            return hex(&h[..4]);
        }
    }
    if let Some(h) = base.strip_prefix('u') {
        if (4..=6).contains(&h.len()) {
            return hex(h);
        }
    }
    None
}

/// A font descriptor's `/Flags` says symbolic (bit 3) and not
/// non-symbolic (bit 6).
pub fn is_symbolic(desc: Option<&Dict>) -> bool {
    let f = desc.and_then(|d| d.int("Flags")).unwrap_or(32);
    f & 4 != 0 && f & 32 == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(unicode_of("A"), Some('A'));
        assert_eq!(unicode_of("eacute"), Some('é'));
        assert_eq!(unicode_of("uni20AC"), Some('€'));
        assert_eq!(unicode_of("u1F600"), Some('😀'));
        assert_eq!(unicode_of("fi"), Some('\u{FB01}'));
        assert_eq!(unicode_of("A.sc"), Some('A'));
        assert_eq!(WIN_ANSI[0x80], Some("Euro"));
        assert_eq!(STANDARD[0x27], Some("quoteright"));
    }
}
