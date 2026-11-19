//! A sheet as PDF (printing from Sheets): the used range cut into pages
//! down, then across, the way Excel prints by default. Each cell shows what
//! the grid shows: its formatted text (the app hands it over as drawn, so a
//! number format or a conditional format prints as it looks), its fill,
//! borders and data bar, in the screen's face, cut with "…" or shown as
//! "###" when it doesn't fit. A merged area prints as one cell, its
//! top-left cell's, with no lines inside. Pixels are the screen's (96 to
//! the inch).

use std::cell::RefCell;
use std::io::{self, Write};

use officina_text::layout::{layout_para, Fonts, ParaLayout};
use officina_text::model::{AttrId, CharAttrs, Para, TextDoc};
use officina_text::paint::{paint_para, Item};
use officina_text::LU;

use crate::out::{Page, Pdf};
use crate::{FontFiles, Stats};

/// Points to a screen pixel.
pub const PT_PER_PX: f64 = 0.75;
/// The face cells are drawn in on screen (Pergola's sans) and its size, px.
pub const FACE: &str = "IBM Plex Sans";
pub const TEXT_PX: f64 = 12.0;
/// Room left and right of a cell's text, px; one indent step, px.
pub const PAD_PX: f64 = 4.0;
pub const INDENT_PX: f64 = 12.0;
/// The most cells a sheet prints (a bound on time and size).
pub const MAX_CELLS: u64 = 4_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

/// A cell as the grid draws it. Colours are sRGB 0xRRGGBB.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Cell {
    pub text: String,
    pub align: Align,
    pub bold: bool,
    /// A number: "###" when it doesn't fit, never cut.
    pub numeric: bool,
    pub color: Option<u32>,
    pub fill: Option<u32>,
    /// Left, right, top, bottom: width in px and colour.
    pub borders: [Option<(f32, u32)>; 4],
    /// A data bar: the share of the cell's width (0–1) and its colour.
    pub bar: Option<(f32, u32)>,
    pub indent: u8,
}

/// What to print: the column widths and row heights of the range from A1
/// (px, 0 for hidden), and each cell.
pub struct Sheet<'a> {
    pub name: &'a str,
    pub cols: Vec<f64>,
    pub rows: Vec<f64>,
    pub cell: &'a dyn Fn(u32, u32) -> Option<Cell>,
    /// Light lines between cells, as the screen draws them.
    pub gridlines: bool,
    /// Merged areas, `[r0, c0, r1, c1]` inclusive: each drawn as its
    /// top-left cell across the whole area.
    pub merges: Vec<[u32; 4]>,
    /// Center Across Selection runs, `[row, c0, c1]` inclusive: the text
    /// of the cell at `c0` centred across them all, no lines inside.
    pub across: Vec<[u32; 3]>,
    /// Print titles: rows `a..b` repeated at the top of every page that
    /// starts below them, columns `a..b` at the left of every page that
    /// starts right of them (a sheet's frozen panes). Empty: none.
    pub title_rows: (u32, u32),
    pub title_cols: (u32, u32),
}

/// The paper, in points, and its margins.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Setup {
    pub w: f64,
    pub h: f64,
    pub margin: f64,
    /// Scales the sheet down so its columns fit one page across (never
    /// below a third).
    pub fit_width: bool,
}

impl Setup {
    /// Paper `w` × `h` points with Excel's normal margins (0.75 in).
    pub fn paper(w: f64, h: f64) -> Self {
        Self { w: w.clamp(144.0, 14400.0), h: h.clamp(144.0, 14400.0), margin: 54.0, fit_width: false }
    }
}

/// Cuts `sizes` (already in points) into runs that fit `room`: each run's
/// first index and end. A size wider than the room gets a run of its own.
fn bands(sizes: &[f64], room: f64) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let (mut start, mut used) = (0, 0.0);
    for (i, &s) in sizes.iter().enumerate() {
        if used + s > room + 0.01 && i > start {
            out.push((start, i));
            start = i;
            used = 0.0;
        }
        used += s;
    }
    if start < sizes.len() {
        out.push((start, sizes.len()));
    }
    out
}

/// [`bands`], with `title` taken off the room of each run that starts at
/// or after `from` (where print titles are repeated).
fn bands_after(sizes: &[f64], room: f64, from: usize, title: f64) -> Vec<(usize, usize)> {
    if title <= 0.0 || from == 0 || from >= sizes.len() {
        return bands(sizes, room);
    }
    let mut out = bands(&sizes[..from], room);
    // The run holding the titles may have room left for rows after them.
    let (mut start, mut used) = match out.pop() {
        Some((a, b)) => (a, sizes[a..b].iter().sum::<f64>()),
        None => (0, 0.0),
    };
    let mut room_now = room;
    for (i, &s) in sizes.iter().enumerate().skip(from) {
        if used + s > room_now + 0.01 && i > start {
            out.push((start, i));
            start = i;
            used = 0.0;
            room_now = room - title;
        }
        used += s;
    }
    out.push((start, sizes.len()));
    out
}

/// An object in sheet pixel coordinates, relative to the print area's origin.
pub struct Drawing {
    pub bounds: [f64; 4],
    pub content: DrawingContent,
}

pub enum DrawingContent {
    Chart(Vec<officina_draw::chart::Prim>),
    Picture { bytes: std::sync::Arc<[u8]>, crop: [i32; 4], rotation: i32 },
}

impl Drawing {
    /// Axis-aligned bounds of the drawing after rotation, in sheet pixels.
    pub fn paint_bounds(&self) -> [f64; 4] {
        let [x, y, w, h] = self.bounds;
        if w <= 0.0 || h <= 0.0 {
            return [x, y, 0.0, 0.0];
        }
        let (sin, cos) = self.rotation();
        let width = w * cos.abs() + h * sin.abs();
        let height = w * sin.abs() + h * cos.abs();
        [x + (w - width) / 2.0, y + (h - height) / 2.0, width, height]
    }

    fn rotation(&self) -> (f64, f64) {
        let DrawingContent::Picture { rotation, .. } = &self.content else { return (0.0, 1.0) };
        let angle = rotation.rem_euclid(360 * 60_000);
        match angle {
            0 => (0.0, 1.0),
            5_400_000 => (1.0, 0.0),
            10_800_000 => (0.0, -1.0),
            16_200_000 => (-1.0, 0.0),
            _ => (f64::from(angle) / 60_000.0).to_radians().sin_cos(),
        }
    }
}

mod drawing;

/// Cells' text shaped in the screen's face at one size.
pub struct Shaper<'f> {
    doc: RefCell<TextDoc>,
    plain: AttrId,
    bold: AttrId,
    fonts: &'f dyn Fonts,
}

impl<'f> Shaper<'f> {
    pub fn new(fonts: &'f dyn Fonts, size_pt: f64) -> Self {
        let mut doc = TextDoc::new();
        let half = (size_pt * 2.0).round().clamp(2.0, 3200.0) as u16;
        let a = |bold| CharAttrs { family: Some(FACE.into()), size: Some(half), bold: Some(bold), ..Default::default() };
        let plain = doc.attrs.intern(a(false));
        let bold = doc.attrs.intern(a(true));
        Self { doc: RefCell::new(doc), plain, bold, fonts }
    }

    fn para(&self, text: &str, bold: bool, color: Option<u32>) -> Para {
        let base = if bold { self.bold } else { self.plain };
        let id = match color {
            Some(c) => {
                let mut d = self.doc.borrow_mut();
                let mut a = d.attrs.get(base).clone();
                a.color = Some(c);
                d.attrs.intern(a)
            }
            None => base,
        };
        let mut p = Para::new(0, text, 0);
        p.runs = vec![(0, id)];
        p
    }

    /// `text` laid out on one line, and its width (LU).
    fn lay(&self, text: &str, bold: bool, color: Option<u32>) -> (Para, ParaLayout, i64) {
        let p = self.para(text, bold, color);
        let l = layout_para(&self.doc.borrow(), &p, i64::MAX / 4, self.fonts);
        let w = l.lines.iter().map(|l| l.width).max().unwrap_or(0);
        (p, l, w)
    }

    pub fn width(&self, text: &str, bold: bool) -> i64 {
        self.lay(text, bold, None).2
    }

    /// Shape a complete spreadsheet label at the given top-left in layout
    /// units. Sanitized raster export checks the returned bounds before use.
    pub fn label(&self, text: &str, bold: bool, color: u32, left: i64, top: i64) -> (Vec<Item>, i64, i64) {
        let (p, layout, width) = self.lay(text, bold, Some(color));
        let mut items = Vec::new();
        self.paint(&mut items, &p, &layout, left, |_| top);
        (items, width, layout.height)
    }

    /// A laid-out line painted from `left` (LU), its top where `top` puts
    /// it given the line's height.
    fn paint(&self, out: &mut Vec<Item>, p: &Para, l: &ParaLayout, left: i64, top: impl FnOnce(i64) -> i64) {
        let top = top(l.lines.first().map_or(l.height, |x| x.ascent + x.descent));
        let x0 = l.lines.first().map_or(0, |x| x.x);
        paint_para(out, &self.doc.borrow(), self.fonts, l, &p.text, left - x0, top, None);
    }
}

/// The cell's text laid out to fit `room` (LU): cut with "…", or "###"
/// for a number; None when nothing fits.
fn fit(s: &Shaper, c: &Cell, room: i64) -> Option<(Para, ParaLayout, i64)> {
    let whole = s.lay(&c.text, c.bold, c.color);
    if whole.2 <= room {
        return Some(whole);
    }
    if room <= 0 {
        return None;
    }
    if c.numeric {
        let one = s.width("#", c.bold).max(1);
        return Some(s.lay(&"#".repeat((room / one).max(1) as usize), c.bold, c.color));
    }
    let chars: Vec<char> = c.text.chars().collect();
    let mut n = ((room as f64 / whole.2 as f64) * chars.len() as f64) as usize;
    loop {
        if n == 0 {
            return None;
        }
        let t: String = chars[..n.min(chars.len())].iter().collect::<String>() + "…";
        let cut = s.lay(&t, c.bold, c.color);
        if cut.2 <= room {
            return Some(cut);
        }
        n -= 1;
    }
}

/// Writes `sheet` as PDF on `setup`'s paper to `out`, calling
/// `progress(done, total)` after each page.
pub fn export<W: Write>(sheet: &Sheet, setup: &Setup, fonts: &dyn Fonts, files: &dyn FontFiles, title: &str, out: W, progress: &mut dyn FnMut(usize, usize)) -> io::Result<(W, Stats)> {
    export_drawings(sheet, &[], setup, fonts, files, title, out, progress)
}

fn export_drawings<W: Write>(sheet: &Sheet, drawings: &[Drawing], setup: &Setup, fonts: &dyn Fonts, files: &dyn FontFiles, title: &str, out: W, progress: &mut dyn FnMut(usize, usize)) -> io::Result<(W, Stats)> {
    let (pw, ph) = (setup.w, setup.h);
    let m = setup.margin.clamp(0.0, pw.min(ph) / 3.0);
    let (room_w, room_h) = (pw - 2.0 * m, ph - 2.0 * m);
    if sheet.rows.len() as u64 * sheet.cols.len() as u64 > MAX_CELLS {
        return Err(io::Error::other(format!("more than {MAX_CELLS} cells to print")));
    }
    let total_w: f64 = sheet.cols.iter().sum::<f64>() * PT_PER_PX;
    let k = if setup.fit_width && total_w > room_w { (room_w / total_w).max(1.0 / 3.0) } else { 1.0 } * PT_PER_PX;
    let cols: Vec<f64> = sheet.cols.iter().map(|w| w.max(0.0) * k).collect();
    let rows: Vec<f64> = sheet.rows.iter().map(|h| h.max(0.0) * k).collect();
    // Print titles, unless they would take more than half the page.
    let titles = |(a, b): (u32, u32), sizes: &[f64], room: f64| {
        let (a, b) = ((a as usize).min(sizes.len()), (b as usize).min(sizes.len()));
        let size: f64 = sizes[a..b.max(a)].iter().sum();
        if b > a && size > 0.0 && size <= room / 2.0 { ((a, b), size) } else { ((0, 0), 0.0) }
    };
    let (tr_, tr_h) = titles(sheet.title_rows, &rows, room_h);
    let (tc_, tc_w) = titles(sheet.title_cols, &cols, room_w);
    let across = bands_after(&cols, room_w, tc_.1, tc_w);
    let down = bands_after(&rows, room_h, tr_.1, tr_h);
    let shaper = Shaper::new(fonts, TEXT_PX * k);
    let lu = |v: f64| (v * LU as f64).round() as i64;
    let total = (across.len() * down.len()).max(1);
    let mut pdf = Pdf::new(out)?;
    let none = |_: u32| None;
    let mut done = 0;
    // A cell's text laid in x..x + cw (pt), centred on the row y, ch.
    let words = |items: &mut Vec<Item>, cell: &Cell, x: f64, y: f64, cw: f64, ch: f64| {
        let lead = cell.indent as f64 * INDENT_PX * k;
        let room = lu(cw - 2.0 * PAD_PX * k - lead);
        if cell.text.is_empty() {
            return;
        }
        if let Some((p, l, tw)) = fit(&shaper, cell, room) {
            let left = match cell.align {
                Align::Left => lu(x + PAD_PX * k + lead),
                Align::Center => lu(x) + (lu(cw) - tw) / 2,
                Align::Right => lu(x + cw - PAD_PX * k) - tw,
            };
            let (cy, chl) = (lu(y), lu(ch));
            shaper.paint(items, &p, &l, left, |lh| cy + ((chl - lh) / 2).max(0));
        }
    };
    // One cell (or merged area) at x, y (pt), cw × ch: fill and bar under
    // the text, borders over it.
    let draw = |items: &mut Vec<Item>, cell: &Cell, x: f64, y: f64, cw: f64, ch: f64, text: bool| {
        if let Some(f) = cell.fill {
            items.push(Item::Rect { x: lu(x), y: lu(y), w: lu(cw), h: lu(ch), color: f });
        }
        if let Some((share, col)) = cell.bar {
            let bw = ((cw - 4.0 * k).max(0.0) * share.clamp(0.0, 1.0) as f64).max(0.0);
            items.push(Item::Rect { x: lu(x + 2.0 * k), y: lu(y + 2.0 * k), w: lu(bw), h: lu((ch - 5.0 * k).max(0.0)), color: col });
        }
        if text {
            words(items, cell, x, y, cw, ch);
        }
        for (side, b) in cell.borders.iter().enumerate() {
            let Some((bw, col)) = *b else { continue };
            let bw = (bw as f64 * k).max(0.25);
            let (bx, by, bwid, bh) = match side {
                0 => (x, y, bw, ch),
                1 => (x + cw - bw, y, bw, ch),
                2 => (x, y, cw, bw),
                _ => (x, y + ch - bw, cw, bw),
            };
            items.push(Item::Rect { x: lu(bx), y: lu(by), w: lu(bwid), h: lu(bh), color: col });
        }
    };
    // Rows r0..r1 by columns c0..c1 drawn from (ox, oy): cells, merged
    // areas cut to the block, Center Across runs.
    let block = |items: &mut Vec<Item>, lines: &mut Vec<(f64, f64, f64, f64)>, (r0, r1): (usize, usize), (c0, c1): (usize, usize), ox: f64, oy: f64| {
        // Where each column and row starts in this block.
        let mut xs = vec![ox; c1 - c0 + 1];
        for c in c0..c1 {
            xs[c - c0 + 1] = xs[c - c0] + cols[c].max(0.0);
        }
        let mut ys = vec![oy; r1 - r0 + 1];
        for r in r0..r1 {
            ys[r - r0 + 1] = ys[r - r0] + rows[r].max(0.0);
        }
        // The merged areas on this page, cut to it.
        let pieces: Vec<([u32; 4], [usize; 4])> = sheet
            .merges
            .iter()
            .filter(|a| (a[0] as usize) < r1 && a[2] as usize >= r0 && (a[1] as usize) < c1 && a[3] as usize >= c0)
            .map(|a| (*a, [(a[0] as usize).max(r0), (a[1] as usize).max(c0), (a[2] as usize).min(r1 - 1), (a[3] as usize).min(c1 - 1)]))
            .collect();
        let merged = |r: usize, c: usize| pieces.iter().any(|(_, p)| (p[0]..=p[2]).contains(&r) && (p[1]..=p[3]).contains(&c));
        // Center Across runs on this page, cut to it.
        let runs: Vec<([u32; 3], usize, usize)> = sheet
            .across
            .iter()
            .filter(|a| (r0..r1).contains(&(a[0] as usize)) && (a[1] as usize) < c1 && a[2] as usize >= c0 && a[2] > a[1])
            .map(|a| (*a, (a[1] as usize).max(c0), (a[2] as usize).min(c1 - 1)))
            .collect();
        let in_run = |r: usize, c: usize| runs.iter().any(|(a, p0, p1)| a[0] as usize == r && (*p0..=*p1).contains(&c));
        for r in r0..r1 {
            let (y, ch) = (ys[r - r0], rows[r]);
            if ch <= 0.0 {
                continue;
            }
            for c in c0..c1 {
                let (x, cw) = (xs[c - c0], cols[c]);
                if cw <= 0.0 || merged(r, c) {
                    continue;
                }
                let run = in_run(r, c);
                if sheet.gridlines && !run {
                    lines.push((x, y, cw, ch));
                }
                if let Some(cell) = (sheet.cell)(r as u32, c as u32) {
                    draw(items, &cell, x, y, cw, ch, !run);
                }
            }
        }
        for (a, p) in &pieces {
            let (x, y) = (xs[p[1] - c0], ys[p[0] - r0]);
            let (cw, ch) = (xs[p[3] + 1 - c0] - x, ys[p[2] + 1 - r0] - y);
            if cw <= 0.0 || ch <= 0.0 {
                continue;
            }
            if sheet.gridlines {
                lines.push((x, y, cw, ch));
            }
            if let Some(cell) = (sheet.cell)(a[0], a[1]) {
                // The text goes on the page with the area's first cell.
                let first = p[0] == a[0] as usize && p[1] == a[1] as usize;
                draw(items, &cell, x, y, cw, ch, first);
            }
        }
        for (a, p0, p1) in &runs {
            let r = a[0] as usize;
            let (x, y) = (xs[p0 - c0], ys[r - r0]);
            let (cw, ch) = (xs[p1 + 1 - c0] - x, rows[r]);
            if cw <= 0.0 || ch <= 0.0 {
                continue;
            }
            if sheet.gridlines {
                lines.push((x, y, cw, ch));
            }
            // The text on the page with the run's first cell, centred
            // across the part of the run on it.
            if *p0 == a[1] as usize {
                if let Some(cell) = (sheet.cell)(a[0], a[1]) {
                    words(items, &Cell { align: Align::Center, indent: 0, ..cell }, x, y, cw, ch);
                }
            }
        }
    };
    let offsets = |sizes: &[f64]| {
        let mut at = 0.0;
        std::iter::once(0.0).chain(sizes.iter().map(move |size| {
            at += size;
            at
        })).collect::<Vec<_>>()
    };
    let (xs, ys) = (offsets(&cols), offsets(&rows));
    // Down, then across; the print titles (frozen rows and columns) on
    // every page that doesn't already start with them.
    for &(c0, c1) in &across {
        for &(r0, r1) in &down {
            let mut page = Page::default();
            let mut items = Vec::new();
            let mut lines = Vec::new();
            let tr = if r0 >= tr_.1 && tr_.1 > tr_.0 { Some(tr_) } else { None };
            let tc = if c0 >= tc_.1 && tc_.1 > tc_.0 { Some(tc_) } else { None };
            let (dx, dy) = (tc.map_or(0.0, |_| tc_w), tr.map_or(0.0, |_| tr_h));
            if let (Some(tr), Some(tc)) = (tr, tc) {
                block(&mut items, &mut lines, tr, tc, m, m);
            }
            if let Some(tr) = tr {
                block(&mut items, &mut lines, tr, (c0, c1), m + dx, m);
            }
            if let Some(tc) = tc {
                block(&mut items, &mut lines, (r0, r1), tc, m, m + dy);
            }
            block(&mut items, &mut lines, (r0, r1), (c0, c1), m + dx, m + dy);
            // Gridlines first, so fills and borders cover them.
            if !lines.is_empty() {
                use folium_content::ops::Op;
                page.c.extend([Op::Save, Op::StrokeRgb([0.85, 0.86, 0.88]), Op::LineWidth(0.25)]);
                for (x, y, w, h) in lines {
                    page.c.push(Op::Rect(x, ph - y - h, w, h));
                }
                page.c.extend([Op::Stroke, Op::Restore]);
            }
            pdf.items(&mut page, items, ph, files, &none)?;
            drawing::paint(&mut pdf, &mut page, drawings, drawing::Tile {
                origin: [m + dx - xs[c0], m + dy - ys[r0]],
                clip: [m + dx, m + dy, (xs[c1] - xs[c0]).min(room_w - dx), (ys[r1] - ys[r0]).min(room_h - dy)],
                scale: k,
                height: ph,
            }, fonts, files)?;
            pdf.end_page(page, pw, ph)?;
            done += 1;
            progress(done, total);
        }
    }
    if done == 0 {
        // An empty sheet still prints a page.
        pdf.end_page(Page::default(), pw, ph)?;
        progress(1, 1);
    }
    pdf.finish(files, if title.is_empty() { sheet.name } else { title }, "Atrium Sheets")
}

/// [`export`] into memory in the system's fonts: what Sheets hands the
/// print sheet.
pub fn print(sheet: &Sheet, setup: &Setup, title: &str) -> io::Result<Vec<u8>> {
    print_drawings(sheet, &[], setup, title)
}

/// Print with the sheet's anchored charts and pictures over the cells.
pub fn print_drawings(sheet: &Sheet, drawings: &[Drawing], setup: &Setup, title: &str) -> io::Result<Vec<u8>> {
    let fonts = officina_text::layout::PinnedFonts::new().map_err(io::Error::other)?;
    export_drawings(sheet, drawings, setup, &fonts, &fonts, title, Vec::new(), &mut |_, _| {}).map(|(b, _)| b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bands_fit_the_room_and_wide_ones_stand_alone() {
        assert_eq!(bands(&[10.0, 10.0, 10.0], 25.0), vec![(0, 2), (2, 3)]);
        assert_eq!(bands(&[50.0, 10.0], 25.0), vec![(0, 1), (1, 2)]);
        assert_eq!(bands(&[], 25.0), vec![]);
    }
}
