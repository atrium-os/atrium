//! What a page shows, as a list of simple items in layout units from the
//! page's top-left corner: glyph runs, rectangles, pictures and link areas.
//! The screen view and PDF export both draw from it, so a page looks the same
//! in both (docs/spec/officina.md §7.4, O3f).

use std::ops::Range;

use crate::layout::{object_height, size_lu, vert_shift, Fonts, LayoutCache, ParaLayout, OBJECT_FACE};
use crate::lists::Numbering;
use crate::model::{Wrap, Align, CharFormat, Leader, TextDoc, HEADER_FROM_EDGE, OBJECT_CHAR};
use crate::paginate::{Page, NOTE_HANG, NOTE_RULE_SPACE};
use crate::twips;

/// One glyph of a run.
#[derive(Debug, Clone, PartialEq)]
pub struct PaintGlyph {
    pub id: u16,
    /// From the run's origin, offsets included.
    pub x: i64,
    pub dy: i64,
    /// The text this glyph shows, in [`GlyphRun::text`] (empty for the second
    /// and later glyphs of a cluster).
    pub text: Range<u32>,
}

/// Glyphs of one face, size and colour on one baseline.
#[derive(Debug, Clone, PartialEq)]
pub struct GlyphRun {
    pub face: u16,
    /// Font size, layout units.
    pub size: i64,
    pub color: u32,
    /// The origin: the first glyph's pen position on the baseline.
    pub x: i64,
    pub y: i64,
    pub glyphs: Vec<PaintGlyph>,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// A filled rectangle (highlights, underlines, cell rules).
    Rect { x: i64, y: i64, w: i64, h: i64, color: u32 },
    Glyphs(GlyphRun),
    /// A picture from the document's images, its top-left corner and size.
    Image { image: u32, x: i64, y: i64, w: i64, h: i64 },
    /// An area that is a link.
    Link { x: i64, y: i64, w: i64, h: i64, uri: String },
    /// Text under comments (the last one listed is the innermost): the screen
    /// marks it, a PDF leaves it out. Comes before the text it covers.
    Mark { x: i64, y: i64, w: i64, h: i64, comments: std::sync::Arc<[crate::model::CommentId]> },
    /// Tracked change over text (an insertion or formatting change) or at a
    /// point (a tombstone, `w` 0): bytes `start..end` of paragraph `block`. The
    /// screen draws a bar in the margin, and in review the edit it belongs
    /// to; a PDF leaves it out.
    Change { x: i64, y: i64, w: i64, h: i64, block: usize, start: u32, end: u32, rev: crate::model::RevId },
}

/// A table rule's thickness: half a point.
pub const RULE: i64 = crate::LU / 2;
/// The colour of table rules.
pub const RULE_COLOR: u32 = 0x8c9199;
/// Header and footer text.
pub const HEADER_FORMAT_SIZE: u16 = 18;
pub const HEADER_COLOR: u32 = 0x555555;

/// The items of page `i` of `pages`; `pages_text` is what `{pages}` reads.
#[allow(clippy::too_many_arguments)]
pub fn paint_page(doc: &TextDoc, pages: &[Page], i: usize, col_w: i64, cache: &mut LayoutCache, fonts: &dyn Fonts, numbering: &Numbering, pages_text: &str) -> Vec<Item> {
    let mut out = Vec::new();
    let Some(page) = pages.get(i) else { return out };
    // The page's own size and margins (its section's).
    let g = &page.setup;
    let col_w = if *g == doc.page { col_w } else { crate::paginate::content_width(g) };
    let (mx, my) = (twips(g.margin_start as i64), twips(g.margin_top as i64));
    let (pw, ph) = (twips(g.width as i64), twips(g.height as i64));

    // Header and footer, in the margins.
    let (head, foot) = doc.margins_of(i);
    for (hf, top) in [(head, true), (foot, false)] {
        let Some(hf) = hf else { continue };
        let from = twips(HEADER_FROM_EDGE as i64);
        let width = pw - mx - twips(g.margin_end as i64);
        if !hf.paras.is_empty() {
            // Its own paragraphs: the header down from its line, the
            // footer up to its.
            let paras: Vec<_> = hf.paras.iter().map(|p| crate::model::resolve_para(p, i + 1, pages_text)).collect();
            let lays: Vec<_> = paras.iter().map(|p| cache.get(doc, p, width.max(crate::LU), fonts)).collect();
            let total: i64 = lays.iter().map(|l| l.height).sum();
            let mut y = if top { from } else { ph - from - total };
            for (p, l) in paras.iter().zip(&lays) {
                for li in 0..l.lines.len() {
                    line(&mut out, doc, fonts, l, li, &p.text, mx, y, None);
                    y += l.lines[li].height;
                }
            }
            continue;
        }
        let text = hf.resolve(i + 1, pages_text);
        let fmt = CharFormat { size: HEADER_FORMAT_SIZE, color: HEADER_COLOR, ..Default::default() };
        let size = size_lu(&fmt);
        let baseline = if top { from + size * 4 / 5 } else { ph - from };
        text_line(&mut out, fonts, &text, &fmt, hf.align, mx, width, baseline);
    }

    // Table rules: each cell draws its top and left rule; the right and
    // bottom ones only where no cell continues, so shared edges come once.
    for c in &page.cells {
        let (x, y, w, h) = (mx + c.x, my + c.y, c.w, c.h);
        let right = !page.cells.iter().any(|d| d.y == c.y && d.x == c.x + c.w);
        let below = !page.cells.iter().any(|d| d.y == c.y + c.h && d.x < c.x + c.w && d.x + d.w > c.x);
        out.push(Item::Rect { x, y, w, h: RULE, color: RULE_COLOR });
        out.push(Item::Rect { x, y, w: RULE, h, color: RULE_COLOR });
        if right {
            out.push(Item::Rect { x: x + w, y, w: RULE, h: h + RULE, color: RULE_COLOR });
        }
        if below {
            out.push(Item::Rect { x, y: y + h, w: w + RULE, h: RULE, color: RULE_COLOR });
        }
    }

    // Floating pictures behind the text first; the rest go over it.
    let float = |out: &mut Vec<Item>, f: &crate::paginate::PlacedFloat| {
        out.push(Item::Image { image: f.object.image, x: mx + f.x, y: my + f.y, w: f.w, h: f.h });
    };
    for f in page.floats.iter().filter(|f| f.wrap() == Wrap::Behind) {
        float(&mut out, f);
    }

    for s in &page.slices {
        let Some(p) = doc.para(s.block) else { continue };
        // The width the slice was laid out in (its column's, a cell's inside),
        // round the floats it was laid out round.
        let w = s.w;
        let l = cache.get_wrapped(doc, p, w, fonts, s.wrap.as_deref().map_or(&[], |v| v.as_slice()));
        let left = mx + s.x;
        if s.lines.start == 0 {
            if let Some(label) = numbering.label(s.block) {
                list_label(&mut out, &l, &label, fonts, left, my + s.y);
            }
        }
        let mut ly = s.y;
        for li in s.lines.clone() {
            line(&mut out, doc, fonts, &l, li, &p.text, left, my + ly, Some(s.block));
            ly += l.lines[li].height;
        }
    }

    for f in page.floats.iter().filter(|f| f.wrap() != Wrap::Behind) {
        float(&mut out, f);
    }

    // Lines between columns.
    for &(x, top, bottom) in &page.rules {
        out.push(Item::Rect { x: mx + x - RULE / 2, y: my + top, w: RULE, h: (bottom - top).max(RULE), color: RULE_COLOR });
    }

    // Footnotes at the foot, endnotes after the text: a short rule above
    // each, then each note's label in its hang and its text.
    for y in [page.foot_rule, page.end_rule].into_iter().flatten() {
        out.push(Item::Rect { x: mx, y: my + y + NOTE_RULE_SPACE / 2 - RULE / 2, w: (col_w / 3).max(crate::LU), h: RULE, color: 0x000000 });
    }
    for s in &page.notes {
        let Some(note) = doc.notes.get(s.note) else { continue };
        let Some(p) = note.paras.get(s.para) else { continue };
        let l = cache.get(doc, p, (col_w - NOTE_HANG).max(crate::LU), fonts);
        let left = mx + NOTE_HANG;
        if s.para == 0 && s.lines.start == 0 && !l.lines.is_empty() {
            let mut f = l.glyphs.first().map_or(&l.formats[0], |g| &l.formats[g.fmt as usize]).clone();
            f.underline = false;
            f.strike = false;
            f.link = None;
            f.object = None;
            f.note = None;
            f.vert = crate::model::VertAlign::Super;
            let shift = vert_shift(&mut f);
            runs(&mut out, fonts, &doc.note_label(s.note), &f, mx, baseline_of(&l, 0, my + s.y) + shift);
        }
        let mut ly = s.y;
        for li in s.lines.clone() {
            if li >= l.lines.len() {
                break;
            }
            line(&mut out, doc, fonts, &l, li, &p.text, left, my + ly, None);
            ly += l.lines[li].height;
        }
    }
    out
}

/// The baseline of line `li` whose top is `top`.
fn baseline_of(l: &ParaLayout, li: usize, top: i64) -> i64 {
    let line = &l.lines[li];
    let natural = line.ascent + line.descent;
    top + line.skip + line.ascent + (line.height - line.skip - natural).max(0) / 2
}

/// The text a glyph shows: its cluster, up to the next cluster.
fn cluster_text(text: &str, start: u32, next: Option<u32>) -> &str {
    let s = start as usize;
    let e = next.map_or_else(|| text[s..].chars().next().map_or(s, |c| s + c.len_utf8()), |n| n as usize);
    text.get(s..e.max(s)).unwrap_or("")
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
fn line(out: &mut Vec<Item>, doc: &TextDoc, fonts: &dyn Fonts, l: &ParaLayout, li: usize, text: &str, left: i64, top: i64, block: Option<usize>) {
    let line = &l.lines[li];
    let baseline = baseline_of(l, li, top);
    // Room skipped for floats above the line isn't the line's.
    let top = top + line.skip;
    let lh = line.height - line.skip;
    let bytes = text.as_bytes();
    let gl = &l.glyphs[line.glyphs.clone()];
    let mut pen = line.x;
    let mut i = 0;
    while i < gl.len() {
        let (face, fmt) = (gl[i].face, gl[i].fmt);
        let f = &l.formats[fmt as usize];
        if face == OBJECT_FACE {
            let g = gl[i];
            if let (Some(t), Some(b)) = (f.deleted, block) {
                if let Some(d) = doc.deletions.get(t) {
                    out.push(Item::Change { x: left + pen, y: top, w: 0, h: lh, block: b, start: g.cluster, end: g.cluster + OBJECT_CHAR.len_utf8() as u32, rev: d.rev });
                }
            }
            if let Some(o) = f.object.as_ref().filter(|o| o.float.is_none()) {
                let h = object_height(o, g.adv);
                out.push(Item::Image { image: o.image, x: left + pen, y: baseline - h, w: g.adv, h });
                if let Some(u) = &f.link {
                    out.push(Item::Link { x: left + pen, y: baseline - h, w: g.adv, h, uri: u.to_string() });
                }
            }
            pen += g.adv;
            i += 1;
            continue;
        }
        let x0 = pen;
        let first = i;
        let mut run = GlyphRun { face, size: size_lu(f), color: f.color, x: left + x0, y: baseline, glyphs: Vec::new(), text: String::new() };
        let mut last_cluster = None;
        while i < gl.len() && gl[i].face == face && gl[i].fmt == fmt {
            let g = gl[i];
            let ch = text[g.cluster as usize..].chars().next().unwrap_or(' ');
            if !ch.is_control() && ch != '\u{2028}' && ch != '\t' {
                let range = if last_cluster == Some(g.cluster) {
                    let n = run.text.len() as u32;
                    n..n
                } else {
                    // The cluster runs to the next glyph's cluster in the line
                    // (or one char at the line's end).
                    let next = gl[i + 1..].iter().map(|n| n.cluster).find(|&c| c > g.cluster).or(Some(line.end)).filter(|&c| c > g.cluster && c <= line.end);
                    let t = cluster_text(text, g.cluster, next);
                    let n = run.text.len() as u32;
                    // A note anchor's glyphs show its label.
                    match f.note {
                        Some(id) if ch == OBJECT_CHAR => run.text.push_str(&doc.note_label(id)),
                        _ => match f.field {
                            Some(x) if ch == OBJECT_CHAR => run.text.push_str(&doc.field_text(x)),
                            _ => run.text.push_str(t),
                        },
                    }
                    n..run.text.len() as u32
                };
                run.glyphs.push(PaintGlyph { id: g.id, x: pen - x0 + g.dx, dy: g.dy, text: range });
            }
            last_cluster = Some(g.cluster);
            pen += g.adv;
            if line.space_extra != 0 && bytes.get(g.cluster as usize) == Some(&b' ') {
                pen += line.space_extra;
            }
            i += 1;
        }
        let (rx, rw) = (left + x0, pen - x0);
        let size = run.size;
        if let (Some(rev), Some(b)) = (f.rev.or(f.fmt.map(|m| m.rev)), block) {
            let start = gl.get(first).map_or(line.start, |g| g.cluster);
            let end = gl.get(i).map_or(line.end, |g| g.cluster).max(start);
            out.push(Item::Change { x: rx, y: top, w: rw, h: lh, block: b, start, end, rev });
        }
        if let Some(h) = f.highlight {
            out.push(Item::Rect { x: rx, y: top, w: rw, h: lh, color: h });
        }
        if let Some(c) = &f.comments {
            out.push(Item::Mark { x: rx, y: top, w: rw, h: lh, comments: c.clone() });
        }
        let color = f.color;
        if !run.glyphs.is_empty() {
            out.push(Item::Glyphs(run));
        }
        let thick = (size / 14).max(crate::LU / 2);
        if f.underline || f.link.is_some() {
            out.push(Item::Rect { x: rx, y: baseline + size * 12 / 100, w: rw, h: thick, color });
        }
        if f.strike {
            out.push(Item::Rect { x: rx, y: baseline - size * 28 / 100, w: rw, h: thick, color });
        }
        if let Some(u) = &f.link {
            out.push(Item::Link { x: rx, y: top, w: rw, h: lh, uri: u.to_string() });
        }
    }
    // Tab leaders, in the format of the text after the tab.
    for &(a, b, leader) in &line.leaders {
        let fi = gl.iter().find(|g| bytes.get(g.cluster as usize) == Some(&b'\t')).map_or(0, |g| g.fmt);
        let f = &l.formats[fi as usize];
        let size = size_lu(f);
        let thick = (size / 14).max(crate::LU / 2);
        match leader {
            Leader::None => {}
            Leader::Line => out.push(Item::Rect { x: left + a, y: baseline + size * 12 / 100, w: b - a, h: thick, color: f.color }),
            Leader::Dots | Leader::Dashes => {
                let mark = if leader == Leader::Dots { "." } else { "-" };
                let Some(g) = fonts.shape(mark, f).first().copied() else { continue };
                // On a grid from the paragraph's edge, so leaders line up.
                let pitch = (g.adv * 2).max(crate::LU);
                let mut x = (a + pitch / 2 + pitch - 1) / pitch * pitch;
                let mut run = GlyphRun { face: g.face, size, color: f.color, x: left + x, y: baseline, glyphs: Vec::new(), text: String::new() };
                let origin = x;
                while x + g.adv <= b - pitch / 2 {
                    let n = run.text.len() as u32;
                    run.text.push_str(mark);
                    run.glyphs.push(PaintGlyph { id: g.id, x: x - origin + g.dx, dy: g.dy, text: n..n + 1 });
                    x += pitch;
                }
                if !run.glyphs.is_empty() {
                    out.push(Item::Glyphs(run));
                }
            }
        }
    }
}

/// Glyphs for `text` shaped in `fmt` from `x`, as runs by face.
fn runs(out: &mut Vec<Item>, fonts: &dyn Fonts, text: &str, fmt: &CharFormat, x: i64, baseline: i64) {
    let glyphs = fonts.shape(text, fmt);
    let size = size_lu(fmt);
    let mut pen = 0i64;
    let mut i = 0;
    while i < glyphs.len() {
        let face = glyphs[i].face;
        let start = pen;
        let mut run = GlyphRun { face, size, color: fmt.color, x: x + start, y: baseline, glyphs: Vec::new(), text: String::new() };
        while i < glyphs.len() && glyphs[i].face == face {
            let g = glyphs[i];
            let first = i == 0 || glyphs[i - 1].cluster != g.cluster;
            let range = if first {
                let next = glyphs[i + 1..].iter().map(|n| n.cluster).find(|&c| c > g.cluster);
                let n = run.text.len() as u32;
                run.text.push_str(cluster_text(text, g.cluster, next));
                n..run.text.len() as u32
            } else {
                let n = run.text.len() as u32;
                n..n
            };
            run.glyphs.push(PaintGlyph { id: g.id, x: pen - start + g.dx, dy: g.dy, text: range });
            pen += g.adv;
            i += 1;
        }
        out.push(Item::Glyphs(run));
    }
}

/// The items of laid-out paragraph `l` (text `text`) with its first line's
/// top at `top` and its start edge at `left`, and `label` in its hang if it
/// has one: a paragraph outside a page (a slide's text frame).
#[allow(clippy::too_many_arguments)]
pub fn paint_para(out: &mut Vec<Item>, doc: &TextDoc, fonts: &dyn Fonts, l: &ParaLayout, text: &str, left: i64, top: i64, label: Option<(&str, Option<CharFormat>)>) {
    if let Some((label, fmt)) = label {
        match fmt {
            Some(f) if !l.lines.is_empty() => {
                let x0 = twips((l.format.indent_start + l.format.first_line) as i64);
                runs(out, fonts, label, &f, left + x0, baseline_of(l, 0, top));
            }
            _ => list_label(out, l, label, fonts, left, top),
        }
    }
    let mut y = top;
    for li in 0..l.lines.len() {
        line(out, doc, fonts, l, li, text, left, y, None);
        y += l.lines[li].height;
    }
}

/// A list item's label in its hang, on the first line's baseline, in the
/// format the item's text starts with.
fn list_label(out: &mut Vec<Item>, l: &ParaLayout, label: &str, fonts: &dyn Fonts, left: i64, top: i64) {
    if l.lines.is_empty() {
        return;
    }
    let mut f = l.glyphs.first().map_or(&l.formats[0], |g| &l.formats[g.fmt as usize]).clone();
    f.underline = false;
    f.strike = false;
    f.link = None;
    f.object = None;
    let x0 = twips((l.format.indent_start + l.format.first_line) as i64);
    runs(out, fonts, label, &f, left + x0, baseline_of(l, 0, top));
}

/// One line of text in `fmt`, aligned in a box `width` wide from `left`.
#[allow(clippy::too_many_arguments)]
fn text_line(out: &mut Vec<Item>, fonts: &dyn Fonts, text: &str, fmt: &CharFormat, align: Align, left: i64, width: i64, baseline: i64) {
    let w: i64 = fonts.shape(text, fmt).iter().map(|g| g.adv).sum();
    let x0 = match align {
        Align::Center => left + (width - w) / 2,
        Align::End => left + width - w,
        _ => left,
    };
    runs(out, fonts, text, fmt, x0, baseline);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::FixedFonts;
    use crate::model::{CharAttrs, HeaderFooter, ImageData, ObjectRef};
    use crate::paginate::Paginator;

    #[test]
    fn a_page_paints_text_rules_pictures_links_and_footer() {
        let mut d = TextDoc::new();
        d.blocks.remove(0);
        let link = d.attrs.intern(CharAttrs { link: Some("https://example.org/".into()), ..Default::default() });
        d.push_para("Hello world", 0, vec![(0, 0), (6, link)]);
        let img = d.images.add(ImageData { bytes: std::sync::Arc::from(vec![0u8; 4]), width: 10, height: 10 });
        let pic = d.attrs.intern(CharAttrs { object: Some(ObjectRef { image: img, width: 1440, height: 720, float: None }), ..Default::default() });
        d.push_para("a\u{FFFC}b", 0, vec![(0, 0), (1, pic), (4, 0)]);
        d.footer = Some(HeaderFooter { text: "Page {page} of {pages}".into(), align: Align::Center, paras: Vec::new() });
        let fonts = FixedFonts;
        let mut cache = LayoutCache::default();
        let mut p = Paginator::new(&d);
        p.run(&d, &mut cache, &fonts, usize::MAX);
        let items = paint_page(&d, p.pages(), 0, p.width(), &mut cache, &fonts, &Numbering::build(&d), "1");
        let texts: Vec<String> = items.iter().filter_map(|i| if let Item::Glyphs(r) = i { Some(r.text.clone()) } else { None }).collect();
        assert_eq!(texts, ["Page 1 of 1", "Hello ", "world", "a", "b"]);
        assert!(items.iter().any(|i| matches!(i, Item::Link { uri, .. } if uri == "https://example.org/")));
        assert!(items.iter().any(|i| matches!(i, Item::Image { w, h, .. } if *w == twips(1440) && *h == twips(720))));
        // Underline under the link.
        assert!(items.iter().any(|i| matches!(i, Item::Rect { .. })));
        // Every glyph of a run points at its text.
        for i in &items {
            if let Item::Glyphs(r) = i {
                let joined: String = r.glyphs.iter().map(|g| &r.text[g.text.start as usize..g.text.end as usize]).collect();
                assert_eq!(joined, r.text);
            }
        }
    }

    #[test]
    fn headers_of_their_own_paragraphs_paint_pictures_and_page_numbers() {
        use crate::model::Para;
        let mut d = TextDoc::new();
        d.push_para("Body", 0, vec![]);
        let img = d.images.add(ImageData { bytes: std::sync::Arc::from(vec![0u8; 4]), width: 10, height: 10 });
        let pic = d.attrs.intern(CharAttrs { object: Some(ObjectRef { image: img, width: 720, height: 360, float: None }), ..Default::default() });
        let bold = d.attrs.intern(CharAttrs { bold: Some(true), ..Default::default() });
        let mut logo = Para::new(d.new_id(), "\u{FFFC} Acme", 0);
        logo.runs = vec![(0, pic), (3, bold)];
        let mut num = Para::new(d.new_id(), "Page {page} of {pages}", 0);
        num.runs = vec![(0, 0), (5, bold), (11, 0)];
        d.header = Some(HeaderFooter { text: "Acme".into(), align: Align::Start, paras: vec![logo] });
        d.footer = Some(HeaderFooter { text: "Page {page} of {pages}".into(), align: Align::Start, paras: vec![num] });
        let fonts = FixedFonts;
        let mut cache = LayoutCache::default();
        let mut p = Paginator::new(&d);
        p.run(&d, &mut cache, &fonts, usize::MAX);
        let items = paint_page(&d, p.pages(), 0, p.width(), &mut cache, &fonts, &Numbering::build(&d), "7");
        let texts: Vec<String> = items.iter().filter_map(|i| if let Item::Glyphs(r) = i { Some(r.text.clone()) } else { None }).collect();
        assert!(texts.iter().any(|t| t.contains("Acme")), "{texts:?}");
        // The page number keeps the bold its placeholder had.
        assert!(texts.windows(3).any(|w| w == ["Page ", "1", " of 7"]), "{texts:?}");
        let pic_y = items.iter().find_map(|i| if let Item::Image { y, h, .. } = i { Some(*y + *h) } else { None }).unwrap();
        // In the top margin.
        assert!(pic_y <= twips(d.page.margin_top as i64), "{pic_y}");
    }

    #[test]
    fn first_and_even_pages_paint_their_own_footers() {
        use crate::model::Margins;
        let mut d = TextDoc::new();
        d.blocks.remove(0);
        for i in 0..150 {
            d.push_para(&format!("Paragraph {i} of the body text."), 0, vec![]);
        }
        d.footer = Some(HeaderFooter::line("Odd {page}", Align::Center));
        d.first_page = Some(Margins { header: None, footer: Some(HeaderFooter::line("Title", Align::Center)) });
        d.even_pages = Some(Margins { header: None, footer: Some(HeaderFooter::line("Even {page}", Align::Center)) });
        let fonts = FixedFonts;
        let mut cache = LayoutCache::default();
        let mut p = Paginator::new(&d);
        p.run(&d, &mut cache, &fonts, usize::MAX);
        assert!(p.pages().len() >= 4);
        let foot = |i: usize, cache: &mut LayoutCache| {
            let items = paint_page(&d, p.pages(), i, p.width(), cache, &fonts, &Numbering::build(&d), "9");
            items.iter().find_map(|x| if let Item::Glyphs(r) = x { Some(r.text.clone()) } else { None }).unwrap()
        };
        assert_eq!([foot(0, &mut cache), foot(1, &mut cache), foot(2, &mut cache), foot(3, &mut cache)], ["Title", "Even 2", "Odd 3", "Even 4"]);
    }

    #[test]
    fn retext_keeps_the_format_round_the_change() {
        use crate::model::Para;
        let mut d = TextDoc::new();
        let bold = d.attrs.intern(CharAttrs { bold: Some(true), ..Default::default() });
        let img = d.images.add(ImageData { bytes: std::sync::Arc::from(vec![1u8; 4]), width: 1, height: 1 });
        let pic = d.attrs.intern(CharAttrs { object: Some(ObjectRef { image: img, width: 20, height: 20, float: None }), ..Default::default() });
        let p = Para { runs: vec![(0, pic), (3, 0), (4, bold), (8, 0)], ..Para::new(1, "\u{FFFC} Acme Ltd", 0) };
        // A word typed after the bold one: plain, as the space before it.
        let q = p.retext("\u{FFFC} Acme Big Ltd", &d.attrs, 0);
        assert_eq!(q.runs, [(0, pic), (3, 0), (4, bold), (8, 0)]);
        // Typed inside the bold word: bold; the picture kept.
        let q = p.retext("\u{FFFC} Acmes Ltd", &d.attrs, 0);
        assert_eq!(q.runs, [(0, pic), (3, 0), (4, bold), (9, 0)]);
        // Typed right after the picture: plain, not a second picture.
        let q = p.retext("\u{FFFC}X Acme Ltd", &d.attrs, 0);
        assert_eq!(q.runs, [(0, pic), (3, 0), (5, bold), (9, 0)]);
        // Everything replaced.
        assert_eq!(p.retext("New", &d.attrs, 0).runs, [(0, 0)]);
        assert_eq!(p.retext("", &d.attrs, 0).text, "");
    }

    #[test]
    fn a_headers_table_rows_become_lines() {
        use crate::model::{rows_as_lines, CellRef, Para};
        let cell = |row, col| Some(CellRef { table: 1, row, col, width: 1000 });
        let mk = |t: &str, c| Para { props: crate::model::ParaProps { cell: c, ..Default::default() }, ..Para::new(0, t, 0) };
        let v = rows_as_lines(vec![mk("Title", None), mk("A", cell(0, 0)), mk("B", cell(0, 1)), mk("b2", cell(0, 1)), mk("C", cell(1, 0)), mk("", cell(1, 1)), mk("D", cell(1, 2))]);
        assert_eq!(v.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(), ["Title", "A\tB b2", "C\tD"]);
        assert!(v.iter().all(|p| p.props.cell.is_none() && p.runs.iter().all(|r| r.0 as usize <= p.text.len())));
    }

    #[test]
    fn notes_paint_numbers_rules_and_text() {
        use crate::model::NoteKind;
        use officina_core::history::Op;
        let mut d = TextDoc::new();
        d.blocks.remove(0);
        let k = d.push_para("Body text here", 0, vec![]);
        for (at, kind, t) in [(4u32, NoteKind::Foot, "foot text"), (9, NoteKind::End, "end text")] {
            for op in d.insert_note_ops(k, at, kind, t, 0) {
                op.apply(&mut d);
            }
        }
        let fonts = FixedFonts;
        let mut cache = LayoutCache::default();
        let mut p = Paginator::new(&d);
        p.run(&d, &mut cache, &fonts, usize::MAX);
        assert_eq!(p.pages().len(), 1);
        let items = paint_page(&d, p.pages(), 0, p.width(), &mut cache, &fonts, &Numbering::build(&d), "1");
        let texts: Vec<String> = items.iter().filter_map(|i| if let Item::Glyphs(r) = i { Some(r.text.clone()) } else { None }).collect();
        // The anchors show their numbers, raised and smaller; each note its number and text.
        assert!(texts.contains(&"1".to_string()) && texts.contains(&"i".to_string()), "{texts:?}");
        assert!(texts.iter().any(|t| t.contains("foot")) && texts.iter().any(|t| t.contains("end")), "{texts:?}");
        let small = items.iter().filter_map(|i| if let Item::Glyphs(r) = i { Some(r) } else { None }).find(|r| r.text == "1").unwrap();
        assert!(small.size < crate::pt(11));
        assert!(p.pages()[0].foot_rule.is_some() && p.pages()[0].end_rule.is_some());
    }
}
