//! A sheet as a [`GridSource`]: what `GridView` asks for, read from the document.

use std::cell::RefCell;
use std::rc::Rc;

use officina_grid::addr::{CellAddr, MAX_COLS, MAX_ROWS};
use officina_grid::style::{HAlign, Line, StyleId, StyleTable};
use officina_grid::Value;
use pergola::Color;
use pergola::theme::palette;
use pergola::widgets::{CellAlign, CellView, GridSource};

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
}

type ViewKey = (u16, usize, usize, usize, usize, u64);
type ChartKey = (u16, u32, u32, u64, bool);

impl SheetSource {
    pub fn new(doc: Rc<RefCell<Doc>>) -> Self {
        Self { doc, view: RefCell::new(None), charts: RefCell::new(Vec::new()) }
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

fn rgb(c: u32) -> Color {
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
            out.push(pergola::widgets::GridFloat { key: KEY_TABLE | i as u64, at: [fx0, fy0, w as f64, h as f64], nodes: Rc::new(nodes) });
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
                    out.push(pergola::widgets::GridFloat { key: KEY_FILTER | c as u64, at: [x, y, size as f64, size as f64], nodes: Rc::new(nodes) });
                }
            }
        }
        out
    }
}

impl GridSource for SheetSource {
    fn floats(&self, view: [f64; 4], theme: &pergola::theme::Semantic) -> Vec<pergola::widgets::GridFloat> {
        let mut out = self.data_marks(view, theme);
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
            out.push(pergola::widgets::GridFloat { key: i as u64, at: [x0, y0, w, h], nodes });
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
        if id == 0 {
            return view_of(&v);
        }
        styled_view(&d.wb.styles, id, &v)
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
}
