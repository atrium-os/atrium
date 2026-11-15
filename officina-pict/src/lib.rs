//! Old Windows picture formats as pixels: WMF and EMF metafiles and BMP
//! (and bare DIB) bitmaps, the pictures `.doc`, `.ppt`, `.xls` and many
//! `.docx` and `.pptx` files carry (officina.md §16.45).
//!
//! A metafile is a list of GDI calls. It is played here into a `tiny-skia`
//! canvas the size the picture is drawn at, so it stays sharp at any zoom;
//! text is drawn with the Liberation faces (by family: sans, serif, mono),
//! never a system font. EMF+ records (GDI+ drawing in comments) are skipped:
//! files that carry them also carry the plain EMF drawing, except "EMF+
//! only" files, which draw as nothing and are refused.
//!
//! Everything read is bounded: the records walked, the objects, points and
//! glyphs, and the pixels painted (a budget of area per picture), so a
//! hostile file costs at most a few frames' worth of work.
//!
//! Without the `render` feature only the headers are read: [`sniff`] and
//! [`natural_size`], which is what the converter needs.

pub mod dib;
#[cfg(feature = "render")]
mod charset;
#[cfg(feature = "render")]
mod emf;
#[cfg(feature = "render")]
mod font;
#[cfg(feature = "render")]
mod gdi;
#[cfg(feature = "render")]
mod wmf;

/// The largest file read.
pub const MAX_BYTES: usize = 64 << 20;
/// The largest side drawn, either way.
pub const MAX_SIDE: u32 = 8192;
/// The most pixels drawn.
pub const MAX_PIXELS: u64 = 32 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Wmf,
    Emf,
    Bmp,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::Wmf => "wmf",
            Kind::Emf => "emf",
            Kind::Bmp => "bmp",
        }
    }

    pub fn mime(self) -> &'static str {
        match self {
            Kind::Wmf => "image/wmf",
            Kind::Emf => "image/emf",
            Kind::Bmp => "image/bmp",
        }
    }
}

pub(crate) fn u16_at(b: &[u8], i: usize) -> u16 {
    b.get(i..i + 2).map_or(0, |s| u16::from_le_bytes([s[0], s[1]]))
}
pub(crate) fn i16_at(b: &[u8], i: usize) -> i16 {
    u16_at(b, i) as i16
}
pub(crate) fn u32_at(b: &[u8], i: usize) -> u32 {
    b.get(i..i + 4).map_or(0, |s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}
pub(crate) fn i32_at(b: &[u8], i: usize) -> i32 {
    u32_at(b, i) as i32
}

/// The WMF placeable header's key.
pub(crate) const PLACEABLE: u32 = 0x9AC6_CDD7;

/// What the bytes are, from their header alone.
pub fn sniff(b: &[u8]) -> Option<Kind> {
    if b.len() >= 88 && u32_at(b, 0) == 1 && b.get(40..44) == Some(b" EMF") {
        return Some(Kind::Emf);
    }
    if b.len() >= 22 + 18 && u32_at(b, 0) == PLACEABLE {
        return Some(Kind::Wmf);
    }
    if b.len() >= 18 && matches!(u16_at(b, 0), 1 | 2) && u16_at(b, 2) == 9 && matches!(u16_at(b, 4), 0x100 | 0x300) {
        return Some(Kind::Wmf);
    }
    if b.len() >= 26 && b.starts_with(b"BM") && matches!(u32_at(b, 14), 12 | 40 | 52 | 56 | 64 | 108 | 124) {
        return Some(Kind::Bmp);
    }
    None
}

/// Its size in pixels at 96 dpi (a bitmap: its own size).
pub fn natural_size(b: &[u8]) -> Option<(Kind, u32, u32)> {
    let kind = sniff(b)?;
    let px = |v: f64| (v.abs().round() as u32).clamp(1, 16384);
    let (w, h) = match kind {
        Kind::Emf => {
            let (l, t, r, bo) = (i32_at(b, 24), i32_at(b, 28), i32_at(b, 32), i32_at(b, 36));
            if r > l && bo > t {
                // Hundredths of a millimetre.
                (px((r as f64 - l as f64) * 96.0 / 2540.0), px((bo as f64 - t as f64) * 96.0 / 2540.0))
            } else {
                let (l, t, r, bo) = (i32_at(b, 8), i32_at(b, 12), i32_at(b, 16), i32_at(b, 20));
                if r < l || bo < t {
                    return None;
                }
                (px(r as f64 - l as f64 + 1.0), px(bo as f64 - t as f64 + 1.0))
            }
        }
        Kind::Wmf => {
            if u32_at(b, 0) == PLACEABLE {
                let (l, t, r, bo) = (i16_at(b, 6) as f64, i16_at(b, 8) as f64, i16_at(b, 10) as f64, i16_at(b, 12) as f64);
                let inch = match u16_at(b, 14) {
                    0 => 1440.0,
                    i => i as f64,
                };
                if r == l || bo == t {
                    return None;
                }
                (px((r - l) * 96.0 / inch), px((bo - t) * 96.0 / inch))
            } else {
                // No size of its own: the window it sets, taken as pixels.
                let (_, ext, _) = wmf_window(b)?;
                (px(ext.0), px(ext.1))
            }
        }
        Kind::Bmp => {
            let (w, h) = dib::size(b.get(14..)?)?;
            (w, h)
        }
    };
    Some((kind, w, h))
}

/// A raw WMF's frame: the window it sets before it draws (`true`; not one
/// a text mapping set afterwards voids), else the box its drawing covers
/// (`false`).
pub(crate) fn wmf_window(b: &[u8]) -> Option<((f64, f64), (f64, f64), bool)> {
    let start = if u32_at(b, 0) == PLACEABLE { 22 } else { 0 };
    let first = start + 2 * u16_at(b, start + 2) as usize;
    let mut p = first;
    let (mut org, mut window) = ((0.0, 0.0), None);
    let mut n = 0;
    let records = |mut f: Box<dyn FnMut(u16, &[u8]) -> bool>| {
        let mut p = first;
        let mut n = 0;
        while p + 6 <= b.len() && n < 1 << 20 {
            let size = u32_at(b, p) as usize * 2;
            let func = u16_at(b, p + 4);
            if size < 6 || func == 0 {
                break;
            }
            if !f(func, &b[p + 6..p.saturating_add(size).min(b.len())]) {
                break;
            }
            p = p.saturating_add(size);
            n += 1;
        }
    };
    while p + 6 <= b.len() && n < 10_000 {
        let size = u32_at(b, p) as usize * 2;
        let f = u16_at(b, p + 4);
        if size < 6 || f == 0 {
            break;
        }
        let s = |i: usize| i16_at(b, p + 6 + i * 2) as f64;
        match f {
            0x020B => org = (s(1), s(0)),
            0x020C if s(0) != 0.0 && s(1) != 0.0 => window = Some((s(1), s(0))),
            // Text mapping set after the window: the window means nothing.
            0x0103 if u16_at(b, p + 6) == 1 => window = None,
            // Drawing starts: no window of its own.
            0x0213 | 0x0214 | 0x0325 | 0x0324 | 0x0538 | 0x041B | 0x061C | 0x0418 | 0x0817 | 0x0830 | 0x081A | 0x0521 | 0x0A32 | 0x0F43 | 0x0B41 | 0x0940 | 0x061D => break,
            _ => {}
        }
        p = p.saturating_add(size);
        n += 1;
    }
    if let Some(ext) = window {
        return Some((org, ext, true));
    }
    // The box its drawing covers.
    let (mut x0, mut y0, mut x1, mut y1) = (f64::MAX, f64::MAX, f64::MIN, f64::MIN);
    let mut add = |x: f64, y: f64| {
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    };
    records(Box::new(|f, r| {
        let s = |i: usize| i16_at(r, i * 2) as f64;
        let pts = |n: usize, at: usize, add: &mut dyn FnMut(f64, f64)| {
            for k in 0..n.min(1 << 16) {
                if r.len() < at + k * 4 + 4 {
                    break;
                }
                add(i16_at(r, at + k * 4) as f64, i16_at(r, at + k * 4 + 2) as f64);
            }
        };
        match f {
            0x0213 | 0x0214 => add(s(1), s(0)),
            0x0325 | 0x0324 => pts(u16_at(r, 0) as usize, 2, &mut add),
            0x0538 => {
                let k = u16_at(r, 0) as usize;
                let total: usize = (0..k.min(r.len() / 2)).map(|i| u16_at(r, 2 + i * 2) as usize).sum();
                pts(total, 2 + k * 2, &mut add);
            }
            0x041B | 0x0418 => {
                add(s(3), s(2));
                add(s(1), s(0));
            }
            0x061C => {
                add(s(5), s(4));
                add(s(3), s(2));
            }
            0x0817 | 0x0830 | 0x081A => {
                add(s(7), s(6));
                add(s(5), s(4));
            }
            0x0A32 => add(s(1), s(0)),
            0x0F43 => {
                add(s(10), s(9));
                add(s(10) + s(8), s(9) + s(7));
            }
            0x0B41 | 0x0940 => {
                let bare = r.len() + 6 == ((f >> 8) as usize + 3) * 2;
                let (dh, dw, dy, dx) = match (f, bare) {
                    (0x0B41, false) => (s(6), s(7), s(8), s(9)),
                    (0x0B41, true) => (s(7), s(8), s(9), s(10)),
                    (_, false) => (s(4), s(5), s(6), s(7)),
                    _ => (s(5), s(6), s(7), s(8)),
                };
                add(dx, dy);
                add(dx + dw, dy + dh);
            }
            0x061D => {
                add(s(5), s(4));
                add(s(5) + s(3), s(4) + s(2));
            }
            _ => {}
        }
        true
    }));
    (x1 > x0 && y1 > y0).then_some(((x0, y0), (x1 - x0, y1 - y0), false))
}

/// A picture as pixels: RGBA8, straight alpha, rows top to bottom.
#[derive(Clone, PartialEq, Eq)]
pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}

impl std::fmt::Debug for Rgba {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Rgba {}x{}", self.width, self.height)
    }
}

/// Draws the picture at `width` × `height` pixels (a bitmap: at its own
/// size, the caller scales it). `None` when it isn't one of these formats,
/// is damaged past drawing, or is too big.
#[cfg(feature = "render")]
pub fn render(b: &[u8], width: u32, height: u32) -> Option<Rgba> {
    if b.len() > MAX_BYTES {
        return None;
    }
    match sniff(b)? {
        Kind::Bmp => {
            let d = dib::decode_file(b)?;
            Some(Rgba { width: d.width, height: d.height, data: d.rgba })
        }
        k => {
            if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE || width as u64 * height as u64 > MAX_PIXELS {
                return None;
            }
            let pix = match k {
                Kind::Emf => emf::play(b, width, height)?,
                _ => wmf::play(b, width, height)?,
            };
            Some(gdi::to_rgba(pix))
        }
    }
}
