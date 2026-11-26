//! Sparklines (docs/spec/officina.md §16.143): a small line, column or
//! win/loss chart drawn inside one cell from a row or column of numbers,
//! kept with the sheet as Excel keeps them. The list changes only through
//! [`crate::ops::GridOp::Sparklines`], so it undoes.

use officina_core::wire::{Reader, WireError, Writer};

use crate::addr::{Rect, MAX_COLS, MAX_ROWS};
use crate::shift::Shift;
use crate::value::Value;

/// The most sparklines a sheet keeps.
pub const MAX_SPARKLINES: usize = 10_000;
/// The most cells one sparkline reads.
pub const MAX_POINTS: u64 = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SparkKind {
    Line,
    Column,
    /// Each point up (positive), down (negative) or missing (zero).
    WinLoss,
}

impl SparkKind {
    fn code(self) -> u8 {
        match self {
            SparkKind::Line => 0,
            SparkKind::Column => 1,
            SparkKind::WinLoss => 2,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sparkline {
    /// The cell it is drawn in.
    pub row: u32,
    pub col: u32,
    /// The numbers it draws: one row or one column, on any sheet.
    pub data: Rect,
    pub kind: SparkKind,
    /// 0xRRGGBB for the series, and for negative points (columns, win/loss).
    pub color: u32,
    pub negative: u32,
    /// The highest and lowest points marked.
    pub markers: bool,
}

/// Excel's default sparkline colours.
pub const DEFAULT_COLOR: u32 = 0x376092;
pub const DEFAULT_NEGATIVE: u32 = 0xD00000;

impl Sparkline {
    pub fn valid(&self) -> bool {
        let d = &self.data;
        self.row < MAX_ROWS
            && self.col < MAX_COLS
            && d.r0 <= d.r1
            && d.c0 <= d.c1
            && d.r1 < MAX_ROWS
            && d.c1 < MAX_COLS
            && (d.r0 == d.r1 || d.c0 == d.c1)
            && d.cells() <= MAX_POINTS
            && self.color <= 0xFF_FFFF
            && self.negative <= 0xFF_FFFF
    }

    /// The points it draws from the data's values: numbers, `None` for
    /// anything else (a gap, as Excel shows empty cells by default).
    pub fn points(&self, wb: &crate::book::Workbook) -> Vec<Option<f64>> {
        let d = &self.data;
        let mut out = Vec::with_capacity(d.cells() as usize);
        for r in d.r0..=d.r1 {
            for c in d.c0..=d.c1 {
                out.push(match wb.value(d.sheet, r, c) {
                    Value::Num(x) if x.is_finite() => Some(x),
                    _ => None,
                });
            }
        }
        out
    }
}

/// A sheet's list: each valid, at most one in a cell.
pub fn valid_list(list: &[Sparkline]) -> bool {
    if list.len() > MAX_SPARKLINES || !list.iter().all(Sparkline::valid) {
        return false;
    }
    let mut at: Vec<(u32, u32)> = list.iter().map(|s| (s.row, s.col)).collect();
    at.sort_unstable();
    at.windows(2).all(|w| w[0] != w[1])
}

pub fn encode(list: &[Sparkline], w: &mut Writer) {
    w.var(list.len() as u64);
    for s in list {
        w.u32(s.row);
        w.u32(s.col);
        w.u16(s.data.sheet);
        for v in [s.data.r0, s.data.c0, s.data.r1, s.data.c1] {
            w.u32(v);
        }
        w.u8(s.kind.code() | if s.markers { 0x80 } else { 0 });
        w.u32(s.color);
        w.u32(s.negative);
    }
}

pub fn decode(r: &mut Reader) -> Result<Vec<Sparkline>, WireError> {
    let n = r.count("sparklines", MAX_SPARKLINES as u64, 35)?;
    let mut list = Vec::with_capacity(n);
    for _ in 0..n {
        let (row, col, sheet) = (r.u32()?, r.u32()?, r.u16()?);
        let (r0, c0, r1, c1) = (r.u32()?, r.u32()?, r.u32()?, r.u32()?);
        let k = r.u8()?;
        let kind = match k & 0x7F {
            0 => SparkKind::Line,
            1 => SparkKind::Column,
            2 => SparkKind::WinLoss,
            _ => return Err(WireError::Invalid("sparkline kind")),
        };
        list.push(Sparkline { row, col, data: Rect { sheet, r0, c0, r1, c1 }, kind, color: r.u32()?, negative: r.u32()?, markers: k & 0x80 != 0 });
    }
    if !valid_list(&list) {
        return Err(WireError::Invalid("sparklines"));
    }
    Ok(list)
}

/// The list after rows or columns of sheet `sheet` move: each sparkline's
/// cell follows (deleted: it goes) and data on that sheet follows too
/// (all deleted: it goes).
pub(crate) fn shifted(list: &[Sparkline], sheet: u16, s: Shift) -> Vec<Sparkline> {
    list.iter().filter_map(|sp| {
        let (row, col) = s.map_cell(sp.row, sp.col)?;
        let data = if sp.data.sheet == sheet { s.map_rect(sp.data)? } else { sp.data };
        Some(Sparkline { row, col, data, ..sp.clone() }).filter(Sparkline::valid)
    }).collect()
}
