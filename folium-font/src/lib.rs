//! Folium's fonts (docs/spec/folium.md §5): a PDF font dictionary becomes
//! a [`Font`] that splits a string into glyphs with their advances and
//! gives each glyph's outline in text space (1 unit = 1 em). A font that
//! can't be read still places its text by its widths and draws nothing,
//! so a bad font never stops a page.
//!
//! In P2: Type 1 (embedded, our parser), TrueType, CFF and OpenType
//! programs (ttf-parser), Type 0 with CIDFontType0/2 and the Identity or
//! an embedded CMap, ToUnicode, and the standard 14 drawn by Liberation
//! (D7). Type 3, Symbol/ZapfDingbats substitutes and the predefined CJK
//! CMaps are P4.

pub mod cmap;
pub mod encoding;
pub mod subset;
pub mod geom;
mod tables;
pub mod type1;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use folium_syntax::{Dict, Document, Object, Stream};
use ttf_parser::{GlyphId, PlatformId};

use cmap::{Cmap, ToUnicode};
use encoding::{is_symbolic, unicode_of, Base, Encoding};
use geom::{Matrix, Path};
use type1::Type1;

/// Distinct glyph outlines kept per font.
pub const OUTLINES_MAX: usize = 1 << 14;
/// The largest embedded font program read.
pub const PROGRAM_MAX: usize = 32 << 20;

enum Program {
    None,
    Sfnt { data: Arc<[u8]>, index: u32, upem: f64 },
    Cff { data: Arc<[u8]> },
    Type1 { font: Type1, names: Vec<Arc<str>> },
}

enum Kind {
    Simple { gids: Vec<u32>, enc: Encoding, first: u32, widths: Vec<f64>, missing: f64 },
    Cid { cmap: Cmap, cid_to_gid: Option<Arc<[u8]>>, cff_cids: Option<HashMap<u32, u32>>, dw: f64, w: Vec<(u32, u32, f64)> },
}

pub struct Font {
    /// `/BaseFont`, subset prefix removed.
    pub name: String,
    /// A standard-14 or other non-embedded font drawn by a substitute.
    pub substituted: bool,
    /// Type 3 (P4): placed by widths, drawn as nothing for now.
    pub type3: bool,
    kind: Kind,
    program: Program,
    to_unicode: Option<ToUnicode>,
    outlines: Mutex<HashMap<u32, Option<Arc<Path>>>>,
}

/// One glyph of a string.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Glyph {
    pub code: u32,
    /// Bytes of the string it used (1 for simple fonts).
    pub len: u8,
    pub gid: u32,
    /// Advance in text space units (the `/Widths` value / 1000).
    pub advance: f64,
}

static LIBERATION: [&[u8]; 12] = [
    include_bytes!("../../fonts/liberation/LiberationSans-Regular.ttf"),
    include_bytes!("../../fonts/liberation/LiberationSans-Bold.ttf"),
    include_bytes!("../../fonts/liberation/LiberationSans-Italic.ttf"),
    include_bytes!("../../fonts/liberation/LiberationSans-BoldItalic.ttf"),
    include_bytes!("../../fonts/liberation/LiberationSerif-Regular.ttf"),
    include_bytes!("../../fonts/liberation/LiberationSerif-Bold.ttf"),
    include_bytes!("../../fonts/liberation/LiberationSerif-Italic.ttf"),
    include_bytes!("../../fonts/liberation/LiberationSerif-BoldItalic.ttf"),
    include_bytes!("../../fonts/liberation/LiberationMono-Regular.ttf"),
    include_bytes!("../../fonts/liberation/LiberationMono-Bold.ttf"),
    include_bytes!("../../fonts/liberation/LiberationMono-Italic.ttf"),
    include_bytes!("../../fonts/liberation/LiberationMono-BoldItalic.ttf"),
];

/// The Liberation face for a font's name and descriptor (D7).
fn substitute(name: &str, desc: Option<&Dict>) -> Option<&'static [u8]> {
    let n = name.to_ascii_lowercase();
    if n.contains("symbol") || n.contains("dingbat") || n.contains("wingding") {
        return None;
    }
    let flags = desc.and_then(|d| d.int("Flags")).unwrap_or(0);
    let family = if n.contains("courier") || n.contains("mono") || flags & 1 != 0 {
        8
    } else if n.contains("times") || n.contains("serif") && !n.contains("sans") || n.contains("georgia") || n.contains("garamond")
        || n.contains("cambria") || n.contains("book") || flags & 2 != 0
    {
        4
    } else {
        0
    };
    let weight = desc.and_then(|d| d.get("FontWeight")).and_then(Object::as_f64).unwrap_or(400.0);
    let bold = n.contains("bold") || n.contains("black") || n.contains("heavy") || n.contains("semibold") || weight >= 600.0 || flags & (1 << 18) != 0;
    let italic = n.contains("italic") || n.contains("oblique") || flags & (1 << 6) != 0
        || desc.and_then(|d| d.get("ItalicAngle")).and_then(Object::as_f64).is_some_and(|a| a != 0.0);
    Some(LIBERATION[family + bold as usize + 2 * italic as usize])
}

fn stream_bytes(doc: &Document, o: Option<&Object>) -> Option<(Vec<u8>, Dict)> {
    let o = doc.resolve(o?).ok()?;
    let s = o.as_stream()?;
    let d = doc.decode(s).ok()?;
    (d.rest.is_empty() && d.data.len() <= PROGRAM_MAX).then_some((d.data, s.dict.clone()))
}

impl Font {
    /// Read a font dictionary. Never fails: what can't be read is drawn
    /// as nothing.
    pub fn load(doc: &Document, dict: &Dict) -> Font {
        let sub = dict.name("Subtype").map(|n| String::from_utf8_lossy(n.as_bytes()).into_owned()).unwrap_or_default();
        let base = dict.name("BaseFont").map(|n| String::from_utf8_lossy(n.as_bytes()).into_owned()).unwrap_or_default();
        let name = match base.split_once('+') {
            Some((p, rest)) if p.len() == 6 && p.bytes().all(|c| c.is_ascii_uppercase()) => rest.to_string(),
            _ => base.clone(),
        };
        let to_unicode = stream_bytes(doc, dict.get("ToUnicode")).map(|(b, _)| ToUnicode::parse(&b));
        let mut f = if sub == "Type0" {
            Self::load_type0(doc, dict, &name)
        } else {
            Self::load_simple(doc, dict, &name, sub == "Type3")
        };
        f.to_unicode = to_unicode;
        f
    }

    fn descriptor(doc: &Document, d: &Dict) -> Option<Dict> { doc.get_in(d, "FontDescriptor").ok()?.as_dict().cloned() }

    /// The embedded program, if any, and whether it's a substitute.
    fn program(doc: &Document, desc: Option<&Dict>, name: &str) -> (Program, bool) {
        if let Some(desc) = desc {
            if let Some((b, sd)) = stream_bytes(doc, desc.get("FontFile")) {
                let len1 = doc.get_in(&sd, "Length1").ok().and_then(|o| o.as_int()).unwrap_or(0).max(0) as usize;
                if let Some(t) = Type1::parse(&b, len1) {
                    // Glyph key 0 is .notdef, so an unmapped code draws nothing.
                    let names: Vec<Arc<str>> = vec![Arc::from(".notdef")];
                    return (Program::Type1 { font: t, names }, false);
                }
            }
            if let Some((b, _)) = stream_bytes(doc, desc.get("FontFile2")) {
                if let Some(p) = sfnt(b) {
                    return (p, false);
                }
            }
            if let Some((b, sd)) = stream_bytes(doc, desc.get("FontFile3")) {
                let st = sd.name("Subtype").map(|n| n.as_bytes().to_vec()).unwrap_or_default();
                if st == b"OpenType" || b.starts_with(b"OTTO") || b.starts_with(&[0, 1, 0, 0]) || b.starts_with(b"true") {
                    if let Some(p) = sfnt(b) {
                        return (p, false);
                    }
                } else if ttf_parser::cff::Table::parse(&b).is_some() {
                    return (Program::Cff { data: b.into() }, false);
                }
            }
        }
        match substitute(name, desc) {
            Some(b) => (sfnt(b.to_vec()).unwrap_or(Program::None), true),
            None => (Program::None, true),
        }
    }

    fn load_simple(doc: &Document, dict: &Dict, name: &str, type3: bool) -> Font {
        let desc = Self::descriptor(doc, dict);
        let (program, substituted) = if type3 { (Program::None, false) } else { Self::program(doc, desc.as_ref(), name) };
        let symbolic = is_symbolic(desc.as_ref());
        let default = match &program {
            Program::Type1 { .. } | Program::Cff { .. } => Base::Builtin,
            _ if symbolic && !substituted => Base::Builtin,
            _ => Base::Standard,
        };
        let enc = Encoding::read(doc, dict.get("Encoding"), default);
        let first = dict.int("FirstChar").unwrap_or(0).clamp(0, 255) as u32;
        let widths: Vec<f64> = match doc.get_in(dict, "Widths") {
            Ok(Object::Array(a)) => a.iter().take(256).map(|o| doc.resolve(o).ok().and_then(|o| o.as_f64()).unwrap_or(0.0) / 1000.0).collect(),
            _ => Vec::new(),
        };
        let missing = desc.as_ref().and_then(|d| d.get("MissingWidth")).and_then(Object::as_f64).unwrap_or(0.0) / 1000.0;
        let mut program = program;
        let gids = simple_gids(&mut program, &enc, symbolic && !substituted);
        Font {
            name: name.to_string(),
            substituted,
            type3,
            kind: Kind::Simple { gids, enc, first, widths, missing },
            program,
            to_unicode: None,
            outlines: Mutex::new(HashMap::new()),
        }
    }

    fn load_type0(doc: &Document, dict: &Dict, name: &str) -> Font {
        let cmap = match doc.get_in(dict, "Encoding") {
            Ok(Object::Name(n)) if n.is("Identity-V") => Cmap::Identity { vertical: true },
            Ok(Object::Stream(s)) => match doc.decode(&s) {
                Ok(d) => Cmap::parse(&d.data),
                Err(_) => Cmap::Identity { vertical: false },
            },
            // Identity-H, and the predefined CMaps until P4.
            _ => Cmap::Identity { vertical: false },
        };
        let cid = match doc.get_in(dict, "DescendantFonts") {
            Ok(Object::Array(a)) => a.first().and_then(|o| doc.resolve(o).ok()).and_then(|o| o.as_dict().cloned()).unwrap_or_default(),
            _ => Dict::new(),
        };
        let desc = Self::descriptor(doc, &cid);
        let (program, substituted) = Self::program(doc, desc.as_ref(), name);
        let cid_to_gid = match doc.get_in(&cid, "CIDToGIDMap") {
            Ok(Object::Stream(s)) => doc.decode(&s).ok().map(|d| Arc::from(d.data)),
            _ => None,
        };
        let cff_cids = match &program {
            Program::Cff { data } => ttf_parser::cff::Table::parse(data).and_then(|t| {
                let n = t.number_of_glyphs();
                let mut m = HashMap::new();
                for g in 0..n {
                    if let Some(c) = t.glyph_cid(GlyphId(g)) {
                        m.insert(c as u32, g as u32);
                    }
                }
                (!m.is_empty() && m.iter().any(|(c, g)| c != g)).then_some(m)
            }),
            _ => None,
        };
        let dw = cid.get("DW").and_then(Object::as_f64).unwrap_or(1000.0) / 1000.0;
        let mut w = Vec::new();
        if let Ok(Object::Array(a)) = doc.get_in(&cid, "W") {
            let mut i = 0;
            while i < a.len() && w.len() < cmap::CMAP_ENTRIES_MAX {
                let Some(c) = a[i].as_int() else { break };
                match a.get(i + 1).map(|o| doc.resolve(o)) {
                    Some(Ok(Object::Array(ws))) => {
                        for (k, v) in ws.iter().enumerate() {
                            if let Some(v) = v.as_f64() {
                                let c = (c + k as i64).max(0) as u32;
                                w.push((c, c, v / 1000.0));
                            }
                        }
                        i += 2;
                    }
                    Some(Ok(o)) => {
                        let (Some(hi), Some(v)) = (o.as_int(), a.get(i + 2).and_then(Object::as_f64)) else { break };
                        if hi >= c && c >= 0 {
                            w.push((c as u32, hi as u32, v / 1000.0));
                        }
                        i += 3;
                    }
                    _ => break,
                }
            }
        }
        w.sort_by_key(|r| r.0);
        Font {
            name: name.to_string(),
            substituted,
            type3: false,
            kind: Kind::Cid { cmap, cid_to_gid, cff_cids, dw, w },
            program,
            to_unicode: None,
            outlines: Mutex::new(HashMap::new()),
        }
    }

    pub fn vertical(&self) -> bool { matches!(&self.kind, Kind::Cid { cmap, .. } if cmap.vertical()) }

    /// Split `s` into glyphs.
    pub fn glyphs<'a>(&'a self, s: &'a [u8]) -> impl Iterator<Item = Glyph> + 'a {
        let mut i = 0;
        std::iter::from_fn(move || {
            if i >= s.len() {
                return None;
            }
            let g = match &self.kind {
                Kind::Simple { gids, first, widths, missing, .. } => {
                    let c = s[i] as u32;
                    i += 1;
                    let w = c.checked_sub(*first).and_then(|k| widths.get(k as usize)).copied();
                    let w = match w {
                        Some(w) => w,
                        None if widths.is_empty() => self.program_advance(gids[c as usize]).unwrap_or(*missing),
                        None => *missing,
                    };
                    Glyph { code: c, len: 1, gid: gids[c as usize], advance: w }
                }
                Kind::Cid { cmap, cid_to_gid, cff_cids, dw, w } => {
                    let (code, n) = cmap.next_code(&s[i..]);
                    i += n;
                    let cid = cmap.cid(code);
                    let gid = match (cid_to_gid, cff_cids) {
                        (Some(m), _) => {
                            let k = cid as usize * 2;
                            m.get(k..k + 2).map(|b| u16::from_be_bytes([b[0], b[1]]) as u32).unwrap_or(0)
                        }
                        (_, Some(m)) => m.get(&cid).copied().unwrap_or(0),
                        _ => cid,
                    };
                    let k = w.partition_point(|r| r.0 <= cid);
                    let adv = k
                        .checked_sub(1)
                        .and_then(|k| w.get(k))
                        .filter(|r| cid >= r.0 && cid <= r.1)
                        .map(|r| r.2)
                        .unwrap_or(*dw);
                    Glyph { code, len: n as u8, gid, advance: adv }
                }
            };
            Some(g)
        })
    }

    fn program_advance(&self, gid: u32) -> Option<f64> {
        match &self.program {
            Program::Sfnt { data, index, upem } => {
                let f = ttf_parser::Face::parse(data, *index).ok()?;
                Some(f.glyph_hor_advance(GlyphId(gid as u16))? as f64 / upem)
            }
            _ => None,
        }
    }

    /// The text a code stands for: ToUnicode, else the glyph name.
    pub fn unicode(&self, code: u32) -> Option<String> {
        if let Some(t) = self.to_unicode.as_ref().and_then(|t| t.get(code)) {
            return Some(t.to_string());
        }
        match &self.kind {
            Kind::Simple { enc, .. } => enc.name(code as u8).and_then(unicode_of).map(String::from).or_else(|| {
                (32..127).contains(&code).then(|| (code as u8 as char).to_string())
            }),
            Kind::Cid { .. } => None,
        }
    }

    /// The glyph's outline in text space (1 = 1 em), cached.
    pub fn outline(&self, gid: u32) -> Option<Arc<Path>> {
        if let Some(p) = self.outlines.lock().unwrap().get(&gid) {
            return p.clone();
        }
        let p = self.build_outline(gid).map(Arc::new);
        let mut c = self.outlines.lock().unwrap();
        if c.len() >= OUTLINES_MAX {
            c.clear();
        }
        c.insert(gid, p.clone());
        p
    }

    fn build_outline(&self, gid: u32) -> Option<Path> {
        let mut p = Path::new();
        match &self.program {
            Program::None => return None,
            Program::Sfnt { data, index, upem } => {
                let f = ttf_parser::Face::parse(data, *index).ok()?;
                f.outline_glyph(GlyphId(u16::try_from(gid).ok()?), &mut p)?;
                Some(p.transform(&Matrix::scale(1.0 / upem, 1.0 / upem)))
            }
            Program::Cff { data } => {
                let t = ttf_parser::cff::Table::parse(data)?;
                t.outline(GlyphId(u16::try_from(gid).ok()?), &mut p).ok()?;
                let m = t.matrix();
                let m = Matrix::new(m.sx as f64, m.ky as f64, m.kx as f64, m.sy as f64, m.tx as f64, m.ty as f64);
                Some(p.transform(&m))
            }
            Program::Type1 { font, names } => {
                let name = names.get(gid as usize)?;
                let (p, _) = font.outline(name)?;
                Some(p.transform(&font.matrix))
            }
        }
    }
}

fn sfnt(b: Vec<u8>) -> Option<Program> {
    let f = ttf_parser::Face::parse(&b, 0).ok()?;
    let upem = f.units_per_em() as f64;
    if upem <= 0.0 {
        return None;
    }
    Some(Program::Sfnt { data: b.into(), index: 0, upem })
}

/// Code to glyph for a simple font (as pdf.js and PDFium choose).
fn simple_gids(program: &mut Program, enc: &Encoding, symbolic: bool) -> Vec<u32> {
    let mut gids = vec![0u32; 256];
    match program {
        Program::None => {}
        Program::Type1 { font, names } => {
            // Glyph keys are indices into `names`.
            let mut idx: HashMap<Arc<str>, u32> = names.iter().enumerate().map(|(i, n)| (n.clone(), i as u32)).collect();
            for code in 0..256usize {
                let n = if enc.explicit { enc.names[code].clone().or_else(|| font.encoding[code].clone()) } else { font.encoding[code].clone().or_else(|| enc.names[code].clone()) };
                let Some(n) = n.filter(|n| font.has_glyph(n)) else { continue };
                let k = *idx.entry(n.clone()).or_insert_with(|| {
                    names.push(n.clone());
                    (names.len() - 1) as u32
                });
                gids[code] = k;
            }
        }
        Program::Cff { data } => {
            if let Some(t) = ttf_parser::cff::Table::parse(data) {
                for code in 0..256usize {
                    let by_name = enc.explicit.then(|| enc.names[code].as_deref().and_then(|n| t.glyph_index_by_name(n))).flatten();
                    gids[code] = by_name.or_else(|| t.glyph_index(code as u8)).map(|g| g.0 as u32).unwrap_or(0);
                }
            }
        }
        Program::Sfnt { data, index, .. } => {
            let Ok(f) = ttf_parser::Face::parse(data, *index) else { return gids };
            let subs: Vec<_> = f.tables().cmap.map(|c| c.subtables.into_iter().collect()).unwrap_or_default();
            let find = |p: PlatformId, e: u16| subs.iter().find(|s| s.platform_id == p && s.encoding_id == e);
            let win_uni = find(PlatformId::Windows, 1).or_else(|| find(PlatformId::Windows, 10)).or_else(|| find(PlatformId::Unicode, 3)).or_else(|| find(PlatformId::Unicode, 4));
            let win_sym = find(PlatformId::Windows, 0);
            let mac = find(PlatformId::Macintosh, 0);
            for code in 0..256u32 {
                let mut g = None;
                if !symbolic || enc.explicit {
                    let name = enc.name(code as u8);
                    if let (Some(s), Some(u)) = (win_uni, name.and_then(unicode_of)) {
                        g = s.glyph_index(u as u32);
                    }
                    if g.is_none() {
                        if let (Some(s), Some(n)) = (mac, name) {
                            let mc = crate::tables::MAC_ROMAN.iter().position(|m| *m == Some(n));
                            g = mc.and_then(|c| s.glyph_index(c as u32));
                        }
                    }
                    if g.is_none() {
                        g = name.and_then(|n| f.glyph_index_by_name(n));
                    }
                }
                if g.is_none() {
                    if let Some(s) = win_sym {
                        g = [0xF000 + code, code, 0xF100 + code, 0xF200 + code].into_iter().find_map(|c| s.glyph_index(c));
                    }
                }
                if g.is_none() {
                    if let Some(s) = mac {
                        g = s.glyph_index(code);
                    }
                }
                if g.is_none() && win_uni.is_some() && symbolic {
                    g = win_uni.and_then(|s| s.glyph_index(code));
                }
                gids[code as usize] = match g {
                    Some(g) => g.0 as u32,
                    // No cmap at all: codes are glyph ids.
                    None if subs.is_empty() => code,
                    None => 0,
                };
            }
        }
    }
    gids
}

/// A font's stream, for callers that key caches by object.
pub fn font_stream(o: &Object) -> Option<&Stream> { o.as_stream() }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitutes() {
        assert!(std::ptr::eq(substitute("Helvetica", None).unwrap(), LIBERATION[0]));
        assert!(std::ptr::eq(substitute("Times-BoldItalic", None).unwrap(), LIBERATION[7]));
        assert!(std::ptr::eq(substitute("Courier-Oblique", None).unwrap(), LIBERATION[10]));
        assert!(substitute("Symbol", None).is_none());
    }
}
