//! A sheet as PDF (printing from Sheets): the used range cut into pages
//! down, then across, the way Excel prints by default. Each cell shows what
//! the grid shows: its formatted text (the app hands it over as drawn, so a
//! number format or a conditional format prints as it looks), its fill,
//! borders and data bar, in the screen's face, cut with "…" or shown as
//! "###" when it doesn't fit. A merged area prints as one cell, its
//! top-left cell's, with no lines inside. Pixels are the screen's (96 to
//! the inch).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::Arc;
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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum VAlign {
    Top,
    #[default]
    Center,
    Bottom,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pattern {
    pub rows: [u8; 8],
    pub ink: u32,
    pub paper: u32,
}

/// Vector parts of a conditional icon in a 14-pixel square.
#[derive(Debug, Clone, PartialEq)]
pub enum IconPart {
    Rect { rect: [f32; 4], radius: f32, color: u32 },
    Line { from: [f32; 2], to: [f32; 2], width: f32, color: u32 },
    Polygon { points: Vec<[f32; 2]>, clip: Option<[f32; 4]>, color: u32 },
}

/// A cell as the grid draws it. Colours are sRGB 0xRRGGBB.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Cell {
    pub text: String,
    pub align: Align,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    /// Text size, px, when not the grid's own ([`TEXT_PX`]).
    pub size: Option<f32>,
    pub wrap: bool,
    pub valign: VAlign,
    /// Counterclockwise degrees, or 255 for vertically stacked graphemes.
    pub rotation: i16,
    pub shrink_to_fit: bool,
    /// A number: "###" when it doesn't fit, never cut.
    pub numeric: bool,
    pub color: Option<u32>,
    pub fill: Option<u32>,
    pub pattern: Option<Pattern>,
    /// Angle in millidegrees and stops at basis point offsets.
    pub gradient: Option<(u32, Arc<[(u16, u32)]>)>,
    pub diagonals: [Option<(f32, u32)>; 2],
    /// Left, right, top, bottom: width in px and colour.
    pub borders: [Option<(f32, u32)>; 4],
    /// A data bar: the share of the cell's width (0–1) and its colour.
    pub bar: Option<(f32, u32)>,
    pub icon: Option<Arc<[IconPart]>>,
    pub indent: u8,
}

/// Maximum text carried into the printed comments appendix.
pub const MAX_COMMENT_BYTES: usize = comments::MAX_COMMENT_BYTES;

/// Printed notes and threads, keyed by snapshot cell.
pub type Comments = BTreeMap<(u32, u32), Comment>;

pub struct Comment {
    pub address: String,
    /// Each message has a heading (author) and styled text.
    pub messages: Vec<CommentMessage>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CommentStyle {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
}

pub struct CommentMessage {
    pub heading: String,
    pub text: Arc<str>,
    pub runs: Vec<(std::ops::Range<usize>, CommentStyle)>,
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
        if used + s > room + 0.01 && used > 0.0 {
            out.push((start, i));
            start = i;
            used = 0.0;
        }
        used += s;
    }
    if used > 0.0 {
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
        if used + s > room_now + 0.01 && used > 0.0 {
            out.push((start, i));
            start = i;
            used = 0.0;
            room_now = room - title;
        }
        used += s;
    }
    if used > 0.0 { out.push((start, sizes.len())); }
    out
}

/// Body bounds inside the snapshot and which title bands came from print
/// settings. The snapshot may also contain explicit titles outside the body.
#[derive(Clone, Copy)]
pub struct PrintRegion {
    pub body: [u32; 4],
    pub explicit_rows: bool,
    pub explicit_cols: bool,
}

#[derive(Clone, Copy)]
struct AxisTile {
    start: usize,
    end: usize,
    length: f64,
    draw_objects: bool,
}

fn axis_pages(sizes: &[f64], room: f64, (lo, hi): (usize, usize), title: (u32, u32), explicit: bool) -> Vec<Vec<AxisTile>> {
    let (a, b) = if explicit {
        ((title.0 as usize).min(sizes.len()), (title.1 as usize).min(sizes.len()))
    } else {
        ((title.0 as usize).clamp(lo, hi), (title.1 as usize).clamp(lo, hi))
    };
    let measure = |start, end| sizes[start..end].iter().sum::<f64>();
    let tile = |start, end, draw_objects| AxisTile { start, end, length: measure(start, end), draw_objects };
    let size = measure(a, b.max(a));
    let title = (a < b && size > 0.0 && size <= room / 2.0).then_some(tile(a, b.max(a), explicit));
    if let Some(title) = title.filter(|_| explicit) {
        // The title is a prefix on every page. Exclude its original cells from
        // the body so a title inside the print area appears exactly once.
        let indices: Vec<_> = (lo..hi).filter(|i| !(a..b).contains(i)).collect();
        let lengths: Vec<_> = indices.iter().map(|&i| sizes[i]).collect();
        let runs = bands(&lengths, room - size);
        if runs.is_empty() { return vec![vec![title]]; }
        return runs.into_iter().map(|(start, end)| {
            let mut tiles = vec![title];
            let mut run = indices[start];
            let mut previous = run;
            for &i in &indices[start + 1..end] {
                if i != previous + 1 {
                    tiles.push(tile(run, previous + 1, true));
                    run = i;
                }
                previous = i;
            }
            tiles.push(tile(run, previous + 1, true));
            tiles
        }).collect();
    }
    let split = title.map_or(0, |t| t.end - lo);
    bands_after(&sizes[lo..hi], room, split, title.map_or(0.0, |t| t.length))
        .into_iter().map(|(start, end)| {
            let start = start + lo;
            let mut tiles = Vec::with_capacity(2);
            if let Some(t) = title.filter(|t| start >= t.end) { tiles.push(t); }
            tiles.push(tile(start, end + lo, true));
            tiles
        }).collect()
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

mod appearance;
mod comments;
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
        self.attributed(text, &CharAttrs { bold: Some(bold), color, ..Default::default() })
    }

    fn attributed(&self, text: &str, attrs: &CharAttrs) -> Para {
        let base = if attrs.bold == Some(true) { self.bold } else { self.plain };
        let mut doc = self.doc.borrow_mut();
        let mut a = doc.attrs.get(base).clone();
        a.color = attrs.color;
        a.italic = attrs.italic;
        a.underline = attrs.underline;
        a.strike = attrs.strike;
        a.size = attrs.size.or(a.size);
        let id = doc.attrs.intern(a);
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

#[derive(Default)]
struct CellPaint {
    items: Vec<Item>,
    clips: Vec<(usize, usize, [f64; 4])>,
}

impl CellPaint {
    fn words(&mut self, shaper: &Shaper, cell: &Cell, rect: [f64; 4], scale: f64) {
        let start = self.items.len();
        appearance::words(&mut self.items, shaper, cell, rect, scale);
        let end = self.items.len();
        if start != end && (cell.rotation != 0 || cell.wrap || cell.text.contains(['\r', '\n']) || rect[3] < 20.0 * scale) {
            self.clips.push((start, end, rect));
        }
    }

    fn write<W: Write>(self, pdf: &mut Pdf<W>, page: &mut Page, height: f64, files: &dyn FontFiles) -> io::Result<()> {
        use folium_content::ops::Op;
        let mut items = self.items.into_iter();
        let mut written = 0;
        for (start, end, [x, y, w, h]) in self.clips {
            pdf.items(page, items.by_ref().take(start - written), height, files, &|_| None)?;
            page.c.extend([Op::Save, Op::Rect(x, height - y - h, w, h), Op::Clip, Op::EndPath]);
            pdf.items(page, items.by_ref().take(end - start), height, files, &|_| None)?;
            page.c.push(Op::Restore);
            written = end;
        }
        pdf.items(page, items, height, files, &|_| None)
    }
}

/// Writes `sheet` as PDF on `setup`'s paper to `out`, calling
/// `progress(done, total)` after each page.
pub fn export<W: Write>(sheet: &Sheet, setup: &Setup, fonts: &dyn Fonts, files: &dyn FontFiles, title: &str, out: W, progress: &mut dyn FnMut(usize, usize)) -> io::Result<(W, Stats)> {
    export_drawings(sheet, &[], None, &Comments::new(), setup, fonts, files, title, out, progress)
}

fn export_drawings<W: Write>(sheet: &Sheet, drawings: &[Drawing], region: Option<PrintRegion>, notes: &Comments, setup: &Setup, fonts: &dyn Fonts, files: &dyn FontFiles, title: &str, out: W, progress: &mut dyn FnMut(usize, usize)) -> io::Result<(W, Stats)> {
    let (pw, ph) = (setup.w, setup.h);
    let m = setup.margin.clamp(0.0, pw.min(ph) / 3.0);
    let (room_w, room_h) = (pw - 2.0 * m, ph - 2.0 * m);
    if sheet.rows.len().max(1) as u64 * sheet.cols.len().max(1) as u64 > MAX_CELLS {
        return Err(io::Error::other(format!("more than {MAX_CELLS} cells to print")));
    }
    let PrintRegion { body, explicit_rows, explicit_cols } = region.unwrap_or(PrintRegion {
        body: [0, 0, sheet.rows.len() as u32, sheet.cols.len() as u32],
        explicit_rows: false, explicit_cols: false,
    });
    let [top, left, bottom, right] = body.map(|i| i as usize);
    if top > bottom || bottom > sheet.rows.len() || left > right || right > sheet.cols.len() {
        return Err(io::Error::other("print area is outside the snapshot"));
    }
    let extra_title_width = if explicit_cols {
        let (a, b) = sheet.title_cols;
        (a as usize..(b as usize).min(sheet.cols.len())).filter(|i| !(left..right).contains(i)).map(|i| sheet.cols[i]).sum()
    } else { 0.0 };
    let total_w = (sheet.cols[left..right].iter().sum::<f64>() + extra_title_width) * PT_PER_PX;
    let k = if setup.fit_width && total_w > room_w { (room_w / total_w).max(1.0 / 3.0) } else { 1.0 } * PT_PER_PX;
    let cols: Vec<f64> = sheet.cols.iter().map(|w| w.max(0.0) * k).collect();
    let rows: Vec<f64> = sheet.rows.iter().map(|h| h.max(0.0) * k).collect();
    let across = axis_pages(&cols, room_w, (left, right), sheet.title_cols, explicit_cols);
    let down = axis_pages(&rows, room_h, (top, bottom), sheet.title_rows, explicit_rows);
    let shaper = Shaper::new(fonts, TEXT_PX * k);
    let note_pages = if notes.is_empty() {
        Vec::new()
    } else {
        let mask = |len: usize, pages: &[Vec<AxisTile>]| {
            let mut changes = vec![0i32; len + 1];
            for tile in pages.iter().flatten() {
                changes[tile.start] += 1;
                changes[tile.end] -= 1;
            }
            changes.into_iter().take(len).scan(0, |active, change| {
                *active += change;
                Some(*active > 0)
            }).collect::<Vec<_>>()
        };
        let (printed_rows, printed_cols) = (mask(rows.len(), &down), mask(cols.len(), &across));
        let printable = |r: u32, c: u32| {
            let (r, c) = (r as usize, c as usize);
            printed_rows.get(r) == Some(&true) && printed_cols.get(c) == Some(&true)
                && rows[r] > 0.0 && cols[c] > 0.0
        };
        let comments: Vec<_> = notes.iter().filter(|((r, c), _)| printable(*r, *c))
            .map(|(_, comment)| comment).collect();
        comments::layout(&comments, &shaper, m, room_w, room_h)?
    };
    let total = (across.len() * down.len()).max(1) + note_pages.len();
    let mut pdf = Pdf::new(out)?;
    let none = |_: u32| None;
    let mut done = 0;
    // Rows r0..r1 by columns c0..c1 drawn from (ox, oy): cells, merged
    // areas cut to the block, Center Across runs.
    let block = |page: &mut Page, items: &mut CellPaint, lines: &mut Vec<(f64, f64, f64, f64)>, (r0, r1): (usize, usize), (c0, c1): (usize, usize), ox: f64, oy: f64| {
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
                let at = (r as u32, c as u32);
                if let Some(cell) = (sheet.cell)(at.0, at.1) {
                    appearance::cell(page, items, &shaper, &cell, [x, y, cw, ch], k, ph, !run);
                }
                if !run && notes.contains_key(&at) {
                    appearance::note_marker(&mut items.items, [x, y, cw, ch], k);
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
                appearance::cell(page, items, &shaper, &cell, [x, y, cw, ch], k, ph, first);
                if first {
                    if notes.contains_key(&(a[0], a[1])) { appearance::note_marker(&mut items.items, [x, y, cw, ch], k); }
                }
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
                    items.words(&shaper, &Cell { align: Align::Center, indent: 0, ..cell }, [x, y, cw, ch], k);
                    if notes.contains_key(&(a[0], a[1])) { appearance::note_marker(&mut items.items, [x, y, cw, ch], k); }
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
    // Down, then across, placing the Cartesian product of each page's
    // title and body bands.
    for columns in &across {
        for rows in &down {
            let mut page = Page::default();
            let mut items = CellPaint::default();
            let mut lines = Vec::new();
            let mut drawing_tiles = Vec::new();
            let mut y = m;
            for r in rows {
                let mut x = m;
                for c in columns {
                    block(&mut page, &mut items, &mut lines, (r.start, r.end), (c.start, c.end), x, y);
                    if c.draw_objects && r.draw_objects {
                        // Include objects that intersect the print area. On a
                        // title tile, paint only the part inside the area.
                        let x0 = c.start.max(left).min(c.end);
                        let x1 = c.end.min(right).max(x0);
                        let y0 = r.start.max(top).min(r.end);
                        let y1 = r.end.min(bottom).max(y0);
                        let origin = [x - xs[c.start], y - ys[r.start]];
                        let (clip_x, clip_y) = (origin[0] + xs[x0], origin[1] + ys[y0]);
                        let w = (xs[x1] - xs[x0]).min(m + room_w - clip_x);
                        let h = (ys[y1] - ys[y0]).min(m + room_h - clip_y);
                        if w > 0.0 && h > 0.0 {
                            drawing_tiles.push(drawing::Tile { origin, clip: [clip_x, clip_y, w, h], scale: k, height: ph });
                        }
                    }
                    x += c.length;
                }
                y += r.length;
            }
            // Gridlines first, so fills and borders cover them. All grid
            // content stays within the margins, including oversized rows.
            use folium_content::ops::Op;
            let gradients = std::mem::take(&mut page.c);
            page.c.extend([Op::Save, Op::Rect(m, m, room_w, room_h), Op::Clip, Op::EndPath]);
            if !lines.is_empty() {
                page.c.extend([Op::Save, Op::StrokeRgb([0.85, 0.86, 0.88]), Op::LineWidth(0.25)]);
                for (x, y, w, h) in lines {
                    page.c.push(Op::Rect(x, ph - y - h, w, h));
                }
                page.c.extend([Op::Stroke, Op::Restore]);
            }
            page.c.extend(gradients);
            items.write(&mut pdf, &mut page, ph, files)?;
            page.c.push(Op::Restore);
            for tile in drawing_tiles {
                drawing::paint(&mut pdf, &mut page, drawings, tile, fonts, files)?;
            }
            pdf.end_page(page, pw, ph)?;
            done += 1;
            progress(done, total);
        }
    }
    if done == 0 {
        // An empty sheet still prints a page.
        pdf.end_page(Page::default(), pw, ph)?;
        done = 1;
        progress(done, total);
    }
    for items in note_pages {
        let mut page = Page::default();
        pdf.items(&mut page, items, ph, files, &none)?;
        pdf.end_page(page, pw, ph)?;
        done += 1;
        progress(done, total);
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
    print_region(sheet, drawings, None, setup, title)
}

pub fn print_region(sheet: &Sheet, drawings: &[Drawing], region: Option<PrintRegion>, setup: &Setup, title: &str) -> io::Result<Vec<u8>> {
    print_with_comments(sheet, drawings, region, &Comments::new(), setup, title)
}

pub fn print_with_comments(sheet: &Sheet, drawings: &[Drawing], region: Option<PrintRegion>, notes: &Comments, setup: &Setup, title: &str) -> io::Result<Vec<u8>> {
    let fonts = officina_text::layout::PinnedFonts::new().map_err(io::Error::other)?;
    export_drawings(sheet, drawings, region, notes, setup, &fonts, &fonts, title, Vec::new(), &mut |_, _| {}).map(|(b, _)| b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_titles_can_precede_or_overlap_the_print_area() {
        let sizes = [10.0; 9];
        let parts = |body, title| {
            axis_pages(&sizes, 35.0, body, title, true).into_iter().map(|page| {
                page.into_iter().map(|tile| (tile.start, tile.end)).collect::<Vec<_>>()
            }).collect::<Vec<_>>()
        };
        assert_eq!(parts((3, 8), (0, 1)), [vec![(0, 1), (3, 5)], vec![(0, 1), (5, 7)], vec![(0, 1), (7, 8)]]);
        assert_eq!(parts((0, 5), (1, 2)), [vec![(1, 2), (0, 1), (2, 3)], vec![(1, 2), (3, 5)]]);
    }

    #[test]
    fn bands_fit_the_room_and_wide_ones_stand_alone() {
        assert_eq!(bands(&[10.0, 10.0, 10.0], 25.0), vec![(0, 2), (2, 3)]);
        assert_eq!(bands(&[50.0, 10.0], 25.0), vec![(0, 1), (1, 2)]);
        assert_eq!(bands(&[], 25.0), vec![]);
        assert_eq!(bands(&[0.0, 0.0, 50.0, 0.0], 25.0), vec![(0, 3)]);
        assert!(bands(&[0.0, 0.0], 25.0).is_empty());
    }
}
