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
}

impl ChartKind {
    pub const ALL: [ChartKind; 7] = [ChartKind::Column, ChartKind::Bar, ChartKind::Line, ChartKind::Area, ChartKind::Scatter, ChartKind::Pie, ChartKind::Doughnut];
    fn code(self) -> u8 {
        ChartKind::ALL.iter().position(|k| *k == self).unwrap() as u8
    }
    /// Pie and doughnut: one series around a circle, no axes.
    pub fn round(self) -> bool {
        matches!(self, ChartKind::Pie | ChartKind::Doughnut)
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
}

/// Where a chart sits: its top-left and bottom-right corners, each a cell plus an
/// offset into it in points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Anchor {
    pub sheet: u16,
    pub from: (u32, u32, f32, f32),
    pub to: (u32, u32, f32, f32),
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
}

/// The cells of a one-row-or-column range, in order, at most `max`.
fn cells(r: &Rect, max: usize) -> Vec<CellAddr> {
    let mut out = Vec::new();
    for row in r.r0..=r.r1 {
        for col in r.c0..=r.c1 {
            if out.len() >= max {
                return out;
            }
            out.push(CellAddr::new(r.sheet, row, col));
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
                    cells(&s.vals, max).into_iter().map(|a| if let Value::Num(x) = wb.get(a) { Some(x) } else { None }).collect()
                } else {
                    Vec::new()
                };
                let cat_cells = s.cats.filter(|r| ok_rect(wb, r)).map(|r| cells(&r, max)).unwrap_or_default();
                let cats: Vec<String> = cat_cells.iter().map(|a| wb.formatted(*a).text).collect();
                let kind = s.kind.unwrap_or(self.kind);
                let xs = if kind == ChartKind::Scatter { cat_cells.iter().map(|a| if let Value::Num(x) = wb.get(*a) { Some(x) } else { None }).collect() } else { Vec::new() };
                let name = s.name.as_ref().map(|t| Chart::text(wb, t)).unwrap_or_else(|| format!("Series{}", i + 1));
                SeriesData { name, cats, xs, vals, kind }
            })
            .collect()
    }

    /// Changes whenever a cell the chart reads may have changed (a hash of its
    /// columns' versions), so a drawing of it can be kept until then.
    pub fn data_version(&self, wb: &Workbook) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
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
            for r in [Some(s.vals), s.cats].into_iter().flatten() {
                for c in r.c0..=r.c1.min(r.c0 + 4096) {
                    col(r.sheet, c);
                }
            }
        }
        h.finish()
    }

    pub fn encode(&self, w: &mut Writer) {
        w.u8(self.kind.code());
        w.u8(match self.grouping {
            Grouping::Standard => 0,
            Grouping::Stacked => 1,
            Grouping::Percent => 2,
        });
        w.u8(match self.legend {
            None => 0,
            Some(LegendPos::Right) => 1,
            Some(LegendPos::Left) => 2,
            Some(LegendPos::Top) => 3,
            Some(LegendPos::Bottom) => 4,
        });
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
    }

    pub fn decode(r: &mut Reader) -> Result<Chart, WireError> {
        let kind = kind_of(r.u8()?)?;
        let grouping = match r.u8()? {
            0 => Grouping::Standard,
            1 => Grouping::Stacked,
            2 => Grouping::Percent,
            _ => return Err(WireError::Invalid("chart grouping")),
        };
        let legend = match r.u8()? {
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
            series.push(Series { name, cats, vals, kind });
        }
        Ok(Chart { kind, grouping, title, series, legend, x_title, y_title, anchor: Anchor { sheet, from: corners[0], to: corners[1] } })
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
        if c.series.iter().any(|s| !line(&s.vals) || s.cats.as_ref().is_some_and(|r| !line(r))) {
            return Err("a series range that isn't one row or column");
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
                Series { name: Some(ChartText::Cell(CellAddr::new(0, 0, 1))), cats: Some(Rect { sheet: 0, r0: 1, c0: 0, r1: 3, c1: 0 }), vals: Rect { sheet: 0, r0: 1, c0: 1, r1: 3, c1: 1 }, kind: None },
                Series { name: None, cats: None, vals: Rect { sheet: 0, r0: 1, c0: 2, r1: 3, c1: 2 }, kind: Some(ChartKind::Line) },
            ],
            legend: Some(LegendPos::Bottom),
            x_title: None,
            y_title: Some("€".into()),
            anchor: Anchor { sheet: 0, from: (5, 1, 0.0, 0.0), to: (20, 8, 12.5, 3.0) },
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
        let mut w = Writer::new();
        c.encode(&mut w);
        let b = w.into_bytes();
        assert_eq!(Chart::decode(&mut Reader::new(&b)).unwrap(), c);
        for n in 0..b.len() {
            let _ = Chart::decode(&mut Reader::new(&b[..n]));
        }
        let mut bad = c.clone();
        bad.series[0].vals = Rect { sheet: 0, r0: 0, c0: 0, r1: 2, c1: 2 };
        assert!(wb.add_chart(bad).is_err());
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
}
