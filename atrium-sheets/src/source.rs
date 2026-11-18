//! A sheet as a [`GridSource`]: what `GridView` asks for, read from the document.

use std::cell::RefCell;
use std::rc::Rc;

use officina_grid::addr::{CellAddr, MAX_COLS, MAX_ROWS};
use officina_grid::style::{HAlign, Line, StyleId, StyleTable};
use officina_grid::Value;
use pergola::Color;
use pergola::theme::palette;
use pergola::widgets::{CellAlign, CellIcon, CellRange, CellView, GridSource};

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
    /// The cell whose comment is shown beside it (the cursor's, while not
    /// editing); the window sets it each frame.
    pub note_at: std::cell::Cell<Option<(u32, u32)>>,
}

type ViewKey = (u16, usize, usize, usize, usize, u64);
type ChartKey = (u16, u32, u32, u64, bool);

impl SheetSource {
    pub fn new(doc: Rc<RefCell<Doc>>) -> Self {
        Self { doc, view: RefCell::new(None), charts: RefCell::new(Vec::new()), note_at: std::cell::Cell::new(None) }
    }

    /// `f` over the sheet's row sizes as laid out (hidden rows zero high).
    fn rows_view<T>(&self, f: impl FnOnce(&officina_grid::column::Sizes) -> T) -> T {
        let d = self.doc.borrow();
        let sh = d.wb.sheet(d.sheet);
        if sh.hidden.is_empty() {
            return f(&sh.row_sizes);
        }
        let o = sh.row_sizes.overrides();
        let key = (d.sheet, sh.hidden.as_ptr() as usize, sh.hidden.len(), o.as_ptr() as usize, o.len(), sh.row_sizes.default.to_bits());
        let mut v = self.view.borrow_mut();
        if v.as_ref().is_none_or(|(k, _)| *k != key) {
            *v = Some((key, sh.view_row_sizes()));
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
pub fn styled_view(t: &StyleTable, id: StyleId, v: &Value) -> Option<CellView> {
    let st = t.get(id);
    let has_look = st.fill.is_some() || st.borders.iter().any(|b| b.line != Line::None);
    let mut cv = match v {
        Value::Empty if !has_look => return None,
        Value::Empty => CellView::text(""),
        Value::Num(_) | Value::Str(_) => {
            let f = t.format(id).format(v);
            let text = if f.hashes { "#".repeat(8) } else { f.text };
            let mut cv = if matches!(v, Value::Num(_)) { CellView::number(text) } else { CellView::text(text) };
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
    cv.fill = st.fill.map(rgb);
    for (k, e) in st.borders.iter().enumerate() {
        if e.line != Line::None {
            cv.borders[k] = Some((e.line.width(), rgb(e.color.unwrap_or(0))));
        }
    }
    cv.align = match st.h_align {
        HAlign::General => cv.align,
        HAlign::Left | HAlign::Fill | HAlign::Justify | HAlign::Distributed => CellAlign::Left,
        HAlign::Center | HAlign::CenterAcross => CellAlign::Center,
        HAlign::Right => CellAlign::Right,
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

/// A cell's view under its conditional formats: their fill, colour scale, font
/// colour, weight and number format, and the data bar under the text.
pub fn cond_view(cv: Option<CellView>, look: &officina_grid::condfmt::Look, v: &Value) -> Option<CellView> {
    let d = &look.dxf;
    if look.scale.is_none() && look.bar.is_none() && look.icon.is_none() && *d == officina_grid::condfmt::Dxf::default() {
        return cv;
    }
    let mut cv = cv.unwrap_or_else(|| CellView::text(""));
    if let (Some(code), Value::Num(_)) = (&d.num_fmt, v) {
        let f = officina_grid::numfmt::NumFmt::parse(code).format(v);
        cv.text = if f.hashes { "#".repeat(8) } else { f.text };
        if let Some(c) = f.color {
            cv.color = Some(rgb(c.rgb()));
        }
    }
    if let Some(c) = look.scale.or(d.fill) {
        cv.fill = Some(rgb(c));
    }
    if let Some(c) = d.color {
        cv.color = Some(rgb(c));
    }
    if let Some(b) = d.bold {
        cv.bold = b;
    }
    cv.bar = look.bar.map(|(share, c)| (share, rgb(c)));
    cv.icon = look.icon.and_then(|(set, k)| icon_of(set, k));
    if look.icon.is_some() && look.hide_value {
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
const KEY_FILTER: u64 = 1 << 61;
const KEY_TABLE: u64 = 1 << 60;

impl SheetSource {
    /// Tables (an outline, a header tint, stripes) and the auto-filter's header marks
    /// (a filtered column's mark is lit), for the part of the sheet in `view`.
    fn data_marks(&self, view: [f64; 4], theme: &pergola::theme::Semantic) -> Vec<pergola::widgets::GridFloat> {
        use pergola::shape::{Paint, PathData, Shape};
        let d = self.doc.borrow();
        let s = d.sheet;
        let mut out = Vec::new();
        let (vx0, vy0, vx1, vy1) = (view[0], view[1], view[0] + view[2], view[1] + view[3]);
        let col_x = |c: u32| d.wb.sheet(s).col_sizes.offset(c);
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
            out.push(pergola::widgets::GridFloat { key: KEY_TABLE | i as u64, at: [fx0, fy0, w as f64, h as f64], nodes: Rc::new(nodes), movable: false });
        }
        if let Some(f) = &d.wb.sheet(s).filter {
            let r = f.rect;
            let (y0, y1) = (self.row_top(r.r0), self.row_top(r.r0 + 1));
            if y1 > y0 && y0 <= vy1 && y1 >= vy0 {
                let a = d.wb.sheet(s).col_sizes.index_at(vx0.max(0.0)).max(r.c0);
                let b = d.wb.sheet(s).col_sizes.index_at(vx1.max(0.0)).min(r.c1);
                for c in a..=b {
                    let lit = f.conds.iter().any(|(k, _)| *k == c);
                    let size = 14.0f32.min((y1 - y0) as f32 - 2.0).max(6.0);
                    let x = col_x(c + 1) - size as f64 - 3.0;
                    let y = y0 + ((y1 - y0) - size as f64) / 2.0;
                    let bg = if lit { theme.accent_fill() } else { theme.bg_surface() };
                    let fg = if lit { theme.accent_fg() } else { theme.text_secondary() };
                    let mut nodes = vec![
                        pergola::Node::Rect { rect: pergola::Rect::new(0.0, 0.0, size, size), fill: theme.border_default(), radius: 3.0 },
                        pergola::Node::Rect { rect: pergola::Rect::new(1.0, 1.0, size - 2.0, size - 2.0), fill: bg, radius: 2.0 },
                    ];
                    // A small down-pointing triangle.
                    let (cx, cy, k) = (size / 2.0, size / 2.0 + 1.0, size * 0.22);
                    let tri = PathData::new().move_to(cx - k * 1.3, cy - k * 0.7).line_to(cx + k * 1.3, cy - k * 0.7).line_to(cx, cy + k * 0.8).close();
                    nodes.extend(Shape::new(tri).fill(Paint::Solid(fg)).fills().into_iter().map(|f| pergola::Node::Fill(std::sync::Arc::new(f))));
                    out.push(pergola::widgets::GridFloat { key: KEY_FILTER | c as u64, at: [x, y, size as f64, size as f64], nodes: Rc::new(nodes), movable: false });
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
            (r, c, ((x - self.col_left(c)) * 72.0 / 96.0).max(0.0) as f32, ((y - self.row_top(r)) * 72.0 / 96.0).max(0.0) as f32)
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

impl SheetSource {
    /// The comment of [`Self::note_at`]'s cell as a box beside the cell.
    fn note_float(&self) -> Option<pergola::widgets::GridFloat> {
        let (r, c) = self.note_at.get()?;
        let (comment, link, prompt) = {
            let d = self.doc.borrow();
            let k = d.wb.comment(d.sheet, r, c).map(|k| (k.author.to_string(), k.text.to_string()));
            let l = d.wb.link_at(CellAddr::new(d.sheet, r, c)).map(|l| l.target.to_string());
            // A validation rule's input prompt, and whether it offers a list.
            let p = d.wb.validation_at(CellAddr::new(d.sheet, r, c)).and_then(|v| {
                let list = v.allow == officina_grid::validation::Allow::List && v.dropdown;
                let msg = (v.show_prompt && !v.prompt.is_empty()).then(|| (v.prompt.title.to_string(), v.prompt.text.to_string()));
                (list || msg.is_some()).then_some((msg, list))
            });
            (k, l, p)
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
        if let Some((author, text)) = comment {
            if !author.is_empty() {
                lines.push((author, 1));
            }
            lines.extend(wrap_lines(&text, &st(0), NOTE_W - 2.0 * pad, NOTE_LINES).into_iter().map(|l| (l, 0)));
        }
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
        let h = 2.0 * pad + lh * lines.len() as f32;
        // A comment's pale yellow; a link alone, a plain white box.
        let (edge, fill) = if lines.iter().any(|l| l.1 < 2) || !linked {
            (Color::rgba(0.55, 0.5, 0.3, 1.0), Color::rgba(1.0, 0.97, 0.78, 1.0))
        } else {
            (Color::rgba(0.62, 0.64, 0.68, 1.0), Color::rgba(1.0, 1.0, 1.0, 1.0))
        };
        let mut nodes = vec![
            pergola::Node::Rect { rect: pergola::Rect::new(0.0, 0.0, NOTE_W, h), fill: edge, radius: 3.0 },
            pergola::Node::Rect { rect: pergola::Rect::new(1.0, 1.0, NOTE_W - 2.0, h - 2.0), fill, radius: 2.0 },
        ];
        for (i, (l, kind)) in lines.into_iter().enumerate() {
            let w = pergola::text::measure(&l, &st(kind)).w.min(NOTE_W - 2.0 * pad);
            nodes.push(pergola::Node::Text { rect: pergola::Rect::new(pad, pad + i as f32 * lh, w, lh), content: l, style: st(kind) });
        }
        // Beside the cell (or its merged area), level with its top.
        let (c1, r0) = {
            let d = self.doc.borrow();
            d.wb.merge_at(CellAddr::new(d.sheet, r, c)).map_or((c, r), |m| (m.c1, m.r0))
        };
        let x = self.col_left(c1 + 1) + 6.0;
        let y = self.row_top(r0);
        Some(pergola::widgets::GridFloat { key: 1 << 62, at: [x, y, NOTE_W as f64, h as f64], nodes: std::rc::Rc::new(nodes), movable: false })
    }
}

impl GridSource for SheetSource {
    fn frozen(&self) -> (u32, u32) {
        let d = self.doc.borrow();
        d.wb.sheet(d.sheet).freeze
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
        out.extend(self.note_float());
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
            let x0 = self.col_left(a.from.1) + a.from.2 as f64 * 96.0 / 72.0;
            let y0 = self.row_top(a.from.0) + a.from.3 as f64 * 96.0 / 72.0;
            let x1 = self.col_left(a.to.1) + a.to.2 as f64 * 96.0 / 72.0;
            let y1 = self.row_top(a.to.0) + a.to.3 as f64 * 96.0 / 72.0;
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
            out.push(pergola::widgets::GridFloat { key: i as u64, at: [x0, y0, w, h], nodes, movable: true });
        }
        out
    }

    fn rows(&self) -> u32 {
        let (r, _) = self.used();
        MIN_ROWS.max(r.saturating_add(1000)).min(MAX_ROWS)
    }
    fn cols(&self) -> u32 {
        let (_, c) = self.used();
        MIN_COLS.max(c.saturating_add(26)).min(MAX_COLS)
    }
    fn row_top(&self, r: u32) -> f64 {
        self.rows_view(|s| s.offset(r))
    }
    fn row_at(&self, y: f64) -> u32 {
        let rows = self.rows();
        self.rows_view(|s| s.index_at(y.max(0.0))).min(rows - 1)
    }
    fn col_left(&self, c: u32) -> f64 {
        let d = self.doc.borrow();
        d.wb.sheet(d.sheet).col_sizes.offset(c)
    }
    fn col_at(&self, x: f64) -> u32 {
        let d = self.doc.borrow();
        d.wb.sheet(d.sheet).col_sizes.index_at(x.max(0.0)).min(self.cols() - 1)
    }
    fn cell(&self, r: u32, c: u32) -> Option<CellView> {
        let d = self.doc.borrow();
        let v = d.wb.value(d.sheet, r, c);
        let sh = d.wb.sheet(d.sheet);
        let id = sh.style_id(r, c);
        let mut cv = if id == 0 { view_of(&v) } else { styled_view(&d.wb.styles, id, &v) };
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
        let mut cv = if d.wb.cond_formats().is_empty() { cv } else { cond_view(cv, &d.wb.look(CellAddr::new(d.sheet, r, c)), &v) };
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
    fn edit_text(&self, r: u32, c: u32) -> String {
        let d = self.doc.borrow();
        let a = CellAddr::new(d.sheet, r, c);
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
        let cv = cond_view(view_of(&v), &look, &v).unwrap();
        assert_eq!(cv.text, "25.0%");
        assert_eq!((cv.fill, cv.color, cv.bold), (Some(rgb(0xFFC7CE)), Some(rgb(0x9C0006)), true));
        assert_eq!(cv.bar, Some((0.5, rgb(0x638EC6))));
        // A blank cell a rule fills; a colour scale over the fill.
        let look = Look { dxf: Dxf { fill: Some(1), ..Dxf::default() }, scale: Some(0x63BE7B), ..Look::default() };
        assert_eq!(cond_view(None, &look, &Value::Empty).unwrap().fill, Some(rgb(0x63BE7B)));
        assert!(cond_view(None, &Look::default(), &Value::Empty).is_none());
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
        let cv = styled_view(&t, id, &Value::Num(-1234.5)).unwrap();
        assert_eq!(cv.text, "-1,234.50");
        assert!(cv.bold && cv.numeric && cv.align == CellAlign::Right);
        assert_eq!(cv.color, Some(rgb(0xFF0000)));
        assert_eq!(cv.fill, Some(rgb(0xFFFF00)));
        assert_eq!(cv.borders[3].map(|b| b.0), Some(3.0));
        // An empty cell with a fill still draws; an empty plain one does not.
        assert!(styled_view(&t, id, &Value::Empty).is_some_and(|c| c.text.is_empty()));
        let d = t.intern(Style::default().with_num_fmt("yyyy-mm-dd"));
        assert!(styled_view(&t, d, &Value::Empty).is_none());
        assert_eq!(styled_view(&t, d, &Value::Num(46340.0)).unwrap().text, "2026-11-14");
        assert_eq!(styled_view(&t, d, &Value::Num(-1.0)).unwrap().text, "########");
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
        let cv = cond_view(Some(CellView::number("5")), &look, &v).unwrap();
        assert_eq!((cv.text.as_str(), cv.icon.map(|i| i.0)), ("5", Some(CellIcon::Arrow(0))));
        let cv = cond_view(Some(CellView::number("5")), &Look { hide_value: true, ..look }, &v).unwrap();
        assert_eq!(cv.text, "");
    }
}