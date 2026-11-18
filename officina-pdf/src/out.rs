//! The file every export writes through: pages of content operators, each
//! face embedded once as a subset of the glyphs used, each picture once,
//! colour glyphs as pictures. Write's pages, a sheet's and a deck's slides
//! all go through here, so text, pictures and emoji look alike in each.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{self, Write};

use folium_content::ops::{write_ops, Op, TjPart};
use folium_syntax::write::{array, dict, int, name, nums, real, reference, text_string, Writer};
use folium_syntax::{Dict, Object, Ref};
use officina_text::paint::Item;

use crate::{font, jpeg_info, pt, pt_f, rgb, uri_ok, FontFiles, Stats, EMOJI_PX};

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

/// A face used in the document.
struct FontUse {
    obj: Ref,
    glyphs: BTreeMap<u16, String>,
}

/// One page being written: its operators and what they name.
#[derive(Default)]
pub(crate) struct Page {
    pub c: Vec<Op>,
    annots: Vec<Object>,
    fonts: BTreeSet<u16>,
    xobjects: BTreeMap<String, Ref>,
    gstates: BTreeMap<String, Object>,
}

impl Page {
    /// The graphics state for fill and stroke opacity (0–255); None when
    /// both are opaque.
    pub fn alpha(&mut self, fill: u8, stroke: u8) -> Option<Op> {
        if fill == 255 && stroke == 255 {
            return None;
        }
        let n = format!("A{fill}_{stroke}");
        self.gstates.entry(n.clone()).or_insert_with(|| Object::Dict(dict([("Type", name("ExtGState")), ("ca", real(fill as f64 / 255.0)), ("CA", real(stroke as f64 / 255.0))])));
        Some(Op::GState(n))
    }

    /// A link over `x0, y0, x1, y1` (page points); only http, https and mailto.
    pub fn link(&mut self, r: [f64; 4], uri: &str) {
        if uri_ok(uri) {
            self.annots.push(Object::Dict(dict([
                ("Type", name("Annot")),
                ("Subtype", name("Link")),
                ("Rect", nums(&r)),
                ("Border", array([int(0), int(0), int(0)])),
                ("A", Object::Dict(dict([("S", name("URI")), ("URI", Object::String(uri.as_bytes().into()))]))),
            ])));
        }
    }
}

/// The file being written.
pub(crate) struct Pdf<W: Write> {
    o: Writer<W>,
    catalog: Ref,
    tree: Ref,
    info: Ref,
    kids: Vec<Ref>,
    fonts_used: BTreeMap<u16, FontUse>,
    pictures: HashMap<u64, Option<Ref>>,
    color_faces: HashMap<u16, bool>,
    emoji: HashMap<(u16, u16), Option<Emoji>>,
    pub stats: Stats,
}

impl<W: Write> Pdf<W> {
    pub fn new(out: W) -> io::Result<Self> {
        let mut o = Writer::new(out, "1.7")?;
        let (catalog, tree, info) = (o.reserve(), o.reserve(), o.reserve());
        Ok(Self { o, catalog, tree, info, kids: Vec::new(), fonts_used: BTreeMap::new(), pictures: HashMap::new(), color_faces: HashMap::new(), emoji: HashMap::new(), stats: Stats::default() })
    }

    /// Picture `key` (written once, from `bytes` the first time) named in
    /// `page`'s resources; None when it can't be read.
    pub fn picture(&mut self, page: &mut Page, key: u64, bytes: impl FnOnce() -> Option<Vec<u8>>) -> io::Result<Option<String>> {
        let obj = match self.pictures.get(&key) {
            Some(o) => *o,
            None => {
                let n = match bytes() {
                    Some(b) => write_picture(&mut self.o, &b)?,
                    None => None,
                };
                self.pictures.insert(key, n);
                n
            }
        };
        Ok(obj.map(|obj| {
            let n = format!("Im{key}");
            page.xobjects.insert(n.clone(), obj);
            n
        }))
    }

    /// Officina's painted items onto `page`, in layout units with y down
    /// from the top of a page `ph` points high. `doc_image` gives a
    /// document picture's bytes.
    pub fn items(&mut self, page: &mut Page, items: impl IntoIterator<Item = Item>, ph: f64, files: &dyn FontFiles, doc_image: &dyn Fn(u32) -> Option<Vec<u8>>) -> io::Result<()> {
        for it in items {
            match it {
                Item::Rect { x, y, w, h, color } => {
                    page.c.extend([Op::FillRgb(rgb(color)), Op::Rect(pt(x), ph - pt(y + h), pt(w), pt(h)), Op::Fill]);
                }
                Item::Image { image, x, y, w, h } => match self.picture(page, image as u64, || doc_image(image))? {
                    Some(n) => page.c.extend([Op::Save, Op::Transform([pt(w), 0.0, 0.0, pt(h), pt(x), ph - pt(y + h)]), Op::XObject(n), Op::Restore]),
                    None => page.c.extend([Op::FillRgb([0.85, 0.86, 0.88]), Op::Rect(pt(x), ph - pt(y + h), pt(w), pt(h)), Op::Fill]),
                },
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
                        page.c.push(Op::FillRgb(rgb(f)));
                        page.c.extend(path.iter().cloned());
                        page.c.push(Op::Fill);
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
                        page.c.extend([Op::Save, Op::StrokeRgb(rgb(color)), Op::LineWidth(w), Op::Dash(pattern, 0.0)]);
                        page.c.extend(path);
                        page.c.extend([Op::Stroke, Op::Restore]);
                    }
                }
                Item::Link { x, y, w, h, uri } => page.link([pt(x), ph - pt(y + h), pt(x + w), ph - pt(y)], &uri),
                Item::Glyphs(r) => {
                    let Some(bytes) = files.face(r.face) else { continue };
                    let Some(face) = font::Face::parse(bytes) else { continue };
                    let upem = face.info.upem as f64;
                    let o = &mut self.o;
                    let fu = self.fonts_used.entry(r.face).or_insert_with(|| FontUse { obj: o.reserve(), glyphs: BTreeMap::new() });
                    page.fonts.insert(r.face);
                    let size = pt(r.size);
                    if size <= 0.0 {
                        continue;
                    }
                    let color = *self.color_faces.entry(r.face).or_insert_with(|| atrium_colr::is_color_font(bytes));
                    let c = &mut page.c;
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
                            let e = match self.emoji.get(&(r.face, g.id)) {
                                Some(e) => *e,
                                None => {
                                    let e = write_emoji(&mut self.o, bytes, g.id)?;
                                    self.emoji.insert((r.face, g.id), e);
                                    e
                                }
                            };
                            let Some(e) = e else { continue };
                            let n = format!("Em{}_{}", r.face, g.id);
                            page.xobjects.insert(n.clone(), e.obj);
                            let gx = g.x as f64;
                            let (sx, sy) = (r.x as f64 + ta * gx, r.y as f64 + tb * gx);
                            page.c.extend([
                                Op::Save,
                                Op::Transform([ta, -tb, -tc, td, pt_f(sx), ph - pt_f(sy)]),
                                Op::Transform([e.w as f64 * px, 0.0, 0.0, e.h as f64 * px, e.left as f64 * px, (e.top as f64 - e.h as f64) * px - pt(g.dy)]),
                                Op::XObject(n),
                                Op::Restore,
                            ]);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Writes `page`, `pw` × `ph` points.
    pub fn end_page(&mut self, page: Page, pw: f64, ph: f64) -> io::Result<()> {
        let content = self.o.reserve();
        self.o.stream(content, Dict::new(), &write_ops(&page.c), true)?;
        let obj = self.o.reserve();
        let mut res = Dict::new();
        if !page.fonts.is_empty() {
            let mut f = Dict::new();
            for face in &page.fonts {
                f.insert(&format!("F{face}"), reference(self.fonts_used[face].obj));
            }
            res.insert("Font", Object::Dict(f));
        }
        let named = |m: BTreeMap<String, Object>| {
            let mut d = Dict::new();
            for (k, v) in m {
                d.insert(&k, v);
            }
            Object::Dict(d)
        };
        if !page.xobjects.is_empty() {
            res.insert("XObject", named(page.xobjects.into_iter().map(|(k, r)| (k, reference(r))).collect()));
        }
        if !page.gstates.is_empty() {
            res.insert("ExtGState", named(page.gstates));
        }
        self.stats.links += page.annots.len();
        let mut p = dict([
            ("Type", name("Page")),
            ("Parent", reference(self.tree)),
            ("MediaBox", nums(&[0.0, 0.0, pw, ph])),
            ("Resources", Object::Dict(res)),
            ("Contents", reference(content)),
        ]);
        if !page.annots.is_empty() {
            p.insert("Annots", Object::Array(page.annots));
        }
        self.o.obj(obj, &Object::Dict(p))?;
        self.kids.push(obj);
        self.stats.pages += 1;
        Ok(())
    }

    /// The fonts (now that every glyph is known), the page tree and the
    /// document information; `creator` is the app.
    pub fn finish(mut self, files: &dyn FontFiles, title: &str, creator: &str) -> io::Result<(W, Stats)> {
        let o = &mut self.o;
        for (&face, fu) in &self.fonts_used {
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
        self.stats.fonts = self.fonts_used.len();
        self.stats.images = self.pictures.values().filter(|v| v.is_some()).count();

        o.obj(self.tree, &Object::Dict(dict([("Type", name("Pages")), ("Kids", array(self.kids.iter().map(|k| reference(*k)))), ("Count", int(self.kids.len() as i64))])))?;
        o.obj(self.catalog, &Object::Dict(dict([("Type", name("Catalog")), ("Pages", reference(self.tree))])))?;
        let mut inf = dict([("Producer", Object::String(b"Officina"[..].into())), ("Creator", Object::String(creator.as_bytes().into()))]);
        if !title.is_empty() {
            inf.insert("Title", text_string(title));
        }
        o.obj(self.info, &Object::Dict(inf))?;
        let (w, bytes) = self.o.finish(self.catalog, Some(self.info))?;
        self.stats.bytes = bytes;
        Ok((w, self.stats))
    }
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

/// A picture's bytes as an image XObject (JPEG as it is, anything else
/// decoded and deflated); None when they can't be read.
fn write_picture<W: Write>(o: &mut Writer<W>, bytes: &[u8]) -> io::Result<Option<Ref>> {
    let head = |w: u32, h: u32, cs: &str| dict([("Type", name("XObject")), ("Subtype", name("Image")), ("Width", int(w as i64)), ("Height", int(h as i64)), ("ColorSpace", name(cs)), ("BitsPerComponent", int(8))]);
    if let Some((w, h, comps)) = jpeg_info(bytes) {
        let n = o.reserve();
        let mut d = head(w, h, if comps == 1 { "DeviceGray" } else { "DeviceRGB" });
        d.insert("Filter", name("DCTDecode"));
        o.stream(n, d, bytes, false)?;
        return Ok(Some(n));
    }
    let Some((w, h, rgba)) = decode(bytes) else { return Ok(None) };
    let count = (w as usize) * (h as usize);
    if rgba.len() < count * 4 || count == 0 {
        return Ok(None);
    }
    let mut rgb = Vec::with_capacity(count * 3);
    let mut alpha = Vec::with_capacity(count);
    for p in rgba.chunks_exact(4).take(count) {
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
