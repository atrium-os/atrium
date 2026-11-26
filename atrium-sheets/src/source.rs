//! A sheet as a [`GridSource`]: what `GridView` asks for, read from the document.

use std::cell::RefCell;
use std::rc::Rc;

use officina_grid::eval::dates::DateSystem;
use officina_grid::addr::{CellAddr, MAX_COLS, MAX_ROWS};
use officina_grid::shift::Axis;
use officina_grid::style::{HAlign, Line, StyleId, StyleTable, VAlign};
use officina_grid::Value;
use pergola::Color;
use pergola::theme::palette;
use pergola::widgets::{CellAlign, CellIcon, CellRange, CellVAlign, CellView, GridSource};

use crate::doc::Doc;

/// Rows the grid always offers, used or not (Excel shows all of them; so do we).
pub const MIN_ROWS: u32 = 1 << 20;
/// Columns the grid always offers.
pub const MIN_COLS: u32 = 16384;

/// The sheet on screen.
pub struct SheetSource {
    pub doc: Rc<RefCell<Doc>>,
    /// Charts as drawn, by chart: (sheet, size, data version, nodes).
    charts: RefCell<Vec<Option<(ChartKey, Rc<Vec<pergola::Node>>)>>>,
    /// Row sizes with hidden rows zero high, while the sheet hides any, keyed by what
    /// they were built from.
    view: RefCell<Option<(ViewKey, officina_grid::column::Sizes)>>,
    col_view: RefCell<Option<(ColViewKey, officina_grid::column::Sizes)>>,
    /// The cell whose comment is shown beside it (the cursor's, while not
    /// editing); the window sets it each frame.
    pub note_at: std::cell::Cell<Option<(u32, u32)>>,
}

type ViewKey = (u16, u64, usize, usize, usize, usize, usize, usize, usize, u64);
type ColViewKey = (u16, u64, usize, usize, usize, u64);
type ChartKey = (u16, u32, u32, u64, bool);

impl SheetSource {
    /// The sheet on screen's zoom (1: 100 %): rows, columns and cell text
    /// scale by it; the sheet keeps its sizes at 100 %. 1 while the
    /// document is being changed (callers read it before borrowing).
    pub fn zoom_factor(&self) -> f64 {
        self.doc.try_borrow().map_or(1.0, |d| f64::from(d.wb.sheet(d.sheet).zoom) / 100.0)
    }

    pub fn new(doc: Rc<RefCell<Doc>>) -> Self {
        Self { doc, view: RefCell::new(None), col_view: RefCell::new(None), charts: RefCell::new(Vec::new()), note_at: std::cell::Cell::new(None) }
    }

    /// `f` over the sheet's row sizes as laid out (hidden rows zero high).
    fn rows_view<T>(&self, f: impl FnOnce(&officina_grid::column::Sizes) -> T) -> T {
        let d = self.doc.borrow();
        let sh = d.wb.sheet(d.sheet);
        let outline = sh.outline(Axis::Rows).hidden();
        if sh.hidden.is_empty() && sh.table_hidden().is_empty() && outline.is_empty() {
            return f(&sh.row_sizes);
        }
        let o = sh.row_sizes.overrides();
        let key = (d.sheet, d.edits, outline.as_ptr() as usize, sh.hidden.as_ptr() as usize, sh.hidden.len(), sh.table_hidden().as_ptr() as usize, sh.table_hidden().len(), o.as_ptr() as usize, o.len(), sh.row_sizes.default.to_bits());
        let mut v = self.view.borrow_mut();
        if v.as_ref().is_none_or(|(k, _)| *k != key) {
            *v = Some((key, sh.view_row_sizes()));
        }
        f(&v.as_ref().unwrap().1)
    }
    fn cols_view<T>(&self, f: impl FnOnce(&officina_grid::column::Sizes) -> T) -> T {
        let d = self.doc.borrow();
        let sh = d.wb.sheet(d.sheet);
        let outline = sh.outline(Axis::Cols).hidden();
        if outline.is_empty() {
            return f(&sh.col_sizes);
        }
        let o = sh.col_sizes.overrides();
        let key = (d.sheet, d.edits, outline.as_ptr() as usize, o.as_ptr() as usize, o.len(), sh.col_sizes.default.to_bits());
        let mut v = self.col_view.borrow_mut();
        if v.as_ref().is_none_or(|(k, _)| *k != key) {
            *v = Some((key, sh.view_col_sizes()));
        }
        f(&v.as_ref().unwrap().1)
    }

    fn used(&self) -> (u32, u32) {
        let d = self.doc.borrow();
        d.wb.extent(d.sheet)
    }
    fn filled(&self, r: u32, c: u32) -> bool {
        let d = self.doc.borrow();
        !d.wb.value(d.sheet, r, c).is_empty()
    }
}

/// How a value shows in a cell (General format).
pub fn view_of(v: &Value) -> Option<CellView> {
    // At the bottom of a taller row, as spreadsheets show cells.
    view_of_centred(v).map(|cv| CellView { valign: CellVAlign::Bottom, ..cv })
}

fn view_of_centred(v: &Value) -> Option<CellView> {
    Some(match v {
        Value::Empty => return None,
        Value::Num(_) => CellView::number(v.display()),
        Value::Str(s) => CellView::text(s.to_string()),
        Value::Bool(_) => CellView { align: CellAlign::Center, ..CellView::text(v.display()) },
        Value::Err(_) => CellView { align: CellAlign::Center, color: Some(palette::danger_500()), ..CellView::text(v.display()) },
    })
}

pub fn rgb(c: u32) -> Color {
    Color::from_srgb8((c >> 16) as u8, (c >> 8) as u8, c as u8)
}

/// How a value shows in a cell with a style: through its number format, in its
/// font's colour and weight, its fill and borders, aligned as it says.
fn rotation_face() -> pergola::glyphs::Face {
    use std::sync::OnceLock;
    static FACE: OnceLock<pergola::glyphs::Face> = OnceLock::new();
    FACE.get_or_init(|| {
        pergola::glyphs::Face::from_static(include_bytes!("../../fonts/ibm-plex/IBMPlexSans.ttf"), 0)
    }).clone()
}

/// The grid's italic, underline and strikethrough bits for `f`.
pub fn font_marks(f: &officina_grid::style::Font) -> u8 {
    use pergola::inline::Emphasis;
    (if f.italic { Emphasis::Italic as u8 } else { 0 }) | (if f.underline { Emphasis::Underline as u8 } else { 0 }) | (if f.strike { Emphasis::Strike as u8 } else { 0 })
}

/// Whether a font named `name` is a monospace one, drawn in the monospace
/// face (the others in the sans face): Courier, Consolas, Menlo and the
/// like, and any name saying Mono or Code.
pub fn is_mono_font(name: &str) -> bool {
    const MONO: [&str; 9] = ["courier", "consolas", "menlo", "monaco", "lucida console", "inconsolata", "mono", "code", "fixedsys"];
    let n = name.to_ascii_lowercase();
    MONO.iter().any(|m| n.contains(m))
}

/// The text size `f` asks for, px, when not the default 11 pt (the grid's
/// own size); 6 to 72 pt.
pub fn font_px(f: &officina_grid::style::Font) -> Option<f32> {
    f.size.filter(|&s| s != 220 && s > 0).map(|s| pergola::theme::type_size::SM * (s.clamp(120, 1440) as f32) / 220.0)
}

fn styled_view(t: &StyleTable, id: StyleId, v: &Value, system: DateSystem) -> Option<CellView> {
    let st = t.get(id);
    // An empty cell centring across shows, so the run can reach over it.
    let has_look = st.fill.is_some() || st.pattern.is_some() || st.gradient.is_some()
        || st.borders.iter().chain(&st.diagonals).any(|b| b.line != Line::None) || st.h_align == HAlign::CenterAcross;
    let mut cv = match v {
        Value::Empty if !has_look => return None,
        Value::Empty => CellView::text(""),
        Value::Num(_) | Value::Str(_) => {
            let f = t.format(id).format_in(v, system);
            let fill_at = f.fill.filter(|_| !f.hashes);
            let text = if f.hashes { "#".repeat(8) } else { f.text };
            let mut cv = if matches!(v, Value::Num(_)) { CellView::number(text) } else { CellView::text(text) };
            cv.fill_at = fill_at;
            if let Some(c) = f.color {
                cv.color = Some(rgb(c.rgb()));
            }
            cv
        }
        _ => view_of(v)?,
    };
    if cv.color.is_none() {
        cv.color = st.font.color.map(rgb);
    }
    cv.bold = st.font.bold;
    cv.marks = font_marks(&st.font);
    cv.size = font_px(&st.font);
    cv.mono = st.font.name.as_deref().is_some_and(is_mono_font);
    if st.indent > 0 && matches!(st.h_align, officina_grid::style::HAlign::Left | officina_grid::style::HAlign::General) {
        cv.indent = st.indent;
    }
    cv.fill = st.fill.map(rgb);
    cv.pattern = st.pattern.map(|p| (p.name(), rgb(p.foreground.unwrap_or(0)), rgb(p.background.unwrap_or(0xFFFFFF))));
    cv.gradient = st.gradient.as_ref().map(|g| (g.angle, g.stops.clone()));
    cv.rotation = st.rotation;
    cv.shrink_to_fit = st.shrink_to_fit;
    if matches!(st.rotation, -90..=-1 | 1..=90) && !cv.text.is_empty() {
        cv.rotation_face = Some(rotation_face());
    }
    for (target, edge) in cv.diagonals.iter_mut().zip(st.diagonals) {
        if edge.line != Line::None {
            *target = Some((edge.line.width(), rgb(edge.color.unwrap_or(0))));
        }
    }
    for (k, e) in st.borders.iter().enumerate() {
        if e.line != Line::None {
            cv.borders[k] = Some((e.line.width(), rgb(e.color.unwrap_or(0))));
        }
    }
    cv.align = match st.h_align {
        HAlign::General => cv.align,
        HAlign::Left | HAlign::Fill | HAlign::Justify | HAlign::Distributed => CellAlign::Left,
        HAlign::Center => CellAlign::Center,
        HAlign::CenterAcross => CellAlign::Across,
        HAlign::Right => CellAlign::Right,
    };
    cv.wrap = st.wrap;
    cv.valign = match st.v_align {
        VAlign::Top => CellVAlign::Top,
        VAlign::Center => CellVAlign::Center,
        VAlign::Bottom | VAlign::Justify | VAlign::Distributed => CellVAlign::Bottom,
    };
    Some(cv)
}

/// A pivot table's cell: header and total rows bold on a light band, page fields
/// bold, as Excel's default pivot style shows them.
pub fn pivot_view(cv: Option<CellView>, role: officina_grid::pivot::Role) -> Option<CellView> {
    use officina_grid::pivot::Role;
    let band = rgb(0xDDEBF7);
    match role {
        Role::Header | Role::Total => {
            let mut cv = cv.unwrap_or_else(|| CellView::text(""));
            cv.bold = true;
            cv.fill = cv.fill.or(Some(band));
            if role == Role::Total {
                cv.borders[2] = Some((1.0, rgb(0x9BC2E6)));
            }
            Some(cv)
        }
        Role::Page => cv.map(|mut cv| {
            cv.bold = true;
            cv
        }),
        Role::Label | Role::Data => cv,
    }
}

/// What the grid draws for sparkline `sp`.
pub fn spark_view(wb: &officina_grid::Workbook, sp: &officina_grid::sparkline::Sparkline) -> std::sync::Arc<pergola::widgets::Spark> {
    use officina_grid::sparkline::SparkKind;
    use pergola::widgets::SparkStyle;
    std::sync::Arc::new(pergola::widgets::Spark {
        style: match sp.kind {
            SparkKind::Line => SparkStyle::Line,
            SparkKind::Column => SparkStyle::Column,
            SparkKind::WinLoss => SparkStyle::WinLoss,
        },
        points: sp.points(wb),
        color: rgb(sp.color),
        negative: rgb(sp.negative),
        markers: sp.markers,
    })
}

/// A cell's view under its conditional formats: their fill, colour scale, font
/// colour, weight and number format, and the data bar under the text.
fn cond_view(cv: Option<CellView>, look: &officina_grid::condfmt::Look, v: &Value, system: DateSystem) -> Option<CellView> {
    let d = &look.dxf;
    if look.scale.is_none() && look.bar.is_none() && look.icon.is_none() && !look.hide_value && *d == officina_grid::condfmt::Dxf::default() {
        return cv;
    }
    let mut cv = cv.unwrap_or_else(|| CellView::text(""));
    if let (Some(code), Value::Num(_)) = (&d.num_fmt, v) {
        let f = officina_grid::numfmt::NumFmt::parse(code).format_in(v, system);
        cv.fill_at = f.fill.filter(|_| !f.hashes);
        cv.text = if f.hashes { "#".repeat(8) } else { f.text };
        if let Some(c) = f.color {
            cv.color = Some(rgb(c.rgb()));
        }
    }
    if let Some(c) = look.scale.or(d.fill) {
        cv.fill = Some(rgb(c));
        cv.pattern = None;
        cv.gradient = None;
    }
    if let Some(c) = d.color {
        cv.color = Some(rgb(c));
    }
    if let Some(b) = d.bold {
        cv.bold = b;
    }
    cv.bar = look.bar.map(|(share, c)| (share, rgb(c)));
    cv.icon = look.icon.and_then(|(set, k)| icon_of(set, k));
    if look.hide_value {
        cv.text.clear();
    }
    Some(cv)
}

/// Icon `k` (0 the lowest) of icon set `set` ([`officina_grid::condfmt::ICON_SETS`]),
/// as Excel draws it.
pub fn icon_of(set: u8, k: u8) -> Option<(CellIcon, Color)> {
    const RED: u32 = 0xE0393E;
    const AMBER: u32 = 0xF4C431;
    const GREEN: u32 = 0x3BAA5C;
    const GRAY: u32 = 0x8C8C8C;
    const BLUE: u32 = 0x4A7EBB;
    const GOLD: u32 = 0xF2B705;
    let name = officina_grid::condfmt::ICON_SETS.get(set as usize)?.0;
    let k = k as usize;
    let pick = |v: &[(CellIcon, u32)]| v.get(k).map(|(i, c)| (*i, rgb(*c)));
    use CellIcon::*;
    match name {
        "3Arrows" => pick(&[(Arrow(-2), RED), (Arrow(0), AMBER), (Arrow(2), GREEN)]),
        "3ArrowsGray" => pick(&[(Arrow(-2), GRAY), (Arrow(0), GRAY), (Arrow(2), GRAY)]),
        "3Flags" => pick(&[(Flag, RED), (Flag, AMBER), (Flag, GREEN)]),
        "3TrafficLights1" | "3TrafficLights2" => pick(&[(Dot, RED), (Dot, AMBER), (Dot, GREEN)]),
        "3Signs" => pick(&[(Diamond, RED), (Triangle(true), AMBER), (Dot, GREEN)]),
        "3Symbols" => pick(&[(Cross(true), RED), (Exclaim(true), AMBER), (Check(true), GREEN)]),
        "3Symbols2" => pick(&[(Cross(false), RED), (Exclaim(false), AMBER), (Check(false), GREEN)]),
        "3Stars" => pick(&[(Star(0), GOLD), (Star(1), GOLD), (Star(2), GOLD)]),
        "3Triangles" => pick(&[(Triangle(false), RED), (Dash, AMBER), (Triangle(true), GREEN)]),
        "4Arrows" => pick(&[(Arrow(-2), RED), (Arrow(-1), AMBER), (Arrow(1), AMBER), (Arrow(2), GREEN)]),
        "4ArrowsGray" => pick(&[(Arrow(-2), GRAY), (Arrow(-1), GRAY), (Arrow(1), GRAY), (Arrow(2), GRAY)]),
        "4RedToBlack" => pick(&[(Dot, 0x262626), (Dot, GRAY), (Dot, 0xF4A6A8), (Dot, RED)]),
        "4Rating" => pick(&[(Bars(1), BLUE), (Bars(2), BLUE), (Bars(3), BLUE), (Bars(4), BLUE)]),
        "4TrafficLights" => pick(&[(Dot, 0x262626), (Dot, RED), (Dot, AMBER), (Dot, GREEN)]),
        "5Arrows" => pick(&[(Arrow(-2), RED), (Arrow(-1), AMBER), (Arrow(0), AMBER), (Arrow(1), AMBER), (Arrow(2), GREEN)]),
        "5ArrowsGray" => pick(&[(Arrow(-2), GRAY), (Arrow(-1), GRAY), (Arrow(0), GRAY), (Arrow(1), GRAY), (Arrow(2), GRAY)]),
        "5Rating" => pick(&[(Bars(0), BLUE), (Bars(1), BLUE), (Bars(2), BLUE), (Bars(3), BLUE), (Bars(4), BLUE)]),
        "5Quarters" => pick(&[(Quarters(0), 0x404040), (Quarters(1), 0x404040), (Quarters(2), 0x404040), (Quarters(3), 0x404040), (Quarters(4), 0x404040)]),
        "5Boxes" => pick(&[(Boxes(0), BLUE), (Boxes(1), BLUE), (Boxes(2), BLUE), (Boxes(3), BLUE), (Boxes(4), BLUE)]),
        _ => None,
    }
}

/// sRGB components to Pergola's linear colour.
fn linear(c: [f32; 4]) -> Color {
    let l = |v: f32| if v <= 0.040_45 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) };
    Color::rgba(l(c[0]), l(c[1]), l(c[2]), c[3])
}

/// A chart's shapes as Pergola nodes at the chart's origin.
fn chart_nodes(prims: Vec<officina_draw::chart::Prim>, theme: &pergola::theme::Semantic) -> Vec<pergola::Node> {
    use officina_draw::chart::{Align, Prim};
    use pergola::shape::{Paint, PathData, Shape, Stroke};
    use pergola::theme::font;
    let fills = |sh: Shape| sh.fills().into_iter().map(|f| pergola::Node::Fill(std::sync::Arc::new(f))).collect::<Vec<_>>();
    let poly = |pts: &[(f32, f32)], close: bool| {
        let mut p = PathData::new();
        for (i, (x, y)) in pts.iter().enumerate() {
            p = if i == 0 { p.move_to(*x, *y) } else { p.line_to(*x, *y) };
        }
        if close {
            p = p.close();
        }
        p
    };
    let mut out = Vec::new();
    for p in prims {
        match p {
            Prim::Rect { x, y, w, h, color } => out.push(pergola::Node::Rect { rect: pergola::Rect::new(x, y, w, h), fill: linear(color), radius: 0.0 }),
            Prim::Line { pts, width, color } => {
                let mut st = Stroke::new(Paint::Solid(linear(color)), width);
                st.join = pergola::shape::Join::Round;
                out.extend(fills(Shape::new(poly(&pts, false)).stroke(st)));
            }
            Prim::Area { pts, color } => out.extend(fills(Shape::new(poly(&pts, true)).fill(Paint::Solid(linear([color[0], color[1], color[2], 0.85]))))),
            Prim::Dot { x, y, r, color } => out.extend(fills(Shape::new(PathData::ellipse(pergola::Point::new(x, y), r, r)).fill(Paint::Solid(linear(color))))),
            Prim::Wedge { cx, cy, r_in, r_out, a0, a1, color } => {
                // Arcs as segments of at most 3 degrees.
                let n = (((a1 - a0) / 0.05).ceil() as usize).clamp(1, 128);
                let at = |r: f32, a: f32| (cx + r * a.sin(), cy - r * a.cos());
                let mut pts: Vec<(f32, f32)> = (0..=n).map(|k| at(r_out, a0 + (a1 - a0) * k as f32 / n as f32)).collect();
                if r_in > 0.0 {
                    pts.extend((0..=n).rev().map(|k| at(r_in, a0 + (a1 - a0) * k as f32 / n as f32)));
                } else {
                    pts.push((cx, cy));
                }
                let bg = theme.bg_canvas();
                let mut st = Stroke::new(Paint::Solid(bg), 1.0);
                st.join = pergola::shape::Join::Round;
                out.extend(fills(Shape::new(poly(&pts, true)).fill(Paint::Solid(linear(color))).stroke(st)));
            }
            Prim::Text { x, y, text, size, color, align, bold } => {
                let style = pergola::TextStyle { family: font::SANS.into(), size, weight: if bold { pergola::theme::Weight::Semibold } else { pergola::theme::Weight::Regular }, color: linear(color) };
                let w = pergola::text::measure(&text, &style).w;
                let x = match align {
                    Align::Start => x,
                    Align::Middle => x - w / 2.0,
                    Align::End => x - w,
                };
                out.push(pergola::Node::Text { rect: pergola::Rect::new(x, y, w, size * 1.3), content: text, style });
            }
        }
    }
    out
}

/// Float keys: charts use their index; these sit above any chart count.
/// Float keys: a filter column's button (with its column), a table's
/// frame, the cursor's list button.
pub const KEY_FILTER: u64 = 1 << 61;
const KEY_TABLE: u64 = 1 << 60;
pub const KEY_LIST: u64 = 1 << 59;

/// Whether `a` is in a table's total row.
pub fn total_cell(wb: &officina_grid::Workbook, a: CellAddr) -> bool {
    wb.table_at(a.sheet, a.row, a.col).is_some_and(|t| t.totals && a.row == t.rect.r1)
}
/// The fill options button by the area a fill just filled.
pub const KEY_FILL: u64 = 1 << 57;
/// Its size, px.
const FILL_BUTTON: f64 = 16.0;

/// A small square button with a down-pointing triangle, `size` px.
fn drop_button(theme: &pergola::theme::Semantic, size: f32, lit: bool) -> Vec<pergola::Node> {
    use pergola::shape::{Paint, PathData, Shape};
    let bg = if lit { theme.accent_fill() } else { theme.bg_surface() };
    let fg = if lit { theme.accent_fg() } else { theme.text_secondary() };
    let mut nodes = vec![
        pergola::Node::Rect { rect: pergola::Rect::new(0.0, 0.0, size, size), fill: theme.border_default(), radius: 3.0 },
        pergola::Node::Rect { rect: pergola::Rect::new(1.0, 1.0, size - 2.0, size - 2.0), fill: bg, radius: 2.0 },
    ];
    let (cx, cy, k) = (size / 2.0, size / 2.0 + 1.0, size * 0.22);
    let tri = PathData::new().move_to(cx - k * 1.3, cy - k * 0.7).line_to(cx + k * 1.3, cy - k * 0.7).line_to(cx, cy + k * 0.8).close();
    nodes.extend(Shape::new(tri).fill(Paint::Solid(fg)).fills().into_iter().map(|f| pergola::Node::Fill(std::sync::Arc::new(f))));
    nodes
}

impl SheetSource {
    /// Tables (an outline, a header tint, stripes) and the auto-filter's header marks
    /// (a filtered column's mark is lit), for the part of the sheet in `view`.
    fn data_marks(&self, view: [f64; 4], theme: &pergola::theme::Semantic) -> Vec<pergola::widgets::GridFloat> {
        let d = self.doc.borrow();
        let s = d.sheet;
        let mut out = Vec::new();
        let (vx0, vy0, vx1, vy1) = (view[0], view[1], view[0] + view[2], view[1] + view[3]);
        let col_x = |c: u32| self.col_left(c);
        for (i, t) in d.wb.tables().iter().enumerate() {
            let r = t.rect;
            if r.sheet != s {
                continue;
            }
            let (x0, x1) = (col_x(r.c0), col_x(r.c1 + 1));
            let (y0, y1) = (self.row_top(r.r0), self.row_top(r.r1 + 1));
            if x0 > vx1 || x1 < vx0 || y0 > vy1 || y1 < vy0 {
                continue;
            }
            // Only the visible part, so a million-row table costs a screenful.
            let (fx0, fy0) = (x0.max(vx0 - 2.0), y0.max(vy0 - 2.0));
            let (fx1, fy1) = (x1.min(vx1 + 2.0), y1.min(vy1 + 2.0));
            let (w, h) = ((fx1 - fx0) as f32, (fy1 - fy0) as f32);
            let at = |x: f64, y: f64, ww: f64, hh: f64, fill: Color| pergola::Node::Rect { rect: pergola::Rect::new((x - fx0) as f32, (y - fy0) as f32, ww as f32, hh as f32), fill, radius: 0.0 };
            let mut nodes = Vec::new();
            let accent = theme.accent_fill();
            let first_data = r.r0 + t.header as u32;
            if t.stripes {
                let a = self.row_at(fy0).max(first_data);
                let b = self.row_at(fy1).min(r.r1);
                for row in (a..=b).filter(|row| (row - first_data) % 2 == 1) {
                    let (ya, yb) = (self.row_top(row), self.row_top(row + 1));
                    if yb > ya {
                        nodes.push(at(fx0, ya, fx1 - fx0, yb - ya, accent.with_alpha(0.07)));
                    }
                }
            }
            if t.header {
                let hb = self.row_top(r.r0 + 1);
                nodes.push(at(fx0, y0, fx1 - fx0, hb - y0, accent.with_alpha(0.16)));
                nodes.push(at(fx0, hb - 1.0, fx1 - fx0, 2.0, accent));
            }
            let edge = accent.with_alpha(0.8);
            if y0 >= fy0 {
                nodes.push(at(fx0, y0 - 1.0, fx1 - fx0, 2.0, edge));
            }
            if y1 <= fy1 {
                nodes.push(at(fx0, y1 - 1.0, fx1 - fx0, 2.0, edge));
            }
            if x0 >= fx0 {
                nodes.push(at(x0 - 1.0, fy0, 2.0, fy1 - fy0, edge));
            }
            if x1 <= fx1 {
                nodes.push(at(x1 - 1.0, fy0, 2.0, fy1 - fy0, edge));
            }
            out.push(pergola::widgets::GridFloat { key: KEY_TABLE | i as u64, at: [fx0, fy0, w as f64, h as f64], nodes: Rc::new(nodes), movable: false, pressable: false });
        }
        let sheet_filter = d.wb.sheet(s).filter.as_ref().map(|f| (f.rect, f.conds.as_slice(), None));
        let table_filters = d.wb.tables().iter().filter(|t| t.rect.sheet == s && t.header)
            .filter_map(|t| Some((t.filter_rect()?, t.filter.as_ref()?.conds.as_slice(), Some(t.rect.c0))));
        for (r, conds, offset) in sheet_filter.into_iter().chain(table_filters) {
            let (y0, y1) = (self.row_top(r.r0), self.row_top(r.r0 + 1));
            if y1 > y0 && y0 <= vy1 && y1 >= vy0 {
                let a = self.col_at(vx0).max(r.c0);
                let b = self.col_at(vx1).min(r.c1);
                for c in a..=b {
                    // The table's button owns a shared header cell.
                    if offset.is_none() && d.wb.table_at(s, r.r0, c).is_some_and(|t| t.rect.r0 == r.r0 && t.header && t.filter.is_some()) {
                        continue;
                    }
                    let lit = conds.iter().any(|(k, cond)| match cond {
                        officina_grid::filter::Cond::Expression(expr) => expr.uses_column(c - r.c0),
                        _ => *k + offset.unwrap_or(0) == c,
                    });
                    let size = 14.0f32.min((y1 - y0) as f32 - 2.0).max(6.0);
                    let x = col_x(c + 1) - size as f64 - 3.0;
                    let y = y0 + ((y1 - y0) - size as f64) / 2.0;
                    // A press opens the column's value list.
                    out.push(pergola::widgets::GridFloat { key: KEY_FILTER | (u64::from(r.r0) << 32) | u64::from(c), at: [x, y, size as f64, size as f64], nodes: Rc::new(drop_button(theme, size, lit)), movable: false, pressable: true });
                }
            }
        }
        out
    }
}

impl SheetSource {
    /// The cell anchor of a box at `at` (content px) on the current sheet:
    /// the cells its corners fall in and the offsets into them, points.
    pub fn anchor_of(&self, at: [f64; 4]) -> officina_grid::charts::Anchor {
        let sheet = self.doc.borrow().sheet;
        let corner = |x: f64, y: f64| {
            let (r, c) = (self.row_at(y.max(0.0)), self.col_at(x.max(0.0)));
            let z = self.zoom_factor();
            (r, c, ((x - self.col_left(c)) / z * 72.0 / 96.0).max(0.0) as f32, ((y - self.row_top(r)) / z * 72.0 / 96.0).max(0.0) as f32)
        };
        officina_grid::charts::Anchor { sheet, from: corner(at[0], at[1]), to: corner(at[0] + at[2], at[1] + at[3]) }
    }
}

/// The comment box's width, px, and its most lines.
const NOTE_W: f32 = 220.0;
const NOTE_LINES: usize = 10;

/// `text` broken into lines at most `w` wide (words longer than a line are cut
/// by characters), at most `max` of them (the last ending in "…" when cut).
pub fn wrap_lines(text: &str, style: &pergola::TextStyle, w: f32, max: usize) -> Vec<String> {
    let fits = |s: &str| pergola::text::measure(s, style).w <= w;
    let mut out: Vec<String> = Vec::new();
    'paras: for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split(' ') {
            let tried = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
            if fits(&tried) {
                line = tried;
                continue;
            }
            if !line.is_empty() {
                out.push(std::mem::take(&mut line));
            }
            // A word wider than a line goes in pieces.
            let mut piece = String::new();
            for ch in word.chars() {
                piece.push(ch);
                if !fits(&piece) && piece.chars().count() > 1 {
                    piece.pop();
                    out.push(std::mem::replace(&mut piece, ch.to_string()));
                }
            }
            line = piece;
            if out.len() > max {
                break 'paras;
            }
        }
        out.push(line);
        if out.len() > max {
            break;
        }
    }
    if out.len() > max {
        out.truncate(max);
        if let Some(l) = out.last_mut() {
            l.push('…');
        }
    }
    out
}

/// Invalid cells ringed in the view, at most.
pub const MAX_RINGS: usize = 2000;
/// The most comments Show All Comments draws at once.
pub const MAX_SHOWN_NOTES: usize = 200;

impl SheetSource {
    /// A red ring around each cell in `view` whose value its validation rule
    /// doesn't allow, while [`Doc::circling`] is on.
    fn invalid_rings(&self, view: [f64; 4]) -> Option<pergola::widgets::GridFloat> {
        use pergola::shape::{Paint, PathData, Shape, Stroke};
        if !self.doc.borrow().circling {
            return None;
        }
        let area = {
            let d = self.doc.borrow();
            officina_grid::Rect { sheet: d.sheet, r0: self.row_at(view[1].max(0.0)), c0: self.col_at(view[0].max(0.0)), r1: self.row_at(view[1].max(0.0) + view[3]), c1: self.col_at(view[0].max(0.0) + view[2]) }
        };
        let cells = self.doc.borrow().wb.invalid_cells(&area, MAX_RINGS, 1 << 18);
        if cells.is_empty() {
            return None;
        }
        let (ox, oy) = (self.col_left(area.c0), self.row_top(area.r0));
        let (w, h) = (self.col_left(area.c1 + 1) - ox, self.row_top(area.r1 + 1) - oy);
        let red = Color::rgba(0.85, 0.1, 0.1, 1.0);
        let mut nodes = Vec::new();
        for a in cells {
            let (x0, x1) = (self.col_left(a.col) - ox, self.col_left(a.col + 1) - ox);
            let (y0, y1) = (self.row_top(a.row) - oy, self.row_top(a.row + 1) - oy);
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            let c = pergola::geom::Point::new(((x0 + x1) / 2.0) as f32, ((y0 + y1) / 2.0) as f32);
            let ring = Shape::new(PathData::ellipse(c, ((x1 - x0) / 2.0 + 2.0) as f32, ((y1 - y0) / 2.0 + 1.0) as f32)).stroke(Stroke::new(Paint::Solid(red), 1.5));
            nodes.extend(ring.fills().into_iter().map(|f| pergola::Node::Fill(std::sync::Arc::new(f))));
        }
        Some(pergola::widgets::GridFloat { key: 1 << 58, at: [ox, oy, w, h], nodes: Rc::new(nodes), movable: false, pressable: false })
    }

    /// Trace Precedents and Dependents' arrows over the sheet on screen:
    /// blue, red from an error, and black from or to another sheet (drawn
    /// to a small sheet mark beside the cell); a box round cells read
    /// together and a dot where each arrow starts.
    fn trace_arrows(&self, view: [f64; 4]) -> Option<pergola::widgets::GridFloat> {
        use pergola::shape::{Paint, PathData, Shape, Stroke};
        let (arrows, sheet) = {
            let d = self.doc.borrow();
            (d.arrows.clone(), d.sheet)
        };
        let center = |r: u32, c: u32| ((self.col_left(c) + self.col_left(c + 1)) / 2.0, (self.row_top(r) + self.row_top(r + 1)) / 2.0);
        // (from, to, colour, a box round the source).
        let mut lines: Vec<((f64, f64), (f64, f64), Color, Option<[f64; 4]>, bool)> = Vec::new();
        for a in &arrows {
            let here_to = a.to.sheet == sheet;
            let here_from = a.from.sheet == sheet;
            if !here_to && !here_from {
                continue;
            }
            let colour = if !(here_to && here_from) {
                Color::rgba(0.1, 0.1, 0.1, 1.0)
            } else if a.error {
                Color::rgba(0.85, 0.1, 0.1, 1.0)
            } else {
                Color::rgba(0.1, 0.32, 0.85, 1.0)
            };
            let to = if here_to { center(a.to.row, a.to.col) } else { let (x, y) = center(a.from.r0, a.from.c0); (x + 60.0, y - 30.0) };
            let (from, boxed) = if here_from {
                let f = a.from;
                let boxed = (f.r0 != f.r1 || f.c0 != f.c1).then(|| [self.col_left(f.c0), self.row_top(f.r0), self.col_left(f.c1 + 1), self.row_top(f.r1 + 1)]);
                (center(f.r0, f.c0), boxed)
            } else {
                ((to.0 - 60.0, to.1 - 30.0), None)
            };
            lines.push((from, to, colour, boxed, !(here_to && here_from)));
        }
        if lines.is_empty() {
            return None;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
        for (f, t, _, b, _) in &lines {
            for (x, y) in [*f, *t].into_iter().chain(b.iter().flat_map(|b| [(b[0], b[1]), (b[2], b[3])])) {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
        let (x0, y0) = ((x0 - 10.0).max(0.0), (y0 - 10.0).max(0.0));
        let (w, h) = ((x1 + 10.0 - x0).clamp(1.0, 20_000.0), (y1 + 10.0 - y0).clamp(1.0, 20_000.0));
        if x0 > view[0] + view[2] || y0 > view[1] + view[3] || x0 + w < view[0] || y0 + h < view[1] {
            return None;
        }
        let p = |(x, y): (f64, f64)| ((x - x0) as f32, (y - y0) as f32);
        let mut nodes = Vec::new();
        let mut add = |shape: Shape| nodes.extend(shape.fills().into_iter().map(|f| pergola::Node::Fill(std::sync::Arc::new(f))));
        for (f, t, colour, boxed, other) in lines {
            let (fx, fy) = p(f);
            let (tx, ty) = p(t);
            if let Some(b) = boxed {
                let (bx0, by0) = p((b[0], b[1]));
                let (bx1, by1) = p((b[2], b[3]));
                add(Shape::new(PathData::rect(pergola::Rect::new(bx0, by0, bx1 - bx0, by1 - by0))).stroke(Stroke::new(Paint::Solid(colour), 1.5)));
            }
            if other {
                // The other sheet's mark: a small sheet with a fold.
                let (mx, my) = if boxed.is_none() && (fx, fy) != (tx, ty) { (fx, fy) } else { (tx, ty) };
                add(Shape::new(PathData::rect(pergola::Rect::new(mx - 8.0, my - 6.0, 16.0, 12.0))).fill(Paint::Solid(Color::rgba(1.0, 1.0, 1.0, 1.0))).stroke(Stroke::new(Paint::Solid(colour), 1.0)));
                add(Shape::new(PathData::new().move_to(mx - 5.0, my - 2.0).line_to(mx + 5.0, my - 2.0).move_to(mx - 5.0, my + 2.0).line_to(mx + 5.0, my + 2.0)).stroke(Stroke::new(Paint::Solid(colour), 1.0)));
            }
            add(Shape::new(PathData::ellipse(pergola::geom::Point::new(fx, fy), 2.5, 2.5)).fill(Paint::Solid(colour)));
            let (dx, dy) = (tx - fx, ty - fy);
            let len = (dx * dx + dy * dy).sqrt();
            if len < 1.0 {
                continue;
            }
            let (ux, uy) = (dx / len, dy / len);
            let (bx, by) = (tx - 9.0 * ux, ty - 9.0 * uy);
            add(Shape::new(PathData::new().move_to(fx, fy).line_to(bx, by)).stroke(Stroke::new(Paint::Solid(colour), 1.5)));
            add(Shape::new(PathData::new().move_to(tx, ty).line_to(bx - 4.0 * uy, by + 4.0 * ux).line_to(bx + 4.0 * uy, by - 4.0 * ux).close()).fill(Paint::Solid(colour)));
        }
        Some(pergola::widgets::GridFloat { key: (1 << 56) | arrows.len() as u64, at: [x0, y0, w, h], nodes: Rc::new(nodes), movable: false, pressable: false })
    }

    /// A button at the right of [`Self::note_at`]'s cell when a validation
    /// rule offers it a list, or it is a table's total row cell (a press
    /// offers the totals); a press opens the list.
    fn list_button(&self, theme: &pergola::theme::Semantic) -> Option<pergola::widgets::GridFloat> {
        let (r, c) = self.note_at.get()?;
        let (r0, r1, c1) = {
            let d = self.doc.borrow();
            let a = CellAddr::new(d.sheet, r, c);
            let list = d.wb.validation_at(a).is_some_and(|v| v.allow == officina_grid::validation::Allow::List && v.dropdown);
            if !list && !total_cell(&d.wb, a) {
                return None;
            }
            d.wb.merge_at(a).map_or((r, r, c), |m| (m.r0, m.r1, m.c1))
        };
        let (y0, y1) = (self.row_top(r0), self.row_top(r1 + 1));
        if y1 <= y0 {
            return None;
        }
        let size = 16.0f32.min((y1 - y0) as f32 - 2.0).max(6.0);
        let x = self.col_left(c1 + 1) - size as f64 - 2.0;
        let y = y0 + ((y1 - y0) - size as f64) / 2.0;
        Some(pergola::widgets::GridFloat { key: KEY_LIST, at: [x, y, size as f64, size as f64], nodes: Rc::new(drop_button(theme, size, false)), movable: false, pressable: true })
    }

    /// Below the right of the area the last fill filled, while it can still
    /// be done again another way: a press offers the ways (Excel's Auto
    /// Fill Options).
    fn fill_button(&self, theme: &pergola::theme::Semantic) -> Option<pergola::widgets::GridFloat> {
        let to = {
            let d = self.doc.borrow();
            if !d.can_refill() {
                return None;
            }
            d.last_fill?.1
        };
        let at = self.fill_button_at(to)?;
        Some(pergola::widgets::GridFloat { key: KEY_FILL, at, nodes: Rc::new(drop_button(theme, FILL_BUTTON as f32, false)), movable: false, pressable: true })
    }

    /// Where the fill options button for a fill of `to` sits (content px).
    pub fn fill_button_at(&self, to: officina_grid::addr::Rect) -> Option<[f64; 4]> {
        let (x, y) = (self.col_left(to.c1 + 1), self.row_top(to.r1 + 1));
        (x > self.col_left(to.c0) && y > self.row_top(to.r0)).then_some([x + 1.0, y + 1.0, FILL_BUTTON, FILL_BUTTON])
    }

    /// The comment of [`Self::note_at`]'s cell as a box beside the cell.
    fn note_float(&self) -> Option<pergola::widgets::GridFloat> {
        let (r, c) = self.note_at.get()?;
        self.note_box(r, c, 1 << 62, false)
    }

    /// Show All Comments: each comment in `view` in its box beside its cell
    /// (the one under the pointer is drawn by [`Self::note_float`]).
    fn all_notes(&self, view: [f64; 4]) -> Vec<pergola::widgets::GridFloat> {
        let cells: Vec<(u32, u32)> = {
            let d = self.doc.borrow();
            if !d.show_comments {
                return Vec::new();
            }
            let (r0, c0) = (self.row_at(view[1].max(0.0)), self.col_at(view[0].max(0.0)));
            let (r1, c1) = (self.row_at(view[1].max(0.0) + view[3]), self.col_at(view[0].max(0.0) + view[2]));
            d.wb.comments(d.sheet).keys().copied().filter(|&(r, c)| (r0..=r1).contains(&r) && (c0.saturating_sub(4)..=c1).contains(&c)).take(MAX_SHOWN_NOTES).collect()
        };
        let hovered = self.note_at.get();
        cells.into_iter().filter(|&at| Some(at) != hovered).filter_map(|(r, c)| {
            let key = (1u64 << 57) | (u64::from(r) << 20) | u64::from(c & 0xF_FFFF);
            self.note_box(r, c, key, true)
        }).collect()
    }

    /// The box beside (`r`, `c`): its comment and, unless `comments_only`,
    /// its link and its validation prompt.
    fn note_box(&self, r: u32, c: u32, key: u64, comments_only: bool) -> Option<pergola::widgets::GridFloat> {
        let (comment, link, prompt) = {
            let d = self.doc.borrow();
            let k = d.wb.comment(d.sheet, r, c).cloned();
            let l = d.wb.link_at(CellAddr::new(d.sheet, r, c)).map(|l| l.target.to_string());
            // A validation rule's input prompt, and whether it offers a list.
            let p = d.wb.validation_at(CellAddr::new(d.sheet, r, c)).and_then(|v| {
                let list = v.allow == officina_grid::validation::Allow::List && v.dropdown;
                let msg = (v.show_prompt && !v.prompt.is_empty()).then(|| (v.prompt.title.to_string(), v.prompt.text.to_string()));
                (list || msg.is_some()).then_some((msg, list))
            });
            if comments_only { (k, None, None) } else { (k, l, p) }
        };
        if comment.is_none() && link.is_none() && prompt.is_none() {
            return None;
        }
        let ink = Color::rgba(0.11, 0.11, 0.09, 1.0);
        // 0 plain, 1 bold, 2 the link, 3 the hint under it.
        let st = |kind: u8| pergola::TextStyle {
            family: pergola::theme::font::SANS.into(),
            size: if kind == 3 { 11.0 } else { 12.0 },
            weight: if kind == 1 { pergola::theme::Weight::Semibold } else { pergola::theme::Weight::Regular },
            color: match kind {
                2 => palette::info_600(),
                3 => Color::rgba(0.4, 0.38, 0.3, 1.0),
                _ => ink,
            },
        };
        let pad = 8.0;
        let lh = 16.0;
        let mut lines: Vec<(String, u8)> = Vec::new();
        let rich_lines = comment.as_ref().map(|k| crate::comment::wrap(k, &st(0), NOTE_W - 2.0 * pad, NOTE_LINES)).unwrap_or_default();
        let author = comment.as_ref().map(|k| k.author.as_ref()).filter(|name| !name.is_empty());
        let comment_rows = usize::from(author.is_some()) + rich_lines.len();
        if let Some((msg, list)) = &prompt {
            if let Some((title, text)) = msg {
                if !title.trim().is_empty() {
                    lines.push((title.trim().to_string(), 1));
                }
                lines.extend(wrap_lines(text, &st(0), NOTE_W - 2.0 * pad, NOTE_LINES).into_iter().map(|l| (l, 0)));
            }
            if *list {
                lines.push(("Data ▸ Choose from List…".into(), 3));
            }
        }
        let linked = link.is_some();
        if let Some(t) = link {
            let shown = t.strip_prefix('#').map_or_else(|| t.clone(), |p| format!("Go to {p}"));
            lines.extend(wrap_lines(&shown, &st(2), NOTE_W - 2.0 * pad, 3).into_iter().map(|l| (l, 2)));
            lines.push(("Ctrl+click to open".into(), 3));
        }
        let h = 2.0 * pad + lh * (comment_rows + lines.len()) as f32;
        // A comment's pale yellow; a link alone, a plain white box.
        let (edge, fill) = if comment.is_some() || lines.iter().any(|l| l.1 < 2) || !linked {
            (Color::rgba(0.55, 0.5, 0.3, 1.0), Color::rgba(1.0, 0.97, 0.78, 1.0))
        } else {
            (Color::rgba(0.62, 0.64, 0.68, 1.0), Color::rgba(1.0, 1.0, 1.0, 1.0))
        };
        let mut nodes = vec![
            pergola::Node::Rect { rect: pergola::Rect::new(0.0, 0.0, NOTE_W, h), fill: edge, radius: 3.0 },
            pergola::Node::Rect { rect: pergola::Rect::new(1.0, 1.0, NOTE_W - 2.0, h - 2.0), fill, radius: 2.0 },
        ];
        if let Some(author) = author {
            let style = st(1);
            let w = pergola::text::measure(author, &style).w.min(NOTE_W - 2.0 * pad);
            nodes.push(pergola::Node::Text { rect: pergola::Rect::new(pad, pad, w, lh), content: author.into(), style });
        }
        for (i, line) in rich_lines.into_iter().enumerate() {
            let mut x = pad;
            let y = pad + (i + usize::from(author.is_some())) as f32 * lh;
            for (content, marks) in line {
                let content = crate::comment::shown(content);
                let style = pergola::inline::style(&st(0), marks);
                let w = pergola::text::measure(&content, &style).w.min((NOTE_W - pad - x).max(0.0));
                let rect = pergola::Rect::new(x, y, w, lh);
                nodes.extend(pergola::inline::rules(rect, &style, marks));
                nodes.push(pergola::Node::Text { rect, content: content.into_owned(), style });
                x += w;
            }
        }
        for (i, (l, kind)) in lines.into_iter().enumerate() {
            let w = pergola::text::measure(&l, &st(kind)).w.min(NOTE_W - 2.0 * pad);
            nodes.push(pergola::Node::Text { rect: pergola::Rect::new(pad, pad + (comment_rows + i) as f32 * lh, w, lh), content: l, style: st(kind) });
        }
        // Beside the cell (or its merged area), level with its top.
        let (c1, r0) = {
            let d = self.doc.borrow();
            d.wb.merge_at(CellAddr::new(d.sheet, r, c)).map_or((c, r), |m| (m.c1, m.r0))
        };
        let x = self.col_left(c1 + 1) + 6.0;
        let y = self.row_top(r0);
        Some(pergola::widgets::GridFloat { key, at: [x, y, NOTE_W as f64, h as f64], nodes: std::rc::Rc::new(nodes), movable: false, pressable: false })
    }
}

impl SheetSource {
    /// The width column `col` needs to show its widest entry whole, px
    /// (shown rows; cells merged across columns left out; at most
    /// [`FIT_CELLS`] cells read). `None`: nothing in it.
    pub fn fit_width(&self, col: u32) -> Option<f64> {
        let rows: Vec<u32> = {
            let d = self.doc.borrow();
            let (er, _) = d.wb.extent(d.sheet);
            let mut rows = Vec::new();
            if er > 0 {
                d.wb.each(&officina_grid::Rect { sheet: d.sheet, r0: 0, c0: col, r1: er - 1, c1: col }, &mut |r, _, v| {
                    if rows.len() < FIT_CELLS && !v.is_empty() {
                        rows.push(r);
                    }
                });
            }
            let sh = d.wb.sheet(d.sheet);
            rows.retain(|&r| !sh.row_hidden(r) && d.wb.merge_at(CellAddr::new(d.sheet, r, col)).is_none_or(|m| m.c0 == m.c1));
            rows
        };
        let w = rows.into_iter().filter_map(|r| self.cell_at_100(r, col)).map(|cv| pergola::widgets::grid_view::needed_width(&cv)).fold(0.0, f64::max);
        (w > 0.0).then_some(w.max(pergola::widgets::grid_view::MIN_COL_W))
    }
}

impl SheetSource {
    /// The height row `row` needs for what it shows: its tallest cell's
    /// lines (wrapped to their column's width), at least the default
    /// height. Hidden columns and cells merged across rows don't count.
    pub fn fit_height(&self, row: u32) -> f64 {
        let cols: Vec<(u32, f64)> = {
            let d = self.doc.borrow();
            let (_, ec) = d.wb.extent(d.sheet);
            let sh = d.wb.sheet(d.sheet);
            let mut cols = Vec::new();
            if ec > 0 {
                d.wb.each(&officina_grid::Rect { sheet: d.sheet, r0: row, c0: 0, r1: row, c1: ec - 1 }, &mut |_, c, v| {
                    if cols.len() < FIT_CELLS && !v.is_empty() {
                        cols.push((c, sh.col_sizes.size(c)));
                    }
                });
            }
            cols.retain(|&(c, w)| w > 0.0 && d.wb.merge_at(CellAddr::new(d.sheet, row, c)).is_none_or(|m| m.r0 == m.r1));
            cols
        };
        let default = { let d = self.doc.borrow(); d.wb.sheet(d.sheet).row_sizes.default };
        let h = cols.into_iter().filter_map(|(c, w)| self.cell_at_100(row, c).map(|cv| pergola::widgets::grid_view::needed_height(&cv, w))).fold(0.0, f64::max);
        h.max(default)
    }
}

impl SheetSource {
    /// How cell (`r`, `c`) shows at 100 % (fitting a column or row measures this).
    pub fn cell_at_100(&self, r: u32, c: u32) -> Option<CellView> {
        let d = self.doc.borrow();
        if d.wb.hides_value(CellAddr::new(d.sheet, r, c)) {
            return None;
        }
        if d.show_formulas {
            let a = CellAddr::new(d.sheet, r, c);
            if let Some(f) = d.wb.formula_text(a).filter(|_| !d.wb.hides_formula(a)) {
                return Some(CellView::text(f));
            }
        }
        let v = d.wb.value(d.sheet, r, c);
        let sh = d.wb.sheet(d.sheet);
        let id = sh.style_id(r, c);
        let mut cv = if id == 0 { view_of(&v) } else { styled_view(&d.wb.styles, id, &v, d.wb.date_system()) };
        if !d.wb.pivots().is_empty() {
            let a = CellAddr::new(d.sheet, r, c);
            if let Some((i, role)) = d.wb.pivot_at(a) {
                cv = pivot_view(cv, role);
                // Compact form: inner items indented, outer items bold.
                let (indent, group) = d.wb.pivot_shape(i).map_or((0, false), |s| s.indent(a));
                if let Some(v) = cv.as_mut() {
                    v.indent = indent;
                    v.bold |= group;
                }
            }
        }
        let mut cv = if d.wb.cond_formats().is_empty() { cv } else { cond_view(cv, &d.wb.look(CellAddr::new(d.sheet, r, c)), &v, d.wb.date_system()) };
        // A link reads as one: blue, unless the cell has a colour of its own.
        if let Some(v) = cv.as_mut().filter(|v| v.color.is_none() && !v.text.is_empty()) {
            if d.wb.link_at(CellAddr::new(d.sheet, r, c)).is_some() {
                v.color = Some(palette::info_600());
            }
        }
        if d.wb.comment(d.sheet, r, c).is_some() {
            let mut v = cv.unwrap_or_else(|| CellView::text(""));
            v.note = true;
            cv = Some(v);
        }
        // Still to be recalculated: the old value, greyed (§8.4).
        if d.wb.is_stale(d.sheet, r, c) {
            if let Some(v) = cv.as_mut() {
                v.color = Some(palette::neutral_400());
            }
        }
        cv
    }
}

/// The local date and time now: year, month, day, hour, minute.
pub(crate) fn local_now() -> Option<(i32, u32, u32, u32, u32)> {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as libc::time_t;
    // SAFETY: localtime_r writes only into `tm`, which outlives the call.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return None;
    }
    Some((tm.tm_year + 1900, tm.tm_mon as u32 + 1, tm.tm_mday as u32, tm.tm_hour as u32, tm.tm_min as u32))
}

/// The most cells one column fit reads.
pub const FIT_CELLS: usize = 200_000;

impl GridSource for SheetSource {
    fn fills(&self) -> bool {
        true
    }
    fn complete(&self, r: u32, c: u32, prefix: &str) -> Option<String> {
        let d = self.doc.borrow();
        if prefix.starts_with('=') {
            return crate::fnhelp::complete_formula(prefix, &crate::data::paste_names(&d));
        }
        crate::data::complete_entry(&crate::data::column_entries(&d.wb, d.sheet, r, c), prefix)
    }
    fn now_text(&self, time: bool) -> Option<String> {
        let (y, mo, d, h, mi) = local_now()?;
        Some(if time { format!("{h:02}:{mi:02}") } else { format!("{y:04}-{mo:02}-{d:02}") })
    }
    fn frozen(&self) -> (u32, u32) {
        let d = self.doc.borrow();
        d.wb.sheet(d.sheet).freeze
    }
    fn frozen_from(&self) -> (u32, u32) {
        let d = self.doc.borrow();
        d.wb.sheet(d.sheet).freeze_from
    }
    fn merge_at(&self, r: u32, c: u32) -> Option<CellRange> {
        let d = self.doc.borrow();
        d.wb.merge_at(CellAddr::new(d.sheet, r, c)).map(|m| CellRange { r0: m.r0, c0: m.c0, r1: m.r1, c1: m.c1 })
    }
    fn merges_in(&self, g: CellRange) -> Vec<CellRange> {
        let d = self.doc.borrow();
        let rect = officina_grid::addr::Rect { sheet: d.sheet, r0: g.r0, c0: g.c0, r1: g.r1, c1: g.c1 };
        d.wb.merges_in(&rect).map(|m| CellRange { r0: m.r0, c0: m.c0, r1: m.r1, c1: m.c1 }).collect()
    }
    fn floats(&self, view: [f64; 4], theme: &pergola::theme::Semantic) -> Vec<pergola::widgets::GridFloat> {
        let mut out = self.data_marks(view, theme);
        out.extend(self.all_notes(view));
        out.extend(self.note_float());
        out.extend(self.trace_arrows(view));
        out.extend(self.list_button(theme));
        out.extend(self.fill_button(theme));
        out.extend(self.invalid_rings(view));
        let d = self.doc.borrow();
        let charts = d.wb.charts();
        if charts.is_empty() {
            return out;
        }
        let dark = theme.mode == pergola::theme::Mode::Dark;
        let mut cache = self.charts.borrow_mut();
        cache.resize(charts.len(), None);
        for (i, ch) in charts.iter().enumerate() {
            let a = ch.anchor;
            if a.sheet != d.sheet {
                continue;
            }
            // Offsets are points; the grid is in px.
            let z = self.zoom_factor();
            let x0 = self.col_left(a.from.1) + a.from.2 as f64 * 96.0 / 72.0 * z;
            let y0 = self.row_top(a.from.0) + a.from.3 as f64 * 96.0 / 72.0 * z;
            let x1 = self.col_left(a.to.1) + a.to.2 as f64 * 96.0 / 72.0 * z;
            let y1 = self.row_top(a.to.0) + a.to.3 as f64 * 96.0 / 72.0 * z;
            let (w, h) = ((x1 - x0).clamp(0.0, 8000.0), (y1 - y0).clamp(0.0, 8000.0));
            if w < 4.0 || h < 4.0 || x0 > view[0] + view[2] || y0 > view[1] + view[3] || x0 + w < view[0] || y0 + h < view[1] {
                continue;
            }
            let key = (d.sheet, w as u32, h as u32, ch.data_version(&d.wb), dark);
            let nodes = match &cache[i] {
                Some((k, n)) if *k == key => n.clone(),
                _ => {
                    // At most four points per pixel column are drawn; read no more
                    // than a million.
                    let data = ch.data(&d.wb, 1_000_000);
                    let title = ch.title.as_ref().map(|t| officina_grid::charts::Chart::text(&d.wb, t));
                    let mut st = officina_draw::chart::Style::default();
                    if dark {
                        st.background = [0.13, 0.14, 0.15, 1.0];
                        st.text = [0.91, 0.92, 0.93, 1.0];
                        st.muted = [0.65, 0.67, 0.70, 1.0];
                        st.grid = [0.25, 0.26, 0.28, 1.0];
                        st.axis = [0.45, 0.46, 0.48, 1.0];
                    }
                    let measure = |t: &str, size: f32| {
                        let style = pergola::TextStyle { family: pergola::theme::font::SANS.into(), size, weight: pergola::theme::Weight::Regular, color: Color::TRANSPARENT };
                        pergola::text::measure_chars(t, &style)
                    };
                    let prims = officina_draw::chart::layout(ch, &data, title.as_deref(), w as f32, h as f32, &st, &measure);
                    let mut nodes = chart_nodes(prims, theme);
                    // A frame round it.
                    nodes.insert(0, pergola::Node::Rect { rect: pergola::Rect::new(-1.0, -1.0, w as f32 + 2.0, h as f32 + 2.0), fill: theme.border_default(), radius: 0.0 });
                    let n = Rc::new(nodes);
                    cache[i] = Some((key, n.clone()));
                    n
                }
            };
            out.push(pergola::widgets::GridFloat { key: i as u64, at: [x0, y0, w, h], nodes, movable: true, pressable: false });
        }
        out
    }

    fn rows(&self) -> u32 {
        let (r, _) = self.used();
        let d = self.doc.borrow();
        let end = d.wb.sheet(d.sheet).outline(Axis::Rows).last().unwrap_or(0);
        MIN_ROWS.max(r.max(end).saturating_add(1000)).min(MAX_ROWS)
    }
    fn cols(&self) -> u32 {
        let (_, c) = self.used();
        let d = self.doc.borrow();
        let end = d.wb.sheet(d.sheet).outline(Axis::Cols).last().unwrap_or(0);
        MIN_COLS.max(c.max(end).saturating_add(26)).min(MAX_COLS)
    }
    fn row_top(&self, r: u32) -> f64 {
        self.rows_view(|s| s.offset(r)) * self.zoom_factor()
    }
    fn row_at(&self, y: f64) -> u32 {
        let rows = self.rows();
        let z = self.zoom_factor();
        self.rows_view(|s| s.index_at((y / z).max(0.0))).min(rows - 1)
    }
    fn col_left(&self, c: u32) -> f64 {
        self.cols_view(|s| s.offset(c)) * self.zoom_factor()
    }
    fn col_at(&self, x: f64) -> u32 {
        let z = self.zoom_factor();
        self.cols_view(|s| s.index_at((x / z).max(0.0))).min(self.cols() - 1)
    }
    fn zoom(&self) -> f32 {
        self.zoom_factor() as f32
    }
    fn show_grid(&self) -> bool {
        self.doc.try_borrow().map_or(true, |d| d.wb.sheet(d.sheet).show_grid)
    }
    fn show_headings(&self) -> bool {
        self.doc.try_borrow().map_or(true, |d| d.wb.sheet(d.sheet).show_headings)
    }
    fn outline_control(&self, rows: bool, at: u32) -> Option<(u32, u32, bool)> {
        let d = self.doc.borrow();
        let outline = d.wb.sheet(d.sheet).outline(if rows { Axis::Rows } else { Axis::Cols });
        if outline.groups().is_empty() { return None; }
        let limit = if rows { self.rows() } else { self.cols() };
        let control = |sizes: &officina_grid::column::Sizes| {
            outline.groups().iter().filter_map(|g| {
                let pos = if g.collapsed {
                    let origin = sizes.offset(g.start);
                    let right = sizes.index_at(origin);
                    if right < limit && sizes.size(right) >= if rows { 14.0 } else { 30.0 } { right }
                    else if origin >= 0.01 { sizes.index_at(origin - 0.01) }
                    else { return None; }
                } else {
                    let first = sizes.index_at(sizes.offset(g.start));
                    if first > g.end { return None; }
                    first
                };
                (pos == at).then_some((g.start, g.end, g.collapsed))
            }).max_by_key(|&(start, end, collapsed)| (collapsed, end - start))
        };
        if rows { self.rows_view(control) } else { self.cols_view(control) }
    }
    fn cell(&self, r: u32, c: u32) -> Option<CellView> {
        let z = self.zoom_factor() as f32;
        let spark = {
            let d = self.doc.borrow();
            let sh = d.wb.sheet(d.sheet);
            if sh.sparklines().is_empty() { None } else { sh.sparkline_at(r, c).map(|sp| spark_view(&d.wb, sp)) }
        };
        let mut cv = match (self.cell_at_100(r, c), spark) {
            (Some(cv), None) => cv,
            (cv, Some(sp)) => CellView { spark: Some(sp), ..cv.unwrap_or_else(|| CellView::text("")) },
            (None, None) => return None,
        };
        if z != 1.0 {
            cv.size = Some(cv.size.unwrap_or(pergola::theme::type_size::SM) * z);
        }
        Some(cv)
    }
    fn edit_text(&self, r: u32, c: u32) -> String {
        let d = self.doc.borrow();
        let a = CellAddr::new(d.sheet, r, c);
        if d.wb.hides_value(a) || (d.wb.hides_formula(a) && d.wb.group_at(a.sheet, a.row, a.col).is_some()) {
            return String::new();
        }
        match d.wb.formula_text(a) {
            Some(f) => f,
            None => d.wb.get(a).display(),
        }
    }
    /// Excel's Ctrl + arrow: from a filled cell with a filled neighbour, to the end of
    /// the run; otherwise to the next filled cell, or the edge.
    fn data_edge(&self, r: u32, c: u32, dr: i32, dc: i32) -> (u32, u32) {
        let (used_r, used_c) = self.used();
        let (rows, cols) = (self.rows() as i64, self.cols() as i64);
        let step = |r: i64, c: i64| (r + dr.signum() as i64, c + dc.signum() as i64);
        let inside = |r: i64, c: i64| r >= 0 && c >= 0 && r < rows && c < cols;
        let (mut r, mut c) = (r as i64, c as i64);
        let (nr, nc) = step(r, c);
        if !inside(nr, nc) {
            return (r as u32, c as u32);
        }
        let here = self.filled(r as u32, c as u32);
        if here && self.filled(nr as u32, nc as u32) {
            // To the end of this run.
            loop {
                let (nr, nc) = step(r, c);
                if !inside(nr, nc) || !self.filled(nr as u32, nc as u32) {
                    return (r as u32, c as u32);
                }
                (r, c) = (nr, nc);
            }
        }
        // To the next filled cell; past the used area there is none.
        loop {
            let (nr, nc) = step(r, c);
            if !inside(nr, nc) {
                return (r as u32, c as u32);
            }
            (r, c) = (nr, nc);
            if (dr > 0 && r >= used_r as i64) || (dc > 0 && c >= used_c as i64) {
                let r = if dr > 0 { rows - 1 } else { r };
                let c = if dc > 0 { cols - 1 } else { c };
                return (r as u32, c as u32);
            }
            if self.filled(r as u32, c as u32) {
                return (r as u32, c as u32);
            }
        }
    }
    fn last_used(&self) -> (u32, u32) {
        let (r, c) = self.used();
        (r.saturating_sub(1), c.saturating_sub(1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conditional_formats_show_over_the_cell() {
        use officina_grid::condfmt::{Dxf, Look};
        let look = Look { dxf: Dxf { fill: Some(0xFFC7CE), color: Some(0x9C0006), bold: Some(true), num_fmt: Some("0.0%".into()), ..Dxf::default() }, scale: None, bar: Some((0.5, 0x638EC6)), ..Look::default() };
        let v = Value::Num(0.25);
        let cv = cond_view(view_of(&v), &look, &v, DateSystem::Excel1900).unwrap();
        assert_eq!(cv.text, "25.0%");
        assert_eq!((cv.fill, cv.color, cv.bold), (Some(rgb(0xFFC7CE)), Some(rgb(0x9C0006)), true));
        assert_eq!(cv.bar, Some((0.5, rgb(0x638EC6))));
        // A blank cell a rule fills; a colour scale over the fill.
        let look = Look { dxf: Dxf { fill: Some(1), ..Dxf::default() }, scale: Some(0x63BE7B), ..Look::default() };
        assert_eq!(cond_view(None, &look, &Value::Empty, DateSystem::Excel1900).unwrap().fill, Some(rgb(0x63BE7B)));
        assert!(cond_view(None, &Look::default(), &Value::Empty, DateSystem::Excel1900).is_none());
    }

    #[test]
    fn styled_cells_show_their_format_and_look() {
        use officina_grid::style::{Edge, Style};
        let mut t = StyleTable::new();
        let mut st = Style::default().with_num_fmt("#,##0.00;[Red]-#,##0.00");
        st.font.bold = true;
        st.fill = Some(0xFFFF00);
        st.borders[3] = Edge { line: Line::Thick, color: None };
        let id = t.intern(st);
        let cv = styled_view(&t, id, &Value::Num(-1234.5), DateSystem::Excel1900).unwrap();
        assert_eq!(cv.text, "-1,234.50");
        assert!(cv.bold && cv.numeric && cv.align == CellAlign::Right);
        assert_eq!(cv.color, Some(rgb(0xFF0000)));
        assert_eq!(cv.fill, Some(rgb(0xFFFF00)));
        assert_eq!(cv.borders[3].map(|b| b.0), Some(3.0));
        // An empty cell with a fill still draws; an empty plain one does not.
        assert!(styled_view(&t, id, &Value::Empty, DateSystem::Excel1900).is_some_and(|c| c.text.is_empty()));
        let d = t.intern(Style::default().with_num_fmt("yyyy-mm-dd"));
        assert!(styled_view(&t, d, &Value::Empty, DateSystem::Excel1900).is_none());
        assert_eq!(styled_view(&t, d, &Value::Num(46340.0), DateSystem::Excel1900).unwrap().text, "2026-11-14");
        assert_eq!(styled_view(&t, d, &Value::Num(-1.0), DateSystem::Excel1900).unwrap().text, "########");
    }

    #[test]
    fn a_column_fits_its_widest_shown_entry() {
        let s = src(&[(0, 0, "short"), (1, 0, "a much longer entry than that"), (2, 0, "x")]);
        let wide = s.fit_width(0).unwrap();
        assert!(wide > 64.0, "{wide}");
        assert_eq!(s.fit_width(3), None);
        // Hidden, the long one no longer counts.
        s.doc.borrow_mut().wb.sheet_mut(0).hidden = vec![(1, 1)];
        assert!(s.fit_width(0).unwrap() < wide);
    }

    #[test]
    fn ctrl_semicolon_gives_today_and_the_time_as_typed() {
        let s = src(&[]);
        let day = s.now_text(false).unwrap();
        assert!(crate::doc::typed_date(&day).is_some_and(|d| d.1 == "yyyy-mm-dd"), "{day}");
        let time = s.now_text(true).unwrap();
        assert!(crate::doc::typed_date(&time).is_some_and(|d| d.1 == "hh:mm"), "{time}");
    }

    #[test]
    fn a_row_fits_its_wrapped_text() {
        let s = src(&[(0, 0, "short"), (0, 1, "one two three four five six seven eight nine ten")]);
        let default = s.doc.borrow().wb.sheet(0).row_sizes.default;
        assert_eq!(s.fit_height(0), default);
        assert_eq!(s.fit_height(5), default);
        // Wrapped, the long one takes several lines.
        let mut d = s.doc.borrow_mut();
        let r = officina_grid::Rect { sheet: 0, r0: 0, c0: 1, r1: 0, c1: 1 };
        let op = officina_grid::ops::GridOp::restyle(&d.wb, r, |st| officina_grid::style::Style { wrap: true, ..st.clone() });
        d.run("Wrap Text", vec![op]);
        drop(d);
        assert!(s.cell(0, 1).unwrap().wrap);
        assert!(s.fit_height(0) > 2.0 * default, "{}", s.fit_height(0));
    }

    fn src(cells: &[(u32, u32, &str)]) -> SheetSource {
        let mut d = Doc::new("t");
        for &(r, c, s) in cells {
            d.type_into(r, c, s);
        }
        SheetSource::new(Rc::new(RefCell::new(d)))
    }

    #[test]
    fn ctrl_arrows_follow_the_data() {
        let s = src(&[(0, 0, "1"), (1, 0, "2"), (2, 0, "3"), (10, 0, "x")]);
        assert_eq!(s.data_edge(0, 0, 1, 0), (2, 0), "to the end of the run");
        assert_eq!(s.data_edge(2, 0, 1, 0), (10, 0), "to the next filled cell");
        assert_eq!(s.data_edge(10, 0, 1, 0), (s.rows() - 1, 0), "past the data, the edge");
        assert_eq!(s.data_edge(10, 0, -1, 0), (2, 0));
        assert_eq!(s.data_edge(0, 0, 0, 1), (0, s.cols() - 1));
        assert_eq!(s.last_used(), (10, 0));
    }

    #[test]
    fn cells_show_their_values_and_edit_their_formulas() {
        let s = src(&[(0, 0, "2"), (0, 1, "=A1*3"), (0, 2, "=1/0"), (0, 3, "TRUE")]);
        assert_eq!(s.cell(0, 1).unwrap().text, "6");
        assert!(s.cell(0, 1).unwrap().numeric);
        assert_eq!(s.edit_text(0, 1), "=A1*3");
        assert_eq!(s.cell(0, 2).unwrap().text, "#DIV/0!");
        assert_eq!(s.cell(0, 3).unwrap().align, CellAlign::Center);
        assert!(s.cell(5, 5).is_none());
        assert_eq!(s.rows(), MIN_ROWS);
    }

    #[test]
    fn every_icon_set_draws_each_of_its_icons() {
        use officina_grid::condfmt::{Look, ICON_SETS};
        for (set, (name, n)) in ICON_SETS.iter().enumerate() {
            for k in 0..*n {
                assert!(icon_of(set as u8, k).is_some(), "{name} {k}");
            }
            assert!(icon_of(set as u8, *n).is_none(), "{name} has {n}");
        }
        // Arrows run from red down to green up; icon only clears the text.
        assert_eq!(icon_of(0, 0).unwrap().0, CellIcon::Arrow(-2));
        assert_eq!(icon_of(0, 2).unwrap().0, CellIcon::Arrow(2));
        let v = officina_grid::Value::Num(5.0);
        let look = Look { icon: Some((0, 1)), ..Look::default() };
        let cv = cond_view(Some(CellView::number("5")), &look, &v, DateSystem::Excel1900).unwrap();
        assert_eq!((cv.text.as_str(), cv.icon.map(|i| i.0)), ("5", Some(CellIcon::Arrow(0))));
        let cv = cond_view(Some(CellView::number("5")), &Look { hide_value: true, ..look }, &v, DateSystem::Excel1900).unwrap();
        assert_eq!(cv.text, "");
    }
}