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
//! **Frozen panes** ([`GridSource::frozen`]): bands over the body, each a
//! viewport that moves on one axis (the corner on none), draw the frozen
//! rows and columns and their labels; presses there map to the unscrolled
//! cells ([`Geom::content`]), and moving the cursor keeps it clear of them.
//!
//! Not yet: Ctrl+arrow data edges beyond what the source says.

use std::rc::Rc;

use crate::event::{Event, Key, Modifiers};
use crate::geom::Rect;
use crate::interaction::{DragPhase, ScrollInput, ScrollKind};
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

/// How much the headers scale at `zoom`: with it, but kept to half to twice
/// their size so their labels stay readable and don't crowd the cells.
pub fn header_scale(zoom: f32) -> f32 {
    if zoom.is_finite() { zoom.clamp(0.5, 2.0) } else { 1.0 }
}

/// The column header's height at `zoom`.
pub fn header_height(zoom: f32) -> f32 {
    (HEADER_H * header_scale(zoom)).round()
}
/// A press this close to a column's right edge in the header resizes it.
pub const RESIZE_GRIP: f32 = 4.0;
/// The narrowest a column can be dragged.
pub const MIN_COL_W: f64 = 12.0;
/// The lowest a dragged row goes, px.
pub const MIN_ROW_H: f64 = 6.0;
/// Text inset from a cell's edges.
const PAD: f32 = 4.0;
const TEXT_SIZE: f32 = type_size::SM;

/// Where a cell's lines sit up and down in a taller cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CellVAlign {
    Top,
    #[default]
    Center,
    /// A spreadsheet's default.
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CellAlign {
    Left,
    Center,
    Right,
    /// Centred across this cell and the empty cells after it that are
    /// `Across` too (a spreadsheet's Center Across Selection).
    Across,
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
    /// Background over the grid lines.
    pub fill: Option<Color>,
    /// Pattern name and its foreground/background colors.
    pub pattern: Option<(&'static str, Color, Color)>,
    /// Angle in millidegrees and stops (basis points, packed sRGB).
    pub gradient: Option<(u32, std::sync::Arc<[(u16, u32)]>)>,
    /// Diagonal down and up edges.
    pub diagonals: [Option<(f32, Color)>; 2],
    /// Degrees counterclockwise, or 255 for stacked characters.
    pub rotation: i16,
    pub rotation_face: Option<crate::glyphs::Face>,
    pub shrink_to_fit: bool,
    /// Left, right, top, bottom: width in points and colour.
    pub borders: [Option<(f32, Color)>; 4],
    /// A data bar: the share of the cell's width it fills (0–1) and its colour.
    pub bar: Option<(f32, Color)>,
    /// An icon at the cell's left (a conditional format's icon set).
    pub icon: Option<(CellIcon, Color)>,
    /// A sparkline drawn over the cell's ground.
    pub spark: Option<std::sync::Arc<Spark>>,
    /// Steps the text is indented from the left (a pivot's inner items), each
    /// [`INDENT_STEP`] px.
    pub indent: u8,
    /// The cell has a comment: a small triangle in its top-right corner.
    pub note: bool,
    /// Text broken onto lines to the cell's width (Wrap Text); it never
    /// runs over its neighbours. Text with line breaks shows its lines
    /// either way.
    pub wrap: bool,
    pub valign: CellVAlign,
    /// Italic, underline and strikethrough, as [`crate::inline::Emphasis`]
    /// bits (bold is `bold`).
    pub marks: u8,
    /// Text size, px, when not the grid's own.
    pub size: Option<f32>,
    /// Drawn in the monospace face (`font::MONO`) rather than the sans one.
    pub mono: bool,
    /// A number format's `*` fill: the byte offset in `text` of the character
    /// repeated to the cell's width. What comes before it sits at the left
    /// edge, what comes after at the right (Accounting's sign and number).
    pub fill_at: Option<usize>,
    /// How the text spreads over the cell's width beyond `align`.
    pub spread: Spread,
    /// Several lines spaced evenly from the top edge to the bottom (a
    /// spreadsheet's vertical Justify and Distributed); one line centred.
    pub vspread: bool,
}

/// A spreadsheet's other horizontal alignments, drawn from the left.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Spread {
    #[default]
    None,
    /// The text repeated as many whole times as fit; it never runs over.
    Fill,
    /// Wrapped, every line but the last stretched to both edges.
    Justify,
    /// Wrapped, every line's words spaced evenly over the width (one word
    /// centred).
    Distributed,
}

/// How a sparkline draws its points.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SparkStyle {
    Line,
    Column,
    /// Each point a block up (positive) or down (negative) from the middle.
    WinLoss,
}

/// A sparkline: a small chart of `points` inside one cell. A `None` point
/// is a gap.
#[derive(Debug, Clone, PartialEq)]
pub struct Spark {
    pub style: SparkStyle,
    pub points: Vec<Option<f64>>,
    pub color: Color,
    pub negative: Color,
    /// The highest and lowest points marked.
    pub markers: bool,
}

/// The shapes of sparkline `sp` in the box (`x`, `y`, `w`, `h`), also used
/// when printing.
pub fn spark_shapes(sp: &Spark, x: f32, y: f32, w: f32, h: f32, mut emit: impl FnMut(Node)) {
    let n = sp.points.len();
    let (x, y, w, h) = (x + 2.0, y + 2.0, w - 4.0, h - 4.0);
    if n == 0 || w < 4.0 || h < 4.0 {
        return;
    }
    let vals = sp.points.iter().flatten().copied();
    let (lo, hi) = vals.fold((f64::INFINITY, f64::NEG_INFINITY), |(a, b), v| (a.min(v), b.max(v)));
    if !lo.is_finite() {
        return;
    }
    let slot = w / n as f32;
    match sp.style {
        SparkStyle::Line => {
            let span = if hi > lo { hi - lo } else { 1.0 };
            let at = |i: usize, v: f64| (x + slot * (i as f32 + 0.5), y + h - ((v - lo) / span) as f32 * h - if hi > lo { 0.0 } else { h / 2.0 });
            let mut last: Option<(f32, f32)> = None;
            for (i, p) in sp.points.iter().enumerate() {
                match p {
                    Some(v) => {
                        let pt = at(i, *v);
                        if let Some(prev) = last {
                            emit(Node::Path { p0: prev, p1: pt, width: 1.0, color: sp.color });
                        }
                        last = Some(pt);
                    }
                    None => last = None,
                }
            }
            if sp.markers {
                for (i, p) in sp.points.iter().enumerate() {
                    if let Some(v) = *p {
                        if v == hi || v == lo {
                            let (px, py) = at(i, v);
                            emit(Node::Rect { rect: Rect::new(px - 2.0, py - 2.0, 4.0, 4.0), fill: if v == lo && hi > lo { sp.negative } else { sp.color }, radius: 2.0 });
                        }
                    }
                }
            }
        }
        SparkStyle::Column => {
            // Bars stand on zero, so every point shows.
            let (base, top) = (lo.min(0.0), hi.max(0.0));
            let span = if top > base { top - base } else { 1.0 };
            let zero = y + h - ((0.0f64.max(base) - base) / span) as f32 * h;
            for (i, p) in sp.points.iter().enumerate() {
                let Some(v) = *p else { continue };
                let yv = y + h - ((v - base) / span) as f32 * h;
                let (t, b) = if yv < zero { (yv, zero) } else { (zero, yv) };
                let bw = (slot - 1.0).max(1.0);
                let marked = sp.markers && (v == hi || v == lo);
                let fill = if v < 0.0 || (marked && v == lo && hi > lo) { sp.negative } else { sp.color };
                emit(Node::Rect { rect: Rect::new(x + slot * i as f32, t, bw, (b - t).max(1.0)), fill, radius: 0.0 });
            }
        }
        SparkStyle::WinLoss => {
            let mid = y + h / 2.0;
            for (i, p) in sp.points.iter().enumerate() {
                let Some(v) = *p else { continue };
                if v == 0.0 {
                    continue;
                }
                let bw = (slot - 1.0).max(1.0);
                let (t, fill) = if v > 0.0 { (y, sp.color) } else { (mid, sp.negative) };
                emit(Node::Rect { rect: Rect::new(x + slot * i as f32, t, bw, h / 2.0 - 1.0), fill, radius: 0.0 });
            }
        }
    }
}

/// One step of a cell's indent, px.
pub const INDENT_STEP: f32 = 12.0;

/// The icons a cell can show (spreadsheet icon sets), drawn as vectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CellIcon {
    /// An arrow: 2 up, 1 up and right, 0 right, -1 down and right, -2 down.
    Arrow(i8),
    /// A filled circle (a traffic light).
    Dot,
    Flag,
    /// A tick, an exclamation mark, a cross: on a filled circle (`true`)
    /// or bare.
    Check(bool),
    Exclaim(bool),
    Cross(bool),
    /// A triangle pointing up (`true`) or down.
    Triangle(bool),
    Diamond,
    Dash,
    /// A star: 0 empty, 1 half filled, 2 filled.
    Star(u8),
    /// Four rising bars, this many lit.
    Bars(u8),
    /// A circle, this many quarters filled.
    Quarters(u8),
    /// Four boxes, this many filled.
    Boxes(u8),
}

/// A filled polygon in the grid's coordinates.
fn poly(pts: &[(f32, f32)], c: Color, clip: Option<[f32; 4]>) -> Node {
    use fresco_protocol::fill_verb::{CLOSE, LINE, MOVE};
    let mut verbs = vec![MOVE];
    verbs.extend(std::iter::repeat_n(LINE, pts.len().saturating_sub(1)));
    verbs.push(CLOSE);
    Node::Fill(std::sync::Arc::new(fresco_protocol::FillParams {
        rule: 0,
        transform: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0],
        paint: fresco_protocol::FillPaint::Solid { r: c.r, g: c.g, b: c.b, a: c.a },
        opacity: 1.0,
        clip,
        verbs,
        points: pts.iter().map(|p| [p.0, p.1]).collect(),
    }))
}

/// Draws `icon` in `c` in the square at (`x`, `y`), `s` wide; `off` is the
/// colour of unlit parts.
pub fn draw_icon(ctx: &mut Ctx, icon: CellIcon, c: Color, off: Color, x: f32, y: f32, s: f32) {
    icon_shapes(icon, c, off, x, y, s, |node| { ctx.add(node); });
}

/// The grid icon's vector geometry, also used when printing a cell.
pub fn icon_shapes(icon: CellIcon, c: Color, off: Color, x: f32, y: f32, s: f32, mut emit: impl FnMut(Node)) {
    let (mx, my) = (x + s / 2.0, y + s / 2.0);
    let stroke = |a, b, w, color| Node::Path { p0: a, p1: b, width: w, color };
    let white = Color::rgba(1.0, 1.0, 1.0, 1.0);
    let w = (s * 0.16).max(1.5);
    match icon {
        CellIcon::Arrow(d) => {
            // Shaft from behind the middle to the head, the head a triangle.
            let ang = -(d as f32) * std::f32::consts::FRAC_PI_4;
            let (dx, dy) = (ang.cos(), ang.sin());
            let (nx, ny) = (-dy, dx);
            let r = s * 0.45;
            let tip = (mx + dx * r, my + dy * r);
            let base = (mx + dx * (r - s * 0.42), my + dy * (r - s * 0.42));
            emit(stroke((mx - dx * r, my - dy * r), base, w * 1.4, c));
            let hw = s * 0.32;
            emit(poly(&[tip, (base.0 + nx * hw, base.1 + ny * hw), (base.0 - nx * hw, base.1 - ny * hw)], c, None));
        }
        CellIcon::Dot => {
            emit(Node::Rect { rect: Rect::new(x + 1.0, y + 1.0, s - 2.0, s - 2.0), fill: c, radius: (s - 2.0) / 2.0 });
        }
        CellIcon::Flag => {
            emit(stroke((x + s * 0.2, y + s * 0.05), (x + s * 0.2, y + s), w, off));
            emit(poly(&[(x + s * 0.25, y + s * 0.05), (x + s * 0.95, y + s * 0.3), (x + s * 0.25, y + s * 0.6)], c, None));
        }
        CellIcon::Check(ring) | CellIcon::Exclaim(ring) | CellIcon::Cross(ring) => {
            let mark = if ring {
                emit(Node::Rect { rect: Rect::new(x, y, s, s), fill: c, radius: s / 2.0 });
                white
            } else {
                c
            };
            let k = if ring { 0.28 } else { 0.18 };
            match icon {
                CellIcon::Check(_) => {
                    emit(stroke((x + s * k, my), (x + s * 0.43, y + s * (1.0 - k)), w, mark));
                    emit(stroke((x + s * 0.43, y + s * (1.0 - k)), (x + s * (1.0 - k), y + s * k), w, mark));
                }
                CellIcon::Exclaim(_) => {
                    emit(stroke((mx, y + s * k), (mx, y + s * 0.6), w, mark));
                    emit(Node::Rect { rect: Rect::new(mx - w / 2.0, y + s * (0.95 - k) - w / 2.0, w, w), fill: mark, radius: w / 2.0 });
                }
                _ => {
                    emit(stroke((x + s * k, y + s * k), (x + s * (1.0 - k), y + s * (1.0 - k)), w, mark));
                    emit(stroke((x + s * (1.0 - k), y + s * k), (x + s * k, y + s * (1.0 - k)), w, mark));
                }
            }
        }
        CellIcon::Triangle(up) => {
            let (a, b) = if up { (y + s * 0.15, y + s * 0.85) } else { (y + s * 0.85, y + s * 0.15) };
            emit(poly(&[(mx, a), (x + s * 0.95, b), (x + s * 0.05, b)], c, None));
        }
        CellIcon::Diamond => {
            emit(poly(&[(mx, y), (x + s, my), (mx, y + s), (x, my)], c, None));
        }
        CellIcon::Dash => {
            emit(Node::Rect { rect: Rect::new(x + s * 0.15, my - w / 2.0, s * 0.7, w), fill: c, radius: 0.0 });
        }
        CellIcon::Star(n) => {
            let pts: Vec<(f32, f32)> = (0..10)
                .map(|i| {
                    let r = if i % 2 == 0 { s * 0.5 } else { s * 0.21 };
                    let a = -std::f32::consts::FRAC_PI_2 + i as f32 * std::f32::consts::PI / 5.0;
                    (mx + r * a.cos(), my + 0.04 * s + r * a.sin())
                })
                .collect();
            emit(poly(&pts, if n == 2 { c } else { off }, None));
            if n == 1 {
                emit(poly(&pts, c, Some([x - 1.0, y - 1.0, mx, y + s + 1.0])));
            }
        }
        CellIcon::Bars(n) => {
            let bw = s / 4.0 - 1.0;
            for i in 0..4u8 {
                let h = s * (0.25 + 0.25 * i as f32);
                emit(Node::Rect { rect: Rect::new(x + i as f32 * (bw + 1.0) + 0.5, y + s - h, bw, h), fill: if i < n { c } else { off }, radius: 0.0 });
            }
        }
        CellIcon::Quarters(n) => {
            emit(Node::Rect { rect: Rect::new(x, y, s, s), fill: c, radius: s / 2.0 });
            emit(Node::Rect { rect: Rect::new(x + w * 0.6, y + w * 0.6, s - w * 1.2, s - w * 1.2), fill: white, radius: (s - w * 1.2) / 2.0 });
            if n > 0 {
                // A pie from twelve o'clock, clockwise.
                let r = s / 2.0;
                let mut pts = vec![(mx, my)];
                let steps = 6 * n as usize;
                for i in 0..=steps {
                    let a = -std::f32::consts::FRAC_PI_2 + (n as f32 * std::f32::consts::FRAC_PI_2) * i as f32 / steps as f32;
                    pts.push((mx + r * a.cos(), my + r * a.sin()));
                }
                emit(poly(&pts, c, None));
            }
        }
        CellIcon::Boxes(n) => {
            let b = s / 2.0 - 1.0;
            for i in 0..4u8 {
                let (bx, by) = (x + (i % 2) as f32 * (b + 2.0), y + (i / 2) as f32 * (b + 2.0));
                emit(Node::Rect { rect: Rect::new(bx, by, b, b), fill: if i < n { c } else { off }, radius: 1.0 });
            }
        }
    }
}

impl CellView {
    pub fn text(s: impl Into<String>) -> Self {
        Self { text: s.into(), align: CellAlign::Left, color: None, bold: false, numeric: false, fill: None, pattern: None, gradient: None, diagonals: [None; 2], rotation: 0, rotation_face: None, shrink_to_fit: false, borders: [None; 4], bar: None, icon: None, spark: None, indent: 0, note: false, wrap: false, valign: CellVAlign::Center, marks: 0, size: None, mono: false, fill_at: None, spread: Spread::None, vspread: false }
    }
    pub fn number(s: impl Into<String>) -> Self {
        Self { text: s.into(), align: CellAlign::Right, numeric: true, ..Self::text("") }
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
    /// The outline button at a visible row or column header.
    fn outline_control(&self, _rows: bool, _at: u32) -> Option<(u32, u32, bool)> {
        None
    }
    /// Ctrl + arrow from (`r`, `c`) by (`dr`, `dc`): where the data ends.
    /// The default goes to the sheet's edge.
    /// The merged area holding cell (`r`, `c`), drawn as one cell with the
    /// top-left's content; the cursor treats it as one.
    fn merge_at(&self, _r: u32, _c: u32) -> Option<CellRange> {
        None
    }
    /// The merged areas meeting `range`.
    fn merges_in(&self, _range: CellRange) -> Vec<CellRange> {
        Vec::new()
    }
    /// The view's zoom (1: 100 %). The source scales its rows, columns and
    /// cell text itself; the grid scales the in-cell editor's text to match.
    fn zoom(&self) -> f32 {
        1.0
    }
    /// Whether the cells show gridlines (Excel's View > Gridlines).
    fn show_grid(&self) -> bool {
        true
    }
    /// Whether the row and column headings show (View > Headings). Without
    /// them the cells start at the view's top left.
    fn show_headings(&self) -> bool {
        true
    }
    /// Today's date (or, `time`, the time now) as typed into a cell, for
    /// Ctrl+; and Ctrl+Shift+;. None: the keys do nothing.
    fn now_text(&self, _time: bool) -> Option<String> {
        None
    }
    /// AutoComplete: the one entry near (`r`, `c`) that what was typed,
    /// `prefix`, begins (ignoring case), to finish it. None: no completion.
    fn complete(&self, _r: u32, _c: u32, _prefix: &str) -> Option<String> {
        None
    }
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
    /// What is drawn over the cells (charts) meeting `view` (content px: x, y,
    /// width, height), in `theme`'s colours.
    fn floats(&self, _view: [f64; 4], _theme: &crate::theme::Semantic) -> Vec<GridFloat> {
        Vec::new()
    }
    /// Rows and columns frozen at the top and left, kept in view while the
    /// rest scrolls ((0, 0): none).
    fn frozen(&self) -> (u32, u32) {
        (0, 0)
    }
    /// The first frozen row and column ((0, 0) unless panes were frozen
    /// with the view scrolled): rows from it to [`frozen`](Self::frozen)
    /// stay in view, and those above it are out of reach until unfrozen.
    fn frozen_from(&self) -> (u32, u32) {
        (0, 0)
    }
    /// Where Tab (Shift+Tab: `back`) goes from `from` when it doesn't step
    /// to the next column: a protected sheet's next unlocked cell, as in
    /// Excel. `None` steps as usual.
    fn tab_target(&self, _from: (u32, u32), _back: bool) -> Option<(u32, u32)> {
        None
    }
    /// The last used cell (Ctrl + End).
    fn last_used(&self) -> (u32, u32) {
        (self.rows().saturating_sub(1), self.cols().saturating_sub(1))
    }
    /// The selection shows a fill handle at its corner, and Ctrl+D and
    /// Ctrl+R fill down and right ([`GridAction::Fill`]).
    fn fills(&self) -> bool {
        false
    }
}

/// Something drawn over the cells, such as a chart.
#[derive(Clone)]
pub struct GridFloat {
    /// The app's name for it (stable across frames).
    pub key: u64,
    /// Where it sits, content px: x, y, width, height.
    pub at: [f64; 4],
    /// What it draws, placed at its own origin (0, 0).
    pub nodes: Rc<Vec<Node>>,
    /// A click picks it, a drag moves it, its corner sizes it (charts);
    /// otherwise presses go to the cells under it.
    pub movable: bool,
    /// A press on it is [`GridAction::PressFloat`] (a button in a cell), not
    /// a press on the cells.
    pub pressable: bool,
}

/// `n` moved by (dx, dy): rectangles, text and fills.
fn shift(n: &Node, dx: f32, dy: f32) -> Node {
    let mv = |r: &Rect| Rect::new(r.x() + dx, r.y() + dy, r.w(), r.h());
    match n {
        Node::Rect { rect, fill, radius } => Node::Rect { rect: mv(rect), fill: *fill, radius: *radius },
        Node::Text { rect, content, style } => Node::Text { rect: mv(rect), content: content.clone(), style: style.clone() },
        Node::Fill(f) => Node::Fill(std::sync::Arc::new(crate::node::fill_to(f, crate::geom::Point::new(-dx, -dy), 1.0))),
        other => other.clone(),
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
    /// A row's bottom edge dragged (in the row labels).
    ResizeRow { row: u32, y0: f32, h0: f64 },
    /// A picked float pressed: moved (or sized from its corner) by the
    /// distance from where the press began, content px.
    Float { key: u64, size: bool, from: (f64, f64), dx: f64, dy: f64 },
    /// A press in the editor: dragging selects its text.
    Caret,
    /// The fill handle dragged: the area it reaches so far.
    Fill { to: CellRange, from: (f64, f64) },
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
    /// When the fill handle was last pressed: a second press soon after is
    /// a double-click.
    fill_press: Option<std::time::Instant>,
    /// The column edge last pressed, and when: a second press soon after is
    /// a double-click (fit the column).
    edge_press: Option<(u32, std::time::Instant)>,
    /// The row edge last pressed (the row a double-click fits), and when.
    row_edge_press: Option<(u32, std::time::Instant)>,
    /// While a selection or the fill handle is dragged past the body's
    /// edge: where the pointer is (window px). Each frame scrolls toward it.
    edge: Option<crate::geom::Point>,
    /// When the editor was last pressed: a second press soon after is a
    /// double-click (selects the word).
    text_press: Option<std::time::Instant>,
    /// The editor was started by typing (Excel's Enter mode): arrow keys
    /// finish the entry and move. Otherwise (F2, Enter, a press in the
    /// editor) they move the caret.
    entering: bool,
    /// The editor's selected tail is an AutoComplete guess, made by the last key.
    guess: bool,
    /// The column a run of Tabs started in: Enter goes on to the next row
    /// there, as in Excel. Any other move ends the run.
    tab_from: Option<u32>,
    /// F8's Extend Selection: moves extend the selection as with Shift held.
    extend: bool,
    /// End was pressed: the next arrow goes to the edge of the data, as
    /// Ctrl+arrow does (Excel's End mode).
    end: bool,
    /// A selection kept while Enter or Tab steps the active cell through
    /// it, and the cell it is at: shown only while the cursor is there and
    /// nothing else is selected (any other move drops it).
    held: Option<(CellRange, (u32, u32))>,
    /// The movable float a click picked (its key), framed with a corner
    /// handle; Delete asks to remove it, Escape lets it go.
    pub picked: Option<u64>,
}

/// What the arrow keys do in the open editor ([`GridState::edit_mode`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditMode {
    /// Arrows finish the entry and move.
    Enter,
    /// Arrows move the caret.
    Edit,
}

impl GridState {
    /// Whether F8's Extend Selection is on (the app shows it in its status bar).
    pub fn extending(&self) -> bool {
        self.extend
    }
    /// Whether End mode is on: the next arrow goes to the data's edge.
    pub fn end_mode(&self) -> bool {
        self.end
    }
    /// While the editor is open: whether it is entering (started by typing:
    /// arrows finish the entry, Excel's "Enter") or editing (arrows move the
    /// caret, Excel's "Edit"). None when it is closed.
    pub fn edit_mode(&self) -> Option<EditMode> {
        self.edit.as_ref().map(|_| if self.entering { EditMode::Enter } else { EditMode::Edit })
    }
    pub fn selection(&self) -> CellRange {
        match self.held {
            Some((r, at)) if at == self.cursor && self.anchor == self.cursor && r.contains(at.0, at.1) => r,
            _ => CellRange::of(self.anchor, self.cursor),
        }
    }
    /// Select one cell.
    pub fn select(&mut self, r: u32, c: u32) {
        self.anchor = (r, c);
        self.cursor = (r, c);
        self.tab_from = None;
        self.held = None;
        self.end = false;
    }
    /// Enter (`down`) or Tab (`!down`) in a selection of more than one
    /// cell: the active cell steps to the next one in it, down the column
    /// (across the row) and on to the next, round to the first after the
    /// last; `back` (with Shift) the other way. The selection stays. False
    /// for a single cell.
    pub fn step_in_selection(&mut self, down: bool, back: bool) -> bool {
        let s = self.selection();
        if s.r0 == s.r1 && s.c0 == s.c1 {
            return false;
        }
        let (h, w) = (u64::from(s.r1 - s.r0) + 1, u64::from(s.c1 - s.c0) + 1);
        let n = h * w;
        let (r, c) = (u64::from(self.cursor.0.clamp(s.r0, s.r1) - s.r0), u64::from(self.cursor.1.clamp(s.c0, s.c1) - s.c0));
        let i = if down { c * h + r } else { r * w + c };
        let j = if back { (i + n - 1) % n } else { (i + 1) % n };
        let (dr, dc) = if down { (j % h, j / h) } else { (j / w, j % w) };
        // dr ≤ r1 - r0 and dc ≤ c1 - c0, so both fit in u32.
        let to = (s.r0 + dr as u32, s.c0 + dc as u32);
        self.anchor = to;
        self.cursor = to;
        self.held = Some((s, to));
        true
    }
}

/// What the user asked of the grid; the app applies it to its data.
#[derive(Debug, Clone, PartialEq)]
pub enum GridAction {
    Outline { rows: bool, start: u32, end: u32 },
    /// The editor closed with this text for (`row`, `col`).
    Commit { row: u32, col: u32, text: String },
    /// Delete: empty these cells.
    Clear(CellRange),
    /// Column `col` dragged to `width` px.
    ResizeCol { col: u32, width: f64 },
    /// Row `row` dragged to `height` px.
    ResizeRow { row: u32, height: f64 },
    /// A column or row resize let go: the sizes it left are final.
    ResizeEnd,
    /// A double-click on column `col`'s right edge: fit it to what it
    /// holds (the app measures, with [`needed_width`]).
    FitCol(u32),
    /// Ctrl+Enter in the editor: `text` entered in every cell of `sel`
    /// (formulas moving as a copy would); the selection stays.
    CommitAll { sel: CellRange, text: String },
    /// A double-click on row `row`'s bottom edge: fit it to what it holds
    /// (the app measures, with [`needed_height`]). On the double line of
    /// hidden rows, `row` is the first hidden one.
    FitRow(u32),
    Undo,
    Redo,
    /// F4 outside the editor: the last command again, on the selection.
    Repeat,
    /// Ctrl+PageUp (-1) / Ctrl+PageDown (1): the sheet before or after;
    /// Shift+F11 (0): a new sheet.
    Sheet(i8),
    /// Shift+F2: the cursor cell's comment, to write or edit.
    Comment,
    /// A press (and drag) in the cells ended: the selection it made.
    Selected(CellRange),
    /// Ctrl (or Cmd) with the wheel, or a pinch: zoom by this factor.
    Zoom(f32),
    Copy(CellRange),
    Cut(CellRange),
    /// Paste at the active cell; `sel` the selection (a bigger one may be
    /// filled with copies).
    Paste { row: u32, col: u32, sel: CellRange, plain: bool },
    /// Ctrl+Alt+V: ask how to paste (the app's Paste Special).
    PasteSpecial,
    /// Float `key` dragged to `at` (content px: x, y, width, height).
    PlaceFloat { key: u64, at: [f64; 4] },
    /// Delete with float `key` picked.
    DeleteFloat(u64),
    /// Ctrl (or Cmd) + click on cell (`row`, `col`): follow what it links
    /// to, if anything. The click's activation is current.
    Follow { row: u32, col: u32 },
    /// A press on pressable float `key`.
    PressFloat(u64),
    /// Alt+Down on cell (`row`, `col`): offer what it can hold (a list).
    Choose { row: u32, col: u32 },
    /// Fill `to` from `from` (inside it, `to` reaching past it one way):
    /// the fill handle (`copy` false: series), Ctrl+D and Ctrl+R (`copy`).
    /// `flip`: the handle let go with Ctrl held (series and copy swap).
    Fill { from: CellRange, to: CellRange, copy: bool, flip: bool },
    /// A double-click on the fill handle: fill the selection down as far as
    /// the data beside it goes (the app finds how far).
    FillToData(CellRange),
    /// Ctrl+E: fill the cursor's column by the examples typed in it (Flash
    /// Fill; the app learns how).
    FlashFill { row: u32, col: u32 },
    /// Alt+=: sum the numbers next to the selection (AutoSum; the app finds
    /// which).
    AutoSum(CellRange),
    /// Ctrl+G or F5: ask where to go (the app asks).
    GoTo,
    /// A formatting key on the selection: `b`, `i`, `u` and `5` (bold,
    /// italic, underline, strikethrough) with Ctrl; `~ ! @ # $ % ^` with
    /// Ctrl+Shift (General, number, time, date, currency, percent,
    /// scientific), as Excel's.
    Format(char),
}

impl GridState {
    /// The distance a float is being dragged, while it is: (key, sizing, dx, dy).
    pub fn float_drag(&self) -> Option<(u64, bool, f64, f64)> {
        match self.drag {
            Drag::Float { key, size, dx, dy, .. } => Some((key, size, dx, dy)),
            _ => None,
        }
    }

    /// The area the fill handle reaches, while it is dragged.
    pub fn fill_drag(&self) -> Option<CellRange> {
        match self.drag {
            Drag::Fill { to, .. } => Some(to),
            _ => None,
        }
    }
}

/// `at`, or past the hidden (zero-sized) rows or columns it is in, the way
/// the cursor was going (`dr`, `dc`), to the next shown one; `from` (where
/// it was) if there is none.
fn past_hidden(src: &dyn GridSource, from: (u32, u32), at: (u32, u32), dr: i32, dc: i32) -> (u32, u32) {
    let (r, c) = at;
    if dr != 0 && src.row_top(r + 1) - src.row_top(r) <= 0.0 {
        let y = src.row_top(r);
        let to = if dr > 0 { src.row_at(y) } else if y > 0.0 { src.row_at(y - 0.5) } else { r };
        let shown = src.row_top(to + 1) - src.row_top(to) > 0.0;
        return if shown { (to, c) } else { from };
    }
    if dc != 0 && src.col_left(c + 1) - src.col_left(c) <= 0.0 {
        let x = src.col_left(c);
        let to = if dc > 0 { src.col_at(x) } else if x > 0.0 { src.col_at(x - 0.5) } else { c };
        let shown = src.col_left(to + 1) - src.col_left(to) > 0.0;
        return if shown { (r, to) } else { from };
    }
    at
}

/// How near the selection's corner a press takes the fill handle, px.
const FILL_GRIP: f64 = 5.0;

/// Two presses closer than this are a double-click.
const DOUBLE_CLICK: std::time::Duration = std::time::Duration::from_millis(450);

/// The editor's area, content px: the active cell (or its merged area),
/// widened to its text as it is drawn.
fn editor_rect(src: &dyn GridSource, g: &GridState) -> Option<[f64; 4]> {
    let ed = g.edit.as_ref()?;
    let one = src.merge_at(g.cursor.0, g.cursor.1).unwrap_or(CellRange::of(g.cursor, g.cursor));
    let (x, y) = (src.col_left(one.c0), src.row_top(one.r0));
    let (w, h) = (src.col_left(one.c1 + 1) - x, src.row_top(one.r1 + 1) - y);
    let (tw, th) = editor_size(ed.text(), editor_wrap(src, g), &editor_style(src.zoom()));
    Some([x, y, w.max(tw as f64 + 2.0 * PAD as f64 + 2.0), h.max(th as f64)])
}

/// How wide the editor's lines may run before they wrap, px: the cell's
/// width inside its padding when the cell wraps its text, as the cell
/// shows it; None (no wrapping) otherwise.
fn editor_wrap(src: &dyn GridSource, g: &GridState) -> Option<f32> {
    let one = src.merge_at(g.cursor.0, g.cursor.1).unwrap_or(CellRange::of(g.cursor, g.cursor));
    let cv = src.cell(one.r0, one.c0)?;
    let w = (src.col_left(one.c1 + 1) - src.col_left(one.c0)) as f32;
    cv.wrap.then(|| (w - 2.0 * PAD - cv.indent as f32 * INDENT_STEP - 2.0).max(8.0))
}

/// The editor's lines: byte ranges of `text` between line breaks, and,
/// within `wrap` px, broken after spaces (or inside a word too long for
/// the line) as the cell breaks them.
fn editor_lines(text: &str, wrap: Option<f32>, style: &TextStyle) -> Vec<std::ops::Range<usize>> {
    let mut hard = Vec::new();
    let mut start = 0;
    for (i, ch) in text.char_indices() {
        if ch == '\n' {
            hard.push(start..i);
            start = i + 1;
        }
    }
    hard.push(start..text.len());
    let Some(width) = wrap else { return hard };
    let mut out = Vec::new();
    for r in hard {
        let mut start = r.start;
        loop {
            // The furthest place the line fits to, and the last after a space.
            let (mut w, mut fit, mut space) = (0.0f32, start, None);
            for (off, ch) in text[start..r.end].char_indices() {
                w += crate::text::measure(ch.encode_utf8(&mut [0u8; 4]), style).w;
                if w > width {
                    break;
                }
                fit = start + off + ch.len_utf8();
                if ch == ' ' {
                    space = Some(fit);
                }
            }
            if fit >= r.end {
                out.push(start..r.end);
                break;
            }
            let first = start + text[start..].chars().next().map_or(1, char::len_utf8);
            let cut = space.filter(|&s| s > start).unwrap_or(fit.max(first));
            out.push(start..cut);
            start = cut;
        }
    }
    out
}

/// The widest line and the height the editor's lines need, px.
fn editor_size(text: &str, wrap: Option<f32>, style: &TextStyle) -> (f32, f32) {
    let lines = editor_lines(text, wrap, style);
    let w = lines.iter().map(|r| crate::text::measure(&text[r.clone()], style).w).fold(0.0, f32::max);
    let h = if lines.len() > 1 { lines.len() as f32 * style.size * 1.3 + 2.0 * LINE_PAD } else { 0.0 };
    (w, h)
}

/// Where the editor's first line starts down from its top, px: one line
/// centred in the cell, several from the top.
fn editor_top(lines: usize, h: f32, lh: f32) -> f32 {
    if lines > 1 { LINE_PAD } else { ((h - lh) / 2.0).max(0.0) }
}

fn in_editor(src: &dyn GridSource, g: &GridState, cx: f64, cy: f64) -> bool {
    editor_rect(src, g).is_some_and(|r| cx >= r[0] && cx < r[0] + r[2] && cy >= r[1] && cy < r[1] + r[3])
}

/// The editor's text index nearest content point (`cx`, `cy`) (a
/// character boundary): on the line at `cy`, nearest `cx`.
fn editor_index(src: &dyn GridSource, g: &GridState, cx: f64, cy: f64) -> Option<usize> {
    let r = editor_rect(src, g)?;
    let text = g.edit.as_ref()?.text();
    let lines = editor_lines(text, editor_wrap(src, g), &editor_style(src.zoom()));
    let one = src.merge_at(g.cursor.0, g.cursor.1).unwrap_or(CellRange::of(g.cursor, g.cursor));
    let h = (src.row_top(one.r1 + 1) - src.row_top(one.r0)) as f32;
    let lh = TEXT_SIZE * src.zoom() * 1.3;
    let k = (((cy - r[1]) as f32 - editor_top(lines.len(), h, lh)) / lh).floor().clamp(0.0, (lines.len() - 1) as f32) as usize;
    let line = lines[k].clone();
    Some(line.start + index_at(&text[line.clone()], (cx - r[0]) as f32 - PAD, &editor_style(src.zoom())))
}

/// The character boundary of `text` nearest `x` px from its start.
fn index_at(text: &str, x: f32, style: &TextStyle) -> usize {
    let bounds: Vec<usize> = text.char_indices().map(|(i, _)| i).chain(std::iter::once(text.len())).collect();
    let w = |k: usize| crate::text::measure(&text[..bounds[k]], style).w;
    // Widths grow with the prefix: the last boundary at or left of x...
    let (mut lo, mut hi) = (0usize, bounds.len() - 1);
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if w(mid) <= x { lo = mid } else { hi = mid - 1 }
    }
    // ...or the next, when x is nearer it.
    if lo + 1 < bounds.len() && (w(lo + 1) - x) < (x - w(lo)) {
        lo += 1;
    }
    bounds[lo]
}

fn editor_style(zoom: f32) -> TextStyle {
    TextStyle { family: font::SANS.into(), size: TEXT_SIZE * zoom, weight: Weight::Regular, color: Color::rgba(0.0, 0.0, 0.0, 1.0) }
}

/// How far `p` is past the body's edges, px: (right +, left -), (below +,
/// above -); 0 inside.
fn past_edge(geom: &Geom, p: crate::geom::Point) -> (f32, f32) {
    let b = geom.body;
    let o = |v: f32, lo: f32, hi: f32| if v < lo { v - lo } else if v > hi { v - hi } else { 0.0 };
    (o(p.x, b.x(), b.x() + b.w()), o(p.y, b.y(), b.y() + b.h()))
}

/// The cell under `p`, or past the body's edge, the cell at that edge.
fn cell_at_edge(src: &dyn GridSource, geom: &Geom, st: &ScrollState, p: crate::geom::Point) -> (u32, u32) {
    let b = geom.body;
    let q = crate::geom::Point::new(p.x.clamp(b.x(), (b.x() + b.w() - 1.0).max(b.x())), p.y.clamp(b.y(), (b.y() + b.h() - 1.0).max(b.y())));
    let (cx, cy) = geom.content(st, q);
    (src.row_at(cy.max(0.0)), src.col_at(cx.max(0.0)))
}

/// The fill handle dragged to `cell` from selection `sel`: `sel` grown down,
/// up, right or left, whichever way `cell` is farther outside it.
fn fill_reach(sel: CellRange, cell: (u32, u32)) -> CellRange {
    let (r, c) = cell;
    let down = r.saturating_sub(sel.r1) as i64 - sel.r0.saturating_sub(r) as i64;
    let right = c.saturating_sub(sel.c1) as i64 - sel.c0.saturating_sub(c) as i64;
    if down == 0 && right == 0 {
        return sel;
    }
    if down.abs() >= right.abs() {
        if down > 0 { CellRange { r1: r, ..sel } } else { CellRange { r0: r, ..sel } }
    } else if right > 0 {
        CellRange { c1: c, ..sel }
    } else {
        CellRange { c0: c, ..sel }
    }
}

impl GridState {
}

/// How near a float's bottom-right corner a press sizes it, px.
pub const FLOAT_GRIP: f64 = 10.0;

/// `at` moved or sized by (`dx`, `dy`), at least 16 px a side.
pub fn float_moved(at: [f64; 4], size: bool, dx: f64, dy: f64) -> [f64; 4] {
    if size {
        [at[0], at[1], (at[2] + dx).max(16.0), (at[3] + dy).max(16.0)]
    } else {
        [(at[0] + dx).max(0.0), (at[1] + dy).max(0.0), at[2], at[3]]
    }
}

pub type ActionFn = Rc<dyn Fn(GridAction)>;

/// The geometry one render used, for the handlers.
#[derive(Debug, Clone, Copy)]
struct Geom {
    body: Rect,
    rows: u32,
    cols: u32,
    ext: [f64; 2],
    /// The frozen rows and columns, and their size in px (width, height), at
    /// most the body's.
    frozen: (u32, u32),
    fz: [f64; 2],
    /// Where the first frozen column and row start, px (x, y): what is
    /// left of and above them is out of reach.
    skip: [f64; 2],
}

impl Geom {
    /// The content point under window point `p`: in the frozen bands, the
    /// unscrolled place; elsewhere, the scrolled one.
    fn content(&self, st: &ScrollState, p: crate::geom::Point) -> (f64, f64) {
        let (dx, dy) = ((p.x - self.body.x()) as f64, (p.y - self.body.y()) as f64);
        (if dx < self.fz[0] { self.skip[0] + dx } else { st.physics_x.y + dx }, if dy < self.fz[1] { self.skip[1] + dy } else { st.physics.y + dy })
    }
}

/// Visible runs between two edges in content coordinates. The inverse
/// position lookup skips zero-size runs at a shared edge.
fn visible_runs(
    start: f64, end: f64, first: u32, last: u32,
    index: impl Fn(f64) -> u32, offset: impl Fn(u32) -> f64,
) -> Vec<(u32, u32)> {
    if start >= end || first > last { return Vec::new(); }
    let mut i = index(start).max(first);
    let mut runs: Vec<(u32, u32)> = Vec::new();
    while i <= last && runs.len() < 64 {
        let at = offset(i);
        if at >= end { break; }
        let next = offset(i + 1);
        if next > at {
            if let Some(run) = runs.last_mut().filter(|run| run.1 + 1 == i) {
                run.1 = i;
            } else {
                runs.push((i, i));
            }
        }
        i = if next > at { i + 1 } else { index(next).max(i + 1) };
    }
    runs
}

pub struct GridView {
    pub source: Rc<dyn GridSource>,
    pub scroll: Mutable<ScrollState>,
    pub state: Mutable<GridState>,
    pub rect: Rect,
    on_action: Option<ActionFn>,
    on_activate: Option<Rc<dyn Fn()>>,
    on_visible: Option<Rc<dyn Fn(CellRange)>>,
    display_editor: bool,
    key: u64,
}

impl GridView {
    pub fn new(source: Rc<dyn GridSource>, scroll: Mutable<ScrollState>, state: Mutable<GridState>, rect: Rect) -> Self {
        Self { source, scroll, state, rect, on_action: None, on_activate: None, on_visible: None, display_editor: true, key: 0x6772_6964 }
    }

    pub fn on_action(mut self, f: impl Fn(GridAction) + 'static) -> Self {
        self.on_action = Some(Rc::new(f));
        self
    }

    pub fn on_activate(mut self, f: impl Fn() + 'static) -> Self {
        self.on_activate = Some(Rc::new(f));
        self
    }

    /// Reports the visible cell rectangles for this pane when it renders.
    /// Excludes the resident tiles outside the viewport.
    pub fn on_visible(mut self, f: impl Fn(CellRange) + 'static) -> Self {
        self.on_visible = Some(Rc::new(f));
        self
    }

    pub fn display_editor(mut self, show: bool) -> Self {
        self.display_editor = show;
        self
    }

    /// Keys for the grid's three viewports are `k`, `k + 1`, `k + 2`.
    pub fn key(mut self, k: u64) -> Self {
        self.key = k;
        self
    }

    /// The row header's width for rows up to `last`.
    pub fn header_width(last: u32) -> f32 {
        Self::header_width_at(last, 1.0)
    }

    /// The row header's width for rows up to `last` at `zoom`.
    pub fn header_width_at(last: u32, zoom: f32) -> f32 {
        let digits = (last as u64 + 1).to_string().len().max(2) as f32;
        ((digits * TEXT_SIZE * 0.62 + 2.0 * PAD + 6.0) * header_scale(zoom)).ceil()
    }

    /// The row headings' width and the column headings' height for `src`
    /// with rows up to `last`: scaled with its zoom, or none when it hides
    /// its headings.
    pub fn headings_size(src: &dyn GridSource, last: u32) -> (f32, f32) {
        if src.show_headings() { (Self::header_width_at(last, src.zoom()), header_height(src.zoom())) } else { (0.0, 0.0) }
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
        // Movable floats as drawn this frame (key, content rect), for presses.
        let placed: Rc<std::cell::RefCell<Vec<(u64, [f64; 4])>>> = Rc::new(std::cell::RefCell::new(Vec::new()));
        let buttons: Rc<std::cell::RefCell<Vec<(u64, [f64; 4])>>> = Rc::new(std::cell::RefCell::new(Vec::new()));
        let (rows, cols) = (src.rows(), src.cols());
        let ext = [src.col_left(cols), src.row_top(rows)];
        let mut st = self.scroll.get();
        let gs = self.state.get_cloned();
        let r = self.rect;

        // Headers' size; the body is what is left.
        let (tiles_r, _) = Self::tiles(&*src, st.physics_x.y, st.physics.y, r.w() as f64, r.h() as f64);
        // Headers scale with the zoom, within half to twice their size.
        let hs = header_scale(src.zoom());
        let headings = src.show_headings();
        let (header_w, header_h) = Self::headings_size(&*src, (tiles_r.1 + 1) * TILE_ROWS);
        let body = Rect::new(r.x() + header_w, r.y() + header_h, (r.w() - header_w).max(0.0), (r.h() - header_h).max(0.0));
        let frozen = src.frozen();
        let frozen = (frozen.0.min(rows), frozen.1.min(cols));
        let from = src.frozen_from();
        let from = (if frozen.0 > 0 { from.0.min(frozen.0 - 1) } else { 0 }, if frozen.1 > 0 { from.1.min(frozen.1 - 1) } else { 0 });
        let skip = [src.col_left(from.1), src.row_top(from.0)];
        let fz = [(src.col_left(frozen.1) - skip[0]).min(body.w() as f64 * 0.75), (src.row_top(frozen.0) - skip[1]).min(body.h() as f64 * 0.75)];
        let geom = Geom { body, rows, cols, ext, frozen, fz, skip };
        // Rows and columns above and left of the frozen ones are out of reach.
        if st.physics.min != skip[1] || st.physics_x.min != skip[0] {
            st.physics.set_min(skip[1]);
            st.physics_x.set_min(skip[0]);
            self.scroll.set(st);
        }

        // The scroll state knows the extent (and keeps a grid's zoom).
        let ymax = (ext[1] - body.h() as f64).max(st.physics.min);
        let xmax = (ext[0] - body.w() as f64).max(st.physics_x.min);
        if st.extent != Some(ext) || !st.fixed_zoom || st.physics.max != ymax || st.physics_x.max != xmax {
            st.extent = Some(ext);
            st.fixed_zoom = true;
            st.physics.set_max(ymax);
            st.physics_x.set_max(xmax);
            self.scroll.set(st);
        }
        let (x, y) = (st.physics_x.y, st.physics.y);
        if let Some(report) = self.on_visible.as_ref().filter(|_| rows > 0 && cols > 0) {
            let (fw, fh) = (fz[0], fz[1]);
            let (width, height) = (f64::from(body.w()), f64::from(body.h()));
            let row_runs = |a, b, r0, r1| visible_runs(a, b, r0, r1, |v| src.row_at(v), |i| src.row_top(i));
            let col_runs = |a, b, c0, c1| visible_runs(a, b, c0, c1, |v| src.col_at(v), |i| src.col_left(i));
            let mut shown_rows = row_runs(y + fh, y + height, frozen.0, rows - 1);
            let mut shown_cols = col_runs(x + fw, x + width, frozen.1, cols - 1);
            if fh > 0.0 {
                shown_rows.extend(row_runs(skip[1], skip[1] + fh, from.0, frozen.0 - 1));
            }
            if fw > 0.0 {
                shown_cols.extend(col_runs(skip[0], skip[0] + fw, from.1, frozen.1 - 1));
            }
            for (r0, r1) in shown_rows {
                for &(c0, c1) in &shown_cols {
                    report(CellRange { r0, r1, c0, c1 });
                }
            }
        }
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
            let at = |px: f64, py: f64| (q(body.x() as f64 + (px - bx)), q(body.y() as f64 + (py - by)));
            if tiles_r.0 <= tiles_r.1 && tiles_c.0 <= tiles_c.1 && rows > 0 && cols > 0 {
                let rr = (tiles_r.0 * TILE_ROWS, ((tiles_r.1 + 1) * TILE_ROWS).min(rows) - 1);
                let cr = (tiles_c.0 * TILE_COLS, ((tiles_c.1 + 1) * TILE_COLS).min(cols) - 1);
                emit_cells(ctx, &*src, &t, rr, cr, &at);
            }
            // The overlay: selection, active cell, editor.
            ctx.key(u64::MAX);
            let over = ctx.push(Node::vstack(local, 0.0));
            ctx.set_flex(over, FlexStyle { free: true, ..FlexStyle::default() });
            if rows > 0 && cols > 0 {
                // The selection, cut to the resident tiles (it can be a
                // whole column of a billion rows).
                let vis = CellRange { r0: tiles_r.0 * TILE_ROWS, c0: tiles_c.0 * TILE_COLS, r1: ((tiles_r.1 + 1) * TILE_ROWS).min(rows) - 1, c1: ((tiles_c.1 + 1) * TILE_COLS).min(cols) - 1 };
                emit_overlay(ctx, &*src, &t, &gs, vis, &at, self.display_editor);
            }
            ctx.pop();
            // Floats (charts) over the cells, those meeting the view.
            let gs = self.state.get_cloned();
            emit_floats(ctx, src.floats([x, y, body.w() as f64, body.h() as f64], &t), &t, &gs, &at, &placed, &buttons);
            ctx.pop();
        }
        ctx.pop();
        ctx.focusable(body_id);
        // Tab moves the cursor (and takes a formula guess), as in Excel;
        // Ctrl+Tab still moves focus out.
        ctx.takes_tab(body_id);
        let s = self.scroll.clone();
        let (zact, pinch) = (self.on_action.clone(), Rc::new(std::cell::Cell::new(1.0f32)));
        ctx.on_scroll(body_id, move |inp: &ScrollInput| {
            // Zoom gestures go to the app, which scales what it shows.
            let factor = match inp.kind {
                ScrollKind::Wheel { dy, modifiers, .. } if (modifiers.ctrl || modifiers.meta) && zact.is_some() => Some((1.0f32 - dy * 0.0025).clamp(0.8, 1.25)),
                ScrollKind::Magnify { scale, phase, .. } if zact.is_some() => {
                    if matches!(phase, crate::event::ScrollPhase::Begin) || pinch.get() <= 0.0 {
                        pinch.set(1.0);
                    }
                    let f = if pinch.get() > 0.0 { scale / pinch.get() } else { 1.0 };
                    pinch.set(scale);
                    Some(f.clamp(0.5, 2.0))
                }
                _ => None,
            };
            if let Some(f) = factor {
                if let Some(a) = &zact {
                    a(GridAction::Zoom(f));
                }
                return;
            }
            let mut st = s.get();
            st.input(inp.kind, inp.t, inp.node);
            s.set(st);
        });
        let s = self.scroll.clone();
        let (fsrc, fgst) = (src.clone(), self.state.clone());
        ctx.on_frame(body_id, move |t, node| {
            let mut st = s.get();
            let mut more = st.frame(t, node);
            // A drag held past the edge: scroll toward the pointer, and the
            // selection (or fill) follows to the cell now at the edge.
            let mut g = fgst.get_cloned();
            if let Some(p) = g.edge.filter(|_| matches!(g.drag, Drag::Cells | Drag::Fill { .. })) {
                let (ox, oy) = past_edge(&geom, p);
                let speed = |o: f32| if o == 0.0 { 0.0 } else { o.signum() as f64 * (o.abs() as f64 * 0.5).clamp(4.0, 60.0) };
                st.physics.set_max(geom.ext[1] - geom.body.h() as f64);
                st.physics_x.set_max(geom.ext[0] - geom.body.w() as f64);
                let (y0, x0) = (st.physics.y, st.physics_x.y);
                st.physics.stop();
                st.physics_x.stop();
                st.physics.set_position((y0 + speed(oy)).clamp(0.0, (geom.ext[1] - geom.body.h() as f64).max(0.0)));
                st.physics_x.set_position((x0 + speed(ox)).clamp(0.0, (geom.ext[0] - geom.body.w() as f64).max(0.0)));
                let cell = cell_at_edge(&*fsrc, &geom, &st, p);
                match g.drag {
                    Drag::Fill { from, .. } => g.drag = Drag::Fill { to: fill_reach(grow_to_merges(&*fsrc, g.selection()), cell), from },
                    _ => g.cursor = cell,
                }
                fgst.set(g);
                more = true;
            }
            if st != s.get() {
                s.set(st);
            }
            more
        });
        let drag: Rc<dyn Fn(DragPhase, crate::geom::Point)> = {
            let (src, sc, gst, act) = (src.clone(), self.scroll.clone(), self.state.clone(), self.on_action.clone());
            let activate = self.on_activate.clone();
            let placed = placed.clone();
            let buttons = buttons.clone();
            Rc::new(move |phase: DragPhase, p: crate::geom::Point| {
                if phase == DragPhase::Start { if let Some(activate) = &activate { activate(); } }
                let st = sc.get();
                let (cx, cy) = geom.content(&st, p);
                if geom.rows == 0 || geom.cols == 0 {
                    return;
                }
                // Past the body's edge, the cell at the edge (the view scrolls on to it).
                let cell = cell_at_edge(&*src, &geom, &st, p);
                let mut g = gst.get_cloned();
                g.edge = (phase == DragPhase::Move && matches!(g.drag, Drag::Cells | Drag::Fill { .. }) && past_edge(&geom, p) != (0.0, 0.0)).then_some(p);
                // A movable float under the press (the topmost) is picked and dragged.
                if let (Drag::Float { key, size, from, .. }, DragPhase::Move | DragPhase::End) = (g.drag, phase) {
                    let (dx, dy) = (cx - from.0, cy - from.1);
                    if phase == DragPhase::End {
                        g.drag = Drag::None;
                        if dx.abs() + dy.abs() > 3.0 {
                            if let Some(&(_, at0)) = placed.borrow().iter().find(|f| f.0 == key) {
                                if let Some(f) = &act {
                                    f(GridAction::PlaceFloat { key, at: float_moved(at0, size, dx, dy) });
                                }
                            }
                        }
                    } else {
                        g.drag = Drag::Float { key, size, from, dx, dy };
                    }
                    gst.set(g);
                    return;
                }
                if let (Drag::Fill { from, .. }, DragPhase::Move | DragPhase::End) = (g.drag, phase) {
                    let sel = grow_to_merges(&*src, g.selection());
                    // A press that hardly moved (a click on the handle) fills nothing.
                    let to = if (cx - from.0).abs() + (cy - from.1).abs() < 3.0 { sel } else { fill_reach(sel, cell) };
                    if phase == DragPhase::End {
                        g.drag = Drag::None;
                        if to != sel {
                            g.anchor = (to.r0, to.c0);
                            g.cursor = (to.r1, to.c1);
                            gst.set(g);
                            if let Some(f) = &act {
                                f(GridAction::Fill { from: sel, to, copy: false, flip: crate::input::pointer_modifiers().ctrl });
                            }
                            return;
                        }
                    } else {
                        g.drag = Drag::Fill { to, from };
                    }
                    gst.set(g);
                    return;
                }
                // In the editor: the press places the caret, a drag selects,
                // a double-click takes the word.
                if let (Drag::Caret, DragPhase::Move | DragPhase::End) = (g.drag, phase) {
                    if let Some(i) = editor_index(&*src, &g, cx, cy) {
                        if let Some(ed) = g.edit.as_mut() {
                            ed.set_caret(i, true);
                        }
                    }
                    if phase == DragPhase::End {
                        g.drag = Drag::None;
                    }
                    gst.set(g);
                    return;
                }
                if phase == DragPhase::Start && g.edit.is_some() && in_editor(&*src, &g, cx, cy) {
                    let i = editor_index(&*src, &g, cx, cy).unwrap_or(0);
                    let now = std::time::Instant::now();
                    let double = g.text_press.is_some_and(|t| now.duration_since(t) < DOUBLE_CLICK);
                    // A press in the editor: editing in place from now on.
                    g.entering = false;
                    g.text_press = Some(now);
                    if let Some(ed) = g.edit.as_mut() {
                        if double {
                            ed.select_word(i);
                        } else {
                            ed.set_caret(i, crate::input::pointer_modifiers().shift);
                        }
                    }
                    g.drag = if double { Drag::None } else { Drag::Caret };
                    gst.set(g);
                    return;
                }
                if phase == DragPhase::Start && src.fills() && g.edit.is_none() {
                    let sel = grow_to_merges(&*src, g.selection());
                    let (hx, hy) = (src.col_left(sel.c1 + 1), src.row_top(sel.r1 + 1));
                    if (cx - hx).abs() <= FILL_GRIP && (cy - hy).abs() <= FILL_GRIP {
                        g.picked = None;
                        let now = std::time::Instant::now();
                        if g.fill_press.is_some_and(|t| now.duration_since(t) < DOUBLE_CLICK) {
                            g.fill_press = None;
                            gst.set(g);
                            if let Some(f) = &act {
                                f(GridAction::FillToData(sel));
                            }
                            return;
                        }
                        g.fill_press = Some(now);
                        g.drag = Drag::Fill { to: sel, from: (cx, cy) };
                        gst.set(g);
                        return;
                    }
                }
                if phase == DragPhase::Start {
                    let pressed = buttons.borrow().iter().rev().find(|(_, a)| cx >= a[0] && cx < a[0] + a[2] && cy >= a[1] && cy < a[1] + a[3]).map(|b| b.0);
                    if let Some(key) = pressed {
                        commit(&mut g, &act);
                        g.picked = None;
                        gst.set(g);
                        if let Some(f) = &act {
                            f(GridAction::PressFloat(key));
                        }
                        return;
                    }
                    let hit = placed.borrow().iter().rev().find(|(_, a)| cx >= a[0] && cx < a[0] + a[2] + 4.0 && cy >= a[1] && cy < a[1] + a[3] + 4.0).copied();
                    if let Some((key, a)) = hit {
                        commit(&mut g, &act);
                        let size = g.picked == Some(key) && cx >= a[0] + a[2] - FLOAT_GRIP && cy >= a[1] + a[3] - FLOAT_GRIP;
                        g.picked = Some(key);
                        g.drag = Drag::Float { key, size, from: (cx, cy), dx: 0.0, dy: 0.0 };
                        gst.set(g);
                        return;
                    }
                    g.picked = None;
                }
                match phase {
                    DragPhase::Start => {
                        commit(&mut g, &act);
                        let at = src.merge_at(cell.0, cell.1).map_or(cell, |m| (m.r0, m.c0));
                        g.select(at.0, at.1);
                        let m = crate::input::pointer_modifiers();
                        if m.ctrl || m.meta {
                            gst.set(g);
                            if let Some(f) = &act {
                                f(GridAction::Follow { row: at.0, col: at.1 });
                            }
                            return;
                        }
                        g.drag = Drag::Cells;
                    }
                    DragPhase::Move if g.drag == Drag::Cells => g.cursor = cell,
                    DragPhase::End => {
                        let made = g.drag == Drag::Cells;
                        g.drag = Drag::None;
                        if made {
                            let sel = g.selection();
                            gst.set(g);
                            if let Some(f) = &act {
                                f(GridAction::Selected(sel));
                            }
                            return;
                        }
                    }
                    _ => return,
                }
                gst.set(g);
            })
        };
        {
            let d = drag.clone();
            ctx.on_drag(body_id, move |ph, p| d(ph, p));
        }
        let keys: Rc<dyn Fn(&Event)> = {
            let (src, sc, gst, act) = (src.clone(), self.scroll.clone(), self.state.clone(), self.on_action.clone());
            let activate = self.on_activate.clone();
            Rc::new(move |ev: &Event| {
                let Event::Key { kind, key, modifiers, chars } = ev else { return };
                if !kind.pressed() {
                    return;
                }
                if let Some(activate) = &activate { activate(); }
                let mut g = gst.get_cloned();
                let mut st = sc.get();
                if on_key(&*src, &geom, &mut g, &mut st, *key, *modifiers, chars, &act) {
                    gst.set(g);
                    if st != sc.get() {
                        sc.set(st);
                    }
                }
            })
        };
        {
            let k = keys.clone();
            ctx.on_key(body_id, move |ev| k(ev));
        }

        // ---- frozen rows and columns: bands over the body that keep them in view ----
        if frozen != (0, 0) && rows > 0 && cols > 0 {
            let (fw, fh) = (fz[0] as f32, fz[1] as f32);
            let scroll_node = Node::Scroll { rect: body, offset: 0.0, zoom: 1.0, stretch: 1.0, backdrop: None, content_h: 0.0, offset_x: 0.0, content_w: 0.0 };
            let all_c = (tiles_c.0 * TILE_COLS, (((tiles_c.1 + 1) * TILE_COLS).min(cols)).saturating_sub(1));
            let all_r = (tiles_r.0 * TILE_ROWS, (((tiles_r.1 + 1) * TILE_ROWS).min(rows)).saturating_sub(1));
            // (key, viewport, rows, columns, scrolls across, scrolls down)
            let mut bands: Vec<(u64, Rect, (u32, u32), (u32, u32), bool, bool)> = Vec::new();
            if frozen.0 > 0 && fh > 0.0 && tiles_c.0 <= tiles_c.1 {
                bands.push((3, Rect::new(body.x(), body.y(), body.w(), fh), (from.0, frozen.0 - 1), (all_c.0.max(frozen.1), all_c.1), true, false));
            }
            if frozen.1 > 0 && fw > 0.0 && tiles_r.0 <= tiles_r.1 {
                bands.push((4, Rect::new(body.x(), body.y(), fw, body.h()), (all_r.0.max(frozen.0), all_r.1), (from.1, frozen.1 - 1), false, true));
            }
            if frozen.0 > 0 && frozen.1 > 0 && fw > 0.0 && fh > 0.0 {
                bands.push((5, Rect::new(body.x(), body.y(), fw, fh), (from.0, frozen.0 - 1), (from.1, frozen.1 - 1), false, false));
            }
            for (k, rect, rr, cr, across, down) in bands {
                if rr.0 > rr.1 || cr.0 > cr.1 {
                    continue;
                }
                let (ox, oy) = (if across { bx } else { skip[0] }, if down { by } else { skip[1] });
                ctx.key(self.key + k);
                let id = ctx.push(Node::Scroll {
                    rect,
                    offset: if down { (y - by) as f32 } else { 0.0 },
                    zoom: 1.0,
                    stretch: 1.0,
                    backdrop: Some(t.bg_canvas()),
                    content_h: 0.0,
                    offset_x: if across { (x - bx) as f32 } else { 0.0 },
                    content_w: 0.0,
                });
                {
                    ctx.key(1);
                    let page = ctx.push(Node::vstack(Rect::new(rect.x(), rect.y(), ((ext[0] - ox).clamp(0.0, 3.0 * REBASE)) as f32, ((ext[1] - oy).clamp(0.0, 3.0 * REBASE)) as f32), 0.0));
                    ctx.set_flex(page, FlexStyle { free: true, ..FlexStyle::default() });
                    let at = |px: f64, py: f64| (q(body.x() as f64 + (px - ox)), q(body.y() as f64 + (py - oy)));
                    emit_cells(ctx, &*src, &t, rr, cr, &at);
                    ctx.key(u64::MAX);
                    let over = ctx.push(Node::vstack(rect, 0.0));
                    ctx.set_flex(over, FlexStyle { free: true, ..FlexStyle::default() });
                    emit_overlay(ctx, &*src, &t, &gs, CellRange { r0: rr.0, c0: cr.0, r1: rr.1, c1: cr.1 }, &at, self.display_editor);
                    ctx.pop();
                    // Floats meeting the band's cells (a chart in the frozen rows
                    // stays with them; the part past the freeze line scrolls in
                    // the body, as in Excel).
                    let (vx, vy) = (if across { x } else { skip[0] }, if down { y } else { skip[1] });
                    let (vw, vh) = (if across { rect.w() as f64 } else { fz[0] }, if down { rect.h() as f64 } else { fz[1] });
                    let band_floats: Vec<GridFloat> = src.floats([vx, vy, vw, vh], &t).into_iter().filter(|f| f.at[0] < vx + vw && f.at[0] + f.at[2] > vx && f.at[1] < vy + vh && f.at[1] + f.at[3] > vy).collect();
                    if !band_floats.is_empty() {
                        ctx.key(u64::MAX - 2);
                        let fl = ctx.push(Node::vstack(rect, 0.0));
                        ctx.set_flex(fl, FlexStyle { free: true, ..FlexStyle::default() });
                        emit_floats(ctx, band_floats, &t, &gs, &at, &placed, &buttons);
                        ctx.pop();
                    }
                    ctx.pop();
                }
                ctx.pop();
                ctx.focusable(id);
                ctx.takes_tab(id);
                let d = drag.clone();
                ctx.on_drag(id, move |ph, p| d(ph, p));
                let kk = keys.clone();
                ctx.on_key(id, move |ev| kk(ev));
                let (s, n) = (self.scroll.clone(), scroll_node.clone());
                ctx.on_scroll(id, move |inp: &ScrollInput| {
                    let mut st = s.get();
                    st.input(inp.kind, inp.t, &n);
                    s.set(st);
                });
            }
            // The freeze lines.
            if fh > 0.0 {
                ctx.add(Node::Rect { rect: Rect::new(body.x(), body.y() + fh - 1.0, body.w(), 1.5), fill: t.border_strong(), radius: 0.0 });
            }
            if fw > 0.0 {
                ctx.add(Node::Rect { rect: Rect::new(body.x() + fw - 1.0, body.y(), 1.5, body.h()), fill: t.border_strong(), radius: 0.0 });
            }
        }

        // ---- column headers: a viewport that moves across only ----
        let hstyle = text(type_size::XS * hs, Weight::Medium, t.text_secondary());
        let hstyle_on = text(type_size::XS * hs, Weight::Semibold, t.accent_fg());
        ctx.key(self.key + 1);
        let ch_rect = Rect::new(body.x(), r.y(), body.w(), header_h);
        let colh = ctx.push(Node::Scroll { rect: ch_rect, offset: 0.0, zoom: 1.0, stretch: 1.0, backdrop: Some(t.bg_surface()), content_h: 0.0, offset_x: (x - bx) as f32, content_w: 0.0 });
        if headings {
            ctx.key(1);
            let page = ctx.push(Node::vstack(Rect::new(ch_rect.x(), ch_rect.y(), ((ext[0] - bx).clamp(0.0, 3.0 * REBASE)) as f32, header_h), 0.0));
            ctx.set_flex(page, FlexStyle { free: true, ..FlexStyle::default() });
            if cols > 0 && tiles_c.0 <= tiles_c.1 {
              for tc in tiles_c.0..=tiles_c.1 {
                // One group per tile of columns, so a new base moves the
                // group and sends nothing inside it.
                let (c0, c1) = (tc * TILE_COLS, ((tc + 1) * TILE_COLS).min(cols) - 1);
                let gx = q(ch_rect.x() as f64 + (src.col_left(c0) - bx));
                ctx.key(1 << 40 | tc as u64);
                let grp = ctx.push(Node::vstack(Rect::new(gx, ch_rect.y(), (src.col_left(c1 + 1) - src.col_left(c0)) as f32, header_h), 0.0));
                ctx.set_flex(grp, FlexStyle { free: true, ..FlexStyle::default() });
                for c in c0..=c1 {
                    let (xl, xr) = (src.col_left(c), src.col_left(c + 1));
                    let cx = q(ch_rect.x() as f64 + (xl - bx));
                    let w = (xr - xl) as f32;
                    ctx.key(c as u64 + 1);
                    let on = c >= sel.c0 && c <= sel.c1;
                    let cell = ctx.push(Node::vstack(Rect::new(cx, ch_rect.y(), w, header_h), 0.0));
                    ctx.set_flex(cell, FlexStyle { free: true, ..FlexStyle::default() });
                    if on {
                        ctx.add(Node::Rect { rect: Rect::new(cx, ch_rect.y(), w, header_h), fill: t.accent_bg(), radius: 0.0 });
                    }
                    let label = src.col_label(c);
                    let st = if on { hstyle_on.clone() } else { hstyle.clone() };
                    let lw = crate::text::measure_chars(&label, &st);
                    if lw <= w {
                        ctx.add(Node::Text { rect: Rect::new(cx + q(((w - lw) / 2.0) as f64), ch_rect.y() + (header_h - 15.0 * hs) / 2.0, lw, 15.0 * hs), content: label, style: st });
                    }
                    ctx.add(Node::Rect { rect: Rect::new(cx + w - 1.0, ch_rect.y(), 1.0, header_h), fill: line, radius: 0.0 });
                    if w >= 30.0 {
                        outline_mark(ctx, &*src, false, c, cx + 2.0, ch_rect.y() + 4.0, &hstyle, t.accent_bg());
                    }
                    // Hidden columns after this one: a double line, as Excel marks them.
                    if w > 0.0 && c + 1 < cols && src.col_left(c + 2) <= xr {
                        ctx.add(Node::Rect { rect: Rect::new(cx + w - 4.0, ch_rect.y(), 1.0, header_h), fill: t.text_secondary(), radius: 0.0 });
                        ctx.add(Node::Rect { rect: Rect::new(cx + w - 1.0, ch_rect.y(), 1.0, header_h), fill: t.text_secondary(), radius: 0.0 });
                    }
                    ctx.pop();
                }
                ctx.pop();
              }
            }
            ctx.pop();
        }
        ctx.pop();
        let col_drag: Rc<dyn Fn(DragPhase, crate::geom::Point)> = {
            let (src, sc, gst, act) = (src.clone(), self.scroll.clone(), self.state.clone(), self.on_action.clone());
            let activate = self.on_activate.clone();
            Rc::new(move |phase: DragPhase, p: crate::geom::Point| {
                if geom.cols == 0 || geom.rows == 0 {
                    return;
                }
                let st = sc.get();
                let (cx, _) = geom.content(&st, crate::geom::Point::new(p.x, geom.body.y() + geom.fz[1] as f32));
                let c = src.col_at(cx.max(0.0));
                let mut g = gst.get_cloned();
                match (phase, g.drag) {
                    (DragPhase::Start, _) => {
                        if let Some(activate) = &activate { activate(); }
                        commit(&mut g, &act);
                        if src.col_left(c + 1) - src.col_left(c) >= 30.0
                            && (2.0..=15.0).contains(&(cx - src.col_left(c)))
                        {
                            if let (Some((start, end, _)), Some(action)) = (src.outline_control(false, c), &act) {
                                g.drag = Drag::None;
                                gst.set(g);
                                action(GridAction::Outline { rows: false, start, end });
                                return;
                            }
                        }
                        // Near a column's right edge: resize that column.
                        let edge_of = |c: u32| src.col_left(c + 1) - cx;
                        let grip = if edge_of(c).abs() <= RESIZE_GRIP as f64 {
                            Some(c)
                        } else if c > 0 && (cx - src.col_left(c)).abs() <= RESIZE_GRIP as f64 {
                            Some(c - 1)
                        } else {
                            None
                        };
                        let now = std::time::Instant::now();
                        if let Some(k) = grip.filter(|k| g.edge_press.is_some_and(|(c, t)| c == *k && now.duration_since(t) < DOUBLE_CLICK)) {
                            g.edge_press = None;
                            g.drag = Drag::None;
                            gst.set(g);
                            if let Some(a) = &act {
                                a(GridAction::FitCol(k));
                            }
                            return;
                        }
                        g.edge_press = grip.map(|k| (k, now));
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
                    (DragPhase::End, Drag::Resize { .. }) => {
                        g.drag = Drag::None;
                        gst.set(g);
                        if let Some(a) = &act {
                            a(GridAction::ResizeEnd);
                        }
                        return;
                    }
                    (DragPhase::End, Drag::Cols) => {
                        g.drag = Drag::None;
                        let sel = g.selection();
                        gst.set(g);
                        if let Some(a) = &act {
                            a(GridAction::Selected(sel));
                        }
                        return;
                    }
                    (DragPhase::End, _) => g.drag = Drag::None,
                    _ => return,
                }
                gst.set(g);
            })
        };
        {
            let d = col_drag.clone();
            ctx.on_drag(colh, move |ph, p| d(ph, p));
        }

        // ---- row headers: a viewport that moves down only ----
        ctx.key(self.key + 2);
        let rh_rect = Rect::new(r.x(), body.y(), header_w, body.h());
        let rowh = ctx.push(Node::Scroll { rect: rh_rect, offset: (y - by) as f32, zoom: 1.0, stretch: 1.0, backdrop: Some(t.bg_surface()), content_h: 0.0, offset_x: 0.0, content_w: 0.0 });
        if headings {
            ctx.key(1);
            let page = ctx.push(Node::vstack(Rect::new(rh_rect.x(), rh_rect.y(), header_w, ((ext[1] - by).clamp(0.0, 3.0 * REBASE)) as f32), 0.0));
            ctx.set_flex(page, FlexStyle { free: true, ..FlexStyle::default() });
            if rows > 0 && tiles_r.0 <= tiles_r.1 {
              for tr in tiles_r.0..=tiles_r.1 {
                let (r0, r1) = (tr * TILE_ROWS, ((tr + 1) * TILE_ROWS).min(rows) - 1);
                let gy = q(rh_rect.y() as f64 + (src.row_top(r0) - by));
                ctx.key(1 << 40 | tr as u64);
                let grp = ctx.push(Node::vstack(Rect::new(rh_rect.x(), gy, header_w, (src.row_top(r1 + 1) - src.row_top(r0)) as f32), 0.0));
                ctx.set_flex(grp, FlexStyle { free: true, ..FlexStyle::default() });
                for row in r0..=r1 {
                    let (yt, yb) = (src.row_top(row), src.row_top(row + 1));
                    if yb <= yt {
                        continue;
                    }
                    let cy = q(rh_rect.y() as f64 + (yt - by));
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
                    // By character widths: every row number is a new string.
                    let lw = crate::text::measure_chars(&label, &st);
                    ctx.add(Node::Text { rect: Rect::new(rh_rect.x() + q((header_w - PAD - 2.0 - lw) as f64), cy + q(((h - 15.0 * hs) / 2.0).max(0.0) as f64), lw, 15.0 * hs), content: label, style: st });
                    ctx.add(Node::Rect { rect: Rect::new(rh_rect.x(), cy + h - 1.0, header_w, 1.0), fill: line, radius: 0.0 });
                    if h >= 14.0 {
                        outline_mark(ctx, &*src, true, row, rh_rect.x() + 2.0, cy + 1.0, &hstyle, t.accent_bg());
                    }
                    // Hidden rows after this one: a double line.
                    if row + 1 < rows && src.row_top(row + 2) <= yb {
                        ctx.add(Node::Rect { rect: Rect::new(rh_rect.x(), cy + h - 4.0, header_w, 1.0), fill: t.text_secondary(), radius: 0.0 });
                        ctx.add(Node::Rect { rect: Rect::new(rh_rect.x(), cy + h - 1.0, header_w, 1.0), fill: t.text_secondary(), radius: 0.0 });
                    }
                    ctx.pop();
                }
                ctx.pop();
              }
            }
            ctx.pop();
        }
        ctx.pop();
        let row_drag: Rc<dyn Fn(DragPhase, crate::geom::Point)> = {
            let (src, sc, gst, act) = (src.clone(), self.scroll.clone(), self.state.clone(), self.on_action.clone());
            let activate = self.on_activate.clone();
            Rc::new(move |phase: DragPhase, p: crate::geom::Point| {
                if geom.cols == 0 || geom.rows == 0 {
                    return;
                }
                let st = sc.get();
                let (_, cy) = geom.content(&st, crate::geom::Point::new(geom.body.x() + geom.fz[0] as f32, p.y));
                let row = src.row_at(cy.max(0.0));
                let mut g = gst.get_cloned();
                match (phase, g.drag) {
                    (DragPhase::Start, _) => {
                        if let Some(activate) = &activate { activate(); }
                        commit(&mut g, &act);
                        if src.row_top(row + 1) - src.row_top(row) >= 14.0
                            && (2.0..=15.0).contains(&(p.x - r.x()))
                        {
                            if let (Some((start, end, _)), Some(action)) = (src.outline_control(true, row), &act) {
                                g.drag = Drag::None;
                                gst.set(g);
                                action(GridAction::Outline { rows: true, start, end });
                                return;
                            }
                        }
                        // Near a row's bottom edge: resize that row. A
                        // double-click fits it, or just below a double line
                        // shows the hidden rows there.
                        let (grip, fits) = if (src.row_top(row + 1) - cy).abs() <= RESIZE_GRIP as f64 {
                            (Some(row), Some(row))
                        } else if row > 0 && (cy - src.row_top(row)).abs() <= RESIZE_GRIP as f64 {
                            let k = src.row_at((src.row_top(row) - 0.5).max(0.0));
                            let hidden = k + 1 < row && cy >= src.row_top(row);
                            (Some(k), Some(if hidden { k + 1 } else { k }))
                        } else {
                            (None, None)
                        };
                        let now = std::time::Instant::now();
                        if let Some(k) = fits.filter(|k| g.row_edge_press.is_some_and(|(r, t)| r == *k && now.duration_since(t) < DOUBLE_CLICK)) {
                            g.row_edge_press = None;
                            g.drag = Drag::None;
                            gst.set(g);
                            if let Some(a) = &act {
                                a(GridAction::FitRow(k));
                            }
                            return;
                        }
                        g.row_edge_press = fits.map(|k| (k, now));
                        match grip {
                            Some(k) => g.drag = Drag::ResizeRow { row: k, y0: p.y, h0: src.row_top(k + 1) - src.row_top(k) },
                            None => {
                                g.anchor = (row, 0);
                                g.cursor = (row, geom.cols - 1);
                                g.drag = Drag::Rows;
                            }
                        }
                    }
                    (DragPhase::Move, Drag::ResizeRow { row, y0, h0 }) => {
                        if let Some(a) = &act {
                            a(GridAction::ResizeRow { row, height: (h0 + (p.y - y0) as f64).max(MIN_ROW_H) });
                        }
                        return;
                    }
                    (DragPhase::Move, Drag::Rows) => g.cursor = (row, geom.cols - 1),
                    (DragPhase::End, Drag::ResizeRow { .. }) => {
                        g.drag = Drag::None;
                        gst.set(g);
                        if let Some(a) = &act {
                            a(GridAction::ResizeEnd);
                        }
                        return;
                    }
                    (DragPhase::End, Drag::Rows) => {
                        g.drag = Drag::None;
                        let sel = g.selection();
                        gst.set(g);
                        if let Some(a) = &act {
                            a(GridAction::Selected(sel));
                        }
                        return;
                    }
                    (DragPhase::End, _) => g.drag = Drag::None,
                    _ => return,
                }
                gst.set(g);
            })
        };
        {
            let d = row_drag.clone();
            ctx.on_drag(rowh, move |ph, p| d(ph, p));
        }

        // ---- the frozen columns' and rows' labels, kept in view ----
        if headings && fz[0] > 0.0 && frozen.1 > 0 {
            ctx.key(self.key + 6);
            let fr = Rect::new(body.x(), r.y(), fz[0] as f32, header_h);
            let id = ctx.push(Node::Scroll { rect: fr, offset: 0.0, zoom: 1.0, stretch: 1.0, backdrop: Some(t.bg_surface()), content_h: 0.0, offset_x: 0.0, content_w: 0.0 });
            for c in from.1..frozen.1 {
                let (xl, xr) = (src.col_left(c), src.col_left(c + 1));
                let (cx, w) = (q(body.x() as f64 + xl - skip[0]), (xr - xl) as f32);
                let on = c >= sel.c0 && c <= sel.c1;
                if on {
                    ctx.add(Node::Rect { rect: Rect::new(cx, r.y(), w, header_h), fill: t.accent_bg(), radius: 0.0 });
                }
                let label = src.col_label(c);
                let st = if on { hstyle_on.clone() } else { hstyle.clone() };
                let lw = crate::text::measure_chars(&label, &st);
                if lw <= w {
                    ctx.add(Node::Text { rect: Rect::new(cx + q(((w - lw) / 2.0) as f64), r.y() + (header_h - 15.0 * hs) / 2.0, lw, 15.0 * hs), content: label, style: st });
                }
                ctx.add(Node::Rect { rect: Rect::new(cx + w - 1.0, r.y(), 1.0, header_h), fill: line, radius: 0.0 });
            }
            ctx.pop();
            let d = col_drag.clone();
            ctx.on_drag(id, move |ph, p| d(ph, p));
        }
        if headings && fz[1] > 0.0 && frozen.0 > 0 {
            ctx.key(self.key + 7);
            let fr = Rect::new(r.x(), body.y(), header_w, fz[1] as f32);
            let id = ctx.push(Node::Scroll { rect: fr, offset: 0.0, zoom: 1.0, stretch: 1.0, backdrop: Some(t.bg_surface()), content_h: 0.0, offset_x: 0.0, content_w: 0.0 });
            for row in from.0..frozen.0 {
                let (yt, yb) = (src.row_top(row), src.row_top(row + 1));
                if yb <= yt {
                    continue;
                }
                let (cy, h) = (q(body.y() as f64 + yt - skip[1]), (yb - yt) as f32);
                let on = row >= sel.r0 && row <= sel.r1;
                if on {
                    ctx.add(Node::Rect { rect: Rect::new(r.x(), cy, header_w, h), fill: t.accent_bg(), radius: 0.0 });
                }
                let label = src.row_label(row);
                let st = if on { hstyle_on.clone() } else { hstyle.clone() };
                let lw = crate::text::measure_chars(&label, &st);
                ctx.add(Node::Text { rect: Rect::new(r.x() + q((header_w - PAD - 2.0 - lw) as f64), cy + q(((h - 15.0 * hs) / 2.0).max(0.0) as f64), lw, 15.0 * hs), content: label, style: st });
                ctx.add(Node::Rect { rect: Rect::new(r.x(), cy + h - 1.0, header_w, 1.0), fill: line, radius: 0.0 });
            }
            ctx.pop();
            let d = row_drag.clone();
            ctx.on_drag(id, move |ph, p| d(ph, p));
        }

        // ---- the corner and the header edges ----
        let corner = ctx.add(Node::Rect { rect: Rect::new(r.x(), r.y(), header_w, header_h), fill: t.bg_surface(), radius: 0.0 });
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
    #[cfg_attr(debug_assertions, track_caller)]
    fn render(&self, ctx: &mut Ctx) {
        ctx.widget_site();
        self.emit(ctx);
    }
}

/// A 2 px ring inside `r`.
/// A position snapped to 1/64 px. Sums and differences of such values
/// below 2^17 are exact in `f32`, so a cell's place inside its tile (what
/// goes on the wire, [`Node::to_group`]) is the same wherever the tile is:
/// moving the base moves only the tiles' groups, and nothing in them is
/// sent again.
/// `r` grown until no merged area crosses its edge.
pub fn grow_to_merges(src: &dyn GridSource, mut r: CellRange) -> CellRange {
    for _ in 0..64 {
        let mut g = r;
        for m in src.merges_in(r) {
            g = CellRange { r0: g.r0.min(m.r0), c0: g.c0.min(m.c0), r1: g.r1.max(m.r1), c1: g.c1.max(m.c1) };
        }
        if g == r {
            break;
        }
        r = g;
    }
    r
}

/// The selection (cut to `vis`) and, when it is in `vis`, the active cell or
/// its editor, placed by `at` (content px to window px).
/// Floats placed by `at` (content px to window px); movable and pressable
/// ones noted in `placed` and `buttons` for presses.
type Spots = Rc<std::cell::RefCell<Vec<(u64, [f64; 4])>>>;
fn outline_mark(ctx: &mut Ctx, source: &dyn GridSource, rows: bool, at: u32, x: f32, y: f32, style: &TextStyle, fill: Color) {
    let Some((_, _, collapsed)) = source.outline_control(rows, at) else { return };
    ctx.add(Node::Rect { rect: Rect::new(x, y, 13.0, 13.0), fill, radius: 2.0 });
    ctx.add(Node::Text { rect: Rect::new(x + 3.0, y - 1.0, 10.0, 14.0), content: if collapsed { "+" } else { "−" }.into(), style: style.clone() });
}

fn emit_floats(ctx: &mut Ctx, floats: Vec<GridFloat>, t: &crate::theme::Semantic, gs: &GridState, at: &dyn Fn(f64, f64) -> (f32, f32), placed: &Spots, buttons: &Spots) {
    for f in floats {
        let (fx, fy) = at(f.at[0], f.at[1]);
        ctx.key((1u64 << 63) | f.key);
        let id = ctx.push(Node::vstack(Rect::new(fx, fy, f.at[2] as f32, f.at[3] as f32), 0.0));
        ctx.set_flex(id, FlexStyle { free: true, ..FlexStyle::default() });
        for n in f.nodes.iter() {
            ctx.add(shift(n, fx, fy));
        }
        if f.movable && gs.picked == Some(f.key) {
            // Picked: a frame, its corner handle, and where a drag puts it.
            let (fw, fh) = (f.at[2] as f32, f.at[3] as f32);
            ring(ctx, Rect::new(fx - 2.0, fy - 2.0, fw + 4.0, fh + 4.0), t.accent_fg());
            ctx.add(Node::Rect { rect: Rect::new(fx + fw - 6.0, fy + fh - 6.0, 9.0, 9.0), fill: t.accent_fg(), radius: 2.0 });
            if let Some((_, size, dx, dy)) = gs.float_drag().filter(|d| d.0 == f.key) {
                let g = float_moved(f.at, size, dx, dy);
                let (gx, gy) = at(g[0], g[1]);
                ring(ctx, Rect::new(gx, gy, g[2] as f32, g[3] as f32), Color::rgba(0.2, 0.45, 0.95, 0.7));
            }
        }
        let spot = (f.key, f.at);
        if f.movable && !placed.borrow().contains(&spot) {
            placed.borrow_mut().push(spot);
        }
        if f.pressable && !buttons.borrow().contains(&spot) {
            buttons.borrow_mut().push(spot);
        }
        ctx.pop();
    }
}

/// The fill handle at the selection's corner, and while it is dragged the
/// area it reaches, ringed.
fn emit_fill_handle(ctx: &mut Ctx, src: &dyn GridSource, t: &crate::theme::Semantic, gs: &GridState, sel: CellRange, vis: CellRange, at: &dyn Fn(f64, f64) -> (f32, f32)) {
    if let Some(to) = gs.fill_drag().filter(|to| *to != sel) {
        let cut = CellRange { r0: to.r0.max(vis.r0), c0: to.c0.max(vis.c0), r1: to.r1.min(vis.r1), c1: to.c1.min(vis.c1) };
        if cut.r0 <= cut.r1 && cut.c0 <= cut.c1 {
            let (sx, sy) = at(src.col_left(cut.c0), src.row_top(cut.r0));
            let (ex, ey) = at(src.col_left(cut.c1 + 1), src.row_top(cut.r1 + 1));
            let mut c = t.accent_fg();
            c.a = 0.6;
            ring(ctx, Rect::new(sx, sy, ex - sx, ey - sy), c);
        }
    }
    if vis.contains(sel.r1, sel.c1) {
        let (x, y) = at(src.col_left(sel.c1 + 1), src.row_top(sel.r1 + 1));
        ctx.add(Node::Rect { rect: Rect::new(x - 4.0, y - 4.0, 7.0, 7.0), fill: t.accent_fg(), radius: 1.0 });
    }
}

fn emit_overlay(ctx: &mut Ctx, src: &dyn GridSource, t: &crate::theme::Semantic, gs: &GridState, vis: CellRange, at: &dyn Fn(f64, f64) -> (f32, f32), display_editor: bool) {
    // A selection meeting part of a merged area shows all of it.
    let sel = grow_to_merges(src, gs.selection());
    let one = src.merge_at(gs.cursor.0, gs.cursor.1).unwrap_or(CellRange::of(gs.cursor, gs.cursor));
    let cut = CellRange { r0: sel.r0.max(vis.r0), c0: sel.c0.max(vis.c0), r1: sel.r1.min(vis.r1), c1: sel.c1.min(vis.c1) };
    if sel != one && cut.r0 <= cut.r1 && cut.c0 <= cut.c1 {
        let (sx, sy) = at(src.col_left(cut.c0), src.row_top(cut.r0));
        let (ex, ey) = at(src.col_left(cut.c1 + 1), src.row_top(cut.r1 + 1));
        let mut fill = t.accent_fill();
        fill.a = 0.12;
        ctx.add(Node::Rect { rect: Rect::new(sx, sy, ex - sx, ey - sy), fill, radius: 0.0 });
    }
    if src.fills() && gs.edit.is_none() {
        emit_fill_handle(ctx, src, t, gs, sel, vis, at);
    }
    let (ar, ac) = gs.cursor;
    if !vis.contains(ar, ac) {
        return;
    }
    // The active cell, or the whole merged area it is in.
    let (sx, sy) = at(src.col_left(one.c0), src.row_top(one.r0));
    let (ex, ey) = at(src.col_left(one.c1 + 1), src.row_top(one.r1 + 1));
    let (w, h) = (ex - sx, ey - sy);
    let Some(ed) = gs.edit.as_ref().filter(|_| display_editor) else {
        ring(ctx, Rect::new(sx, sy, w, h), t.accent_fg());
        return;
    };
    // The editor: as wide and tall as its lines need, from the cell.
    let style = TextStyle { family: font::SANS.into(), size: TEXT_SIZE * src.zoom(), weight: Weight::Regular, color: t.text_primary() };
    let text = ed.text();
    let wrap = editor_wrap(src, gs);
    let (tw, th) = editor_size(text, wrap, &style);
    let (ew, eh) = (w.max(tw + 2.0 * PAD + 2.0), h.max(th));
    ctx.add(Node::Rect { rect: Rect::new(sx, sy, ew, eh), fill: t.bg_canvas(), radius: 0.0 });
    let lh = style.size * 1.3;
    let lines = editor_lines(text, wrap, &style);
    let top = sy + editor_top(lines.len(), h, lh);
    let sel = ed.has_selection().then(|| ed.selection());
    for (k, line) in lines.iter().enumerate() {
        let ty = top + k as f32 * lh;
        let x_of = |i: usize| crate::text::measure(&text[line.start..i.clamp(line.start, line.end)], &style).w;
        if let Some(s) = sel.as_ref().filter(|s| s.start <= line.end && s.end >= line.start) {
            let (a, b) = (x_of(s.start), x_of(s.end));
            // A selection running on past the line's end shows a little of the break.
            let b = if s.end > line.end { b + 4.0 } else { b };
            ctx.add(Node::Rect { rect: Rect::new(sx + PAD + a, ty, b - a, lh), fill: t.selection_bg(), radius: 0.0 });
        }
        let lw = x_of(line.end);
        ctx.add(Node::Text { rect: Rect::new(sx + PAD, ty, lw, lh), content: text[line.clone()].to_string(), style: style.clone() });
        if (line.start..=line.end).contains(&ed.caret()) && (k + 1 == lines.len() || ed.caret() < lines[k + 1].start) {
            ctx.add(Node::Rect { rect: Rect::new(sx + PAD + x_of(ed.caret()), ty, 1.5, lh), fill: t.accent_fg(), radius: 0.0 });
        }
    }
    ring(ctx, Rect::new(sx, sy, ew, eh), t.accent_fg());
}

/// Cells `rr` × `cr` (inclusive): grid lines, contents and the merged areas
/// meeting them, placed by `at`, in groups keyed by tile as the body's.
fn emit_cells(ctx: &mut Ctx, src: &dyn GridSource, t: &crate::theme::Semantic, rr: (u32, u32), cr: (u32, u32), at: &dyn Fn(f64, f64) -> (f32, f32)) {
    let line = t.border_default();
    let grid = src.show_grid();
    let mut spills: Vec<(u32, u32, (u32, u32), CellView)> = Vec::new();
    for tr in rr.0 / TILE_ROWS..=rr.1 / TILE_ROWS {
        for tc in cr.0 / TILE_COLS..=cr.1 / TILE_COLS {
            let (r0, r1) = ((tr * TILE_ROWS).max(rr.0), ((tr + 1) * TILE_ROWS - 1).min(rr.1));
            let (c0, c1) = ((tc * TILE_COLS).max(cr.0), ((tc + 1) * TILE_COLS - 1).min(cr.1));
            let (top, bottom) = (src.row_top(r0), src.row_top(r1 + 1));
            let (left, right) = (src.col_left(c0), src.col_left(c1 + 1));
            let (tx, ty) = at(left, top);
            let (tw, th) = ((right - left) as f32, (bottom - top) as f32);
            ctx.key(2 + ((tr as u64) << 20 | tc as u64));
            let tile = ctx.push(Node::vstack(Rect::new(tx, ty, tw, th), 0.0));
            ctx.set_flex(tile, FlexStyle { free: true, ..FlexStyle::default() });
            for row in r0..=r1 {
                let (_, yb) = at(left, src.row_top(row + 1));
                if grid {
                    ctx.add(Node::Rect { rect: Rect::new(tx, yb - 1.0, tw, 1.0), fill: line, radius: 0.0 });
                }
            }
            let mut columns = Vec::with_capacity((c1 - c0 + 1) as usize);
            let mut xl = left;
            for col in c0..=c1 {
                let xr = src.col_left(col + 1);
                if xr > xl {
                    columns.push((col, xl, xr));
                    if grid {
                        let (x, _) = at(xr, top);
                        ctx.add(Node::Rect { rect: Rect::new(x - 1.0, ty, 1.0, th), fill: line, radius: 0.0 });
                    }
                }
                xl = xr;
            }
            for row in r0..=r1 {
                let (yt, yb) = (src.row_top(row), src.row_top(row + 1));
                if yb <= yt {
                    continue;
                }
                for &(col, xl, xr) in &columns {
                    if src.merge_at(row, col).is_some() {
                        continue;
                    }
                    let Some(mut cv) = src.cell(row, col) else { continue };
                    let (cx, cy) = at(xl, yt);
                    // Text running over empty neighbours is drawn by the layer below.
                    if let Some(span) = spill(src, row, col, &cv) {
                        spills.push((row, col, span, cv.clone()));
                        cv.text.clear();
                    }
                    draw_cell(ctx, t, &cv, cx, cy, (xr - xl) as f32, (yb - yt) as f32);
                }
            }
            ctx.pop();
        }
    }
    emit_spills(ctx, src, t, rr, cr, spills, at);
    let merged = src.merges_in(CellRange { r0: rr.0, c0: cr.0, r1: rr.1, c1: cr.1 });
    if merged.is_empty() {
        return;
    }
    ctx.key(u64::MAX - 1);
    let (lx, ly) = at(src.col_left(cr.0), src.row_top(rr.0));
    let (lx1, ly1) = at(src.col_left(cr.1 + 1), src.row_top(rr.1 + 1));
    let layer = ctx.push(Node::vstack(Rect::new(lx, ly, lx1 - lx, ly1 - ly), 0.0));
    ctx.set_flex(layer, FlexStyle { free: true, ..FlexStyle::default() });
    for m in merged {
        let (cx, cy) = at(src.col_left(m.c0), src.row_top(m.r0));
        let (ex, ey) = at(src.col_left(m.c1 + 1), src.row_top(m.r1 + 1));
        let (cw, ch) = (ex - cx, ey - cy);
        if cw <= 1.0 || ch <= 1.0 {
            continue;
        }
        ctx.add(Node::Rect { rect: Rect::new(cx, cy, cw - 1.0, ch - 1.0), fill: t.bg_canvas(), radius: 0.0 });
        if let Some(cv) = src.cell(m.r0, m.c0) {
            draw_cell(ctx, t, &cv, cx, cy, cw, ch);
        }
    }
    ctx.pop();
}

/// Visible neighbours ordinary text may run over, at most.
pub const SPILL_COLS: u32 = 32;

/// The width cell view `cv` needs to show whole, px: its widest line, its
/// indent, an icon, and the cell's padding. For fitting a column.
/// The height cell view `cv` needs in a column `width` px wide: its lines
/// (wrapped to the width when it wraps), px.
pub fn needed_height(cv: &CellView, width: f64) -> f64 {
    if cv.text.is_empty() {
        return 0.0;
    }
    let st = cell_style(cv, &crate::theme::Semantic::LIGHT);
    let n = text_lines(cv, (width as f32 - 2.0 * PAD - cv.indent as f32 * INDENT_STEP).max(1.0), &st).len().max(1);
    (n as f32 * st.size * 1.3 + 2.0 * LINE_PAD).ceil() as f64
}

/// Space above and below a cell's lines, px.
const LINE_PAD: f32 = 1.5;

/// The lines cell view `cv` shows in `avail` px: its own lines, each broken
/// where text may break (and inside a word too long for the line) when it
/// wraps.
fn text_lines(cv: &CellView, avail: f32, style: &TextStyle) -> Vec<String> {
    let width = if cv.wrap { avail.max(1.0) } else { f32::INFINITY };
    let text = cv.text.replace("\r\n", "\n");
    crate::text::lines_with(&text, style, width, &crate::wrap::Wrap { hyphens: false, max_lines: MAX_LINES as u16 })
}

/// The most lines one cell shows.
const MAX_LINES: usize = 200;

pub fn needed_width(cv: &CellView) -> f64 {
    if cv.text.is_empty() {
        return 0.0;
    }
    let st = cell_style(cv, &crate::theme::Semantic::LIGHT);
    let w = cv.text.lines().map(|l| crate::text::measure(l, &st).w).fold(0.0f32, f32::max);
    let icon = if cv.icon.is_some() { TEXT_SIZE + 4.0 } else { 0.0 };
    (w + 2.0 * PAD + cv.indent as f32 * INDENT_STEP + icon + 2.0).ceil() as f64
}

fn cell_style(cv: &CellView, t: &crate::theme::Semantic) -> TextStyle {
    let st = TextStyle { family: if cv.mono { font::MONO } else { font::SANS }.into(), size: cv.size.unwrap_or(TEXT_SIZE), weight: if cv.bold { Weight::Semibold } else { Weight::Regular }, color: cv.color.unwrap_or(t.text_primary()) };
    let mut st = crate::inline::style(&st, cv.marks & crate::inline::Emphasis::Italic as u8);
    if cv.marks & (crate::inline::Emphasis::Super as u8 | crate::inline::Emphasis::Sub as u8) != 0 {
        st.size *= crate::inline::SCRIPT_SCALE;
    }
    st
}

/// A cell's text node, with its underline and strikethrough under and
/// through what is shown.
fn cell_text(ctx: &mut Ctx, rect: Rect, content: String, style: TextStyle, marks: u8) {
    use crate::inline::{Emphasis, SCRIPT_SCALE, SCRIPT_SHIFT};
    let shift = style.size / SCRIPT_SCALE * SCRIPT_SHIFT;
    let rect = if marks & Emphasis::Super as u8 != 0 { Rect::new(rect.x(), rect.y() - shift, rect.w(), rect.h()) } else if marks & Emphasis::Sub as u8 != 0 { Rect::new(rect.x(), rect.y() + shift, rect.w(), rect.h()) } else { rect };
    let rules: Vec<Node> = crate::inline::rules(rect, &style, marks).collect();
    ctx.add(Node::Text { rect, content, style });
    for r in rules {
        ctx.add(r);
    }
}

/// The visible column at `col`, or the first one beyond its hidden run.
fn visible_col(src: &dyn GridSource, col: u32, cols: u32, right: bool) -> Option<u32> {
    if col >= cols { return None; }
    let x = src.col_left(col);
    if src.col_left(col + 1) > x { return Some(col); }
    if right {
        let next = src.col_at(x);
        return (next > col && src.col_left(next + 1) > x).then_some(next);
    }
    // Find the left edge of the zero-width run. Looking up x - epsilon
    // can pick the wrong column when column widths are fractional.
    let (mut low, mut high) = (0, col);
    while low < high {
        let mid = low + (high - low) / 2;
        if src.col_left(mid) < x { low = mid + 1; } else { high = mid; }
    }
    low.checked_sub(1)
}

fn next_visible_col(src: &dyn GridSource, col: u32, cols: u32, right: bool) -> Option<u32> {
    visible_col(src, if right { col + 1 } else { col.checked_sub(1)? }, cols, right)
}

/// Whether cell (`r`, `c`) shows nothing: text may run over it.
/// The most cells [`current_region`] looks at.
const MAX_REGION_LOOKS: u64 = 4_000_000;

/// Excel's current region: the rectangle around `at` grown while a cell
/// just outside it (diagonals too) holds something, at most
/// [`MAX_REGION_LOOKS`] cells looked at. The cell itself when nothing is
/// around it.
pub fn current_region(src: &dyn GridSource, at: (u32, u32)) -> CellRange {
    let (rows, cols) = (src.rows(), src.cols());
    let mut r = CellRange { r0: at.0, c0: at.1, r1: at.0, c1: at.1 };
    if rows == 0 || cols == 0 {
        return r;
    }
    let seen = std::cell::Cell::new(0u64);
    let filled = |row: u32, col: u32| {
        seen.set(seen.get() + 1);
        !blank(src, row, col)
    };
    loop {
        let before = r;
        // Each side grown as far as it goes before the next is looked at,
        // so a long column of data costs one pass, not one per row.
        while r.r1 + 1 < rows && seen.get() <= MAX_REGION_LOOKS && (r.c0.saturating_sub(1)..=(r.c1 + 1).min(cols - 1)).any(|c| filled(r.r1 + 1, c)) {
            r.r1 += 1;
        }
        while r.r0 > 0 && seen.get() <= MAX_REGION_LOOKS && (r.c0.saturating_sub(1)..=(r.c1 + 1).min(cols - 1)).any(|c| filled(r.r0 - 1, c)) {
            r.r0 -= 1;
        }
        while r.c1 + 1 < cols && seen.get() <= MAX_REGION_LOOKS && (r.r0.saturating_sub(1)..=(r.r1 + 1).min(rows - 1)).any(|row| filled(row, r.c1 + 1)) {
            r.c1 += 1;
        }
        while r.c0 > 0 && seen.get() <= MAX_REGION_LOOKS && (r.r0.saturating_sub(1)..=(r.r1 + 1).min(rows - 1)).any(|row| filled(row, r.c0 - 1)) {
            r.c0 -= 1;
        }
        if r == before || seen.get() > MAX_REGION_LOOKS {
            return r;
        }
    }
}

fn blank(src: &dyn GridSource, r: u32, c: u32) -> bool {
    src.merge_at(r, c).is_none() && src.cell(r, c).is_none_or(|v| v.text.is_empty())
}

/// The columns cell (`r`, `c`)'s text runs over when it is wider than the
/// cell, as spreadsheets show text: into empty neighbours on the side its
/// alignment points (both, centred), up to [`SPILL_COLS`]. None when it fits,
/// is a number, or has no empty neighbour.
fn spill(src: &dyn GridSource, r: u32, c: u32, cv: &CellView) -> Option<(u32, u32)> {
    if cv.text.is_empty() || cv.text.contains('\n') || cv.wrap || cv.shrink_to_fit || cv.rotation != 0 || cv.spread != Spread::None {
        return None;
    }
    let cols = src.cols();
    let last = cols.saturating_sub(1);
    if cv.align == CellAlign::Across {
        // The run: following cells that show nothing and centre across too.
        let mut b = c;
        while b < last && b - c < SPILL_COLS && src.merge_at(r, b + 1).is_none() && src.cell(r, b + 1).is_some_and(|v| v.text.is_empty() && v.align == CellAlign::Across) {
            b += 1;
        }
        if b > c {
            return Some((c, b));
        }
    }
    if cv.numeric {
        return None;
    }
    let cw = (src.col_left(c + 1) - src.col_left(c)) as f32;
    let room = cw - 2.0 * PAD - cv.indent as f32 * INDENT_STEP - if cv.icon.is_some() { 17.0 } else { 0.0 };
    let est = cv.text.chars().count() as f32 * TEXT_SIZE * 0.6;
    if est < room * 0.7 {
        return None;
    }
    let style = TextStyle { color: Color::TRANSPARENT, ..cell_style(cv, &crate::theme::Semantic::LIGHT) };
    let w = crate::text::measure_chars(&cv.text, &style);
    if w <= room {
        return None;
    }
    let need = (w - room) as f64;
    let (want_l, want_r) = match cv.align {
        CellAlign::Left => (0.0, need),
        CellAlign::Right => (need, 0.0),
        CellAlign::Center | CellAlign::Across => (need / 2.0, need / 2.0),
    };
    let extend = |want, right| {
        let mut end = c;
        let mut got = 0.0;
        for _ in 0..SPILL_COLS {
            if got >= want { break; }
            let Some(next) = next_visible_col(src, end, cols, right) else { break; };
            if !blank(src, r, next) { break; }
            got += src.col_left(next + 1) - src.col_left(next);
            end = next;
        }
        end
    };
    let (a, b) = (extend(want_l, false), extend(want_r, true));
    ((a, b) != (c, c)).then_some((a, b))
}

/// Text that runs over empty neighbours, for rows `rr` and columns `cr` (and
/// cells up to [`SPILL_COLS`] beside them whose text reaches in): the grid
/// lines it crosses hidden, then the text across its columns.
fn emit_spills(ctx: &mut Ctx, src: &dyn GridSource, t: &crate::theme::Semantic, rr: (u32, u32), cr: (u32, u32), mut spills: Vec<(u32, u32, (u32, u32), CellView)>, at: &dyn Fn(f64, f64) -> (f32, f32)) {
    let cols = src.cols();
    // Offscreen text can reach in only through a blank visible edge.
    let (Some(left), Some(right)) = (visible_col(src, cr.0, cols, true), visible_col(src, cr.1, cols, false)) else { return; };
    if left > right { return; }
    for row in rr.0..=rr.1 {
        if src.row_top(row + 1) <= src.row_top(row) {
            continue;
        }
        for (edge, outward) in [(left, false), (right, true)] {
            if (outward && edge + 1 == cols) || (!outward && edge == 0) || !blank(src, row, edge) {
                continue;
            }
            let mut col = edge;
            for _ in 0..SPILL_COLS {
                let Some(next) = next_visible_col(src, col, cols, outward) else { break; };
                col = next;
                if src.merge_at(row, col).is_some() { break; }
                let Some(cv) = src.cell(row, col).filter(|cv| !cv.text.is_empty()) else { continue; };
                if let Some(span) = spill(src, row, col, &cv).filter(|&(a, b)| b >= left && a <= right) {
                    spills.push((row, col, span, cv));
                }
                break;
            }
        }
    }
    if spills.is_empty() {
        return;
    }
    let (lx, ly) = at(src.col_left(cr.0), src.row_top(rr.0));
    let (lx1, ly1) = at(src.col_left(cr.1 + 1), src.row_top(rr.1 + 1));
    let mut layer = None;
    for (row, col, (a, b), cv) in spills {
        let (yt, yb) = (src.row_top(row), src.row_top(row + 1));
        {
            if layer.is_none() {
                ctx.key(u64::MAX - 3);
                let id = ctx.push(Node::vstack(Rect::new(lx, ly, lx1 - lx, ly1 - ly), 0.0));
                ctx.set_flex(id, FlexStyle { free: true, ..FlexStyle::default() });
                layer = Some(id);
            }
            let (x0, y0) = at(src.col_left(a), yt);
            let (x1, y1) = at(src.col_left(b + 1), yb);
            // The grid lines inside the run, where no fill hides them.
            let filled = |c: u32| c != col && src.cell(row, c).is_some_and(|v| v.fill.is_some());
            let mut k = a;
            while let Some(next) = next_visible_col(src, k, cols, true).filter(|&next| next <= b) {
                if !filled(k) && !filled(next) {
                    let (xr, _) = at(src.col_left(next), yt);
                    ctx.add(Node::Rect { rect: Rect::new(xr - 1.0, y0, 1.0, y1 - y0 - 1.0), fill: cv.fill.filter(|_| k == col || next == col).unwrap_or(t.bg_canvas()), radius: 0.0 });
                }
                k = next;
            }
            let style = cell_style(&cv, t);
            let lead = cv.indent as f32 * INDENT_STEP + if cv.icon.is_some() { 17.0 } else { 0.0 };
            let (shown, w) = fit_text(&CellView { align: CellAlign::Left, ..cv.clone() }, x1 - x0 - 2.0 * PAD - lead, &style);
            let (ox, _) = at(src.col_left(col), yt);
            let (ox1, _) = at(src.col_left(col + 1), yt);
            let tx0 = match cv.align {
                CellAlign::Left => ox + PAD + lead,
                CellAlign::Right => ox1 - PAD - w,
                // Centred on its own cell, kept inside the run.
                CellAlign::Center => q((((ox + ox1) / 2.0 - w / 2.0).max(x0 + PAD).min(x1 - PAD - w)) as f64),
                // Centred across the whole run.
                CellAlign::Across => q((((x0 + x1) / 2.0 - w / 2.0).max(x0 + PAD)) as f64),
            };
            let lh = style.size * 1.3;
            cell_text(ctx, Rect::new(tx0, y0 + line_top(cv.valign, y1 - y0, lh), w, lh), shown, style, cv.marks);
        }
    }
    if layer.is_some() {
        ctx.pop();
    }
}

/// One cell's ground, bar, borders, icon and text in the box (`cx`, `cy`,
/// `cw`, `ch`), window px.
fn draw_fill(ctx: &mut Ctx, cv: &CellView, rect: Rect) {
    use fresco_protocol::{FillPaint, FillParams, GradientStop, fill_verb::{CLOSE, LINE, MOVE}};
    let (x, y, w, h) = (rect.x(), rect.y(), rect.w(), rect.h());
    let mut verbs = Vec::new();
    let mut points = Vec::new();
    let mut quad = |x: f32, y: f32, w: f32, h: f32| {
        verbs.extend([MOVE, LINE, LINE, LINE, CLOSE]);
        points.extend([[x,y], [x+w,y], [x+w,y+h], [x,y+h]]);
    };
    let paint = if let Some((angle, stops)) = &cv.gradient {
        let (dy, dx) = (*angle as f32 / 1000.0).to_radians().sin_cos();
        let length = (w * dx.abs() + h * dy.abs()) / 2.0;
        let (cx, cy) = (x + w / 2.0, y + h / 2.0);
        let component = |color: u32, shift| ((color >> shift) & 255u32) as f32 / 255.0;
        quad(x, y, w, h);
        FillPaint::Linear { x0: cx-dx*length, y0: cy-dy*length, x1: cx+dx*length, y1: cy+dy*length,
            stops: stops.iter().map(|&(p,c)| GradientStop { offset: f32::from(p)/10000.0,
                r: component(c,16), g: component(c,8), b: component(c,0), a: 1.0 }).collect() }
    } else if let Some((kind, ink, paper)) = cv.pattern {
        ctx.add(Node::Rect { rect, fill: paper, radius: 0.0 });
        // Bound geometry even for unusually wide cells and merged ranges.
        let step = (w.max(h) / 48.0).ceil().max(1.0);
        let count = |extent: f32| (extent / step).ceil() as u32;
        for row in 0..count(h) {
            let mut col = 0;
            while col < count(w) {
                if !pattern_pixel(kind, col, row) {
                    col += 1;
                    continue;
                }
                let start = col;
                col += 1;
                while col < count(w) && pattern_pixel(kind, col, row) {
                    col += 1;
                }
                let (left, top) = (start as f32 * step, row as f32 * step);
                quad(x + left, y + top, (col as f32 * step).min(w) - left, step.min(h - top));
            }
        }
        FillPaint::Solid { r: ink.r, g: ink.g, b: ink.b, a: ink.a }
    } else {
        if let Some(fill) = cv.fill {
            ctx.add(Node::Rect { rect, fill, radius: 0.0 });
        }
        return;
    };
    if !points.is_empty() {
        ctx.add(Node::Fill(std::sync::Arc::new(FillParams {
            rule: 0, transform: [1.0, 0.0, 0.0, 1.0, 0.0, 0.0], paint,
            opacity: 1.0, clip: Some([x, y, x+w, y+h]), verbs, points,
        })));
    }
}

/// The same fill pattern used by the grid, as an eight-pixel repeating tile.
pub fn cell_pattern(kind: &str) -> [u8; 8] {
    std::array::from_fn(|row| (0..8).fold(0, |bits, col| bits | (u8::from(pattern_pixel(kind, col, row as u32)) << col)))
}

fn pattern_pixel(kind: &str, x: u32, y: u32) -> bool {
    let (x,y) = (x % 8, y % 8);
    let (down, up) = ((x+8-y)%4, (x+y)%4);
    match kind {
        "darkGray" => x%2 == 0 || y%2 == 0,
        "mediumGray" => (x+y)%2 == 0,
        "lightGray" => x%2 == 0 && y%2 == 0,
        "darkHorizontal" => y%4 < 2,
        "darkVertical" => x%4 < 2,
        "darkDown" => down < 2,
        "darkUp" => up < 2,
        "darkGrid" => x%4 < 2 || y%4 < 2,
        "darkTrellis" => up < 2 || down < 2,
        "lightHorizontal" => y%4 == 0,
        "lightVertical" => x%4 == 0,
        "lightDown" => down == 0,
        "lightUp" => up == 0,
        "lightGrid" => x%4 == 0 || y%4 == 0,
        "lightTrellis" => up == 0 || down == 0,
        "gray125" => x % 4 == 2 * (y % 2) && y % 2 == (x / 4 + y / 2) % 2,
        "gray0625" => x % 4 == 0 && y % 4 == 0,
        _ => false,
    }
}

fn draw_rotated(ctx: &mut Ctx, cv: &CellView, style: &TextStyle, rect: Rect) -> bool {
    use crate::geom::Point;
    let (x, y, w, h) = (rect.x(), rect.y(), rect.w(), rect.h());
    if cv.rotation == 255 {
        use unicode_segmentation::UnicodeSegmentation;
        let line_h = style.em() * 1.1;
        let lines = cv.text.graphemes(true).take(((h / line_h).floor() as usize).min(MAX_LINES)).collect::<Vec<_>>();
        let top = y + line_top(cv.valign, h, lines.len() as f32 * line_h);
        for (i, line) in lines.into_iter().enumerate() {
            if line != "\n" {
                let advance = crate::text::measure_chars(line, style).min(w);
                ctx.add(Node::Text { rect: Rect::new(x+(w-advance)/2.0, top+i as f32*line_h, advance, line_h),
                    content: line.to_string(), style: style.clone() });
            }
        }
        return true;
    }
    let Some(face) = &cv.rotation_face else { return false };
    let Some(font) = swash::FontRef::from_index(face.bytes(), face.index() as usize) else { return false };
    let angle = -f32::from(cv.rotation).to_radians();
    let (sin, cos) = angle.sin_cos();
    let (s, c) = (sin.abs(), cos.abs());
    let cap = style.em() * 0.85;
    let fit = ((w-s*cap) / c.max(0.0001)).min((h-c*cap) / s.max(0.0001));
    if fit <= 0.0 {
        return true;
    }
    let text = cv.text.replace(['\r', '\n'], " ");
    let (shown, _) = fit_text(&CellView { text, ..cv.clone() }, fit, style);
    let mut glyphs = Vec::new();
    let mut advance = 0.0;
    thread_local! { static SHAPER: std::cell::RefCell<swash::shape::ShapeContext> = Default::default(); }
    SHAPER.with_borrow_mut(|context| {
        let mut shape = context.builder(font).size(style.em()).build();
        shape.add_str(&shown);
        shape.shape_with(|cluster| {
            for g in cluster.glyphs {
                glyphs.push(crate::glyphs::Glyph { id: g.id, dx: advance + g.x, dy: -g.y });
                advance += g.advance;
            }
        });
    });
    let (box_w, box_h) = (advance*c+cap*s, advance*s+cap*c);
    let left = x + match cv.align {
        CellAlign::Left => 0.0,
        CellAlign::Right => (w-box_w).max(0.0),
        _ => ((w-box_w)/2.0).max(0.0),
    };
    let top = y + line_top(cv.valign, h, box_h);
    // Rotate about the middle of the upright ink box, then align its bounds.
    let ox = left + box_w/2.0 - cos*advance/2.0 - sin*cap/2.0;
    let oy = top + box_h/2.0 - sin*advance/2.0 + cos*cap/2.0;
    ctx.add(Node::Glyphs { origin: Point::new(ox, oy), face: face.clone(), size: style.em(), color: style.color,
        glyphs: glyphs.into(), turn: Some(crate::node::turn_by(angle)) });
    true
}

fn draw_cell(ctx: &mut Ctx, t: &crate::theme::Semantic, cv: &CellView, cx: f32, cy: f32, cw: f32, ch: f32) {
    draw_fill(ctx, cv, Rect::new(cx, cy, cw, ch));
    for (k, edge) in cv.diagonals.iter().enumerate() {
        if let Some((width, color)) = *edge {
            let pad = width.min(cw / 2.0).min(ch / 2.0);
            let (top, bottom) = (cy+pad, cy+ch-pad);
            let (y0,y1) = if k == 0 { (top,bottom) } else { (bottom,top) };
            ctx.add(Node::Path { p0: (cx+pad,y0), p1: (cx+cw-pad,y1), width, color });
        }
    }
    if let Some((share, c)) = cv.bar {
        let w = ((cw - 4.0) * share.clamp(0.0, 1.0)).max(0.0);
        ctx.add(Node::Rect { rect: Rect::new(cx + 2.0, cy + 2.0, q(w as f64), (ch - 5.0).max(0.0)), fill: c, radius: 0.0 });
    }
    if let Some(sp) = &cv.spark {
        spark_shapes(sp, cx, cy, cw, ch, |node| { ctx.add(node); });
    }
    for (k, b) in cv.borders.iter().enumerate() {
        let Some((w, c)) = *b else { continue };
        let r = match k {
            0 => Rect::new(cx - 1.0, cy - 1.0, w, ch + 1.0),
            1 => Rect::new(cx + cw - w, cy - 1.0, w, ch + 1.0),
            2 => Rect::new(cx - 1.0, cy - 1.0, cw + 1.0, w),
            _ => Rect::new(cx - 1.0, cy + ch - w, cw + 1.0, w),
        };
        ctx.add(Node::Rect { rect: r, fill: c, radius: 0.0 });
    }
    // The icon at the left; text keeps clear of it.
    let mut lead = cv.indent as f32 * INDENT_STEP;
    if let Some((icon, c)) = cv.icon {
        let s = (ch - 6.0).clamp(0.0, 14.0);
        if s >= 6.0 && cw > s + 4.0 {
            draw_icon(ctx, icon, c, t.border_default(), cx + 3.0 + lead, cy + q(((ch - s) / 2.0) as f64), s);
            lead += s + 3.0;
        }
    }
    if cv.note && cw > 10.0 && ch > 8.0 {
        let (x1, y0) = (cx + cw - 1.0, cy);
        ctx.add(poly(&[(x1 - 6.0, y0), (x1, y0), (x1, y0 + 6.0)], Color::rgba(0.84, 0.18, 0.15, 1.0), None));
    }
    if cv.text.is_empty() {
        return;
    }
    let mut style = cell_style(cv, t);
    let avail = (cw - 2.0 * PAD - lead).max(0.0);
    if cv.shrink_to_fit && !cv.wrap && !cv.text.contains('\n') && cv.rotation == 0 {
        let width = crate::text::measure_chars(&cv.text, &style);
        if width > avail {
            style.size = (style.size * avail / width * 0.98).max(3.0);
        }
    }
    let lh = style.size * 1.3;
    // An accounting underline runs across the cell; the text is then drawn without it.
    use crate::inline::Emphasis;
    let under = Emphasis::Underline as u8 | Emphasis::Double as u8;
    let plain;
    let cv = if cv.marks & (Emphasis::Accounting as u8 | Emphasis::Underline as u8) == Emphasis::Accounting as u8 | Emphasis::Underline as u8 && !cv.wrap && !cv.text.contains('\n') && cv.rotation == 0 {
        let r = Rect::new(cx + PAD, cy + line_top(cv.valign, ch, lh), (cw - 2.0 * PAD).max(0.0), lh);
        for n in crate::inline::rules(r, &style, cv.marks & under) {
            ctx.add(n);
        }
        plain = CellView { marks: cv.marks & !(under | Emphasis::Accounting as u8), ..cv.clone() };
        &plain
    } else {
        cv
    };
    if cv.rotation != 0 && draw_rotated(ctx, cv, &style, Rect::new(cx+PAD+lead, cy, avail, ch)) {
        return;
    }
    if cv.spread == Spread::Fill && !cv.text.contains('\n') {
        let avail = cw - 2.0 * PAD - lead;
        let w = crate::text::measure_chars(&cv.text, &style);
        let n = if w > 0.0 { ((avail / w).floor() as usize).clamp(1, MAX_FILL_REPEAT) } else { 1 };
        let (shown, w) = fit_text(&CellView { text: cv.text.repeat(n), spread: Spread::None, ..cv.clone() }, avail, &style);
        cell_text(ctx, Rect::new(cx + PAD + lead, cy + line_top(cv.valign, ch, lh), w, lh), shown, style, cv.marks);
        return;
    }
    let spread = matches!(cv.spread, Spread::Justify | Spread::Distributed);
    if !cv.numeric && (cv.wrap || spread || cv.text.contains('\n')) {
        // Several lines: centred down the cell when they fit, else from
        // the top, as many as fit whole (at least one).
        let avail = cw - 2.0 * PAD - lead;
        // Each line, and whether it ends its paragraph (Justify leaves those as they are).
        let lines: Vec<(String, bool)> = if spread {
            let width = avail.max(1.0);
            let wrap = crate::wrap::Wrap { hyphens: false, max_lines: MAX_LINES as u16 };
            cv.text.replace("\r\n", "\n").split('\n').flat_map(|para| {
                let ls = crate::text::lines_with(para, &style, width, &wrap);
                let n = ls.len();
                ls.into_iter().enumerate().map(move |(i, l)| (l, i + 1 == n))
            }).take(MAX_LINES).collect()
        } else {
            text_lines(cv, avail, &style).into_iter().map(|l| (l, false)).collect()
        };
        let room = (((ch - 2.0 * LINE_PAD) / lh).floor() as usize).max(1);
        let n = lines.len().min(room);
        let top = if lines.len() <= room { cy + line_top(cv.valign, ch, n as f32 * lh) } else { cy + LINE_PAD };
        // Vertical spread: the first line at the top, the last at the bottom.
        let (top, step) = match (cv.vspread, n) {
            (true, 1) => (cy + line_top(CellVAlign::Center, ch, lh), lh),
            (true, _) => (cy + LINE_PAD, lh + ((ch - 2.0 * LINE_PAD - n as f32 * lh) / (n - 1) as f32).max(0.0)),
            _ => (top, lh),
        };
        for (i, (line, last)) in lines.iter().take(n).enumerate() {
            let y = q((top + i as f32 * step) as f64);
            if spread && spread_line(ctx, line, cx + PAD + lead, y, avail, lh, &style, cv, *last) {
                continue;
            }
            let (shown, w) = fit_text(&CellView { text: line.clone(), ..cv.clone() }, avail, &style);
            let tx0 = match cv.align {
                CellAlign::Left => cx + PAD + lead,
                CellAlign::Center | CellAlign::Across => cx + q(((cw - w) / 2.0) as f64),
                CellAlign::Right => cx + q((cw - PAD - w) as f64),
            };
            cell_text(ctx, Rect::new(tx0, y, w, lh), shown, style.clone(), cv.marks);
        }
        return;
    }
    if let Some((left, fill, right)) = cv.fill_at.and_then(|at| split_fill(&cv.text, at)) {
        let avail = cw - 2.0 * PAD - lead;
        let (wl, wr) = (crate::text::measure_chars(left, &style), crate::text::measure_chars(right, &style));
        let wf = crate::text::measure_chars(fill, &style);
        if wl + wf + wr <= avail {
            let y = cy + line_top(cv.valign, ch, lh);
            let x0 = cx + PAD + lead;
            let x1 = cx + q((cw - PAD - wr) as f64);
            // The fill character as many times as fit between the two.
            let n = if fill.trim().is_empty() || wf <= 0.0 { 0 } else { (((x1 - x0 - wl) / wf).floor().max(1.0) as usize).min(MAX_FILL_REPEAT) };
            let mid = fill.repeat(n);
            if !left.is_empty() {
                cell_text(ctx, Rect::new(x0, y, wl, lh), left.to_string(), style.clone(), cv.marks);
            }
            if n > 0 {
                cell_text(ctx, Rect::new(x0 + wl, y, wf * n as f32, lh), mid, style.clone(), cv.marks);
            }
            cell_text(ctx, Rect::new(x1, y, wr, lh), right.to_string(), style, cv.marks);
            return;
        }
    }
    let (shown, w) = fit_text(cv, cw - 2.0 * PAD - lead, &style);
    let tx0 = match cv.align {
        CellAlign::Left => cx + PAD + lead,
        CellAlign::Center | CellAlign::Across => cx + q(((cw - w) / 2.0) as f64),
        CellAlign::Right => cx + q((cw - PAD - w) as f64),
    };
    cell_text(ctx, Rect::new(tx0, cy + line_top(cv.valign, ch, lh), w, lh), shown, style, cv.marks);
}

/// Draws `line` with its words spread over `avail` (Justify: not the last
/// line; Distributed: every line, a lone word centred). False, nothing
/// drawn, when it is drawn as it is.
#[allow(clippy::too_many_arguments)]
fn spread_line(ctx: &mut Ctx, line: &str, x: f32, y: f32, avail: f32, lh: f32, style: &TextStyle, cv: &CellView, last: bool) -> bool {
    let words: Vec<&str> = line.split_whitespace().collect();
    if words.is_empty() || (cv.spread == Spread::Justify && (last || words.len() < 2)) {
        return false;
    }
    let widths: Vec<f32> = words.iter().map(|w| crate::text::measure_chars(w, style)).collect();
    let total: f32 = widths.iter().sum();
    if total > avail {
        return false;
    }
    if words.len() == 1 {
        cell_text(ctx, Rect::new(x + q(((avail - total) / 2.0) as f64), y, total, lh), words[0].to_string(), style.clone(), cv.marks);
        return true;
    }
    let gap = (avail - total) / (words.len() - 1) as f32;
    let mut at = x;
    for (word, w) in words.iter().zip(&widths) {
        cell_text(ctx, Rect::new(q(at as f64), y, *w, lh), word.to_string(), style.clone(), cv.marks);
        at += w + gap;
    }
    true
}

/// F4 in a formula: `text` with the cell reference touching byte `caret`
/// moved to its next anchoring (A1 → $A$1 → A$1 → $A1 → A1; both ends of
/// a range together), and the caret after it. `None` outside a formula or
/// a reference.
pub fn cycle_ref(text: &str, caret: usize) -> Option<(String, usize)> {
    if !text.starts_with('=') || caret > text.len() || !text.is_char_boundary(caret) {
        return None;
    }
    let tok = |c: char| c.is_ascii_alphanumeric() || c == '$' || c == ':';
    let start = text[..caret].char_indices().rev().take_while(|&(_, c)| tok(c)).last().map_or(caret, |(i, _)| i);
    let end = caret + text[caret..].chars().take_while(|&c| tok(c)).map(char::len_utf8).sum::<usize>();
    if start == end || text[end..].starts_with('(') {
        return None;
    }
    // A reference starts the token: not part of a name such as `Tax_A1`.
    if text[..start].chars().next_back().is_some_and(|c| c == '_' || c == '.' || c.is_alphanumeric()) {
        return None;
    }
    let parts: Vec<&str> = text[start..end].split(':').collect();
    if parts.len() > 2 {
        return None;
    }
    let mut cells = Vec::new();
    for p in &parts {
        let col_abs = p.starts_with('$');
        let rest = p.trim_start_matches('$');
        let letters = rest.chars().take_while(char::is_ascii_alphabetic).count();
        if !(1..=3).contains(&letters) {
            return None;
        }
        let (col, rest) = rest.split_at(letters);
        let row_abs = rest.starts_with('$');
        let row = rest.trim_start_matches('$');
        if row.is_empty() || !row.chars().all(|c| c.is_ascii_digit()) || row.starts_with('0') || rest.matches('$').count() > 1 {
            return None;
        }
        cells.push((col, row, col_abs, row_abs));
    }
    let (c0, r0) = (cells[0].2, cells[0].3);
    let (col_abs, row_abs) = match (c0, r0) {
        (false, false) => (true, true),
        (true, true) => (false, true),
        (false, true) => (true, false),
        (true, false) => (false, false),
    };
    let dollar = |b: bool| if b { "$" } else { "" };
    let new: Vec<String> = cells.iter().map(|(col, row, _, _)| format!("{}{col}{}{row}", dollar(col_abs), dollar(row_abs))).collect();
    let new = new.join(":");
    let out = format!("{}{new}{}", &text[..start], &text[end..]);
    Some((out, start + new.len()))
}

/// The most times a `*` fill's character is repeated in one cell.
const MAX_FILL_REPEAT: usize = 512;

/// `text` around its fill character at byte `at`: before, the character, after.
/// `None` when `at` is not a character in `text`.
fn split_fill(text: &str, at: usize) -> Option<(&str, &str, &str)> {
    let rest = text.get(at..)?;
    let n = rest.chars().next()?.len_utf8();
    Some((&text[..at], &rest[..n], &rest[n..]))
}

/// The formatting a Ctrl (Ctrl+Shift) key asks for: see [`GridAction::Format`].
/// A shifted digit may arrive as the digit or as its symbol.
fn format_key(chars: &str, shift: bool) -> Option<char> {
    let c = chars.chars().next()?.to_ascii_lowercase();
    if chars.chars().count() != 1 {
        return None;
    }
    if shift {
        return match c {
            '`' | '~' => Some('~'),
            '1' | '!' => Some('!'),
            '2' | '@' => Some('@'),
            '3' | '#' => Some('#'),
            '4' | '$' => Some('$'),
            '5' | '%' => Some('%'),
            '6' | '^' => Some('^'),
            '7' | '&' => Some('&'),
            '9' | '(' => Some('('),
            '0' | ')' => Some(')'),
            '=' | '+' => Some('+'),
            '-' | '_' => Some('_'),
            'l' => Some('L'),
            _ => None,
        };
    }
    matches!(c, 'b' | 'i' | 'u' | '5' | '1' | '`' | '9' | '0' | '-' | '+' | 't' | 'l' | 'k' | '[' | ']').then_some(c)
}

/// How far down a cell `ch` px tall lines `th` px tall start, by `v`.
fn line_top(v: CellVAlign, ch: f32, th: f32) -> f32 {
    let y = match v {
        CellVAlign::Top => LINE_PAD,
        CellVAlign::Center => (ch - th) / 2.0,
        CellVAlign::Bottom => ch - th - LINE_PAD,
    };
    q(y.clamp(0.0, (ch - th).max(0.0)) as f64)
}

fn q(v: f64) -> f32 {
    ((v * 64.0).round() / 64.0) as f32
}

fn ring(ctx: &mut Ctx, r: Rect, c: Color) {
    let k = 2.0;
    ctx.add(Node::Rect { rect: Rect::new(r.x(), r.y(), r.w(), k), fill: c, radius: 0.0 });
    ctx.add(Node::Rect { rect: Rect::new(r.x(), r.y() + r.h() - k, r.w(), k), fill: c, radius: 0.0 });
    ctx.add(Node::Rect { rect: Rect::new(r.x(), r.y(), k, r.h()), fill: c, radius: 0.0 });
    ctx.add(Node::Rect { rect: Rect::new(r.x() + r.w() - k, r.y(), k, r.h()), fill: c, radius: 0.0 });
}

/// The text a cell shows in `avail` px and its width (by cached character widths:
/// a sheet of distinct numbers must not ask the compositor for each one): whole when
/// it fits; a number too wide becomes `#`s (a cut number would lie); other
/// text is cut with an ellipsis.
fn fit_text(cv: &CellView, avail: f32, style: &TextStyle) -> (String, f32) {
    let est = cv.text.chars().count() as f32 * style.size * 0.6;
    let w = if est < avail * 0.7 && !matches!(cv.align, CellAlign::Right | CellAlign::Center | CellAlign::Across) {
        // Clearly fits and left-aligned: no need to measure.
        return (cv.text.clone(), est);
    } else {
        crate::text::measure_chars(&cv.text, style)
    };
    if w <= avail {
        return (cv.text.clone(), w);
    }
    if avail <= 0.0 {
        return (String::new(), 0.0);
    }
    if cv.numeric {
        let one = crate::text::measure_chars("#", style).max(1.0);
        let n = (avail / one).floor().max(1.0) as usize;
        let s = "#".repeat(n);
        return (s, n as f32 * one);
    }
    // Cut by the measured width per character, then check.
    let chars: Vec<char> = cv.text.chars().collect();
    let mut n = ((avail / w) * chars.len() as f32) as usize;
    loop {
        let s: String = chars[..n.min(chars.len())].iter().collect::<String>() + "…";
        let sw = crate::text::measure_chars(&s, style);
        if sw <= avail || n == 0 {
            return if n == 0 { (String::new(), 0.0) } else { (s, sw) };
        }
        n -= 1;
    }
}

/// AutoComplete after a character typed at the end: the rest of the entry
/// the source offers, selected, so the next key replaces it and Enter keeps it.
/// A formula's guess (a function name) is taken with Tab and dropped by the
/// keys that move or finish; a text's is kept by Enter, as in Excel.
fn complete(src: &dyn GridSource, at: (u32, u32), ed: &mut TextEdit) -> bool {
    let typed = ed.text().to_string();
    if typed.is_empty() || ed.has_selection() || ed.caret() != typed.len() {
        return false;
    }
    let Some(full) = src.complete(at.0, at.1, &typed) else { return false };
    let Some((cut, _)) = full.char_indices().nth(typed.chars().count()) else { return false };
    if full[..cut].to_lowercase() != typed.to_lowercase() {
        return false;
    }
    ed.set_text(&full);
    ed.set_caret(cut, false);
    ed.set_caret(full.len(), true);
    true
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
    // Frozen rows and columns are always in view; the others must clear them.
    let [fw, fh] = geom.fz;
    if r >= geom.frozen.0 {
        if top < st.physics.y + fh {
            st.physics.stop();
            st.physics.set_position(top - fh);
        } else if bottom > st.physics.y + h {
            st.physics.stop();
            st.physics.set_position((bottom - h).min(top - fh));
        }
    }
    let (left, right) = (src.col_left(c), src.col_left(c + 1));
    if c >= geom.frozen.1 {
        if left < st.physics_x.y + fw {
            st.physics_x.stop();
            st.physics_x.set_position(left - fw);
        } else if right > st.physics_x.y + w {
            st.physics_x.stop();
            st.physics_x.set_position((right - w).min(left - fw));
        }
    }
}

/// One key on the grid. Returns whether anything changed.
#[allow(clippy::too_many_arguments)]
fn on_key(src: &dyn GridSource, geom: &Geom, g: &mut GridState, st: &mut ScrollState, key: Key, m: Modifiers, chars: &str, act: &Option<ActionFn>) -> bool {
    let before = (g.held, g.anchor, g.cursor);
    let done = on_key_inner(src, geom, g, st, key, m, chars, act);
    // A held selection goes with any move but the step that set it.
    if g.held == before.0 && (g.anchor, g.cursor) != (before.1, before.2) {
        g.held = None;
    }
    done
}

#[allow(clippy::too_many_arguments)]
fn on_key_inner(src: &dyn GridSource, geom: &Geom, g: &mut GridState, st: &mut ScrollState, key: Key, m: Modifiers, chars: &str, act: &Option<ActionFn>) -> bool {
    if geom.rows == 0 || geom.cols == 0 {
        return false;
    }
    let send = |a: GridAction| {
        if let Some(f) = act {
            f(a)
        }
    };
    let (last_r, last_c) = (geom.rows - 1, geom.cols - 1);
    // One step from a merged area starts at its far edge, and one landing in
    // a merged area lands on its top-left cell.
    let step = |(r, c): (u32, u32), dr: i64, dc: i64| -> (u32, u32) {
        let (mut r, mut c) = (r, c);
        if let Some(m) = src.merge_at(r, c) {
            r = if dr > 0 { m.r1 } else if dr < 0 { m.r0 } else { r };
            c = if dc > 0 { m.c1 } else if dc < 0 { m.c0 } else { c };
        }
        let to = ((r as i64 + dr).clamp(0, last_r as i64) as u32, (c as i64 + dc).clamp(0, last_c as i64) as u32);
        src.merge_at(to.0, to.1).map_or(to, |m| if (dr, dc) == (0, 0) || m.contains(r, c) { to } else { (m.r0, m.c0) })
    };
    let cmd = m.ctrl || m.meta;

    // A picked float: Delete asks to remove it, Escape (or any other key)
    // lets it go back to the cells.
    if let Some(k) = g.picked {
        if g.edit.is_none() {
            g.picked = None;
            match key {
                Key::Delete | Key::Backspace => {
                    send(GridAction::DeleteFloat(k));
                    return true;
                }
                Key::Escape => return true,
                _ => {}
            }
        }
    }

    // The editor has the keys, except the ones that end it.
    // How wide the editor's lines may run (a wrapping cell's width).
    let wrap = editor_wrap(src, g);
    let guessed = std::mem::take(&mut g.guess);
    let tab_run = g.tab_from.take();
    let ranged = g.selection() != CellRange::of(g.cursor, g.cursor);
    if let Some(ed) = &mut g.edit {
        if guessed && ed.text().starts_with('=') && ed.has_selection() {
            match key {
                // Tab takes a formula's guess and goes on editing.
                Key::Tab if !m.shift => {
                    ed.set_caret(ed.text().len(), false);
                    g.tab_from = tab_run;
                    return true;
                }
                // A typed key replaces it, Backspace and Delete remove it.
                Key::Char | Key::Backspace | Key::Delete => {}
                _ => {
                    ed.backspace(false);
                }
            }
        }
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
            // Alt+Enter: a line break in the cell, as in Excel.
            Key::Enter if m.alt => {
                ed.insert_break();
                return true;
            }
            // Ctrl+Enter: the same entry in every selected cell.
            Key::Enter if m.ctrl || m.meta => {
                let text = ed.text().to_string();
                let sel = g.selection();
                g.edit = None;
                send(GridAction::CommitAll { sel, text });
                return true;
            }
            Key::Char if (m.ctrl || m.meta) && (chars == ";" || chars == ":") => {
                if let Some(t) = src.now_text(m.shift || chars == ":") {
                    ed.insert(&t);
                }
                return true;
            }
            // Ctrl+' : the formula of the cell above, as it is written;
            // Ctrl+Shift+' : its value as shown.
            Key::Char if (m.ctrl || m.meta) && (chars == "'" || chars == "\"") => {
                if g.cursor.0 > 0 {
                    let (r, c) = (g.cursor.0 - 1, g.cursor.1);
                    let t = if m.shift || chars == "\"" { src.cell(r, c).map(|v| v.text).unwrap_or_default() } else { src.edit_text(r, c) };
                    ed.insert(&t);
                }
                return true;
            }
            // F4 in a formula: the reference at the caret cycles A1, $A$1,
            // A$1, $A1.
            Key::F(4) if !m.ctrl && !m.alt && !m.meta => {
                if let Some((t, caret)) = cycle_ref(ed.text(), ed.caret()) {
                    ed.set_text(&t);
                    ed.set_caret(caret, false);
                }
                return true;
            }
            // F2 switches between entering (arrows move on) and editing.
            Key::F(2) => {
                g.entering = !g.entering;
                return true;
            }
            // Entering: an arrow finishes and moves that way.
            Key::Up | Key::Down | Key::Left | Key::Right if g.entering && !m.shift => {
                let (dr, dc) = match key {
                    Key::Up => (-1, 0),
                    Key::Down => (1, 0),
                    Key::Left => (0, -1),
                    _ => (0, 1),
                };
                finish(g, st, dr, dc);
                return true;
            }
            // Up and Down move between the editor's lines, at about the
            // same place across; past the first or last they end it.
            Key::Up | Key::Down if !m.shift && editor_lines(ed.text(), wrap, &editor_style(src.zoom())).len() > 1 => {
                let text = ed.text().to_string();
                let lines = editor_lines(&text, wrap, &editor_style(src.zoom()));
                let k = lines.iter().position(|l| (l.start..=l.end).contains(&ed.caret())).unwrap_or(0);
                let to = if key == Key::Up { k.checked_sub(1) } else { Some(k + 1).filter(|&n| n < lines.len()) };
                if let Some(n) = to {
                    let style = editor_style(src.zoom());
                    let x = crate::text::measure(&text[lines[k].start..ed.caret()], &style).w;
                    let i = lines[n].start + index_at(&text[lines[n].clone()], x, &style);
                    ed.set_caret(i, false);
                    return true;
                }
                finish(g, st, if key == Key::Up { -1 } else { 1 }, 0);
                return true;
            }
            // In a selection of several cells, Enter and Tab enter the cell
            // and go on to the next one in it.
            Key::Enter | Key::Tab if ranged => {
                commit(g, act);
                g.step_in_selection(key == Key::Enter, m.shift);
                reveal(src, geom, st, g.cursor);
                return true;
            }
            // Enter after a run of Tabs: the next row, where the run began.
            Key::Enter if !m.shift && tab_run.is_some() => {
                let c0 = tab_run.unwrap_or(g.cursor.1);
                commit(g, act);
                let below = step(g.cursor, 1, 0);
                let to = step((below.0, c0), 0, 0);
                g.select(to.0, to.1);
                reveal(src, geom, st, to);
                return true;
            }
            Key::Enter => {
                finish(g, st, if m.shift { -1 } else { 1 }, 0);
                return true;
            }
            Key::Tab => {
                let from = g.cursor.1;
                let jump = src.tab_target(g.cursor, m.shift);
                finish(g, st, 0, if m.shift { -1 } else { 1 });
                if let Some(to) = jump.filter(|_| g.edit.is_none()) {
                    g.select(to.0, to.1);
                } else if !m.shift {
                    g.tab_from = Some(tab_run.unwrap_or(from));
                }
                return true;
            }
            _ => {
                // Typing and moving within the entry keep the Tab run.
                g.tab_from = tab_run;
                return match ed.key(key, m, chars) {
                    KeyOutcome::Changed => {
                        if key == Key::Char && !(m.ctrl || m.meta || m.alt) {
                            g.guess = complete(src, g.cursor, ed);
                        }
                        true
                    }
                    KeyOutcome::Submit => {
                        finish(g, st, 1, 0);
                        true
                    }
                    _ => false,
                };
            }
        }
    }

    // End mode: End, then an arrow goes to the data's edge (Home: the last
    // used cell). Any other key ends it.
    let end = std::mem::take(&mut g.end);
    if key == Key::End && !cmd && !m.alt {
        g.end = !end;
        return true;
    }
    if end && key == Key::Home && !cmd && !m.alt {
        let to = src.last_used();
        if m.shift || g.extend { g.cursor = to } else { g.select(to.0, to.1) }
        reveal(src, geom, st, to);
        return true;
    }
    let cmd_or_end = cmd || (end && matches!(key, Key::Up | Key::Down | Key::Left | Key::Right));
    if key == Key::Down && m.alt && !cmd {
        send(GridAction::Choose { row: g.cursor.0, col: g.cursor.1 });
        return true;
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
            Some(if cmd_or_end { src.data_edge(g.cursor.0, g.cursor.1, dr, dc) } else { past_hidden(src, g.cursor, step(g.cursor, dr as i64, dc as i64), dr, dc) })
        }
        // Alt+PageDown / Alt+PageUp: one screen right or left.
        Key::PageUp | Key::PageDown if m.alt && !cmd => {
            // As many columns as fit, at the active column's width, so the
            // two keys undo each other.
            let c = g.cursor.1;
            let cw = src.col_left(c + 1) - src.col_left(c);
            let n = if cw > 0.0 { (geom.body.w() as f64 / cw).floor().max(1.0) as i64 } else { 1 };
            let dc = if key == Key::PageUp { -1 } else { 1 };
            Some(past_hidden(src, g.cursor, step(g.cursor, 0, n * dc), 0, dc as i32))
        }
        Key::PageUp | Key::PageDown if cmd && !m.shift => {
            send(GridAction::Sheet(if key == Key::PageUp { -1 } else { 1 }));
            return true;
        }
        // Hidden rows and columns are stepped over.
        Key::PageUp => Some(past_hidden(src, g.cursor, step(g.cursor, -page(st), 0), -1, 0)),
        Key::PageDown => Some(past_hidden(src, g.cursor, step(g.cursor, page(st), 0), 1, 0)),
        Key::Home if cmd => {
            let r = past_hidden(src, g.cursor, (0, 0), 1, 0);
            Some(past_hidden(src, g.cursor, (r.0, 0), 0, 1))
        }
        Key::Home => Some(past_hidden(src, g.cursor, (g.cursor.0, 0), 0, 1)),
        Key::End if cmd => Some(src.last_used()),
        // Enter and Tab in a selection of several cells: the next cell in it.
        Key::Enter | Key::Tab if !cmd && !m.alt && g.step_in_selection(key == Key::Enter, m.shift) => {
            reveal(src, geom, st, g.cursor);
            return true;
        }
        Key::Tab if src.tab_target(g.cursor, m.shift).is_some() => {
            let to = src.tab_target(g.cursor, m.shift).unwrap_or(g.cursor);
            g.select(to.0, to.1);
            reveal(src, geom, st, to);
            return true;
        }
        Key::Tab => {
            let from = g.cursor.1;
            let to = step(g.cursor, 0, if m.shift { -1 } else { 1 });
            g.select(to.0, to.1);
            if !m.shift {
                g.tab_from = Some(tab_run.unwrap_or(from));
            }
            reveal(src, geom, st, to);
            return true;
        }
        Key::Enter | Key::F(2) if !(key == Key::F(2) && m.shift && src.fills()) => {
            let mut ed = TextEdit::new(&src.edit_text(g.cursor.0, g.cursor.1));
            ed.set_caret(ed.text().len(), false);
            g.anchor = g.cursor;
            g.edit = Some(ed);
            g.entering = false;
            g.extend = false;
            return true;
        }
        // Ctrl+Backspace: back to the active cell; Shift+Backspace: only it selected.
        Key::Backspace if cmd && !m.shift => {
            reveal(src, geom, st, g.cursor);
            return true;
        }
        Key::Backspace if m.shift && !cmd => {
            g.select(g.cursor.0, g.cursor.1);
            return true;
        }
        Key::Delete | Key::Backspace => {
            send(GridAction::Clear(g.selection()));
            return true;
        }
        // Ctrl+.: the active cell to the selection's next corner, clockwise;
        // the selection stays.
        Key::Char if cmd && !m.shift && chars == "." => {
            let s = g.selection();
            let corners = [(s.r0, s.c0), (s.r0, s.c1), (s.r1, s.c1), (s.r1, s.c0)];
            let at = corners.iter().position(|&k| k == g.cursor).unwrap_or(3);
            let to = corners[(at + 1) % 4];
            g.cursor = to;
            g.anchor = (if to.0 == s.r0 { s.r1 } else { s.r0 }, if to.1 == s.c0 { s.c1 } else { s.c0 });
            reveal(src, geom, st, to);
            return true;
        }
        // Ctrl+' (Ctrl+Shift+'): start editing with the formula (value) above.
        Key::Char if cmd && (chars == "'" || chars == "\"") && g.cursor.0 > 0 => {
            let (r, c) = (g.cursor.0 - 1, g.cursor.1);
            let t = if m.shift || chars == "\"" { src.cell(r, c).map(|v| v.text).unwrap_or_default() } else { src.edit_text(r, c) };
            let mut ed = TextEdit::new("");
            ed.insert(&t);
            g.edit = Some(ed);
            g.entering = false;
            reveal(src, geom, st, g.cursor);
            return true;
        }
        // Ctrl+; (Ctrl+Shift+;): start editing with today's date (the time).
        Key::Char if cmd && (chars == ";" || chars == ":") => {
            let Some(t) = src.now_text(m.shift || chars == ":") else { return false };
            let mut ed = TextEdit::new("");
            ed.insert(&t);
            g.edit = Some(ed);
            g.entering = true;
            reveal(src, geom, st, g.cursor);
            return true;
        }
        Key::Char if m.alt && !cmd && chars == "=" && src.fills() => {
            send(GridAction::AutoSum(g.selection()));
            return true;
        }
        // F8: Extend Selection on or off; Escape turns it off.
        Key::F(8) if !cmd && !m.alt && !m.shift => {
            g.extend = !g.extend;
            return true;
        }
        Key::Escape if g.extend => {
            g.extend = false;
            return true;
        }
        Key::F(5) if !cmd && !m.alt => {
            send(GridAction::GoTo);
            return true;
        }
        Key::F(4) if !cmd && !m.alt && src.fills() => {
            send(GridAction::Repeat);
            return true;
        }
        Key::F(11) if m.shift && !cmd && !m.alt && src.fills() => {
            send(GridAction::Sheet(0));
            return true;
        }
        Key::F(2) if m.shift && !cmd && !m.alt && src.fills() => {
            send(GridAction::Comment);
            return true;
        }
        // Ctrl+Shift+*: the data around the cursor (its current region).
        Key::Char if cmd && m.shift && (chars == "*" || chars == "8") => {
            let r = current_region(src, g.cursor);
            g.anchor = (r.r0, r.c0);
            g.cursor = (r.r1, r.c1);
            return true;
        }
        // Ctrl+Space: the selection's whole columns; Shift+Space: its whole rows.
        // Ctrl+Shift+Space: everything, as Ctrl+A on a lone cell.
        Key::Char if chars == " " && cmd && m.shift && !m.alt => {
            g.anchor = (0, 0);
            g.cursor = (last_r, last_c);
            return true;
        }
        Key::Char if chars == " " && cmd && !m.shift && !m.alt => {
            let s = g.selection();
            g.anchor = (0, s.c0);
            g.cursor = (last_r, s.c1);
            return true;
        }
        Key::Char if chars == " " && m.shift && !cmd && !m.alt => {
            let s = g.selection();
            g.anchor = (s.r0, 0);
            g.cursor = (s.r1, last_c);
            return true;
        }
        Key::Char if cmd && src.fills() && format_key(chars, m.shift).is_some() => {
            send(GridAction::Format(format_key(chars, m.shift).unwrap()));
            return true;
        }
        Key::Char if cmd => {
            match chars.to_ascii_lowercase().as_str() {
                "g" => send(GridAction::GoTo),
                "z" if m.shift => send(GridAction::Redo),
                "z" => send(GridAction::Undo),
                "y" => send(GridAction::Redo),
                "c" => send(GridAction::Copy(g.selection())),
                // Fill down or right: from the selection's first row or
                // column, or the cell above or left of a one-row selection.
                "d" | "r" if src.fills() => {
                    let sel = g.selection();
                    let down = chars.eq_ignore_ascii_case("d");
                    let (from, to) = match down {
                        true if sel.r1 > sel.r0 => (CellRange { r1: sel.r0, ..sel }, sel),
                        true if sel.r0 > 0 => (CellRange { r0: sel.r0 - 1, r1: sel.r0 - 1, ..sel }, CellRange { r0: sel.r0 - 1, ..sel }),
                        false if sel.c1 > sel.c0 => (CellRange { c1: sel.c0, ..sel }, sel),
                        false if sel.c0 > 0 => (CellRange { c0: sel.c0 - 1, c1: sel.c0 - 1, ..sel }, CellRange { c0: sel.c0 - 1, ..sel }),
                        _ => return true,
                    };
                    send(GridAction::Fill { from, to, copy: true, flip: false });
                }
                "e" if src.fills() => send(GridAction::FlashFill { row: g.cursor.0, col: g.cursor.1 }),
                "x" => send(GridAction::Cut(g.selection())),
                "v" if m.alt => send(GridAction::PasteSpecial),
                "v" => send(GridAction::Paste { row: g.selection().r0, col: g.selection().c0, sel: g.selection(), plain: m.shift }),
                // Ctrl+A: the data around the cursor, then (pressed again,
                // or on a lone cell) everything.
                "a" => {
                    let r = current_region(src, g.cursor);
                    if r != g.selection() && (r.r0 != r.r1 || r.c0 != r.c1) {
                        g.anchor = (r.r0, r.c0);
                        g.cursor = (r.r1, r.c1);
                    } else {
                        g.anchor = (0, 0);
                        g.cursor = (last_r, last_c);
                    }
                    return true;
                }
                _ => return false,
            }
            return true;
        }
        Key::Char if !chars.is_empty() && !chars.chars().any(char::is_control) => {
            // Typing replaces the active cell's content; the selection stays
            // (for Ctrl+Enter).
            let mut ed = TextEdit::new("");
            ed.insert(chars);
            g.guess = complete(src, g.cursor, &mut ed);
            g.edit = Some(ed);
            g.tab_from = tab_run;
            g.extend = false;
            g.entering = true;
            reveal(src, geom, st, g.cursor);
            return true;
        }
        _ => None,
    };
    let Some(to) = to else { return false };
    if m.shift || g.extend {
        g.cursor = to;
    } else {
        g.select(to.0, to.1);
    }
    reveal(src, geom, st, to);
    true
}

#[cfg(test)]
mod tests {
    #[test]
    fn visible_runs_clip_edges_and_skip_hidden_cells() {
        let edges = [0.0, 10.0, 10.0, 10.0, 20.0, 30.0, 40.0];
        let index = |px| edges.partition_point(|&value| value <= px).saturating_sub(1).min(5) as u32;
        let offset = |row| edges[row as usize];
        assert_eq!(super::visible_runs(5.0, 30.0, 0, 5, index, offset), vec![(0, 0), (3, 4)]);
        assert_eq!(super::visible_runs(10.0, 25.0, 0, 3, index, offset), vec![(3, 3)]);
        assert!(super::visible_runs(10.0, 10.0, 0, 5, index, offset).is_empty());
    }

    use super::*;
    #[test]
    fn italic_and_size_reach_the_cell_style() {
        let base = CellView::text("Hello");
        let big = CellView { size: Some(TEXT_SIZE * 2.0), ..base.clone() };
        assert!(needed_width(&big) > needed_width(&base) * 1.5);
        assert!(needed_height(&big, 200.0) > needed_height(&base, 200.0) * 1.5);
        let it = CellView { marks: crate::inline::Emphasis::Italic as u8 | crate::inline::Emphasis::Underline as u8, ..base };
        assert_eq!(cell_style(&it, &crate::theme::Semantic::LIGHT).family, "IBMPlexSans-Italic");
    }

    #[test]
    fn viewport_reports_scrolled_and_frozen_bands_without_tile_padding() {
        struct Probe {
            sheet: Rc<Sheet>,
            scroll: Mutable<ScrollState>,
            reported: Rc<RefCell<Vec<CellRange>>>,
        }
        impl View for Probe {
            #[cfg_attr(debug_assertions, track_caller)]
            fn render(&self, ctx: &mut Ctx) {
                ctx.widget_site();
                let reported = self.reported.clone();
                GridView::new(self.sheet.clone(), self.scroll.clone(), Mutable::new(GridState::default()), Rect::new(0.0, 0.0, 400.0, 220.0))
                    .on_visible(move |r| reported.borrow_mut().push(r)).emit(ctx);
            }
        }
        let sheet = Rc::new(Sheet::new(100, 20));
        sheet.frozen.set((2, 1));
        let mut scroll = ScrollState::default();
        scroll.physics.set_max(1900.0);
        scroll.physics_x.set_max(1000.0);
        scroll.physics.set_position(400.0);
        scroll.physics_x.set_position(128.0);
        let reported = Rc::new(RefCell::new(Vec::new()));
        let probe = Probe { sheet, scroll: Mutable::new(scroll), reported: reported.clone() };
        crate::view::render(&probe, crate::theme::Semantic::LIGHT);
        let draws = |row, col| reported.borrow().iter().any(|r| (r.r0..=r.r1).contains(&row) && (r.c0..=r.c1).contains(&col));
        assert!(draws(0, 0) && draws(1, 3) && draws(22, 0) && draws(22, 3));
        assert!(!draws(2, 0) && !draws(21, 3) && !draws(22, 2) && !draws(30, 3));
    }

    #[test]
    fn every_cell_icon_draws_inside_its_square() {
        struct Icons;
        impl View for Icons {
            #[cfg_attr(debug_assertions, track_caller)]
            fn render(&self, ctx: &mut Ctx) {
                ctx.widget_site();
                ctx.push(Node::Rect { rect: Rect::new(0.0, 0.0, 800.0, 40.0), fill: Color::TRANSPARENT, radius: 0.0 });
                let all = [
                    CellIcon::Arrow(2), CellIcon::Arrow(1), CellIcon::Arrow(0), CellIcon::Arrow(-1), CellIcon::Arrow(-2), CellIcon::Dot, CellIcon::Flag,
                    CellIcon::Check(true), CellIcon::Exclaim(false), CellIcon::Cross(true), CellIcon::Triangle(true), CellIcon::Diamond, CellIcon::Dash,
                    CellIcon::Star(0), CellIcon::Star(1), CellIcon::Star(2), CellIcon::Bars(2), CellIcon::Quarters(3), CellIcon::Boxes(4),
                ];
                for (i, ic) in all.into_iter().enumerate() {
                    draw_icon(ctx, ic, Color::rgba(1.0, 0.0, 0.0, 1.0), Color::rgba(0.5, 0.5, 0.5, 1.0), 20.0 + i as f32 * 40.0, 10.0, 14.0);
                }
                ctx.pop();
            }
        }
        let (tree, _) = crate::view::render(&Icons, crate::theme::Semantic::LIGHT);
        let mut per = [0usize; 19];
        for (_, n) in tree.iter() {
            let r = n.rect();
            if r.w() >= 800.0 {
                continue;
            }
            let i = ((r.x() - 20.0) / 40.0).floor().clamp(0.0, 18.0) as usize;
            let x0 = 20.0 + i as f32 * 40.0;
            assert!(r.x() >= x0 - 2.0 && r.x() + r.w() <= x0 + 16.0 && r.y() >= 8.0 && r.y() + r.h() <= 26.0, "icon {i}: {r:?}");
            per[i] += 1;
        }
        assert!(per.iter().all(|&n| n > 0), "{per:?}");
    }
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
        merges: RefCell<Vec<CellRange>>,
        frozen: std::cell::Cell<(u32, u32)>,
        from: std::cell::Cell<(u32, u32)>,
        /// Pressable floats: key and where.
        buttons: RefCell<Vec<(u64, [f64; 4])>>,
        fills: std::cell::Cell<bool>,
        /// Gridlines and headings shown.
        view: std::cell::Cell<(bool, bool)>,
    }

    impl Sheet {
        fn new(rows: u32, cols: u32) -> Self {
            Self { rows, cols, widths: Default::default(), cells: Default::default(), merges: Default::default(), frozen: Default::default(), from: Default::default(), buttons: Default::default(), fills: Default::default(), view: std::cell::Cell::new((true, true)) }
        }
        fn w(&self, c: u32) -> f64 {
            *self.widths.borrow().get(&c).unwrap_or(&64.0)
        }
    }

    impl GridSource for Sheet {
        fn complete(&self, _r: u32, c: u32, prefix: &str) -> Option<String> {
            let p = prefix.to_lowercase();
            let cells = self.cells.borrow();
            let mut hits: Vec<&String> = cells.iter().filter(|((_, cc), t)| *cc == c && t.len() > p.len() && t.to_lowercase().starts_with(&p)).map(|(_, t)| t).collect();
            hits.dedup();
            (hits.len() == 1).then(|| hits[0].clone())
        }
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
                // "»…": centred across the cells after it.
                if let Some(t) = s.strip_prefix('»') {
                    return Some(CellView { align: CellAlign::Across, ..CellView::text(t.to_string()) });
                }
                if let Some(t) = s.strip_prefix('«') {
                    return Some(CellView { align: CellAlign::Right, ..CellView::text(t.to_string()) });
                }
                // "¶…": wraps its text.
                if let Some(t) = s.strip_prefix('¶') {
                    return Some(CellView { wrap: true, ..CellView::text(t.to_string()) });
                }
                return Some(CellView::text(s.clone()));
            }
            (r % 3 == 0).then(|| CellView::number(format!("{}", r as u64 * 1000 + c as u64)))
        }
        fn edit_text(&self, r: u32, c: u32) -> String {
            self.cells.borrow().get(&(r, c)).cloned().unwrap_or_default()
        }
        fn merge_at(&self, r: u32, c: u32) -> Option<CellRange> {
            self.merges.borrow().iter().find(|m| m.contains(r, c)).copied()
        }
        fn merges_in(&self, g: CellRange) -> Vec<CellRange> {
            self.merges.borrow().iter().filter(|m| m.r0 <= g.r1 && g.r0 <= m.r1 && m.c0 <= g.c1 && g.c0 <= m.c1).copied().collect()
        }
        fn frozen(&self) -> (u32, u32) {
            self.frozen.get()
        }
        fn frozen_from(&self) -> (u32, u32) {
            self.from.get()
        }
        fn fills(&self) -> bool {
            self.fills.get()
        }
        fn show_grid(&self) -> bool {
            self.view.get().0
        }
        fn show_headings(&self) -> bool {
            self.view.get().1
        }
        fn floats(&self, _view: [f64; 4], _theme: &crate::theme::Semantic) -> Vec<GridFloat> {
            self.buttons.borrow().iter().map(|&(key, at)| GridFloat { key, at, nodes: Rc::new(vec![Node::Rect { rect: Rect::new(0.0, 0.0, at[2] as f32, at[3] as f32), fill: Color::rgba(1.0, 0.0, 0.0, 1.0), radius: 0.0 }]), movable: false, pressable: true }).collect()
        }
    }

    struct Host {
        grid: Rc<Sheet>,
        scroll: Mutable<ScrollState>,
        state: Mutable<GridState>,
        log: Rc<RefCell<Vec<GridAction>>>,
    }

    impl View for Host {
        #[cfg_attr(debug_assertions, track_caller)]
        fn render(&self, ctx: &mut Ctx) {
            ctx.widget_site();
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
    fn a_star_fill_puts_the_sign_left_and_the_number_right() {
        struct One(CellView, f32);
        impl View for One {
            #[cfg_attr(debug_assertions, track_caller)]
            fn render(&self, ctx: &mut Ctx) {
                ctx.widget_site();
                draw_cell(ctx, &crate::theme::Semantic::LIGHT, &self.0, 0.0, 0.0, self.1, 20.0);
            }
        }
        let nodes = |cv: CellView, w: f32| {
            let mut app = App::new(One(cv, w));
            app.tick();
            let mut v: Vec<(String, f32, f32)> = app.tree().iter().filter_map(|(_, n)| match n {
                Node::Text { content, rect, .. } => Some((content.to_string(), rect.x(), rect.x() + rect.w())),
                _ => None,
            }).collect();
            v.sort_by(|a, b| a.1.total_cmp(&b.1));
            v
        };
        let acc = CellView { fill_at: Some(2), ..CellView::number(" $ 1,234.50 ") };
        let v = nodes(acc.clone(), 200.0);
        assert_eq!(v.len(), 2, "{v:?}");
        assert_eq!(v[0].0, " $");
        assert!(v[0].1 < 10.0, "{v:?}");
        assert_eq!(v[1].0, "1,234.50 ");
        assert!(v[1].2 > 190.0 && v[1].1 > 100.0, "{v:?}");
        // A visible fill character repeats between them.
        let dash = CellView { fill_at: Some(1), ..CellView::number("7-") };
        let v = nodes(dash, 120.0);
        assert!(v.iter().any(|(t, ..)| t.len() > 5 && t.chars().all(|c| c == '-')), "{v:?}");
        // Too narrow: hashes, as any number that doesn't fit.
        let v = nodes(acc, 30.0);
        assert!(v.iter().all(|(t, ..)| t.chars().all(|c| c == '#')), "{v:?}");
        // An offset off a character boundary is ignored.
        let v = nodes(CellView { fill_at: Some(2), ..CellView::number("€5") }, 200.0);
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn fill_justify_distributed_and_script_cells_draw_as_spreadsheets_do() {
        struct One(CellView, f32, f32);
        impl View for One {
            #[cfg_attr(debug_assertions, track_caller)]
            fn render(&self, ctx: &mut Ctx) {
                ctx.widget_site();
                draw_cell(ctx, &crate::theme::Semantic::LIGHT, &self.0, 0.0, 0.0, self.1, self.2);
            }
        }
        // Texts as (content, x, y, size), and the rules (underlines) as y.
        let draw = |cv: CellView, w: f32, h: f32| {
            let mut app = App::new(One(cv, w, h));
            app.tick();
            let mut texts = Vec::new();
            let mut rules = Vec::new();
            for (_, n) in app.tree().iter() {
                match n {
                    Node::Text { content, rect, style } => texts.push((content.to_string(), rect.x(), rect.y(), style.size)),
                    Node::Rect { rect, .. } if rect.h() <= 2.0 => rules.push(rect.y()),
                    _ => {}
                }
            }
            texts.sort_by(|a, b| a.2.total_cmp(&b.2).then(a.1.total_cmp(&b.1)));
            (texts, rules)
        };
        // Fill: the whole text repeated as many times as fit, never cut.
        let (t, _) = draw(CellView { spread: Spread::Fill, ..CellView::text("ab") }, 120.0, 20.0);
        assert_eq!(t.len(), 1);
        let n = t[0].0.len() / 2;
        assert!(n > 3 && t[0].0 == "ab".repeat(n), "{t:?}");
        // Distributed: one line's words reach both edges.
        let (t, _) = draw(CellView { spread: Spread::Distributed, ..CellView::text("a b c") }, 200.0, 20.0);
        assert_eq!(t.iter().map(|x| x.0.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
        assert!(t[0].1 < 10.0 && t[2].1 > 180.0 && (t[1].1 - 100.0).abs() < 10.0, "{t:?}");
        // Justify: wrapped, a paragraph's last line left as it is.
        let (t, _) = draw(CellView { spread: Spread::Justify, ..CellView::text("one two three four five six seven") }, 90.0, 120.0);
        let lines: Vec<f32> = t.iter().map(|x| x.2).fold(Vec::new(), |mut v, y| { if v.last() != Some(&y) { v.push(y); } v });
        assert!(lines.len() >= 2, "{t:?}");
        let first: Vec<_> = t.iter().filter(|x| x.2 == lines[0]).collect();
        assert!(first.len() >= 2 && first.last().unwrap().1 > 40.0, "{t:?}");
        let last: Vec<_> = t.iter().filter(|x| x.2 == *lines.last().unwrap()).collect();
        assert_eq!(last.len(), 1, "the last line is one node: {t:?}");
        // Superscript smaller and higher, subscript lower; a double underline two rules.
        let base = draw(CellView::text("x"), 100.0, 20.0).0[0].clone();
        let sup = draw(CellView { marks: crate::inline::Emphasis::Super as u8, ..CellView::text("x") }, 100.0, 20.0).0[0].clone();
        let sub = draw(CellView { marks: crate::inline::Emphasis::Sub as u8, ..CellView::text("x") }, 100.0, 20.0).0[0].clone();
        assert!(sup.3 < base.3 && sup.2 < base.2 && sub.2 > base.2, "{base:?} {sup:?} {sub:?}");
        let u = crate::inline::Emphasis::Underline as u8;
        assert_eq!(draw(CellView { marks: u, ..CellView::text("x") }, 100.0, 20.0).1.len(), 1);
        assert_eq!(draw(CellView { marks: u | crate::inline::Emphasis::Double as u8, ..CellView::text("x") }, 100.0, 20.0).1.len(), 2);
        assert_eq!(draw(CellView { marks: crate::inline::Emphasis::Double as u8, ..CellView::text("x") }, 100.0, 20.0).1.len(), 0, "double alone draws nothing");
        // An accounting underline spans the cell, not the text.
        let widths = |marks: u8| {
            let mut app = App::new(One(CellView { marks, ..CellView::text("x") }, 100.0, 20.0));
            app.tick();
            app.tree().iter().filter_map(|(_, n)| match n { Node::Rect { rect, .. } if rect.h() <= 2.0 => Some(rect.w()), _ => None }).collect::<Vec<f32>>()
        };
        let acc = crate::inline::Emphasis::Accounting as u8;
        assert!(widths(u).iter().all(|&w| w < 20.0), "{:?}", widths(u));
        let w = widths(u | acc);
        assert!(w.len() == 1 && w[0] > 80.0, "{w:?}");
        assert_eq!(widths(u | acc | crate::inline::Emphasis::Double as u8).len(), 2);
        // Vertical justify: wrapped lines spread from the top edge to the
        // bottom; one line sits in the middle.
        let ys = |cv: CellView| {
            let (t, _) = draw(cv, 60.0, 200.0);
            t.iter().map(|x| x.2).fold(Vec::new(), |mut v: Vec<f32>, y| { if v.last() != Some(&y) { v.push(y); } v })
        };
        let words = "one two three four five six";
        let plain = ys(CellView { wrap: true, ..CellView::text(words) });
        let spread = ys(CellView { wrap: true, vspread: true, ..CellView::text(words) });
        assert!(plain.len() >= 3 && spread.len() == plain.len(), "{plain:?} {spread:?}");
        assert!(spread[0] < 10.0 && *spread.last().unwrap() > 150.0 && *plain.last().unwrap() < 150.0, "{plain:?} {spread:?}");
        let one = ys(CellView { wrap: true, vspread: true, ..CellView::text("x") });
        assert!(one.len() == 1 && (one[0] - 90.0).abs() < 15.0, "{one:?}");
    }

    #[test]
    fn f4_cycles_the_reference_at_the_caret() {
        let c = |t: &str, at: usize| cycle_ref(t, at).map(|(s, k)| (s, k));
        assert_eq!(c("=A1+B2", 3), Some(("=$A$1+B2".into(), 5)));
        assert_eq!(c("=$A$1+B2", 5), Some(("=A$1+B2".into(), 4)));
        assert_eq!(c("=A$1+B2", 4), Some(("=$A1+B2".into(), 4)));
        assert_eq!(c("=$A1+B2", 4), Some(("=A1+B2".into(), 3)));
        // Inside a reference, and both ends of a range.
        assert_eq!(c("=SUM(B2:C10)", 6), Some(("=SUM($B$2:$C$10)".into(), 15)));
        assert_eq!(c("=Sheet2!A1*2", 10), Some(("=Sheet2!$A$1*2".into(), 12)));
        // Not a reference: a function name, a defined name, plain text, no formula.
        assert_eq!(c("=LOG10(5)", 6), None);
        assert_eq!(c("=Tax_A1*2", 7), None);
        assert_eq!(c("=Total+1", 6), None);
        assert_eq!(c("A1", 2), None);
        assert_eq!(c("=A0", 3), None);
        assert_eq!(c("=1+2", 2), None);
    }

    #[test]
    fn ctrl_a_and_ctrl_shift_star_select_the_data_around_the_cursor() {
        let (mut app, grid, _, state, log) = host(100, 30);
        // Blank overrides: the test sheet fills every third row by itself.
        for r in 0..100 {
            for c in 0..30 {
                grid.cells.borrow_mut().insert((r, c), String::new());
            }
        }
        for (r, c) in [(2, 2), (2, 3), (3, 2), (4, 3), (5, 4), (9, 9)] {
            grid.cells.borrow_mut().insert((r, c), "x".into());
        }
        grid.fills.set(true);
        app.tick();
        let hw = GridView::header_width(TILE_ROWS * 4);
        let at = Point::new(hw + 3.5 * 64.0, HEADER_H + 3.5 * 20.0);
        app.handle_event(Event::PointerDown { at });
        app.handle_event(Event::PointerUp { at });
        app.tick();
        let ctrl = Modifiers { ctrl: true, ..Modifiers::default() };
        let mut g = state.get_cloned();
        g.anchor = (3, 3);
        g.cursor = (3, 3);
        state.set(g);
        key(&mut app, Key::Char, "*", Modifiers { shift: true, ..ctrl });
        // (5, 4) joins by the diagonal from (4, 3); (9, 9) is apart.
        assert_eq!(state.get_cloned().selection(), CellRange { r0: 2, c0: 2, r1: 5, c1: 4 });
        // Ctrl+A on the region: everything.
        key(&mut app, Key::Char, "a", ctrl);
        assert_eq!(state.get_cloned().selection(), CellRange { r0: 0, c0: 0, r1: 99, c1: 29 });
        let mut g = state.get_cloned();
        g.anchor = (2, 2);
        g.cursor = (2, 2);
        state.set(g);
        key(&mut app, Key::Char, "a", ctrl);
        assert_eq!(state.get_cloned().selection(), CellRange { r0: 2, c0: 2, r1: 5, c1: 4 });
        // A lone empty cell: everything at once.
        let mut g = state.get_cloned();
        g.anchor = (50, 20);
        g.cursor = (50, 20);
        state.set(g);
        key(&mut app, Key::Char, "a", ctrl);
        assert_eq!(state.get_cloned().selection(), CellRange { r0: 0, c0: 0, r1: 99, c1: 29 });
        // F4 outside the editor asks to repeat.
        key(&mut app, Key::F(4), "", Modifiers::default());
        assert_eq!(log.borrow().last(), Some(&GridAction::Repeat));
        // Sheets: Ctrl+PageDown / Ctrl+PageUp step, Shift+F11 adds; Shift+F2 comments.
        let cursor = state.get_cloned().cursor;
        key(&mut app, Key::PageDown, "", ctrl);
        assert_eq!(log.borrow().last(), Some(&GridAction::Sheet(1)));
        key(&mut app, Key::PageUp, "", ctrl);
        assert_eq!(log.borrow().last(), Some(&GridAction::Sheet(-1)));
        assert_eq!(state.get_cloned().cursor, cursor, "the cursor stays");
        let shift = Modifiers { shift: true, ..Modifiers::default() };
        key(&mut app, Key::F(11), "", shift);
        assert_eq!(log.borrow().last(), Some(&GridAction::Sheet(0)));
        key(&mut app, Key::F(2), "", shift);
        assert_eq!(log.borrow().last(), Some(&GridAction::Comment));
        assert!(state.get_cloned().edit.is_none(), "Shift+F2 doesn't open the cell editor");
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
    fn a_button_in_a_cell_and_alt_down_reach_the_app() {
        let (mut app, grid, _, state, log) = host(100, 10);
        let hw = GridView::header_width(TILE_ROWS * 4);
        // A button at the right of B3 (64 px columns, 20 px rows).
        grid.buttons.borrow_mut().push((7, [64.0 + 48.0, 40.0, 16.0, 20.0]));
        app.tick();
        let click = |app: &mut App<Host>, x: f32, y: f32| {
            app.handle_event(Event::PointerDown { at: Point::new(x, y) });
            app.handle_event(Event::PointerUp { at: Point::new(x, y) });
            app.tick();
        };
        click(&mut app, hw + 64.0 + 56.0, HEADER_H + 45.0);
        assert!(log.borrow().iter().any(|a| matches!(a, GridAction::PressFloat(7))), "{:?}", log.borrow());
        // Elsewhere in B3, the cell is chosen as usual.
        click(&mut app, hw + 64.0 + 10.0, HEADER_H + 45.0);
        assert_eq!(state.get_cloned().cursor, (2, 1));
        key(&mut app, Key::Down, "", Modifiers { alt: true, ..Modifiers::default() });
        assert!(log.borrow().iter().any(|a| matches!(a, GridAction::Choose { row: 2, col: 1 })));
        assert_eq!(state.get_cloned().cursor, (2, 1));
    }

    #[test]
    fn arrows_step_over_hidden_columns() {
        let (mut app, grid, _, state, _) = host(100, 10);
        // C and D hidden (no width).
        grid.widths.borrow_mut().insert(2, 0.0);
        grid.widths.borrow_mut().insert(3, 0.0);
        app.tick();
        // A click on B1 gives the grid the keys.
        let hw = GridView::header_width(TILE_ROWS * 4);
        app.handle_event(Event::PointerDown { at: Point::new(hw + 74.0, HEADER_H + 5.0) });
        app.handle_event(Event::PointerUp { at: Point::new(hw + 74.0, HEADER_H + 5.0) });
        app.tick();
        assert_eq!(state.get_cloned().cursor, (0, 1));
        let none = Modifiers::default();
        key(&mut app, Key::Right, "", none);
        assert_eq!(state.get_cloned().cursor, (0, 4));
        key(&mut app, Key::Left, "", none);
        assert_eq!(state.get_cloned().cursor, (0, 1));
        // With A hidden too, Home lands on B, the first shown column.
        grid.widths.borrow_mut().insert(0, 0.0);
        key(&mut app, Key::Right, "", none);
        key(&mut app, Key::Home, "", none);
        assert_eq!(state.get_cloned().cursor, (0, 1));
        grid.widths.borrow_mut().remove(&0);
        // Hidden up to the last column: the cursor stays.
        for c in 5..10 {
            grid.widths.borrow_mut().insert(c, 0.0);
        }
        let mut g = state.get_cloned();
        g.select(0, 4);
        state.set(g);
        key(&mut app, Key::Right, "", none);
        assert_eq!(state.get_cloned().cursor, (0, 4));
    }

    #[test]
    fn the_fill_handle_and_ctrl_d_ask_to_fill() {
        let (mut app, grid, _, state, log) = host(100, 10);
        let hw = GridView::header_width(TILE_ROWS * 4);
        let at = |c: f64, r: f64| Point::new(hw + (c * 64.0) as f32, HEADER_H + (r * 20.0) as f32);
        let mut g = state.get_cloned();
        g.anchor = (1, 1);
        g.cursor = (2, 1);
        state.set(g);
        app.tick();
        // Without the source's say, no handle.
        app.handle_event(Event::PointerDown { at: at(2.0, 3.0) });
        app.handle_event(Event::PointerUp { at: at(2.0, 3.0) });
        app.tick();
        assert!(!log.borrow().iter().any(|a| matches!(a, GridAction::Fill { .. })));
        log.borrow_mut().clear();
        grid.fills.set(true);
        let mut g = state.get_cloned();
        g.anchor = (1, 1);
        g.cursor = (2, 1);
        state.set(g);
        app.tick();
        // The handle at B3's bottom-right corner, dragged down to row 6.
        app.handle_event(Event::PointerDown { at: at(2.0, 3.0) });
        app.handle_event(Event::PointerMove { at: at(1.5, 5.5) });
        app.tick();
        assert_eq!(state.get_cloned().fill_drag(), Some(CellRange { r0: 1, c0: 1, r1: 5, c1: 1 }));
        app.handle_event(Event::PointerMove { at: at(1.5, 6.5) });
        app.handle_event(Event::PointerUp { at: at(1.5, 6.5) });
        app.tick();
        let from = CellRange { r0: 1, c0: 1, r1: 2, c1: 1 };
        assert_eq!(log.borrow().last(), Some(&GridAction::Fill { from, to: CellRange { r1: 6, ..from }, copy: false, flip: false }));
        assert_eq!(state.get_cloned().selection(), CellRange { r1: 6, ..from });
        // A double-click on the handle (now at B7's corner) asks to fill to the data's end.
        for _ in 0..2 {
            app.handle_event(Event::PointerDown { at: at(2.0, 7.0) });
            app.handle_event(Event::PointerUp { at: at(2.0, 7.0) });
            app.tick();
        }
        assert_eq!(log.borrow().last(), Some(&GridAction::FillToData(CellRange { r1: 6, ..from })));
        // Ctrl+D with one row chosen copies the row above into it.
        let mut g = state.get_cloned();
        g.select(4, 3);
        state.set(g);
        key(&mut app, Key::Char, "e", Modifiers { ctrl: true, ..Modifiers::default() });
        assert!(matches!(log.borrow().last(), Some(&GridAction::FlashFill { .. })));
        // Alt+= sums next to the selection; Ctrl+G and F5 ask where to go.
        key(&mut app, Key::Char, "=", Modifiers { alt: true, ..Modifiers::default() });
        assert_eq!(log.borrow().last(), Some(&GridAction::AutoSum(CellRange { r0: 4, c0: 3, r1: 4, c1: 3 })));
        key(&mut app, Key::Char, "g", Modifiers { ctrl: true, ..Modifiers::default() });
        assert_eq!(log.borrow().last(), Some(&GridAction::GoTo));
        for (k, shift, want) in [("b", false, 'b'), ("I", false, 'i'), ("5", false, '5'), ("%", true, '%'), ("4", true, '$'), ("~", true, '~'), ("`", false, '`'), ("1", false, '1'), ("7", true, '&'), ("_", true, '_'), ("9", false, '9'), ("0", false, '0'), ("(", true, '('), ("0", true, ')'), ("-", false, '-'), ("+", false, '+'), ("=", true, '+'), ("t", false, 't'), ("l", false, 'l'), ("L", true, 'L'), ("k", false, 'k')] {
            key(&mut app, Key::Char, k, Modifiers { ctrl: true, shift, ..Modifiers::default() });
            assert_eq!(log.borrow().last(), Some(&GridAction::Format(want)), "{k}");
        }
        // Ctrl+Space and Shift+Space: whole columns, whole rows.
        let kept = state.get_cloned();
        let mut g = state.get_cloned();
        g.anchor = (4, 2);
        g.cursor = (5, 3);
        state.set(g);
        key(&mut app, Key::Char, " ", Modifiers { ctrl: true, ..Modifiers::default() });
        let all = state.get_cloned().selection();
        assert_eq!((all.r0, all.c0, all.c1), (0, 2, 3));
        assert!(all.r1 > 5, "{all:?}");
        let mut g = state.get_cloned();
        g.anchor = (4, 2);
        g.cursor = (5, 3);
        state.set(g);
        key(&mut app, Key::Char, " ", Modifiers { shift: true, ..Modifiers::default() });
        let all = state.get_cloned().selection();
        assert_eq!((all.r0, all.r1, all.c0), (4, 5, 0));
        assert!(all.c1 > 3, "{all:?}");
        state.set(kept);
        log.borrow_mut().clear();
        key(&mut app, Key::F(5), "", Modifiers::default());
        assert_eq!(log.borrow().last(), Some(&GridAction::GoTo));
        key(&mut app, Key::Char, "d", Modifiers { ctrl: true, ..Modifiers::default() });
        let one = CellRange { r0: 3, c0: 3, r1: 3, c1: 3 };
        assert_eq!(log.borrow().last(), Some(&GridAction::Fill { from: one, to: CellRange { r1: 4, ..one }, copy: true, flip: false }));
    }

    #[test]
    fn a_float_in_the_frozen_rows_stays_with_them() {
        let (mut app, grid, scroll, _, log) = host(200, 10);
        grid.frozen.set((1, 0));
        // A button in A1's row, at D1's right; another down in row 30.
        grid.buttons.borrow_mut().push((9, [3.0 * 64.0 + 48.0, 2.0, 16.0, 16.0]));
        grid.buttons.borrow_mut().push((10, [3.0 * 64.0 + 48.0, 600.0, 16.0, 16.0]));
        app.tick();
        let mut st = scroll.get();
        st.physics.set_position(400.0);
        scroll.set(st);
        app.tick();
        let hw = GridView::header_width(TILE_ROWS * 4);
        let reds = |app: &App<Host>| app.tree().iter().filter_map(|(_, n)| match n { Node::Rect { rect, fill, .. } if *fill == Color::rgba(1.0, 0.0, 0.0, 1.0) => Some((rect.x(), rect.y())), _ => None }).collect::<Vec<_>>();
        let r = reds(&app);
        // Drawn twice: in the body (scrolled away by its viewport) and in the
        // frozen row's band, which doesn't scroll down. The other, once.
        assert_eq!(r.iter().filter(|p| **p == (hw + 240.0, HEADER_H + 2.0)).count(), 2, "{r:?}");
        assert_eq!(r.iter().filter(|p| p.1 > HEADER_H + 100.0).count(), 1, "{r:?}");
        app.handle_event(Event::PointerDown { at: Point::new(hw + 248.0, HEADER_H + 10.0) });
        app.handle_event(Event::PointerUp { at: Point::new(hw + 248.0, HEADER_H + 10.0) });
        app.tick();
        assert!(log.borrow().iter().any(|a| matches!(a, GridAction::PressFloat(9))), "{:?}", log.borrow());
    }

    #[test]
    fn long_text_runs_over_empty_neighbours() {
        let (mut app, grid, _, _, _) = host(100, 10);
        let long = "A heading much wider than one column";
        grid.cells.borrow_mut().insert((1, 0), long.into());
        grid.cells.borrow_mut().insert((2, 0), long.into());
        grid.cells.borrow_mut().insert((2, 1), "x".into());
        app.tick();
        let shown: Vec<(String, f32)> = app.tree().iter().filter_map(|(_, n)| if let Node::Text { content, rect, .. } = n { Some((content.clone(), rect.w())) } else { None }).collect();
        // Row 2: whole, wider than its 64 px column. Row 3: cut by "x" beside it.
        assert!(shown.iter().any(|(t, w)| t == long && *w > 64.0), "{shown:?}");
        assert!(shown.iter().any(|(t, w)| t.ends_with('…') && t.starts_with("A ") && *w <= 64.0), "{shown:?}");
        assert_eq!(shown.iter().filter(|(t, _)| t == long).count(), 1, "{shown:?}");
    }

    #[test]
    fn text_runs_across_hidden_columns_from_visible_cells_only() {
        let (mut app, grid, _, _, _) = host(100, 80);
        let heading = "A heading much wider than one column";
        let next = SPILL_COLS + 9;
        for (r, c, text) in [
            (1, 0, heading), (1, 1, "Hidden blocker"),
            (2, 0, heading), (2, 1, "Hidden blocker"), (2, next, "Visible blocker"),
            (4, next, "«Right aligned"), (4, next - 1, "Hidden left blocker"),
            (5, next - 1, "Hidden source must not overflow into the next column"),
        ] {
            grid.cells.borrow_mut().insert((r, c), text.into());
        }
        let row = |app: &App<Host>, r: u32| {
            let top = HEADER_H + r as f32 * 20.0;
            app.tree().iter().filter_map(|(_, node)| match node {
                Node::Text { content, rect, .. } if rect.y() >= top && rect.y() < top + 20.0 => Some((content.clone(), *rect)),
                _ => None,
            }).collect::<Vec<_>>()
        };
        app.tick();
        let visible = row(&app, 1);
        assert!(visible.iter().any(|(t, rect)| t.starts_with("A ") && t.ends_with('…') && rect.w() <= 64.0), "{visible:?}");
        assert!(visible.iter().any(|(t, _)| t == "Hidden blocker"), "{visible:?}");

        for c in 1..next {
            grid.widths.borrow_mut().insert(c, 0.0);
        }
        app.dirty_flag().store(true, std::sync::atomic::Ordering::Release);
        app.tick();
        let hw = GridView::header_width(TILE_ROWS * 4);
        let spilled = row(&app, 1);
        assert!(spilled.iter().any(|(t, rect)| t == heading && rect.x() < hw + 64.0 && rect.x() + rect.w() > hw + 64.0), "{spilled:?}");
        let blocked = row(&app, 2);
        assert!(blocked.iter().any(|(t, rect)| t.starts_with("A ") && t.ends_with('…') && rect.x() + rect.w() <= hw + 64.0), "{blocked:?}");
        assert!(blocked.iter().any(|(t, _)| t == "Visible blocker"), "{blocked:?}");
        let leftward = row(&app, 4);
        assert!(leftward.iter().any(|(t, rect)| t == "Right aligned" && rect.x() < hw + 64.0 && rect.x() + rect.w() > hw + 64.0), "{leftward:?}");
        for r in [1, 2, 4, 5] {
            let shown = row(&app, r);
            assert!(shown.iter().all(|(t, _)| !t.starts_with("Hidden")), "{shown:?}");
        }
    }

    #[test]
    fn text_centred_across_the_selection_spans_its_cells() {
        let (mut app, grid, _, _, _) = host(100, 10);
        // Row 2: "Total" across A:C. Row 3: across A:B only (C not across).
        for (r, c, t) in [(1, 0, "»Total"), (1, 1, "»"), (1, 2, "»"), (2, 0, "»Total"), (2, 1, "»")] {
            grid.cells.borrow_mut().insert((r, c), t.into());
        }
        app.tick();
        let at: Vec<f32> = app.tree().iter().filter_map(|(_, n)| if let Node::Text { content, rect, .. } = n { (content == "Total").then(|| rect.x() + rect.w() / 2.0) } else { None }).collect();
        assert_eq!(at.len(), 2, "{at:?}");
        let hw = GridView::header_width(TILE_ROWS * 4);
        // Centres at the middle of three columns, then of two.
        let mut at = at;
        at.sort_by(|a, b| b.partial_cmp(a).unwrap());
        assert!((at[0] - (hw + 96.0)).abs() <= 2.0, "{at:?}");
        assert!((at[1] - (hw + 64.0)).abs() <= 2.0, "{at:?}");
    }

    #[test]
    fn frozen_rows_and_columns_stay_in_view() {
        let (mut app, grid, scroll, state, _) = host(200, 40);
        grid.frozen.set((1, 1));
        app.tick();
        let mut st = scroll.get();
        st.physics.set_position(400.0);
        st.physics_x.set_position(640.0);
        scroll.set(st);
        app.tick();
        // The frozen row's and column's cells and labels are drawn, though
        // scrolled past: row 0 shows its numbers, and "A" and "1" label them.
        let shown: Vec<String> = app.tree().iter().filter_map(|(_, n)| if let Node::Text { content, .. } = n { Some(content.clone()) } else { None }).collect();
        for want in ["0", "A", "1", "12", "21000"] {
            assert!(shown.iter().any(|t| t == want), "{want} in {shown:?}");
        }
        let hw = GridView::header_width(TILE_ROWS * 4);
        let click = |app: &mut App<Host>, x: f32, y: f32| {
            app.handle_event(Event::PointerDown { at: Point::new(x, y) });
            app.handle_event(Event::PointerUp { at: Point::new(x, y) });
            app.tick();
        };
        // A press in the frozen row picks row 0, in the column scrolled to.
        click(&mut app, hw + 64.0 + 2.0 * 64.0 + 10.0, HEADER_H + 5.0);
        assert_eq!(state.get_cloned().cursor, (0, 13));
        // In the frozen column: column 0, in the row scrolled to.
        click(&mut app, hw + 10.0, HEADER_H + 3.0 * 20.0 + 5.0);
        assert_eq!(state.get_cloned().cursor, (23, 0));
        // Up to row 20 from 21: it scrolls to show it just under the frozen row.
        let mut g = state.get_cloned();
        g.select(21, 12);
        state.set(g);
        app.tick();
        key(&mut app, Key::Up, "", Modifiers::default());
        assert_eq!(state.get_cloned().cursor, (20, 12));
        assert_eq!(scroll.get().physics.y, 380.0);
        // Moving into the frozen row doesn't scroll.
        let mut g = state.get_cloned();
        g.select(1, 12);
        state.set(g);
        key(&mut app, Key::Up, "", Modifiers::default());
        assert_eq!(state.get_cloned().cursor, (0, 12));
        assert_eq!(scroll.get().physics.y, 380.0);
    }

    #[test]
    fn rows_frozen_from_the_top_of_the_view_hide_those_above() {
        let (mut app, grid, scroll, state, _) = host(200, 40);
        // Rows 10 to 14 frozen, as when the view started at row 10.
        grid.frozen.set((15, 0));
        grid.from.set((10, 0));
        app.tick();
        // Rows above 10 are out of reach: the view starts at row 10's top.
        assert_eq!(scroll.get().physics.y, 200.0);
        let mut st = scroll.get();
        st.physics.set_position(0.0);
        scroll.set(st);
        app.tick();
        assert_eq!(scroll.get().physics.y, 200.0);
        let shown: Vec<String> = app.tree().iter().filter_map(|(_, n)| if let Node::Text { content, .. } = n { Some(content.clone()) } else { None }).collect();
        assert!(shown.iter().any(|t| t == "11") && shown.iter().any(|t| t == "15"), "{shown:?}");
        // A press at the band's top is row 10; just below the band, row 15.
        let hw = GridView::header_width(TILE_ROWS * 4);
        let click = |app: &mut App<Host>, x: f32, y: f32| {
            app.handle_event(Event::PointerDown { at: Point::new(x, y) });
            app.handle_event(Event::PointerUp { at: Point::new(x, y) });
            app.tick();
        };
        click(&mut app, hw + 10.0, HEADER_H + 5.0);
        assert_eq!(state.get_cloned().cursor, (10, 0));
        click(&mut app, hw + 10.0, HEADER_H + 5.0 * 20.0 + 5.0);
        assert_eq!(state.get_cloned().cursor, (15, 0));
        // Scrolled down, the band still shows rows 10 to 14.
        let mut st = scroll.get();
        st.physics.set_position(1000.0);
        scroll.set(st);
        app.tick();
        click(&mut app, hw + 10.0, HEADER_H + 4.0 * 20.0 + 5.0);
        assert_eq!(state.get_cloned().cursor, (14, 0));
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
    fn crossing_a_rebase_moves_tiles_without_resending_them() {
        let (mut app, _, scroll, _, _) = host(1_000_000, 100);
        app.tick();
        let mut st = scroll.get();
        st.physics.set_position(REBASE - 100.0);
        scroll.set(st);
        app.tick();
        let mut st = scroll.get();
        st.physics.set_position(REBASE + 100.0);
        scroll.set(st);
        let d = app.tick();
        let mut kinds = std::collections::BTreeMap::new();
        for x in &d {
            let k = match x {
                NodeDelta::Added { node, .. } => format!("add {:?}", std::mem::discriminant(node)),
                NodeDelta::Changed { node, .. } => format!("chg {}", match node { Node::Stack { .. } => "stack", Node::Scroll { .. } => "scroll", Node::Text { .. } => "text", Node::Rect { .. } => "rect", _ => "other" }),
                NodeDelta::Removed { .. } => "rm".into(),
            };
            *kinds.entry(k).or_insert(0) += 1;
        }
        // Only the viewports and the tiles' groups: nothing inside a tile.
        assert!(d.iter().all(|x| matches!(x, NodeDelta::Changed { node: Node::Stack { .. } | Node::Scroll { .. }, .. })), "{kinds:?}");
        assert!(d.len() < 40, "{kinds:?}");
    }

    #[test]
    fn a_merged_area_is_one_cell_to_the_pointer_and_the_keys() {
        let (mut app, grid, _, state, _) = host(100, 20);
        // B2:D3 merged.
        grid.merges.borrow_mut().push(CellRange { r0: 1, c0: 1, r1: 2, c1: 3 });
        grid.cells.borrow_mut().insert((1, 1), "Title".into());
        app.tick();
        // A press on C3, inside it, puts the cursor on its top-left.
        let hw = GridView::header_width(TILE_ROWS * 2);
        let p = Point::new(hw + 2.0 * 64.0 + 10.0, HEADER_H + 2.0 * 20.0 + 5.0);
        app.handle_event(Event::PointerDown { at: p });
        app.handle_event(Event::PointerUp { at: p });
        app.tick();
        assert_eq!(state.get_cloned().cursor, (1, 1));
        let none = Modifiers::default();
        // Out past its edges, and back in onto its top-left.
        key(&mut app, Key::Right, "", none);
        assert_eq!(state.get_cloned().cursor, (1, 4));
        key(&mut app, Key::Left, "", none);
        assert_eq!(state.get_cloned().cursor, (1, 1));
        key(&mut app, Key::Down, "", none);
        assert_eq!(state.get_cloned().cursor, (3, 1));
        key(&mut app, Key::Right, "", none);
        key(&mut app, Key::Up, "", none);
        assert_eq!(state.get_cloned().cursor, (1, 1));
        // A selection meeting part of it shows all of it.
        let src: &dyn GridSource = &*grid;
        assert_eq!(grow_to_merges(src, CellRange { r0: 0, c0: 2, r1: 1, c1: 2 }), CellRange { r0: 0, c0: 1, r1: 2, c1: 3 });
    }

    #[test]
    fn enter_and_tab_step_through_a_selection_end_mode_and_ctrl_wheel_zoom() {
        let (mut app, grid, _, state, log) = host(100, 20);
        app.tick();
        let hw = GridView::header_width(TILE_ROWS * 2);
        let p = Point::new(hw + 64.0 + 10.0, HEADER_H + 20.0 + 5.0);
        app.handle_event(Event::PointerDown { at: p });
        app.handle_event(Event::PointerUp { at: p });
        app.tick();
        let none = Modifiers::default();
        let shift = Modifiers { shift: true, ..Default::default() };
        let ctrl = Modifiers { ctrl: true, ..Default::default() };
        // B2:C3 selected, the active cell at B2.
        let mut g = state.get_cloned();
        g.anchor = (2, 2);
        g.cursor = (1, 1);
        state.set(g);
        app.tick();
        let sel = CellRange { r0: 1, c0: 1, r1: 2, c1: 2 };
        // Enter goes down the column, then to the next column's top, round to the first.
        for want in [(2, 1), (1, 2), (2, 2), (1, 1)] {
            key(&mut app, Key::Enter, "", none);
            let g = state.get_cloned();
            assert_eq!((g.cursor, g.selection(), g.edit.is_none()), (want, sel, true));
        }
        // Tab goes across the row; Shift+Tab and Shift+Enter go back.
        key(&mut app, Key::Tab, "", none);
        assert_eq!((state.get_cloned().cursor, state.get_cloned().selection()), ((1, 2), sel));
        key(&mut app, Key::Tab, "", none);
        assert_eq!(state.get_cloned().cursor, (2, 1));
        key(&mut app, Key::Tab, "", shift);
        assert_eq!(state.get_cloned().cursor, (1, 2));
        key(&mut app, Key::Enter, "", shift);
        assert_eq!((state.get_cloned().cursor, state.get_cloned().selection()), ((2, 1), sel));
        // Typing enters the active cell; Tab enters it and goes on in the selection.
        key(&mut app, Key::Char, "7", none);
        key(&mut app, Key::Tab, "", none);
        assert_eq!(grid.cells.borrow().get(&(2, 1)).map(String::as_str), Some("7"));
        assert!(matches!(log.borrow().last(), Some(GridAction::Commit { row: 2, col: 1, .. })));
        assert_eq!((state.get_cloned().cursor, state.get_cloned().selection()), ((2, 2), sel));
        // Shift+arrow extends from where the active cell is, dropping the stepping.
        key(&mut app, Key::Up, "", shift);
        assert_eq!(state.get_cloned().selection(), CellRange { r0: 1, c0: 2, r1: 2, c1: 2 });
        // A lone cell: Enter edits, as before.
        key(&mut app, Key::Right, "", none);
        key(&mut app, Key::Enter, "", none);
        assert!(state.get_cloned().edit.is_some());
        key(&mut app, Key::Escape, "", none);
        // End, then an arrow: the data's edge (the grid's end here), as Ctrl+arrow.
        key(&mut app, Key::End, "", none);
        assert!(state.get_cloned().end_mode());
        key(&mut app, Key::Down, "", none);
        let g = state.get_cloned();
        assert_eq!((g.cursor, g.end_mode()), ((99, 3), false));
        // Without End, Down is one row.
        key(&mut app, Key::Up, "", none);
        assert_eq!(state.get_cloned().cursor, (98, 3));
        // End, Shift+Left extends to the edge; End twice is off again.
        key(&mut app, Key::End, "", none);
        key(&mut app, Key::Left, "", shift);
        assert_eq!(state.get_cloned().selection(), CellRange { r0: 98, c0: 0, r1: 98, c1: 3 });
        key(&mut app, Key::End, "", none);
        key(&mut app, Key::End, "", none);
        assert!(!state.get_cloned().end_mode());
        // End, Home: the last used cell.
        key(&mut app, Key::End, "", none);
        key(&mut app, Key::Home, "", none);
        assert_eq!(state.get_cloned().cursor, (99, 19));
        // Ctrl+Shift+Space: everything.
        key(&mut app, Key::Char, " ", Modifiers { ctrl: true, shift: true, ..Default::default() });
        assert_eq!(state.get_cloned().selection(), CellRange { r0: 0, c0: 0, r1: 99, c1: 19 });
        // Ctrl+wheel asks for zoom, wheel up in, down out, and doesn't scroll.
        log.borrow_mut().clear();
        app.handle_event(Event::Scroll { at: Some(Point::new(400.0, 300.0)), dx: 0.0, dy: -40.0, modifiers: ctrl, phase: ScrollPhase::Update });
        app.handle_event(Event::Scroll { at: Some(Point::new(400.0, 300.0)), dx: 0.0, dy: 40.0, modifiers: ctrl, phase: ScrollPhase::Update });
        app.tick();
        let zooms: Vec<f32> = log.borrow().iter().filter_map(|a| if let GridAction::Zoom(f) = a { Some(*f) } else { None }).collect();
        assert_eq!(zooms.len(), 2, "{:?}", log.borrow());
        assert!(zooms[0] > 1.0 && zooms[1] < 1.0, "{zooms:?}");
    }

    #[test]
    fn gridlines_and_headings_can_be_turned_off() {
        let (mut app, grid, scroll, state, _) = host(100, 20);
        app.tick();
        let thin = |app: &App<Host>| app.tree().iter().filter(|(_, n)| matches!(n, Node::Rect { rect, .. } if rect.w() == 1.0 || rect.h() == 1.0)).count();
        let label = |app: &App<Host>, l: &str| app.tree().iter().any(|(_, n)| matches!(n, Node::Text { content, .. } if content.as_str() == l));
        let lines = thin(&app);
        assert!(label(&app, "B"), "column B's heading shows");
        grid.view.set((false, true));
        scroll.set(scroll.get());
        app.tick();
        assert!(thin(&app) * 4 < lines, "gridlines gone: {} of {lines}", thin(&app));
        assert!(label(&app, "B"));
        grid.view.set((false, false));
        scroll.set(scroll.get());
        app.tick();
        assert!(!label(&app, "B"), "no headings");
        // The cells start at the view's top left: B2 is at (64, 20).
        let p = Point::new(64.0 + 10.0, 20.0 + 5.0);
        app.handle_event(Event::PointerDown { at: p });
        app.handle_event(Event::PointerUp { at: p });
        app.tick();
        assert_eq!(state.get_cloned().cursor, (1, 1));
    }

    #[test]
    fn headers_scale_with_the_zoom_within_half_to_twice() {
        assert_eq!((header_height(1.0), header_height(2.0), header_height(1.5), header_height(0.1), header_height(4.0)), (HEADER_H, 48.0, 36.0, 12.0, 48.0));
        assert_eq!(GridView::header_width_at(99, 1.0), GridView::header_width(99));
        assert!(GridView::header_width_at(99, 2.0) >= 2.0 * GridView::header_width(99) - 1.0);
        assert_eq!(header_scale(f32::NAN), 1.0);
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
        // Typing starts an edit; Enter commits and goes on to the next cell
        // of the selection (round to its top), which stays.
        key(&mut app, Key::Char, "4", none);
        key(&mut app, Key::Char, "2", none);
        assert_eq!(state.get_cloned().edit.as_ref().map(|e| e.text().to_string()), Some("42".into()));
        key(&mut app, Key::Enter, "", none);
        assert_eq!(grid.cells.borrow().get(&(3, 2)).map(String::as_str), Some("42"));
        assert_eq!(state.get_cloned().cursor, (1, 2));
        assert_eq!(state.get_cloned().selection(), CellRange { r0: 1, c0: 2, r1: 3, c1: 2 });
        assert!(matches!(log.borrow().last(), Some(GridAction::Commit { row: 3, col: 2, .. })));
        // A plain arrow drops the selection.
        key(&mut app, Key::Down, "", none);
        assert_eq!(state.get_cloned().selection(), CellRange { r0: 2, c0: 2, r1: 2, c1: 2 });
        // Enter edits what is there; Escape leaves it.
        key(&mut app, Key::Down, "", none);
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
    fn excel_keys_for_the_active_cell_corners_and_screens() {
        let (mut app, _, scroll, state, log) = host(10_000, 200);
        app.tick();
        let hw = GridView::header_width(TILE_ROWS * 2);
        let p = Point::new(hw + 5.0, HEADER_H + 5.0);
        app.handle_event(Event::PointerDown { at: p });
        app.handle_event(Event::PointerUp { at: p });
        log.borrow_mut().clear();
        let ctrl = Modifiers { ctrl: true, ..Default::default() };
        let shift = Modifiers { shift: true, ..Default::default() };
        let alt = Modifiers { alt: true, ..Default::default() };
        // Ctrl+. walks the corners clockwise; the selection stays.
        let mut g = state.get_cloned();
        g.cursor = (2, 1);
        g.anchor = (5, 4);
        state.set(g);
        let sel = CellRange { r0: 2, c0: 1, r1: 5, c1: 4 };
        for want in [(2, 4), (5, 4), (5, 1), (2, 1)] {
            key(&mut app, Key::Char, ".", ctrl);
            assert_eq!((state.get_cloned().cursor, state.get_cloned().selection()), (want, sel));
        }
        assert!(log.borrow().is_empty(), "Ctrl+. is not a format key");
        // Shift+Backspace: only the active cell; nothing is cleared.
        key(&mut app, Key::Backspace, "", shift);
        assert_eq!(state.get_cloned().selection(), CellRange { r0: 2, c0: 1, r1: 2, c1: 1 });
        assert!(log.borrow().is_empty());
        // Ctrl+Backspace: scrolled away, back to the active cell, which stays.
        let mut st = scroll.get();
        st.physics.set_max(1e9);
        st.physics.set_position(50_000.0);
        scroll.set(st);
        key(&mut app, Key::Backspace, "", ctrl);
        assert_eq!(scroll.get().physics.y, 2.0 * 20.0, "row 3 at the top");
        assert_eq!(state.get_cloned().cursor, (2, 1));
        assert!(log.borrow().is_empty(), "Ctrl+Backspace clears nothing");
        // Alt+PageDown / Alt+PageUp: a screen right and back.
        key(&mut app, Key::PageDown, "", alt);
        let c = state.get_cloned().cursor;
        assert_eq!(c.0, 2);
        assert!(c.1 > 3, "{c:?}");
        assert!(scroll.get().physics_x.y > 0.0);
        key(&mut app, Key::PageUp, "", alt);
        assert_eq!(state.get_cloned().cursor, (2, 1));
        assert_eq!(log.borrow().len(), 0, "Alt+PageDown doesn't change sheet");
        // Ctrl+Alt+V asks for Paste Special; Ctrl+V still pastes.
        key(&mut app, Key::Char, "v", Modifiers { ctrl: true, alt: true, ..Default::default() });
        assert_eq!(log.borrow().last(), Some(&GridAction::PasteSpecial));
        key(&mut app, Key::Char, "v", ctrl);
        assert!(matches!(log.borrow().last(), Some(GridAction::Paste { plain: false, .. })));
    }

    #[test]
    fn typing_completes_from_the_source_and_the_next_key_replaces_it() {
        let (mut app, grid, _, state, log) = host(100, 30);
        grid.cells.borrow_mut().insert((0, 1), "Apple".into());
        grid.cells.borrow_mut().insert((1, 1), "Banana".into());
        grid.cells.borrow_mut().insert((2, 1), "Bandana".into());
        app.tick();
        let hw = GridView::header_width(TILE_ROWS * 2);
        let p = Point::new(hw + 5.0, HEADER_H + 5.0);
        app.handle_event(Event::PointerDown { at: p });
        app.handle_event(Event::PointerUp { at: p });
        let mut g = state.get_cloned();
        g.select(5, 1);
        state.set(g);
        let none = Modifiers::default();
        let text = |st: &Mutable<GridState>| st.get_cloned().edit.map(|e| (e.text().to_string(), e.selected_text().to_string()));
        // One entry: finished, the rest selected; the case is the entry's.
        key(&mut app, Key::Char, "a", none);
        assert_eq!(text(&state), Some(("Apple".into(), "pple".into())));
        key(&mut app, Key::Enter, "", none);
        assert_eq!(log.borrow().last(), Some(&GridAction::Commit { row: 5, col: 1, text: "Apple".into() }));
        // Two entries: nothing until one is left.
        let mut g = state.get_cloned();
        g.select(6, 1);
        state.set(g);
        for (ch, want) in [("b", ("b", "")), ("a", ("ba", "")), ("n", ("ban", "")), ("d", ("Bandana", "ana"))] {
            key(&mut app, Key::Char, ch, none);
            assert_eq!(text(&state), Some((want.0.into(), want.1.into())), "after {ch}");
        }
        // Typing on replaces the guess; Backspace drops it.
        key(&mut app, Key::Char, "x", none);
        assert_eq!(text(&state), Some(("Bandx".into(), "".into())));
        key(&mut app, Key::Escape, "", none);
        let mut g = state.get_cloned();
        g.select(7, 1);
        state.set(g);
        key(&mut app, Key::Char, "a", none);
        key(&mut app, Key::Backspace, "", none);
        assert_eq!(text(&state), Some(("A".into(), "".into())));
        // Another column has nothing to offer.
        key(&mut app, Key::Escape, "", none);
        let mut g = state.get_cloned();
        g.select(7, 2);
        state.set(g);
        key(&mut app, Key::Char, "a", none);
        assert_eq!(text(&state), Some(("a".into(), "".into())));
        key(&mut app, Key::Escape, "", none);
        // A formula's guess: Tab takes it and editing goes on; Enter drops it.
        grid.cells.borrow_mut().insert((0, 2), "=SUMPRODUCT(".into());
        for ch in ["=", "s"] {
            key(&mut app, Key::Char, ch, none);
        }
        assert_eq!(text(&state), Some(("=SUMPRODUCT(".into(), "UMPRODUCT(".into())));
        key(&mut app, Key::Tab, "", none);
        assert_eq!(text(&state), Some(("=SUMPRODUCT(".into(), "".into())));
        assert_eq!(state.get_cloned().edit.unwrap().caret(), 12);
        key(&mut app, Key::Escape, "", none);
        let n = log.borrow().len();
        for ch in ["=", "s"] {
            key(&mut app, Key::Char, ch, none);
        }
        key(&mut app, Key::Enter, "", none);
        assert_eq!(log.borrow()[n..], [GridAction::Commit { row: 7, col: 2, text: "=S".into() }], "the guess dropped; the typed letters take the name's case");
        // A manual selection of a formula's tail is not a guess: Enter keeps it.
        let mut g = state.get_cloned();
        g.select(8, 3);
        let mut ed = TextEdit::new("=1+2");
        ed.set_caret(2, false);
        ed.set_caret(4, true);
        g.edit = Some(ed);
        state.set(g);
        key(&mut app, Key::Enter, "", none);
        assert_eq!(log.borrow().last(), Some(&GridAction::Commit { row: 8, col: 3, text: "=1+2".into() }));
        // Tab reaches the grid (not focus traversal): one cell right, as in Excel.
        let c = state.get_cloned().cursor;
        key(&mut app, Key::Tab, "", none);
        assert_eq!(state.get_cloned().cursor, (c.0, c.1 + 1));
    }

    #[test]
    fn f8_extends_the_selection_until_pressed_again_or_escape() {
        let (mut app, _, _, state, _) = host(100, 30);
        app.tick();
        let hw = GridView::header_width(TILE_ROWS * 2);
        let p = Point::new(hw + 5.0, HEADER_H + 5.0);
        app.handle_event(Event::PointerDown { at: p });
        app.handle_event(Event::PointerUp { at: p });
        let mut g = state.get_cloned();
        g.select(2, 2);
        state.set(g);
        let none = Modifiers::default();
        key(&mut app, Key::F(8), "", none);
        assert!(state.get_cloned().extending());
        key(&mut app, Key::Down, "", none);
        key(&mut app, Key::Right, "", none);
        assert_eq!(state.get_cloned().selection(), CellRange { r0: 2, c0: 2, r1: 3, c1: 3 });
        // F8 again: arrows move again.
        key(&mut app, Key::F(8), "", none);
        key(&mut app, Key::Down, "", none);
        assert_eq!(state.get_cloned().selection(), CellRange { r0: 4, c0: 3, r1: 4, c1: 3 });
        // Escape turns it off too; typing ends it and edits the active cell.
        key(&mut app, Key::F(8), "", none);
        key(&mut app, Key::Escape, "", none);
        assert!(!state.get_cloned().extending());
        key(&mut app, Key::F(8), "", none);
        key(&mut app, Key::Char, "x", none);
        assert!(!state.get_cloned().extending());
        assert!(state.get_cloned().edit.is_some());
    }

    #[test]
    fn enter_after_tabs_goes_to_the_next_row_where_they_began() {
        let (mut app, _, _, state, log) = host(100, 30);
        app.tick();
        let hw = GridView::header_width(TILE_ROWS * 2);
        let p = Point::new(hw + 5.0, HEADER_H + 5.0);
        app.handle_event(Event::PointerDown { at: p });
        app.handle_event(Event::PointerUp { at: p });
        let mut g = state.get_cloned();
        g.select(2, 1);
        state.set(g);
        let none = Modifiers::default();
        // Type, Tab, type, Tab, type, Enter: row 4 at column B.
        for (ch, k) in [("a", Key::Tab), ("b", Key::Tab), ("c", Key::Enter)] {
            key(&mut app, Key::Char, ch, none);
            key(&mut app, k, "", none);
        }
        assert_eq!(state.get_cloned().cursor, (3, 1));
        let commits: Vec<(u32, u32)> = log.borrow().iter().filter_map(|a| match a { GridAction::Commit { row, col, .. } => Some((*row, *col)), _ => None }).collect();
        assert_eq!(commits, [(2, 1), (2, 2), (2, 3)]);
        // The run carries on from there; Tab outside the editor counts too.
        key(&mut app, Key::Tab, "", none);
        key(&mut app, Key::Char, "d", none);
        key(&mut app, Key::Enter, "", none);
        assert_eq!(state.get_cloned().cursor, (4, 1));
        // An arrow ends the run: Enter then goes straight down.
        key(&mut app, Key::Tab, "", none);
        key(&mut app, Key::Right, "", none);
        key(&mut app, Key::Char, "e", none);
        key(&mut app, Key::Enter, "", none);
        assert_eq!(state.get_cloned().cursor, (5, 3));
        // So does a click.
        key(&mut app, Key::Tab, "", none);
        let mut g = state.get_cloned();
        g.select(10, 5);
        state.set(g);
        key(&mut app, Key::Char, "f", none);
        key(&mut app, Key::Enter, "", none);
        assert_eq!(state.get_cloned().cursor, (11, 5));
    }

    #[test]
    fn dragging_selects_and_the_header_edge_resizes() {
        let (mut app, grid, _, state, log) = host(100, 20);
        app.tick();
        let hw = GridView::header_width(TILE_ROWS * 2);
        let cell = |r: f32, c: f32| Point::new(hw + c * 64.0 + 10.0, HEADER_H + r * 20.0 + 5.0);
        app.handle_event(Event::PointerDown { at: cell(1.0, 1.0) });
        app.handle_event(Event::PointerMove { at: cell(4.0, 3.0) });
        app.handle_event(Event::PointerUp { at: cell(4.0, 3.0) });
        app.tick();
        assert_eq!(state.get_cloned().selection(), CellRange { r0: 1, c0: 1, r1: 4, c1: 3 });
        // The press's end reports the selection it made (for Format Painter).
        assert_eq!(log.borrow().last(), Some(&GridAction::Selected(CellRange { r0: 1, c0: 1, r1: 4, c1: 3 })));
        assert_eq!(log.borrow().iter().filter(|a| matches!(a, GridAction::Selected(_))).count(), 1, "once, at the end");
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
        assert_eq!(log.borrow().last(), Some(&GridAction::Selected(CellRange { r0: 0, c0: 2, r1: 99, c1: 2 })), "a header press reports its selection");
    }

    #[test]
    fn a_row_label_s_edge_resizes_the_row() {
        let (mut app, _, _, state, log) = host(100, 20);
        app.tick();
        let hw = GridView::header_width(TILE_ROWS * 2);
        // Row 2's bottom edge (40 px into the rows), dragged 15 px down.
        let edge = Point::new(hw / 2.0, HEADER_H + 40.0 - 1.0);
        app.handle_event(Event::PointerDown { at: edge });
        app.handle_event(Event::PointerMove { at: Point::new(edge.x, edge.y + 15.0) });
        app.handle_event(Event::PointerUp { at: Point::new(edge.x, edge.y + 15.0) });
        app.tick();
        let got: Vec<_> = log.borrow().iter().filter(|a| matches!(a, GridAction::ResizeRow { .. } | GridAction::ResizeEnd)).cloned().collect();
        assert_eq!(got, [GridAction::ResizeRow { row: 1, height: 35.0 }, GridAction::ResizeEnd]);
        // Just below the edge, the next row's top: still row 2. Mid-label: selects.
        log.borrow_mut().clear();
        std::thread::sleep(std::time::Duration::from_millis(500));
        app.handle_event(Event::PointerDown { at: Point::new(edge.x, HEADER_H + 41.0) });
        app.handle_event(Event::PointerMove { at: Point::new(edge.x, HEADER_H + 31.0) });
        app.handle_event(Event::PointerUp { at: Point::new(edge.x, HEADER_H + 31.0) });
        assert_eq!(log.borrow().first(), Some(&GridAction::ResizeRow { row: 1, height: MIN_ROW_H.max(10.0) }));
        app.handle_event(Event::PointerDown { at: Point::new(edge.x, HEADER_H + 70.0) });
        app.handle_event(Event::PointerUp { at: Point::new(edge.x, HEADER_H + 70.0) });
        app.tick();
        assert_eq!(state.get_cloned().selection(), CellRange { r0: 3, c0: 0, r1: 3, c1: 19 });
    }

    #[test]
    fn a_double_click_on_a_row_edge_asks_to_fit_it_and_wrapped_text_is_taller() {
        let (mut app, _, _, _, log) = host(100, 20);
        app.tick();
        let hw = GridView::header_width(TILE_ROWS * 2);
        let edge = Point::new(hw / 2.0, HEADER_H + 40.0 - 1.0);
        for _ in 0..2 {
            app.handle_event(Event::PointerDown { at: edge });
            app.handle_event(Event::PointerUp { at: edge });
            app.tick();
        }
        assert_eq!(log.borrow().last(), Some(&GridAction::FitRow(1)));
        crate::text::clear_measurer();
        let long = "one two three four five six seven eight";
        let one = needed_height(&CellView::text("abc"), 64.0);
        assert_eq!(needed_height(&CellView::text(long), 64.0), one);
        let wrapped = needed_height(&CellView { wrap: true, ..CellView::text(long) }, 64.0);
        assert!(wrapped >= 3.0 * one - 2.0 * 2.0 * LINE_PAD as f64, "{one} {wrapped}");
        assert!(needed_height(&CellView::text("a\nb"), 64.0) > one);
        let style = editor_style(1.0);
        let lines = text_lines(&CellView { wrap: true, ..CellView::text(long) }, 60.0, &style);
        assert!(lines.len() > 2 && lines.iter().all(|l| crate::text::measure(l.trim_end(), &style).w <= 60.0), "{lines:?}");
        assert_eq!(lines.iter().map(|l| l.trim()).collect::<Vec<_>>().join(" "), long);
        // Lines sit at the top, middle or bottom of a taller cell.
        assert_eq!(line_top(CellVAlign::Top, 60.0, 17.0), LINE_PAD);
        assert_eq!(line_top(CellVAlign::Center, 60.0, 17.0), 21.5);
        assert_eq!(line_top(CellVAlign::Bottom, 60.0, 17.0), 60.0 - 17.0 - LINE_PAD);
        assert_eq!(line_top(CellVAlign::Bottom, 10.0, 17.0), 0.0);
        // A word too long breaks between letters.
        let lines = text_lines(&CellView { wrap: true, ..CellView::text("abcdefghijklmnopqrstuvwxyz") }, 30.0, &style);
        assert!(lines.len() > 1 && lines.concat().replace('-', "") == "abcdefghijklmnopqrstuvwxyz", "{lines:?}");
    }

    #[test]
    fn a_double_click_on_a_column_edge_asks_to_fit_it() {
        let (mut app, _, _, _, log) = host(100, 20);
        app.tick();
        let hw = GridView::header_width(TILE_ROWS * 2);
        let edge = Point::new(hw + 128.0 - 1.0, HEADER_H / 2.0);
        for _ in 0..2 {
            app.handle_event(Event::PointerDown { at: edge });
            app.handle_event(Event::PointerUp { at: edge });
            app.tick();
        }
        assert_eq!(log.borrow().last(), Some(&GridAction::FitCol(1)));
        // Longer text needs more; an indent adds a step.
        let a = needed_width(&CellView::text("abc"));
        let b = needed_width(&CellView::text("abcdefghij"));
        assert!(b > a && a > 2.0 * PAD as f64, "{a} {b}");
        assert_eq!(needed_width(&CellView { indent: 1, ..CellView::text("abc") }), a + INDENT_STEP as f64);
    }

    #[test]
    fn a_selection_held_past_the_edge_scrolls_on() {
        let (mut app, _, scroll, state, _) = host(1000, 50);
        app.tick();
        let hw = GridView::header_width(TILE_ROWS * 2);
        let start = Point::new(hw + 10.0, HEADER_H + 5.0);
        app.handle_event(Event::PointerDown { at: start });
        // Below the grid (it is 600 px high), held there.
        let below = Point::new(hw + 10.0, 640.0);
        app.handle_event(Event::PointerMove { at: below });
        app.tick();
        let first = state.get_cloned().cursor.0;
        assert!(app.wants_frame());
        for i in 0..20 {
            app.set_time(16.0 * i as f64);
            app.frame();
            app.tick();
        }
        let y = scroll.get().physics.y;
        let row = state.get_cloned().cursor.0;
        assert!(y > 300.0, "{y}");
        assert!(row > first + 10, "{first} -> {row}");
        // Above the grid: back up. Let go: it stops.
        app.handle_event(Event::PointerMove { at: Point::new(hw + 10.0, 2.0) });
        for i in 20..25 {
            app.set_time(16.0 * i as f64);
            app.frame();
            app.tick();
        }
        assert!(scroll.get().physics.y < y);
        app.handle_event(Event::PointerUp { at: Point::new(hw + 10.0, 2.0) });
        app.tick();
        let y2 = scroll.get().physics.y;
        for i in 25..30 {
            app.set_time(16.0 * i as f64);
            app.frame();
            app.tick();
        }
        assert_eq!(scroll.get().physics.y, y2);
        assert_eq!(state.get_cloned().selection().c0, 0);
    }

    #[test]
    fn a_press_in_the_editor_places_the_caret_and_a_drag_selects() {
        let (mut app, grid, _, state, log) = host(100, 20);
        app.tick();
        let hw = GridView::header_width(TILE_ROWS * 2);
        let b2 = Point::new(hw + 64.0 + 10.0, HEADER_H + 25.0);
        app.handle_event(Event::PointerDown { at: b2 });
        app.handle_event(Event::PointerUp { at: b2 });
        app.tick();
        key(&mut app, Key::Char, "hello world", Modifiers::default());
        let style = editor_style(1.0);
        let x_of = |s: &str| hw + 64.0 + PAD + crate::text::measure(s, &style).w;
        // A press just past "hello": the caret goes there, still editing.
        let at = Point::new(x_of("hello") + 1.0, HEADER_H + 30.0);
        app.handle_event(Event::PointerDown { at });
        app.handle_event(Event::PointerUp { at });
        app.tick();
        let g = state.get_cloned();
        assert_eq!(g.edit.as_ref().map(|e| e.caret()), Some(5));
        assert!(!log.borrow().iter().any(|a| matches!(a, GridAction::Commit { .. })));
        // Dragged from the start to past "hello w": that is selected.
        std::thread::sleep(std::time::Duration::from_millis(500));
        app.handle_event(Event::PointerDown { at: Point::new(x_of("") + 1.0, HEADER_H + 30.0) });
        app.handle_event(Event::PointerMove { at: Point::new(x_of("hello w") + 1.0, HEADER_H + 30.0) });
        app.handle_event(Event::PointerUp { at: Point::new(x_of("hello w") + 1.0, HEADER_H + 30.0) });
        app.tick();
        assert_eq!(state.get_cloned().edit.unwrap().selected_text(), "hello w");
        // A double click picks the word under it.
        std::thread::sleep(std::time::Duration::from_millis(500));
        let on_world = Point::new(x_of("hello wo") + 1.0, HEADER_H + 30.0);
        for _ in 0..2 {
            app.handle_event(Event::PointerDown { at: on_world });
            app.handle_event(Event::PointerUp { at: on_world });
        }
        app.tick();
        assert_eq!(state.get_cloned().edit.unwrap().selected_text(), "world");
        // A plain press, then Shift and a press, selects between them.
        std::thread::sleep(std::time::Duration::from_millis(500));
        let at2 = Point::new(x_of("he") + 1.0, HEADER_H + 30.0);
        app.handle_event(Event::PointerDown { at: at2 });
        app.handle_event(Event::PointerUp { at: at2 });
        std::thread::sleep(std::time::Duration::from_millis(500));
        crate::input::set_pointer_modifiers(Modifiers { shift: true, ..Default::default() });
        let at3 = Point::new(x_of("hello") + 1.0, HEADER_H + 30.0);
        app.handle_event(Event::PointerDown { at: at3 });
        app.handle_event(Event::PointerUp { at: at3 });
        crate::input::set_pointer_modifiers(Modifiers::default());
        app.tick();
        assert_eq!(state.get_cloned().edit.unwrap().selected_text(), "llo");
        // Alt+Enter breaks the line and keeps editing; Enter commits.
        key(&mut app, Key::End, "", Modifiers::default());
        key(&mut app, Key::Enter, "", Modifiers { alt: true, ..Default::default() });
        key(&mut app, Key::Char, "x", Modifiers::default());
        assert_eq!(state.get_cloned().edit.unwrap().text(), "hello world\nx");
        // The editor grows a line; a press on the second line lands there.
        let g = state.get_cloned();
        let r = editor_rect(&*grid, &g).unwrap();
        assert!(r[3] > 20.0, "{r:?}");
        std::thread::sleep(std::time::Duration::from_millis(500));
        let second = Point::new(x_of("") + 1.0, HEADER_H + 20.0 + LINE_PAD + TEXT_SIZE * 1.3 + 3.0);
        app.handle_event(Event::PointerDown { at: second });
        app.handle_event(Event::PointerUp { at: second });
        app.tick();
        assert_eq!(state.get_cloned().edit.as_ref().map(|e| e.caret()), Some("hello world\n".len()));
        // Up and Down move between its lines; Down past the last ends it.
        key(&mut app, Key::End, "", Modifiers::default());
        key(&mut app, Key::Up, "", Modifiers::default());
        assert_eq!(state.get_cloned().edit.as_ref().map(|e| e.caret()), Some(1));
        key(&mut app, Key::Down, "", Modifiers::default());
        assert_eq!(state.get_cloned().edit.as_ref().map(|e| e.caret()), Some("hello world\nx".len()));
        key(&mut app, Key::Down, "", Modifiers::default());
        assert!(state.get_cloned().edit.is_none());
        assert!(log.borrow().iter().any(|a| matches!(a, GridAction::Commit { row: 1, col: 1, text } if text == "hello world\nx")));
        log.borrow_mut().clear();
        key(&mut app, Key::Char, "z", Modifiers::default());
        // A press on another cell still commits and moves there.
        let c5 = Point::new(hw + 128.0 + 10.0, HEADER_H + 85.0);
        app.handle_event(Event::PointerDown { at: c5 });
        app.handle_event(Event::PointerUp { at: c5 });
        app.tick();
        assert!(log.borrow().iter().any(|a| matches!(a, GridAction::Commit { row: 2, col: 1, .. })));
    }

    #[test]
    fn ctrl_enter_enters_the_text_in_every_selected_cell() {
        let (mut app, _, _, state, log) = host(100, 20);
        app.tick();
        let hw = GridView::header_width(TILE_ROWS * 2);
        let b2 = Point::new(hw + 64.0 + 10.0, HEADER_H + 25.0);
        app.handle_event(Event::PointerDown { at: b2 });
        app.handle_event(Event::PointerUp { at: b2 });
        app.tick();
        let shift = Modifiers { shift: true, ..Default::default() };
        key(&mut app, Key::Down, "", shift);
        key(&mut app, Key::Right, "", shift);
        key(&mut app, Key::Char, "7", Modifiers::default());
        key(&mut app, Key::Enter, "", Modifiers { ctrl: true, ..Default::default() });
        let sel = CellRange { r0: 1, c0: 1, r1: 2, c1: 2 };
        assert_eq!(log.borrow().last(), Some(&GridAction::CommitAll { sel, text: "7".into() }));
        let g = state.get_cloned();
        assert!(g.edit.is_none());
        assert_eq!(g.selection(), sel);
        // Ctrl+; with no date to give does nothing.
        key(&mut app, Key::Char, ";", Modifiers { ctrl: true, ..Default::default() });
        assert!(state.get_cloned().edit.is_none());
    }

    #[test]
    fn the_editor_wraps_as_its_cell_does() {
        let style = editor_style(1.0);
        // Broken after spaces within the width; a word too long, inside it.
        let text = "aaa bbb ccc\ndddddddddddddddddddd";
        let w = crate::text::measure("aaa bbb ", &style).w + 1.0;
        let lines: Vec<&str> = editor_lines(text, Some(w), &style).into_iter().map(|r| &text[r]).collect();
        assert_eq!(lines[..2], ["aaa bbb ", "ccc"]);
        assert!(lines.len() > 3 && lines[2..].concat() == "dddddddddddddddddddd", "{lines:?}");
        assert_eq!(editor_lines(text, None, &style).len(), 2);
        // In the grid: F2 on a wrapping cell, Down moves to the next line it shows.
        let (mut app, grid, _, state, _) = host(100, 20);
        grid.cells.borrow_mut().insert((1, 1), "¶one two three four five six".into());
        app.tick();
        let hw = GridView::header_width(TILE_ROWS * 2);
        let b2 = Point::new(hw + 64.0 + 10.0, HEADER_H + 25.0);
        app.handle_event(Event::PointerDown { at: b2 });
        app.handle_event(Event::PointerUp { at: b2 });
        app.tick();
        key(&mut app, Key::F(2), "", Modifiers::default());
        let g = state.get_cloned();
        let ed = g.edit.as_ref().expect("editing");
        assert!(ed.text().ends_with("five six"));
        let lines = editor_lines(ed.text(), editor_wrap(&*grid, &g), &style);
        assert!(lines.len() >= 3, "{lines:?}");
        // The caret at the end: Up goes to the line before, still editing.
        key(&mut app, Key::Up, "", Modifiers::default());
        let g = state.get_cloned();
        let caret = g.edit.as_ref().expect("Up stays in the editor").caret();
        let k = lines.len() - 2;
        assert!((lines[k].start..=lines[k].end).contains(&caret), "{caret} {lines:?}");
        app.tick();
    }

    #[test]
    fn arrows_finish_a_typed_entry_and_f2_edits_in_place() {
        let (mut app, grid, _, state, log) = host(100, 20);
        app.tick();
        let hw = GridView::header_width(TILE_ROWS * 2);
        let b2 = Point::new(hw + 64.0 + 10.0, HEADER_H + 25.0);
        app.handle_event(Event::PointerDown { at: b2 });
        app.handle_event(Event::PointerUp { at: b2 });
        app.tick();
        let none = Modifiers::default();
        // Typed, then Right: committed, one cell right.
        key(&mut app, Key::Char, "ab", none);
        key(&mut app, Key::Right, "", none);
        assert_eq!(log.borrow().last(), Some(&GridAction::Commit { row: 1, col: 1, text: "ab".into() }));
        assert_eq!(state.get_cloned().cursor, (1, 2));
        // Typed, F2, Left: the caret moves instead.
        key(&mut app, Key::Char, "cd", none);
        assert_eq!(state.get_cloned().edit_mode(), Some(EditMode::Enter));
        key(&mut app, Key::F(2), "", none);
        assert_eq!(state.get_cloned().edit_mode(), Some(EditMode::Edit));
        key(&mut app, Key::Left, "", none);
        let g = state.get_cloned();
        assert_eq!((g.cursor, g.edit.as_ref().map(|e| e.caret())), ((1, 2), Some(1)));
        key(&mut app, Key::Escape, "", none);
        assert_eq!(state.get_cloned().edit_mode(), None);
        // F2 on a cell: its text, editing in place.
        grid.cells.borrow_mut().insert((1, 2), "=A1".into());
        key(&mut app, Key::F(2), "", none);
        key(&mut app, Key::Left, "", none);
        assert_eq!(state.get_cloned().edit.as_ref().map(|e| (e.text().to_string(), e.caret())), Some(("=A1".into(), 2)));
        // F4 there: the reference anchored, the caret after it.
        key(&mut app, Key::F(4), "", none);
        assert_eq!(state.get_cloned().edit.as_ref().map(|e| (e.text().to_string(), e.caret())), Some(("=$A$1".into(), 5)));
        key(&mut app, Key::Escape, "", none);
        // Ctrl+' below it: the formula above as written; Ctrl+Shift+': its text.
        key(&mut app, Key::Down, "", none);
        key(&mut app, Key::Char, "'", Modifiers { ctrl: true, ..Default::default() });
        assert_eq!(state.get_cloned().edit.as_ref().map(|e| e.text().to_string()), Some("=A1".into()));
        key(&mut app, Key::Escape, "", none);
        grid.cells.borrow_mut().insert((1, 2), "seen".into());
        key(&mut app, Key::Char, "\"", Modifiers { ctrl: true, shift: true, ..Default::default() });
        assert_eq!(state.get_cloned().edit.as_ref().map(|e| e.text().to_string()), Some("seen".into()));
    }

    #[test]
    fn ctrl_click_selects_and_asks_to_follow() {
        let (mut app, _, _, state, log) = host(100, 20);
        app.tick();
        let hw = GridView::header_width(TILE_ROWS * 2);
        let p = Point::new(hw + 2.0 * 64.0 + 10.0, HEADER_H + 3.0 * 20.0 + 5.0);
        crate::input::set_pointer_modifiers(Modifiers { ctrl: true, ..Default::default() });
        app.handle_event(Event::PointerDown { at: p });
        app.handle_event(Event::PointerMove { at: Point::new(p.x + 80.0, p.y + 40.0) });
        app.handle_event(Event::PointerUp { at: p });
        crate::input::set_pointer_modifiers(Modifiers::default());
        app.tick();
        assert_eq!(state.get_cloned().selection(), CellRange { r0: 3, c0: 2, r1: 3, c1: 2 });
        assert_eq!(log.borrow().last(), Some(&GridAction::Follow { row: 3, col: 2 }));
        // A plain click doesn't.
        log.borrow_mut().clear();
        app.handle_event(Event::PointerDown { at: p });
        app.handle_event(Event::PointerUp { at: p });
        app.tick();
        assert!(!log.borrow().iter().any(|a| matches!(a, GridAction::Follow { .. })));
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

#[cfg(test)]
mod spark_tests {
    use super::*;

    fn shapes(style: SparkStyle, points: &[Option<f64>], markers: bool) -> Vec<Node> {
        let sp = Spark { style, points: points.to_vec(), color: Color::rgba(0.0, 0.0, 1.0, 1.0), negative: Color::rgba(1.0, 0.0, 0.0, 1.0), markers };
        let mut out = Vec::new();
        spark_shapes(&sp, 0.0, 0.0, 84.0, 24.0, |n| out.push(n));
        out
    }

    #[test]
    fn sparklines_draw_segments_bars_and_blocks_inside_the_cell() {
        // A line: a segment between neighbours, none across the gap.
        let line = shapes(SparkStyle::Line, &[Some(1.0), Some(3.0), None, Some(2.0), Some(5.0)], false);
        assert_eq!(line.len(), 2);
        // The highest and lowest marked.
        assert_eq!(shapes(SparkStyle::Line, &[Some(1.0), Some(3.0), Some(2.0)], true).len(), 4);
        // Columns: one each, the negative one in the negative colour, all inside.
        let cols = shapes(SparkStyle::Column, &[Some(2.0), Some(-1.0), Some(4.0)], false);
        assert_eq!(cols.len(), 3);
        for n in &cols {
            let Node::Rect { rect, .. } = n else { panic!("{n:?}") };
            assert!(rect.x() >= 0.0 && rect.y() >= 0.0 && rect.max_x() <= 84.0 && rect.max_y() <= 24.0, "{rect:?}");
        }
        assert!(matches!(cols[1], Node::Rect { fill, .. } if fill.r == 1.0));
        // Win/loss: zeros draw nothing; up above the middle, down below.
        let wl = shapes(SparkStyle::WinLoss, &[Some(1.0), Some(0.0), Some(-2.0)], false);
        assert_eq!(wl.len(), 2);
        let (Node::Rect { rect: up, .. }, Node::Rect { rect: down, .. }) = (&wl[0], &wl[1]) else { panic!() };
        assert!(up.max_y() <= 12.0 && down.y() >= 12.0);
        // Nothing to draw: no numbers, or no room.
        assert!(shapes(SparkStyle::Line, &[None, None], false).is_empty());
        let sp = Spark { style: SparkStyle::Column, points: vec![Some(1.0)], color: Color::TRANSPARENT, negative: Color::TRANSPARENT, markers: false };
        let mut n = 0;
        spark_shapes(&sp, 0.0, 0.0, 6.0, 24.0, |_| n += 1);
        assert_eq!(n, 0);
    }
}
