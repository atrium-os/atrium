//! What a page shows, as a list of simple items in layout units from the
//! page's top-left corner: glyph runs, rectangles, pictures and link areas.
//! The screen view and PDF export both draw from it, so a page looks the same
//! in both (docs/spec/officina.md §7.4, O3f).

use std::ops::Range;

use crate::layout::{object_height, size_lu, vert_shift, Fonts, LayoutCache, ParaLayout, OBJECT_FACE};
use crate::lists::Numbering;
use crate::model::{Align, CharFormat, TextDoc, HEADER_FROM_EDGE, OBJECT_CHAR};
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
    let g = &doc.page;
    let (mx, my) = (twips(g.margin_start as i64), twips(g.margin_top as i64));
    let (pw, ph) = (twips(g.width as i64), twips(g.height as i64));

    // Header and footer, in the margins.
    for (hf, top) in [(&doc.header, true), (&doc.footer, false)] {
        let Some(hf) = hf else { continue };
        let text = hf.resolve(i + 1, pages_text);
        let from = twips(HEADER_FROM_EDGE as i64);
        let fmt = CharFormat { size: HEADER_FORMAT_SIZE, color: HEADER_COLOR, ..Default::default() };
        let size = size_lu(&fmt);
        let baseline = if top { from + size * 4 / 5 } else { ph - from };
        let width = pw - mx - twips(g.margin_end as i64);
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

    for s in &page.slices {
        let Some(p) = doc.para(s.block) else { continue };
        let w = doc.block_box(s.block, col_w).1;
        let l = cache.get(doc, p, w, fonts);
        let left = mx + s.x;
        if s.lines.start == 0 {
            if let Some(label) = numbering.label(s.block) {
                list_label(&mut out, &l, &label, fonts, left, my + s.y);
            }
        }
        let mut ly = s.y;
        for li in s.lines.clone() {
            line(&mut out, doc, &l, li, &p.text, left, my + ly);
            ly += l.lines[li].height;
        }
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
            line(&mut out, doc, &l, li, &p.text, left, my + ly);
            ly += l.lines[li].height;
        }
    }
    out
}

/// The baseline of line `li` whose top is `top`.
fn baseline_of(l: &ParaLayout, li: usize, top: i64) -> i64 {
    let line = &l.lines[li];
    let natural = line.ascent + line.descent;
    top + line.ascent + (line.height - natural).max(0) / 2
}

/// The text a glyph shows: its cluster, up to the next cluster.
fn cluster_text(text: &str, start: u32, next: Option<u32>) -> &str {
    let s = start as usize;
    let e = next.map_or_else(|| text[s..].chars().next().map_or(s, |c| s + c.len_utf8()), |n| n as usize);
    text.get(s..e.max(s)).unwrap_or("")
}

#[allow(clippy::too_many_arguments)]
fn line(out: &mut Vec<Item>, doc: &TextDoc, l: &ParaLayout, li: usize, text: &str, left: i64, top: i64) {
    let line = &l.lines[li];
    let baseline = baseline_of(l, li, top);
    let bytes = text.as_bytes();
    let gl = &l.glyphs[line.glyphs.clone()];
    let mut pen = line.x;
    let mut i = 0;
    while i < gl.len() {
        let (face, fmt) = (gl[i].face, gl[i].fmt);
        let f = &l.formats[fmt as usize];
        if face == OBJECT_FACE {
            let g = gl[i];
            if let Some(o) = &f.object {
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
                        _ => run.text.push_str(t),
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
        if let Some(h) = f.highlight {
            out.push(Item::Rect { x: rx, y: top, w: rw, h: line.height, color: h });
        }
        if let Some(c) = &f.comments {
            out.push(Item::Mark { x: rx, y: top, w: rw, h: line.height, comments: c.clone() });
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
            out.push(Item::Link { x: rx, y: top, w: rw, h: line.height, uri: u.to_string() });
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
        let pic = d.attrs.intern(CharAttrs { object: Some(ObjectRef { image: img, width: 1440, height: 720 }), ..Default::default() });
        d.push_para("a\u{FFFC}b", 0, vec![(0, 0), (1, pic), (4, 0)]);
        d.footer = Some(HeaderFooter { text: "Page {page} of {pages}".into(), align: Align::Center });
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
