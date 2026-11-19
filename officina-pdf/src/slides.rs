//! A deck as PDF (printing from Slides): a page per slide, drawn from the
//! slide's display list (officina_slides::draw), the same one the editor,
//! thumbnails and presenting draw, so a printed slide looks like it does on
//! screen. Hidden slides are left out by default. Gradients are PDF shadings, pictures
//! are written once however many slides show them, text is real text.

use std::io::{self, Write};

use folium_content::ops::Op;
use officina_slides::draw::{self, DrawItem, Notes, Paint, StrokePaint, Xform};
use officina_slides::model::{Dash, Deck, MediaId, Path, PathCmd};
use officina_slides::EMU_PER_PT;
use officina_text::layout::Fonts;
use officina_text::LU;

use crate::out::{Page, Pdf};
use crate::{FontFiles, Stats};

/// Picture keys of the deck's media (apart from a document's pictures).
const MEDIA_KEY: u64 = 1 << 32;

fn rgb(c: u32) -> [f64; 3] {
    let f = |s: u32| ((c >> s) & 0xff) as f64 / 255.0;
    [f(24), f(16), f(8)]
}

fn alpha(c: u32) -> u8 {
    (c & 0xff) as u8
}

/// A path's commands in the shape's own box (`w` × `h`).
fn path_ops(p: &Path, w: f64, h: f64, out: &mut Vec<Op>) {
    let sx = if p.w > 0 { w / p.w as f64 } else { 1.0 };
    let sy = if p.h > 0 { h / p.h as f64 } else { 1.0 };
    let q = |x: i64, y: i64| (x as f64 * sx, y as f64 * sy);
    for c in &p.cmds {
        out.push(match *c {
            PathCmd::Move(x, y) => {
                let (x, y) = q(x, y);
                Op::MoveTo(x, y)
            }
            PathCmd::Line(x, y) => {
                let (x, y) = q(x, y);
                Op::LineTo(x, y)
            }
            PathCmd::Cubic(a, b, c, d, e, f) => {
                let ((a, b), (c, d), (e, f)) = (q(a, b), q(c, d), q(e, f));
                Op::CurveTo([a, b, c, d, e, f])
            }
            PathCmd::Close => Op::ClosePath,
        });
    }
}

/// Bands a gradient is drawn in.
pub const GRADIENT_BANDS: usize = 128;

/// The gradient's colour at `t` (0–1), stops in any order.
fn at(stops: &[(f32, u32)], t: f64) -> [f64; 3] {
    let mut s: Vec<(f64, [f64; 3])> = stops.iter().map(|(o, c)| (*o as f64, rgb(*c))).collect();
    s.sort_by(|a, b| a.0.total_cmp(&b.0));
    match s.iter().position(|x| x.0 >= t) {
        None => s.last().map_or([0.0; 3], |x| x.1),
        Some(0) => s[0].1,
        Some(i) => {
            let (a, b) = (s[i - 1], s[i]);
            let k = if b.0 > a.0 { (t - a.0) / (b.0 - a.0) } else { 1.0 };
            [0, 1, 2].map(|j| a.1[j] + (b.1[j] - a.1[j]) * k)
        }
    }
}

/// A gradient as bands of solid colour over the clip (every renderer,
/// Prelum's rasteriser included, draws them; shadings it would skip).
/// `reach` is past every corner of the shape's box.
fn gradient(p: &Paint, reach: f64, out: &mut Vec<Op>) {
    let n = GRADIENT_BANDS;
    match p {
        Paint::Linear { stops, from, to } => {
            let (dx, dy) = (to.0 - from.0, to.1 - from.1);
            let l = (dx * dx + dy * dy).sqrt();
            if l < 1e-9 {
                out.extend([Op::FillRgb(at(stops, 1.0)), Op::Rect(-reach, -reach, 2.0 * reach, 2.0 * reach), Op::Fill]);
                return;
            }
            let (ux, uy, vx, vy) = (dx / l, dy / l, -dy / l, dx / l);
            let pt = |t: f64, side: f64| (from.0 + ux * t + vx * side, from.1 + uy * t + vy * side);
            for i in 0..n {
                let t0 = if i == 0 { -reach } else { l * i as f64 / n as f64 };
                // A little over the next band, so no seam shows.
                let t1 = if i + 1 == n { reach + l } else { l * (i as f64 + 1.0) / n as f64 + l * 0.002 };
                let quad = [pt(t0, -reach), pt(t1, -reach), pt(t1, reach), pt(t0, reach)];
                out.push(Op::FillRgb(at(stops, (i as f64 + 0.5) / n as f64)));
                out.push(Op::MoveTo(quad[0].0, quad[0].1));
                for q in &quad[1..] {
                    out.push(Op::LineTo(q.0, q.1));
                }
                out.extend([Op::ClosePath, Op::Fill]);
            }
        }
        Paint::Radial { stops, center, radius } => {
            let r = radius.max(0.01);
            out.extend([Op::FillRgb(at(stops, 1.0)), Op::Rect(-reach, -reach, 2.0 * reach, 2.0 * reach), Op::Fill]);
            for i in (1..=n).rev() {
                let ri = r * i as f64 / n as f64;
                out.push(Op::FillRgb(at(stops, (i as f64 - 0.5) / n as f64)));
                circle(center.0, center.1, ri, out);
                out.push(Op::Fill);
            }
        }
        _ => {}
    }
}

fn circle(cx: f64, cy: f64, r: f64, out: &mut Vec<Op>) {
    let k = 0.552_284_75 * r;
    out.extend([
        Op::MoveTo(cx + r, cy),
        Op::CurveTo([cx + r, cy + k, cx + k, cy + r, cx, cy + r]),
        Op::CurveTo([cx - k, cy + r, cx - r, cy + k, cx - r, cy]),
        Op::CurveTo([cx - r, cy - k, cx - k, cy - r, cx, cy - r]),
        Op::CurveTo([cx + k, cy - r, cx + r, cy - k, cx + r, cy]),
        Op::ClosePath,
    ]);
}

/// The paint's colour set up for filling: ops before the fill, or None
/// for paints that clip and shade instead (gradients, pictures).
fn solid(page: &mut Page, p: &Paint, out: &mut Vec<Op>) -> bool {
    if let Paint::Solid(c) = p {
        out.push(Op::FillRgb(rgb(*c)));
        out.extend(page.alpha(alpha(*c), 255));
        true
    } else {
        false
    }
}

fn stroke_ops(page: &mut Page, s: &StrokePaint, k: f64, out: &mut Vec<Op>) {
    // Hairlines stay half a point wide.
    let w = (s.width / U).max(0.5 / k.max(1e-12));
    let pattern = match s.dash {
        Dash::Solid => vec![],
        Dash::Dot => vec![w, w],
        Dash::Dash => vec![4.0 * w, 3.0 * w],
        Dash::DashDot => vec![4.0 * w, 3.0 * w, w, 3.0 * w],
        Dash::LongDash => vec![8.0 * w, 3.0 * w],
    };
    out.extend([Op::StrokeRgb(rgb(s.color)), Op::LineWidth(w), Op::Dash(pattern, 0.0)]);
    out.extend(page.alpha(255, alpha(s.color)));
}

/// Picture `media` stretched over `0..w`, `0..h`, cropped (thousandths of
/// a percent off left, top, right, bottom), in the current coordinates
/// (y down); a grey box when it can't be read.
fn picture<W: Write>(pdf: &mut Pdf<W>, page: &mut Page, deck: &Deck, media: MediaId, w: f64, h: f64, crop: [i32; 4]) -> io::Result<()> {
    let name = pdf.picture(page, MEDIA_KEY | media as u64, || deck.media_bytes(media).ok().map(|b| b.to_vec()))?;
    let Some(n) = name else {
        page.c.extend([Op::FillRgb([0.85, 0.86, 0.88]), Op::Rect(0.0, 0.0, w, h), Op::Fill]);
        return Ok(());
    };
    let f = |v: i32| (v as f64 / 100_000.0).clamp(-10.0, 0.99);
    let (l, t, r, b) = (f(crop[0]), f(crop[1]), f(crop[2]), f(crop[3]));
    let (fw, fh) = (w / (1.0 - l - r).max(0.01), h / (1.0 - t - b).max(0.01));
    let (x0, y0) = (-l * fw, -t * fh);
    page.c.extend([Op::Save, Op::Rect(0.0, 0.0, w, h), Op::Clip, Op::Transform([fw, 0.0, 0.0, -fh, x0, y0 + fh]), Op::XObject(n), Op::Restore]);
    Ok(())
}

/// What `export` wrote beyond [`Stats`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Printed {
    pub stats: Stats,
    /// Slides that couldn't be read (printed blank).
    pub damaged: usize,
    pub hidden: usize,
}

/// What to put on each printed page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrintLayout {
    Slides,
    SpeakerNotes,
    HandoutTwo,
    HandoutThree,
    HandoutFour,
    HandoutSix,
    HandoutNine,
}

/// App choices captured for the whole print session, including relayouts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrintOptions {
    pub layout: PrintLayout,
    pub include_hidden: bool,
}

/// Writes the visible slides, one per page.
pub fn export<W: Write>(deck: &mut Deck, paper: Option<(f64, f64)>, fonts: &dyn Fonts, files: &dyn FontFiles, title: &str, out: W, progress: &mut dyn FnMut(usize, usize)) -> io::Result<(W, Printed)> {
    export_layout(deck, paper, PrintLayout::Slides, fonts, files, title, out, progress)
}

#[allow(clippy::too_many_arguments)]
pub fn export_layout<W: Write>(deck: &mut Deck, paper: Option<(f64, f64)>, layout: PrintLayout, fonts: &dyn Fonts, files: &dyn FontFiles, title: &str, out: W, progress: &mut dyn FnMut(usize, usize)) -> io::Result<(W, Printed)> {
    export_with_options(deck, paper, PrintOptions { layout, include_hidden: false }, fonts, files, title, out, progress)
}

#[allow(clippy::too_many_arguments)]
pub fn export_with_options<W: Write>(deck: &mut Deck, paper: Option<(f64, f64)>, options: PrintOptions, fonts: &dyn Fonts, files: &dyn FontFiles, title: &str, out: W, progress: &mut dyn FnMut(usize, usize)) -> io::Result<(W, Printed)> {
    let PrintOptions { layout, include_hidden } = options;
    let slide_size = [deck.size.0.max(1) as f64 / U, deck.size.1.max(1) as f64 / U];
    let (pw, ph) = paper.map_or((slide_size[0], slide_size[1]), |(w, h)| (w.clamp(72.0, 14400.0), h.clamp(72.0, 14400.0)));
    let margin = 36.0f64.min(pw.min(ph) / 8.0);
    let (body_w, body_h) = (pw - 2.0 * margin, ph - 2.0 * margin);
    let (columns, rows) = match layout {
        PrintLayout::HandoutTwo => (1, 2),
        PrintLayout::HandoutThree => (1, 3),
        PrintLayout::HandoutFour => (2, 2),
        PrintLayout::HandoutSix => (2, 3),
        PrintLayout::HandoutNine => (3, 3),
        PrintLayout::Slides | PrintLayout::SpeakerNotes => (1, 1),
    };
    let gap = 12.0;
    let width_columns = if layout == PrintLayout::HandoutThree { 2 } else { columns };
    let slot_h = body_h / rows as f64;
    let slot_w = (body_w - gap * (width_columns - 1) as f64) / width_columns as f64;
    let mut pdf = Pdf::new(out)?;
    let mut printed = Printed::default();
    let mut draw_notes = Notes::default();
    let mut page = Page::default();
    let mut slot = 0;
    let total = deck.slide_count();
    for i in 0..total {
        let items = draw::draw_slide(deck, i, fonts, &mut draw_notes);
        let slide = deck.slide_if_loaded(i);
        if !include_hidden && slide.is_some_and(|sl| sl.hidden) {
            printed.hidden += 1;
        } else {
            let items = items.unwrap_or_else(|_| {
                printed.damaged += 1;
                Vec::new()
            });
            match layout {
                PrintLayout::Slides => {
                    draw_thumbnail(&mut pdf, &mut page, deck, &items, slide_size, [0.0, 0.0, pw, ph], ph, paper.is_some(), false, files)?;
                    pdf.end_page(std::mem::take(&mut page), pw, ph)?;
                }
                PrintLayout::SpeakerNotes => {
                    let note_top = margin + body_h * 0.42;
                    let thumbnail = [margin, margin, body_w, note_top - margin - 16.0];
                    let note = slide.and_then(|slide| slide.notes.as_ref()).map(|body| &body.doc);
                    write_notes(&mut pdf, deck, &items, slide_size, thumbnail, [pw, ph, margin, note_top], note, fonts, files)?;
                }
                PrintLayout::HandoutTwo | PrintLayout::HandoutThree | PrintLayout::HandoutFour
                | PrintLayout::HandoutSix | PrintLayout::HandoutNine => {
                    let (col, row) = (slot % columns, slot / columns);
                    let x = margin + col as f64 * (slot_w + gap);
                    let y = margin + row as f64 * slot_h;
                    let frame = [x, y, slot_w, slot_h - gap];
                    draw_thumbnail(&mut pdf, &mut page, deck, &items, slide_size, frame, ph, true, true, files)?;
                    if layout == PrintLayout::HandoutThree {
                        let left = margin + slot_w + gap;
                        page.c.extend([Op::Save, Op::StrokeRgb([0.75, 0.76, 0.78]), Op::LineWidth(0.5)]);
                        for line in 1..=5 {
                            let line_y = ph - y - frame[3] * f64::from(line) / 6.0;
                            page.c.extend([Op::MoveTo(left, line_y), Op::LineTo(pw - margin, line_y)]);
                        }
                        page.c.extend([Op::Stroke, Op::Restore]);
                    }
                    slot += 1;
                    if slot == columns * rows {
                        pdf.end_page(std::mem::take(&mut page), pw, ph)?;
                        slot = 0;
                    }
                }
            }
        }
        progress(i + 1, total);
    }
    if slot != 0 || pdf.stats.pages == 0 {
        pdf.end_page(page, pw, ph)?;
    }
    let (w, stats) = pdf.finish(files, title, "Atrium Slides")?;
    printed.stats = stats;
    Ok((w, printed))
}

#[allow(clippy::too_many_arguments)]
fn draw_thumbnail<W: Write>(pdf: &mut Pdf<W>, page: &mut Page, deck: &Deck, items: &[DrawItem], [sw, sh]: [f64; 2], [x, y, w, h]: [f64; 4], ph: f64, border: bool, clip: bool, files: &dyn FontFiles) -> io::Result<()> {
    let s = (w / sw).min(h / sh);
    let (slide_w, slide_h) = (sw * s, sh * s);
    let (x, top) = (x + (w - slide_w) / 2.0, ph - y - (h - slide_h) / 2.0);
    let (w, h) = (slide_w, slide_h);
    let e = s / U;
    let base = [e, 0.0, 0.0, -e, x, top];
    if clip { page.c.extend([Op::Save, Op::Rect(x, top - h, w, h), Op::Clip, Op::EndPath]); }
    for it in items {
        item(pdf, page, deck, it, &base, files, &|_| None)?;
    }
    if clip { page.c.push(Op::Restore); }
    if border {
        page.c.extend([Op::Save, Op::StrokeRgb([0.75, 0.76, 0.78]), Op::LineWidth(0.5), Op::Rect(x, top - h, w, h), Op::Stroke, Op::Restore]);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_notes<W: Write>(pdf: &mut Pdf<W>, deck: &Deck, items: &[DrawItem], size: [f64; 2], thumbnail: [f64; 4], [pw, ph, margin, top]: [f64; 4], note: Option<&officina_text::model::TextDoc>, fonts: &dyn Fonts, files: &dyn FontFiles) -> io::Result<()> {
    use officina_text::{layout::LayoutCache, lists::Numbering, model::PageSetup, paginate::Paginator, paint::paint_page};

    let mut page = Page::default();
    draw_thumbnail(pdf, &mut page, deck, items, size, thumbnail, ph, true, true, files)?;
    let Some(note) = note else { return pdf.end_page(page, pw, ph) };
    let mut note = note.clone();
    let twips = |pt: f64| (pt * 20.0).round() as i32;
    note.page = PageSetup {
        width: twips(pw), height: twips(ph),
        margin_top: twips(top), margin_bottom: twips(margin),
        margin_start: twips(margin), margin_end: twips(margin),
    };
    let mut cache = LayoutCache::default();
    let mut pager = Paginator::new(&note);
    while !pager.run(&note, &mut cache, fonts, crate::CACHE_PAGES) {
        cache = LayoutCache::default();
    }
    let pages = pager.pages();
    let numbering = Numbering::build(&note);
    let count = pages.len().to_string();
    for i in 0..pages.len() {
        if i % crate::CACHE_PAGES == 0 { cache = LayoutCache::default(); }
        if i != 0 {
            draw_thumbnail(pdf, &mut page, deck, items, size, thumbnail, ph, true, true, files)?;
        }
        let text = paint_page(&note, pages, i, pager.width(), &mut cache, fonts, &numbering, &count);
        pdf.items(&mut page, text, ph, files, &|_| None)?;
        pdf.end_page(std::mem::take(&mut page), pw, ph)?;
    }
    if pages.is_empty() { pdf.end_page(page, pw, ph)?; }
    Ok(())
}

/// Print into memory using the system's fonts.
pub fn print(deck: &mut Deck, paper: Option<(f64, f64)>, title: &str) -> io::Result<(Vec<u8>, Printed)> {
    print_layout(deck, paper, title, PrintLayout::Slides)
}

pub fn print_layout(deck: &mut Deck, paper: Option<(f64, f64)>, title: &str, layout: PrintLayout) -> io::Result<(Vec<u8>, Printed)> {
    print_with_options(deck, paper, title, PrintOptions { layout, include_hidden: false })
}

pub fn print_with_options(deck: &mut Deck, paper: Option<(f64, f64)>, title: &str, options: PrintOptions) -> io::Result<(Vec<u8>, Printed)> {
    let fonts = officina_text::layout::PinnedFonts::new().map_err(io::Error::other)?;
    export_with_options(deck, paper, options, &fonts, &fonts, title, Vec::new(), &mut |_, _| {})
}

/// EMU to a point.
const U: f64 = EMU_PER_PT as f64;

/// `base` after `xf`, taking the item's coordinates in points (EMU written
/// out would need scales too small for a content stream's numbers).
fn pts(base: &Xform, xf: &Xform) -> Xform {
    draw::mul(&draw::mul(base, xf), &[U, 0.0, 0.0, U, 0.0, 0.0])
}

/// A gradient's geometry in points.
fn in_points(p: &Paint) -> Paint {
    match p {
        Paint::Linear { stops, from, to } => Paint::Linear { stops: stops.clone(), from: (from.0 / U, from.1 / U), to: (to.0 / U, to.1 / U) },
        Paint::Radial { stops, center, radius } => Paint::Radial { stops: stops.clone(), center: (center.0 / U, center.1 / U), radius: radius / U },
        p => p.clone(),
    }
}

fn item<W: Write>(pdf: &mut Pdf<W>, page: &mut Page, deck: &Deck, it: &DrawItem, base: &Xform, files: &dyn FontFiles, none: &dyn Fn(u32) -> Option<Vec<u8>>) -> io::Result<()> {
    match it {
        DrawItem::Path { xf, w, h, paths, fill, stroke } => {
            let m = pts(base, xf);
            let (w, h) = (&(*w / U), &(*h / U));
            let k = (m[0] * m[3] - m[1] * m[2]).abs().sqrt();
            page.c.extend([Op::Save, Op::Transform(m)]);
            for p in paths {
                let mut geo = Vec::new();
                path_ops(p, *w, *h, &mut geo);
                if p.filled {
                    if let Some(f) = fill {
                        let mut c = vec![Op::Save];
                        if solid(page, f, &mut c) {
                            c.extend(geo.iter().cloned());
                            c.push(Op::Fill);
                        } else {
                            c.extend(geo.iter().cloned());
                            c.push(Op::Clip);
                            match f {
                                Paint::Linear { stops, .. } | Paint::Radial { stops, .. } => {
                                    c.extend(page.alpha(stops.first().map_or(255, |s| alpha(s.1)), 255));
                                    gradient(&in_points(f), 4.0 * (*w + *h), &mut c);
                                }
                                Paint::Picture(media) => {
                                    page.c.append(&mut c);
                                    picture(pdf, page, deck, *media, *w, *h, [0; 4])?;
                                }
                                Paint::Solid(_) => {}
                            }
                        }
                        c.push(Op::Restore);
                        page.c.append(&mut c);
                    }
                }
                if let (Some(s), true) = (stroke, p.stroked) {
                    page.c.push(Op::Save);
                    let mut c = Vec::new();
                    stroke_ops(page, s, k, &mut c);
                    page.c.append(&mut c);
                    page.c.extend(geo);
                    page.c.extend([Op::Stroke, Op::Restore]);
                }
            }
            page.c.push(Op::Restore);
        }
        DrawItem::Picture { xf, w, h, media, crop } => {
            page.c.extend([Op::Save, Op::Transform(pts(base, xf))]);
            picture(pdf, page, deck, *media, *w / U, *h / U, *crop)?;
            page.c.push(Op::Restore);
        }
        DrawItem::Video { xf, w, h, poster, .. } => {
            let (w, h) = (&(*w / U), &(*h / U));
            page.c.extend([Op::Save, Op::Transform(pts(base, xf))]);
            match poster {
                Some(p) => picture(pdf, page, deck, *p, *w, *h, [0; 4])?,
                None => page.c.extend([Op::FillRgb([0.0, 0.0, 0.0]), Op::Rect(0.0, 0.0, *w, *h), Op::Fill]),
            }
            page.c.push(Op::Restore);
        }
        DrawItem::Text { xf, items, .. } => {
            // The writer puts layout units at (x / LU, -y / LU) on a page
            // 0 high; this map takes that onto the slide's place.
            let m = draw::mul(&draw::mul(base, xf), &[LU as f64, 0.0, 0.0, -(LU as f64), 0.0, 0.0]);
            page.c.extend([Op::Save, Op::Transform(m)]);
            pdf.items(page, items.iter().cloned(), 0.0, files, none)?;
            page.c.push(Op::Restore);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gradient_colours_blend_between_stops_and_hold_past_the_ends() {
        let s = [(0.75, 0x0000ffff), (0.25, 0xff0000ff)];
        assert_eq!(at(&s, 0.0), [1.0, 0.0, 0.0]);
        assert_eq!(at(&s, 1.0), [0.0, 0.0, 1.0]);
        assert_eq!(at(&s, 0.5), [0.5, 0.0, 0.5]);
    }
}
