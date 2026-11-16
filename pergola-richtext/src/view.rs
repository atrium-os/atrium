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

use officina_text::track::{Edit, EditKind};

use crate::editor::{Editor, Outcome, Pos};

/// Space between pages and around them, in units.
pub const GAP: f32 = 16.0;
/// The base for node coordinates moves in steps of this many units.
pub const REBASE: f64 = 8192.0;
/// Pagination per frame while it runs in the background.
pub const FRAME_BUDGET: Duration = Duration::from_millis(6);

/// The geometry one render used, for the handlers.
#[derive(Debug, Clone)]
struct Geom {
    rect: Rect,
    /// Units per layout unit.
    k: f64,
    /// The document's page: its size, where it sits across, its margins.
    page_w: f64,
    page_h: f64,
    left: f64,
    margin_x: f64,
    margin_y: f64,
    /// Pages of other sizes (sections with their own): each page laid out so
    /// far, when any differs; pages past them are the document's.
    boxes: std::rc::Rc<[PageBox]>,
    /// The widest page.
    max_w: f64,
}

/// One page's place in the view: its top, size and margins (view units).
#[derive(Debug, Clone, Copy)]
struct PageBox {
    top: f64,
    w: f64,
    h: f64,
    mx: f64,
    my: f64,
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
        let left = if ed.review {
            // The review lane takes the right of the view.
            ((rect.w() as f64 - LANE_W as f64 - page_w) / 2.0).max(GAP as f64)
        } else if !ed.doc.comments.is_empty() && rect.w() as f64 >= page_w + side + 2.0 * GAP as f64 {
            ((rect.w() as f64 - page_w - side) / 2.0).max(GAP as f64)
        } else {
            ((rect.w() as f64 - page_w) / 2.0).max(GAP as f64)
        };
        let pages = ed.pages.pages();
        let (mut boxes, mut max_w) = (Vec::new(), page_w);
        if pages.iter().any(|p| p.setup != *g) {
            let mut top = GAP as f64;
            for p in pages {
                let s = &p.setup;
                let b = PageBox { top, w: twips(s.width as i64) as f64 * k, h: twips(s.height as i64) as f64 * k, mx: twips(s.margin_start as i64) as f64 * k, my: twips(s.margin_top as i64) as f64 * k };
                max_w = max_w.max(b.w);
                top += b.h + GAP as f64;
                boxes.push(b);
            }
        }
        Geom { rect, k, page_w, page_h, left, margin_x: twips(g.margin_start as i64) as f64 * k, margin_y: twips(g.margin_top as i64) as f64 * k, boxes: boxes.into(), max_w }
    }
    /// Page `i`'s place: its own, or the document page's.
    fn page(&self, i: usize) -> PageBox {
        match self.boxes.get(i) {
            Some(b) => *b,
            None => {
                let (top, n) = match self.boxes.last() {
                    Some(b) => (b.top + b.h + GAP as f64, self.boxes.len()),
                    None => (GAP as f64, 0),
                };
                PageBox { top: top + (i - n) as f64 * (self.page_h + GAP as f64), w: self.page_w, h: self.page_h, mx: self.margin_x, my: self.margin_y }
            }
        }
    }
    fn page_top(&self, i: usize) -> f64 {
        self.page(i).top
    }
    /// Where page `i`'s left edge sits: pages of other widths centred on the
    /// document's.
    fn left_of(&self, i: usize) -> f64 {
        let w = self.page(i).w;
        (self.left + (self.page_w - w) / 2.0).max(GAP as f64)
    }
    /// The page at content y (nearest).
    fn page_at(&self, y: f64, pages: usize) -> usize {
        let n = self.boxes.len();
        if n > 0 && y < self.page(n).top - GAP as f64 / 2.0 {
            return self.boxes.partition_point(|b| b.top + b.h + GAP as f64 / 2.0 <= y).min(pages.saturating_sub(1));
        }
        let top = self.page(n).top;
        (n + ((y - top + GAP as f64 / 2.0) / (self.page_h + GAP as f64)).floor().max(0.0) as usize).min(pages.saturating_sub(1))
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
/// The review lane's width (officina.md §16.22).
pub const LANE_W: f32 = 280.0;
/// One edit's row in the review lane.
const LANE_ROW: f32 = 44.0;

/// Colours for authors' changes, in the order authors first edited.
const AUTHOR_COLORS: [u32; 8] = [0x2f6fdf, 0xc2410c, 0x15803d, 0x9333ea, 0x0e7490, 0xa16207, 0xbe185d, 0x4b5563];

/// The colour of `author`'s changes in `ed`'s document: the n-th author to
/// edit gets the n-th colour, so two people never share one (up to eight).
pub fn author_color(ed: &Editor, author: &str) -> Color {
    let mut seen: Vec<&str> = Vec::new();
    for (_, r) in ed.doc.revisions.iter() {
        if !seen.contains(&&*r.author) {
            seen.push(&r.author);
        }
    }
    let i = seen.iter().position(|a| *a == author).unwrap_or(seen.len());
    rgb(AUTHOR_COLORS[i % AUTHOR_COLORS.len()])
}

fn rev_author(ed: &Editor, rev: officina_text::model::RevId) -> String {
    ed.doc.revisions.get(rev).map_or_else(|| "?".to_string(), |r| r.author.to_string())
}

/// Whether text `block`, `start..end` lies in one of `e`'s ranges.
fn in_edit(e: &Edit, block: usize, start: u32, end: u32) -> bool {
    e.ranges.iter().any(|&(a, b)| (block, start) < b.max((b.0, b.1 + 1)) && (block, end) > a)
}

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
        let ext_w = (g.max_w + 2.0 * GAP as f64).max(self.rect.w() as f64);
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
            let picked = if ed.review { ed.review_edit() } else { None };
            for i in first..=last {
                let pb = g.page(i);
                let (px, py) = at(g.left_of(i), pb.top);
                ctx.key(2 + i as u64);
                let grp = ctx.push(Node::vstack(Rect::new(px, py, pb.w as f32, pb.h as f32), 0.0));
                ctx.set_flex(grp, FlexStyle { free: true, ..FlexStyle::default() });
                ctx.add(Node::Rect { rect: Rect::new(px - 1.0, py - 1.0, pb.w as f32 + 2.0, pb.h as f32 + 2.0), fill: t.border_default(), radius: 0.0 });
                ctx.add(Node::Rect { rect: Rect::new(px, py, pb.w as f32, pb.h as f32), fill: paper, radius: 0.0 });
                let numbering = ed.numbering();
                let pages_text = if ed.pages.done() { known.to_string() } else { format!("{est}") };
                let col_w = ed.pages.width();
                // The selection, on the paper and under the text.
                let (a, b) = ed.selection();
                if a != b {
                    let sel = t.selection_bg();
                    for (_, rx, ry, rw, rh) in selection_rects(&mut ed, a, b, i, i) {
                        let (sx, sy) = at(g.left_of(i) + pb.mx + rx as f64 * g.k, pb.top + pb.my + ry as f64 * g.k);
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
                let mut bars: Vec<(i64, String)> = Vec::new();
                let mut labels: Vec<(f64, f64, String)> = Vec::new();
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
                        Item::Path { cmds, fill, line } => {
                            use officina_text::model::{Dash, PathCmd};
                            use pergola::shape::{Paint, PathData, Shape, Stroke};
                            let p = |x: i64, y: i64| ((ox + x as f64 * g.k) as f32, (oy + y as f64 * g.k) as f32);
                            let mut d = PathData::new();
                            for c in &cmds {
                                d = match *c {
                                    PathCmd::Move(x, y) => {
                                        let (x, y) = p(x, y);
                                        d.move_to(x, y)
                                    }
                                    PathCmd::Line(x, y) => {
                                        let (x, y) = p(x, y);
                                        d.line_to(x, y)
                                    }
                                    PathCmd::Cubic(a, b, c, e, f, h) => {
                                        let ((a, b), (c, e), (f, h)) = (p(a, b), p(c, e), p(f, h));
                                        d.cubic_to(a, b, c, e, f, h)
                                    }
                                    PathCmd::Close => d.close(),
                                };
                            }
                            let mut sh = Shape::new(d);
                            if let Some(f) = fill {
                                sh = sh.fill(Paint::Solid(rgb(f)));
                            }
                            if let Some((color, width, dash)) = line {
                                let wd = ((width as f64 * g.k) as f32).max(0.75);
                                let mut st = Stroke::new(Paint::Solid(rgb(color)), wd);
                                st.dash = match dash {
                                    Dash::Solid => None,
                                    Dash::Dot => Some((vec![wd, wd], 0.0)),
                                    Dash::Dash => Some((vec![4.0 * wd, 3.0 * wd], 0.0)),
                                    Dash::DashDot => Some((vec![4.0 * wd, 3.0 * wd, wd, 3.0 * wd], 0.0)),
                                    Dash::LongDash => Some((vec![8.0 * wd, 3.0 * wd], 0.0)),
                                };
                                sh = sh.stroke(st);
                            }
                            for f in sh.fills() {
                                ctx.add(Node::Fill(Arc::new(f)));
                            }
                        }
                        Item::Shape { .. } => {}
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
                        Item::Change { x, y, w, h, block, start, end, rev } => {
                            // The page reads clean: a bar in the left margin
                            // says a line has changes, in its author's colour.
                            let author = rev_author(&ed, rev);
                            let col = author_color(&ed, &author);
                            // Two people on one line: two bars side by side.
                            if !bars.iter().any(|b| b.0 == y && b.1 == author) {
                                let n = bars.iter().filter(|b| b.0 == y).count() as f64;
                                bars.push((y, author.clone()));
                                ctx.add(Node::Rect { rect: Rect::new(q(ox + pb.mx * 0.5 - 1.5 - 5.0 * n), q(oy + y as f64 * g.k), 3.0, (h as f64 * g.k).max(2.0) as f32), fill: col, radius: 1.5 });
                            }
                            // In review, the picked edit is shown in place.
                            let Some(e) = picked.as_ref().filter(|e| in_edit(e, block, start, end)) else { continue };
                            let (rx, ry, rh) = (ox + x as f64 * g.k, oy + y as f64 * g.k, (h as f64 * g.k) as f32);
                            if w == 0 {
                                ctx.add(Node::Rect { rect: Rect::new(q(rx - 1.0), q(ry), 2.0, rh), fill: Color::from_srgb8(0xc0, 0x26, 0x26), radius: 0.0 });
                                let old = e.old.trim();
                                let other;
                                let old = if let EditKind::Conflict(_) = e.kind {
                                    // A conflict shows the other version where it waits.
                                    other = format!("or: {}", e.new.trim());
                                    other.as_str()
                                } else {
                                    old
                                };
                                let text = if e.kind == EditKind::Move {
                                    "moved from here".to_string()
                                } else if old.chars().count() > 60 {
                                    old.chars().take(59).chain(std::iter::once('…')).collect()
                                } else {
                                    old.to_string()
                                };
                                if !text.is_empty() && !labels.iter().any(|l| l.2 == text) {
                                    labels.push((rx, ry, text));
                                }
                            } else {
                                let fill = match e.kind {
                                    EditKind::Format => Color::rgba(0.2, 0.45, 0.95, 0.18),
                                    EditKind::Conflict(_) => Color::rgba(0.95, 0.6, 0.1, 0.25),
                                    _ => Color::rgba(0.1, 0.65, 0.3, 0.2),
                                };
                                ctx.add(Node::Rect { rect: Rect::new(q(rx), q(ry), (w as f64 * g.k).max(1.0) as f32, rh), fill, radius: 0.0 });
                            }
                        }
                        Item::Link { .. } => {}
                    }
                }
                // What a deletion took away, struck through, over its place.
                // A conflict's other version, in amber, not struck.
                for (lx, ly, text) in labels {
                    let alt = text.starts_with("or: ");
                    let ink = if alt { Color::from_srgb8(0x8a, 0x4b, 0x08) } else { Color::from_srgb8(0x99, 0x1b, 0x1b) };
                    let style = pergola::TextStyle { family: pergola::theme::font::SANS.into(), size: pergola::theme::type_size::XS, weight: pergola::theme::Weight::Regular, color: ink };
                    let tw = Node::measure_text(&text, &style).w + 12.0;
                    let (bx, by) = (q(lx - 6.0), q(ly - 20.0));
                    let back = if alt { Color::rgba(1.0, 0.95, 0.85, 1.0) } else { Color::rgba(1.0, 0.93, 0.93, 1.0) };
                    ctx.add(Node::Rect { rect: Rect::new(bx, by, tw, 18.0), fill: back, radius: 4.0 });
                    ctx.add(Node::Text { rect: Rect::new(bx + 6.0, by + 1.0, tw - 8.0, 16.0), content: text, style });
                    if !alt {
                        ctx.add(Node::Rect { rect: Rect::new(bx + 6.0, by + 9.0, tw - 12.0, 1.0), fill: ink, radius: 0.0 });
                    }
                }
                // Comment balloons in the space right of the page, each at its
                // text's first line on this page, pushed down so none overlap.
                let room = r.w() as f64 - (g.left_of(i) + pb.w);
                if !ed.review && !balloons.is_empty() && room >= BALLOON_W as f64 + 16.0 {
                    let at_caret = ed.comment_at_caret();
                    let bx = px + pb.w as f32 + 8.0;
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
                // (Pages can be empty for a frame while pagination restarts at
                // the top, e.g. when fields near the start change.)
                if let (true, Some(page)) = (ed.checker.is_some(), ed.pages.pages().get(i).cloned()) {
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
                            let base = top + line.skip + line.ascent + (line.height - line.skip - natural).max(0) / 2;
                            for &(a, b) in bad.iter() {
                                if b <= line.start || a >= line.end.max(line.start + 1) || (caret.block == s.block && caret.at == b) {
                                    continue;
                                }
                                let x0 = ed.x_in_line(s.block, &l, line, a.max(line.start));
                                let x1 = ed.x_in_line(s.block, &l, line, b.min(line.end));
                                let y = py as f64 + pb.my + (base + line.descent / 2) as f64 * g.k;
                                let (sx0, sx1) = (px as f64 + pb.mx + (s.x + x0) as f64 * g.k, px as f64 + pb.mx + (s.x + x1) as f64 * g.k);
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
                let pb = g.page(s.page);
                let (px, py) = at(g.left_of(s.page) + pb.mx + s.x as f64 * g.k, pb.top + pb.my + s.y as f64 * g.k);
                ctx.add(Node::Rect { rect: Rect::new(px, py, 1.5, (s.h as f64 * g.k) as f32), fill: t.accent_fg(), radius: 0.0 });
            }
            ctx.pop();
            ctx.pop();
        }
        ctx.pop();
        if ed.review {
            self.lane(ctx, &mut ed);
        }
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
            let g = g.clone();
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
            let g = g.clone();
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

impl RichTextView {
    /// The review lane at the right: the document's edits in order, each in
    /// a sentence, the picked one marked; a click picks one.
    fn lane(&self, ctx: &mut Ctx, ed: &mut Editor) {
        let t = ctx.theme;
        let r = self.rect;
        let edits = ed.edits();
        let picked = ed.review_at;
        let lx = r.x() + r.w() - LANE_W;
        ctx.add(Node::Rect { rect: Rect::new(lx - 1.0, r.y(), 1.0, r.h()), fill: t.border_default(), radius: 0.0 });
        ctx.add(Node::Rect { rect: Rect::new(lx, r.y(), LANE_W, r.h()), fill: t.bg_surface(), radius: 0.0 });
        let text = |c: Color, wt, size| pergola::TextStyle { family: pergola::theme::font::SANS.into(), size, weight: wt, color: c };
        let xs = pergola::theme::type_size::XS;
        let sm = pergola::theme::type_size::SM;
        let mut authors: Vec<String> = Vec::new();
        for e in edits.iter() {
            let a = rev_author(ed, e.rev);
            if !authors.contains(&a) {
                authors.push(a);
            }
        }
        let head = match (edits.len(), authors.len()) {
            (0, _) => "No changes".to_string(),
            (1, _) => "1 edit".to_string(),
            (n, 1) => format!("{n} edits"),
            (n, m) => format!("{n} edits by {m} people"),
        };
        ctx.add(Node::Text { rect: Rect::new(lx + 12.0, r.y() + 10.0, LANE_W - 24.0, 20.0), content: head, style: text(t.text_primary(), pergola::theme::Weight::Semibold, sm) });
        // Who changed what, one line per author.
        let mut y = r.y() + 34.0;
        for a in &authors {
            let n = edits.iter().filter(|e| rev_author(ed, e.rev) == *a).count();
            ctx.add(Node::Rect { rect: Rect::new(lx + 12.0, y + 5.0, 8.0, 8.0), fill: author_color(ed, a), radius: 4.0 });
            ctx.add(Node::Text { rect: Rect::new(lx + 26.0, y, LANE_W - 38.0, 18.0), content: format!("{a} · {n}"), style: text(t.text_secondary(), pergola::theme::Weight::Regular, xs) });
            y += 18.0;
        }
        y += 8.0;
        ctx.add(Node::Rect { rect: Rect::new(lx + 12.0, y, LANE_W - 24.0, 1.0), fill: t.border_default(), radius: 0.0 });
        y += 6.0;
        // The rows, scrolled so the picked one shows.
        let fit = (((r.y() + r.h() - y) / LANE_ROW).floor() as usize).max(1);
        let first = picked.saturating_sub(fit / 2).min(edits.len().saturating_sub(fit));
        for (i, e) in edits.iter().enumerate().skip(first).take(fit) {
            let a = rev_author(ed, e.rev);
            let row = Rect::new(lx + 6.0, y, LANE_W - 12.0, LANE_ROW - 4.0);
            let bg = if i == picked { t.selection_bg() } else { t.bg_surface() };
            let node = ctx.add(Node::Rect { rect: row, fill: bg, radius: 6.0 });
            ctx.add(Node::Rect { rect: Rect::new(lx + 10.0, y + 6.0, 3.0, LANE_ROW - 16.0), fill: author_color(ed, &a), radius: 1.5 });
            let kind = match e.kind {
                EditKind::Insert => "Insertion",
                EditKind::Delete => "Deletion",
                EditKind::Replace => "Replacement",
                EditKind::Move => "Move",
                EditKind::Break => "Paragraph",
                EditKind::Format => "Formatting",
                EditKind::Conflict(_) => "Conflict",
            };
            let meta = if e.minor { format!("{a} · {kind} · minor") } else { format!("{a} · {kind}") };
            ctx.add(Node::Text { rect: Rect::new(lx + 20.0, y + 3.0, LANE_W - 36.0, 16.0), content: meta, style: text(t.text_secondary(), pergola::theme::Weight::Regular, xs) });
            let summary: String = if e.summary.chars().count() > 38 { e.summary.chars().take(37).chain(std::iter::once('…')).collect() } else { e.summary.clone() };
            ctx.add(Node::Text { rect: Rect::new(lx + 20.0, y + 19.0, LANE_W - 36.0, 18.0), content: summary, style: text(t.text_primary(), pergola::theme::Weight::Regular, xs) });
            let (edc, rev, sc) = (self.editor.clone(), self.rev.clone(), self.scroll.clone());
            let (rect, zoom) = (self.rect, self.zoom);
            ctx.on_click(node, move || {
                let mut e = edc.borrow_mut();
                e.review_select(i);
                let g = Geom::of(&e, rect, zoom);
                let mut st = sc.get();
                reveal(&mut e, &g, &mut st);
                if st != sc.get() {
                    sc.set(st);
                }
                rev.set(rev.get().wrapping_add(1));
            });
            y += LANE_ROW;
        }
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
    let pb = g.page(pg);
    let lx = ((p.x - g.rect.x()) as f64 - g.left_of(pg) - pb.mx) / g.k;
    let ly = (cy - pb.top - pb.my) / g.k;
    e.hit(pg, lx as i64, ly as i64)
}

/// Scrolls so the caret shows.
fn reveal(e: &mut Editor, g: &Geom, st: &mut ScrollState) {
    let c = e.caret;
    let s = e.spot(c);
    let pb = g.page(s.page);
    let top = pb.top + pb.my + s.y as f64 * g.k;
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
                        out.push((pg, s.x + x0, y + line.skip, x1 - x0, line.height - line.skip));
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
    fn contents_inserted_at_the_top_repaginate_without_a_gap() {
        struct NoWords;
        impl crate::editor::Checker for NoWords {
            fn misspelled(&self, _: &str) -> Vec<(u32, u32)> {
                Vec::new()
            }
        }
        let (mut app, ed, _) = host(3000);
        {
            let mut e = ed.borrow_mut();
            let h1 = e.doc.styles.by_name("Heading 1").unwrap();
            for b in (0..3000).step_by(50) {
                e.doc.blocks.update(b, |x| x.as_para_mut().unwrap().style = h1);
            }
            let d = e.doc.clone();
            e.pages.reset(&d);
            while !e.paginate(Duration::from_secs(5)) {}
            e.checker = Some(Rc::new(NoWords));
            e.set_caret(Pos::new(0, 0), false);
            assert!(e.insert_toc());
        }
        // Frames while pagination and fields settle (the page list restarts
        // at the top): every frame draws.
        for _ in 0..200 {
            app.tick();
            if ed.borrow().pages.done() {
                break;
            }
        }
        assert!(ed.borrow().pages.done());
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

    /// Pages of a section with a landscape page of its own sit at their own
    /// size, one under another, and clicks land on the page under them.
    #[test]
    fn pages_of_other_sizes_stack_and_take_clicks() {
        use officina_text::model::{PageSetup, Section};
        let (mut app, ed, scroll) = host(400);
        {
            let mut guard = ed.borrow_mut();
            let e = &mut *guard;
            let g = e.doc.page;
            let wide = PageSetup { width: g.height, height: g.width, ..g };
            e.doc.blocks.update(200, |b| b.as_para_mut().unwrap().props.section = Some(Section { page: Some(wide), ..Section::default() }));
            e.pages.reset(&e.doc);
            while !e.paginate(Duration::from_secs(5)) {}
        }
        app.tick();
        let g = Geom::of(&ed.borrow(), Rect::new(0.0, 0.0, 900.0, 700.0), 1.0);
        let n = ed.borrow().pages.pages().len();
        let k = ed.borrow().pages.page_of(200, 0);
        assert!(k > 0 && k + 1 < n, "the landscape section is in the middle");
        let (a, b) = (g.page(k - 1), g.page(k));
        assert!(b.w > a.w && b.h < a.h, "landscape: {a:?} then {b:?}");
        for i in 0..n {
            let p = g.page(i);
            assert_eq!(g.page_at(p.top + p.h / 2.0, n), i, "page {i}");
            if i + 1 < n {
                assert!((g.page(i + 1).top - (p.top + p.h + GAP as f64)).abs() < 1e-6);
            }
        }
        // Scrolled to it, a click on the landscape page's first line, 10
        // characters in.
        let mut st = scroll.get();
        st.physics.set_position(b.top - 20.0);
        scroll.set(st);
        app.tick();
        let x = (g.left_of(k) + b.mx + (10.0 * 352.0 + 100.0) * g.k) as f32;
        let y = (b.top + b.my + (0.5 * 11.0 * 64.0 * 1.15) * g.k - scroll.get().physics.y) as f32;
        app.handle_event(Event::PointerDown { at: Point::new(x, y) });
        app.handle_event(Event::PointerUp { at: Point::new(x, y) });
        app.tick();
        let c = ed.borrow().caret;
        assert_eq!((c.block, ed.borrow_mut().spot(c).page), (200, k), "{c:?}");
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
