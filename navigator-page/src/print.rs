//! The page printed (navigator-ui.md "Printing"): the document laid out
//! again at the paper's width, cut into pages between lines, and painted
//! into a PDF through Officina's writer, here in the page's jail. The
//! parent gets the PDF's bytes and hands them to the print sheet, which
//! never parses them itself (printing.md §5.2).
//!
//! What a PDF page can't hold the screen's way is simplified: shadows are
//! left out, a gradient is its middle colour, corners are square, an
//! opacity group paints opaque, and a transformed node keeps only its
//! move. Video slots print their placeholder.

use std::collections::BTreeMap;

use navigator_render::fontset::FontSet;
use navigator_render::source::{ContentSource, DocumentSource, Event, GroupId, SceneSink, Want};
use navigator_render::{Scene, Subresources, U};
use officina_pdf::{FontFiles, Painted};
use officina_text::paint::{GlyphRun, Item, PaintGlyph};

/// Paper and margins, in points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Paper {
    pub w_pt: f64,
    pub h_pt: f64,
    pub margin_pt: f64,
}

impl Paper {
    /// The margin the print sheet's paper sizes get: 1.5 cm.
    pub const MARGIN_PT: f64 = 42.5;
    pub fn new(w_pt: u32, h_pt: u32) -> Self {
        Paper { w_pt: w_pt as f64, h_pt: h_pt as f64, margin_pt: Self::MARGIN_PT }
    }
}

/// The most pages one page prints to; the rest is left out.
pub const PAGES_MAX: usize = 500;
/// Bands laid out per round, and the most rounds.
const ROUND_BANDS: usize = 16;
const ROUNDS_MAX: usize = 4096;

/// What was printed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Printed {
    pub pdf: Vec<u8>,
    pub pages: usize,
    /// More than [`PAGES_MAX`] pages: the rest was left out.
    pub cut: bool,
}

#[derive(Default)]
struct Collect {
    groups: BTreeMap<GroupId, (Option<(U, U)>, Scene)>,
}

impl SceneSink for Collect {
    fn put(&mut self, group: GroupId, clip: Option<(U, U)>, scene: &Scene) {
        self.groups.insert(group, (clip, scene.clone()));
    }
    fn clear(&mut self, group: GroupId) {
        self.groups.remove(&group);
    }
}

struct Faces<'a>(&'a FontSet);

impl FontFiles for Faces<'_> {
    fn face(&self, face: u16) -> Option<&[u8]> {
        self.0.faces.get(face as usize).map(|f| f.bytes.as_slice())
    }
}

/// 1/64 px to layout units (1/64 pt): a CSS px is 3/4 pt.
fn lu(v: U) -> i64 {
    v * 3 / 4
}

/// `html` (normalized, with its `subs`) printed on `paper`. `picture`
/// gives an image's bytes by content address.
pub fn pdf(html: String, subs: Subresources, title: &str, paper: Paper, picture: &dyn Fn(&str) -> Option<Vec<u8>>) -> Result<Printed, String> {
    write_pdf(html, subs, title, paper, picture, false)
}

/// Write only passive paint items. The caller supplies freshly encoded pixels.
pub fn sanitized_pdf(html: String, subs: Subresources, picture: &dyn Fn(&str) -> Option<Vec<u8>>) -> Result<Printed, String> {
    write_pdf(html, subs, "", Paper::new(595, 842), picture, true)
}

struct PdfBytes {
    bytes: Vec<u8>,
    limit: usize,
}

impl std::io::Write for PdfBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit - self.bytes.len() {
            return Err(std::io::Error::other("PDF exceeds size limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
}

fn write_pdf(html: String, subs: Subresources, title: &str, paper: Paper, picture: &dyn Fn(&str) -> Option<Vec<u8>>, passive: bool) -> Result<Printed, String> {
    let fonts = FontSet::shared()?;
    let (pw, ph, m) = (paper.w_pt, paper.h_pt, paper.margin_pt);
    if !(pw > 2.0 * m + 72.0 && ph > 2.0 * m + 72.0 && pw < 14400.0 && ph < 14400.0) {
        return Err("that paper is too small or too large".into());
    }
    // The printable area in CSS px, and as layout lengths.
    let (w_px, h_px) = ((pw - 2.0 * m) / 0.75, (ph - 2.0 * m) / 0.75);
    let page_h: U = (h_px * 64.0) as U;
    let mut src = DocumentSource::new(html, fonts, subs, (w_px, h_px), page_h);
    let mut sink = Collect::default();
    let (mut height, mut settled) = (0, false);
    let take = |ev: Vec<Event>, height: &mut U, settled: &mut bool| {
        for e in ev {
            if let Event::Extent { height: h, settled: s } = e {
                *height = h;
                *settled = s;
            }
        }
    };
    for _ in 0..ROUNDS_MAX {
        let ev = src.idle(ROUND_BANDS, &mut sink);
        take(ev, &mut height, &mut settled);
        if settled {
            break;
        }
    }
    // The whole of it, resident at once (laying it out may move the end).
    let cap = page_h * PAGES_MAX as U;
    for _ in 0..8 {
        let before = height;
        let ev = src.want(Want { range: (0, height.min(cap).max(1)), zoom_64: 64, prefetch: 0 }, &mut sink);
        take(ev, &mut height, &mut settled);
        if height == before {
            break;
        }
    }
    let height = height.max(1);

    // Every node once, in document order: a band's own nodes are those
    // that start in it; the overlay's are all its own.
    let mut addresses: Vec<String> = Vec::new();
    let mut items: Vec<(U, U, Item)> = Vec::new();
    let mut lines: Vec<(U, U)> = Vec::new();
    for (g, (clip, scene)) in &sink.groups {
        let own = |top: U| match (g, clip) {
            (GroupId::Band(_), Some((a, b))) => top >= *a && top < *b,
            _ => true,
        };
        paint(scene, &own, &mut items, &mut lines, &mut addresses);
    }

    // Pages: each ends at the lowest point in its lower half no line
    // straddles, else where the paper does.
    lines.sort();
    let mut breaks = vec![0];
    while *breaks.last().unwrap() < height && breaks.len() <= PAGES_MAX {
        let y0 = *breaks.last().unwrap();
        let mut y = (y0 + page_h).min(height);
        if y < height {
            let floor = y0 + page_h / 2;
            while let Some(&(a, _)) = lines.iter().find(|(a, b)| *a < y && *b > y) {
                y = a;
                if y <= floor {
                    y = y0 + page_h;
                    break;
                }
            }
        }
        breaks.push(y);
    }
    let cut = !settled || *breaks.last().unwrap() < height;
    if passive && cut { return Err("document exceeds layout limits".into()); }
    if passive {
        items.retain(|(_, _, item)| matches!(item, Item::Glyphs(_) | Item::Rect { .. } | Item::Path { .. } | Item::Image { .. }));
    }
    breaks.truncate(PAGES_MAX + 1);

    let files = Faces(fonts);
    let pic = |i: u32| addresses.get(i as usize).and_then(|a| picture(a));
    let limit = if passive { limen::doc_viewer::DOC_MAX as usize } else { usize::MAX };
    let mut out = Painted::new(PdfBytes { bytes: Vec::new(), limit }).map_err(|e| e.to_string())?;
    let (mx, my) = ((m * 64.0) as i64, (m * 64.0) as i64);
    for w in breaks.windows(2) {
        let (y0, y1) = (w[0], w[1]);
        let page: Vec<Item> = items.iter()
            .filter(|(t, b, _)| *b > y0 && *t < y1)
            .map(|(_, _, it)| moved(it, mx, my - lu(y0)))
            .collect();
        let clip = [mx, my, lu((w_px * 64.0) as U), lu(y1 - y0)];
        out.page(page, pw, ph, Some(clip), &files, &pic).map_err(|e| e.to_string())?;
    }
    let (pdf, stats) = out.finish(&files, title, "Atrium Navigator").map_err(|e| e.to_string())?;
    Ok(Printed { pdf: pdf.bytes, pages: stats.pages, cut })
}

/// An item moved by (`dx`, `dy`) layout units.
fn moved(it: &Item, dx: i64, dy: i64) -> Item {
    let mut it = it.clone();
    match &mut it {
        Item::Rect { x, y, .. } | Item::Image { x, y, .. } | Item::Link { x, y, .. } => {
            *x += dx;
            *y += dy;
        }
        Item::Glyphs(r) => {
            r.x += dx;
            r.y += dy;
        }
        Item::Path { cmds, .. } => {
            use officina_text::model::PathCmd;
            for c in cmds.iter_mut() {
                match c {
                    PathCmd::Move(x, y) | PathCmd::Line(x, y) => {
                        *x += dx;
                        *y += dy;
                    }
                    PathCmd::Cubic(a, b, c2, d, e, f) => {
                        *a += dx;
                        *b += dy;
                        *c2 += dx;
                        *d += dy;
                        *e += dx;
                        *f += dy;
                    }
                    PathCmd::Close => {}
                }
            }
        }
        _ => {}
    }
    it
}

/// `scene`'s nodes as items in document layout units (with their top
/// and bottom in 1/64 px, for paging); text lines into `lines`.
fn paint(scene: &Scene, own: &dyn Fn(U) -> bool, items: &mut Vec<(U, U, Item)>, lines: &mut Vec<(U, U)>, addresses: &mut Vec<String>) {
    let shift = |attrs: &navigator_render::Attrs| -> (U, U) {
        match attrs.2.and_then(|x| scene.xforms.get(x as usize)) {
            Some(m) => (m[4], m[5]),
            None => (0, 0),
        }
    };
    let clipped_out = |attrs: &navigator_render::Attrs| -> bool {
        matches!(attrs.0.and_then(|c| scene.clips.get(c as usize)), Some(&(_, _, w, h)) if w <= 0 || h <= 0)
    };
    let rgb = |rgba: u32| rgba >> 8;
    let seen = |rgba: u32| rgba & 0xff >= 0x10;
    for &(kind, i) in &scene.order {
        match kind {
            0 => {
                let (Some(r), Some(a)) = (scene.rects.get(i), scene.rect_attrs.get(i)) else { continue };
                if !seen(r.rgba) || clipped_out(a) {
                    continue;
                }
                let (sx, sy) = shift(a);
                let (x, y) = (r.x + sx, r.y + sy);
                if !own(y) {
                    continue;
                }
                let c = rgb(r.rgba);
                let mut push = |x: U, y: U, w: U, h: U| {
                    if w > 0 && h > 0 {
                        items.push((y, y + h, Item::Rect { x: lu(x), y: lu(y), w: lu(w), h: lu(h), color: c }));
                    }
                };
                if r.ring > 0 && 2 * r.ring < r.w.min(r.h) {
                    let k = r.ring;
                    push(x, y, r.w, k);
                    push(x, y + r.h - k, r.w, k);
                    push(x, y + k, k, r.h - 2 * k);
                    push(x + r.w - k, y + k, k, r.h - 2 * k);
                } else {
                    push(x, y, r.w, r.h);
                }
            }
            1 => {
                let (Some(r), Some(a)) = (scene.runs.get(i), scene.run_attrs.get(i)) else { continue };
                if !seen(r.rgba) || clipped_out(a) || r.glyphs.is_empty() {
                    continue;
                }
                let (sx, sy) = shift(a);
                let (x, y) = (r.x + sx, r.y + sy);
                let (top, bottom) = (y - r.size, y + r.size / 3);
                if !own(top) {
                    continue;
                }
                lines.push((top, bottom));
                let glyphs = r.glyphs.iter().enumerate().map(|(k, &(id, dx, dy))| PaintGlyph {
                    id,
                    x: lu(dx),
                    dy: lu(dy),
                    text: r.clusters[k].clone(),
                }).collect();
                let run = GlyphRun { face: r.face as u16, size: lu(r.size), color: rgb(r.rgba), x: lu(x), y: lu(y), glyphs, text: r.text.clone(), turn: None };
                items.push((top, bottom, Item::Glyphs(run)));
            }
            2 => {
                let (Some(l), Some(a)) = (scene.links.get(i), scene.link_attrs.get(i)) else { continue };
                let (sx, sy) = shift(a);
                let (x, y) = (l.x + sx, l.y + sy);
                if !own(y) || !(l.href.starts_with("https://") || l.href.starts_with("http://") || l.href.starts_with("mailto:")) {
                    continue;
                }
                items.push((y, y + l.h, Item::Link { x: lu(x), y: lu(y), w: lu(l.w), h: lu(l.h), uri: l.href.clone() }));
            }
            4 => {
                let (Some(g), Some(a)) = (scene.grads.get(i), scene.grad_attrs.get(i)) else { continue };
                let Some(&(c, _)) = g.stops.get(g.stops.len() / 2) else { continue };
                if !seen(c) || clipped_out(a) {
                    continue;
                }
                let (sx, sy) = shift(a);
                let t = &g.area;
                let (x, y) = (t.x + sx, t.y + sy);
                if !own(y) || t.w <= 0 || t.h <= 0 {
                    continue;
                }
                items.push((y, y + t.h, Item::Rect { x: lu(x), y: lu(y), w: lu(t.w), h: lu(t.h), color: rgb(c) }));
            }
            5 => {
                let (Some(im), Some(a)) = (scene.images.get(i), scene.image_attrs.get(i)) else { continue };
                if clipped_out(a) {
                    continue;
                }
                let (sx, sy) = shift(a);
                let t = &im.area;
                // Not repeated: the one tile, where it sits; repeated: the
                // area (one copy, stretched).
                let (x, y, w, h) = if t.repeat == 0 { (t.tx, t.ty, t.tw, t.th) } else { (t.x, t.y, t.w, t.h) };
                let (x, y) = (x + sx, y + sy);
                if !own(y) || w <= 0 || h <= 0 {
                    continue;
                }
                let id = match addresses.iter().position(|a| *a == im.address) {
                    Some(k) => k,
                    None => {
                        addresses.push(im.address.clone());
                        addresses.len() - 1
                    }
                };
                items.push((y, y + h, Item::Image { image: id as u32, x: lu(x), y: lu(y), w: lu(w), h: lu(h) }));
            }
            6 => {
                let (Some(p), Some(a)) = (scene.paths.get(i), scene.path_attrs.get(i)) else { continue };
                if clipped_out(a) {
                    continue;
                }
                if let Some((top, bottom, it)) = path_item(p) {
                    if own(top) {
                        items.push((top, bottom, it));
                    }
                }
            }
            _ => {}
        }
    }
}

/// A vector shape (an inline SVG's path) as a filled outline; a gradient
/// paints its first colour.
fn path_item(p: &navigator_render::path::Path) -> Option<(U, U, Item)> {
    use navigator_render::path::Paint;
    use officina_text::model::PathCmd;
    let color = match &p.paint {
        Paint::Solid(c) => *c,
        Paint::Linear { stops, .. } | Paint::Radial { stops, .. } => stops.first()?.0,
    };
    if color & 0xff < 0x10 || p.opacity < 0x10 {
        return None;
    }
    let [a, b, c, d, e, f] = p.transform;
    // Path units (1/64) to the page (1/64 px), then layout units.
    let at = |(x, y): (i64, i64)| -> (i64, i64) {
        let px = ((a as i128 * x as i128 + c as i128 * y as i128) / 65536) as i64 / 64 + e;
        let py = ((b as i128 * x as i128 + d as i128 * y as i128) / 65536) as i64 / 64 + f;
        (px, py)
    };
    let mut cmds = Vec::new();
    let (mut top, mut bottom) = (i64::MAX, i64::MIN);
    let mut pts = p.points.iter().copied();
    let mut last = (0, 0);
    let mut put = |q: (i64, i64)| {
        top = top.min(q.1);
        bottom = bottom.max(q.1);
        (lu(q.0), lu(q.1))
    };
    for &v in &p.verbs {
        match v {
            0 => {
                let q = at(pts.next()?);
                last = q;
                let (x, y) = put(q);
                cmds.push(PathCmd::Move(x, y));
            }
            1 => {
                let q = at(pts.next()?);
                last = q;
                let (x, y) = put(q);
                cmds.push(PathCmd::Line(x, y));
            }
            2 => {
                let (c1, e1) = (at(pts.next()?), at(pts.next()?));
                // The quadratic as a cubic.
                let k1 = (last.0 + 2 * (c1.0 - last.0) / 3, last.1 + 2 * (c1.1 - last.1) / 3);
                let k2 = (e1.0 + 2 * (c1.0 - e1.0) / 3, e1.1 + 2 * (c1.1 - e1.1) / 3);
                last = e1;
                let (k1, k2, e1) = (put(k1), put(k2), put(e1));
                cmds.push(PathCmd::Cubic(k1.0, k1.1, k2.0, k2.1, e1.0, e1.1));
            }
            3 => {
                let (k1, k2, e1) = (at(pts.next()?), at(pts.next()?), at(pts.next()?));
                last = e1;
                let (k1, k2, e1) = (put(k1), put(k2), put(e1));
                cmds.push(PathCmd::Cubic(k1.0, k1.1, k2.0, k2.1, e1.0, e1.1));
            }
            4 => cmds.push(PathCmd::Close),
            _ => return None,
        }
    }
    (!cmds.is_empty() && top <= bottom).then(|| (top, bottom, Item::Path { cmds, fill: Some(color >> 8), line: None }))
}
