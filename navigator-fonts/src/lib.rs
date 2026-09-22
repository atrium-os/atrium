//! The producer's web-font step (Document Profile v1, "Web fonts", conditions 1–5).
//!
//! Whatever a page's `@font-face` fetched goes in; one of two things comes out:
//! a **canonical** font — bare sfnt, static, unhinted, only admitted tables,
//! content-addressed — or a **refusal** that says why. Nothing in between: a
//! font that is "mostly fine" is not a state the worker can be handed.
//!
//! ```text
//!   bytes ──preflight──▶ decode ──pin──▶ sanitize ──validate──▶ Canonical
//!         (bounded)      (WOFF/WOFF2   (fvar → static  (allowlist,     (independent
//!                         → sfnt)       instance)       no hinting)     re-parse)
//! ```
//!
//! ★ THIS RUNS ON HOSTILE INPUT, in the converter's jail. Three things follow:
//!
//! 1. **Every decompression is bounded BEFORE the decoder sees the file.**
//!    `allsorts` inflates WOFF2's Brotli block and WOFF's zlib tables with an
//!    unbounded `read_to_end` and does not enforce the header's declared size,
//!    so a kilobyte can claim to be small and inflate to gigabytes. `preflight`
//!    decodes the same streams through a counting limit first. The cost is a
//!    second decompression, once per font per document.
//! 2. **The output is checked by a DIFFERENT parser from the one that wrote
//!    it.** `allsorts` decodes and rebuilds; `ttf-parser` — the family the
//!    worker renders with — must accept the result, report it static, and find
//!    no hinting in it. A rebuild is only as trustworthy as its re-read.
//! 3. **Refusals are values, and panics are caught where the build allows.**
//!    Under `panic = "abort"` (the FreeBSD cross-build) a decoder panic kills
//!    the converter instead; the jail makes that a refused document, not a
//!    compromised one.

use allsorts::binary::read::ReadScope;
use allsorts::font_data::FontData;
use allsorts::subset::whole_font;
use allsorts::tables::glyf::Glyph;
use allsorts::tables::loca::LocaTable;
use allsorts::tables::{FontTableProvider, HeadTable, IndexToLocFormat, MaxpTable, OpenTypeFont, SfntVersion};
use allsorts::tag;
use std::borrow::Cow;
use std::io::Read;
use std::panic::{catch_unwind, AssertUnwindSafe};

/// Profile §3.12: bytes per web font, applied to the CANONICAL bytes (and so
/// to every intermediate decompression, none of which should exceed it).
pub const MAX_FONT_BYTES: usize = 8 * 1024 * 1024;
/// Profile §3.12: the format's own `numGlyphs` limit.
pub const MAX_GLYPHS: usize = 65_536;

/// What the `@font-face` rule declared — the only inputs pinning may use.
#[derive(Debug, Clone, Copy)]
pub struct Declared {
    /// `font-weight`, 100–900. Pins a `wght` axis (clamped to its range).
    pub weight: u16,
}

impl Default for Declared {
    fn default() -> Self { Declared { weight: 400 } }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format { Woff2, Woff, TrueType, OpenTypeCff }

#[derive(Debug, Clone, PartialEq)]
pub struct Canonical {
    /// The bare sfnt the store holds and the worker parses.
    pub sfnt: Vec<u8>,
    /// `blake3:<hex>` of `sfnt` — what `@font-face src` names after conversion.
    pub address: String,
    pub source: Format,
    /// Axis values a variable font was pinned at, in the font's axis order.
    pub pinned: Vec<(String, f32)>,
    /// Tables present in the source and absent from the result.
    pub dropped: Vec<String>,
    /// Bytes of per-glyph hinting instructions removed.
    pub instruction_bytes_stripped: usize,
    pub glyphs: u16,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    /// Not a font format this step decodes. EOT lands here, deliberately.
    UnknownFormat,
    /// A collection (`ttcf`): one `@font-face` names one face.
    Collection,
    /// Input, a decompressed stream, or the result exceeds `MAX_FONT_BYTES`.
    TooLarge { what: &'static str },
    TooManyGlyphs(usize),
    /// A compressed stream disagreed with the size its header declared.
    SizeMismatch { what: &'static str },
    /// The decoder or rebuilder rejected the font.
    Malformed(String),
    /// Variable, and pinning is not possible (no `gvar`/`CFF2`, or it failed).
    CannotPin(String),
    /// No outlines this profile renders (`glyf` or `CFF `), or no `cmap`.
    NoRenderableOutlines,
    /// The rebuilt font failed the independent re-parse — never shipped.
    FailedRevalidation(String),
    DecoderPanicked,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

/// Tables a canonical font may carry. Everything else is dropped:
/// hinting (`fpgm` `prep` `cvt ` `gasp` `hdmx` `LTSH` `VDMX` — condition 2a),
/// variation (`fvar` `gvar` `avar` `cvar` `HVAR` `VVAR` `MVAR` `STAT` — gone
/// after pinning), colour and bitmaps (not in Profile v1), signatures and
/// metadata (`DSIG` `meta`), and anything unknown.
const ADMITTED: &[&[u8; 4]] = &[
    b"cmap", b"head", b"hhea", b"hmtx", b"maxp", b"name", b"OS/2", b"post",
    b"glyf", b"loca", b"CFF ",
    b"GDEF", b"GSUB", b"GPOS", b"kern",
    b"vhea", b"vmtx", b"VORG",
];

fn t(b: &[u8; 4]) -> u32 { u32::from_be_bytes(*b) }
fn tag_name(v: u32) -> String { String::from_utf8_lossy(&v.to_be_bytes()).into_owned() }

/// Fetched bytes → a canonical font, or the reason there is none.
pub fn canonicalize(bytes: &[u8], declared: Declared) -> Result<Canonical, Refusal> {
    match catch_unwind(AssertUnwindSafe(|| canonicalize_inner(bytes, declared))) {
        Ok(r) => r,
        Err(_) => Err(Refusal::DecoderPanicked),
    }
}

fn canonicalize_inner(bytes: &[u8], declared: Declared) -> Result<Canonical, Refusal> {
    if bytes.len() > MAX_FONT_BYTES { return Err(Refusal::TooLarge { what: "input" }) }
    let source = preflight(bytes)?;

    // 1. Decode to a bare sfnt, keeping every table for now: pinning needs
    //    the variation tables, and `dropped` reports against the source.
    let fd = ReadScope::new(bytes).read::<FontData<'_>>().map_err(|e| Refusal::Malformed(format!("{e:?}")))?;
    let provider = fd.table_provider(0).map_err(|e| Refusal::Malformed(format!("{e:?}")))?;
    let source_tags = provider.table_tags().ok_or_else(|| Refusal::Malformed("no table directory".into()))?;
    let decoded = whole_font(&provider, &source_tags).map_err(|e| Refusal::Malformed(format!("decode: {e:?}")))?;
    if decoded.len() > MAX_FONT_BYTES { return Err(Refusal::TooLarge { what: "decoded font" }) }

    // 2. Pin a variable font to a static instance (condition 3).
    let (static_sfnt, pinned) = pin(&decoded, declared)?;

    // 3. Sanitize: admitted tables only, and no hinting anywhere.
    let (sfnt, stripped) = sanitize(&static_sfnt)?;
    if sfnt.len() > MAX_FONT_BYTES { return Err(Refusal::TooLarge { what: "canonical font" }) }

    // 4. Independent re-validation — the worker's parser must agree.
    let glyphs = revalidate(&sfnt)?;

    let kept: Vec<u32> = table_tags_of(&sfnt)?;
    let mut dropped: Vec<String> = source_tags.iter().filter(|t| !kept.contains(t)).map(|&t| tag_name(t)).collect();
    dropped.sort();
    Ok(Canonical {
        address: format!("blake3:{}", blake3::hash(&sfnt).to_hex()),
        sfnt, source, pinned, dropped, instruction_bytes_stripped: stripped, glyphs,
    })
}

// ---- 0. preflight: identify, and bound every decompression ------------------

fn be32(b: &[u8], at: usize) -> Option<u32> { b.get(at..at + 4).map(|s| u32::from_be_bytes(s.try_into().unwrap())) }
fn be16(b: &[u8], at: usize) -> Option<u16> { b.get(at..at + 2).map(|s| u16::from_be_bytes(s.try_into().unwrap())) }

fn preflight(b: &[u8]) -> Result<Format, Refusal> {
    match b.get(..4) {
        Some(b"wOF2") => { preflight_woff2(b)?; Ok(Format::Woff2) }
        Some(b"wOFF") => { preflight_woff(b)?; Ok(Format::Woff) }
        Some([0, 1, 0, 0]) | Some(b"true") => Ok(Format::TrueType),
        Some(b"OTTO") => Ok(Format::OpenTypeCff),
        Some(b"ttcf") => Err(Refusal::Collection),
        _ => Err(Refusal::UnknownFormat),
    }
}

/// Inflate `r` through a hard limit; refuse past it. The limit is enforced on
/// OUTPUT bytes, which is the quantity a bomb inflates.
fn bounded(mut r: impl Read, limit: usize, what: &'static str) -> Result<usize, Refusal> {
    let mut sink = [0u8; 16 * 1024];
    let mut n = 0usize;
    loop {
        match r.read(&mut sink) {
            Ok(0) => return Ok(n),
            Ok(k) => { n += k; if n > limit { return Err(Refusal::TooLarge { what }) } }
            Err(_) => return Err(Refusal::Malformed(format!("{what}: corrupt compressed stream"))),
        }
    }
}

/// WOFF2: find the Brotli block (after a variable-length table directory) and
/// inflate it through the limit.
fn preflight_woff2(b: &[u8]) -> Result<(), Refusal> {
    let bad = |m: &str| Refusal::Malformed(format!("woff2 header: {m}"));
    if be32(b, 4) == Some(t(b"ttcf")) { return Err(Refusal::Collection) }
    let num_tables = be16(b, 12).ok_or_else(|| bad("short"))? as usize;
    let total_compressed = be32(b, 20).ok_or_else(|| bad("short"))? as usize;
    let mut at = 48; // fixed header size
    for _ in 0..num_tables {
        let flags = *b.get(at).ok_or_else(|| bad("directory"))?;
        at += 1;
        let version = (flags >> 6) & 0x3;
        // ★ glyf/loca are identified by TAG, not by index: a directory may
        // name any table with an explicit tag (index 63), `glyf` included, and
        // keying on indices 10/11 alone would read a transformLength that is
        // not there and walk the rest of the directory out of step.
        let is_glyf_loca = match flags & 0x3f {
            10 | 11 => true,
            0x3f => {
                let tg = be32(b, at).ok_or_else(|| bad("explicit tag"))?;
                at += 4;
                tg == t(b"glyf") || tg == t(b"loca")
            }
            _ => false,
        };
        let (_orig, len) = base128(b, at).ok_or_else(|| bad("origLength"))?;
        at += len;
        // glyf/loca: transformLength present unless version 3 (null).
        // Every other table: present only if version != 0.
        let transformed = if is_glyf_loca { version != 3 } else { version != 0 };
        if transformed {
            let (_tl, len) = base128(b, at).ok_or_else(|| bad("transformLength"))?;
            at += len;
        }
    }
    let block = b.get(at..at.checked_add(total_compressed).ok_or_else(|| bad("overflow"))?)
        .ok_or_else(|| bad("compressed block past end of file"))?;
    bounded(brotli_decompressor::Decompressor::new(block, 4096), MAX_FONT_BYTES, "woff2 brotli block")?;
    Ok(())
}

fn base128(b: &[u8], at: usize) -> Option<(u32, usize)> {
    let mut v: u32 = 0;
    for i in 0..5 {
        let byte = *b.get(at + i)?;
        if i == 0 && byte == 0x80 { return None } // leading zeros forbidden
        if v & 0xFE00_0000 != 0 { return None }   // would overflow
        v = (v << 7) | u32::from(byte & 0x7f);
        if byte & 0x80 == 0 { return Some((v, i + 1)) }
    }
    None
}

/// WOFF: each compressed table must inflate to EXACTLY its declared original
/// length, and the declared lengths must fit the limit in total.
fn preflight_woff(b: &[u8]) -> Result<(), Refusal> {
    let bad = |m: &str| Refusal::Malformed(format!("woff header: {m}"));
    if be32(b, 4) == Some(t(b"ttcf")) { return Err(Refusal::Collection) }
    let num_tables = be16(b, 12).ok_or_else(|| bad("short"))? as usize;
    let mut total: usize = 0;
    for i in 0..num_tables {
        let e = 44 + 20 * i;
        let off = be32(b, e + 4).ok_or_else(|| bad("directory"))? as usize;
        let comp = be32(b, e + 8).ok_or_else(|| bad("directory"))? as usize;
        let orig = be32(b, e + 12).ok_or_else(|| bad("directory"))? as usize;
        total = total.saturating_add(orig);
        if total > MAX_FONT_BYTES { return Err(Refusal::TooLarge { what: "woff declared tables" }) }
        if comp < orig {
            let data = b.get(off..off.saturating_add(comp)).ok_or_else(|| bad("table past end of file"))?;
            let n = bounded(ZlibReader::new(data), orig, "woff zlib table")?;
            if n != orig { return Err(Refusal::SizeMismatch { what: "woff zlib table" }) }
        }
    }
    Ok(())
}

/// miniz_oxide's one-shot API has a size-limited variant, which is exactly the
/// bound needed; this adapts it to `Read` so both formats share `bounded`.
struct ZlibReader { out: Vec<u8>, pos: usize, err: bool }
impl ZlibReader {
    fn new(data: &[u8]) -> Self {
        match miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(data, MAX_FONT_BYTES + 1) {
            Ok(out) => ZlibReader { out, pos: 0, err: false },
            // Over the limit reports as MORE than the limit, so `bounded`
            // refuses it as too large rather than as corrupt.
            Err(e) if matches!(e.status, miniz_oxide::inflate::TINFLStatus::HasMoreOutput) =>
                ZlibReader { out: vec![0; MAX_FONT_BYTES + 1], pos: 0, err: false },
            Err(_) => ZlibReader { out: vec![], pos: 0, err: true },
        }
    }
}
impl Read for ZlibReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.err { return Err(std::io::Error::other("corrupt zlib")) }
        let n = buf.len().min(self.out.len() - self.pos);
        buf[..n].copy_from_slice(&self.out[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

// ---- 2. pin -----------------------------------------------------------------

fn pin(sfnt: &[u8], declared: Declared) -> Result<(Vec<u8>, Vec<(String, f32)>), Refusal> {
    let face = ttf_parser::Face::parse(sfnt, 0).map_err(|e| Refusal::Malformed(format!("{e:?}")))?;
    if !face.is_variable() { return Ok((sfnt.to_vec(), vec![])) }
    let values: Vec<(String, f32)> = face.variation_axes().into_iter().map(|a| {
        let v = if &a.tag.to_bytes() == b"wght" {
            f32::from(declared.weight).clamp(a.min_value, a.max_value)
        } else {
            a.def_value
        };
        (a.tag.to_string(), v)
    }).collect();
    let user: Vec<allsorts::tables::Fixed> = values.iter().map(|&(_, v)| allsorts::tables::Fixed::from(v)).collect();
    let otf = ReadScope::new(sfnt).read::<OpenTypeFont<'_>>().map_err(|e| Refusal::CannotPin(format!("{e:?}")))?;
    let p = otf.table_provider(0).map_err(|e| Refusal::CannotPin(format!("{e:?}")))?;
    let (out, _) = allsorts::variations::instance(&p, &user).map_err(|e| Refusal::CannotPin(format!("{e:?}")))?;
    Ok((out, values))
}

// ---- 3. sanitize ------------------------------------------------------------

/// A provider that serves the source's tables, except `glyf`/`loca`, which
/// are served from a copy with every glyph's instructions removed.
struct Stripped<'a, P> { inner: &'a P, glyf: Option<(Vec<u8>, Vec<u8>)> }

impl<P: FontTableProvider> FontTableProvider for Stripped<'_, P> {
    fn table_data(&self, tag: u32) -> Result<Option<Cow<'_, [u8]>>, allsorts::error::ParseError> {
        match (&self.glyf, tag) {
            (Some((g, _)), tag::GLYF) => Ok(Some(Cow::Borrowed(g))),
            (Some((_, l)), tag::LOCA) => Ok(Some(Cow::Borrowed(l))),
            _ => self.inner.table_data(tag),
        }
    }
    fn has_table(&self, tag: u32) -> bool { self.inner.has_table(tag) }
    fn table_tags(&self) -> Option<Vec<u32>> { self.inner.table_tags() }
}
impl<P: SfntVersion> SfntVersion for Stripped<'_, P> {
    fn sfnt_version(&self) -> u32 { self.inner.sfnt_version() }
}

/// One glyph's bytes with its hinting instructions removed, and how many
/// instruction bytes that was.
///
/// ★ BYTE-LEVEL, NOT RE-ENCODED. allsorts' glyf writer does not compact
/// coordinates ("TODO" in its source): every point becomes 16-bit deltas and
/// the flag compression is lost. Round-tripping through it doubled the corpus
/// (14.1 → 28.2 MB) and pushed ten CJK fonts past the 8 MiB ceiling. Removing
/// the instruction block from the original encoding keeps every other byte —
/// flags, coordinates, components — exactly as the font had it, and can only
/// shrink the glyph.
fn strip_instructions(d: &[u8]) -> Option<(Vec<u8>, usize)> {
    let contours = be16(d, 0)? as i16;
    if contours >= 0 {
        let at = 10 + 2 * contours as usize;          // instructionLength
        let n = be16(d, at)? as usize;
        let rest = at.checked_add(2 + n)?;
        if rest > d.len() { return None }
        let mut out = Vec::with_capacity(d.len() - n);
        out.extend_from_slice(&d[..at]);
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&d[rest..]);
        Some((out, n))
    } else {
        // Composite: clear WE_HAVE_INSTRUCTIONS (0x0100) on every component,
        // and drop the instruction block that follows the last one.
        let mut out = d.to_vec();
        let (mut p, mut has) = (10, false);
        loop {
            let flags = be16(d, p)?;
            if flags & 0x0100 != 0 {
                has = true;
                out[p..p + 2].copy_from_slice(&(flags & !0x0100).to_be_bytes());
            }
            p += 4 + if flags & 0x0001 != 0 { 4 } else { 2 };
            p += if flags & 0x0008 != 0 { 2 } else if flags & 0x0040 != 0 { 4 } else if flags & 0x0080 != 0 { 8 } else { 0 };
            if p > d.len() { return None }
            if flags & 0x0020 == 0 { break } // no MORE_COMPONENTS
        }
        if !has { return Some((out, 0)) }
        let n = be16(d, p)? as usize;
        if p.checked_add(2 + n)? > d.len() { return None }
        out.truncate(p);
        Some((out, n))
    }
}

fn sanitize(sfnt: &[u8]) -> Result<(Vec<u8>, usize), Refusal> {
    let m = |e: &dyn std::fmt::Debug| Refusal::Malformed(format!("sanitize: {e:?}"));
    let otf = ReadScope::new(sfnt).read::<OpenTypeFont<'_>>().map_err(|e| m(&e))?;
    let p = otf.table_provider(0).map_err(|e| m(&e))?;
    let has_glyf = p.has_table(tag::GLYF);
    if !(has_glyf || p.has_table(t(b"CFF "))) || !p.has_table(tag::CMAP) {
        return Err(Refusal::NoRenderableOutlines);
    }
    let mut stripped = 0usize;
    let glyf = if has_glyf {
        let head = ReadScope::new(&p.read_table_data(tag::HEAD).map_err(|e| m(&e))?).read::<HeadTable>().map_err(|e| m(&e))?;
        let maxp = ReadScope::new(&p.read_table_data(tag::MAXP).map_err(|e| m(&e))?).read::<MaxpTable>().map_err(|e| m(&e))?;
        let loca_data = p.read_table_data(tag::LOCA).map_err(|e| m(&e))?;
        let loca = ReadScope::new(&loca_data)
            .read_dep::<LocaTable<'_>>((maxp.num_glyphs, head.index_to_loc_format)).map_err(|e| m(&e))?;
        let offsets: Vec<usize> = loca.offsets.iter().map(|o| o as usize).collect();
        let glyf_data = p.read_table_data(tag::GLYF).map_err(|e| m(&e))?;
        let long = matches!(head.index_to_loc_format, IndexToLocFormat::Long);
        let (mut glyf, mut new_offsets) = (Vec::with_capacity(glyf_data.len()), Vec::with_capacity(offsets.len()));
        for w in offsets.windows(2) {
            new_offsets.push(glyf.len());
            if w[1] <= w[0] { continue } // empty glyph
            let d = glyf_data.get(w[0]..w[1]).ok_or_else(|| m(&"glyph past end of glyf"))?;
            // ★ Every glyph PARSED, as a structural check: a glyph that does
            // not parse is a refusal, not bytes passed through to the worker.
            ReadScope::new(d).read::<Glyph>().map_err(|e| m(&e))?;
            let (out, n) = strip_instructions(d).ok_or_else(|| m(&"glyph structure"))?;
            stripped += n;
            glyf.extend_from_slice(&out);
            while glyf.len() % if long { 4 } else { 2 } != 0 { glyf.push(0) }
        }
        new_offsets.push(glyf.len());
        let mut loca_out = Vec::with_capacity(new_offsets.len() * 4);
        for o in new_offsets {
            if long { loca_out.extend_from_slice(&(o as u32).to_be_bytes()) }
            else {
                let half = u16::try_from(o / 2).map_err(|_| m(&"short loca overflow"))?;
                loca_out.extend_from_slice(&half.to_be_bytes());
            }
        }
        Some((glyf, loca_out))
    } else {
        None
    };
    let keep: Vec<u32> = p.table_tags().unwrap_or_default().into_iter()
        .filter(|tg| ADMITTED.iter().any(|a| t(a) == *tg)).collect();
    let provider = Stripped { inner: &p, glyf };
    let out = whole_font(&provider, &keep).map_err(|e| m(&e))?;
    Ok((out, stripped))
}

// ---- 4. revalidate ----------------------------------------------------------

fn table_tags_of(sfnt: &[u8]) -> Result<Vec<u32>, Refusal> {
    let otf = ReadScope::new(sfnt).read::<OpenTypeFont<'_>>().map_err(|e| Refusal::FailedRevalidation(format!("{e:?}")))?;
    let p = otf.table_provider(0).map_err(|e| Refusal::FailedRevalidation(format!("{e:?}")))?;
    p.table_tags().ok_or_else(|| Refusal::FailedRevalidation("no table directory".into()))
}

/// The worker's parser must accept the result on its own terms.
fn revalidate(sfnt: &[u8]) -> Result<u16, Refusal> {
    let f = |m: String| Refusal::FailedRevalidation(m);
    let face = ttf_parser::Face::parse(sfnt, 0).map_err(|e| f(format!("{e:?}")))?;
    if face.is_variable() { return Err(f("still variable after pinning".into())) }
    let raw = face.raw_face();
    for tg in [b"fpgm", b"prep", b"cvt ", b"fvar", b"gvar", b"CFF2"] {
        if raw.table(ttf_parser::Tag::from_bytes(tg)).is_some() {
            return Err(f(format!("{} survived sanitizing", String::from_utf8_lossy(tg))));
        }
    }
    for rec in raw.table_records {
        if !ADMITTED.iter().any(|a| ttf_parser::Tag::from_bytes(a) == rec.tag) {
            return Err(f(format!("unadmitted table {} in output", rec.tag)));
        }
    }
    let n = face.number_of_glyphs();
    if usize::from(n) > MAX_GLYPHS { return Err(Refusal::TooManyGlyphs(n.into())) }
    Ok(n)
}
