//! Page raster formats driverless printers take (printing.md §6.1, §6.2):
//! PWG Raster (PWG 5102.4, IPP Everywhere) and URF (AirPrint).
//!
//! Both are a stream of pages, each a fixed header and rows compressed with
//! the same scheme: a line-repeat byte, then packets of repeated or literal
//! pixels. The writers take one row at a time, so `prelum-render` can draw a
//! page in bands and never hold the whole bitmap. The readers exist for round
//! trips and fuzzing (a hostile stream must fail cleanly), and are bounded.

#![forbid(unsafe_code)]

use std::io::{self, Write};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorSpace {
    /// sGray, 8 bits.
    SGray8,
    /// sRGB, 8 bits per colour.
    SRgb8,
}

impl ColorSpace {
    pub fn bytes_per_pixel(self) -> usize {
        match self {
            ColorSpace::SGray8 => 1,
            ColorSpace::SRgb8 => 3,
        }
    }
    fn white(self) -> &'static [u8] {
        match self {
            ColorSpace::SGray8 => &[0xFF],
            ColorSpace::SRgb8 => &[0xFF, 0xFF, 0xFF],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Duplex {
    Simplex,
    LongEdge,
    ShortEdge,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageHeader {
    pub width: u32,
    pub height: u32,
    pub dpi: (u32, u32),
    pub color: ColorSpace,
    pub duplex: Duplex,
    /// 0 = printer default, 3 draft, 4 normal, 5 high.
    pub quality: u8,
    /// PWG media size name, e.g. `iso_a4_210x297mm` (PWG Raster only).
    pub media_name: String,
    /// PWG media type, e.g. `stationery` (PWG Raster only).
    pub media_type: String,
    /// Total pages in the job, 0 if unknown (PWG Raster only).
    pub total_pages: u32,
    /// How the image was transformed for the back of a two-sided sheet,
    /// (cross-feed, feed): 1 as drawn, -1 mirrored on that axis (PWG
    /// 5102.4 CrossFeedTransform, FeedTransform). The writer records it; the
    /// caller draws the pixels that way.
    pub transform: (i32, i32),
}

impl PageHeader {
    pub fn bytes_per_line(&self) -> usize {
        self.width as usize * self.color.bytes_per_pixel()
    }
    /// Page size in points, from pixels and resolution.
    pub fn size_pt(&self) -> (u32, u32) {
        let pt = |px: u32, dpi: u32| if dpi == 0 { 0 } else { ((px as u64 * 72 + dpi as u64 / 2) / dpi as u64) as u32 };
        (pt(self.width, self.dpi.0), pt(self.height, self.dpi.1))
    }
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// A row of the wrong length, too many or too few rows, a bad header.
    Usage(&'static str),
    /// A stream that isn't valid (readers only).
    Invalid(&'static str),
    /// A page larger than the reader's limit.
    TooLarge,
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Usage(w) => write!(f, "raster writer: {w}"),
            Error::Invalid(w) => write!(f, "invalid raster: {w}"),
            Error::TooLarge => write!(f, "raster page too large"),
        }
    }
}

impl std::error::Error for Error {}

/// Compresses one row into packets: control byte 0..=127 means the next
/// pixel repeats (n + 1) times; 129..=255 means (257 - n) literal pixels
/// follow. 128 is never written.
pub fn encode_row(row: &[u8], bpp: usize, out: &mut Vec<u8>) {
    let n = row.len() / bpp;
    let px = |i: usize| &row[i * bpp..(i + 1) * bpp];
    let mut i = 0;
    while i < n {
        let mut run = 1;
        while i + run < n && run < 128 && px(i + run) == px(i) {
            run += 1;
        }
        if run >= 2 {
            out.push((run - 1) as u8);
            out.extend_from_slice(px(i));
            i += run;
            continue;
        }
        // Literal span: up to the start of the next run of two, at most 128.
        let mut lit = 1;
        while i + lit < n && lit < 128 && !(i + lit + 1 < n && px(i + lit) == px(i + lit + 1)) {
            lit += 1;
        }
        if lit == 1 {
            out.push(0);
        } else {
            out.push((257 - lit) as u8);
        }
        out.extend_from_slice(&row[i * bpp..(i + lit) * bpp]);
        i += lit;
    }
}

/// Row-at-a-time compression with line repeats, shared by both writers.
struct Lines {
    bpl: usize,
    bpp: usize,
    height: u32,
    rows: u32,
    prev: Vec<u8>,
    repeat: u32,
    buf: Vec<u8>,
}

impl Lines {
    fn new(h: &PageHeader) -> Lines {
        Lines { bpl: h.bytes_per_line(), bpp: h.color.bytes_per_pixel(), height: h.height, rows: 0, prev: Vec::new(), repeat: 0, buf: Vec::new() }
    }
    fn flush(&mut self, w: &mut impl Write) -> io::Result<()> {
        if self.repeat == 0 {
            return Ok(());
        }
        self.buf.clear();
        self.buf.push((self.repeat - 1) as u8);
        encode_row(&self.prev, self.bpp, &mut self.buf);
        self.repeat = 0;
        w.write_all(&self.buf)
    }
    fn row(&mut self, row: &[u8], w: &mut impl Write) -> Result<(), Error> {
        if row.len() != self.bpl {
            return Err(Error::Usage("row length"));
        }
        if self.rows >= self.height {
            return Err(Error::Usage("more rows than the page height"));
        }
        self.rows += 1;
        if self.repeat > 0 && self.repeat < 256 && row == &self.prev[..] {
            self.repeat += 1;
            return Ok(());
        }
        self.flush(w)?;
        self.prev.clear();
        self.prev.extend_from_slice(row);
        self.repeat = 1;
        Ok(())
    }
    fn finish(&mut self, w: &mut impl Write) -> Result<(), Error> {
        if self.rows != self.height {
            return Err(Error::Usage("fewer rows than the page height"));
        }
        self.flush(w)?;
        Ok(())
    }
}

fn check_header(h: &PageHeader) -> Result<(), Error> {
    if h.width == 0 || h.height == 0 || h.width > 100_000 || h.height > 100_000 {
        return Err(Error::Usage("page size"));
    }
    if h.dpi.0 == 0 || h.dpi.1 == 0 || h.dpi.0 > 4800 || h.dpi.1 > 4800 {
        return Err(Error::Usage("resolution"));
    }
    Ok(())
}

// ---------------------------------------------------------------- PWG Raster

pub const PWG_SYNC: &[u8; 4] = b"RaS2";
pub const PWG_HEADER_LEN: usize = 1796;
const CSPACE_SGRAY: u32 = 18;
const CSPACE_SRGB: u32 = 19;

fn put_str(h: &mut [u8], off: usize, s: &str) {
    let b = s.as_bytes();
    let n = b.len().min(63);
    h[off..off + n].copy_from_slice(&b[..n]);
}

fn put_u32(h: &mut [u8], off: usize, v: u32) {
    h[off..off + 4].copy_from_slice(&v.to_be_bytes());
}

/// The 1796-byte PWG Raster page header (PWG 5102.4 §4.3).
pub fn pwg_header(h: &PageHeader) -> [u8; PWG_HEADER_LEN] {
    let mut b = [0u8; PWG_HEADER_LEN];
    put_str(&mut b, 0, "PwgRaster");
    put_str(&mut b, 128, &h.media_type);
    put_u32(&mut b, 272, (h.duplex != Duplex::Simplex) as u32);
    put_u32(&mut b, 276, h.dpi.0);
    put_u32(&mut b, 280, h.dpi.1);
    let (wpt, hpt) = h.size_pt();
    put_u32(&mut b, 352, wpt);
    put_u32(&mut b, 356, hpt);
    put_u32(&mut b, 368, (h.duplex == Duplex::ShortEdge) as u32);
    put_u32(&mut b, 372, h.width);
    put_u32(&mut b, 376, h.height);
    put_u32(&mut b, 384, 8);
    put_u32(&mut b, 388, 8 * h.color.bytes_per_pixel() as u32);
    put_u32(&mut b, 392, h.bytes_per_line() as u32);
    put_u32(&mut b, 396, 0); // chunky
    put_u32(&mut b, 400, if h.color == ColorSpace::SRgb8 { CSPACE_SRGB } else { CSPACE_SGRAY });
    put_u32(&mut b, 420, h.color.bytes_per_pixel() as u32);
    put_u32(&mut b, 452, h.total_pages);
    put_u32(&mut b, 456, h.transform.0 as u32); // CrossFeedTransform
    put_u32(&mut b, 460, h.transform.1 as u32); // FeedTransform
    put_u32(&mut b, 480, 0x00FF_FFFF); // AlternatePrimary
    put_u32(&mut b, 484, h.quality as u32);
    put_str(&mut b, 1668, "perceptual");
    put_str(&mut b, 1732, &h.media_name);
    b
}

/// Writes a PWG Raster stream.
pub struct PwgWriter<W: Write> {
    w: W,
    page: Option<Lines>,
}

impl<W: Write> PwgWriter<W> {
    pub fn new(mut w: W) -> Result<Self, Error> {
        w.write_all(PWG_SYNC)?;
        Ok(PwgWriter { w, page: None })
    }
    pub fn begin_page(&mut self, h: &PageHeader) -> Result<(), Error> {
        if self.page.is_some() {
            return Err(Error::Usage("page already open"));
        }
        check_header(h)?;
        self.w.write_all(&pwg_header(h))?;
        self.page = Some(Lines::new(h));
        Ok(())
    }
    pub fn write_row(&mut self, row: &[u8]) -> Result<(), Error> {
        let p = self.page.as_mut().ok_or(Error::Usage("no page open"))?;
        p.row(row, &mut self.w)
    }
    pub fn end_page(&mut self) -> Result<(), Error> {
        let mut p = self.page.take().ok_or(Error::Usage("no page open"))?;
        p.finish(&mut self.w)
    }
    pub fn finish(mut self) -> Result<W, Error> {
        if self.page.is_some() {
            return Err(Error::Usage("page still open"));
        }
        self.w.flush()?;
        Ok(self.w)
    }
}

// ----------------------------------------------------------------------- URF

pub const URF_MAGIC: &[u8; 8] = b"UNIRAST\0";
pub const URF_HEADER_LEN: usize = 32;

/// The 32-byte URF page header: bits per pixel, colour space (0 sGray,
/// 1 sRGB), duplex (1 simplex, 2 short edge, 3 long edge), quality, then
/// width, height and resolution as big-endian u32 at 12, 16 and 20.
pub fn urf_header(h: &PageHeader) -> [u8; URF_HEADER_LEN] {
    let mut b = [0u8; URF_HEADER_LEN];
    b[0] = 8 * h.color.bytes_per_pixel() as u8;
    b[1] = if h.color == ColorSpace::SRgb8 { 1 } else { 0 };
    b[2] = match h.duplex {
        Duplex::Simplex => 1,
        Duplex::ShortEdge => 2,
        Duplex::LongEdge => 3,
    };
    b[3] = h.quality;
    put_u32(&mut b, 12, h.width);
    put_u32(&mut b, 16, h.height);
    put_u32(&mut b, 20, h.dpi.0);
    b
}

/// Writes a URF stream. URF states the page count up front.
pub struct UrfWriter<W: Write> {
    w: W,
    page: Option<Lines>,
    pages_left: u32,
}

impl<W: Write> UrfWriter<W> {
    pub fn new(mut w: W, pages: u32) -> Result<Self, Error> {
        if pages == 0 {
            return Err(Error::Usage("no pages"));
        }
        w.write_all(URF_MAGIC)?;
        w.write_all(&pages.to_be_bytes())?;
        Ok(UrfWriter { w, page: None, pages_left: pages })
    }
    pub fn begin_page(&mut self, h: &PageHeader) -> Result<(), Error> {
        if self.page.is_some() {
            return Err(Error::Usage("page already open"));
        }
        if self.pages_left == 0 {
            return Err(Error::Usage("more pages than announced"));
        }
        check_header(h)?;
        if h.dpi.0 != h.dpi.1 {
            return Err(Error::Usage("URF needs square resolution"));
        }
        self.pages_left -= 1;
        self.w.write_all(&urf_header(h))?;
        self.page = Some(Lines::new(h));
        Ok(())
    }
    pub fn write_row(&mut self, row: &[u8]) -> Result<(), Error> {
        let p = self.page.as_mut().ok_or(Error::Usage("no page open"))?;
        p.row(row, &mut self.w)
    }
    pub fn end_page(&mut self) -> Result<(), Error> {
        let mut p = self.page.take().ok_or(Error::Usage("no page open"))?;
        p.finish(&mut self.w)
    }
    pub fn finish(mut self) -> Result<W, Error> {
        if self.page.is_some() || self.pages_left != 0 {
            return Err(Error::Usage("pages missing"));
        }
        self.w.flush()?;
        Ok(self.w)
    }
}

// ------------------------------------------------------------------- readers

/// A decoded page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Page {
    pub width: u32,
    pub height: u32,
    pub dpi: (u32, u32),
    pub color: ColorSpace,
    pub duplex: Duplex,
    pub quality: u8,
    pub pixels: Vec<u8>,
}

struct Cursor<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let e = self.p.checked_add(n).ok_or(Error::Invalid("truncated"))?;
        let s = self.b.get(self.p..e).ok_or(Error::Invalid("truncated"))?;
        self.p = e;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }
}

fn be32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn decode_rows(c: &mut Cursor, width: u32, height: u32, color: ColorSpace, max_bytes: usize) -> Result<Vec<u8>, Error> {
    let bpp = color.bytes_per_pixel();
    let bpl = (width as usize).checked_mul(bpp).ok_or(Error::TooLarge)?;
    let total = bpl.checked_mul(height as usize).ok_or(Error::TooLarge)?;
    if total > max_bytes || total == 0 {
        return Err(Error::TooLarge);
    }
    let mut out = Vec::with_capacity(total);
    let mut row = vec![0u8; bpl];
    let mut y = 0u32;
    while y < height {
        let repeat = c.u8()? as u32 + 1;
        let mut x = 0usize;
        while x < bpl {
            let ctl = c.u8()?;
            if ctl == 128 {
                // Rest of the line is white.
                let w = color.white();
                while x < bpl {
                    row[x..x + bpp].copy_from_slice(w);
                    x += bpp;
                }
            } else if ctl < 128 {
                let n = (ctl as usize + 1) * bpp;
                if x + n > bpl {
                    return Err(Error::Invalid("run past end of line"));
                }
                let p = c.take(bpp)?;
                for k in 0..ctl as usize + 1 {
                    row[x + k * bpp..x + (k + 1) * bpp].copy_from_slice(p);
                }
                x += n;
            } else {
                let n = (257 - ctl as usize) * bpp;
                if x + n > bpl {
                    return Err(Error::Invalid("literal past end of line"));
                }
                row[x..x + n].copy_from_slice(c.take(n)?);
                x += n;
            }
        }
        for _ in 0..repeat {
            if y >= height {
                return Err(Error::Invalid("line repeat past page end"));
            }
            out.extend_from_slice(&row);
            y += 1;
        }
    }
    Ok(out)
}

/// Reads a whole PWG Raster stream; `max_page_bytes` bounds each page.
pub fn read_pwg(b: &[u8], max_page_bytes: usize) -> Result<Vec<Page>, Error> {
    let mut c = Cursor { b, p: 0 };
    if c.take(4)? != PWG_SYNC {
        return Err(Error::Invalid("sync word"));
    }
    let mut pages = Vec::new();
    while c.p < b.len() {
        let h = c.take(PWG_HEADER_LEN)?;
        let width = be32(h, 372);
        let height = be32(h, 376);
        let bpc = be32(h, 384);
        let bpp = be32(h, 388);
        let color = match (be32(h, 400), bpc, bpp) {
            (CSPACE_SGRAY, 8, 8) => ColorSpace::SGray8,
            (CSPACE_SRGB, 8, 24) => ColorSpace::SRgb8,
            _ => return Err(Error::Invalid("colour space")),
        };
        if be32(h, 392) as usize != width as usize * color.bytes_per_pixel() || be32(h, 396) != 0 {
            return Err(Error::Invalid("bytes per line"));
        }
        let duplex = match (be32(h, 272), be32(h, 368)) {
            (0, _) => Duplex::Simplex,
            (_, 0) => Duplex::LongEdge,
            _ => Duplex::ShortEdge,
        };
        let pixels = decode_rows(&mut c, width, height, color, max_page_bytes)?;
        pages.push(Page { width, height, dpi: (be32(h, 276), be32(h, 280)), color, duplex, quality: be32(h, 484).min(255) as u8, pixels });
    }
    Ok(pages)
}

/// Reads a whole URF stream; `max_page_bytes` bounds each page.
pub fn read_urf(b: &[u8], max_page_bytes: usize) -> Result<Vec<Page>, Error> {
    let mut c = Cursor { b, p: 0 };
    if c.take(8)? != URF_MAGIC {
        return Err(Error::Invalid("magic"));
    }
    let count = be32(c.take(4)?, 0);
    let mut pages = Vec::new();
    for _ in 0..count {
        let h = c.take(URF_HEADER_LEN)?;
        let color = match (h[0], h[1]) {
            (8, 0) | (8, 4) => ColorSpace::SGray8,
            (24, 1) => ColorSpace::SRgb8,
            _ => return Err(Error::Invalid("colour space")),
        };
        let duplex = match h[2] {
            2 => Duplex::ShortEdge,
            3 => Duplex::LongEdge,
            _ => Duplex::Simplex,
        };
        let (width, height, dpi) = (be32(h, 12), be32(h, 16), be32(h, 20));
        let pixels = decode_rows(&mut c, width, height, color, max_page_bytes)?;
        pages.push(Page { width, height, dpi: (dpi, dpi), color, duplex, quality: h[3], pixels });
    }
    if c.p != b.len() {
        return Err(Error::Invalid("trailing bytes"));
    }
    Ok(pages)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(w: u32, h: u32, color: ColorSpace) -> PageHeader {
        PageHeader {
            width: w,
            height: h,
            dpi: (300, 300),
            color,
            duplex: Duplex::LongEdge,
            quality: 4,
            media_name: "iso_a4_210x297mm".into(),
            media_type: "stationery".into(),
            total_pages: 2,
            transform: (1, 1),
        }
    }

    /// A page with white margins, a gradient, noise and repeated rows: every
    /// packet kind and line repeats over 256.
    fn page(w: u32, h: u32, color: ColorSpace, seed: u32) -> Vec<u8> {
        let bpp = color.bytes_per_pixel();
        let mut s = seed.wrapping_mul(2654435761).max(1);
        let mut out = vec![0xFFu8; w as usize * h as usize * bpp];
        for y in 0..h as usize {
            if y < 300 || y % 7 == 0 {
                continue; // white rows, long repeats
            }
            for x in 0..w as usize {
                let v = if x < w as usize / 3 {
                    (x % 256) as u8
                } else if x < 2 * w as usize / 3 {
                    s ^= s << 13;
                    s ^= s >> 17;
                    s ^= s << 5;
                    (s >> 24) as u8
                } else {
                    0
                };
                for k in 0..bpp {
                    out[(y * w as usize + x) * bpp + k] = v.wrapping_add(k as u8 * 40);
                }
            }
        }
        out
    }

    #[test]
    fn pwg_round_trip() {
        for color in [ColorSpace::SGray8, ColorSpace::SRgb8] {
            let (w, h) = (517, 700);
            let mut wr = PwgWriter::new(Vec::new()).unwrap();
            let pages = [page(w, h, color, 1), page(w, h, color, 2)];
            for p in &pages {
                wr.begin_page(&header(w, h, color)).unwrap();
                for row in p.chunks(w as usize * color.bytes_per_pixel()) {
                    wr.write_row(row).unwrap();
                }
                wr.end_page().unwrap();
            }
            let bytes = wr.finish().unwrap();
            let raw = 2 * pages[0].len();
            assert!(bytes.len() < raw, "compressed {} of {raw}", bytes.len());
            let back = read_pwg(&bytes, 64 << 20).unwrap();
            assert_eq!(back.len(), 2);
            for (b, p) in back.iter().zip(&pages) {
                assert_eq!(&b.pixels, p);
                assert_eq!((b.width, b.height, b.dpi, b.color, b.duplex, b.quality), (w, h, (300, 300), color, Duplex::LongEdge, 4));
            }
        }
    }

    #[test]
    fn urf_round_trip() {
        for color in [ColorSpace::SGray8, ColorSpace::SRgb8] {
            let (w, h) = (300, 400);
            let mut wr = UrfWriter::new(Vec::new(), 1).unwrap();
            let p = page(w, h, color, 9);
            wr.begin_page(&header(w, h, color)).unwrap();
            for row in p.chunks(w as usize * color.bytes_per_pixel()) {
                wr.write_row(row).unwrap();
            }
            wr.end_page().unwrap();
            let bytes = wr.finish().unwrap();
            let back = read_urf(&bytes, 64 << 20).unwrap();
            assert_eq!(back[0].pixels, p);
            assert_eq!(back[0].duplex, Duplex::LongEdge);
        }
    }

    #[test]
    fn pwg_header_fields() {
        let h = header(2480, 3508, ColorSpace::SRgb8);
        let b = pwg_header(&h);
        assert_eq!(&b[0..9], b"PwgRaster");
        assert_eq!(be32(&b, 352), 595); // A4 width in points at 300 dpi
        assert_eq!(be32(&b, 356), 842);
        assert_eq!(be32(&b, 400), CSPACE_SRGB);
        assert_eq!(be32(&b, 392), 2480 * 3);
        assert_eq!(&b[1732..1748], b"iso_a4_210x297mm");
    }

    #[test]
    fn encode_row_packets() {
        let mut out = Vec::new();
        encode_row(&[1, 1, 1, 2, 3, 4, 4], 1, &mut out);
        assert_eq!(out, [2, 1, 255, 2, 3, 1, 4]);
        out.clear();
        encode_row(&[7], 1, &mut out);
        assert_eq!(out, [0, 7]);
    }

    #[test]
    fn writer_misuse() {
        let mut wr = PwgWriter::new(Vec::new()).unwrap();
        assert!(wr.write_row(&[0]).is_err());
        wr.begin_page(&header(4, 2, ColorSpace::SGray8)).unwrap();
        assert!(wr.write_row(&[0; 3]).is_err());
        wr.write_row(&[0; 4]).unwrap();
        assert!(wr.end_page().is_err());
        let mut u = UrfWriter::new(Vec::new(), 1).unwrap();
        let mut h = header(4, 1, ColorSpace::SGray8);
        h.dpi = (300, 600);
        assert!(u.begin_page(&h).is_err());
    }

    #[test]
    fn hostile_streams_fail_cleanly() {
        let mut wr = PwgWriter::new(Vec::new()).unwrap();
        wr.begin_page(&header(64, 64, ColorSpace::SRgb8)).unwrap();
        let p = page(64, 64, ColorSpace::SRgb8, 3);
        for row in p.chunks(64 * 3) {
            wr.write_row(row).unwrap();
        }
        wr.end_page().unwrap();
        let bytes = wr.finish().unwrap();
        for n in 0..bytes.len() {
            let _ = read_pwg(&bytes[..n], 1 << 20);
        }
        // a huge declared page is refused before allocation
        let mut big = bytes.clone();
        big[4 + 372..4 + 376].copy_from_slice(&100_000u32.to_be_bytes());
        big[4 + 392..4 + 396].copy_from_slice(&300_000u32.to_be_bytes());
        assert!(matches!(read_pwg(&big, 1 << 20), Err(Error::TooLarge)));
    }
}
