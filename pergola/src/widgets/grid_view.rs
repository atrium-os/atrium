//! `GridView` (officina.md §8.5, §10.2): a spreadsheet-style grid of any
//! size, scrolled both ways, with frozen row and column headers, a
//! selection, the spreadsheet keyboard model, an in-cell editor and
//! column resizing. The same widget serves `atrium-sheets`, Scrinium's
//! details view, logs and any large table.
//!
//! **Data.** A [`GridSource`] says how many rows and columns there are,
//! where each starts (in `f64`: a sheet of 2³¹ rows is far taller than an
//! `f32` holds exactly) and what each cell shows. The grid asks only for
//! the cells near the view.
//!
//! **Scene.** The body is a `SCROLL` group (as `ScrollView`'s) whose
//! content holds tiles of [`TILE_ROWS`] × [`TILE_COLS`] cells, each a free
//! stack keyed by its tile, for the tiles in view plus one each side. A
//! scroll frame changes only the three viewports' translations (body,
//! column headers, row headers), so the renderer shifts all three, the
//! body diagonally when both axes move. Headers are viewports that move
//! on one axis.
//!
//! **Precision.** Node coordinates are `f32`, exact to 2²⁴ px. The
//! content is placed relative to a base that moves in steps of
//! [`REBASE`] px as the view travels, so coordinates stay small; crossing
//! a step re-sends the resident tiles once (one full draw).
//!
//! **Selection and editing** draw in an overlay above the tiles, so
//! moving the selection never touches a tile.
//!
//! Not yet: frozen panes of the sheet's own rows and columns (only the
//! headers are frozen), row resizing, auto-scroll while dragging a
//! selection past the edge, Ctrl+arrow data edges beyond what the source
//! says, and placing the caret by pointer in the editor.

use std::rc::Rc;

use crate::event::{Event, Key, Modifiers};
use crate::geom::Rect;
use crate::interaction::{DragPhase, ScrollInput};
use crate::layout::FlexStyle;
use crate::node::{Node, NodeId, TextStyle};
use crate::reactive::Mutable;
use crate::text_edit::{KeyOutcome, TextEdit};
use crate::theme::{font, type_size, Weight};
use crate::view::{Ctx, View};
use crate::widgets::scroll_view::ScrollState;
use crate::Color;

/// Rows in a tile.
pub const TILE_ROWS: u32 = 16;
/// Columns in a tile.
pub const TILE_COLS: u32 = 4;
/// The content's base moves in steps this big (px), keeping `f32`
/// coordinates exact and the viewport's translation inside the protocol's
/// bound: a group transform may move at most ±32,768 device px
/// (`fresco_protocol::FILL_MAX_COORD`), so 8,192 logical px leaves room for
/// a scale of 3 and a viewport's worth of tiles past the base.
pub const REBASE: f64 = 8192.0;
/// The column header's height.
pub const HEADER_H: f32 = 24.0;
/// A press this close to a column's right edge in the header resizes it.
pub const RESIZE_GRIP: f32 = 4.0;
/// The narrowest a column can be dragged.
pub const MIN_COL_W: f64 = 12.0;
/// Text inset from a cell's edges.
const PAD: f32 = 4.0;
const TEXT_SIZE: f32 = type_size::SM;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellAlign {
    Left,
    Center,
    Right,
}

/// What one cell shows.
#[derive(Debug, Clone, PartialEq)]
pub struct CellView {
    pub text: String,
    pub align: CellAlign,
    /// Text colour; the theme's primary text when `None`.
    pub color: Option<Color>,
    pub bold: bool,
    /// A number: shown as `###` when it doesn't fit, never cut.
    pub numeric: bool,
}

impl CellView {
    pub fn text(s: impl Into<String>) -> Self {
        Self { text: s.into(), align: CellAlign::Left, color: None, bold: false, numeric: false }
    }
    pub fn number(s: impl Into<String>) -> Self {
        Self { text: s.into(), align: CellAlign::Right, color: None, bold: false, numeric: true }
    }
}

/// The grid's data, asked for the cells near the view only.
pub trait GridSource {
    fn rows(&self) -> u32;
    fn cols(&self) -> u32;
    /// Where row `r` starts, px from the top (`r` in `0..=rows`).
    fn row_top(&self, r: u32) -> f64;
    /// The row at `y` px (clamped to the last row).
    fn row_at(&self, y: f64) -> u32;
    /// Where column `c` starts, px from the left (`c` in `0..=cols`).
    fn col_left(&self, c: u32) -> f64;
    /// The column at `x` px (clamped to the last column).
    fn col_at(&self, x: f64) -> u32;
    /// What cell (`r`, `c`) shows; `None` when empty.
    fn cell(&self, r: u32, c: u32) -> Option<CellView>;
    /// What editing the cell starts with (a formula's text, not its value).
    fn edit_text(&self, _r: u32, _c: u32) -> String {
        String::new()
    }
    fn col_label(&self, c: u32) -> String {
        col_name(c)
    }
    fn row_label(&self, r: u32) -> String {
        (r as u64 + 1).to_string()
    }
    /// Ctrl + arrow from (`r`, `c`) by (`dr`, `dc`): where the data ends.
    /// The default goes to the sheet's edge.
    fn data_edge(&self, r: u32, c: u32, dr: i32, dc: i32) -> (u32, u32) {
        let r = match dr.signum() {
            -1 => 0,
            1 => self.rows().saturating_sub(1),
            _ => r,
        };
        let c = match dc.signum() {
            -1 => 0,
            1 => self.cols().saturating_sub(1),
            _ => c,
        };
        (r, c)
    }
    /// The last used cell (Ctrl + End).
    fn last_used(&self) -> (u32, u32) {
        (self.rows().saturating_sub(1), self.cols().saturating_sub(1))
    }
}

/// Spreadsheet column letters: 0 is A, 25 is Z, 26 is AA.
pub fn col_name(mut c: u32) -> String {
    let mut s = Vec::new();
    loop {
        s.push(b'A' + (c % 26) as u8);
        if c < 26 {
            break;
        }
        c = c / 26 - 1;
    }
    s.reverse();
    String::from_utf8(s).unwrap()
}

/// A rectangle of cells, inclusive, `r0 <= r1` and `c0 <= c1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CellRange {
    pub r0: u32,
    pub c0: u32,
    pub r1: u32,
    pub c1: u32,
}

impl CellRange {
    pub fn of(a: (u32, u32), b: (u32, u32)) -> Self {
        Self { r0: a.0.min(b.0), c0: a.1.min(b.1), r1: a.0.max(b.0), c1: a.1.max(b.1) }
    }
    pub fn contains(&self, r: u32, c: u32) -> bool {
        (self.r0..=self.r1).contains(&r) && (self.c0..=self.c1).contains(&c)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
enum Drag {
    #[default]
    None,
    Cells,
    Cols,
    Rows,
    Resize { col: u32, x0: f32, w0: f64 },
}

/// The grid's own state, kept by the app in a `Mutable`.
#[derive(Debug, Clone, Default)]
pub struct GridState {
    /// Where the selection started, (row, column).
    pub anchor: (u32, u32),
    /// The active cell, (row, column): the selection's other corner.
    pub cursor: (u32, u32),
    /// The in-cell editor, while editing the active cell.
    pub edit: Option<TextEdit>,
    drag: Drag,
}

impl GridState {
    pub fn selection(&self) -> CellRange {
        CellRange::of(self.anchor, self.cursor)
    }
    /// Select one cell.
    pub fn select(&mut self, r: u32, c: u32) {
        self.anchor = (r, c);
        self.cursor = (r, c);
    }
}

/// What the user asked of the grid; the app applies it to its data.
#[derive(Debug, Clone, PartialEq)]
pub enum GridAction {
    /// The editor closed with this text for (`row`, `col`).
    Commit { row: u32, col: u32, text: String },
    /// Delete: empty these cells.
    Clear(CellRange),
    /// Column `col` dragged to `width` px.
    ResizeCol { col: u32, width: f64 },
    Undo,
    Redo,
    Copy(CellRange),
    Cut(CellRange),
    /// Paste at the active cell.
    Paste { row: u32, col: u32 },
}

pub type ActionFn = Rc<dyn Fn(GridAction)>;

/// The geometry one render used, for the handlers.
#[derive(Debug, Clone, Copy)]
struct Geom {
    body: Rect,
    rows: u32,
    cols: u32,
    ext: [f64; 2],
}

pub struct GridView {
    pub source: Rc<dyn GridSource>,
    pub scroll: Mutable<ScrollState>,
    pub state: Mutable<GridState>,
    pub rect: Rect,
    on_action: Option<ActionFn>,
    key: u64,
}

impl GridView {
    pub fn new(source: Rc<dyn GridSource>, scroll: Mutable<ScrollState>, state: Mutable<GridState>, rect: Rect) -> Self {
        Self { source, scroll, state, rect, on_action: None, key: 0x6772_6964 }
    }

    pub fn on_action(mut self, f: impl Fn(GridAction) + 'static) -> Self {
        self.on_action = Some(Rc::new(f));
        self
    }

    /// Keys for the grid's three viewports are `k`, `k + 1`, `k + 2`.
    pub fn key(mut self, k: u64) -> Self {
        self.key = k;
        self
    }

    /// The row header's width for rows up to `last`.
    pub fn header_width(last: u32) -> f32 {
        let digits = (last as u64 + 1).to_string().len().max(2) as f32;
        (digits * TEXT_SIZE * 0.62 + 2.0 * PAD + 6.0).ceil()
    }

    /// The tiles to send for a view at (`x`, `y`) of `w` × `h` px: those
    /// it touches plus one each side, as (first tile row, last), (first
    /// tile column, last).
    pub fn tiles(src: &dyn GridSource, x: f64, y: f64, w: f64, h: f64) -> ((u32, u32), (u32, u32)) {
        let (rows, cols) = (src.rows(), src.cols());
        if rows == 0 || cols == 0 {
            return ((1, 0), (1, 0));
        }
        let (ra, rb) = (src.row_at(y), src.row_at(y + h));
        let (ca, cb) = (src.col_at(x), src.col_at(x + w));
        let last_tr = (rows - 1) / TILE_ROWS;
        let last_tc = (cols - 1) / TILE_COLS;
        (((ra / TILE_ROWS).saturating_sub(1), (rb / TILE_ROWS + 1).min(last_tr)), ((ca / TILE_COLS).saturating_sub(1), (cb / TILE_COLS + 1).min(last_tc)))
    }

    pub fn emit(&self, ctx: &mut Ctx) -> NodeId {
        let src = self.source.clone();
        let t = ctx.theme;
        let (rows, cols) = (src.rows(), src.cols());
        let ext = [src.col_left(cols), src.row_top(rows)];
        let mut st = self.scroll.get();
        let gs = self.state.get_cloned();
        let r = self.rect;

        // Headers' size; the body is what is left.
        let (tiles_r, _) = Self::tiles(&*src, st.physics_x.y, st.physics.y, r.w() as f64, r.h() as f64);
        let header_w = Self::header_width((tiles_r.1 + 1) * TILE_ROWS);
        let body = Rect::new(r.x() + header_w, r.y() + HEADER_H, (r.w() - header_w).max(0.0), (r.h() - HEADER_H).max(0.0));
        let geom = Geom { body, rows, cols, ext };

        // The scroll state knows the extent (and keeps a grid's zoom).
        if st.extent != Some(ext) || !st.fixed_zoom {
            st.extent = Some(ext);
            st.fixed_zoom = true;
            st.physics.set_max(ext[1] - body.h() as f64);
            st.physics_x.set_max(ext[0] - body.w() as f64);
            self.scroll.set(st);
        }
        let (x, y) = (st.physics_x.y, st.physics.y);
        let (bx, by) = ((x / REBASE).floor() * REBASE, (y / REBASE).floor() * REBASE);
        let (tiles_r, tiles_c) = Self::tiles(&*src, x, y, body.w() as f64, body.h() as f64);
        let sel = gs.selection();

        let text = |size: f32, w: Weight, c: Color| TextStyle { family: font::SANS.into(), size, weight: w, color: c };
        let line = t.border_default();

        // ---- body ----
        ctx.key(self.key);
        let body_id = ctx.push(Node::Scroll {
            rect: body,
            offset: (y - by) as f32,
            zoom: 1.0,
            stretch: 1.0,
            backdrop: Some(t.bg_canvas()),
            content_h: 0.0,
            offset_x: (x - bx) as f32,
            content_w: 0.0,
        });
        {
            ctx.key(1);
            let local = Rect::new(body.x(), body.y(), (ext[0] - bx).clamp(0.0, 3.0 * REBASE) as f32, (ext[1] - by).clamp(0.0, 3.0 * REBASE) as f32);
            let page = ctx.push(Node::vstack(local, 0.0));
            ctx.set_flex(page, FlexStyle { free: true, ..FlexStyle::default() });
            // Window position of content point (px, py).
            let at = |px: f64, py: f64| (body.x() + (px - bx) as f32, body.y() + (py - by) as f32);
            if tiles_r.0 <= tiles_r.1 && tiles_c.0 <= tiles_c.1 {
                for tr in tiles_r.0..=tiles_r.1 {
                    for tc in tiles_c.0..=tiles_c.1 {
                        let (r0, r1) = (tr * TILE_ROWS, ((tr + 1) * TILE_ROWS).min(rows) - 1);
                        let (c0, c1) = (tc * TILE_COLS, ((tc + 1) * TILE_COLS).min(cols) - 1);
                        let (top, bottom) = (src.row_top(r0), src.row_top(r1 + 1));
                        let (left, right) = (src.col_left(c0), src.col_left(c1 + 1));
                        let (tx, ty) = at(left, top);
                        let (tw, th) = ((right - left) as f32, (bottom - top) as f32);
                        ctx.key(2 + ((tr as u64) << 20 | tc as u64));
                        let tile = ctx.push(Node::vstack(Rect::new(tx, ty, tw, th), 0.0));
                        ctx.set_flex(tile, FlexStyle { free: true, ..FlexStyle::default() });
                        // Grid lines: each row's bottom and each column's right edge.
                        for row in r0..=r1 {
                            let (_, yb) = at(left, src.row_top(row + 1));
                            ctx.add(Node::Rect { rect: Rect::new(tx, yb - 1.0, tw, 1.0), fill: line, radius: 0.0 });
                        }
                        for col in c0..=c1 {
                            let (xr, _) = at(src.col_left(col + 1), top);
                            ctx.add(Node::Rect { rect: Rect::new(xr - 1.0, ty, 1.0, th), fill: line, radius: 0.0 });
                        }
                        for row in r0..=r1 {
                            let (yt, yb) = (src.row_top(row), src.row_top(row + 1));
                            for col in c0..=c1 {
                                let Some(cv) = src.cell(row, col) else { continue };
                                if cv.text.is_empty() {
                                    continue;
                                }
                                let (xl, xr) = (src.col_left(col), src.col_left(col + 1));
                                let (cx, cy) = at(xl, yt);
                                let (cw, ch) = ((xr - xl) as f32, (yb - yt) as f32);
                                let style = text(TEXT_SIZE, if cv.bold { Weight::Semibold } else { Weight::Regular }, cv.color.unwrap_or(t.text_primary()));
                                let (shown, w) = fit_text(&cv, cw - 2.0 * PAD, &style);
                                let tx0 = match cv.align {
                                    CellAlign::Left => cx + PAD,
                                    CellAlign::Center => cx + (cw - w) / 2.0,
                                    CellAlign::Right => cx + cw - PAD - w,
                                };
                                let lh = TEXT_SIZE * 1.3;
                                ctx.add(Node::Text { rect: Rect::new(tx0, cy + ((ch - lh) / 2.0).max(0.0), w, lh), content: shown, style });
                            }
                        }
                        ctx.pop();
                    }
                }
            }
            // The overlay: selection, active cell, editor.
            ctx.key(u64::MAX);
            let over = ctx.push(Node::vstack(local, 0.0));
            ctx.set_flex(over, FlexStyle { free: true, ..FlexStyle::default() });
            if rows > 0 && cols > 0 {
                // The selection, cut to the resident tiles (it can be a
                // whole column of a billion rows).
                let vis = CellRange { r0: tiles_r.0 * TILE_ROWS, c0: tiles_c.0 * TILE_COLS, r1: ((tiles_r.1 + 1) * TILE_ROWS).min(rows) - 1, c1: ((tiles_c.1 + 1) * TILE_COLS).min(cols) - 1 };
                let cut = CellRange { r0: sel.r0.max(vis.r0), c0: sel.c0.max(vis.c0), r1: sel.r1.min(vis.r1), c1: sel.c1.min(vis.c1) };
                if (sel.r0 != sel.r1 || sel.c0 != sel.c1) && cut.r0 <= cut.r1 && cut.c0 <= cut.c1 {
                    let (sx, sy) = at(src.col_left(cut.c0), src.row_top(cut.r0));
                    let (ex, ey) = at(src.col_left(cut.c1 + 1), src.row_top(cut.r1 + 1));
                    let mut fill = t.accent_fill();
                    fill.a = 0.12;
                    ctx.add(Node::Rect { rect: Rect::new(sx, sy, ex - sx, ey - sy), fill, radius: 0.0 });
                }
                let (ar, ac) = gs.cursor;
                if vis.contains(ar, ac) {
                    let (sx, sy) = at(src.col_left(ac), src.row_top(ar));
                    let (ex, ey) = at(src.col_left(ac + 1), src.row_top(ar + 1));
                    let (w, h) = (ex - sx, ey - sy);
                    if let Some(ed) = &gs.edit {
                        // The editor: as wide as its text needs, from the cell.
                        let style = text(TEXT_SIZE, Weight::Regular, t.text_primary());
                        let tw = crate::text::measure(ed.text(), &style).w;
                        let ew = w.max(tw + 2.0 * PAD + 2.0);
                        ctx.add(Node::Rect { rect: Rect::new(sx, sy, ew, h), fill: t.bg_canvas(), radius: 0.0 });
                        let lh = TEXT_SIZE * 1.3;
                        let ty = sy + ((h - lh) / 2.0).max(0.0);
                        if ed.has_selection() {
                            let s = ed.selection();
                            let a = crate::text::measure(&ed.text()[..s.start], &style).w;
                            let b = crate::text::measure(&ed.text()[..s.end], &style).w;
                            ctx.add(Node::Rect { rect: Rect::new(sx + PAD + a, ty, b - a, lh), fill: t.selection_bg(), radius: 0.0 });
                        }
                        let cx = crate::text::measure(&ed.text()[..ed.caret()], &style).w;
                        ctx.add(Node::Text { rect: Rect::new(sx + PAD, ty, tw, lh), content: ed.text().to_string(), style });
                        ctx.add(Node::Rect { rect: Rect::new(sx + PAD + cx, ty, 1.5, lh), fill: t.accent_fg(), radius: 0.0 });
                        ring(ctx, Rect::new(sx, sy, ew, h), t.accent_fg());
                    } else {
                        ring(ctx, Rect::new(sx, sy, w, h), t.accent_fg());
                    }
                }
            }
            ctx.pop();
            ctx.pop();
        }
        ctx.pop();
        ctx.focusable(body_id);
        let s = self.scroll.clone();
        ctx.on_scroll(body_id, move |inp: &ScrollInput| {
            let mut st = s.get();
            st.input(inp.kind, inp.t, inp.node);
            s.set(st);
        });
        let s = self.scroll.clone();
        ctx.on_frame(body_id, move |t, node| {
            let mut st = s.get();
            let more = st.frame(t, node);
            if st != s.get() {
                s.set(st);
            }
            more
        });
        {
            let (src, sc, gst, act) = (src.clone(), self.scroll.clone(), self.state.clone(), self.on_action.clone());
            ctx.on_drag(body_id, move |phase, p| {
                let st = sc.get();
                let cx = st.physics_x.y + (p.x - geom.body.x()) as f64;
                let cy = st.physics.y + (p.y - geom.body.y()) as f64;
                if geom.rows == 0 || geom.cols == 0 {
                    return;
                }
                let cell = (src.row_at(cy.max(0.0)), src.col_at(cx.max(0.0)));
                let mut g = gst.get_cloned();
                match phase {
                    DragPhase::Start => {
                        commit(&mut g, &act);
                        g.select(cell.0, cell.1);
                        g.drag = Drag::Cells;
                    }
                    DragPhase::Move if g.drag == Drag::Cells => g.cursor = cell,
                    DragPhase::End => g.drag = Drag::None,
                    _ => return,
                }
                gst.set(g);
            });
        }
        {
            let (src, sc, gst, act) = (src.clone(), self.scroll.clone(), self.state.clone(), self.on_action.clone());
            ctx.on_key(body_id, move |ev| {
                let Event::Key { kind, key, modifiers, chars } = ev else { return };
                if !kind.pressed() {
                    return;
                }
                let mut g = gst.get_cloned();
                let mut st = sc.get();
                if on_key(&*src, &geom, &mut g, &mut st, *key, *modifiers, chars, &act) {
                    gst.set(g);
                    if st != sc.get() {
                        sc.set(st);
                    }
                }
            });
        }

        // ---- column headers: a viewport that moves across only ----
        let hstyle = text(type_size::XS, Weight::Medium, t.text_secondary());
        let hstyle_on = text(type_size::XS, Weight::Semibold, t.accent_fg());
        ctx.key(self.key + 1);
        let ch_rect = Rect::new(body.x(), r.y(), body.w(), HEADER_H);
        let colh = ctx.push(Node::Scroll { rect: ch_rect, offset: 0.0, zoom: 1.0, stretch: 1.0, backdrop: Some(t.bg_surface()), content_h: 0.0, offset_x: (x - bx) as f32, content_w: 0.0 });
        {
            ctx.key(1);
            let page = ctx.push(Node::vstack(Rect::new(ch_rect.x(), ch_rect.y(), ((ext[0] - bx).clamp(0.0, 3.0 * REBASE)) as f32, HEADER_H), 0.0));
            ctx.set_flex(page, FlexStyle { free: true, ..FlexStyle::default() });
            if cols > 0 && tiles_c.0 <= tiles_c.1 {
                for c in tiles_c.0 * TILE_COLS..=((tiles_c.1 + 1) * TILE_COLS).min(cols) - 1 {
                    let (xl, xr) = (src.col_left(c), src.col_left(c + 1));
                    let cx = ch_rect.x() + (xl - bx) as f32;
                    let w = (xr - xl) as f32;
                    ctx.key(c as u64 + 1);
                    let on = c >= sel.c0 && c <= sel.c1;
                    let cell = ctx.push(Node::vstack(Rect::new(cx, ch_rect.y(), w, HEADER_H), 0.0));
                    ctx.set_flex(cell, FlexStyle { free: true, ..FlexStyle::default() });
                    if on {
                        ctx.add(Node::Rect { rect: Rect::new(cx, ch_rect.y(), w, HEADER_H), fill: t.accent_bg(), radius: 0.0 });
                    }
                    let label = src.col_label(c);
                    let st = if on { hstyle_on.clone() } else { hstyle.clone() };
                    let lw = crate::text::measure(&label, &st).w;
                    if lw <= w {
                        ctx.add(Node::Text { rect: Rect::new(cx + (w - lw) / 2.0, ch_rect.y() + (HEADER_H - 15.0) / 2.0, lw, 15.0), content: label, style: st });
                    }
                    ctx.add(Node::Rect { rect: Rect::new(cx + w - 1.0, ch_rect.y(), 1.0, HEADER_H), fill: line, radius: 0.0 });
                    ctx.pop();
                }
            }
            ctx.pop();
        }
        ctx.pop();
        {
            let (src, sc, gst, act) = (src.clone(), self.scroll.clone(), self.state.clone(), self.on_action.clone());
            ctx.on_drag(colh, move |phase, p| {
                if geom.cols == 0 || geom.rows == 0 {
                    return;
                }
                let st = sc.get();
                let cx = st.physics_x.y + (p.x - geom.body.x()) as f64;
                let c = src.col_at(cx.max(0.0));
                let mut g = gst.get_cloned();
                match (phase, g.drag) {
                    (DragPhase::Start, _) => {
                        commit(&mut g, &act);
                        // Near a column's right edge: resize that column.
                        let edge_of = |c: u32| src.col_left(c + 1) - cx;
                        let grip = if edge_of(c).abs() <= RESIZE_GRIP as f64 {
                            Some(c)
                        } else if c > 0 && (cx - src.col_left(c)).abs() <= RESIZE_GRIP as f64 {
                            Some(c - 1)
                        } else {
                            None
                        };
                        match grip {
                            Some(k) => g.drag = Drag::Resize { col: k, x0: p.x, w0: src.col_left(k + 1) - src.col_left(k) },
                            None => {
                                g.anchor = (0, c);
                                g.cursor = (geom.rows - 1, c);
                                g.drag = Drag::Cols;
                            }
                        }
                    }
                    (DragPhase::Move, Drag::Resize { col, x0, w0 }) => {
                        if let Some(a) = &act {
                            a(GridAction::ResizeCol { col, width: (w0 + (p.x - x0) as f64).max(MIN_COL_W) });
                        }
                        return;
                    }
                    (DragPhase::Move, Drag::Cols) => g.cursor = (geom.rows - 1, c),
                    (DragPhase::End, _) => g.drag = Drag::None,
                    _ => return,
                }
                gst.set(g);
            });
        }

        // ---- row headers: a viewport that moves down only ----
        ctx.key(self.key + 2);
        let rh_rect = Rect::new(r.x(), body.y(), header_w, body.h());
        let rowh = ctx.push(Node::Scroll { rect: rh_rect, offset: (y - by) as f32, zoom: 1.0, stretch: 1.0, backdrop: Some(t.bg_surface()), content_h: 0.0, offset_x: 0.0, content_w: 0.0 });
        {
            ctx.key(1);
            let page = ctx.push(Node::vstack(Rect::new(rh_rect.x(), rh_rect.y(), header_w, ((ext[1] - by).clamp(0.0, 3.0 * REBASE)) as f32), 0.0));
            ctx.set_flex(page, FlexStyle { free: true, ..FlexStyle::default() });
            if rows > 0 && tiles_r.0 <= tiles_r.1 {
                for row in tiles_r.0 * TILE_ROWS..=((tiles_r.1 + 1) * TILE_ROWS).min(rows) - 1 {
                    let (yt, yb) = (src.row_top(row), src.row_top(row + 1));
                    let cy = rh_rect.y() + (yt - by) as f32;
                    let h = (yb - yt) as f32;
                    ctx.key(row as u64 + 1);
                    let on = row >= sel.r0 && row <= sel.r1;
                    let cell = ctx.push(Node::vstack(Rect::new(rh_rect.x(), cy, header_w, h), 0.0));
                    ctx.set_flex(cell, FlexStyle { free: true, ..FlexStyle::default() });
                    if on {
                        ctx.add(Node::Rect { rect: Rect::new(rh_rect.x(), cy, header_w, h), fill: t.accent_bg(), radius: 0.0 });
                    }
                    let label = src.row_label(row);
                    let st = if on { hstyle_on.clone() } else { hstyle.clone() };
                    let lw = crate::text::measure(&label, &st).w;
                    ctx.add(Node::Text { rect: Rect::new(rh_rect.x() + header_w - PAD - 2.0 - lw, cy + ((h - 15.0) / 2.0).max(0.0), lw, 15.0), content: label, style: st });
                    ctx.add(Node::Rect { rect: Rect::new(rh_rect.x(), cy + h - 1.0, header_w, 1.0), fill: line, radius: 0.0 });
                    ctx.pop();
                }
            }
            ctx.pop();
        }
        ctx.pop();
        {
            let (src, sc, gst, act) = (src.clone(), self.scroll.clone(), self.state.clone(), self.on_action.clone());
            ctx.on_drag(rowh, move |phase, p| {
                if geom.cols == 0 || geom.rows == 0 {
                    return;
                }
                let st = sc.get();
                let cy = st.physics.y + (p.y - geom.body.y()) as f64;
                let row = src.row_at(cy.max(0.0));
                let mut g = gst.get_cloned();
                match phase {
                    DragPhase::Start => {
                        commit(&mut g, &act);
                        g.anchor = (row, 0);
                        g.cursor = (row, geom.cols - 1);
                        g.drag = Drag::Rows;
                    }
                    DragPhase::Move if g.drag == Drag::Rows => g.cursor = (row, geom.cols - 1),
                    DragPhase::End => g.drag = Drag::None,
                    _ => return,
                }
                gst.set(g);
            });
        }

        // ---- the corner and the header edges ----
        let corner = ctx.add(Node::Rect { rect: Rect::new(r.x(), r.y(), header_w, HEADER_H), fill: t.bg_surface(), radius: 0.0 });
        {
            let gst = self.state.clone();
            ctx.on_click(corner, move || {
                // Select everything.
                let mut g = gst.get_cloned();
                if geom.rows > 0 && geom.cols > 0 {
                    g.anchor = (0, 0);
                    g.cursor = (geom.rows - 1, geom.cols - 1);
                    gst.set(g);
                }
            });
        }
        ctx.add(Node::Rect { rect: Rect::new(r.x(), body.y() - 1.0, r.w(), 1.0), fill: t.border_strong(), radius: 0.0 });
        ctx.add(Node::Rect { rect: Rect::new(body.x() - 1.0, r.y(), 1.0, r.h()), fill: t.border_strong(), radius: 0.0 });
        body_id
    }
}

impl View for GridView {
    fn render(&self, ctx: &mut Ctx) {
        self.emit(ctx);
    }
}

/// A 2 px ring inside `r`.
fn ring(ctx: &mut Ctx, r: Rect, c: Color) {
    let k = 2.0;
    ctx.add(Node::Rect { rect: Rect::new(r.x(), r.y(), r.w(), k), fill: c, radius: 0.0 });
    ctx.add(Node::Rect { rect: Rect::new(r.x(), r.y() + r.h() - k, r.w(), k), fill: c, radius: 0.0 });
    ctx.add(Node::Rect { rect: Rect::new(r.x(), r.y(), k, r.h()), fill: c, radius: 0.0 });
    ctx.add(Node::Rect { rect: Rect::new(r.x() + r.w() - k, r.y(), k, r.h()), fill: c, radius: 0.0 });
}

/// The text a cell shows in `avail` px and its width: whole when it
/// fits; a number too wide becomes `#`s (a cut number would lie); other
/// text is cut with an ellipsis.
fn fit_text(cv: &CellView, avail: f32, style: &TextStyle) -> (String, f32) {
    let est = cv.text.chars().count() as f32 * style.size * 0.6;
    let w = if est < avail * 0.7 && !matches!(cv.align, CellAlign::Right | CellAlign::Center) {
        // Clearly fits and left-aligned: no need to measure.
        return (cv.text.clone(), est);
    } else {
        crate::text::measure(&cv.text, style).w
    };
    if w <= avail {
        return (cv.text.clone(), w);
    }
    if avail <= 0.0 {
        return (String::new(), 0.0);
    }
    if cv.numeric {
        let one = crate::text::measure("#", style).w.max(1.0);
        let n = (avail / one).floor().max(1.0) as usize;
        let s = "#".repeat(n);
        return (s, n as f32 * one);
    }
    // Cut by the measured width per character, then check.
    let chars: Vec<char> = cv.text.chars().collect();
    let mut n = ((avail / w) * chars.len() as f32) as usize;
    loop {
        let s: String = chars[..n.min(chars.len())].iter().collect::<String>() + "…";
        let sw = crate::text::measure(&s, style).w;
        if sw <= avail || n == 0 {
            return if n == 0 { (String::new(), 0.0) } else { (s, sw) };
        }
        n -= 1;
    }
}

/// Close the editor, sending what it holds.
fn commit(g: &mut GridState, act: &Option<ActionFn>) {
    if let Some(ed) = g.edit.take() {
        if let Some(a) = act {
            a(GridAction::Commit { row: g.cursor.0, col: g.cursor.1, text: ed.text().to_string() });
        }
    }
}

/// Bring the active cell into view.
fn reveal(src: &dyn GridSource, geom: &Geom, st: &mut ScrollState, cell: (u32, u32)) {
    let (r, c) = cell;
    st.physics.set_max(geom.ext[1] - geom.body.h() as f64);
    st.physics_x.set_max(geom.ext[0] - geom.body.w() as f64);
    let (top, bottom) = (src.row_top(r), src.row_top(r + 1));
    let (h, w) = (geom.body.h() as f64, geom.body.w() as f64);
    if top < st.physics.y {
        st.physics.stop();
        st.physics.set_position(top);
    } else if bottom > st.physics.y + h {
        st.physics.stop();
        st.physics.set_position(bottom - h);
    }
    let (left, right) = (src.col_left(c), src.col_left(c + 1));
    if left < st.physics_x.y {
        st.physics_x.stop();
        st.physics_x.set_position(left);
    } else if right > st.physics_x.y + w {
        st.physics_x.stop();
        st.physics_x.set_position((right - w).min(left));
    }
}

/// One key on the grid. Returns whether anything changed.
#[allow(clippy::too_many_arguments)]
fn on_key(src: &dyn GridSource, geom: &Geom, g: &mut GridState, st: &mut ScrollState, key: Key, m: Modifiers, chars: &str, act: &Option<ActionFn>) -> bool {
    if geom.rows == 0 || geom.cols == 0 {
        return false;
    }
    let send = |a: GridAction| {
        if let Some(f) = act {
            f(a)
        }
    };
    let (last_r, last_c) = (geom.rows - 1, geom.cols - 1);
    let step = |(r, c): (u32, u32), dr: i64, dc: i64| -> (u32, u32) { ((r as i64 + dr).clamp(0, last_r as i64) as u32, (c as i64 + dc).clamp(0, last_c as i64) as u32) };
    let cmd = m.ctrl || m.meta;

    // The editor has the keys, except the ones that end it.
    if let Some(ed) = &mut g.edit {
        let finish = |g: &mut GridState, st: &mut ScrollState, dr: i64, dc: i64| {
            commit(g, act);
            let to = step(g.cursor, dr, dc);
            g.select(to.0, to.1);
            reveal(src, geom, st, to);
        };
        match key {
            Key::Escape => {
                g.edit = None;
                return true;
            }
            Key::Enter => {
                finish(g, st, if m.shift { -1 } else { 1 }, 0);
                return true;
            }
            Key::Tab => {
                finish(g, st, 0, if m.shift { -1 } else { 1 });
                return true;
            }
            _ => {
                return match ed.key(key, m, chars) {
                    KeyOutcome::Changed => true,
                    KeyOutcome::Submit => {
                        finish(g, st, 1, 0);
                        true
                    }
                    _ => false,
                };
            }
        }
    }

    let page = |st: &ScrollState| -> i64 {
        let h = geom.body.h() as f64;
        let a = src.row_at(st.physics.y);
        let b = src.row_at(st.physics.y + h);
        (b as i64 - a as i64).max(1)
    };
    let to = match key {
        Key::Up | Key::Down | Key::Left | Key::Right => {
            let (dr, dc) = match key {
                Key::Up => (-1, 0),
                Key::Down => (1, 0),
                Key::Left => (0, -1),
                _ => (0, 1),
            };
            Some(if cmd { src.data_edge(g.cursor.0, g.cursor.1, dr, dc) } else { step(g.cursor, dr as i64, dc as i64) })
        }
        Key::PageUp => Some(step(g.cursor, -page(st), 0)),
        Key::PageDown => Some(step(g.cursor, page(st), 0)),
        Key::Home if cmd => Some((0, 0)),
        Key::Home => Some((g.cursor.0, 0)),
        Key::End if cmd => Some(src.last_used()),
        Key::Tab => {
            let to = step(g.cursor, 0, if m.shift { -1 } else { 1 });
            g.select(to.0, to.1);
            reveal(src, geom, st, to);
            return true;
        }
        Key::Enter => {
            let mut ed = TextEdit::new(&src.edit_text(g.cursor.0, g.cursor.1));
            ed.set_caret(ed.text().len(), false);
            g.anchor = g.cursor;
            g.edit = Some(ed);
            return true;
        }
        Key::Delete | Key::Backspace => {
            send(GridAction::Clear(g.selection()));
            return true;
        }
        Key::Char if cmd => {
            match chars.to_ascii_lowercase().as_str() {
                "z" if m.shift => send(GridAction::Redo),
                "z" => send(GridAction::Undo),
                "y" => send(GridAction::Redo),
                "c" => send(GridAction::Copy(g.selection())),
                "x" => send(GridAction::Cut(g.selection())),
                "v" => send(GridAction::Paste { row: g.selection().r0, col: g.selection().c0 }),
                "a" => {
                    g.anchor = (0, 0);
                    g.cursor = (last_r, last_c);
                    return true;
                }
                _ => return false,
            }
            return true;
        }
        Key::Char if !chars.is_empty() && !chars.chars().any(char::is_control) => {
            // Typing replaces the active cell's content.
            let mut ed = TextEdit::new("");
            ed.insert(chars);
            g.anchor = g.cursor;
            g.edit = Some(ed);
            reveal(src, geom, st, g.cursor);
            return true;
        }
        _ => None,
    };
    let Some(to) = to else { return false };
    if m.shift {
        g.cursor = to;
    } else {
        g.select(to.0, to.1);
    }
    reveal(src, geom, st, to);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use crate::event::{KeyEventKind, ScrollPhase};
    use crate::geom::Point;
    use crate::node::NodeDelta;
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;
    use std::sync::Arc;

    /// A sheet of `rows` rows of 20 px and 64 px columns, cells showing
    /// their address, edits kept in a map.
    struct Sheet {
        rows: u32,
        cols: u32,
        widths: RefCell<HashMap<u32, f64>>,
        cells: RefCell<HashMap<(u32, u32), String>>,
    }

    impl Sheet {
        fn new(rows: u32, cols: u32) -> Self {
            Self { rows, cols, widths: Default::default(), cells: Default::default() }
        }
        fn w(&self, c: u32) -> f64 {
            *self.widths.borrow().get(&c).unwrap_or(&64.0)
        }
    }

    impl GridSource for Sheet {
        fn rows(&self) -> u32 {
            self.rows
        }
        fn cols(&self) -> u32 {
            self.cols
        }
        fn row_top(&self, r: u32) -> f64 {
            r as f64 * 20.0
        }
        fn row_at(&self, y: f64) -> u32 {
            ((y / 20.0) as u32).min(self.rows - 1)
        }
        fn col_left(&self, c: u32) -> f64 {
            // Few overrides in tests: sum them.
            let extra: f64 = self.widths.borrow().iter().filter(|(k, _)| **k < c).map(|(_, w)| w - 64.0).sum();
            c as f64 * 64.0 + extra
        }
        fn col_at(&self, x: f64) -> u32 {
            let mut c = 0;
            while c + 1 < self.cols && self.col_left(c + 1) <= x {
                c += 1;
            }
            c
        }
        fn cell(&self, r: u32, c: u32) -> Option<CellView> {
            if let Some(s) = self.cells.borrow().get(&(r, c)) {
                return Some(CellView::text(s.clone()));
            }
            (r % 3 == 0).then(|| CellView::number(format!("{}", r as u64 * 1000 + c as u64)))
        }
        fn edit_text(&self, r: u32, c: u32) -> String {
            self.cells.borrow().get(&(r, c)).cloned().unwrap_or_default()
        }
    }

    struct Host {
        grid: Rc<Sheet>,
        scroll: Mutable<ScrollState>,
        state: Mutable<GridState>,
        log: Rc<RefCell<Vec<GridAction>>>,
    }

    impl View for Host {
        fn render(&self, ctx: &mut Ctx) {
            let log = self.log.clone();
            let grid = self.grid.clone();
            GridView::new(self.grid.clone(), self.scroll.clone(), self.state.clone(), Rect::new(0.0, 0.0, 800.0, 600.0))
                .on_action(move |a| {
                    match &a {
                        GridAction::Commit { row, col, text } => {
                            grid.cells.borrow_mut().insert((*row, *col), text.clone());
                        }
                        GridAction::ResizeCol { col, width } => {
                            grid.widths.borrow_mut().insert(*col, *width);
                        }
                        _ => {}
                    }
                    log.borrow_mut().push(a)
                })
                .emit(ctx);
        }
    }

    fn host(rows: u32, cols: u32) -> (App<Host>, Rc<Sheet>, Mutable<ScrollState>, Mutable<GridState>, Rc<RefCell<Vec<GridAction>>>) {
        let dirty = Arc::new(AtomicBool::new(true));
        let scroll = Mutable::with_dirty(ScrollState::default(), dirty.clone());
        let state = Mutable::with_dirty(GridState::default(), dirty.clone());
        let grid = Rc::new(Sheet::new(rows, cols));
        let log = Rc::new(RefCell::new(Vec::new()));
        let app = App::new_with_flag(Host { grid: grid.clone(), scroll: scroll.clone(), state: state.clone(), log: log.clone() }, dirty);
        (app, grid, scroll, state, log)
    }

    fn key(app: &mut App<Host>, k: Key, chars: &str, m: Modifiers) {
        app.handle_event(Event::Key { kind: KeyEventKind::Down, key: k, modifiers: m, chars: chars.into() });
        app.tick();
    }

    fn texts(d: &[NodeDelta]) -> usize {
        d.iter().filter(|d| matches!(d, NodeDelta::Added { node: Node::Text { .. }, .. })).count()
    }

    #[test]
    fn column_letters() {
        assert_eq!(col_name(0), "A");
        assert_eq!(col_name(25), "Z");
        assert_eq!(col_name(26), "AA");
        assert_eq!(col_name(16383), "XFD");
    }

    #[test]
    fn a_billion_rows_send_a_few_tiles_wherever_it_is() {
        let (mut app, grid, scroll, _, _) = host(1 << 30, 16384);
        grid.cells.borrow_mut().insert((999_999_000, 9_000), "here".into());
        let first = app.tick();
        let n0 = first.len();
        assert!(n0 < 3000, "{n0} nodes");
        // Near the bottom of a billion rows: still a few tiles, and the
        // coordinates sent are small (rebased).
        let mut st = scroll.get();
        st.physics.set_max(f64::MAX);
        st.physics_x.set_max(f64::MAX);
        st.physics.set_position(20.0 * 999_999_000.0);
        st.physics_x.set_position(64.0 * 9_000.0);
        scroll.set(st);
        let d = app.tick();
        assert!(app.tree().iter().count() < 3000, "{} resident", app.tree().iter().count());
        let max_y = app.tree().iter().map(|(_, n)| n.rect().y().abs()).fold(0.0f32, f32::max);
        assert!(max_y < 3.0 * REBASE as f32, "coordinates stay small: {max_y}");
        // frescod refuses a group translated past ±32,768 device px: the
        // viewports' offsets stay well inside it at any scale up to 3.
        for (_, n) in app.tree().iter() {
            if let Node::Scroll { offset, offset_x, .. } = n {
                assert!(*offset * 3.0 <= 32768.0 && *offset_x * 3.0 <= 32768.0, "offset {offset}, {offset_x}");
            }
        }
        // The cells there are the right ones.
        assert!(d.iter().any(|d| matches!(d, NodeDelta::Added { node: Node::Text { content, .. }, .. } if content == "here")), "the cell at row 999,999,001 sent");
        let row_label = "999999001".to_string();
        assert!(app.tree().iter().any(|(_, n)| matches!(n, Node::Text { content, .. } if *content == row_label)), "its row header");
    }

    #[test]
    fn a_scroll_frame_changes_only_the_three_viewports() {
        let (mut app, _, scroll, _, _) = host(1_000_000, 100);
        app.tick();
        // Scroll by less than a tile, both ways.
        let mut st = scroll.get();
        st.physics.set_position(7.0);
        st.physics_x.set_position(5.0);
        scroll.set(st);
        let d = app.tick();
        assert_eq!(d.len(), 3, "{d:?}");
        assert!(d.iter().all(|d| matches!(d, NodeDelta::Changed { node: Node::Scroll { .. }, .. })));
        // A fling over many tiles: tiles come and go, the scene stays small.
        app.handle_event(Event::Scroll { at: Some(Point::new(400.0, 300.0)), dx: 0.0, dy: 0.0, modifiers: Modifiers::default(), phase: ScrollPhase::Begin });
        for i in 0..6 {
            app.set_time(i as f64 * 8.0);
            app.handle_event(Event::Scroll { at: Some(Point::new(400.0, 300.0)), dx: 0.0, dy: 60.0, modifiers: Modifiers::default(), phase: ScrollPhase::Update });
        }
        app.set_time(48.0);
        app.handle_event(Event::Scroll { at: Some(Point::new(400.0, 300.0)), dx: 0.0, dy: 0.0, modifiers: Modifiers::default(), phase: ScrollPhase::End });
        let mut t = 48.0;
        let mut most = 0;
        while app.wants_frame() && t < 5000.0 {
            t += 1000.0 / 60.0;
            app.set_time(t);
            app.frame();
            app.tick();
            most = most.max(app.tree().iter().count());
        }
        assert!(scroll.get().physics.y > 2000.0, "flung to {}", scroll.get().physics.y);
        assert!(most < 4000, "at most {most} nodes resident");
    }

    #[test]
    fn keys_move_extend_and_edit() {
        let (mut app, grid, _, state, log) = host(100, 20);
        app.tick();
        // Focus the grid with a press on B2 (row 1, col 1).
        let hw = GridView::header_width(TILE_ROWS * 2);
        let p = Point::new(hw + 64.0 + 10.0, HEADER_H + 20.0 + 5.0);
        app.handle_event(Event::PointerDown { at: p });
        app.handle_event(Event::PointerUp { at: p });
        app.tick();
        assert_eq!(state.get_cloned().cursor, (1, 1));
        let none = Modifiers::default();
        let shift = Modifiers { shift: true, ..Default::default() };
        key(&mut app, Key::Right, "", none);
        key(&mut app, Key::Down, "", shift);
        key(&mut app, Key::Down, "", shift);
        let g = state.get_cloned();
        assert_eq!(g.selection(), CellRange { r0: 1, c0: 2, r1: 3, c1: 2 });
        // Typing starts an edit; Enter commits and moves down.
        key(&mut app, Key::Char, "4", none);
        key(&mut app, Key::Char, "2", none);
        assert_eq!(state.get_cloned().edit.as_ref().map(|e| e.text().to_string()), Some("42".into()));
        key(&mut app, Key::Enter, "", none);
        assert_eq!(grid.cells.borrow().get(&(3, 2)).map(String::as_str), Some("42"));
        assert_eq!(state.get_cloned().cursor, (4, 2));
        assert!(matches!(log.borrow().last(), Some(GridAction::Commit { row: 3, col: 2, .. })));
        // Enter edits what is there; Escape leaves it.
        key(&mut app, Key::Up, "", none);
        key(&mut app, Key::Enter, "", none);
        assert_eq!(state.get_cloned().edit.as_ref().map(|e| e.text().to_string()), Some("42".into()));
        key(&mut app, Key::Backspace, "", none);
        key(&mut app, Key::Escape, "", none);
        assert!(state.get_cloned().edit.is_none());
        assert_eq!(grid.cells.borrow().get(&(3, 2)).map(String::as_str), Some("42"));
        // Delete clears the selection; Ctrl+Z asks for undo.
        key(&mut app, Key::Delete, "", none);
        assert_eq!(log.borrow().last(), Some(&GridAction::Clear(CellRange { r0: 3, c0: 2, r1: 3, c1: 2 })));
        key(&mut app, Key::Char, "z", Modifiers { ctrl: true, ..Default::default() });
        assert_eq!(log.borrow().last(), Some(&GridAction::Undo));
    }

    #[test]
    fn moving_past_the_view_scrolls_to_the_active_cell() {
        let (mut app, _, scroll, state, _) = host(10_000, 200);
        app.tick();
        let mut g = state.get_cloned();
        g.select(0, 0);
        state.set(g);
        app.tick();
        let hw = GridView::header_width(TILE_ROWS * 2);
        let p = Point::new(hw + 5.0, HEADER_H + 5.0);
        app.handle_event(Event::PointerDown { at: p });
        app.handle_event(Event::PointerUp { at: p });
        let none = Modifiers::default();
        for _ in 0..40 {
            key(&mut app, Key::Down, "", none);
        }
        let st = scroll.get();
        let body_h = 600.0 - HEADER_H as f64;
        assert_eq!(st.physics.y, 41.0 * 20.0 - body_h, "the active row's bottom at the view's bottom");
        key(&mut app, Key::End, "", Modifiers { ctrl: true, ..Default::default() });
        assert_eq!(state.get_cloned().cursor, (9_999, 199));
        let st = scroll.get();
        assert_eq!(st.physics.y, 10_000.0 * 20.0 - body_h);
        key(&mut app, Key::Home, "", Modifiers { ctrl: true, ..Default::default() });
        assert_eq!((scroll.get().physics.y, scroll.get().physics_x.y), (0.0, 0.0));
    }

    #[test]
    fn dragging_selects_and_the_header_edge_resizes() {
        let (mut app, grid, _, state, _) = host(100, 20);
        app.tick();
        let hw = GridView::header_width(TILE_ROWS * 2);
        let cell = |r: f32, c: f32| Point::new(hw + c * 64.0 + 10.0, HEADER_H + r * 20.0 + 5.0);
        app.handle_event(Event::PointerDown { at: cell(1.0, 1.0) });
        app.handle_event(Event::PointerMove { at: cell(4.0, 3.0) });
        app.handle_event(Event::PointerUp { at: cell(4.0, 3.0) });
        app.tick();
        assert_eq!(state.get_cloned().selection(), CellRange { r0: 1, c0: 1, r1: 4, c1: 3 });
        // Column B's right edge in the header, dragged 30 px right.
        let edge = Point::new(hw + 128.0 - 1.0, HEADER_H / 2.0);
        app.handle_event(Event::PointerDown { at: edge });
        app.handle_event(Event::PointerMove { at: Point::new(edge.x + 30.0, edge.y) });
        app.handle_event(Event::PointerUp { at: Point::new(edge.x + 30.0, edge.y) });
        app.tick();
        assert_eq!(grid.w(1), 94.0);
        // A press on a header selects the column.
        // (C now starts at 64 + 94 = 158.)
        app.handle_event(Event::PointerDown { at: Point::new(hw + 158.0 + 30.0, HEADER_H / 2.0) });
        app.handle_event(Event::PointerUp { at: Point::new(hw + 158.0 + 30.0, HEADER_H / 2.0) });
        app.tick();
        assert_eq!(state.get_cloned().selection(), CellRange { r0: 0, c0: 2, r1: 99, c1: 2 });
    }

    #[test]
    fn numbers_too_wide_show_hashes_and_text_is_cut() {
        let style = TextStyle { family: font::SANS.into(), size: 10.0, weight: Weight::Regular, color: Color::TRANSPARENT };
        crate::text::clear_measurer();
        let (s, w) = fit_text(&CellView::number("123456789012"), 30.0, &style);
        assert!(s.chars().all(|c| c == '#') && w <= 30.0, "{s}");
        let (s, w) = fit_text(&CellView::text("a long piece of text"), 40.0, &style);
        assert!(s.ends_with('…') && w <= 40.0, "{s}");
        let (s, _) = fit_text(&CellView::text("ok"), 40.0, &style);
        assert_eq!(s, "ok");
        let _ = texts(&[]);
    }
}
