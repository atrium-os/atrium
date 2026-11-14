//! PDF export (docs/spec/officina.md §7.4 "Export", §14).
//!
//! Pages come from `officina_text::paint`, the same items the screen draws.
//! The file is written page by page: a page's content stream goes out as soon
//! as it is painted, and layouts are dropped every [`CACHE_PAGES`] pages, so
//! memory stays about a page's worth however long the document is. Each face
//! is embedded once as a TrueType subset of the glyphs used (CIDs are glyph
//! ids, with a ToUnicode map so text copies and searches); each picture is
//! embedded once (JPEG as it is, anything else decoded and deflated); links
//! become URI annotations (http, https and mailto only).

pub mod font;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::io::{self, Write};

use officina_text::layout::{Fonts, LayoutCache, PinnedFonts};
use officina_text::lists::Numbering;
use officina_text::model::TextDoc;
use officina_text::paginate::Paginator;
use officina_text::paint::{paint_page, Item};
use officina_text::{twips, LU};

/// Layouts are dropped after this many pages (memory stays flat).
pub const CACHE_PAGES: usize = 64;

/// Where face files come from.
pub trait FontFiles {
    fn face(&self, face: u16) -> Option<&[u8]>;
}

impl FontFiles for PinnedFonts {
    fn face(&self, face: u16) -> Option<&[u8]> {
        Some(self.face_bytes(face))
    }
}

/// Document information.
#[derive(Debug, Clone, Default)]
pub struct Info {
    pub title: String,
}

/// What an export wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stats {
    pub pages: usize,
    pub fonts: usize,
    pub images: usize,
    pub links: usize,
    pub bytes: u64,
}

struct Out<W: Write> {
    w: W,
    at: u64,
    offsets: Vec<u64>,
}

impl<W: Write> Out<W> {
    fn raw(&mut self, b: &[u8]) -> io::Result<()> {
        self.w.write_all(b)?;
        self.at += b.len() as u64;
        Ok(())
    }

    /// A fresh object number.
    fn reserve(&mut self) -> usize {
        self.offsets.push(0);
        self.offsets.len()
    }

    fn obj(&mut self, n: usize, body: &str) -> io::Result<()> {
        self.offsets[n - 1] = self.at;
        self.raw(format!("{n} 0 obj\n{body}\nendobj\n").as_bytes())
    }

    /// A stream object, deflated when `deflate`.
    fn stream(&mut self, n: usize, dict: &str, data: &[u8], deflate: bool) -> io::Result<()> {
        self.offsets[n - 1] = self.at;
        let (data, filter) = if deflate { (miniz_oxide::deflate::compress_to_vec_zlib(data, 6), " /Filter /FlateDecode") } else { (data.to_vec(), "") };
        self.raw(format!("{n} 0 obj\n<< {dict}{filter} /Length {} >>\nstream\n", data.len()).as_bytes())?;
        self.raw(&data)?;
        self.raw(b"\nendstream\nendobj\n")
    }
}

/// A face used in the document.
struct FontUse {
    obj: usize,
    glyphs: BTreeMap<u16, String>,
}

/// A number with at most three decimals, no trailing zeros.
fn num(v: f64) -> String {
    let r = (v * 1000.0).round() / 1000.0;
    if r == r.trunc() {
        format!("{}", r as i64)
    } else {
        let s = format!("{r:.3}");
        s.trim_end_matches('0').to_string()
    }
}

/// Layout units to points.
fn pt(v: i64) -> f64 {
    v as f64 / LU as f64
}

fn rgb(c: u32) -> String {
    let f = |s: u32| num(((c >> s) & 0xff) as f64 / 255.0);
    format!("{} {} {}", f(16), f(8), f(0))
}

/// A PDF literal string.
fn lit(s: &str) -> String {
    let mut o = String::from("(");
    for ch in s.chars() {
        match ch {
            '(' | ')' | '\\' => {
                o.push('\\');
                o.push(ch);
            }
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => o.push('?'),
            c => o.push(c),
        }
    }
    o.push(')');
    o
}

/// A PDF text string: UTF-16 with a byte-order mark, in hex.
fn text_string(s: &str) -> String {
    let mut o = String::from("<FEFF");
    for u in s.encode_utf16() {
        let _ = write!(o, "{u:04X}");
    }
    o.push('>');
    o
}

fn uri_ok(u: &str) -> bool {
    let l = u.to_ascii_lowercase();
    (l.starts_with("https://") || l.starts_with("http://") || l.starts_with("mailto:")) && u.len() <= 2048 && u.chars().all(|c| (' '..='~').contains(&c))
}

/// The JPEG's (width, height, components) when it can go in as it is.
fn jpeg_info(b: &[u8]) -> Option<(u32, u32, u8)> {
    if !b.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut i = 2;
    while i + 4 <= b.len() {
        if b[i] != 0xFF {
            return None;
        }
        let m = b[i + 1];
        if m == 0xFF {
            i += 1;
            continue;
        }
        let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        // Baseline, extended and progressive Huffman frames.
        if matches!(m, 0xC0 | 0xC1 | 0xC2) {
            let s = b.get(i + 4..i + 10)?;
            let (h, w, c) = (u16::from_be_bytes([s[1], s[2]]) as u32, u16::from_be_bytes([s[3], s[4]]) as u32, s[5]);
            return (w > 0 && h > 0 && (c == 1 || c == 3)).then_some((w, h, c));
        }
        if (0xC3..=0xCF).contains(&m) && m != 0xC4 && m != 0xC8 && m != 0xCC {
            return None;
        }
        i += 2 + len;
    }
    None
}

/// Writes `doc` as PDF to `out`, calling `progress(done, total)` after each page.
pub fn export<W: Write>(doc: &TextDoc, fonts: &dyn Fonts, files: &dyn FontFiles, info: &Info, out: W, progress: &mut dyn FnMut(usize, usize)) -> io::Result<(W, Stats)> {
    // Pagination first: `{pages}` needs the count.
    let mut cache = LayoutCache::default();
    let mut pager = Paginator::new(doc);
    while !pager.run(doc, &mut cache, fonts, CACHE_PAGES) {
        cache = LayoutCache::default();
    }
    let pages = pager.pages().to_vec();
    let col_w = pager.width();
    drop(pager);
    let numbering = Numbering::build(doc);
    let total = pages.len();
    let pages_text = total.to_string();

    let mut o = Out { w: out, at: 0, offsets: Vec::new() };
    o.raw(b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n")?;
    let catalog = o.reserve();
    let tree = o.reserve();
    let info_obj = o.reserve();
    let (pw, ph) = (pt(twips(doc.page.width as i64)), pt(twips(doc.page.height as i64)));
    let mut kids = Vec::with_capacity(total);
    let mut fonts_used: BTreeMap<u16, FontUse> = BTreeMap::new();
    let mut images: HashMap<u32, Option<usize>> = HashMap::new();
    let mut stats = Stats { pages: total, ..Default::default() };

    for (i, _) in pages.iter().enumerate() {
        if i % CACHE_PAGES == 0 {
            cache = LayoutCache::default();
        }
        let items = paint_page(doc, &pages, i, col_w, &mut cache, fonts, &numbering, &pages_text);
        let mut c = String::new();
        let mut annots = Vec::new();
        let mut page_fonts: BTreeSet<u16> = BTreeSet::new();
        let mut page_images: BTreeSet<u32> = BTreeSet::new();
        for it in items {
            match it {
                Item::Rect { x, y, w, h, color } => {
                    let _ = writeln!(c, "{} rg {} {} {} {} re f", rgb(color), num(pt(x)), num(ph - pt(y + h)), num(pt(w)), num(pt(h)));
                }
                Item::Image { image, x, y, w, h } => {
                    let obj = match images.get(&image) {
                        Some(o) => *o,
                        None => {
                            let n = write_image(&mut o, doc, image)?;
                            images.insert(image, n);
                            n
                        }
                    };
                    if obj.is_some() {
                        page_images.insert(image);
                        let _ = writeln!(c, "q {} 0 0 {} {} {} cm /Im{image} Do Q", num(pt(w)), num(pt(h)), num(pt(x)), num(ph - pt(y + h)));
                    } else {
                        let _ = writeln!(c, "0.85 0.86 0.88 rg {} {} {} {} re f", num(pt(x)), num(ph - pt(y + h)), num(pt(w)), num(pt(h)));
                    }
                }
                // Comments stay out of the PDF.
                Item::Mark { .. } => {}
                Item::Link { x, y, w, h, uri } => {
                    if uri_ok(&uri) {
                        annots.push(format!("<< /Type /Annot /Subtype /Link /Rect [{} {} {} {}] /Border [0 0 0] /A << /S /URI /URI {} >> >>", num(pt(x)), num(ph - pt(y + h)), num(pt(x + w)), num(ph - pt(y)), lit(&uri)));
                    }
                }
                Item::Glyphs(r) => {
                    let Some(bytes) = files.face(r.face) else { continue };
                    let Some(face) = font::Face::parse(bytes) else { continue };
                    let upem = face.info.upem as f64;
                    let next = o.offsets.len() + 1;
                    let fu = fonts_used.entry(r.face).or_insert_with(|| FontUse { obj: next, glyphs: BTreeMap::new() });
                    if fu.obj == next {
                        o.reserve();
                    }
                    page_fonts.insert(r.face);
                    let size = pt(r.size);
                    if size <= 0.0 {
                        continue;
                    }
                    let _ = write!(c, "BT /F{} {} Tf {} rg ", r.face, num(size), rgb(r.color));
                    // Glyphs on the baseline in one TJ; a glyph raised or
                    // lowered starts a new one with a rise.
                    let mut k = 0;
                    while k < r.glyphs.len() {
                        let dy = r.glyphs[k].dy;
                        let _ = write!(c, "{} Ts 1 0 0 1 {} {} Tm [", num(-pt(dy)), num(pt(r.x + r.glyphs[k].x)), num(ph - pt(r.y)));
                        let x0 = r.glyphs[k].x;
                        let mut pen = 0.0f64;
                        while k < r.glyphs.len() && r.glyphs[k].dy == dy {
                            let g = &r.glyphs[k];
                            let want = pt(g.x - x0);
                            let adj = (pen - want) * 1000.0 / size;
                            if adj.abs() >= 0.001 {
                                let _ = write!(c, "{}", num(adj));
                            }
                            let _ = write!(c, "<{:04X}>", g.id);
                            pen = want + face.advance(g.id) as f64 / upem * size;
                            let t = &r.text[g.text.start as usize..g.text.end as usize];
                            if !t.is_empty() {
                                fu.glyphs.entry(g.id).or_insert_with(|| t.to_string());
                            } else {
                                fu.glyphs.entry(g.id).or_default();
                            }
                            k += 1;
                        }
                        c.push_str("] TJ ");
                    }
                    c.push_str("ET\n");
                }
            }
        }
        let content = o.reserve();
        o.stream(content, "", c.as_bytes(), true)?;
        let page = o.reserve();
        let mut res = String::from("<< ");
        if !page_fonts.is_empty() {
            res.push_str("/Font << ");
            for f in &page_fonts {
                let _ = write!(res, "/F{f} {} 0 R ", fonts_used[f].obj);
            }
            res.push_str(">> ");
        }
        if !page_images.is_empty() {
            res.push_str("/XObject << ");
            for im in &page_images {
                let _ = write!(res, "/Im{im} {} 0 R ", images[im].expect("written"));
            }
            res.push_str(">> ");
        }
        res.push_str(">>");
        stats.links += annots.len();
        let annots = if annots.is_empty() { String::new() } else { format!(" /Annots [{}]", annots.join(" ")) };
        o.obj(page, &format!("<< /Type /Page /Parent {tree} 0 R /MediaBox [0 0 {} {}] /Resources {res} /Contents {content} 0 R{annots} >>", num(pw), num(ph)))?;
        kids.push(page);
        progress(i + 1, total);
    }

    // The fonts, now that every glyph is known.
    for (&face, fu) in &fonts_used {
        let bytes = files.face(face).expect("parsed before");
        let f = font::Face::parse(bytes).expect("parsed before");
        let used: BTreeSet<u16> = fu.glyphs.keys().copied().collect();
        let sub = f.subset(&used);
        let scale = 1000.0 / f.info.upem as f64;
        let s = |v: i16| num(v as f64 * scale);
        let (file, desc, cid, uni) = (o.reserve(), o.reserve(), o.reserve(), o.reserve());
        o.stream(file, &format!("/Length1 {}", sub.len()), &sub, true)?;
        // A subset's name: six letters from the face and its glyphs, then the face.
        let tag: String = {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ face as u64;
            for g in &used {
                h = (h ^ *g as u64).wrapping_mul(0x100_0000_01b3);
            }
            (0..6).map(|k| (b'A' + ((h >> (k * 5)) % 26) as u8) as char).collect()
        };
        let name = format!("{tag}+OfficinaFace{face}");
        let [x0, y0, x1, y1] = f.info.bbox;
        let flags = 4 | if f.info.italic_angle != 0 { 64 } else { 0 };
        let angle = (f.info.italic_angle >> 16) as f64 + (f.info.italic_angle & 0xffff) as f64 / 65536.0;
        o.obj(desc, &format!("<< /Type /FontDescriptor /FontName /{name} /Flags {flags} /FontBBox [{} {} {} {}] /ItalicAngle {} /Ascent {} /Descent {} /CapHeight {} /StemV 80 /FontFile2 {file} 0 R >>", s(x0), s(y0), s(x1), s(y1), num(angle), s(f.info.ascent), s(f.info.descent), s(f.info.ascent)))?;
        let mut w = String::new();
        for g in &used {
            let _ = write!(w, "{g} [{}] ", num(f.advance(*g) as f64 * scale));
        }
        o.obj(cid, &format!("<< /Type /Font /Subtype /CIDFontType2 /BaseFont /{name} /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /FontDescriptor {desc} 0 R /CIDToGIDMap /Identity /DW 1000 /W [{w}] >>"))?;
        o.stream(uni, "", to_unicode(&fu.glyphs).as_bytes(), true)?;
        o.obj(fu.obj, &format!("<< /Type /Font /Subtype /Type0 /BaseFont /{name} /Encoding /Identity-H /DescendantFonts [{cid} 0 R] /ToUnicode {uni} 0 R >>"))?;
    }
    stats.fonts = fonts_used.len();
    stats.images = images.values().filter(|v| v.is_some()).count();

    let kids_s: Vec<String> = kids.iter().map(|k| format!("{k} 0 R")).collect();
    o.obj(tree, &format!("<< /Type /Pages /Kids [{}] /Count {} >>", kids_s.join(" "), kids.len()))?;
    o.obj(catalog, &format!("<< /Type /Catalog /Pages {tree} 0 R >>"))?;
    let title = if info.title.is_empty() { String::new() } else { format!("/Title {} ", text_string(&info.title)) };
    o.obj(info_obj, &format!("<< {title}/Producer (Officina) /Creator (Atrium Write) >>"))?;

    let xref = o.at;
    let mut x = format!("xref\n0 {}\n0000000000 65535 f \n", o.offsets.len() + 1);
    for off in &o.offsets {
        let _ = write!(x, "{off:010} 00000 n \n");
    }
    let _ = write!(x, "trailer\n<< /Size {} /Root {catalog} 0 R /Info {info_obj} 0 R >>\nstartxref\n{xref}\n%%EOF\n", o.offsets.len() + 1);
    o.raw(x.as_bytes())?;
    o.w.flush()?;
    stats.bytes = o.at;
    Ok((o.w, stats))
}

/// Picture `image` as an image XObject; None when it can't be read.
fn write_image<W: Write>(o: &mut Out<W>, doc: &TextDoc, image: u32) -> io::Result<Option<usize>> {
    let Some(data) = doc.images.get(image) else { return Ok(None) };
    if let Some((w, h, comps)) = jpeg_info(&data.bytes) {
        let n = o.reserve();
        let cs = if comps == 1 { "/DeviceGray" } else { "/DeviceRGB" };
        o.stream(n, &format!("/Type /XObject /Subtype /Image /Width {w} /Height {h} /ColorSpace {cs} /BitsPerComponent 8 /Filter /DCTDecode"), &data.bytes, false)?;
        return Ok(Some(n));
    }
    let Ok(px) = navigator_image::image::decode_unaddressed(&data.bytes, navigator_image::image::Ceilings::default()) else { return Ok(None) };
    let (w, h) = (px.width, px.height);
    let count = (w as usize) * (h as usize);
    if px.rgba.len() < count * 4 || count == 0 {
        return Ok(None);
    }
    let mut rgb = Vec::with_capacity(count * 3);
    let mut alpha = Vec::with_capacity(count);
    for p in px.rgba.chunks_exact(4).take(count) {
        rgb.extend_from_slice(&p[..3]);
        alpha.push(p[3]);
    }
    let smask = if alpha.iter().any(|&a| a != 255) {
        let m = o.reserve();
        o.stream(m, &format!("/Type /XObject /Subtype /Image /Width {w} /Height {h} /ColorSpace /DeviceGray /BitsPerComponent 8"), &alpha, true)?;
        format!(" /SMask {m} 0 R")
    } else {
        String::new()
    };
    let n = o.reserve();
    o.stream(n, &format!("/Type /XObject /Subtype /Image /Width {w} /Height {h} /ColorSpace /DeviceRGB /BitsPerComponent 8{smask}"), &rgb, true)?;
    Ok(Some(n))
}

/// The ToUnicode CMap: each glyph to the text it first showed.
fn to_unicode(glyphs: &BTreeMap<u16, String>) -> String {
    let mut s = String::from("/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n/CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n");
    let with_text: Vec<(&u16, &String)> = glyphs.iter().filter(|(_, t)| !t.is_empty()).collect();
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
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_and_strings() {
        assert_eq!(num(1.0), "1");
        assert_eq!(num(-0.5), "-0.5");
        assert_eq!(num(12.3456), "12.346");
        assert_eq!(lit("a(b)\\"), "(a\\(b\\)\\\\)");
        assert_eq!(text_string("é"), "<FEFF00E9>");
        assert!(uri_ok("https://example.org/a?b=c"));
        assert!(!uri_ok("javascript:alert(1)"));
        assert!(!uri_ok("file:///etc/passwd"));
    }
}
