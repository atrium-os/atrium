//! Drawing the Applications view (docs/spec/forum.md §6.1) with Pergola: the
//! filter field, the grid of app tiles, page dots, and a line for what the
//! launch said. The model is `apps`; the window and its loop are the dock's
//! binary, which drains [`AppsView::actions`] after each event.
//!
//! Sizes: layout in `em` (grows with the text size), glyphs handed over in
//! points (the renderer applies the text size to them).

use std::cell::RefCell;
use std::rc::Rc;

use pergola::geom::Rect;
use pergola::node::Node;
use pergola::text_edit::TextEdit;
use pergola::theme::{font, radius, type_size, Weight};
use pergola::view::{Ctx, View};
use pergola::widgets::{Glyph, TextField};
use pergola::{Color, Mutable, NodeId, TextStyle};

use crate::apps::{filter, Grid};
use crate::icons::{self, AppIcon};
use crate::pins::Pins;
use crate::AppEntry;

fn em(pt: f32) -> f32 {
    pergola::theme::size::em(pt)
}

/// A grid cell, at 100% text.
pub fn cell_w() -> f32 { em(116.0) }
pub fn cell_h() -> f32 { em(120.0) }
/// The rounded plate an icon sits on.
pub fn plate() -> f32 { em(64.0) }
/// The icon glyph, in points (visual language §9, `icon-xl`).
pub const GLYPH_PT: f32 = 32.0;
/// Space around the grid.
pub fn margin() -> f32 { em(32.0) }
pub fn field_w() -> f32 { em(420.0) }
pub fn field_top() -> f32 { em(28.0) }
/// From the field's bottom to the grid's top.
pub fn grid_gap() -> f32 { em(28.0) }
/// The dots row and the message line, under the grid.
pub fn foot_h() -> f32 { em(56.0) }
/// Never more columns than this, however wide the screen.
pub const MAX_COLS: usize = 8;

const PUSH_PIN: char = '\u{e3e2}';
const PUSH_PIN_SLASH: char = '\u{e3e4}';
const MAGNIFYING_GLASS: char = '\u{e30c}';

/// What the person asked for; the loop carries it out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Launch(String),
    TogglePin(String),
    Close,
}

/// A line under the grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub text: String,
    pub error: bool,
}

#[derive(Debug, Clone, Default)]
pub struct AppsState {
    /// What the view lists (`apps::listed`).
    pub apps: Vec<AppEntry>,
    /// The selection, as a position in the current matches.
    pub selected: usize,
    pub pins: Pins,
    pub note: Option<Note>,
    /// The app whose launch is being asked for (its tile shows it).
    pub launching: Option<String>,
}

/// Where things are, for a window `w` x `h` (points): the grid's area, its
/// fit, and where the grid starts so it sits centred.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layout {
    pub grid: Grid,
    pub x0: f32,
    pub y0: f32,
    pub field: Rect,
    pub foot_y: f32,
}

pub fn layout(w: f32, h: f32) -> Layout {
    let fw = field_w().min(w - 2.0 * margin()).max(em(160.0));
    let field = Rect::new((w - fw) / 2.0, field_top(), fw, pergola::theme::size::input_height());
    let top = field.y() + field.h() + grid_gap();
    let foot_y = h - foot_h();
    let grid = Grid::fit(w - 2.0 * margin(), foot_y - top, cell_w(), cell_h(), MAX_COLS);
    let x0 = (w - grid.cols as f32 * cell_w()) / 2.0;
    Layout { grid, x0, y0: top, field, foot_y }
}

impl Layout {
    /// The cell of the `k`-th item on its page.
    pub fn cell(&self, k: usize) -> Rect {
        let (r, c) = (k / self.grid.cols, k % self.grid.cols);
        Rect::new(self.x0 + c as f32 * cell_w(), self.y0 + r as f32 * cell_h(), cell_w(), cell_h())
    }

    /// Which item on page `page` is under (`x`, `y`), if any of `n`.
    pub fn item_at(&self, page: usize, n: usize, x: f32, y: f32) -> Option<usize> {
        if x < self.x0 || y < self.y0 {
            return None;
        }
        let (c, r) = (((x - self.x0) / cell_w()) as usize, ((y - self.y0) / cell_h()) as usize);
        if c >= self.grid.cols || r >= self.grid.rows {
            return None;
        }
        let i = page * self.grid.per_page() + r * self.grid.cols + c;
        (i < n).then_some(i)
    }
}

pub struct AppsView {
    pub state: Mutable<AppsState>,
    pub query: Mutable<TextEdit>,
    pub size: Mutable<(f32, f32)>,
    pub actions: Rc<RefCell<Vec<Action>>>,
    /// The size it asks the WM for at least (points): the screen, so the
    /// dialog rule gives it the work area.
    pub min: (f32, f32),
}

impl AppsView {
    /// `dirty` is the app's flag: every cell here trips it on a change.
    pub fn new(state: AppsState, size: (f32, f32), dirty: std::sync::Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self {
            state: Mutable::with_dirty(state, dirty.clone()),
            query: Mutable::with_dirty(TextEdit::new(""), dirty.clone()),
            size: Mutable::with_dirty(size, dirty),
            actions: Rc::new(RefCell::new(Vec::new())),
            min: (0.0, 0.0),
        }
    }

    /// The current matches (indices into `state.apps`).
    pub fn matches(&self) -> Vec<usize> {
        let s = self.state.get_cloned();
        filter(&s.apps, self.query.get_cloned().text())
    }

    fn act(&self, a: Action) -> impl Fn() + 'static {
        let q = self.actions.clone();
        move || q.borrow_mut().push(a.clone())
    }
}

fn sans(size: f32, weight: Weight, color: Color) -> TextStyle {
    TextStyle { family: font::SANS.into(), size, weight, color }
}

/// A line of text whose middle is at `cy`, starting at `x`.
fn text(ctx: &mut Ctx, x: f32, cy: f32, s: &str, size: f32, weight: Weight, color: Color) -> NodeId {
    let h = em(size) * 1.5;
    ctx.add(Node::Text { rect: Rect::new(x, cy - h / 2.0, 0.0, h), content: s.into(), style: sans(size, weight, color) })
}

fn width(s: &str, size: f32, weight: Weight) -> f32 {
    Node::measure_text(s, &sans(size, weight, Color::TRANSPARENT)).w
}

/// `s`, cut with an ellipsis to fit `max`.
fn fit(s: &str, size: f32, weight: Weight, max: f32) -> String {
    if width(s, size, weight) <= max {
        return s.to_string();
    }
    let mut out: String = s.to_string();
    while !out.is_empty() {
        out.pop();
        let t = format!("{}…", out.trim_end());
        if width(&t, size, weight) <= max {
            return t;
        }
    }
    "…".into()
}

fn rect(ctx: &mut Ctx, r: Rect, fill: Color, radius: f32) -> NodeId {
    ctx.add(Node::Rect { rect: r, fill, radius })
}

/// The icon of `app` on a `p`-sized plate at (`x`, `y`).
pub fn draw_icon(ctx: &mut Ctx, app: &AppEntry, x: f32, y: f32, p: f32, plate: Color, ink: Color) {
    match icons::for_app(&app.id, &app.name, app.icon.as_deref()) {
        AppIcon::Glyph(g) => {
            rect(ctx, Rect::new(x, y, p, p), plate, radius::LG);
            let gs = em(GLYPH_PT);
            Glyph::new(g).size(GLYPH_PT).color(ink).at(x + (p - gs) / 2.0, y + (p - gs) / 2.0).emit(ctx);
        }
        AppIcon::Letter(l, c) => {
            rect(ctx, Rect::new(x, y, p, p), c, radius::LG);
            let s = l.to_string();
            let size = type_size::XL;
            let tw = width(&s, size, Weight::Semibold);
            text(ctx, x + (p - tw) / 2.0, y + p / 2.0, &s, size, Weight::Semibold, Color::rgba(1.0, 1.0, 1.0, 1.0));
        }
    }
}

impl View for AppsView {
    fn render(&self, ctx: &mut Ctx) {
        let t = ctx.theme;
        let (w, h) = self.size.get();
        let s = self.state.get_cloned();
        if self.min != (0.0, 0.0) {
            ctx.min_size(pergola::geom::Size::new(self.min.0, self.min.1));
        }
        let lay = layout(w, h);
        let m = filter(&s.apps, self.query.get_cloned().text());
        let sel = s.selected.min(m.len().saturating_sub(1));
        let page = lay.grid.page_of(sel);

        // The backdrop: a press on empty space closes the view.
        ctx.key(1);
        let bg = ctx.push(Node::Rect { rect: Rect::new(0.0, 0.0, w, h), fill: t.bg_canvas(), radius: 0.0 });
        ctx.on_click(bg, self.act(Action::Close));
        ctx.pop();

        // The filter field, with keyboard focus while the view is up.
        let f = lay.field;
        ctx.key(2);
        let holder = ctx.push(Node::Rect { rect: f, fill: Color::TRANSPARENT, radius: 0.0 });
        TextField::new(self.query.clone()).placeholder("Type to find an app").at(f.x(), f.y()).width(f.w()).render(ctx);
        ctx.pop();
        if let Some(&field_bg) = ctx.tree.children_of(holder).get(2) {
            ctx.autofocus(field_bg);
        }
        if self.query.get_cloned().text().is_empty() {
            // A magnifier at the field's right end (pt: a glyph).
            let gs = em(16.0);
            Glyph::new(MAGNIFYING_GLASS).size(16.0).color(t.text_tertiary()).at(f.x() + f.w() - gs - em(10.0), f.y() + (f.h() - gs) / 2.0).emit(ctx);
        }

        if m.is_empty() {
            let msg = if s.apps.is_empty() { "No apps are installed" } else { "No app matches" };
            let tw = width(msg, type_size::MD, Weight::Regular);
            text(ctx, (w - tw) / 2.0, lay.y0 + em(40.0), msg, type_size::MD, Weight::Regular, t.text_secondary());
        }

        let per = lay.grid.per_page();
        for (k, &ai) in m.iter().enumerate().skip(page * per).take(per) {
            let app = &s.apps[ai];
            let cell = lay.cell(k - page * per);
            let selected = k == sel;
            ctx.key(1000 + ai as u64);
            let inner = Rect::new(cell.x() + em(4.0), cell.y() + em(4.0), cell.w() - em(8.0), cell.h() - em(8.0));
            let id = ctx.push(Node::Rect { rect: inner, fill: Color::TRANSPARENT, radius: radius::LG });
            let hovered = ctx.is_hovered(id) || ctx.is_pressed(id);
            let fill = if ctx.is_pressed(id) {
                t.state_pressed()
            } else if selected {
                t.selection_idle()
            } else if hovered {
                t.state_hover()
            } else {
                Color::TRANSPARENT
            };
            if let Some(Node::Rect { fill: f, .. }) = ctx.tree.get_mut(id) {
                *f = fill;
            }
            ctx.on_click(id, self.act(Action::Launch(app.id.clone())));
            if let Some(d) = &app.description {
                ctx.tooltip(id, d.clone());
            }
            let p = plate();
            let px = cell.x() + (cell.w() - p) / 2.0;
            let py = cell.y() + em(12.0);
            if selected {
                // The keyboard's place: a ring round the plate.
                rect(ctx, Rect::new(px - 3.0, py - 3.0, p + 6.0, p + 6.0), t.focus_ring(), radius::LG + 3.0);
                rect(ctx, Rect::new(px - 1.0, py - 1.0, p + 2.0, p + 2.0), t.bg_canvas(), radius::LG + 1.0);
            }
            draw_icon(ctx, app, px, py, p, t.bg_surface(), t.text_primary());
            let launching = s.launching.as_deref() == Some(app.id.as_str());
            let label = if launching { "Starting…".to_string() } else { fit(&app.name, type_size::SM, Weight::Medium, cell.w() - em(12.0)) };
            let lw = width(&label, type_size::SM, Weight::Medium);
            text(ctx, cell.x() + (cell.w() - lw) / 2.0, py + p + em(18.0), &label, type_size::SM, Weight::Medium, if launching { t.text_secondary() } else { t.text_primary() });
            ctx.pop();

            // Keep in Dock / Remove from Dock, on the hovered or selected tile.
            let pinned = s.pins.contains(&s.apps, &app.id);
            let b = em(28.0);
            let br = Rect::new(cell.x() + cell.w() - b - em(8.0), cell.y() + em(8.0), b, b);
            ctx.key(5000 + ai as u64);
            let pin = ctx.push(Node::Rect { rect: br, fill: Color::TRANSPARENT, radius: radius::SM });
            let on_pin = ctx.is_hovered(pin) || ctx.is_pressed(pin);
            if hovered || selected || on_pin {
                if on_pin {
                    let c = if ctx.is_pressed(pin) { t.state_pressed() } else { t.state_hover() };
                    if let Some(Node::Rect { fill: f, .. }) = ctx.tree.get_mut(pin) {
                        *f = c;
                    }
                }
                let gs = em(16.0);
                let g = if pinned { PUSH_PIN_SLASH } else { PUSH_PIN };
                Glyph::new(g).size(16.0).color(if pinned { t.accent_fg() } else { t.text_secondary() }).at(br.x() + (b - gs) / 2.0, br.y() + (b - gs) / 2.0).emit(ctx);
                ctx.on_click(pin, self.act(Action::TogglePin(app.id.clone())));
                ctx.tooltip(pin, if pinned { "Remove from Dock" } else { "Keep in Dock" });
            }
            ctx.pop();
        }

        // Page dots.
        let pages = lay.grid.pages(m.len());
        if pages > 1 {
            let d = em(8.0);
            let step = em(20.0);
            let x0 = (w - (pages as f32 - 1.0) * step) / 2.0;
            let cy = lay.foot_y + em(14.0);
            for p in 0..pages {
                let cx = x0 + p as f32 * step;
                ctx.key(9000 + p as u64);
                let hit = ctx.push(Node::Rect { rect: Rect::new(cx - step / 2.0, cy - step / 2.0, step, step), fill: Color::TRANSPARENT, radius: radius::PILL });
                let c = if p == page { t.accent_fg() } else if ctx.is_hovered(hit) { t.text_secondary() } else { t.border_strong() };
                rect(ctx, Rect::new(cx - d / 2.0, cy - d / 2.0, d, d), c, radius::PILL);
                let st = self.state.clone();
                let first = p * per;
                ctx.on_click(hit, move || {
                    let mut s = st.get_cloned();
                    s.selected = first;
                    st.set(s);
                });
                ctx.tooltip(hit, format!("Page {}", p + 1));
                ctx.pop();
            }
        }

        // What the last launch said.
        if let Some(n) = &s.note {
            let msg = fit(&n.text, type_size::SM, Weight::Regular, w - 2.0 * margin());
            let tw = width(&msg, type_size::SM, Weight::Regular);
            text(ctx, (w - tw) / 2.0, lay.foot_y + em(38.0), &msg, type_size::SM, Weight::Regular, if n.error { t.danger_fg() } else { t.text_secondary() });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_centres_the_grid_and_hit_tests_it() {
        let l = layout(1280.0, 700.0);
        assert!(l.grid.cols >= 1 && l.grid.cols <= MAX_COLS);
        let span = l.grid.cols as f32 * cell_w();
        assert!((l.x0 - (1280.0 - span) / 2.0).abs() < 0.01);
        assert!(l.y0 + l.grid.rows as f32 * cell_h() <= l.foot_y + 0.01);
        let c = l.cell(l.grid.cols + 1);
        let at = l.item_at(0, 100, c.x() + 2.0, c.y() + 2.0);
        assert_eq!(at, Some(l.grid.cols + 1));
        // The second page's same place.
        assert_eq!(l.item_at(1, 1000, c.x() + 2.0, c.y() + 2.0), Some(l.grid.per_page() + l.grid.cols + 1));
        assert_eq!(l.item_at(0, 3, c.x() + 2.0, c.y() + 2.0), None);
        assert_eq!(l.item_at(0, 100, l.x0 - 1.0, l.y0), None);
    }

    #[test]
    fn a_small_window_still_has_one_cell() {
        let l = layout(200.0, 200.0);
        assert_eq!(l.grid.per_page(), 1);
    }
}
