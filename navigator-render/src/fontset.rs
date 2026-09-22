//! The pinned font set (navigator-backend §4.1a).
//!
//! ★ SHIPPED, VERSIONED, NEVER THE SYSTEM'S. The fonts are compiled into the
//! binary, put through the SAME canonicalization as a web font (static,
//! unhinted, sanitized — `navigator-fonts`), and checked against pinned
//! addresses. A mismatch refuses to render: a changed font changes every
//! golden, and that has to be a deliberate version bump, never a drift.

use navigator_fonts::{canonicalize, Declared};

pub struct Face {
    /// `blake3:<hex>` of the canonical bytes — what NSG names.
    pub address: String,
    pub name: &'static str,
    pub weight: u16,
    pub bytes: Vec<u8>,
    pub upem: i64,
    pub ascent: i64,
    pub descent: i64,
}

impl Face {
    pub fn parse(&self) -> ttf_parser::Face<'_> {
        ttf_parser::Face::parse(&self.bytes, 0).expect("pinned font parses")
    }
    pub fn has(&self, ch: char) -> bool { self.parse().glyph_index(ch).is_some() }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Family { Sans, Mono }

pub struct FontSet {
    pub faces: Vec<Face>,
}

struct Source { name: &'static str, weight: u16, bytes: &'static [u8], pinned: &'static str }

/// The set, in stack order per (family, weight): primary first, then fallback.
///
/// IBM Plex (OFL) is the face; DejaVu (Bitstream Vera licence) is the fallback
/// for what Plex lacks — ★, which this repo's own documents use constantly.
const SOURCES: &[Source] = &[
    Source { name: "IBM Plex Sans", weight: 400, bytes: include_bytes!("../../fonts/ibm-plex/IBMPlexSans.ttf"),
             pinned: "blake3:9e6c018b074313ce2961085e6d0bf9372b90f4d4ad896f76af8fce2f5c8365d1" },
    Source { name: "IBM Plex Sans", weight: 700, bytes: include_bytes!("../../fonts/ibm-plex/IBMPlexSans.ttf"),
             pinned: "blake3:a2b316ffa00d73f530ecb84e651a792da709505d2491482ffdb382f1c253de90" },
    Source { name: "IBM Plex Mono", weight: 400, bytes: include_bytes!("../../fonts/ibm-plex/IBMPlexMono-Regular.ttf"),
             pinned: "blake3:b14b0975ee2a9bb7cc6f4319c57b8bb45bc7450dea58b27b0ebc0cbd148ad646" },
    Source { name: "IBM Plex Mono", weight: 700, bytes: include_bytes!("../../fonts/ibm-plex/IBMPlexMono-Bold.ttf"),
             pinned: "blake3:d1354ce2d43437b484d123cd41155402dc4601ae1b6080e988fe0992c0d67610" },
    Source { name: "DejaVu Sans", weight: 400, bytes: include_bytes!("../../test-assets/DejaVuSans.ttf"),
             pinned: "blake3:df6274799aa858df6b0baae7edd0b308bdce4c3001346c69222801c6ff614357" },
    Source { name: "DejaVu Sans Mono", weight: 400, bytes: include_bytes!("../../test-assets/DejaVuSansMono.ttf"),
             pinned: "blake3:7b6f683f72cc75b3763172c4e05827bfd7a9b4c49ff0fe83e20f3f71c19dcbc5" },
];

impl FontSet {
    /// Canonicalize and verify every face. `Err` names each face whose
    /// canonical address is not the pinned one.
    pub fn load() -> Result<FontSet, String> {
        let mut faces = vec![];
        let mut wrong = vec![];
        for s in SOURCES {
            let c = canonicalize(s.bytes, Declared { weight: s.weight })
                .map_err(|r| format!("{} {}: {r}", s.name, s.weight))?;
            if c.address != s.pinned {
                wrong.push(format!("{} {}: canonical {} but pinned {}", s.name, s.weight, c.address, s.pinned));
            }
            let f = ttf_parser::Face::parse(&c.sfnt, 0).map_err(|e| format!("{e:?}"))?;
            let (upem, ascent, descent) = (f.units_per_em() as i64, f.ascender() as i64, f.descender() as i64);
            faces.push(Face { address: c.address, name: s.name, weight: s.weight, bytes: c.sfnt, upem, ascent, descent });
        }
        if !wrong.is_empty() { return Err(format!("font set is not the pinned one:\n  {}", wrong.join("\n  "))) }
        Ok(FontSet { faces })
    }

    /// Face indices to try, in order, for a family and weight.
    pub fn stack(&self, family: Family, bold: bool) -> Vec<usize> {
        let w = if bold { 700 } else { 400 };
        let (primary, fallback) = match family {
            Family::Sans => ("IBM Plex Sans", "DejaVu Sans"),
            Family::Mono => ("IBM Plex Mono", "DejaVu Sans Mono"),
        };
        let mut v: Vec<usize> = self.faces.iter().enumerate()
            .filter(|(_, f)| f.name == primary && f.weight == w).map(|(i, _)| i).collect();
        v.extend(self.faces.iter().enumerate().filter(|(_, f)| f.name == fallback).map(|(i, _)| i));
        v
    }
}
