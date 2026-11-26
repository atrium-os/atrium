//! Charts (docs/spec/officina.md §9.2): what a chart shows and where it sits, fed from
//! sheet ranges. Drawing is `officina-draw`'s; this is the model the files and the
//! journal carry.

use std::sync::Arc;

use crate::addr::{CellAddr, Rect, MAX_COLS, MAX_ROWS};
use crate::book::Workbook;
use crate::value::Value;
use officina_core::wire::{Reader, WireError, Writer};

/// Charts per workbook, series per chart, points drawn per series, at most.
pub const MAX_CHARTS: usize = 4096;
pub const MAX_SERIES: usize = 255;
pub const MAX_TITLE: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ChartKind {
    /// Vertical bars.
    Column,
    /// Horizontal bars.
    Bar,
    Line,
    Area,
    Scatter,
    Pie,
    Doughnut,
    /// Each category a spoke from the centre, each series a polygon.
    Radar,
    /// Scatter with a third value, each point's size ([`SeriesLook::sizes`]).
    Bubble,
    /// High-low-close (three series) or open-high-low-close (four).
    Stock,
    /// Each value a bar floating from the running total; points in
    /// [`SeriesLook::totals`] stand on the axis as totals.
    Waterfall,
    /// The first series' values counted into bins ([`SeriesLook::bins`]).
    Histogram,
    /// Each series' quartiles, whiskers to the last value within 1.5 IQR,
    /// its mean marked and outliers as dots.
    BoxWhisker,
}

impl ChartKind {
    pub const ALL: [ChartKind; 13] = [
        ChartKind::Column, ChartKind::Bar, ChartKind::Line, ChartKind::Area, ChartKind::Scatter, ChartKind::Pie, ChartKind::Doughnut,
        ChartKind::Radar, ChartKind::Bubble, ChartKind::Stock, ChartKind::Waterfall, ChartKind::Histogram, ChartKind::BoxWhisker,
    ];
    fn code(self) -> u8 {
        ChartKind::ALL.iter().position(|k| *k == self).unwrap() as u8
    }
    /// Pie and doughnut: one series around a circle, no axes.
    pub fn round(self) -> bool {
        matches!(self, ChartKind::Pie | ChartKind::Doughnut)
    }
    /// Kinds only the chart as a whole can be: no combo with others.
    pub fn whole(self) -> bool {
        matches!(self, ChartKind::Pie | ChartKind::Doughnut | ChartKind::Radar | ChartKind::Stock | ChartKind::Waterfall | ChartKind::Histogram | ChartKind::BoxWhisker)
    }
    /// Whether the x values are numbers (scatter and bubble).
    pub fn xy(self) -> bool {
        matches!(self, ChartKind::Scatter | ChartKind::Bubble)
    }
}

/// A trendline's fit (Excel's Format Trendline).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TrendKind {
    Linear,
    Exponential,
    Logarithmic,
    /// Order 2 to 6.
    Polynomial(u8),
    Power,
    /// Period 2 to 255.
    MovingAverage(u8),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Trendline {
    pub kind: TrendKind,
    /// Show the equation and R² on the chart.
    pub equation: bool,
    pub r_squared: bool,
    /// Periods to extend it forward and backward, 0 to 10⁶.
    pub forward: f64,
    pub backward: f64,
}

/// Error bars on the values (Excel's Y error bars, both directions).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ErrorBars {
    Fixed(f64),
    /// Of each value.
    Percent(f64),
    /// That many standard deviations of the series, around its mean.
    StdDev(f64),
    StdErr,
}

/// A histogram's bins: Excel's automatic (Scott's rule), a count or a width.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Bins {
    Count(u32),
    Width(f64),
}

/// The most waterfall total points a series marks.
pub const MAX_TOTALS: usize = 4096;

/// What a series shows beyond its values.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SeriesLook {
    pub trend: Option<Trendline>,
    /// Data labels: each point's value.
    pub labels: bool,
    /// Plotted against a second value axis on the right.
    pub secondary: bool,
    pub errors: Option<ErrorBars>,
    /// A bubble chart's sizes: one row or column.
    pub sizes: Option<Rect>,
    /// Waterfall points (0-based) that are totals, ascending.
    pub totals: Vec<u32>,
    pub bins: Option<Bins>,
}

impl SeriesLook {
    /// Whether every part is in range (what the wire format accepts): polynomial
    /// order 2 to 6, period 2 to 255, extensions 0 to 10⁶, error amounts finite and
    /// not negative, totals ascending and on the sheet, 1 to 10 000 bins or a
    /// positive width.
    pub fn valid(&self) -> bool {
        let trend = self.trend.is_none_or(|t| {
            let kind = match t.kind {
                TrendKind::Polynomial(n) => (2..=6).contains(&n),
                TrendKind::MovingAverage(n) => n >= 2,
                _ => true,
            };
            kind && (0.0..=1e6).contains(&t.forward) && (0.0..=1e6).contains(&t.backward)
        });
        let errors = self.errors.is_none_or(|e| match e {
            ErrorBars::Fixed(v) | ErrorBars::Percent(v) | ErrorBars::StdDev(v) => v.is_finite() && v >= 0.0,
            ErrorBars::StdErr => true,
        });
        let totals = self.totals.len() <= MAX_TOTALS && self.totals.windows(2).all(|w| w[0] < w[1]) && self.totals.last().is_none_or(|&t| t < MAX_ROWS);
        let bins = self.bins.is_none_or(|b| match b {
            Bins::Count(n) => (1..=10_000).contains(&n),
            Bins::Width(x) => x.is_finite() && x > 0.0,
        });
        trend && errors && totals && bins
    }

    fn plain(&self) -> bool {
        *self == SeriesLook::default()
    }

    fn encode(&self, w: &mut Writer) {
        let flags = u8::from(self.trend.is_some())
            | u8::from(self.labels) << 1
            | u8::from(self.secondary) << 2
            | u8::from(self.errors.is_some()) << 3
            | u8::from(self.sizes.is_some()) << 4
            | u8::from(!self.totals.is_empty()) << 5
            | u8::from(self.bins.is_some()) << 6;
        w.u8(flags);
        if let Some(t) = &self.trend {
            let (k, n) = match t.kind {
                TrendKind::Linear => (0, 0),
                TrendKind::Exponential => (1, 0),
                TrendKind::Logarithmic => (2, 0),
                TrendKind::Polynomial(n) => (3, n),
                TrendKind::Power => (4, 0),
                TrendKind::MovingAverage(n) => (5, n),
            };
            w.u8(k);
            w.u8(n);
            w.u8(u8::from(t.equation) | u8::from(t.r_squared) << 1);
            w.f64(t.forward);
            w.f64(t.backward);
        }
        if let Some(e) = &self.errors {
            let (k, v) = match *e {
                ErrorBars::Fixed(v) => (0, v),
                ErrorBars::Percent(v) => (1, v),
                ErrorBars::StdDev(v) => (2, v),
                ErrorBars::StdErr => (3, 0.0),
            };
            w.u8(k);
            w.f64(v);
        }
        if let Some(r) = &self.sizes {
            rect(w, r);
        }
        if !self.totals.is_empty() {
            w.var(self.totals.len() as u64);
            for t in &self.totals {
                w.var(u64::from(*t));
            }
        }
        if let Some(b) = &self.bins {
            match *b {
                Bins::Count(n) => {
                    w.u8(0);
                    w.var(u64::from(n));
                }
                Bins::Width(x) => {
                    w.u8(1);
                    w.f64(x);
                }
            }
        }
    }

    fn decode(r: &mut Reader) -> Result<SeriesLook, WireError> {
        let flags = r.u8()?;
        if flags & 0x80 != 0 {
            return Err(WireError::Invalid("series look"));
        }
        let mut look = SeriesLook { labels: flags & 2 != 0, secondary: flags & 4 != 0, ..SeriesLook::default() };
        if flags & 1 != 0 {
            let (k, n, f) = (r.u8()?, r.u8()?, r.u8()?);
            let kind = match (k, n) {
                (0, _) => TrendKind::Linear,
                (1, _) => TrendKind::Exponential,
                (2, _) => TrendKind::Logarithmic,
                (3, 2..=6) => TrendKind::Polynomial(n),
                (4, _) => TrendKind::Power,
                (5, 2..=255) => TrendKind::MovingAverage(n),
                _ => return Err(WireError::Invalid("trendline")),
            };
            let (forward, backward) = (r.f64()?, r.f64()?);
            if !(0.0..=1e6).contains(&forward) || !(0.0..=1e6).contains(&backward) {
                return Err(WireError::Invalid("trendline"));
            }
            look.trend = Some(Trendline { kind, equation: f & 1 != 0, r_squared: f & 2 != 0, forward, backward });
        }
        if flags & 8 != 0 {
            let (k, v) = (r.u8()?, r.f64()?);
            if !v.is_finite() || v < 0.0 {
                return Err(WireError::Invalid("error bars"));
            }
            look.errors = Some(match k {
                0 => ErrorBars::Fixed(v),
                1 => ErrorBars::Percent(v),
                2 => ErrorBars::StdDev(v),
                3 => ErrorBars::StdErr,
                _ => return Err(WireError::Invalid("error bars")),
            });
        }
        if flags & 16 != 0 {
            look.sizes = Some(read_rect(r)?);
        }
        if flags & 32 != 0 {
            let n = r.count("waterfall totals", MAX_TOTALS as u64, 1)?;
            for _ in 0..n {
                let t = r.var()?;
                if t >= u64::from(MAX_ROWS) || look.totals.last().is_some_and(|&l| u64::from(l) >= t) {
                    return Err(WireError::Invalid("waterfall totals"));
                }
                look.totals.push(t as u32);
            }
        }
        if flags & 64 != 0 {
            look.bins = Some(match r.u8()? {
                0 => {
                    let n = r.var()?;
                    if !(1..=10_000).contains(&n) {
                        return Err(WireError::Invalid("bins"));
                    }
                    Bins::Count(n as u32)
                }
                1 => {
                    let x = r.f64()?;
                    if !(x.is_finite() && x > 0.0) {
                        return Err(WireError::Invalid("bins"));
                    }
                    Bins::Width(x)
                }
                _ => return Err(WireError::Invalid("bins")),
            });
        }
        Ok(look)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Grouping {
    /// Side by side (bars) or on their own values (lines, areas).
    Standard,
    Stacked,
    /// Stacked to 100 %.
    Percent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LegendPos {
    Right,
    Left,
    Top,
    Bottom,
}

/// Text a chart shows: a cell's (followed as it changes) or typed.
#[derive(Debug, Clone, PartialEq)]
pub enum ChartText {
    Cell(CellAddr),
    Lit(Arc<str>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    pub name: Option<ChartText>,
    /// Category labels (x values for scatter): one row or column.
    pub cats: Option<Rect>,
    /// The values: one row or column.
    pub vals: Rect,
    /// A combo chart's other kind for this series (`None`: the chart's).
    pub kind: Option<ChartKind>,
    /// Trendline, data labels, secondary axis, error bars and the like.
    pub look: SeriesLook,
}

/// Where a chart sits: its top-left and bottom-right corners, each a cell plus an
/// offset into it in points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Anchor {
    pub sheet: u16,
    pub from: (u32, u32, f32, f32),
    pub to: (u32, u32, f32, f32),
}

/// An embedded raster drawing, anchored like a chart. The bytes stay encoded.
#[derive(Debug, Clone, PartialEq)]
pub struct Picture {
    pub anchor: Anchor,
    /// Embedded image or the workbook's cached copy of a linked image.
    pub bytes: Arc<[u8]>,
    /// External relationship target, kept as inert text.
    pub link: Option<Arc<str>>,
    /// Source insets in thousandths of a percent: left, top, right, bottom.
    pub crop: [i32; 4],
    /// Clockwise rotation about the anchor's center, in 1/60,000 degrees.
    pub rotation: i32,
    /// Alt Text: what the picture shows, for a screen reader (see
    /// [`clean_alt`]).
    pub alt: Option<Arc<str>>,
}

pub const MAX_PICTURE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_PICTURES: usize = 256;
// Leave room for anchors and lengths in a journal record.
pub(crate) const MAX_PICTURE_TOTAL: usize = (64 << 20) - (MAX_PICTURES * 64);

// Picture records originally started with an unflagged sheet index.
const PICTURE_TRANSFORM: u16 = 1 << 15;
const PICTURE_LINK: u16 = 1 << 14;
const PICTURE_ALT: u16 = 1 << 13;
pub const MAX_PICTURE_LINK: usize = 4096;

impl Picture {
    fn has_transform(&self) -> bool {
        self.crop != [0; 4] || self.rotation != 0
    }

    // There is already room for the anchor and image length in the budget.
    fn extra_bytes(&self) -> usize {
        let transform = if self.has_transform() { 16 } else { 0 };
        transform + self.link.as_ref().map_or(0, |link| link.len() + 2) + self.alt.as_ref().map_or(0, |alt| alt.len() + 2)
    }

    pub(crate) fn collection_fits(pictures: &[Self]) -> bool {
        pictures.len() <= MAX_PICTURES
            && pictures.iter().map(|picture| picture.bytes.len() + picture.extra_bytes()).sum::<usize>() <= MAX_PICTURE_TOTAL
    }

    /// The extension used when saving the embedded bytes to XLSX.
    pub fn extension(bytes: &[u8]) -> Option<&'static str> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some("png")
        } else if bytes.starts_with(b"\xff\xd8\xff") {
            Some("jpg")
        } else {
            None
        }
    }

    pub(crate) fn encode(&self, w: &mut Writer) {
        let transformed = self.has_transform();
        let transform_flag = if transformed { PICTURE_TRANSFORM } else { 0 };
        let link_flag = if self.link.is_some() { PICTURE_LINK } else { 0 };
        let alt = self.alt.as_deref().filter(|a| !a.is_empty());
        let alt_flag = if alt.is_some() { PICTURE_ALT } else { 0 };
        w.u16(self.anchor.sheet | transform_flag | link_flag | alt_flag);
        for (row, col, x, y) in [self.anchor.from, self.anchor.to] {
            w.u32(row);
            w.u32(col);
            w.f64(f64::from(x));
            w.f64(f64::from(y));
        }
        if transformed {
            for inset in self.crop {
                w.ivar(i64::from(inset));
            }
            w.ivar(i64::from(self.rotation));
        }
        if let Some(link) = &self.link {
            w.str(link);
        }
        if let Some(alt) = alt {
            w.str(alt);
        }
        w.bytes(&self.bytes);
    }

    pub(crate) fn decode(r: &mut Reader, limit: usize) -> Result<Self, WireError> {
        let flags = r.u16()?;
        let sheet = flags & !(PICTURE_TRANSFORM | PICTURE_LINK | PICTURE_ALT);
        let mut corner = || -> Result<_, WireError> {
            Ok((r.u32()?, r.u32()?, r.f64()? as f32, r.f64()? as f32))
        };
        let anchor = Anchor { sheet, from: corner()?, to: corner()? };
        let (mut crop, mut rotation) = ([0; 4], 0);
        if flags & PICTURE_TRANSFORM != 0 {
            let mut coordinate = || i32::try_from(r.ivar()?).map_err(|_| WireError::Invalid("picture transform"));
            for inset in &mut crop {
                *inset = coordinate()?;
            }
            rotation = coordinate()?;
        }
        let link = if flags & PICTURE_LINK != 0 {
            Some(Arc::from(r.str("picture link", MAX_PICTURE_LINK as u64)?))
        } else {
            None
        };
        let alt = if flags & PICTURE_ALT != 0 {
            let a = r.str("alt text", MAX_ALT as u64)?;
            if a.is_empty() || a.chars().any(|c| c.is_control() && c != '\n') {
                return Err(WireError::Invalid("alt text"));
            }
            Some(Arc::from(a))
        } else {
            None
        };
        let bytes = Arc::from(r.bytes("picture", limit.min(MAX_PICTURE_BYTES) as u64)?);
        Ok(Self { anchor, bytes, link, crop, rotation, alt })
    }
}

impl Workbook {
    pub fn pictures(&self) -> &[Picture] {
        &self.pictures
    }

    /// Whether there is room for another embedded picture of this size.
    pub fn can_add_picture(&self, size: usize) -> bool {
        size > 0 && self.has_picture_room(size, 0)
    }

    fn has_picture_room(&self, size: usize, extra: usize) -> bool {
        self.pictures.len() < MAX_PICTURES
            && size <= MAX_PICTURE_BYTES
            && self.pictures.iter().map(|p| p.bytes.len() + p.extra_bytes()).sum::<usize>()
                <= MAX_PICTURE_TOTAL.saturating_sub(size.saturating_add(extra))
    }

    pub fn add_picture(&mut self, picture: Picture) -> Result<(), &'static str> {
        let a = picture.anchor;
        if picture.link.as_ref().is_some_and(|link| {
                link.is_empty() || link.len() > MAX_PICTURE_LINK || link.chars().any(char::is_control)
            })
            || !self.has_picture_room(picture.bytes.len(), picture.extra_bytes())
            || if picture.bytes.is_empty() { picture.link.is_none() } else { Picture::extension(&picture.bytes).is_none() }
            || picture.crop.iter().any(|inset| !(-1_000_000..=1_000_000).contains(inset))
            || a.sheet as usize >= self.sheet_count()
            || [a.from, a.to].iter().any(|&(r, c, x, y)| {
                r >= MAX_ROWS || c >= MAX_COLS || !(0.0..=1e6).contains(&x) || !(0.0..=1e6).contains(&y)
            })
        {
            return Err("invalid picture or workbook picture limits exceeded");
        }
        self.pictures.push(picture);
        Ok(())
    }

    pub(crate) fn replace_pictures(&mut self, pictures: Vec<Picture>) -> Vec<Picture> {
        let old = std::mem::take(&mut self.pictures);
        for picture in pictures {
            if self.add_picture(picture).is_err() {
                self.pictures.clone_from(&old);
                return old;
            }
        }
        old
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Chart {
    pub kind: ChartKind,
    pub grouping: Grouping,
    pub title: Option<ChartText>,
    pub series: Vec<Series>,
    pub legend: Option<LegendPos>,
    pub x_title: Option<Arc<str>>,
    pub y_title: Option<Arc<str>>,
    pub anchor: Anchor,
    /// Alt Text: what the chart shows, for a screen reader (Excel's
    /// `descr`), at most [`MAX_ALT`] bytes.
    pub alt: Option<Arc<str>>,
}

/// The longest alt text, in bytes.
pub const MAX_ALT: usize = 4096;

/// Alt text as kept: trimmed, control characters other than line breaks
/// dropped, cut to [`MAX_ALT`] bytes on a character boundary; `None` when
/// nothing is left.
pub fn clean_alt(s: &str) -> Option<Arc<str>> {
    let clean: String = s.trim().chars().filter(|c| !c.is_control() || *c == '\n').collect();
    let mut end = clean.len().min(MAX_ALT);
    while !clean.is_char_boundary(end) {
        end -= 1;
    }
    let cut = clean[..end].trim();
    (!cut.is_empty()).then(|| Arc::from(cut))
}

/// A series' values as the chart draws them.
#[derive(Debug, Clone, PartialEq)]
pub struct SeriesData {
    pub name: String,
    pub cats: Vec<String>,
    /// Scatter x values (numbers of `cats`); empty otherwise.
    pub xs: Vec<Option<f64>>,
    /// `None`: an empty or non-number cell (a gap).
    pub vals: Vec<Option<f64>>,
    pub kind: ChartKind,
    /// A bubble series' sizes, as `vals`; empty otherwise.
    pub sizes: Vec<Option<f64>>,
    pub look: SeriesLook,
}

/// The cells of a one-row-or-column range, in order, at most `max`.
/// The cells of `r` in rows the sheet shows (as Excel charts do: rows a filter or
/// the user hid are left out), at most `max`.
fn cells(wb: &Workbook, r: &Rect, max: usize) -> Vec<CellAddr> {
    let mut out = Vec::new();
    let sh = wb.sheet(r.sheet);
    for row in r.r0..=r.r1 {
        if sh.row_hidden(row) {
            continue;
        }
        for col in r.c0..=r.c1 {
            if out.len() >= max {
                return out;
            }
            if !sh.outline(crate::shift::Axis::Cols).hides(col) {
                out.push(CellAddr::new(r.sheet, row, col));
            }
        }
    }
    out
}

fn ok_rect(wb: &Workbook, r: &Rect) -> bool {
    (r.sheet as usize) < wb.sheet_count() && r.r0 <= r.r1 && r.c0 <= r.c1 && r.r1 < MAX_ROWS && r.c1 < MAX_COLS
}

impl Chart {
    /// The text of a title or name.
    pub fn text(wb: &Workbook, t: &ChartText) -> String {
        match t {
            ChartText::Lit(s) => s.to_string(),
            ChartText::Cell(a) if (a.sheet as usize) < wb.sheet_count() => wb.formatted(*a).text,
            ChartText::Cell(_) => String::new(),
        }
    }

    /// Every series' values now, each at most `max` points (the caller decimates
    /// for drawing).
    pub fn data(&self, wb: &Workbook, max: usize) -> Vec<SeriesData> {
        self.series
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let vals: Vec<Option<f64>> = if ok_rect(wb, &s.vals) {
                    cells(wb, &s.vals, max).into_iter().map(|a| if let Value::Num(x) = wb.get(a) { Some(x) } else { None }).collect()
                } else {
                    Vec::new()
                };
                let cat_cells = s.cats.filter(|r| ok_rect(wb, r)).map(|r| cells(wb, &r, max)).unwrap_or_default();
                let cats: Vec<String> = cat_cells.iter().map(|a| wb.formatted(*a).text).collect();
                let kind = s.kind.unwrap_or(self.kind);
                let xs = if kind.xy() { cat_cells.iter().map(|a| if let Value::Num(x) = wb.get(*a) { Some(x) } else { None }).collect() } else { Vec::new() };
                let name = s.name.as_ref().map(|t| Chart::text(wb, t)).unwrap_or_else(|| format!("Series{}", i + 1));
                let sizes = match s.look.sizes.filter(|r| ok_rect(wb, r)) {
                    Some(r) if kind == ChartKind::Bubble => cells(wb, &r, max).into_iter().map(|a| if let Value::Num(x) = wb.get(a) { Some(x) } else { None }).collect(),
                    _ => Vec::new(),
                };
                SeriesData { name, cats, xs, vals, kind, sizes, look: s.look.clone() }
            })
            .collect()
    }

    /// Changes whenever a cell the chart reads may have changed (a hash of its
    /// columns' versions), so a drawing of it can be kept until then.
    pub fn data_version(&self, wb: &Workbook) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        // Hidden rows leave the chart: their state is part of its data.
        let mut sheets: Vec<u16> = self.series.iter().flat_map(|s| [Some(s.vals), s.cats, s.look.sizes].into_iter().flatten())
            .map(|r| r.sheet).filter(|&s| (s as usize) < wb.sheet_count()).collect();
        sheets.sort_unstable();
        sheets.dedup();
        for s in sheets {
            let sheet = wb.sheet(s);
            sheet.hidden.hash(&mut h);
            sheet.table_hidden().hash(&mut h);
            for axis in [crate::shift::Axis::Rows, crate::shift::Axis::Cols] {
                sheet.outline(axis).hidden().hash(&mut h);
            }
        }
        let mut col = |sheet: u16, c: u32| {
            if (sheet as usize) < wb.sheet_count() {
                (sheet, c, wb.sheet(sheet).cols.get(c as usize).map_or(0, |c| c.generation)).hash(&mut h);
            }
        };
        let mut texts: Vec<&ChartText> = self.series.iter().filter_map(|s| s.name.as_ref()).collect();
        texts.extend(self.title.as_ref());
        for t in texts {
            if let ChartText::Cell(a) = t {
                col(a.sheet, a.col);
            }
        }
        for s in &self.series {
            for r in [Some(s.vals), s.cats, s.look.sizes].into_iter().flatten() {

                for c in r.c0..=r.c1.min(r.c0 + 4096) {
                    col(r.sheet, c);
                }
            }
        }
        h.finish()
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u8(self.kind.code());
        // Bit 7 of the grouping byte: each series' look follows the alt text.
        let looks = self.series.iter().any(|s| !s.look.plain());
        w.u8(match self.grouping {
            Grouping::Standard => 0,
            Grouping::Stacked => 1,
            Grouping::Percent => 2,
        } | if looks { 0x80 } else { 0 });
        // Bit 7 of the legend byte: alt text follows the series.
        let alt = self.alt.as_deref().filter(|a| !a.is_empty());
        w.u8(match self.legend {
            None => 0,
            Some(LegendPos::Right) => 1,
            Some(LegendPos::Left) => 2,
            Some(LegendPos::Top) => 3,
            Some(LegendPos::Bottom) => 4,
        } | if alt.is_some() { 0x80 } else { 0 });
        text(w, self.title.as_ref());
        w.str(self.x_title.as_deref().unwrap_or(""));
        w.str(self.y_title.as_deref().unwrap_or(""));
        let a = &self.anchor;
        w.u16(a.sheet);
        for (r, c, dx, dy) in [a.from, a.to] {
            w.var(r as u64);
            w.var(c as u64);
            w.f64(dx as f64);
            w.f64(dy as f64);
        }
        w.var(self.series.len() as u64);
        for s in &self.series {
            text(w, s.name.as_ref());
            match &s.cats {
                Some(r) => {
                    w.u8(1);
                    rect(w, r);
                }
                None => w.u8(0),
            }
            rect(w, &s.vals);
            w.u8(s.kind.map_or(0xFF, |k| k.code()));
        }
        if let Some(alt) = alt {
            w.str(alt);
        }
        if looks {
            for s in &self.series {
                s.look.encode(w);
            }
        }
    }

    pub fn decode(r: &mut Reader) -> Result<Chart, WireError> {
        let kind = kind_of(r.u8()?)?;
        let grouping_byte = r.u8()?;
        let has_looks = grouping_byte & 0x80 != 0;
        let grouping = match grouping_byte & 0x7F {
            0 => Grouping::Standard,
            1 => Grouping::Stacked,
            2 => Grouping::Percent,
            _ => return Err(WireError::Invalid("chart grouping")),
        };
        let legend_byte = r.u8()?;
        let has_alt = legend_byte & 0x80 != 0;
        let legend = match legend_byte & 0x7F {
            0 => None,
            1 => Some(LegendPos::Right),
            2 => Some(LegendPos::Left),
            3 => Some(LegendPos::Top),
            4 => Some(LegendPos::Bottom),
            _ => return Err(WireError::Invalid("chart legend")),
        };
        let title = read_text(r)?;
        let opt = |s: &str| (!s.is_empty()).then(|| Arc::from(s));
        let x_title = opt(r.str("axis title", MAX_TITLE as u64)?);
        let y_title = opt(r.str("axis title", MAX_TITLE as u64)?);
        let sheet = r.u16()?;
        let mut corners = [(0u32, 0u32, 0f32, 0f32); 2];
        for c in corners.iter_mut() {
            let row = r.var()?;
            let col = r.var()?;
            if row >= MAX_ROWS as u64 || col >= MAX_COLS as u64 {
                return Err(WireError::Invalid("chart anchor"));
            }
            let (dx, dy) = (r.f64()?, r.f64()?);
            if !(dx.is_finite() && dy.is_finite()) {
                return Err(WireError::Invalid("chart anchor"));
            }
            *c = (row as u32, col as u32, dx as f32, dy as f32);
        }
        let n = r.count("series", MAX_SERIES as u64, 9)?;
        let mut series = Vec::with_capacity(n);
        for _ in 0..n {
            let name = read_text(r)?;
            let cats = match r.u8()? {
                0 => None,
                1 => Some(read_rect(r)?),
                _ => return Err(WireError::Invalid("chart categories")),
            };
            let vals = read_rect(r)?;
            let kind = match r.u8()? {
                0xFF => None,
                k => Some(kind_of(k)?),
            };
            series.push(Series { name, cats, vals, kind, look: SeriesLook::default() });
        }
        let alt = if has_alt {
            let a = r.str("alt text", MAX_ALT as u64)?;
            if a.is_empty() || a.chars().any(|c| c.is_control() && c != '\n') {
                return Err(WireError::Invalid("alt text"));
            }
            Some(Arc::from(a))
        } else {
            None
        };
        if has_looks {
            for s in &mut series {
                s.look = SeriesLook::decode(r)?;
            }
        }
        Ok(Chart { kind, grouping, title, series, legend, x_title, y_title, anchor: Anchor { sheet, from: corners[0], to: corners[1] }, alt })
    }
}

fn kind_of(k: u8) -> Result<ChartKind, WireError> {
    ChartKind::ALL.get(k as usize).copied().ok_or(WireError::Invalid("chart kind"))
}

fn text(w: &mut Writer, t: Option<&ChartText>) {
    match t {
        None => w.u8(0),
        Some(ChartText::Lit(s)) => {
            w.u8(1);
            w.str(s);
        }
        Some(ChartText::Cell(a)) => {
            w.u8(2);
            w.u16(a.sheet);
            w.var(a.row as u64);
            w.var(a.col as u64);
        }
    }
}

fn read_text(r: &mut Reader) -> Result<Option<ChartText>, WireError> {
    Ok(match r.u8()? {
        0 => None,
        1 => Some(ChartText::Lit(Arc::from(r.str("chart text", MAX_TITLE as u64)?))),
        2 => {
            let sheet = r.u16()?;
            let (row, col) = (r.var()?, r.var()?);
            if row >= MAX_ROWS as u64 || col >= MAX_COLS as u64 {
                return Err(WireError::Invalid("chart text cell"));
            }
            Some(ChartText::Cell(CellAddr::new(sheet, row as u32, col as u32)))
        }
        _ => return Err(WireError::Invalid("chart text")),
    })
}

fn rect(w: &mut Writer, r: &Rect) {
    w.u16(r.sheet);
    for v in [r.r0, r.c0, r.r1, r.c1] {
        w.var(v as u64);
    }
}

fn read_rect(r: &mut Reader) -> Result<Rect, WireError> {
    let sheet = r.u16()?;
    let mut v = [0u32; 4];
    for (x, max) in v.iter_mut().zip([MAX_ROWS, MAX_COLS, MAX_ROWS, MAX_COLS]) {
        let n = r.var()?;
        if n >= max as u64 {
            return Err(WireError::Invalid("chart range"));
        }
        *x = n as u32;
    }
    let rc = Rect { sheet, r0: v[0], c0: v[1], r1: v[2], c1: v[3] };
    // One row or one column.
    if rc.r0 > rc.r1 || rc.c0 > rc.c1 || (rc.r0 != rc.r1 && rc.c0 != rc.c1) {
        return Err(WireError::Invalid("chart range"));
    }
    Ok(rc)
}

impl Workbook {
    pub fn charts(&self) -> &[Chart] {
        &self.charts
    }

    /// Adds a chart; refused past [`MAX_CHARTS`], with too many series, on a sheet
    /// that doesn't exist, or with a series range that isn't one row or column.
    pub fn add_chart(&mut self, c: Chart) -> Result<usize, &'static str> {
        if self.charts.len() >= MAX_CHARTS {
            return Err("too many charts");
        }
        if c.series.len() > MAX_SERIES {
            return Err("too many series");
        }
        if c.anchor.sheet as usize >= self.sheet_count() {
            return Err("no such sheet");
        }
        let line = |r: &Rect| r.r0 <= r.r1 && r.c0 <= r.c1 && (r.r0 == r.r1 || r.c0 == r.c1) && r.r1 < MAX_ROWS && r.c1 < MAX_COLS;
        if c.series.iter().any(|s| !line(&s.vals) || s.cats.as_ref().is_some_and(|r| !line(r)) || s.look.sizes.as_ref().is_some_and(|r| !line(r))) {
            return Err("a series range that isn't one row or column");
        }
        if c.series.iter().any(|s| !s.look.valid()) {
            return Err("a series look out of range");
        }
        self.charts.push(c);
        Ok(self.charts.len() - 1)
    }

    /// Swaps in `cs` as the workbook's charts (each checked as [`add_chart`] does)
    /// and returns the old ones.
    ///
    /// [`add_chart`]: Workbook::add_chart
    pub fn replace_charts(&mut self, cs: Vec<Chart>) -> Vec<Chart> {
        let old = std::mem::take(&mut self.charts);
        for c in cs {
            let _ = self.add_chart(c);
        }
        old
    }

    pub fn remove_chart(&mut self, i: usize) -> Option<Chart> {
        (i < self.charts.len()).then(|| self.charts.remove(i))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn sample(wb: &mut Workbook) -> Chart {
        wb.add_sheet("S").unwrap();
        wb.set_value(CellAddr::new(0, 0, 1), Value::str("Sales"));
        for (r, (m, v)) in [("Jan", 3.0), ("Feb", 5.5), ("Mar", -1.0)].iter().enumerate() {
            wb.set_value(CellAddr::new(0, r as u32 + 1, 0), Value::str(m));
            wb.set_value(CellAddr::new(0, r as u32 + 1, 1), Value::Num(*v));
        }
        wb.set_value(CellAddr::new(0, 2, 2), Value::str("n/a"));
        Chart {
            kind: ChartKind::Column,
            grouping: Grouping::Standard,
            title: Some(ChartText::Lit("Quarter".into())),
            series: vec![
                Series { name: Some(ChartText::Cell(CellAddr::new(0, 0, 1))), cats: Some(Rect { sheet: 0, r0: 1, c0: 0, r1: 3, c1: 0 }), vals: Rect { sheet: 0, r0: 1, c0: 1, r1: 3, c1: 1 }, kind: None, look: Default::default() },
                Series { name: None, cats: None, vals: Rect { sheet: 0, r0: 1, c0: 2, r1: 3, c1: 2 }, kind: Some(ChartKind::Line), look: Default::default() },
            ],
            legend: Some(LegendPos::Bottom),
            x_title: None,
            y_title: Some("€".into()),
            anchor: Anchor { sheet: 0, from: (5, 1, 0.0, 0.0), to: (20, 8, 12.5, 3.0) }, alt: None
        }
    }

    #[test]
    fn data_and_wire() {
        let mut wb = Workbook::new();
        let c = sample(&mut wb);
        wb.add_chart(c.clone()).unwrap();
        let d = c.data(&wb, 1000);
        assert_eq!(d[0].name, "Sales");
        assert_eq!(d[0].cats, ["Jan", "Feb", "Mar"]);
        assert_eq!(d[0].vals, [Some(3.0), Some(5.5), Some(-1.0)]);
        assert_eq!(d[1].name, "Series2");
        assert_eq!(d[1].vals, [None, None, None]);
        assert_eq!(d[1].kind, ChartKind::Line);
        // A hidden row (by a filter or by hand) drops out of the chart, and the
        // version changes so a drawing is redone.
        let v = c.data_version(&wb);
        let row = c.series[0].vals.r0 + 1;
        wb.sheet_mut(c.series[0].vals.sheet).hidden = vec![(row, row)];
        assert_ne!(c.data_version(&wb), v);
        let d = c.data(&wb, 1000);
        assert_eq!(d[0].cats, ["Jan", "Mar"]);
        assert_eq!(d[0].vals, [Some(3.0), Some(-1.0)]);
        wb.sheet_mut(c.series[0].vals.sheet).hidden.clear();
        let mut w = Writer::new();
        c.encode(&mut w);
        let b = w.into_bytes();
        assert_eq!(Chart::decode(&mut Reader::new(&b)).unwrap(), c);
        for n in 0..b.len() {
            let _ = Chart::decode(&mut Reader::new(&b[..n]));
        }
        // Alt text rides after the series, flagged in the legend byte.
        let mut described = c.clone();
        described.alt = Some("Sales by month, rising".into());
        let mut w = Writer::new();
        described.encode(&mut w);
        let b = w.into_bytes();
        assert_eq!(Chart::decode(&mut Reader::new(&b)).unwrap(), described);
        assert_eq!(b.len(), { let mut w = Writer::new(); c.encode(&mut w); w.into_bytes().len() } + 1 + "Sales by month, rising".len());
        let mut bad = c.clone();
        bad.series[0].vals = Rect { sheet: 0, r0: 0, c0: 0, r1: 2, c1: 2 };
        assert!(wb.add_chart(bad).is_err());
    }

    #[test]
    fn series_looks_ride_the_wire_and_out_of_range_ones_are_refused() {
        let mut wb = Workbook::new();
        let mut c = sample(&mut wb);
        let plain = { let mut w = Writer::new(); c.encode(&mut w); w.into_bytes() };
        c.kind = ChartKind::Waterfall;
        c.series[0].look = SeriesLook {
            trend: Some(Trendline { kind: TrendKind::Polynomial(4), equation: true, r_squared: false, forward: 1.5, backward: 0.5 }),
            labels: true,
            secondary: true,
            errors: Some(ErrorBars::StdDev(2.0)),
            sizes: Some(c.series[0].vals),
            totals: vec![0, 2],
            bins: Some(Bins::Width(0.25)),
        };
        c.series[1].look = SeriesLook { errors: Some(ErrorBars::StdErr), bins: Some(Bins::Count(12)), ..SeriesLook::default() };
        let mut w = Writer::new();
        c.encode(&mut w);
        let b = w.into_bytes();
        assert!(b.len() > plain.len());
        assert_eq!(Chart::decode(&mut Reader::new(&b)).unwrap(), c);
        for n in 0..b.len() {
            let _ = Chart::decode(&mut Reader::new(&b[..n]));
        }
        assert!(wb.add_chart(c.clone()).is_ok());
        // What the decoder refuses, add_chart refuses too.
        let broken: [fn(&mut SeriesLook); 6] = [
            |l| l.trend = Some(Trendline { kind: TrendKind::Polynomial(7), equation: false, r_squared: false, forward: 0.0, backward: 0.0 }),
            |l| l.trend = Some(Trendline { kind: TrendKind::Linear, equation: false, r_squared: false, forward: -1.0, backward: 0.0 }),
            |l| l.errors = Some(ErrorBars::Fixed(f64::NAN)),
            |l| l.totals = vec![3, 1],
            |l| l.bins = Some(Bins::Count(0)),
            |l| l.bins = Some(Bins::Width(-2.0)),
        ];
        for f in broken {
            let mut bad = c.clone();
            f(&mut bad.series[0].look);
            assert!(!bad.series[0].look.valid());
            assert!(wb.add_chart(bad.clone()).is_err());
            let mut w = Writer::new();
            bad.encode(&mut w);
            assert!(Chart::decode(&mut Reader::new(&w.into_bytes())).is_err());
        }
    }
}

#[cfg(test)]
mod native_tests {
    #[test]
    fn charts_in_native_files() {
        let mut wb = super::Workbook::new();
        let c = super::tests::sample(&mut wb);
        wb.add_chart(c).unwrap();
        let mut b = Vec::new();
        crate::native::save(&wb, &mut b).unwrap();
        assert_eq!(crate::native::load(&b).unwrap().charts(), wb.charts());
    }

    #[test]
    fn picture_alt_text_round_trips_with_its_flag() {
        use super::*;
        let png = b"\x89PNG\r\n\x1a\nrest".to_vec();
        let p = Picture { anchor: Anchor { sheet: 4095, from: (1, 1, 0.0, 0.0), to: (5, 4, 0.0, 0.0) }, bytes: png.into(), link: None, crop: [0; 4], rotation: 0, alt: Some("A red logo\non white".into()) };
        let mut w = Writer::new();
        p.encode(&mut w);
        let b = w.into_bytes();
        assert_eq!(Picture::decode(&mut Reader::new(&b), 1 << 20).unwrap(), p);
        let plain = Picture { alt: None, ..p.clone() };
        let mut w = Writer::new();
        plain.encode(&mut w);
        assert_eq!(b.len(), w.into_bytes().len() + 1 + "A red logo\non white".len());
    }
}
