//! The paged view: pages stacked down a scroll view, each a scene group keyed
//! by its page number, sent only for the pages near the viewport (so a
//! 1,500-page document holds as many nodes as a 3-page one, officina.md §7.4).
//! Text is drawn as shaped glyph runs from the paragraph layouts; the selection
//! and the caret sit in an overlay group, so moving the caret sends no page.
//!
//! The content scrolls in `f64` and is placed relative to a base that moves in
//! steps of [`REBASE`] (as `GridView` does), so node coordinates stay small at
//! page 1,500.
//!
//! Zoom 1 shows a point as 4/3 Pergola units (the 96-dpi convention of other
//! word processors' 100%).

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use officina_text::layout::{size_lu, ParaLayout};
use officina_text::twips;
use pergola::event::{Event, KeyEventKind};
use pergola::geom::{Point, Rect};
use pergola::glyphs::Glyph as PGlyph;
use pergola::interaction::{DragPhase, ScrollInput};
use pergola::layout::FlexStyle;
use pergola::node::{Node, NodeId};
use pergola::reactive::Mutable;
use pergola::view::{Ctx, View};
use pergola::widgets::scroll_view::ScrollState;
use pergola::Color;
use pergola_document::Faces;

use crate::editor::{Editor, Outcome, Pos};

/// Space between pages and around them, in units.
pub const GAP: f32 = 16.0;
/// The base for node coordinates moves in steps of this many units.
pub const REBASE: f64 = 8192.0;
/// Pagination per frame while it runs in the background.
pub const FRAME_BUDGET: Duration = Duration::from_millis(6);

/// The geometry one render used, for the handlers.
#[derive(Debug, Clone, Copy)]
struct Geom {
    rect: Rect,
    /// Units per layout unit.
    k: f64,
    page_w: f64,
    page_h: f64,
    left: f64,
    margin_x: f64,
    margin_y: f64,
}

impl Geom {
    fn of(ed: &Editor, rect: Rect, zoom: f32) -> Geom {
        let k = zoom as f64 * 4.0 / 3.0 / officina_text::LU as f64;
        let g = &ed.doc.page;
        let page_w = twips(g.width as i64) as f64 * k;
        let page_h = twips(g.height as i64) as f64 * k;
        let left = ((rect.w() as f64 - page_w) / 2.0).max(GAP as f64);
        Geom { rect, k, page_w, page_h, left, margin_x: twips(g.margin_start as i64) as f64 * k, margin_y: twips(g.margin_top as i64) as f64 * k }
    }
    fn page_top(&self, i: usize) -> f64 {
        GAP as f64 + i as f64 * (self.page_h + GAP as f64)
    }
    /// The page at content y (nearest).
    fn page_at(&self, y: f64, pages: usize) -> usize {
        (((y - GAP as f64 / 2.0) / (self.page_h + GAP as f64)).floor().max(0.0) as usize).min(pages.saturating_sub(1))
    }
}

/// A rich-text editor showing pages.
pub struct RichTextView {
    pub editor: Rc<RefCell<Editor>>,
    pub scroll: Mutable<ScrollState>,
    /// Bumped by the view when it needs another render: an edit, or
    /// pagination still running in the background.
    pub rev: Mutable<u64>,
    pub rect: Rect,
    pub zoom: f32,
    key: u64,
}

impl RichTextView {
    pub fn new(editor: Rc<RefCell<Editor>>, scroll: Mutable<ScrollState>, rev: Mutable<u64>, rect: Rect) -> Self {
        RichTextView { editor, scroll, rev, rect, zoom: 1.0, key: 0x7274_6578 }
    }
    pub fn zoom(mut self, z: f32) -> Self {
        self.zoom = z.clamp(0.25, 5.0);
        self
    }
    pub fn key(mut self, k: u64) -> Self {
        self.key = k;
        self
    }

    /// Pages laid out so far, and an estimate of the whole count while
    /// pagination is behind (from the characters it has reached).
    fn page_count(ed: &Editor) -> (usize, usize) {
        let n = ed.pages.pages().len().max(1);
        if ed.pages.done() {
            return (n, n);
        }
        let reached = ed.pages.pages().last().map_or(0, |p| ed.doc.blocks.char_of_block(p.start.block)).max(1);
        let total = ed.doc.blocks.counts().chars.max(1);
        (n, ((n as f64 * total as f64 / reached as f64).ceil() as usize).max(n))
    }

    pub fn emit(&self, ctx: &mut Ctx) -> NodeId {
        let t = ctx.theme;
        let faces = Faces::shared().ok();
        let mut ed = self.editor.borrow_mut();
        if !ed.pages.done() && !ed.paginate(FRAME_BUDGET) {
            // Another frame for the rest.
            self.rev.set(self.rev.get().wrapping_add(1));
        }
        let g = Geom::of(&ed, self.rect, self.zoom);
        let (known, est) = Self::page_count(&ed);
        let ext_h = g.page_top(est);
        let ext_w = (g.page_w + 2.0 * GAP as f64).max(self.rect.w() as f64);
        let mut st = self.scroll.get();
        if st.extent != Some([ext_w, ext_h]) || !st.fixed_zoom {
            st.extent = Some([ext_w, ext_h]);
            st.fixed_zoom = true;
            st.physics.set_max(ext_h - self.rect.h() as f64);
            st.physics_x.set_max(ext_w - self.rect.w() as f64);
            self.scroll.set(st);
        }
        let (x, y) = (st.physics_x.y, st.physics.y);
        let by = (y / REBASE).floor() * REBASE;
        let r = self.rect;
        let at = |cx: f64, cy: f64| (q(r.x() as f64 + cx - x), q(r.y() as f64 + cy - by));

        ctx.key(self.key);
        let body = ctx.push(Node::Scroll { rect: r, offset: (y - by) as f32, zoom: 1.0, stretch: 1.0, backdrop: Some(t.bg_surface()), content_h: 0.0, offset_x: 0.0, content_w: 0.0 });
        {
            ctx.key(1);
            let local = Rect::new(r.x(), r.y(), r.w(), ((ext_h - by).clamp(0.0, 3.0 * REBASE)) as f32);
            let content = ctx.push(Node::vstack(local, 0.0));
            ctx.set_flex(content, FlexStyle { free: true, ..FlexStyle::default() });
            let first = g.page_at(y, known).saturating_sub(1);
            let last = (g.page_at(y + r.h() as f64, known) + 1).min(known - 1);
            let paper = Color::rgba(1.0, 1.0, 1.0, 1.0);
            for i in first..=last {
                let (px, py) = at(g.left, g.page_top(i));
                ctx.key(2 + i as u64);
                let grp = ctx.push(Node::vstack(Rect::new(px, py, g.page_w as f32, g.page_h as f32), 0.0));
                ctx.set_flex(grp, FlexStyle { free: true, ..FlexStyle::default() });
                ctx.add(Node::Rect { rect: Rect::new(px - 1.0, py - 1.0, g.page_w as f32 + 2.0, g.page_h as f32 + 2.0), fill: t.border_default(), radius: 0.0 });
                ctx.add(Node::Rect { rect: Rect::new(px, py, g.page_w as f32, g.page_h as f32), fill: paper, radius: 0.0 });
                let (cx, cy) = (px as f64 + g.margin_x, py as f64 + g.margin_y);
                let page = ed.pages.pages()[i].clone();
                let numbering = ed.numbering();
                // Header and footer, in the margins.
                let pages_text = if ed.pages.done() { known.to_string() } else { format!("{est}") };
                for (hf, top) in [(&ed.doc.header, true), (&ed.doc.footer, false)] {
                    let Some(hf) = hf else { continue };
                    let text = hf.resolve(i + 1, &pages_text);
                    let from = twips(officina_text::model::HEADER_FROM_EDGE as i64) as f64 * g.k;
                    let fmt = officina_text::model::CharFormat { size: 18, color: 0x555555, ..Default::default() };
                    let size = size_lu(&fmt) as f64 * g.k;
                    let baseline = if top { py as f64 + from + size * 0.8 } else { py as f64 + g.page_h - from };
                    let width = g.page_w - 2.0 * g.margin_x;
                    draw_text(ctx, faces, ed.fonts(), &text, &fmt, hf.align, px as f64 + g.margin_x, width, baseline, g.k);
                }
                for s in &page.slices {
                    let l = ed.layout(s.block);
                    let text = ed.doc.para(s.block).map(|p| p.text.clone()).unwrap_or_default();
                    if s.lines.start == 0 {
                        if let Some(label) = numbering.label(s.block) {
                            draw_label(ctx, faces, &l, &label, ed.fonts(), cx, cy + s.y as f64 * g.k, g.k);
                        }
                    }
                    let mut ly = s.y;
                    for li in s.lines.clone() {
                        draw_line(ctx, faces, &l, li, &text, cx, cy + ly as f64 * g.k, g.k);
                        ly += l.lines[li].height;
                    }
                }
                ctx.pop();
            }
            // The overlay: selection and caret.
            ctx.key(u64::MAX);
            let over = ctx.push(Node::vstack(local, 0.0));
            ctx.set_flex(over, FlexStyle { free: true, ..FlexStyle::default() });
            let (a, b) = ed.selection();
            if a != b {
                let mut sel = t.selection_bg();
                sel.a = sel.a.max(0.35);
                for (pg, rx, ry, rw, rh) in selection_rects(&mut ed, a, b, first, last) {
                    let (px, py) = at(g.left + g.margin_x + rx as f64 * g.k, g.page_top(pg) + g.margin_y + ry as f64 * g.k);
                    ctx.add(Node::Rect { rect: Rect::new(px, py, (rw as f64 * g.k) as f32, (rh as f64 * g.k) as f32), fill: sel, radius: 0.0 });
                }
            }
            let c = ed.caret;
            let s = ed.spot(c);
            if s.page >= first && s.page <= last {
                let (px, py) = at(g.left + g.margin_x + s.x as f64 * g.k, g.page_top(s.page) + g.margin_y + s.y as f64 * g.k);
                ctx.add(Node::Rect { rect: Rect::new(px, py, 1.5, (s.h as f64 * g.k) as f32), fill: t.accent_fg(), radius: 0.0 });
            }
            ctx.pop();
            ctx.pop();
        }
        ctx.pop();
        drop(ed);
        ctx.focusable(body);

        let s = self.scroll.clone();
        ctx.on_scroll(body, move |inp: &ScrollInput| {
            let mut st = s.get();
            st.input(inp.kind, inp.t, inp.node);
            s.set(st);
        });
        let s = self.scroll.clone();
        ctx.on_frame(body, move |t, node| {
            let mut st = s.get();
            let more = st.frame(t, node);
            if st != s.get() {
                s.set(st);
            }
            more
        });
        {
            let (ed, sc, rev) = (self.editor.clone(), self.scroll.clone(), self.rev.clone());
            ctx.on_key(body, move |ev| {
                let Event::Key { kind, key, modifiers, chars } = ev else { return };
                if *kind == KeyEventKind::Up {
                    return;
                }
                let mut e = ed.borrow_mut();
                let page_h = ((g.rect.h() as f64 - 2.0 * GAP as f64) / g.k).max(1.0) as i64;
                let out = e.key(*key, *modifiers, chars, page_h);
                match out {
                    Outcome::Ignored => return,
                    Outcome::Copy(t) => {
                        let _ = pergola::clipboard::copy_text(&t, false);
                    }
                    Outcome::Paste => {
                        if let Ok(Some(t)) = pergola::clipboard::paste_text() {
                            e.insert(&t);
                        }
                    }
                    Outcome::Edited | Outcome::Moved => {}
                }
                let mut st = sc.get();
                reveal(&mut e, &g, &mut st);
                if st != sc.get() {
                    sc.set(st);
                }
                rev.set(rev.get().wrapping_add(1));
            });
        }
        {
            let (ed, sc, rev) = (self.editor.clone(), self.scroll.clone(), self.rev.clone());
            ctx.on_drag(body, move |phase, p| {
                let st = sc.get();
                let mut e = ed.borrow_mut();
                let pos = hit(&mut e, &g, st.physics.y, p);
                match phase {
                    DragPhase::Start => e.set_caret(pos, false),
                    DragPhase::Move => e.set_caret(pos, true),
                    _ => return,
                }
                rev.set(rev.get().wrapping_add(1));
            });
        }
        body
    }
}

impl View for RichTextView {
    fn render(&self, ctx: &mut Ctx) {
        self.emit(ctx);
    }
}

/// The text position under window point `p`.
fn hit(e: &mut Editor, g: &Geom, scroll_y: f64, p: Point) -> Pos {
    let cy = scroll_y + (p.y - g.rect.y()) as f64;
    let n = e.pages.pages().len();
    let pg = g.page_at(cy, n);
    let lx = ((p.x - g.rect.x()) as f64 - g.left - g.margin_x) / g.k;
    let ly = (cy - g.page_top(pg) - g.margin_y) / g.k;
    e.hit(pg, lx as i64, ly as i64)
}

/// Scrolls so the caret shows.
fn reveal(e: &mut Editor, g: &Geom, st: &mut ScrollState) {
    let c = e.caret;
    let s = e.spot(c);
    let top = g.page_top(s.page) + g.margin_y + s.y as f64 * g.k;
    let bottom = top + s.h as f64 * g.k;
    let h = g.rect.h() as f64;
    let margin = (s.h as f64 * g.k * 2.0).min(h / 4.0);
    if top - margin < st.physics.y {
        st.physics.stop();
        st.physics.set_position((top - margin).max(0.0));
    } else if bottom + margin > st.physics.y + h {
        st.physics.stop();
        st.physics.set_position(bottom + margin - h);
    }
}

/// Rects (page, x, y, w, h in layout units from the page's content box) that
/// cover a..b on pages first..=last.
fn selection_rects(e: &mut Editor, a: Pos, b: Pos, first: usize, last: usize) -> Vec<(usize, i64, i64, i64, i64)> {
    let mut out = Vec::new();
    let width = e.pages.width();
    for pg in first..=last {
        let Some(page) = e.pages.pages().get(pg).cloned() else { continue };
        for s in &page.slices {
            if s.block < a.block || s.block > b.block {
                continue;
            }
            let l = e.layout(s.block);
            let mut y = s.y;
            for li in s.lines.clone() {
                let line = &l.lines[li];
                let (ls, le) = (Pos::new(s.block, line.start), Pos::new(s.block, line.end));
                let last_line = li + 1 == l.lines.len();
                let (from, to) = (a.max(ls), b.min(le));
                // A line is covered where the selection meets it; a paragraph's
                // last line also when the selection runs on past its mark.
                if from < to || (last_line && from == le && b > le) {
                    let x0 = if from > ls { e.x_in_line(s.block, &l, line, from.at) } else { line.x };
                    let x1 = if b > le {
                        let end = e.x_in_line(s.block, &l, line, line.end);
                        // The paragraph mark: a little past the text.
                        if last_line { (end + 320).min(width.max(end)) } else { end }
                    } else {
                        e.x_in_line(s.block, &l, line, to.at)
                    };
                    if x1 > x0 {
                        out.push((pg, x0, y, x1 - x0, line.height));
                    }
                }
                y += line.height;
            }
        }
    }
    out
}

/// Draws line `li` of a paragraph with its top at window y `top`, content
/// left at `left`.
#[allow(clippy::too_many_arguments)]
fn draw_line(ctx: &mut Ctx, faces: Option<&Faces>, l: &ParaLayout, li: usize, text: &str, left: f64, top: f64, k: f64) {
    let line = &l.lines[li];
    let natural = line.ascent + line.descent;
    let baseline = top + (line.ascent + (line.height - natural).max(0) / 2) as f64 * k;
    let bytes = text.as_bytes();
    let gl = &l.glyphs[line.glyphs.clone()];
    let mut pen = line.x;
    let mut i = 0;
    while i < gl.len() {
        let (face, fmt) = (gl[i].face, gl[i].fmt);
        let f = &l.formats[fmt as usize];
        let x0 = pen;
        let mut run: Vec<PGlyph> = Vec::new();
        while i < gl.len() && gl[i].face == face && gl[i].fmt == fmt {
            let g = gl[i];
            let ch = text[g.cluster as usize..].chars().next().unwrap_or(' ');
            if !ch.is_control() && ch != '\u{2028}' && ch != '\t' {
                run.push(PGlyph { id: g.id, dx: ((pen - x0 + g.dx) as f64 * k) as f32, dy: (g.dy as f64 * k) as f32 });
            }
            pen += g.adv;
            if line.space_extra != 0 && bytes.get(g.cluster as usize) == Some(&b' ') {
                pen += line.space_extra;
            }
            i += 1;
        }
        let (rx, rw) = (left + x0 as f64 * k, (pen - x0) as f64 * k);
        let size = size_lu(f) as f64 * k;
        if let Some(h) = f.highlight {
            ctx.add(Node::Rect { rect: Rect::new(rx as f32, top as f32, rw as f32, (line.height as f64 * k) as f32), fill: rgb(h), radius: 0.0 });
        }
        let color = rgb(f.color);
        if let (Some(fs), false) = (faces, run.is_empty()) {
            if let Some(face) = fs.get(face as usize) {
                ctx.add(Node::Glyphs { origin: Point::new(q(rx), q(baseline)), face: face.clone(), size: size as f32, color, glyphs: Arc::from(run) });
            }
        }
        let thick = (size / 14.0).max(1.0) as f32;
        if f.underline || f.link.is_some() {
            ctx.add(Node::Rect { rect: Rect::new(rx as f32, (baseline + size * 0.12) as f32, rw as f32, thick), fill: color, radius: 0.0 });
        }
        if f.strike {
            ctx.add(Node::Rect { rect: Rect::new(rx as f32, (baseline - size * 0.28) as f32, rw as f32, thick), fill: color, radius: 0.0 });
        }
    }
}

/// A list item's label in its hang, on the first line's baseline, in the
/// format the item's text starts with.
#[allow(clippy::too_many_arguments)]
fn draw_label(ctx: &mut Ctx, faces: Option<&Faces>, l: &ParaLayout, label: &str, fonts: &dyn officina_text::layout::Fonts, left: f64, top: f64, k: f64) {
    let Some(line) = l.lines.first() else { return };
    let f = l.glyphs.first().map_or(&l.formats[0], |g| &l.formats[g.fmt as usize]);
    let mut f = f.clone();
    f.underline = false;
    f.strike = false;
    f.link = None;
    let glyphs = fonts.shape(label, &f);
    let natural = line.ascent + line.descent;
    let baseline = top + (line.ascent + (line.height - natural).max(0) / 2) as f64 * k;
    let x0 = twips((l.format.indent_start + l.format.first_line) as i64) as f64;
    let size = (size_lu(&f) as f64 * k) as f32;
    let mut pen = 0i64;
    let mut i = 0;
    while i < glyphs.len() {
        let face = glyphs[i].face;
        let start = pen;
        let mut run = Vec::new();
        while i < glyphs.len() && glyphs[i].face == face {
            run.push(PGlyph { id: glyphs[i].id, dx: ((pen - start + glyphs[i].dx) as f64 * k) as f32, dy: (glyphs[i].dy as f64 * k) as f32 });
            pen += glyphs[i].adv;
            i += 1;
        }
        if let Some(fc) = faces.and_then(|fs| fs.get(face as usize)) {
            ctx.add(Node::Glyphs { origin: Point::new(q(left + (x0 + start as f64) * k), q(baseline)), face: fc.clone(), size, color: rgb(f.color), glyphs: Arc::from(run) });
        }
    }
}

/// One line of text in `fmt`, aligned in a box `width` wide from `left`.
#[allow(clippy::too_many_arguments)]
fn draw_text(ctx: &mut Ctx, faces: Option<&Faces>, fonts: &dyn officina_text::layout::Fonts, text: &str, fmt: &officina_text::model::CharFormat, align: officina_text::model::Align, left: f64, width: f64, baseline: f64, k: f64) {
    use officina_text::model::Align;
    let glyphs = fonts.shape(text, fmt);
    let w = glyphs.iter().map(|g| g.adv).sum::<i64>() as f64 * k;
    let x0 = match align {
        Align::Center => left + (width - w) / 2.0,
        Align::End => left + width - w,
        _ => left,
    };
    let size = (size_lu(fmt) as f64 * k) as f32;
    let mut pen = 0i64;
    let mut i = 0;
    while i < glyphs.len() {
        let face = glyphs[i].face;
        let start = pen;
        let mut run = Vec::new();
        while i < glyphs.len() && glyphs[i].face == face {
            run.push(PGlyph { id: glyphs[i].id, dx: ((pen - start + glyphs[i].dx) as f64 * k) as f32, dy: (glyphs[i].dy as f64 * k) as f32 });
            pen += glyphs[i].adv;
            i += 1;
        }
        if let Some(fc) = faces.and_then(|fs| fs.get(face as usize)) {
            ctx.add(Node::Glyphs { origin: Point::new(q(x0 + start as f64 * k), q(baseline)), face: fc.clone(), size, color: rgb(fmt.color), glyphs: Arc::from(run) });
        }
    }
}

fn rgb(c: u32) -> Color {
    Color::from_srgb8((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

/// A coordinate snapped to 1/64 unit (see `GridView`).
fn q(v: f64) -> f32 {
    ((v * 64.0).round() / 64.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use officina_text::layout::FixedFonts;
    use officina_text::model::TextDoc;
    use pergola::app::App;
    use pergola::event::{Key, Modifiers};
    use pergola::node::NodeDelta;
    use std::sync::atomic::AtomicBool;

    struct Host {
        v: RichTextView,
    }
    impl View for Host {
        fn render(&self, ctx: &mut Ctx) {
            RichTextView { editor: self.v.editor.clone(), scroll: self.v.scroll.clone(), rev: self.v.rev.clone(), rect: self.v.rect, zoom: 1.0, key: 7 }.emit(ctx);
        }
    }

    fn host(paras: usize) -> (App<Host>, Rc<RefCell<Editor>>, Mutable<ScrollState>) {
        let mut d = TextDoc::new();
        d.blocks.remove(0);
        for i in 0..paras {
            d.push_para(&format!("{i} {}", "lorem ipsum dolor sit amet ".repeat(8)), 0, vec![]);
        }
        let mut e = Editor::new(d, Rc::new(FixedFonts));
        while !e.paginate(Duration::from_secs(5)) {}
        let ed = Rc::new(RefCell::new(e));
        let dirty = Arc::new(AtomicBool::new(true));
        let scroll = Mutable::with_dirty(ScrollState::default(), dirty.clone());
        let rev = Mutable::with_dirty(0u64, dirty.clone());
        let v = RichTextView::new(ed.clone(), scroll.clone(), rev, Rect::new(0.0, 0.0, 900.0, 700.0));
        (App::new_with_flag(Host { v }, dirty), ed, scroll)
    }

    fn added(d: &[NodeDelta]) -> usize {
        d.iter().filter(|d| matches!(d, NodeDelta::Added { .. })).count()
    }

    #[test]
    fn a_long_document_sends_only_pages_near_the_view() {
        let (mut app, ed, scroll) = host(16000);
        let pages = ed.borrow().pages.pages().len();
        assert!(pages >= 1000, "{pages} pages");
        let first = app.tick();
        let n0 = added(&first);
        assert!(n0 > 50 && n0 < 4000, "{n0} nodes");
        // Far down: about the same number of nodes, at small coordinates.
        let mut st = scroll.get();
        let g = Geom::of(&ed.borrow(), Rect::new(0.0, 0.0, 900.0, 700.0), 1.0);
        st.physics.set_position(g.page_top(pages - 10));
        scroll.set(st);
        let d = app.tick();
        assert!(added(&d) < 4000);
        for x in &d {
            if let NodeDelta::Added { node: Node::Glyphs { origin, .. }, .. } = x {
                assert!(origin.y.abs() < 3.0 * REBASE as f32 + 2000.0, "rebased: {}", origin.y);
            }
        }
    }

    #[test]
    fn clicking_places_the_caret_and_typing_changes_one_page() {
        let (mut app, ed, _) = host(200);
        app.tick();
        let g = Geom::of(&ed.borrow(), Rect::new(0.0, 0.0, 900.0, 700.0), 1.0);
        // Click on the third line of the first page, 10 characters in.
        let x = (g.left + g.margin_x + (10.0 * 352.0 + 100.0) * g.k) as f32;
        let lh = 11.0 * 64.0 * 1.15;
        let y = (g.page_top(0) + g.margin_y + (2.5 * lh) * g.k) as f32;
        app.handle_event(Event::PointerDown { at: Point::new(x, y) });
        app.handle_event(Event::PointerUp { at: Point::new(x, y) });
        app.tick();
        let c = ed.borrow().caret;
        let s = ed.borrow_mut().spot(c);
        assert_eq!(s.page, 0);
        assert_eq!(s.x, 10 * 352, "{c:?}");
        // Type: the caret's page and the overlay change; no other page.
        app.handle_event(Event::Key { kind: KeyEventKind::Down, key: Key::Char, modifiers: Modifiers::default(), chars: "Z".into() });
        let d = app.tick();
        assert!(ed.borrow().doc.para(c.block).unwrap().text.contains('Z'));
        assert!(!d.is_empty());
        assert!(added(&d) < 200, "{} nodes added for one key", added(&d));
        // Arrow keys move only the overlay.
        app.handle_event(Event::Key { kind: KeyEventKind::Down, key: Key::Right, modifiers: Modifiers::default(), chars: String::new() });
        let d = app.tick();
        assert!(added(&d) <= 3, "{d:?}");
    }
}
