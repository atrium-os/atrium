//! PDF export (docs/spec/officina.md §7.4 "Export", §14).
//!
//! Pages come from `officina_text::paint`, the same items the screen draws.
//! The file is written page by page: a page's content stream goes out as soon
//! as it is painted, and layouts are dropped every [`CACHE_PAGES`] pages, so
//! memory stays about a page's worth however long the document is. Each face
//! is embedded once as a TrueType subset of the glyphs used (CIDs are glyph
//! ids, with a ToUnicode map so text copies and searches); each picture is
//! embedded once (JPEG as it is, anything else decoded and deflated); links
//! become URI annotations (http, https and mailto only). Colour glyphs
//! (emoji of Noto Color Emoji) are drawn as pictures, each glyph once
//! ([`EMOJI_PX`] to the em), over their text written invisibly, so the
//! text still copies and searches.

/// The TrueType subsetter, now Folium's (folium.md §5.1).
pub use folium_font::subset as font;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{self, Write};

use folium_content::ops::{write_ops, Op, TjPart};
use folium_syntax::write::{array, dict, int, name, nums, real, reference, text_string, Writer};
use folium_syntax::{Dict, Object, Ref};
use officina_text::layout::{Fonts, LayoutCache, PinnedFonts};
use officina_text::lists::Numbering;
use officina_text::model::TextDoc;
use officina_text::paginate::Paginator;
use officina_text::paint::{paint_page, Item};
use officina_text::{twips, LU};

/// Layouts are dropped after this many pages (memory stays flat).
pub const CACHE_PAGES: usize = 64;

/// A colour glyph's picture: pixels to the em.
pub const EMOJI_PX: f32 = 160.0;

/// A colour glyph written as a picture: its object, size and where it
/// hangs off the pen (pixels at [`EMOJI_PX`]).
#[derive(Clone, Copy)]
struct Emoji {
    obj: Ref,
    w: u32,
    h: u32,
    left: i32,
    top: i32,
}

/// Where face files come from.
pub trait FontFiles {
    fn face(&self, face: u16) -> Option<&[u8]>;
}

impl FontFiles for PinnedFonts {
    fn face(&self, face: u16) -> Option<&[u8]> {
        // Empty when a library face's file is missing or changed.
        Some(self.face_bytes(face)).filter(|b| !b.is_empty())
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

/// A face used in the document.
struct FontUse {
    obj: Ref,
    glyphs: BTreeMap<u16, String>,
}

/// Layout units to points.
fn pt(v: i64) -> f64 {
    v as f64 / LU as f64
}

/// Fractional layout units to points.
fn pt_f(v: f64) -> f64 {
    v / LU as f64
}

fn rgb(c: u32) -> [f64; 3] {
    let f = |s: u32| ((c >> s) & 0xff) as f64 / 255.0;
    [f(16), f(8), f(0)]
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

    let mut o = Writer::new(out, "1.7")?;
    let catalog = o.reserve();
    let tree = o.reserve();
    let info_obj = o.reserve();
    let mut kids = Vec::with_capacity(total);
    let mut fonts_used: BTreeMap<u16, FontUse> = BTreeMap::new();
    let mut images: HashMap<u32, Option<Ref>> = HashMap::new();
    let mut color_faces: HashMap<u16, bool> = HashMap::new();
    let mut emoji: HashMap<(u16, u16), Option<Emoji>> = HashMap::new();
    let mut stats = Stats { pages: total, ..Default::default() };

    for (i, pg) in pages.iter().enumerate() {
        // Each page its own size (a section may have its own).
        let (pw, ph) = (pt(twips(pg.setup.width as i64)), pt(twips(pg.setup.height as i64)));
        if i % CACHE_PAGES == 0 {
            cache = LayoutCache::default();
        }
        let items = paint_page(doc, &pages, i, col_w, &mut cache, fonts, &numbering, &pages_text);
        let mut c: Vec<Op> = Vec::new();
        let mut annots = Vec::new();
        let mut page_fonts: BTreeSet<u16> = BTreeSet::new();
        let mut page_images: BTreeSet<u32> = BTreeSet::new();
        let mut page_emoji: BTreeSet<(u16, u16)> = BTreeSet::new();
        for it in items {
            match it {
                Item::Rect { x, y, w, h, color } => {
                    c.extend([Op::FillRgb(rgb(color)), Op::Rect(pt(x), ph - pt(y + h), pt(w), pt(h)), Op::Fill]);
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
                        c.extend([Op::Save, Op::Transform([pt(w), 0.0, 0.0, pt(h), pt(x), ph - pt(y + h)]), Op::XObject(format!("Im{image}")), Op::Restore]);
                    } else {
                        c.extend([Op::FillRgb([0.85, 0.86, 0.88]), Op::Rect(pt(x), ph - pt(y + h), pt(w), pt(h)), Op::Fill]);
                    }
                }
                // Comments stay out of the PDF.
                Item::Mark { .. } | Item::Change { .. } | Item::Shape { .. } => {}
                Item::Path { cmds, fill, line } => {
                    use officina_text::model::{Dash, PathCmd};
                    let p = |x: i64, y: i64| (pt(x), ph - pt(y));
                    let mut path = Vec::with_capacity(cmds.len());
                    for cm in &cmds {
                        path.push(match *cm {
                            PathCmd::Move(x, y) => {
                                let (x, y) = p(x, y);
                                Op::MoveTo(x, y)
                            }
                            PathCmd::Line(x, y) => {
                                let (x, y) = p(x, y);
                                Op::LineTo(x, y)
                            }
                            PathCmd::Cubic(a, b, cc, d, e, f) => {
                                let ((a, b), (cc, d), (e, f)) = (p(a, b), p(cc, d), p(e, f));
                                Op::CurveTo([a, b, cc, d, e, f])
                            }
                            PathCmd::Close => Op::ClosePath,
                        });
                    }
                    if let Some(f) = fill {
                        c.push(Op::FillRgb(rgb(f)));
                        c.extend(path.iter().cloned());
                        c.push(Op::Fill);
                    }
                    if let Some((color, width, dash)) = line {
                        let w = pt(width);
                        let pattern = match dash {
                            Dash::Solid => vec![],
                            Dash::Dot => vec![w, w],
                            Dash::Dash => vec![4.0 * w, 3.0 * w],
                            Dash::DashDot => vec![4.0 * w, 3.0 * w, w, 3.0 * w],
                            Dash::LongDash => vec![8.0 * w, 3.0 * w],
                        };
                        c.extend([Op::Save, Op::StrokeRgb(rgb(color)), Op::LineWidth(w), Op::Dash(pattern, 0.0)]);
                        c.extend(path);
                        c.extend([Op::Stroke, Op::Restore]);
                    }
                }
                Item::Link { x, y, w, h, uri } => {
                    if uri_ok(&uri) {
                        annots.push(Object::Dict(dict([
                            ("Type", name("Annot")),
                            ("Subtype", name("Link")),
                            ("Rect", nums(&[pt(x), ph - pt(y + h), pt(x + w), ph - pt(y)])),
                            ("Border", array([int(0), int(0), int(0)])),
                            ("A", Object::Dict(dict([("S", name("URI")), ("URI", Object::String(uri.as_bytes().into()))]))),
                        ])));
                    }
                }
                Item::Glyphs(r) => {
                    let Some(bytes) = files.face(r.face) else { continue };
                    let Some(face) = font::Face::parse(bytes) else { continue };
                    let upem = face.info.upem as f64;
                    let fu = match fonts_used.entry(r.face) {
                        std::collections::btree_map::Entry::Occupied(e) => e.into_mut(),
                        std::collections::btree_map::Entry::Vacant(e) => e.insert(FontUse { obj: o.reserve(), glyphs: BTreeMap::new() }),
                    };
                    page_fonts.insert(r.face);
                    let size = pt(r.size);
                    if size <= 0.0 {
                        continue;
                    }
                    let color = *color_faces.entry(r.face).or_insert_with(|| atrium_colr::is_color_font(bytes));
                    c.extend([Op::BeginText, Op::Font(format!("F{}", r.face), size), Op::FillRgb(rgb(r.color))]);
                    if color {
                        c.push(Op::TextRender(3));
                    }
                    // Glyphs on the baseline in one TJ; a glyph raised or
                    // lowered starts a new one with a rise.
                    let mut k = 0;
                    while k < r.glyphs.len() {
                        let dy = r.glyphs[k].dy;
                        c.push(Op::Rise(-pt(dy)));
                        // Turned runs (a turned shape's words): the matrix
                        // turns, the pen starts along the turned baseline
                        // (y flips: the page's y is up).
                        let [ta, tb, tc, td] = r.turn.unwrap_or([1.0, 0.0, 0.0, 1.0]);
                        let gx = r.glyphs[k].x as f64;
                        let (sx, sy) = (r.x as f64 + ta * gx, r.y as f64 + tb * gx);
                        c.push(Op::TextMatrix([ta, -tb, -tc, td, pt_f(sx), ph - pt_f(sy)]));
                        let x0 = r.glyphs[k].x;
                        let mut pen = 0.0f64;
                        let mut parts: Vec<TjPart> = Vec::new();
                        while k < r.glyphs.len() && r.glyphs[k].dy == dy {
                            let g = &r.glyphs[k];
                            let want = pt(g.x - x0);
                            let adj = (pen - want) * 1000.0 / size;
                            if adj.abs() >= 0.001 {
                                parts.push(TjPart::Adjust(adj));
                            }
                            match parts.last_mut() {
                                Some(TjPart::Codes(b)) => b.extend(g.id.to_be_bytes()),
                                _ => parts.push(TjPart::Codes(g.id.to_be_bytes().to_vec())),
                            }
                            pen = want + face.advance(g.id) as f64 / upem * size;
                            let t = &r.text[g.text.start as usize..g.text.end as usize];
                            if !t.is_empty() {
                                fu.glyphs.entry(g.id).or_insert_with(|| t.to_string());
                            } else {
                                fu.glyphs.entry(g.id).or_default();
                            }
                            k += 1;
                        }
                        c.push(Op::ShowArray(parts));
                    }
                    if color {
                        c.push(Op::TextRender(0));
                    }
                    c.push(Op::EndText);
                    if color {
                        // The pictures, each on its glyph's pen position.
                        let [ta, tb, tc, td] = r.turn.unwrap_or([1.0, 0.0, 0.0, 1.0]);
                        let px = size / EMOJI_PX as f64;
                        for g in &r.glyphs {
                            let e = match emoji.get(&(r.face, g.id)) {
                                Some(e) => *e,
                                None => {
                                    let e = write_emoji(&mut o, bytes, g.id)?;
                                    emoji.insert((r.face, g.id), e);
                                    e
                                }
                            };
                            let Some(e) = e else { continue };
                            page_emoji.insert((r.face, g.id));
                            let gx = g.x as f64;
                            let (sx, sy) = (r.x as f64 + ta * gx, r.y as f64 + tb * gx);
                            c.extend([
                                Op::Save,
                                Op::Transform([ta, -tb, -tc, td, pt_f(sx), ph - pt_f(sy)]),
                                Op::Transform([e.w as f64 * px, 0.0, 0.0, e.h as f64 * px, e.left as f64 * px, (e.top as f64 - e.h as f64) * px - pt(g.dy)]),
                                Op::XObject(format!("Em{}_{}", r.face, g.id)),
                                Op::Restore,
                            ]);
                        }
                    }
                }
            }
        }
        let content = o.reserve();
        o.stream(content, Dict::new(), &write_ops(&c), true)?;
        let page = o.reserve();
        let mut res = Dict::new();
        if !page_fonts.is_empty() {
            let mut f = Dict::new();
            for face in &page_fonts {
                f.insert(&format!("F{face}"), reference(fonts_used[face].obj));
            }
            res.insert("Font", Object::Dict(f));
        }
        if !page_images.is_empty() || !page_emoji.is_empty() {
            let mut x = Dict::new();
            for im in &page_images {
                x.insert(&format!("Im{im}"), reference(images[im].expect("written")));
            }
            for k in &page_emoji {
                x.insert(&format!("Em{}_{}", k.0, k.1), reference(emoji[k].expect("written").obj));
            }
            res.insert("XObject", Object::Dict(x));
        }
        stats.links += annots.len();
        let mut p = dict([
            ("Type", name("Page")),
            ("Parent", reference(tree)),
            ("MediaBox", nums(&[0.0, 0.0, pw, ph])),
            ("Resources", Object::Dict(res)),
            ("Contents", reference(content)),
        ]);
        if !annots.is_empty() {
            p.insert("Annots", Object::Array(annots));
        }
        o.obj(page, &Object::Dict(p))?;
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
        let s = |v: i16| v as f64 * scale;
        let (file, desc, cid, uni) = (o.reserve(), o.reserve(), o.reserve(), o.reserve());
        o.stream(file, dict([("Length1", int(sub.len() as i64))]), &sub, true)?;
        // A subset's name: six letters from the face and its glyphs, then the face.
        let tag: String = {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325 ^ face as u64;
            for g in &used {
                h = (h ^ *g as u64).wrapping_mul(0x100_0000_01b3);
            }
            (0..6).map(|k| (b'A' + ((h >> (k * 5)) % 26) as u8) as char).collect()
        };
        let base = name(&format!("{tag}+OfficinaFace{face}"));
        let [x0, y0, x1, y1] = f.info.bbox;
        let flags = 4 | if f.info.italic_angle != 0 { 64 } else { 0 };
        let angle = (f.info.italic_angle >> 16) as f64 + (f.info.italic_angle & 0xffff) as f64 / 65536.0;
        o.obj(desc, &Object::Dict(dict([
            ("Type", name("FontDescriptor")),
            ("FontName", base.clone()),
            ("Flags", int(flags)),
            ("FontBBox", nums(&[s(x0), s(y0), s(x1), s(y1)])),
            ("ItalicAngle", real(angle)),
            ("Ascent", real(s(f.info.ascent))),
            ("Descent", real(s(f.info.descent))),
            ("CapHeight", real(s(f.info.ascent))),
            ("StemV", int(80)),
            ("FontFile2", reference(file)),
        ])))?;
        let mut w = Vec::new();
        for g in &used {
            w.push(int(*g as i64));
            w.push(array([real(f.advance(*g) as f64 * scale)]));
        }
        o.obj(cid, &Object::Dict(dict([
            ("Type", name("Font")),
            ("Subtype", name("CIDFontType2")),
            ("BaseFont", base.clone()),
            ("CIDSystemInfo", Object::Dict(dict([("Registry", Object::String(b"Adobe"[..].into())), ("Ordering", Object::String(b"Identity"[..].into())), ("Supplement", int(0))]))),
            ("FontDescriptor", reference(desc)),
            ("CIDToGIDMap", name("Identity")),
            ("DW", int(1000)),
            ("W", Object::Array(w)),
        ])))?;
        o.stream(uni, Dict::new(), &folium_font::cmap::write_to_unicode(&fu.glyphs), true)?;
        o.obj(fu.obj, &Object::Dict(dict([
            ("Type", name("Font")),
            ("Subtype", name("Type0")),
            ("BaseFont", base),
            ("Encoding", name("Identity-H")),
            ("DescendantFonts", array([reference(cid)])),
            ("ToUnicode", reference(uni)),
        ])))?;
    }
    stats.fonts = fonts_used.len();
    stats.images = images.values().filter(|v| v.is_some()).count();

    o.obj(tree, &Object::Dict(dict([("Type", name("Pages")), ("Kids", array(kids.iter().map(|k| reference(*k)))), ("Count", int(kids.len() as i64))])))?;
    o.obj(catalog, &Object::Dict(dict([("Type", name("Catalog")), ("Pages", reference(tree))])))?;
    let mut inf = dict([("Producer", Object::String(b"Officina"[..].into())), ("Creator", Object::String(b"Atrium Write"[..].into()))]);
    if !info.title.is_empty() {
        inf.insert("Title", text_string(&info.title));
    }
    o.obj(info_obj, &Object::Dict(inf))?;
    let (w, bytes) = o.finish(catalog, Some(info_obj))?;
    stats.bytes = bytes;
    Ok((w, stats))
}

/// Colour glyph `gid` of `face` as an image XObject (RGB with a soft mask);
/// None when the face has no colour version of it.
fn write_emoji<W: Write>(o: &mut Writer<W>, face: &[u8], gid: u16) -> io::Result<Option<Emoji>> {
    let Some(bm) = atrium_colr::render(face, gid as u32, EMOJI_PX, [0.0, 0.0, 0.0, 1.0]) else { return Ok(None) };
    let (w, h) = (bm.width, bm.height);
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    let mut alpha = Vec::with_capacity((w * h) as usize);
    for p in bm.rgba.chunks_exact(4) {
        rgb.extend_from_slice(&p[..3]);
        alpha.push(p[3]);
    }
    let head = |cs: &str| dict([("Type", name("XObject")), ("Subtype", name("Image")), ("Width", int(w as i64)), ("Height", int(h as i64)), ("ColorSpace", name(cs)), ("BitsPerComponent", int(8))]);
    let m = o.reserve();
    o.stream(m, head("DeviceGray"), &alpha, true)?;
    let mut d = head("DeviceRGB");
    d.insert("SMask", reference(m));
    let n = o.reserve();
    o.stream(n, d, &rgb, true)?;
    Ok(Some(Emoji { obj: n, w, h, left: bm.left, top: bm.top }))
}

struct Px {
    rgba: Vec<u8>,
}

/// `bytes` decoded at their natural size: by the app's installed decoder
/// (officina_text::picture), else here when built with `local-decode`.
fn decode(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    if let Some(d) = officina_text::picture::decoder() {
        return d.decode(bytes, None).map(|p| (p.width, p.height, p.rgba));
    }
    #[cfg(feature = "local-decode")]
    {
        use navigator_image::image::{decode_unaddressed, Ceilings};
        decode_unaddressed(bytes, Ceilings::default()).ok().map(|p| (p.width, p.height, p.rgba))
    }
    #[cfg(not(feature = "local-decode"))]
    None
}

/// Picture `image` as an image XObject; None when it can't be read.
fn write_image<W: Write>(o: &mut Writer<W>, doc: &TextDoc, image: u32) -> io::Result<Option<Ref>> {
    let Some(data) = doc.images.get(image) else { return Ok(None) };
    let head = |w: u32, h: u32, cs: &str| dict([("Type", name("XObject")), ("Subtype", name("Image")), ("Width", int(w as i64)), ("Height", int(h as i64)), ("ColorSpace", name(cs)), ("BitsPerComponent", int(8))]);
    if let Some((w, h, comps)) = jpeg_info(&data.bytes) {
        let n = o.reserve();
        let mut d = head(w, h, if comps == 1 { "DeviceGray" } else { "DeviceRGB" });
        d.insert("Filter", name("DCTDecode"));
        o.stream(n, d, &data.bytes, false)?;
        return Ok(Some(n));
    }
    let Some((w, h, rgba)) = decode(&data.bytes) else { return Ok(None) };
    let px = Px { rgba };
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
    let mut d = head(w, h, "DeviceRGB");
    if alpha.iter().any(|&a| a != 255) {
        let m = o.reserve();
        o.stream(m, head(w, h, "DeviceGray"), &alpha, true)?;
        d.insert("SMask", reference(m));
    }
    let n = o.reserve();
    o.stream(n, d, &rgb, true)?;
    Ok(Some(n))
}

/// One block of a [`plain`] document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plain<'a> {
    /// A heading (the Heading 2 style).
    Heading(&'a str),
    /// Text, a paragraph for each line.
    Text(&'a str),
}

/// The most characters of text [`plain`] lays out; the rest is cut.
pub const PLAIN_MAX: usize = 4 << 20;

/// Text that has no page layout of its own (a message, a reader page, a
/// terminal) as a PDF on paper `w_pt` × `h_pt` points with 2 cm margins:
/// what an app hands the print sheet (printing.md §5.1, `print_text`).
pub fn plain(title: &str, blocks: &[Plain<'_>], w_pt: u32, h_pt: u32) -> io::Result<Vec<u8>> {
    let mut d = TextDoc::new();
    let heading = d.styles.by_name("Heading 2").unwrap_or(0);
    let mut left = PLAIN_MAX;
    for b in blocks {
        let (text, style) = match *b {
            Plain::Heading(t) => (t, heading),
            Plain::Text(t) => (t, 0),
        };
        for line in text.split('\n') {
            if left == 0 {
                break;
            }
            let line: String = line.trim_end_matches('\r').chars().filter(|c| !c.is_control() || *c == '\t').take(left).collect();
            left = left.saturating_sub(line.chars().count() + 1);
            d.push_para(&line, style, Vec::new());
        }
    }
    if d.blocks.len() > 1 {
        d.blocks.remove(0);
    }
    // Points to twips, kept to a sane sheet.
    let tw = |pt: u32| (pt.clamp(144, 14400) * 20) as i32;
    let margin = 1134;
    d.page = officina_text::model::PageSetup { width: tw(w_pt), height: tw(h_pt), margin_top: margin, margin_bottom: margin, margin_start: margin, margin_end: margin };
    let fonts = PinnedFonts::new().map_err(io::Error::other)?;
    export(&d, &fonts, &fonts, &Info { title: title.into() }, Vec::new(), &mut |_, _| {}).map(|(b, _)| b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_allowed() {
        assert!(uri_ok("https://example.org/a?b=c"));
        assert!(!uri_ok("javascript:alert(1)"));
        assert!(!uri_ok("file:///etc/passwd"));
    }

    /// Library faces (a stand-in and a script's Noto face) embed like the
    /// pinned ones.
    #[test]
    fn library_faces_embed() {
        use officina_text::model::CharAttrs;
        let fonts = PinnedFonts::new().unwrap();
        let mut d = TextDoc::new();
        let a = d.attrs.intern(CharAttrs { family: Some("Calibri".into()), ..Default::default() });
        d.blocks.update(0, |b| {
            let p = b.as_para_mut().unwrap();
            p.text = "Calibri text, नमस्ते, வணக்கம்".to_string();
            p.runs = vec![(0, a)];
        });
        let (pdf, stats) = export(&d, &fonts, &fonts, &Info::default(), Vec::new(), &mut |_, _| {}).unwrap();
        assert_eq!(stats.pages, 1);
        assert_eq!(stats.fonts, 3, "Carlito, Noto Sans Devanagari, Noto Sans Tamil");
        assert!(pdf.len() > 1000);
    }

    /// Emoji: the colour face's text written invisibly (still copies), each
    /// glyph once as a picture with a soft mask.
    #[test]
    fn emoji_are_pictures_over_invisible_text() {
        let fonts = PinnedFonts::new().unwrap();
        let mut d = TextDoc::new();
        d.blocks.update(0, |b| b.as_para_mut().unwrap().text = "Hi 😀🚀😀".to_string());
        let (pdf, stats) = export(&d, &fonts, &fonts, &Info::default(), Vec::new(), &mut |_, _| {}).unwrap();
        assert_eq!(stats.fonts, 2, "IBM Plex Sans, Noto Color Emoji");
        let text = String::from_utf8_lossy(&pdf);
        assert!(text.contains("/Em"), "the page names the pictures");
        assert_eq!(text.matches("/SMask").count(), 2, "two emoji pictures, one each");
    }
    #[test]
    fn plain_text_is_paged_on_the_paper_asked_for() {
        let body = "Hello,\n\nthe minutes are attached.\n".repeat(80);
        let pdf = plain("Minutes", &[Plain::Heading("Minutes"), Plain::Text("From: Ana\nTo: Ben"), Plain::Text(&body)], 612, 792).unwrap();
        let text = String::from_utf8_lossy(&pdf);
        assert!(text.starts_with("%PDF"));
        assert!(text.contains("612 792"), "US Letter pages");
        let n: usize = text.split("/Count").nth(1).and_then(|r| r.trim_start().split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()).unwrap_or(0);
        assert!(n >= 2, "240 lines take more than a page ({n})");
    }
}
