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
//! Not yet: auto-scroll while dragging a
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
/// The lowest a dragged row goes, px.
pub const MIN_ROW_H: f64 = 6.0;
/// Text inset from a cell's edges.
const PAD: f32 = 4.0;
const TEXT_SIZE: f32 = type_size::SM;

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
    /// Left, right, top, bottom: width in points and colour.
    pub borders: [Option<(f32, Color)>; 4],
    /// A data bar: the share of the cell's width it fills (0–1) and its colour.
    pub bar: Option<(f32, Color)>,
    /// An icon at the cell's left (a conditional format's icon set).
    pub icon: Option<(CellIcon, Color)>,
    /// Steps the text is indented from the left (a pivot's inner items), each
    /// [`INDENT_STEP`] px.
    pub indent: u8,
    /// The cell has a comment: a small triangle in its top-right corner.
    pub note: bool,
}

/// One step of a cell's indent, px.
pub const INDENT_STEP: f32 = 12.0;

/// The icons a cell can show (spreadsheet icon sets), drawn as vectors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    let (mx, my) = (x + s / 2.0, y + s / 2.0);
    let line = |ctx: &mut Ctx, a: (f32, f32), b: (f32, f32), w: f32, col: Color| {
        ctx.add(Node::Path { p0: a, p1: b, width: w, color: col });
    };
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
            line(ctx, (mx - dx * r, my - dy * r), base, w * 1.4, c);
            let hw = s * 0.32;
            ctx.add(poly(&[tip, (base.0 + nx * hw, base.1 + ny * hw), (base.0 - nx * hw, base.1 - ny * hw)], c, None));
        }
        CellIcon::Dot => {
            ctx.add(Node::Rect { rect: Rect::new(x + 1.0, y + 1.0, s - 2.0, s - 2.0), fill: c, radius: (s - 2.0) / 2.0 });
        }
        CellIcon::Flag => {
            line(ctx, (x + s * 0.2, y + s * 0.05), (x + s * 0.2, y + s), w, off);
            ctx.add(poly(&[(x + s * 0.25, y + s * 0.05), (x + s * 0.95, y + s * 0.3), (x + s * 0.25, y + s * 0.6)], c, None));
        }
        CellIcon::Check(ring) | CellIcon::Exclaim(ring) | CellIcon::Cross(ring) => {
            let mark = if ring {
                ctx.add(Node::Rect { rect: Rect::new(x, y, s, s), fill: c, radius: s / 2.0 });
                white
            } else {
                c
            };
            let k = if ring { 0.28 } else { 0.18 };
            match icon {
                CellIcon::Check(_) => {
                    line(ctx, (x + s * k, my), (x + s * 0.43, y + s * (1.0 - k)), w, mark);
                    line(ctx, (x + s * 0.43, y + s * (1.0 - k)), (x + s * (1.0 - k), y + s * k), w, mark);
                }
                CellIcon::Exclaim(_) => {
                    line(ctx, (mx, y + s * k), (mx, y + s * 0.6), w, mark);
                    ctx.add(Node::Rect { rect: Rect::new(mx - w / 2.0, y + s * (0.95 - k) - w / 2.0, w, w), fill: mark, radius: w / 2.0 });
                }
                _ => {
                    line(ctx, (x + s * k, y + s * k), (x + s * (1.0 - k), y + s * (1.0 - k)), w, mark);
                    line(ctx, (x + s * (1.0 - k), y + s * k), (x + s * k, y + s * (1.0 - k)), w, mark);
                }
            }
        }
        CellIcon::Triangle(up) => {
            let (a, b) = if up { (y + s * 0.15, y + s * 0.85) } else { (y + s * 0.85, y + s * 0.15) };
            ctx.add(poly(&[(mx, a), (x + s * 0.95, b), (x + s * 0.05, b)], c, None));
        }
        CellIcon::Diamond => {
            ctx.add(poly(&[(mx, y), (x + s, my), (mx, y + s), (x, my)], c, None));
        }
        CellIcon::Dash => {
            ctx.add(Node::Rect { rect: Rect::new(x + s * 0.15, my - w / 2.0, s * 0.7, w), fill: c, radius: 0.0 });
        }
        CellIcon::Star(n) => {
            let pts: Vec<(f32, f32)> = (0..10)
                .map(|i| {
                    let r = if i % 2 == 0 { s * 0.5 } else { s * 0.21 };
                    let a = -std::f32::consts::FRAC_PI_2 + i as f32 * std::f32::consts::PI / 5.0;
                    (mx + r * a.cos(), my + 0.04 * s + r * a.sin())
                })
                .collect();
            ctx.add(poly(&pts, if n == 2 { c } else { off }, None));
            if n == 1 {
                ctx.add(poly(&pts, c, Some([x - 1.0, y - 1.0, mx, y + s + 1.0])));
            }
        }
        CellIcon::Bars(n) => {
            let bw = s / 4.0 - 1.0;
            for i in 0..4u8 {
                let h = s * (0.25 + 0.25 * i as f32);
                ctx.add(Node::Rect { rect: Rect::new(x + i as f32 * (bw + 1.0) + 0.5, y + s - h, bw, h), fill: if i < n { c } else { off }, radius: 0.0 });
            }
        }
        CellIcon::Quarters(n) => {
            ctx.add(Node::Rect { rect: Rect::new(x, y, s, s), fill: c, radius: s / 2.0 });
            ctx.add(Node::Rect { rect: Rect::new(x + w * 0.6, y + w * 0.6, s - w * 1.2, s - w * 1.2), fill: white, radius: (s - w * 1.2) / 2.0 });
            if n > 0 {
                // A pie from twelve o'clock, clockwise.
                let r = s / 2.0;
                let mut pts = vec![(mx, my)];
                let steps = 6 * n as usize;
                for i in 0..=steps {
                    let a = -std::f32::consts::FRAC_PI_2 + (n as f32 * std::f32::consts::FRAC_PI_2) * i as f32 / steps as f32;
                    pts.push((mx + r * a.cos(), my + r * a.sin()));
                }
                ctx.add(poly(&pts, c, None));
            }
        }
        CellIcon::Boxes(n) => {
            let b = s / 2.0 - 1.0;
            for i in 0..4u8 {
                let (bx, by) = (x + (i % 2) as f32 * (b + 2.0), y + (i / 2) as f32 * (b + 2.0));
                ctx.add(Node::Rect { rect: Rect::new(bx, by, b, b), fill: if i < n { c } else { off }, radius: 1.0 });
            }
        }
    }
}

impl CellView {
    pub fn text(s: impl Into<String>) -> Self {
        Self { text: s.into(), align: CellAlign::Left, color: None, bold: false, numeric: false, fill: None, borders: [None; 4], bar: None, icon: None, indent: 0, note: false }
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
    /// The movable float a click picked (its key), framed with a corner
    /// handle; Delete asks to remove it, Escape lets it go.
    pub picked: Option<u64>,
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
    /// Row `row` dragged to `height` px.
    ResizeRow { row: u32, height: f64 },
    /// A column or row resize let go: the sizes it left are final.
    ResizeEnd,
    Undo,
    Redo,
    Copy(CellRange),
    Cut(CellRange),
    /// Paste at the active cell.
    Paste { row: u32, col: u32 },
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
    Fill { from: CellRange, to: CellRange, copy: bool },
    /// A double-click on the fill handle: fill the selection down as far as
    /// the data beside it goes (the app finds how far).
    FillToData(CellRange),
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
}

impl Geom {
    /// The content point under window point `p`: in the frozen bands, the
    /// unscrolled place; elsewhere, the scrolled one.
    fn content(&self, st: &ScrollState, p: crate::geom::Point) -> (f64, f64) {
        let (dx, dy) = ((p.x - self.body.x()) as f64, (p.y - self.body.y()) as f64);
        (if dx < self.fz[0] { dx } else { st.physics_x.y + dx }, if dy < self.fz[1] { dy } else { st.physics.y + dy })
    }
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
        let header_w = Self::header_width((tiles_r.1 + 1) * TILE_ROWS);
        let body = Rect::new(r.x() + header_w, r.y() + HEADER_H, (r.w() - header_w).max(0.0), (r.h() - HEADER_H).max(0.0));
        let frozen = src.frozen();
        let frozen = (frozen.0.min(rows), frozen.1.min(cols));
        let fz = [src.col_left(frozen.1).min(body.w() as f64 * 0.75), src.row_top(frozen.0).min(body.h() as f64 * 0.75)];
        let geom = Geom { body, rows, cols, ext, frozen, fz };

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
                emit_overlay(ctx, &*src, &t, &gs, vis, &at);
            }
            ctx.pop();
            // Floats (charts) over the cells, those meeting the view.
            let gs = self.state.get_cloned();
            emit_floats(ctx, src.floats([x, y, body.w() as f64, body.h() as f64], &t), &t, &gs, &at, &placed, &buttons);
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
        let drag: Rc<dyn Fn(DragPhase, crate::geom::Point)> = {
            let (src, sc, gst, act) = (src.clone(), self.scroll.clone(), self.state.clone(), self.on_action.clone());
            let placed = placed.clone();
            let buttons = buttons.clone();
            Rc::new(move |phase: DragPhase, p: crate::geom::Point| {
                let st = sc.get();
                let (cx, cy) = geom.content(&st, p);
                if geom.rows == 0 || geom.cols == 0 {
                    return;
                }
                let cell = (src.row_at(cy.max(0.0)), src.col_at(cx.max(0.0)));
                let mut g = gst.get_cloned();
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
                                f(GridAction::Fill { from: sel, to, copy: false });
                            }
                            return;
                        }
                    } else {
                        g.drag = Drag::Fill { to, from };
                    }
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
                    DragPhase::End => g.drag = Drag::None,
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
            Rc::new(move |ev: &Event| {
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
                bands.push((3, Rect::new(body.x(), body.y(), body.w(), fh), (0, frozen.0 - 1), (all_c.0.max(frozen.1), all_c.1), true, false));
            }
            if frozen.1 > 0 && fw > 0.0 && tiles_r.0 <= tiles_r.1 {
                bands.push((4, Rect::new(body.x(), body.y(), fw, body.h()), (all_r.0.max(frozen.0), all_r.1), (0, frozen.1 - 1), false, true));
            }
            if frozen.0 > 0 && frozen.1 > 0 && fw > 0.0 && fh > 0.0 {
                bands.push((5, Rect::new(body.x(), body.y(), fw, fh), (0, frozen.0 - 1), (0, frozen.1 - 1), false, false));
            }
            for (k, rect, rr, cr, across, down) in bands {
                if rr.0 > rr.1 || cr.0 > cr.1 {
                    continue;
                }
                let (ox, oy) = (if across { bx } else { 0.0 }, if down { by } else { 0.0 });
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
                    emit_overlay(ctx, &*src, &t, &gs, CellRange { r0: rr.0, c0: cr.0, r1: rr.1, c1: cr.1 }, &at);
                    ctx.pop();
                    // Floats meeting the band's cells (a chart in the frozen rows
                    // stays with them; the part past the freeze line scrolls in
                    // the body, as in Excel).
                    let (vx, vy) = (if across { x } else { 0.0 }, if down { y } else { 0.0 });
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
              for tc in tiles_c.0..=tiles_c.1 {
                // One group per tile of columns, so a new base moves the
                // group and sends nothing inside it.
                let (c0, c1) = (tc * TILE_COLS, ((tc + 1) * TILE_COLS).min(cols) - 1);
                let gx = q(ch_rect.x() as f64 + (src.col_left(c0) - bx));
                ctx.key(1 << 40 | tc as u64);
                let grp = ctx.push(Node::vstack(Rect::new(gx, ch_rect.y(), (src.col_left(c1 + 1) - src.col_left(c0)) as f32, HEADER_H), 0.0));
                ctx.set_flex(grp, FlexStyle { free: true, ..FlexStyle::default() });
                for c in c0..=c1 {
                    let (xl, xr) = (src.col_left(c), src.col_left(c + 1));
                    let cx = q(ch_rect.x() as f64 + (xl - bx));
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
                    let lw = crate::text::measure_chars(&label, &st);
                    if lw <= w {
                        ctx.add(Node::Text { rect: Rect::new(cx + q(((w - lw) / 2.0) as f64), ch_rect.y() + (HEADER_H - 15.0) / 2.0, lw, 15.0), content: label, style: st });
                    }
                    ctx.add(Node::Rect { rect: Rect::new(cx + w - 1.0, ch_rect.y(), 1.0, HEADER_H), fill: line, radius: 0.0 });
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
                    (DragPhase::End, Drag::Resize { .. }) => {
                        g.drag = Drag::None;
                        gst.set(g);
                        if let Some(a) = &act {
                            a(GridAction::ResizeEnd);
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
        {
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
                    ctx.add(Node::Text { rect: Rect::new(rh_rect.x() + q((header_w - PAD - 2.0 - lw) as f64), cy + q(((h - 15.0) / 2.0).max(0.0) as f64), lw, 15.0), content: label, style: st });
                    ctx.add(Node::Rect { rect: Rect::new(rh_rect.x(), cy + h - 1.0, header_w, 1.0), fill: line, radius: 0.0 });
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
                        commit(&mut g, &act);
                        // Near a row's bottom edge: resize that row.
                        let grip = if (src.row_top(row + 1) - cy).abs() <= RESIZE_GRIP as f64 {
                            Some(row)
                        } else if row > 0 && (cy - src.row_top(row)).abs() <= RESIZE_GRIP as f64 {
                            Some(src.row_at((src.row_top(row) - 0.5).max(0.0)))
                        } else {
                            None
                        };
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
        if fz[0] > 0.0 && frozen.1 > 0 {
            ctx.key(self.key + 6);
            let fr = Rect::new(body.x(), r.y(), fz[0] as f32, HEADER_H);
            let id = ctx.push(Node::Scroll { rect: fr, offset: 0.0, zoom: 1.0, stretch: 1.0, backdrop: Some(t.bg_surface()), content_h: 0.0, offset_x: 0.0, content_w: 0.0 });
            for c in 0..frozen.1 {
                let (xl, xr) = (src.col_left(c), src.col_left(c + 1));
                let (cx, w) = (q(body.x() as f64 + xl), (xr - xl) as f32);
                let on = c >= sel.c0 && c <= sel.c1;
                if on {
                    ctx.add(Node::Rect { rect: Rect::new(cx, r.y(), w, HEADER_H), fill: t.accent_bg(), radius: 0.0 });
                }
                let label = src.col_label(c);
                let st = if on { hstyle_on.clone() } else { hstyle.clone() };
                let lw = crate::text::measure_chars(&label, &st);
                if lw <= w {
                    ctx.add(Node::Text { rect: Rect::new(cx + q(((w - lw) / 2.0) as f64), r.y() + (HEADER_H - 15.0) / 2.0, lw, 15.0), content: label, style: st });
                }
                ctx.add(Node::Rect { rect: Rect::new(cx + w - 1.0, r.y(), 1.0, HEADER_H), fill: line, radius: 0.0 });
            }
            ctx.pop();
            let d = col_drag.clone();
            ctx.on_drag(id, move |ph, p| d(ph, p));
        }
        if fz[1] > 0.0 && frozen.0 > 0 {
            ctx.key(self.key + 7);
            let fr = Rect::new(r.x(), body.y(), header_w, fz[1] as f32);
            let id = ctx.push(Node::Scroll { rect: fr, offset: 0.0, zoom: 1.0, stretch: 1.0, backdrop: Some(t.bg_surface()), content_h: 0.0, offset_x: 0.0, content_w: 0.0 });
            for row in 0..frozen.0 {
                let (yt, yb) = (src.row_top(row), src.row_top(row + 1));
                if yb <= yt {
                    continue;
                }
                let (cy, h) = (q(body.y() as f64 + yt), (yb - yt) as f32);
                let on = row >= sel.r0 && row <= sel.r1;
                if on {
                    ctx.add(Node::Rect { rect: Rect::new(r.x(), cy, header_w, h), fill: t.accent_bg(), radius: 0.0 });
                }
                let label = src.row_label(row);
                let st = if on { hstyle_on.clone() } else { hstyle.clone() };
                let lw = crate::text::measure_chars(&label, &st);
                ctx.add(Node::Text { rect: Rect::new(r.x() + q((header_w - PAD - 2.0 - lw) as f64), cy + q(((h - 15.0) / 2.0).max(0.0) as f64), lw, 15.0), content: label, style: st });
                ctx.add(Node::Rect { rect: Rect::new(r.x(), cy + h - 1.0, header_w, 1.0), fill: line, radius: 0.0 });
            }
            ctx.pop();
            let d = row_drag.clone();
            ctx.on_drag(id, move |ph, p| d(ph, p));
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

fn emit_overlay(ctx: &mut Ctx, src: &dyn GridSource, t: &crate::theme::Semantic, gs: &GridState, vis: CellRange, at: &dyn Fn(f64, f64) -> (f32, f32)) {
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
    let Some(ed) = &gs.edit else {
        ring(ctx, Rect::new(sx, sy, w, h), t.accent_fg());
        return;
    };
    // The editor: as wide as its text needs, from the cell.
    let style = TextStyle { family: font::SANS.into(), size: TEXT_SIZE, weight: Weight::Regular, color: t.text_primary() };
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
}

/// Cells `rr` × `cr` (inclusive): grid lines, contents and the merged areas
/// meeting them, placed by `at`, in groups keyed by tile as the body's.
fn emit_cells(ctx: &mut Ctx, src: &dyn GridSource, t: &crate::theme::Semantic, rr: (u32, u32), cr: (u32, u32), at: &dyn Fn(f64, f64) -> (f32, f32)) {
    let line = t.border_default();
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
                ctx.add(Node::Rect { rect: Rect::new(tx, yb - 1.0, tw, 1.0), fill: line, radius: 0.0 });
            }
            for col in c0..=c1 {
                let (xr, _) = at(src.col_left(col + 1), top);
                ctx.add(Node::Rect { rect: Rect::new(xr - 1.0, ty, 1.0, th), fill: line, radius: 0.0 });
            }
            for row in r0..=r1 {
                let (yt, yb) = (src.row_top(row), src.row_top(row + 1));
                if yb <= yt {
                    continue;
                }
                for col in c0..=c1 {
                    if src.merge_at(row, col).is_some() {
                        continue;
                    }
                    let Some(mut cv) = src.cell(row, col) else { continue };
                    let (xl, xr) = (src.col_left(col), src.col_left(col + 1));
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

/// Columns text may run over beside its cell, at most.
pub const SPILL_COLS: u32 = 32;

fn cell_style(cv: &CellView, t: &crate::theme::Semantic) -> TextStyle {
    TextStyle { family: font::SANS.into(), size: TEXT_SIZE, weight: if cv.bold { Weight::Semibold } else { Weight::Regular }, color: cv.color.unwrap_or(t.text_primary()) }
}

/// Whether cell (`r`, `c`) shows nothing: text may run over it.
fn blank(src: &dyn GridSource, r: u32, c: u32) -> bool {
    src.merge_at(r, c).is_none() && src.cell(r, c).is_none_or(|v| v.text.is_empty())
}

/// The columns cell (`r`, `c`)'s text runs over when it is wider than the
/// cell, as spreadsheets show text: into empty neighbours on the side its
/// alignment points (both, centred), up to [`SPILL_COLS`]. None when it fits,
/// is a number, or has no empty neighbour.
fn spill(src: &dyn GridSource, r: u32, c: u32, cv: &CellView) -> Option<(u32, u32)> {
    if cv.text.is_empty() || cv.text.contains('\n') {
        return None;
    }
    let last = src.cols().saturating_sub(1);
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
    let style = TextStyle { family: font::SANS.into(), size: TEXT_SIZE, weight: if cv.bold { Weight::Semibold } else { Weight::Regular }, color: Color::TRANSPARENT };
    let w = crate::text::measure_chars(&cv.text, &style);
    if w <= room {
        return None;
    }
    let need = (w - room) as f64;
    let (mut a, mut b) = (c, c);
    let (want_l, want_r) = match cv.align {
        CellAlign::Left => (0.0, need),
        CellAlign::Right => (need, 0.0),
        CellAlign::Center | CellAlign::Across => (need / 2.0, need / 2.0),
    };
    let mut got = 0.0;
    while got < want_r && b < last && b - c < SPILL_COLS && blank(src, r, b + 1) {
        b += 1;
        got += src.col_left(b + 1) - src.col_left(b);
    }
    got = 0.0;
    while got < want_l && a > 0 && c - a < SPILL_COLS && blank(src, r, a - 1) {
        a -= 1;
        got += src.col_left(a + 1) - src.col_left(a);
    }
    ((a, b) != (c, c)).then_some((a, b))
}

/// Text that runs over empty neighbours, for rows `rr` and columns `cr` (and
/// cells up to [`SPILL_COLS`] beside them whose text reaches in): the grid
/// lines it crosses hidden, then the text across its columns.
fn emit_spills(ctx: &mut Ctx, src: &dyn GridSource, t: &crate::theme::Semantic, rr: (u32, u32), cr: (u32, u32), mut spills: Vec<(u32, u32, (u32, u32), CellView)>, at: &dyn Fn(f64, f64) -> (f32, f32)) {
    let last = src.cols().saturating_sub(1);
    // Cells beside the columns whose text reaches in: only through a blank
    // edge cell, so a full row costs nothing more.
    for row in rr.0..=rr.1 {
        if src.row_top(row + 1) <= src.row_top(row) {
            continue;
        }
        let left = cr.0 > 0 && blank(src, row, cr.0);
        let right = cr.1 < last && blank(src, row, cr.1);
        let mut side = |cols: &mut dyn Iterator<Item = u32>| {
            for col in cols {
                if src.merge_at(row, col).is_some() {
                    continue;
                }
                let Some(cv) = src.cell(row, col) else { continue };
                if let Some(span) = spill(src, row, col, &cv).filter(|&(a, b)| b >= cr.0 && a <= cr.1) {
                    spills.push((row, col, span, cv));
                }
            }
        };
        if left {
            side(&mut (cr.0.saturating_sub(SPILL_COLS)..cr.0));
        }
        if right {
            side(&mut (cr.1 + 1..=(cr.1 + SPILL_COLS).min(last)));
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
            for k in a..b {
                let filled = |c: u32| c != col && src.cell(row, c).is_some_and(|v| v.fill.is_some());
                if !filled(k) && !filled(k + 1) {
                    let (xr, _) = at(src.col_left(k + 1), yt);
                    ctx.add(Node::Rect { rect: Rect::new(xr - 1.0, y0, 1.0, y1 - y0 - 1.0), fill: cv.fill.filter(|_| k == col || k + 1 == col).unwrap_or(t.bg_canvas()), radius: 0.0 });
                }
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
            let lh = TEXT_SIZE * 1.3;
            ctx.add(Node::Text { rect: Rect::new(tx0, y0 + q(((y1 - y0 - lh) / 2.0).max(0.0) as f64), w, lh), content: shown, style });
        }
    }
    if layer.is_some() {
        ctx.pop();
    }
}

/// One cell's ground, bar, borders, icon and text in the box (`cx`, `cy`,
/// `cw`, `ch`), window px.
fn draw_cell(ctx: &mut Ctx, t: &crate::theme::Semantic, cv: &CellView, cx: f32, cy: f32, cw: f32, ch: f32) {
    if let Some(f) = cv.fill {
        ctx.add(Node::Rect { rect: Rect::new(cx, cy, cw, ch), fill: f, radius: 0.0 });
    }
    if let Some((share, c)) = cv.bar {
        let w = ((cw - 4.0) * share.clamp(0.0, 1.0)).max(0.0);
        ctx.add(Node::Rect { rect: Rect::new(cx + 2.0, cy + 2.0, q(w as f64), (ch - 5.0).max(0.0)), fill: c, radius: 0.0 });
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
    let style = TextStyle { family: font::SANS.into(), size: TEXT_SIZE, weight: if cv.bold { Weight::Semibold } else { Weight::Regular }, color: cv.color.unwrap_or(t.text_primary()) };
    let (shown, w) = fit_text(cv, cw - 2.0 * PAD - lead, &style);
    let tx0 = match cv.align {
        CellAlign::Left => cx + PAD + lead,
        CellAlign::Center | CellAlign::Across => cx + q(((cw - w) / 2.0) as f64),
        CellAlign::Right => cx + q((cw - PAD - w) as f64),
    };
    let lh = TEXT_SIZE * 1.3;
    ctx.add(Node::Text { rect: Rect::new(tx0, cy + q(((ch - lh) / 2.0).max(0.0) as f64), w, lh), content: shown, style });
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
            st.physics.set_position((top - fh).max(0.0));
        } else if bottom > st.physics.y + h {
            st.physics.stop();
            st.physics.set_position((bottom - h).min(top - fh));
        }
    }
    let (left, right) = (src.col_left(c), src.col_left(c + 1));
    if c >= geom.frozen.1 {
        if left < st.physics_x.y + fw {
            st.physics_x.stop();
            st.physics_x.set_position((left - fw).max(0.0));
        } else if right > st.physics_x.y + w {
            st.physics_x.stop();
            st.physics_x.set_position((right - w).min(left - fw));
        }
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
            Some(if cmd { src.data_edge(g.cursor.0, g.cursor.1, dr, dc) } else { past_hidden(src, g.cursor, step(g.cursor, dr as i64, dc as i64), dr, dc) })
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
                    send(GridAction::Fill { from, to, copy: true });
                }
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

    #[test]
    fn every_cell_icon_draws_inside_its_square() {
        struct Icons;
        impl View for Icons {
            fn render(&self, ctx: &mut Ctx) {
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
        /// Pressable floats: key and where.
        buttons: RefCell<Vec<(u64, [f64; 4])>>,
        fills: std::cell::Cell<bool>,
    }

    impl Sheet {
        fn new(rows: u32, cols: u32) -> Self {
            Self { rows, cols, widths: Default::default(), cells: Default::default(), merges: Default::default(), frozen: Default::default(), buttons: Default::default(), fills: Default::default() }
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
                // "»…": centred across the cells after it.
                if let Some(t) = s.strip_prefix('»') {
                    return Some(CellView { align: CellAlign::Across, ..CellView::text(t.to_string()) });
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
        fn fills(&self) -> bool {
            self.fills.get()
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
        assert!(log.borrow().is_empty());
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
        assert_eq!(log.borrow().last(), Some(&GridAction::Fill { from, to: CellRange { r1: 6, ..from }, copy: false }));
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
        key(&mut app, Key::Char, "d", Modifiers { ctrl: true, ..Modifiers::default() });
        let one = CellRange { r0: 3, c0: 3, r1: 3, c1: 3 };
        assert_eq!(log.borrow().last(), Some(&GridAction::Fill { from: one, to: CellRange { r1: 4, ..one }, copy: true }));
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
