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

use officina_text::model::CommentId;
use officina_text::paint::{paint_page, Item};
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
        // With comments, the page and the balloon column are centred together
        // when both fit.
        let side = BALLOON_W as f64 + 16.0;
        let left = if !ed.doc.comments.is_empty() && rect.w() as f64 >= page_w + side + 2.0 * GAP as f64 {
            ((rect.w() as f64 - page_w - side) / 2.0).max(GAP as f64)
        } else {
            ((rect.w() as f64 - page_w) / 2.0).max(GAP as f64)
        };
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
    on_find: Option<Rc<dyn Fn()>>,
    on_comment: Option<Rc<dyn Fn(CommentId)>>,
}

/// A comment balloon's width beside the page, and the room it needs.
pub const BALLOON_W: f32 = 200.0;

impl RichTextView {
    pub fn new(editor: Rc<RefCell<Editor>>, scroll: Mutable<ScrollState>, rev: Mutable<u64>, rect: Rect) -> Self {
        RichTextView { editor, scroll, rev, rect, zoom: 1.0, key: 0x7274_6578, on_find: None, on_comment: None }
    }
    pub fn zoom(mut self, z: f32) -> Self {
        self.zoom = z.clamp(0.25, 5.0);
        self
    }
    pub fn key(mut self, k: u64) -> Self {
        self.key = k;
        self
    }
    /// Called on Ctrl+F (and Ctrl+H) in the text.
    pub fn on_find(mut self, f: impl Fn() + 'static) -> Self {
        self.on_find = Some(Rc::new(f));
        self
    }

    /// Called when a comment's balloon is clicked (the caret has moved to
    /// the comment's text).
    pub fn on_comment(mut self, f: impl Fn(CommentId) + 'static) -> Self {
        self.on_comment = Some(Rc::new(f));
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

    /// Scrolls so the editor's caret shows (after a command moved it: find,
    /// go to).
    pub fn reveal_caret(&self) {
        let mut e = self.editor.borrow_mut();
        let g = Geom::of(&e, self.rect, self.zoom);
        let mut st = self.scroll.get();
        reveal(&mut e, &g, &mut st);
        if st != self.scroll.get() {
            self.scroll.set(st);
        }
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
                let numbering = ed.numbering();
                let pages_text = if ed.pages.done() { known.to_string() } else { format!("{est}") };
                let col_w = ed.pages.width();
                // The selection, on the paper and under the text.
                let (a, b) = ed.selection();
                if a != b {
                    let sel = t.selection_bg();
                    for (_, rx, ry, rw, rh) in selection_rects(&mut ed, a, b, i, i) {
                        let (sx, sy) = at(g.left + g.margin_x + rx as f64 * g.k, g.page_top(i) + g.margin_y + ry as f64 * g.k);
                        ctx.add(Node::Rect { rect: Rect::new(sx, sy, (rw as f64 * g.k) as f32, (rh as f64 * g.k) as f32), fill: sel, radius: 0.0 });
                    }
                }
                let items = {
                    let e = &mut *ed;
                    let fonts = e.fonts_rc();
                    paint_page(&e.doc, e.pages.pages(), i, col_w, &mut e.cache, &*fonts, &numbering, &pages_text)
                };
                let (ox, oy) = (px as f64, py as f64);
                let mut balloons: Vec<(CommentId, f64)> = Vec::new();
                for it in items {
                    match it {
                        Item::Rect { x, y, w, h, color } => {
                            ctx.add(Node::Rect { rect: Rect::new(q(ox + x as f64 * g.k), q(oy + y as f64 * g.k), (w as f64 * g.k).max(1.0) as f32, (h as f64 * g.k).max(1.0) as f32), fill: rgb(color), radius: 0.0 });
                        }
                        Item::Glyphs(r) => {
                            if let Some(face) = faces.and_then(|fs| fs.get(r.face as usize)) {
                                let run: Vec<PGlyph> = r.glyphs.iter().map(|gl| PGlyph { id: gl.id, dx: (gl.x as f64 * g.k) as f32, dy: (gl.dy as f64 * g.k) as f32 }).collect();
                                ctx.add(Node::Glyphs { origin: Point::new(q(ox + r.x as f64 * g.k), q(oy + r.y as f64 * g.k)), face: face.clone(), size: (r.size as f64 * g.k) as f32, color: rgb(r.color), glyphs: Arc::from(run) });
                            }
                        }
                        Item::Image { image, x, y, w, h } => {
                            let (w, h) = (w as f64 * g.k, h as f64 * g.k);
                            let rect = Rect::new(q(ox + x as f64 * g.k), q(oy + y as f64 * g.k), w as f32, h as f32);
                            // A placeholder until it decodes.
                            match ed.doc.images.get(image).and_then(|d| crate::pictures::picture(d, w * 2.0, h * 2.0)) {
                                Some(image) => {
                                    ctx.add(Node::Image { rect, image });
                                }
                                None => {
                                    ctx.add(Node::Rect { rect, fill: Color::rgba(0.85, 0.86, 0.88, 1.0), radius: 0.0 });
                                }
                            }
                        }
                        Item::Mark { x, y, w, h, comments } => {
                            let done = comments.iter().all(|&c| ed.doc.comments.get(c).is_none_or(|c| c.done));
                            let fill = if done { Color::rgba(0.55, 0.57, 0.6, 0.12) } else { Color::rgba(1.0, 0.78, 0.15, 0.32) };
                            ctx.add(Node::Rect { rect: Rect::new(q(ox + x as f64 * g.k), q(oy + y as f64 * g.k), (w as f64 * g.k).max(1.0) as f32, (h as f64 * g.k).max(1.0) as f32), fill, radius: 0.0 });
                            for &c in comments.iter() {
                                let root = ed.thread_root(c);
                                if !balloons.iter().any(|b: &(CommentId, f64)| b.0 == root) {
                                    balloons.push((root, oy + y as f64 * g.k));
                                }
                            }
                        }
                        Item::Link { .. } => {}
                    }
                }
                // Comment balloons in the space right of the page, each at its
                // text's first line on this page, pushed down so none overlap.
                let room = r.w() as f64 - (g.left + g.page_w);
                if !balloons.is_empty() && room >= BALLOON_W as f64 + 16.0 {
                    let at_caret = ed.comment_at_caret();
                    let bx = px + g.page_w as f32 + 8.0;
                    let mut next_y = f64::MIN;
                    let small = |c: Color, wt| pergola::TextStyle { family: pergola::theme::font::SANS.into(), size: pergola::theme::type_size::XS, weight: wt, color: c };
                    for (id, y) in std::mem::take(&mut balloons) {
                        let thread = ed.thread(id);
                        let Some(first) = ed.doc.comments.get(id).cloned() else { continue };
                        let y = y.max(next_y);
                        let lines: Vec<String> = first.text.lines().take(3).map(|l| l.to_string()).collect();
                        let h = 22.0 + 16.0 * lines.len().max(1) as f32 + if thread.len() > 1 { 16.0 } else { 0.0 };
                        let rect = Rect::new(bx, q(y), BALLOON_W, h);
                        let border = if at_caret == Some(id) { t.accent_fg() } else { t.border_default() };
                        ctx.add(Node::Rect { rect: Rect::new(bx - 1.0, q(y) - 1.0, BALLOON_W + 2.0, h + 2.0), fill: border, radius: 6.0 });
                        let bg = if first.done { Color::rgba(0.96, 0.96, 0.97, 1.0) } else { Color::rgba(1.0, 0.98, 0.9, 1.0) };
                        let node = ctx.add(Node::Rect { rect, fill: bg, radius: 5.0 });
                        let head = if first.done { format!("{} · resolved", first.author) } else { first.author.to_string() };
                        ctx.add(Node::Text { rect: Rect::new(bx + 8.0, q(y) + 5.0, BALLOON_W - 16.0, 16.0), content: head, style: small(t.text_secondary(), pergola::theme::Weight::Semibold) });
                        for (k, l) in lines.iter().enumerate() {
                            ctx.add(Node::Text { rect: Rect::new(bx + 8.0, q(y) + 22.0 + 16.0 * k as f32, BALLOON_W - 16.0, 16.0), content: l.clone(), style: small(t.text_primary(), pergola::theme::Weight::Regular) });
                        }
                        if thread.len() > 1 {
                            let n = thread.len() - 1;
                            ctx.add(Node::Text { rect: Rect::new(bx + 8.0, q(y) + h - 19.0, BALLOON_W - 16.0, 16.0), content: if n == 1 { "1 reply".into() } else { format!("{n} replies") }, style: small(t.text_secondary(), pergola::theme::Weight::Regular) });
                        }
                        let (edc, cb) = (self.editor.clone(), self.on_comment.clone());
                        ctx.on_click(node, move || {
                            let start = edc.borrow().doc.comment_ranges().into_iter().find(|r| r.0 == id).map(|r| r.1);
                            if let Some((b, at)) = start {
                                edc.borrow_mut().set_caret(Pos::new(b, at), false);
                            }
                            if let Some(f) = &cb {
                                f(id);
                            }
                        });
                        next_y = y + h as f64 + 6.0;
                    }
                }
                // Spelling: a red dotted line under each misspelled word, but
                // not the one being typed.
                if ed.checker.is_some() {
                    let page = ed.pages.pages()[i].clone();
                    let red = Color::from_srgb8(0xd0, 0x20, 0x20);
                    let caret = ed.caret;
                    for s in &page.slices {
                        let bad = ed.misspelled(s.block);
                        if bad.is_empty() {
                            continue;
                        }
                        let l = ed.layout(s.block);
                        let mut top = s.y;
                        for li in s.lines.clone() {
                            let line = &l.lines[li];
                            let natural = line.ascent + line.descent;
                            let base = top + line.ascent + (line.height - natural).max(0) / 2;
                            for &(a, b) in bad.iter() {
                                if b <= line.start || a >= line.end.max(line.start + 1) || (caret.block == s.block && caret.at == b) {
                                    continue;
                                }
                                let x0 = ed.x_in_line(s.block, &l, line, a.max(line.start));
                                let x1 = ed.x_in_line(s.block, &l, line, b.min(line.end));
                                let y = py as f64 + g.margin_y + (base + line.descent / 2) as f64 * g.k;
                                let (sx0, sx1) = (px as f64 + g.margin_x + (s.x + x0) as f64 * g.k, px as f64 + g.margin_x + (s.x + x1) as f64 * g.k);
                                let mut x = sx0;
                                while x < sx1 {
                                    ctx.add(Node::Rect { rect: Rect::new(q(x), q(y), 2.0f32.min((sx1 - x) as f32), 1.5), fill: red, radius: 0.0 });
                                    x += 3.5;
                                }
                            }
                            top += line.height;
                        }
                    }
                }
                ctx.pop();
            }
            // The overlay: selection and caret.
            ctx.key(u64::MAX);
            let over = ctx.push(Node::vstack(local, 0.0));
            ctx.set_flex(over, FlexStyle { free: true, ..FlexStyle::default() });
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
            let (ed, sc, rev, find) = (self.editor.clone(), self.scroll.clone(), self.rev.clone(), self.on_find.clone());
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
                    Outcome::Find => {
                        drop(e);
                        if let Some(f) = &find {
                            f();
                        }
                        return;
                    }
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
    for pg in first..=last {
        let Some(page) = e.pages.pages().get(pg).cloned() else { continue };
        for s in &page.slices {
            if s.block < a.block || s.block > b.block {
                continue;
            }
            let l = e.layout(s.block);
            let width = s.w;
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
                        out.push((pg, s.x + x0, y, x1 - x0, line.height));
                    }
                }
                y += line.height;
            }
        }
    }
    out
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
            RichTextView { editor: self.v.editor.clone(), scroll: self.v.scroll.clone(), rev: self.v.rev.clone(), rect: self.v.rect, zoom: 1.0, key: 7, on_find: None, on_comment: None }.emit(ctx);
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
    fn pictures_decode_once_and_only_near_the_view() {
        use officina_text::model::ImageData;
        let (mut app, ed, scroll) = host(3000);
        {
            let mut e = ed.borrow_mut();
            // A picture on the first page and one far down.
            for b in [0usize, 2900] {
                e.set_caret(Pos::new(b, 0), false);
                e.insert_image(ImageData { bytes: crate::editor::tests::png(64, 32).into(), width: 64, height: 32 });
            }
            while !e.paginate(Duration::from_secs(5)) {}
        }
        let before = crate::pictures::decodes();
        let d = app.tick();
        let images = |d: &[NodeDelta]| d.iter().filter(|x| matches!(x, NodeDelta::Added { node: Node::Image { .. }, .. })).count();
        assert_eq!(images(&d), 1, "only the picture near the view");
        assert_eq!(crate::pictures::decodes() - before, 1, "the same bytes decode once");
        let mut st = scroll.get();
        st.physics.set_position(40.0);
        scroll.set(st);
        app.tick();
        assert_eq!(crate::pictures::decodes() - before, 1, "scrolling reuses the decode");
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
