//! Cell styles (docs/spec/officina.md §8.1): a style is a number format, a font, a
//! fill, alignment and borders. Styles are interned per workbook (a [`StyleId`] per
//! distinct combination, 0 the default) and stored run-length per column
//! ([`StyleRuns`]), so a column formatted from top to bottom is one run at any height.

use std::collections::HashMap;
use std::sync::Arc;

use crate::numfmt::NumFmt;
use officina_core::wire::{Reader, WireError, Writer};

pub type StyleId = u32;

/// Most distinct styles in a workbook (Excel's limit is 64,000).
pub const MAX_STYLES: usize = 65_536;
/// Longest number format code, font name.
pub const MAX_CODE: usize = 255;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum HAlign {
    /// Text left, numbers right, booleans and errors centred.
    #[default]
    General,
    Left,
    Center,
    Right,
    Fill,
    Justify,
    CenterAcross,
    Distributed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum VAlign {
    Top,
    Center,
    #[default]
    Bottom,
    Justify,
    Distributed,
}

/// A border line's look (ECMA-376 §18.18.3); `None` is no line.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Line {
    #[default]
    None,
    Thin,
    Medium,
    Dashed,
    Dotted,
    Thick,
    Double,
    Hair,
    MediumDashed,
    DashDot,
    MediumDashDot,
    DashDotDot,
    MediumDashDotDot,
    SlantDashDot,
}

impl Line {
    pub const ALL: [Line; 14] = [
        Line::None,
        Line::Thin,
        Line::Medium,
        Line::Dashed,
        Line::Dotted,
        Line::Thick,
        Line::Double,
        Line::Hair,
        Line::MediumDashed,
        Line::DashDot,
        Line::MediumDashDot,
        Line::DashDotDot,
        Line::MediumDashDotDot,
        Line::SlantDashDot,
    ];
    /// The OOXML name.
    pub fn name(self) -> &'static str {
        ["none", "thin", "medium", "dashed", "dotted", "thick", "double", "hair", "mediumDashed", "dashDot", "mediumDashDot", "dashDotDot", "mediumDashDotDot", "slantDashDot"][self as usize]
    }
    pub fn from_name(s: &str) -> Option<Line> {
        Line::ALL.iter().copied().find(|l| l.name() == s)
    }
    /// Width on screen, in points.
    pub fn width(self) -> f32 {
        match self {
            Line::None => 0.0,
            Line::Hair | Line::Thin | Line::Dashed | Line::Dotted | Line::DashDot | Line::DashDotDot => 1.0,
            Line::Medium | Line::MediumDashed | Line::MediumDashDot | Line::MediumDashDotDot | Line::SlantDashDot => 2.0,
            Line::Thick | Line::Double => 3.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Edge {
    pub line: Line,
    /// 0xRRGGBB; `None` is automatic (black).
    pub color: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Font {
    /// `None` is the workbook's default font.
    pub name: Option<Arc<str>>,
    /// Size in twentieths of a point (220 = 11 pt); `None` is the default.
    pub size: Option<u16>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strike: bool,
    /// 0xRRGGBB; `None` is automatic.
    pub color: Option<u32>,
}

impl Default for Font {
    fn default() -> Self {
        Font { name: None, size: None, bold: false, italic: false, underline: false, strike: false, color: None }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Style {
    /// The number format code ("General" by default).
    pub num_fmt: Arc<str>,
    pub font: Font,
    /// Solid background, 0xRRGGBB.
    pub fill: Option<u32>,
    pub h_align: HAlign,
    pub v_align: VAlign,
    pub wrap: bool,
    pub indent: u8,
    /// Text rotation in degrees, -90..=90 (255 is vertical stacked, as in OOXML).
    pub rotation: i16,
    /// Left, right, top, bottom.
    pub borders: [Edge; 4],
}

impl Default for Style {
    fn default() -> Self {
        Style {
            num_fmt: Arc::from("General"),
            font: Font::default(),
            fill: None,
            h_align: HAlign::General,
            v_align: VAlign::Bottom,
            wrap: false,
            indent: 0,
            rotation: 0,
            borders: [Edge::default(); 4],
        }
    }
}

impl Style {
    pub fn with_num_fmt(mut self, code: &str) -> Self {
        self.num_fmt = Arc::from(code);
        self
    }

    pub fn encode(&self, w: &mut Writer) {
        w.str(&self.num_fmt);
        let f = &self.font;
        w.str(f.name.as_deref().unwrap_or(""));
        w.var(f.size.unwrap_or(0) as u64);
        w.u8(f.bold as u8 | (f.italic as u8) << 1 | (f.underline as u8) << 2 | (f.strike as u8) << 3 | (self.wrap as u8) << 4);
        color_out(w, f.color);
        color_out(w, self.fill);
        w.u8(self.h_align as u8);
        w.u8(self.v_align as u8);
        w.u8(self.indent);
        w.ivar(self.rotation as i64);
        for e in &self.borders {
            w.u8(e.line as u8);
            color_out(w, e.color);
        }
    }

    pub fn decode(r: &mut Reader) -> Result<Style, WireError> {
        let num_fmt = Arc::from(r.str("number format", MAX_CODE as u64)?);
        let name = r.str("font name", MAX_CODE as u64)?;
        let size = r.len("font size", 409 * 20)? as u16;
        let flags = r.u8()?;
        let color = color_in(r)?;
        let fill = color_in(r)?;
        const H: [HAlign; 8] = [HAlign::General, HAlign::Left, HAlign::Center, HAlign::Right, HAlign::Fill, HAlign::Justify, HAlign::CenterAcross, HAlign::Distributed];
        const V: [VAlign; 5] = [VAlign::Top, VAlign::Center, VAlign::Bottom, VAlign::Justify, VAlign::Distributed];
        let h_align = *H.get(r.u8()? as usize).ok_or(WireError::Invalid("alignment"))?;
        let v_align = *V.get(r.u8()? as usize).ok_or(WireError::Invalid("alignment"))?;
        let indent = r.u8()?;
        let rotation = r.ivar()?;
        if !(-90..=255).contains(&rotation) {
            return Err(WireError::Invalid("rotation"));
        }
        let mut borders = [Edge::default(); 4];
        for e in borders.iter_mut() {
            e.line = *Line::ALL.get(r.u8()? as usize).ok_or(WireError::Invalid("border"))?;
            e.color = color_in(r)?;
        }
        Ok(Style {
            num_fmt,
            font: Font {
                name: if name.is_empty() { None } else { Some(Arc::from(name)) },
                size: if size == 0 { None } else { Some(size) },
                bold: flags & 1 != 0,
                italic: flags & 2 != 0,
                underline: flags & 4 != 0,
                strike: flags & 8 != 0,
                color,
            },
            fill,
            h_align,
            v_align,
            wrap: flags & 16 != 0,
            indent,
            rotation: rotation as i16,
            borders,
        })
    }
}

fn color_out(w: &mut Writer, c: Option<u32>) {
    match c {
        None => w.u8(0),
        Some(c) => {
            w.u8(1);
            w.u32(c & 0xFF_FFFF);
        }
    }
}

fn color_in(r: &mut Reader) -> Result<Option<u32>, WireError> {
    Ok(match r.u8()? {
        0 => None,
        1 => Some(r.u32()? & 0xFF_FFFF),
        _ => return Err(WireError::Invalid("colour")),
    })
}

/// The workbook's styles, interned. Id 0 is the default style.
#[derive(Debug, Clone)]
pub struct StyleTable {
    styles: Vec<Arc<Style>>,
    formats: Vec<Arc<NumFmt>>,
    index: HashMap<Arc<Style>, StyleId>,
}

impl Default for StyleTable {
    fn default() -> Self {
        Self::new()
    }
}

impl StyleTable {
    pub fn new() -> Self {
        let mut t = StyleTable { styles: Vec::new(), formats: Vec::new(), index: HashMap::new() };
        t.intern(Style::default());
        t
    }

    /// The id of `s`, adding it if new. Past [`MAX_STYLES`] distinct styles, the
    /// default (the caller's report says so).
    pub fn intern(&mut self, s: Style) -> StyleId {
        if let Some(&id) = self.index.get(&s) {
            return id;
        }
        if self.styles.len() >= MAX_STYLES {
            return 0;
        }
        let id = self.styles.len() as StyleId;
        let fmt = match self.formats.iter().find(|f| f.code() == &*s.num_fmt) {
            Some(f) => f.clone(),
            None => Arc::new(NumFmt::parse(&s.num_fmt)),
        };
        let s = Arc::new(s);
        self.styles.push(s.clone());
        self.formats.push(fmt);
        self.index.insert(s, id);
        id
    }

    pub fn get(&self, id: StyleId) -> &Style {
        self.styles.get(id as usize).unwrap_or(&self.styles[0])
    }

    pub fn format(&self, id: StyleId) -> &NumFmt {
        self.formats.get(id as usize).unwrap_or(&self.formats[0])
    }

    pub fn len(&self) -> usize {
        self.styles.len()
    }

    pub fn is_empty(&self) -> bool {
        false
    }

    pub fn iter(&self) -> impl Iterator<Item = (StyleId, &Style)> {
        self.styles.iter().enumerate().map(|(i, s)| (i as StyleId, &**s))
    }
}

/// One column's styles: sorted runs `(first row, style)`, each lasting until the next;
/// rows before the first run have style 0. Adjacent runs never share a style.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StyleRuns {
    runs: Vec<(u32, StyleId)>,
}

impl StyleRuns {
    pub fn get(&self, row: u32) -> StyleId {
        match self.runs.partition_point(|&(r, _)| r <= row) {
            0 => 0,
            i => self.runs[i - 1].1,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.runs.iter().all(|&(_, s)| s == 0)
    }

    /// Runs as `(r0, r1, style)` covering `r0..=r1` (style 0 included).
    pub fn spans(&self, r0: u32, r1: u32) -> Vec<(u32, u32, StyleId)> {
        let mut out = Vec::new();
        let mut at = r0;
        let mut i = self.runs.partition_point(|&(r, _)| r <= r0);
        let mut cur = if i == 0 { 0 } else { self.runs[i - 1].1 };
        while at <= r1 {
            let next = self.runs.get(i).map(|&(r, _)| r).filter(|&r| r <= r1);
            let end = next.map(|r| r - 1).unwrap_or(r1);
            out.push((at, end, cur));
            match next {
                Some(r) => {
                    cur = self.runs[i].1;
                    at = r;
                    i += 1;
                }
                None => break,
            }
        }
        out
    }

    /// Runs with a style other than 0, as `(r0, r1, style)`; `last` is the end of the
    /// final open run.
    pub fn styled(&self, last: u32) -> Vec<(u32, u32, StyleId)> {
        let mut out = Vec::new();
        for (k, &(r, s)) in self.runs.iter().enumerate() {
            if s == 0 {
                continue;
            }
            let end = self.runs.get(k + 1).map(|&(n, _)| n - 1).unwrap_or(last);
            out.push((r, end, s));
        }
        out
    }

    /// Sets rows `r0..=r1` to `s`.
    pub fn set(&mut self, r0: u32, r1: u32, s: StyleId) {
        if r0 > r1 {
            return;
        }
        let a = self.runs.partition_point(|&(r, _)| r < r0);
        let resume = if r1 == u32::MAX { None } else { Some((r1 + 1, self.get(r1 + 1))) };
        let b = if r1 == u32::MAX { self.runs.len() } else { self.runs.partition_point(|&(r, _)| r <= r1 + 1) };
        let mut mid = vec![(r0, s)];
        if let Some(x) = resume {
            mid.push(x);
        }
        self.runs.splice(a..b, mid);
        // Merge neighbours with the same style; drop a leading run of 0.
        let mut out: Vec<(u32, StyleId)> = Vec::with_capacity(self.runs.len());
        for &(r, st) in &self.runs {
            let prev = out.last().map(|&(_, p)| p).unwrap_or(0);
            if prev == st {
                continue;
            }
            out.push((r, st));
        }
        self.runs = out;
    }

    pub fn runs(&self) -> &[(u32, StyleId)] {
        &self.runs
    }

    pub fn from_runs(runs: Vec<(u32, StyleId)>) -> Self {
        let mut s = StyleRuns { runs: Vec::new() };
        for (r, st) in runs {
            let prev = s.runs.last().map(|&(_, p)| p).unwrap_or(0);
            if prev != st && s.runs.last().is_none_or(|&(p, _)| p < r) {
                s.runs.push((r, st));
            }
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs() {
        let mut c = StyleRuns::default();
        assert_eq!(c.get(5), 0);
        c.set(10, 19, 3);
        assert_eq!((c.get(9), c.get(10), c.get(19), c.get(20)), (0, 3, 3, 0));
        c.set(15, 30, 4);
        assert_eq!((c.get(14), c.get(15), c.get(30), c.get(31)), (3, 4, 4, 0));
        assert_eq!(c.runs(), &[(10, 3), (15, 4), (31, 0)]);
        c.set(0, u32::MAX, 0);
        assert!(c.runs().is_empty());
        c.set(0, 1_048_575, 7);
        assert_eq!(c.runs(), &[(0, 7), (1_048_576, 0)]);
        c.set(100, 200, 7);
        assert_eq!(c.runs(), &[(0, 7), (1_048_576, 0)]);
        c.set(100, 200, 2);
        assert_eq!(c.spans(99, 201), vec![(99, 99, 7), (100, 200, 2), (201, 201, 7)]);
        assert_eq!(c.styled(2_000_000), vec![(0, 99, 7), (100, 200, 2), (201, 1_048_575, 7)]);
        // Random sets against a plain array.
        let mut plain = vec![0u32; 300];
        let mut c = StyleRuns::default();
        let mut x: u64 = 7;
        for _ in 0..2000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let a = (x >> 33) as u32 % 300;
            let b = (a + (x >> 20) as u32 % 40).min(299);
            let s = (x >> 50) as u32 % 4;
            c.set(a, b, s);
            for v in &mut plain[a as usize..=b as usize] {
                *v = s;
            }
            for (r, &v) in plain.iter().enumerate() {
                assert_eq!(c.get(r as u32), v);
            }
            assert!(c.runs().windows(2).all(|w| w[0].0 < w[1].0 && w[0].1 != w[1].1));
        }
    }

    #[test]
    fn table_interns() {
        let mut t = StyleTable::new();
        let a = t.intern(Style::default().with_num_fmt("0.00"));
        let b = t.intern(Style::default().with_num_fmt("0.00"));
        assert_eq!(a, b);
        assert_eq!(t.intern(Style::default()), 0);
        assert_eq!(t.format(a).code(), "0.00");
        let mut s = Style::default();
        s.font.bold = true;
        s.fill = Some(0xFFEE00);
        s.borders[3] = Edge { line: Line::Double, color: Some(0x123456) };
        s.rotation = -45;
        let mut w = Writer::new();
        s.encode(&mut w);
        let bytes = w.into_bytes();
        assert_eq!(Style::decode(&mut Reader::new(&bytes)).unwrap(), s);
    }
}
