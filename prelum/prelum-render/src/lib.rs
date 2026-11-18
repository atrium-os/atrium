//! Prelum's conversion step, raster half (printing.md §6.1).
//!
//! A PDF read by Folium is laid out on the printer's paper (page ranges,
//! pages per sheet, fit or actual size, pages turned to match the paper) and
//! drawn in bands of rows at the printer's resolution with Folium's CPU
//! renderer, straight into a PWG Raster or URF writer. A whole sheet is never
//! held in memory: a band is the sheet's width times [`BAND_ROWS`].
//!
//! The back of a two-sided sheet is drawn transformed the way the printer
//! asks (`pwg-raster-document-sheet-back`, or URF's `DM` entry), and the
//! transform is recorded in the page header.
//!
//! This runs inside `prelum-render`'s jail: no network, no files but the
//! document descriptor, budgets on time per page.

#![forbid(unsafe_code)]

use std::io::Write;
use std::time::{Duration, Instant};

use folium_content::{interpret, DisplayList, Rect, Shared};
use folium_doc::{Doc, Page};
use folium_render::{Renderer, View};
use prelum_raster::{ColorSpace, Duplex, PageHeader, PwgWriter, UrfWriter};

pub mod pdf;

/// Rows drawn at a time.
pub const BAND_ROWS: u32 = 256;

#[derive(Debug)]
pub enum Error {
    /// Folium could not read the document or a page.
    Document(String),
    /// A page went past its time budget; 1-based page number.
    PageTooSlow(usize),
    /// Nothing to print (no pages selected).
    Empty,
    /// The settings make no sense (zero size, unknown media).
    Settings(&'static str),
    Raster(prelum_raster::Error),
    Cancelled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Document(e) => write!(f, "document: {e}"),
            Error::PageTooSlow(n) => write!(f, "page {n} could not be prepared in time"),
            Error::Empty => write!(f, "no pages to print"),
            Error::Settings(w) => write!(f, "settings: {w}"),
            Error::Raster(e) => write!(f, "{e}"),
            Error::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for Error {}

impl From<prelum_raster::Error> for Error {
    fn from(e: prelum_raster::Error) -> Self {
        Error::Raster(e)
    }
}

/// Size in points of a PWG self-describing media name
/// (`iso_a4_210x297mm`, `na_letter_8.5x11in`; PWG 5101.1).
pub fn media_size_pt(keyword: &str) -> Option<(f64, f64)> {
    let dims = keyword.rsplit('_').next()?;
    let (num, unit) = if let Some(d) = dims.strip_suffix("mm") {
        (d, 72.0 / 25.4)
    } else if let Some(d) = dims.strip_suffix("in") {
        (d, 72.0)
    } else {
        return None;
    };
    let (w, h) = num.split_once('x')?;
    let (w, h): (f64, f64) = (w.parse().ok()?, h.parse().ok()?);
    let ok = |v: f64| v.is_finite() && v > 0.0 && v < 5000.0;
    (ok(w) && ok(h)).then_some((w * unit, h * unit))
}

/// Parses a page selection such as `1-3,5,8-` against a page count. The
/// result is 0-based, sorted and without repeats. Empty or blank means all.
pub fn parse_pages(spec: &str, count: usize) -> Result<Vec<usize>, Error> {
    if spec.trim().is_empty() {
        return Ok((0..count).collect());
    }
    let mut pick = vec![false; count];
    for part in spec.split(',').take(256) {
        let part = part.trim();
        let num = |s: &str| s.trim().parse::<usize>().ok().filter(|n| *n >= 1);
        let (a, b) = match part.split_once('-') {
            Some((a, b)) => (if a.trim().is_empty() { Some(1) } else { num(a) }, if b.trim().is_empty() { Some(count) } else { num(b) }),
            None => (num(part), num(part)),
        };
        let (Some(a), Some(b)) = (a, b) else { return Err(Error::Settings("page range")) };
        for p in a..=b.min(count) {
            pick[p - 1] = true;
        }
    }
    let pages: Vec<usize> = pick.iter().enumerate().filter(|(_, p)| **p).map(|(i, _)| i).collect();
    if pages.is_empty() {
        return Err(Error::Empty);
    }
    Ok(pages)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Scale {
    /// Grow or shrink each page to fill its place on the sheet.
    Fit,
    /// Actual size, shrunk only if the page doesn't fit.
    ShrinkToFit,
    /// A fixed percentage of actual size.
    Percent(f64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RasterFormat {
    Pwg,
    Urf,
}

#[derive(Clone, Debug)]
pub struct Settings {
    /// PWG media name; its size is the sheet.
    pub media: String,
    /// `media-type` keyword for the PWG header (`stationery`).
    pub media_type: String,
    pub dpi: u32,
    pub color: ColorSpace,
    pub scale: Scale,
    /// 0-based pages, in order (see [`parse_pages`]).
    pub pages: Vec<usize>,
    /// Pages per sheet: 1, 2 or 4.
    pub per_sheet: u8,
    pub duplex: Duplex,
    /// The printer's back-side rule: `normal`, `flipped`, `rotated`,
    /// `manual-tumble` (anything else is treated as `normal`).
    pub sheet_back: String,
    pub quality: u8,
    /// Time a page may take to read and draw before the job fails.
    pub page_budget: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Cell {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

/// Where each page of a sheet goes, in sheet pixels.
#[derive(Clone, Copy, Debug)]
struct Placement {
    page: usize,
    view: View,
    x: i64,
    y: i64,
    w: i64,
    h: i64,
}

/// The cells of a sheet, `(w, h)` in pixels, for 1, 2 or 4 pages per sheet.
/// Two pages share a sheet across its long side.
fn cells(w: f64, h: f64, per_sheet: u8) -> Vec<Cell> {
    match per_sheet {
        2 if h >= w => vec![Cell { x: 0.0, y: 0.0, w, h: h / 2.0 }, Cell { x: 0.0, y: h / 2.0, w, h: h / 2.0 }],
        2 => vec![Cell { x: 0.0, y: 0.0, w: w / 2.0, h }, Cell { x: w / 2.0, y: 0.0, w: w / 2.0, h }],
        4 => (0..4)
            .map(|i| Cell { x: (i % 2) as f64 * w / 2.0, y: (i / 2) as f64 * h / 2.0, w: w / 2.0, h: h / 2.0 })
            .collect(),
        _ => vec![Cell { x: 0.0, y: 0.0, w, h }],
    }
}

fn place(page: &Page, index: usize, cell: Cell, dpi: f64, scale: Scale) -> Placement {
    let crop = Rect::new(page.crop_box.x0, page.crop_box.y0, page.crop_box.x1, page.crop_box.y1);
    let (pw, ph) = page.size();
    // Turn the page a quarter if that matches the cell's shape better,
    // counter-clockwise as IPP's `landscape` is (the top of the page ends up
    // on the left edge of the paper).
    let turn = (pw > ph) != (cell.w > cell.h) && (pw - ph).abs() > 1.0;
    let (pw, ph) = if turn { (ph, pw) } else { (pw, ph) };
    let px_per_pt = dpi / 72.0;
    let fit = (cell.w / (pw * px_per_pt)).min(cell.h / (ph * px_per_pt));
    let k = match scale {
        Scale::Fit => fit,
        Scale::ShrinkToFit => fit.min(1.0),
        Scale::Percent(p) => (p / 100.0).clamp(0.01, 10.0),
    };
    let view = View { crop, rotate: (page.rotate + if turn { 270 } else { 0 }) % 360, scale: px_per_pt * k * page.user_unit };
    let (dw, dh) = view.size();
    let (w, h) = (dw.round() as i64, dh.round() as i64);
    let x = (cell.x + (cell.w - dw) / 2.0).round() as i64;
    let y = (cell.y + (cell.h - dh) / 2.0).round() as i64;
    Placement { page: index, view, x, y, w, h }
}

/// (cross-feed, feed) for a sheet: the back of a two-sided sheet as the
/// printer's back-side rule wants it (the same reading of PWG 5102.4 §4.3.1
/// other IPP Everywhere clients use).
pub fn back_transform(duplex: Duplex, sheet_back: &str, back: bool) -> (i32, i32) {
    if duplex == Duplex::Simplex || !back {
        return (1, 1);
    }
    let tumble = duplex == Duplex::ShortEdge;
    match sheet_back {
        "flipped" if tumble => (-1, 1),
        "flipped" => (1, -1),
        "manual-tumble" if tumble => (-1, -1),
        "rotated" if !tumble => (-1, -1),
        _ => (1, 1),
    }
}

/// Where drawn sheets go: a raster writer, or [`preview`]'s images.
trait Sheets {
    fn begin(&mut self, h: &PageHeader) -> Result<(), Error>;
    fn row(&mut self, r: &[u8]) -> Result<(), Error>;
    fn end(&mut self) -> Result<(), Error>;
}

enum Sink<W: Write> {
    Pwg(PwgWriter<W>),
    Urf(UrfWriter<W>),
}

impl<W: Write> Sheets for Sink<W> {
    fn begin(&mut self, h: &PageHeader) -> Result<(), Error> {
        match self {
            Sink::Pwg(w) => w.begin_page(h)?,
            Sink::Urf(w) => w.begin_page(h)?,
        }
        Ok(())
    }
    fn row(&mut self, r: &[u8]) -> Result<(), Error> {
        match self {
            Sink::Pwg(w) => w.write_row(r)?,
            Sink::Urf(w) => w.write_row(r)?,
        }
        Ok(())
    }
    fn end(&mut self) -> Result<(), Error> {
        match self {
            Sink::Pwg(w) => w.end_page()?,
            Sink::Urf(w) => w.end_page()?,
        }
        Ok(())
    }
}

impl<W: Write> Sink<W> {
    fn finish(self) -> Result<W, Error> {
        Ok(match self {
            Sink::Pwg(w) => w.finish()?,
            Sink::Urf(w) => w.finish()?,
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub sheets: u32,
    pub pages: u32,
    pub bands: u32,
}

/// Sheet count for these settings (URF states it up front; the sheet shows it).
pub fn sheet_count(s: &Settings) -> u32 {
    let per = s.per_sheet.clamp(1, 4) as usize;
    s.pages.len().div_ceil(per) as u32
}

fn check(doc: &Doc, s: &Settings) -> Result<(f64, f64), Error> {
    if s.pages.is_empty() {
        return Err(Error::Empty);
    }
    if s.pages.iter().any(|p| *p >= doc.page_count()) {
        return Err(Error::Settings("page out of range"));
    }
    if !matches!(s.per_sheet, 1 | 2 | 4) {
        return Err(Error::Settings("pages per sheet"));
    }
    media_size_pt(&s.media).ok_or(Error::Settings("media"))
}

/// Draws the selected pages of `doc` into `out` as PWG Raster or URF.
/// `progress` is called after each sheet with (sheets done, sheets).
pub fn render<W: Write>(
    doc: &Doc,
    s: &Settings,
    format: RasterFormat,
    out: W,
    cancel: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(u32, u32),
) -> Result<(W, Stats), Error> {
    check(doc, s)?;
    if !(72..=1200).contains(&s.dpi) {
        return Err(Error::Settings("resolution"));
    }
    let sheets = sheet_count(s);
    let mut sink = match format {
        RasterFormat::Pwg => Sink::Pwg(PwgWriter::new(out)?),
        RasterFormat::Urf => Sink::Urf(UrfWriter::new(out, sheets)?),
    };
    let stats = draw_sheets(doc, s, &mut sink, 0..sheets, cancel, progress)?;
    Ok((sink.finish()?, stats))
}

/// One sheet as the print sheet shows it (printing.md §5.2): 8-bit sRGB,
/// `w` x `h`, rows top to bottom, no transform for the back of a sheet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Preview {
    /// 1-based.
    pub sheet: u32,
    pub w: u32,
    pub h: u32,
    pub rgb: Vec<u8>,
}

/// The longest side of a preview, in pixels.
pub const MAX_PREVIEW: u32 = 400;
/// The most sheets one [`preview`] call draws.
pub const MAX_PREVIEWS: u32 = 8;

struct Collect {
    out: Vec<Preview>,
    next: u32,
}

impl Sheets for Collect {
    fn begin(&mut self, h: &PageHeader) -> Result<(), Error> {
        self.next += 1;
        self.out.push(Preview { sheet: self.next, w: h.width, h: h.height, rgb: Vec::with_capacity((h.width * h.height * 3) as usize) });
        Ok(())
    }
    fn row(&mut self, r: &[u8]) -> Result<(), Error> {
        if let Some(p) = self.out.last_mut() {
            p.rgb.extend_from_slice(r);
        }
        Ok(())
    }
    fn end(&mut self) -> Result<(), Error> {
        Ok(())
    }
}

/// Small images of sheets `first..first+count` (from 1) laid out as `s`
/// would print them, the longer side at most `longest` pixels (at most
/// [`MAX_PREVIEW`]); and how many sheets there are in all. `s.dpi`,
/// `s.duplex` and `s.color` are ignored: previews are sRGB and face up.
pub fn preview(doc: &Doc, s: &Settings, first: u32, count: u32, longest: u32, cancel: &dyn Fn() -> bool) -> Result<(u32, Vec<Preview>), Error> {
    let (mw, mh) = check(doc, s)?;
    let longest = longest.clamp(16, MAX_PREVIEW) as f64;
    let mut p = s.clone();
    // A resolution the long side comes out at, at most `longest`.
    p.dpi = ((longest * 72.0 / mw.max(mh)).floor() as u32).max(1);
    p.color = ColorSpace::SRgb8;
    p.duplex = Duplex::Simplex;
    let sheets = sheet_count(&p);
    let first = first.max(1);
    let last = first.saturating_add(count.min(MAX_PREVIEWS)).min(sheets + 1);
    let mut c = Collect { out: Vec::new(), next: first - 1 };
    if first <= sheets {
        draw_sheets(doc, &p, &mut c, first - 1..last - 1, cancel, &mut |_, _| {})?;
    }
    Ok((sheets, c.out))
}

/// Draws sheets `range` (from 0) of `s` into `sink`.
fn draw_sheets(
    doc: &Doc,
    s: &Settings,
    sink: &mut dyn Sheets,
    range: std::ops::Range<u32>,
    cancel: &dyn Fn() -> bool,
    progress: &mut dyn FnMut(u32, u32),
) -> Result<Stats, Error> {
    let (mw, mh) = check(doc, s)?;
    let dpi = s.dpi as f64;
    let (sw, sh) = ((mw * dpi / 72.0).round() as u32, (mh * dpi / 72.0).round() as u32);
    let sheets = sheet_count(s);
    let shared = Shared::new();
    let renderer = Renderer::new();
    let bpp = s.color.bytes_per_pixel();
    let mut stats = Stats::default();
    let grid = cells(sw as f64, sh as f64, s.per_sheet);

    for (sheet_no, chunk) in s.pages.chunks(s.per_sheet as usize).enumerate() {
        if !range.contains(&(sheet_no as u32)) {
            continue;
        }
        // Read and lay out this sheet's pages.
        let mut placed: Vec<(Placement, DisplayList)> = Vec::new();
        for (slot, &index) in chunk.iter().enumerate() {
            if cancel() {
                return Err(Error::Cancelled);
            }
            let deadline = Instant::now() + s.page_budget;
            let late = || Instant::now() > deadline;
            let stop = || cancel() || late();
            let page = doc.page(index).map_err(|e| Error::Document(format!("page {}: {e:?}", index + 1)))?;
            let contents = doc.contents(&page).map_err(|e| Error::Document(format!("page {}: {e:?}", index + 1)))?;
            let crop = Rect::new(page.crop_box.x0, page.crop_box.y0, page.crop_box.x1, page.crop_box.y1);
            let dl = interpret(&doc.syntax, &shared, &contents, &page.resources, Some(crop), &stop);
            if late() {
                return Err(Error::PageTooSlow(index + 1));
            }
            let dl = dl.map_err(|e| Error::Document(format!("page {}: {e:?}", index + 1)))?;
            placed.push((place(&page, index, grid[slot], dpi, s.scale), dl));
            stats.pages += 1;
        }

        let transform = back_transform(s.duplex, &s.sheet_back, sheet_no % 2 == 1);
        let header = PageHeader {
            width: sw,
            height: sh,
            dpi: (s.dpi, s.dpi),
            color: s.color,
            duplex: s.duplex,
            quality: s.quality,
            media_name: s.media.clone(),
            media_type: s.media_type.clone(),
            total_pages: sheets,
            transform,
        };
        sink.begin(&header)?;
        let bands: Vec<u32> = (0..sh.div_ceil(BAND_ROWS)).collect();
        let order: Box<dyn Iterator<Item = &u32>> = if transform.1 < 0 { Box::new(bands.iter().rev()) } else { Box::new(bands.iter()) };
        let deadline = Instant::now() + s.page_budget * chunk.len() as u32;
        let mut band = Vec::new();
        for &b in order {
            if cancel() {
                return Err(Error::Cancelled);
            }
            if Instant::now() > deadline {
                return Err(Error::PageTooSlow(chunk[0] + 1));
            }
            let y0 = b * BAND_ROWS;
            let rows = BAND_ROWS.min(sh - y0);
            draw_band(&doc.syntax, &renderer, &placed, sw, y0, rows, s.color, &mut band, &|| cancel() || Instant::now() > deadline);
            stats.bands += 1;
            let line = sw as usize * bpp;
            let mut emit = |r: usize| -> Result<(), Error> {
                let row = &mut band[r * line..(r + 1) * line];
                if transform.0 < 0 {
                    mirror(row, bpp);
                }
                sink.row(row)
            };
            if transform.1 < 0 {
                for r in (0..rows as usize).rev() {
                    emit(r)?;
                }
            } else {
                for r in 0..rows as usize {
                    emit(r)?;
                }
            }
        }
        sink.end()?;
        stats.sheets += 1;
        progress(stats.sheets, sheets);
    }
    Ok(stats)
}

fn mirror(row: &mut [u8], bpp: usize) {
    let n = row.len() / bpp;
    for i in 0..n / 2 {
        for k in 0..bpp {
            row.swap(i * bpp + k, (n - 1 - i) * bpp + k);
        }
    }
}

/// Fills `band` with rows `y0..y0+rows` of the sheet: white, then each
/// page's part of the band drawn by Folium and copied in.
#[allow(clippy::too_many_arguments)]
fn draw_band(
    syntax: &folium_syntax::Document,
    renderer: &Renderer,
    placed: &[(Placement, DisplayList)],
    width: u32,
    y0: u32,
    rows: u32,
    color: ColorSpace,
    band: &mut Vec<u8>,
    cancel: &dyn Fn() -> bool,
) {
    let bpp = color.bytes_per_pixel();
    let line = width as usize * bpp;
    band.clear();
    band.resize(line * rows as usize, 0xFF);
    let (by0, by1) = (y0 as i64, y0 as i64 + rows as i64);
    for (p, dl) in placed {
        let ix0 = p.x.max(0);
        let ix1 = (p.x + p.w).min(width as i64);
        let iy0 = p.y.max(by0);
        let iy1 = (p.y + p.h).min(by1);
        if ix0 >= ix1 || iy0 >= iy1 {
            continue;
        }
        let tile = ((ix0 - p.x) as i32, (iy0 - p.y) as i32, (ix1 - ix0) as u32, (iy1 - iy0) as u32);
        let (pm, _) = renderer.render(syntax, dl, &p.view, tile, cancel);
        let src = pm.data();
        let tw = pm.width() as usize;
        for ty in 0..tile.3 as usize {
            let dy = (iy0 - by0) as usize + ty;
            let srow = &src[ty * tw * 4..(ty * tw + tile.2 as usize) * 4];
            let drow = &mut band[dy * line + ix0 as usize * bpp..dy * line + ix1 as usize * bpp];
            match color {
                ColorSpace::SRgb8 => {
                    for (d, s) in drow.chunks_exact_mut(3).zip(srow.chunks_exact(4)) {
                        d.copy_from_slice(&s[..3]);
                    }
                }
                ColorSpace::SGray8 => {
                    for (d, s) in drow.iter_mut().zip(srow.chunks_exact(4)) {
                        *d = ((s[0] as u32 * 54 + s[1] as u32 * 183 + s[2] as u32 * 19) >> 8) as u8;
                    }
                }
            }
        }
        let _ = p.page;
    }
}

#[cfg(test)]
mod tests;
