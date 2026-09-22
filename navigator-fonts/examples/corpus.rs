//! Canonicalize every font in a directory and check the result with tools the
//! step itself does not use.
//!
//!   corpus <dir-of-fetched-fonts>
//!
//! Per font, beyond `canonicalize`'s own re-validation:
//! - **deterministic** — a second run yields the same address;
//! - **cmap preserved** — every mapped code point maps to the same glyph id;
//! - **outlines preserved** (static sources) — every glyph's path is identical
//!   to the source's, so sanitizing removed hints and tables, never shapes;
//! - **no instructions left** — counted by `instruction_bytes`, a `glyf` walker
//!   written here, and positive-controlled on the SOURCE, where hinted fonts
//!   must show instructions or the checker is reading nothing.

use navigator_fonts::{canonicalize, Declared};
use ttf_parser::{Face, OutlineBuilder};

#[derive(Default, PartialEq)]
struct Path(Vec<(u8, i32, i32, i32, i32, i32, i32)>);
impl OutlineBuilder for Path {
    fn move_to(&mut self, x: f32, y: f32) { self.0.push((b'M', x as i32, y as i32, 0, 0, 0, 0)) }
    fn line_to(&mut self, x: f32, y: f32) { self.0.push((b'L', x as i32, y as i32, 0, 0, 0, 0)) }
    fn quad_to(&mut self, a: f32, b: f32, x: f32, y: f32) { self.0.push((b'Q', a as i32, b as i32, x as i32, y as i32, 0, 0)) }
    fn curve_to(&mut self, a: f32, b: f32, c: f32, d: f32, x: f32, y: f32) {
        self.0.push((b'C', a as i32, b as i32, c as i32, d as i32, x as i32, y as i32))
    }
    fn close(&mut self) { self.0.push((b'Z', 0, 0, 0, 0, 0, 0)) }
}

fn table<'a>(f: &Face<'a>, t: &[u8; 4]) -> Option<&'a [u8]> { f.raw_face().table(ttf_parser::Tag::from_bytes(t)) }
fn u16be(b: &[u8], i: usize) -> Option<u16> { b.get(i..i + 2).map(|s| u16::from_be_bytes([s[0], s[1]])) }
fn u32be(b: &[u8], i: usize) -> Option<u32> { b.get(i..i + 4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]])) }

/// Total per-glyph instruction bytes, read straight from `glyf`/`loca`.
fn instruction_bytes(f: &Face) -> Option<usize> {
    let (glyf, loca, head) = (table(f, b"glyf")?, table(f, b"loca")?, table(f, b"head")?);
    let long = u16be(head, 50)? == 1;
    let n = f.number_of_glyphs() as usize;
    let off = |i: usize| if long { u32be(loca, i * 4).map(|v| v as usize) } else { u16be(loca, i * 2).map(|v| v as usize * 2) };
    let mut total = 0;
    for g in 0..n {
        let (a, b) = (off(g)?, off(g + 1)?);
        if b <= a { continue }
        let d = glyf.get(a..b)?;
        let contours = u16be(d, 0)? as i16;
        if contours >= 0 {
            total += u16be(d, 10 + 2 * contours as usize)? as usize;
        } else {
            // Composite: walk components; instructions follow the last one
            // when any component set WE_HAVE_INSTRUCTIONS (0x0100).
            let (mut p, mut has) = (10, false);
            loop {
                let flags = u16be(d, p)?;
                has |= flags & 0x0100 != 0;
                p += 4 + if flags & 0x0001 != 0 { 4 } else { 2 };
                p += if flags & 0x0008 != 0 { 2 } else if flags & 0x0040 != 0 { 4 } else if flags & 0x0080 != 0 { 8 } else { 0 };
                if flags & 0x0020 == 0 { break }
            }
            if has { total += u16be(d, p)? as usize }
        }
    }
    Some(total)
}

fn decode_source(bytes: &[u8]) -> Option<Vec<u8>> {
    use allsorts::binary::read::ReadScope;
    use allsorts::font_data::FontData;
    use allsorts::tables::FontTableProvider;
    let fd = ReadScope::new(bytes).read::<FontData<'_>>().ok()?;
    let p = fd.table_provider(0).ok()?;
    allsorts::subset::whole_font(&p, &p.table_tags()?).ok()
}

fn main() {
    let dir = std::env::args().nth(1).expect("directory of fonts");
    let mut files: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.path()).collect();
    files.sort();
    let (mut ok, mut refused, mut fail) = (0, 0, 0);
    let (mut hinted_src, mut src_instr, mut out_instr, mut pinned) = (0, 0usize, 0usize, 0);
    let (mut outlines_checked, mut glyphs_compared) = (0, 0usize);
    let mut refusals: std::collections::BTreeMap<String, u32> = Default::default();
    let (mut wire, mut canon) = (0usize, 0usize);
    for p in &files {
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        let bytes = std::fs::read(p).unwrap();
        let c = match canonicalize(&bytes, Declared::default()) {
            Ok(c) => c,
            Err(r) => {
                refused += 1;
                let k = format!("{r:?}"); let k = k.split(['(', '{']).next().unwrap().trim().to_string();
                *refusals.entry(k).or_default() += 1;
                continue
            }
        };
        let mut problems: Vec<String> = vec![];
        if canonicalize(&bytes, Declared::default()).map(|d| d.address) != Ok(c.address.clone()) {
            problems.push("NOT DETERMINISTIC".into());
        }
        let out = Face::parse(&c.sfnt, 0).expect("revalidated");
        let src_bytes = decode_source(&bytes).expect("canonicalized, so it decodes");
        let src = Face::parse(&src_bytes, 0).expect("decodes");

        // Positive control first: the walker must SEE instructions in a hinted source.
        let si = instruction_bytes(&src).unwrap_or(0);
        if table(&src, b"fpgm").is_some() || table(&src, b"prep").is_some() { hinted_src += 1 }
        src_instr += si;
        let oi = instruction_bytes(&out).unwrap_or(0);
        out_instr += oi;
        if oi != 0 { problems.push(format!("{oi} instruction bytes remain")) }
        if si != c.instruction_bytes_stripped && table(&src, b"glyf").is_some() {
            problems.push(format!("walker saw {si} source instruction bytes, step reports {}", c.instruction_bytes_stripped));
        }

        // cmap: every code point the source maps must map to the same glyph.
        if let Some(sub) = src.tables().cmap.and_then(|c| c.subtables.into_iter().find(|s| s.is_unicode())) {
            let mut bad = 0;
            sub.codepoints(|cp| {
                if let Some(ch) = char::from_u32(cp) {
                    if src.glyph_index(ch) != out.glyph_index(ch) { bad += 1 }
                }
            });
            if bad > 0 { problems.push(format!("{bad} code points remapped")) }
        }

        // Outlines, for static sources (a pinned font's source outlines are the
        // variable default, not the instance, so they are not comparable).
        if c.pinned.is_empty() {
            outlines_checked += 1;
            let mut diff = 0;
            for g in 0..src.number_of_glyphs() {
                let (mut a, mut b) = (Path::default(), Path::default());
                let ra = src.outline_glyph(ttf_parser::GlyphId(g), &mut a);
                let rb = out.outline_glyph(ttf_parser::GlyphId(g), &mut b);
                if ra != rb || a != b { diff += 1 }
                glyphs_compared += 1;
            }
            if diff > 0 { problems.push(format!("{diff} glyph outlines differ")) }
        } else {
            pinned += 1;
        }
        if out.number_of_glyphs() != src.number_of_glyphs() { problems.push("glyph count changed".into()) }

        wire += bytes.len(); canon += c.sfnt.len();
        if problems.is_empty() { ok += 1 } else { fail += 1; println!("FAIL {name}: {}", problems.join("; ")) }
    }
    println!("fonts {}  canonical+verified {ok}  FAILED {fail}  refused {refused} {refusals:?}", files.len());
    println!("pinned {pinned}; outlines compared on {outlines_checked} static fonts ({glyphs_compared} glyphs)");
    println!("hinted sources {hinted_src}; instruction bytes: source {src_instr} (control: must be > 0), output {out_instr} (must be 0)");
    println!("bytes: wire {:.1} MB -> canonical {:.1} MB", wire as f64 / 1e6, canon as f64 / 1e6);
    if fail > 0 || out_instr > 0 || (hinted_src > 0 && src_instr == 0) { std::process::exit(1) }
}
