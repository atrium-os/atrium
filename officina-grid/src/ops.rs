//! Undoable grid edits for `officina_core::history`.
//!
//! Every edit is "replace this rectangle's contents with a snapshot". Applying one
//! returns the snapshot of what it replaced, which is its inverse, so undo and redo are
//! the same operation. A snapshot keeps formulas as fills (one per group piece), not as
//! cells, so undoing an edit inside a million-row fill costs one fill, not a million.

use crate::addr::{CellAddr, Rect, MAX_COLS, MAX_ROWS};
use crate::book::{Input, Workbook};
use crate::column::Cell;
use crate::style::Style;
use crate::formula;
use crate::value::{ErrorKind, Value};
use officina_core::history::Op;
use officina_core::wire::{Reader, WireError, Writer};

/// A formula filled down rows `r0..=r1` of one column; `src` is written for row `r0`.
#[derive(Debug, Clone, PartialEq)]
pub struct Fill {
    pub col: u32,
    pub r0: u32,
    pub r1: u32,
    pub src: String,
}

/// The contents of a rectangle: constants by cell, formulas by fill. Cells of the
/// rectangle not named are empty.
#[derive(Debug, Clone, PartialEq)]
pub struct Region {
    pub rect: Rect,
    pub values: Vec<(u32, u32, Value)>,
    pub fills: Vec<Fill>,
}

/// The styles of a rectangle: spans of one column with a style other than the
/// default, by value (ids are per workbook). Cells not covered have the default.
#[derive(Debug, Clone, PartialEq)]
pub struct StyleRegion {
    pub rect: Rect,
    pub spans: Vec<(u32, u32, u32, Style)>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum GridOp {
    Replace(Region),
    Restyle(StyleRegion),
    /// A sheet's filter and hidden rows, swapped in whole.
    Filter(crate::filter::FilterState),
    /// Several edits as one (a sort moves values and styles together). Not nested.
    Batch(Vec<GridOp>),
    /// The workbook's structured tables, swapped in whole (tables that don't fit
    /// are left out).
    Tables(Vec<crate::tables::Table>),
    /// The workbook's charts, swapped in whole.
    Charts(Vec<crate::charts::Chart>),
    /// The workbook's embedded pictures, swapped in whole.
    Pictures(Vec<crate::charts::Picture>),
    /// The workbook's pivot tables with where their output is, swapped in whole
    /// (their cells change by the `Replace`s beside it).
    Pivots(Vec<(crate::pivot::Pivot, Option<Rect>)>),
    /// The workbook's conditional formats, swapped in whole, as
    /// [`Workbook::encode_cond_formats`] writes them (their formulas are text, read
    /// against the workbook when the op is applied). Made by [`GridOp::cond_formats`].
    CondFormats(std::sync::Arc<[u8]>),
    /// The workbook's data validation rules, swapped in whole, as
    /// [`Workbook::encode_validations`] writes them. Made by [`GridOp::validations`].
    Validations(std::sync::Arc<[u8]>),
    /// The workbook's defined names, swapped in whole, as
    /// [`Workbook::encode_names`] writes them (formula text, read against the
    /// workbook when applied). Made by [`GridOp::names`].
    Names(std::sync::Arc<[u8]>),
    /// A sheet's row heights (`rows`) or column widths that differ from the
    /// default, swapped in whole: (index, size), sorted.
    Sizes { sheet: u16, rows: bool, over: Vec<(u32, f64)> },
    /// A complete size axis, including its default and explicit overrides.
    LayoutSizes { sheet: u16, rows: bool, default: f64, over: Vec<(u32, f64)> },
    /// A new empty sheet at the end, by this name (refused, changing nothing, if
    /// the name is taken or bad). Its inverse is [`GridOp::DropSheet`].
    AddSheet(String),
    AddIdentifiedSheet { name: String, identity: u128 },
    /// Takes away the last sheet (the inverse of [`GridOp::AddSheet`], which
    /// undo order leaves empty again by then). The first sheet always stays.
    DropSheet,
    /// A sheet's new name; formulas name sheets by number, so they follow.
    RenameSheet { sheet: u16, name: String },
    /// Sheet `from` moved to place `to` ([`Workbook::move_sheet`]); its inverse
    /// moves it back.
    MoveSheet { from: u16, to: u16 },
    /// A sheet's tab colour (0xRRGGBB), or none.
    TabColor { sheet: u16, color: Option<u32> },
    /// A sheet hidden or shown again. Hiding the last sheet that shows
    /// changes nothing.
    HideSheet { sheet: u16, hidden: bool },
    /// A sheet's merged cells ([`crate::merges`]), swapped in whole.
    Merges { sheet: u16, list: Vec<Rect> },
    /// Comments set, or taken away (`None`), on cells of a sheet, in order.
    Comments { sheet: u16, set: Vec<(u32, u32, Option<crate::comments::Comment>)> },
    /// Links set, or taken away (`None`), on cells of a sheet, in order.
    Links { sheet: u16, set: Vec<(u32, u32, Option<crate::links::Link>)> },
    /// A sheet's frozen rows and columns ([`crate::book::Sheet::freeze`]).
    /// `from`: the first frozen row and column ([`crate::book::Sheet::freeze_from`]).
    Freeze { sheet: u16, rows: u32, cols: u32, from: (u32, u32) },
    /// A sheet's remembered widths of hidden columns
    /// ([`crate::book::Sheet::hidden_widths`]), swapped in whole.
    HiddenWidths { sheet: u16, list: Vec<(u32, f64)> },
    Outline { sheet: u16, axis: crate::shift::Axis, outline: crate::outline::Outline },
    AxisIdentities { sheet: u16, axis: crate::shift::Axis, ids: crate::axis_ids::AxisIds },
    /// Iterative calculation on (with its settings) or off
    /// ([`crate::book::Workbook::set_iteration`]).
    Iteration(Option<crate::book::Iteration>),
    /// A sheet's What-If scenarios, the whole list replaced.
    Scenarios { sheet: u16, list: Vec<crate::scenario::Scenario> },
    /// Protect Workbook on (with its settings) or off.
    Structure(Option<crate::protection::SheetProtection>),
    /// A sheet's sparklines, the whole list replaced.
    Sparklines { sheet: u16, list: Vec<crate::sparkline::Sparkline> },
    /// Manual (true) or automatic calculation.
    Calculation(bool),
    /// A sheet's watched cells, the whole list replaced.
    Watches { sheet: u16, list: Vec<(u32, u32)> },
    /// A sheet's Allow Edit Ranges, the whole list replaced.
    EditRanges { sheet: u16, list: Vec<crate::sheetareas::EditRange> },
    /// A sheet's ignored errors, the whole list replaced.
    IgnoredErrors { sheet: u16, list: Vec<crate::sheetareas::Ignored> },
    /// Protect Sheet on (with its verifier and allowances) or off.
    Protection { sheet: u16, protection: Option<crate::protection::SheetProtection> },
    /// The workbook's document properties, all replaced.
    Properties(crate::docprops::DocProps),
    /// The 1904 date system on (`true`) or off: the same serial numbers,
    /// read as dates four years and a day later. Everything recalculates.
    DateSystem1904(bool),
    /// The workbook's Custom Views, the whole list replaced.
    CustomViews(Vec<crate::views::CustomView>),
}

/// Remembered widths: count, then each column (rising) and its width
/// (finite, 0 to 8192 px).
pub(crate) fn decode_widths(r: &mut Reader) -> Result<Vec<(u32, f64)>, WireError> {
    let n = r.count("widths", MAX_COLS as u64, 9)?;
    let mut list = Vec::with_capacity(n);
    let mut last = None;
    for _ in 0..n {
        let i = r.var()?;
        let v = r.f64()?;
        if i >= MAX_COLS as u64 || last.is_some_and(|l| i <= l) || !v.is_finite() || !(0.0..=8192.0).contains(&v) {
            return Err(WireError::Invalid("hidden width"));
        }
        last = Some(i);
        list.push((i as u32, v));
    }
    Ok(list)
}

impl GridOp {
    /// Sets one cell.
    pub fn add_sheet(name: String) -> Self {
        Self::AddIdentifiedSheet { name, identity: crate::book::new_sheet_identity() }
    }

    pub fn set(a: CellAddr, input: Input) -> Self {
        Self::block(a.sheet, a.row, a.col, vec![vec![input]])
    }

    /// Sets a block of cells from `rows[r][c]` at (`r0`, `c0`), as a paste does.
    pub fn block(sheet: u16, r0: u32, c0: u32, rows: Vec<Vec<Input>>) -> Self {
        let h = rows.len().max(1) as u32;
        let w = rows.iter().map(Vec::len).max().unwrap_or(1).max(1) as u32;
        let mut values = Vec::new();
        let mut fills = Vec::new();
        for (i, row) in rows.into_iter().enumerate() {
            for (j, input) in row.into_iter().enumerate() {
                let (r, c) = (r0 + i as u32, c0 + j as u32);
                match input {
                    Input::Empty | Input::Value(Value::Empty) => {}
                    Input::Value(v) => values.push((r, c, v)),
                    Input::Formula(src) => fills.push(Fill { col: c, r0: r, r1: r, src }),
                }
            }
        }
        GridOp::Replace(Region { rect: Rect { sheet, r0, c0, r1: r0 + h - 1, c1: c0 + w - 1 }, values, fills })
    }

    /// Fills `src` (written for row `r0`) down `r0..=r1` of a column.
    pub fn fill(sheet: u16, col: u32, r0: u32, r1: u32, src: &str) -> Self {
        GridOp::Replace(Region {
            rect: Rect { sheet, r0, c0: col, r1, c1: col },
            values: Vec::new(),
            fills: vec![Fill { col, r0, r1, src: src.to_string() }],
        })
    }

    /// Puts `fs` in place of the workbook's conditional formats.
    pub fn cond_formats(wb: &Workbook, fs: &[crate::condfmt::CondFormat]) -> Self {
        GridOp::CondFormats(wb.encode_cond_formats(fs).into())
    }
    pub fn validations(wb: &Workbook, vs: &[crate::validation::Validation]) -> Self {
        GridOp::Validations(wb.encode_validations(vs).into())
    }

    /// Puts `defs` in place of the workbook's defined names.
    pub fn names(wb: &Workbook, defs: &[crate::names::NameDef]) -> Self {
        GridOp::Names(wb.encode_names(defs).into())
    }

    /// Empties a rectangle.
    pub fn clear(rect: Rect) -> Self {
        GridOp::Replace(Region { rect, values: Vec::new(), fills: Vec::new() })
    }

    /// Sets `style` on every cell of `rect`.
    pub fn style(rect: Rect, style: Style) -> Self {
        let spans = if style == Style::default() { Vec::new() } else { (rect.c0..=rect.c1).map(|c| (c, rect.r0, rect.r1, style.clone())).collect() };
        GridOp::Restyle(StyleRegion { rect, spans })
    }

    /// Changes the style of every cell of `rect` with `f` (bold on, a new number
    /// format, ...), keeping the rest of each cell's style.
    pub fn restyle(wb: &Workbook, rect: Rect, f: impl Fn(&Style) -> Style) -> Self {
        let mut spans = Vec::new();
        for c in rect.c0..=rect.c1 {
            let runs = wb.sheets.get(rect.sheet as usize).and_then(|s| s.styles.get(c as usize));
            let parts = match runs {
                Some(r) => r.spans(rect.r0, rect.r1),
                None => vec![(rect.r0, rect.r1, 0)],
            };
            for (a, b, id) in parts {
                let st = f(wb.styles.get(id));
                if st != Style::default() {
                    spans.push((c, a, b, st));
                }
            }
        }
        GridOp::Restyle(StyleRegion { rect, spans })
    }

    /// The op as journal bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        let r = match self {
            GridOp::Filter(f) => {
                w.u8(4);
                f.encode(&mut w);
                return w.into_bytes();
            }
            GridOp::Tables(ts) => {
                w.u8(5);
                w.var(ts.len() as u64);
                for t in ts {
                    t.encode(&mut w);
                }
                return w.into_bytes();
            }
            GridOp::Charts(cs) => {
                w.u8(6);
                w.var(cs.len() as u64);
                for c in cs {
                    c.encode(&mut w);
                }
                return w.into_bytes();
            }
            GridOp::Pictures(pictures) => {
                w.u8(23);
                w.var(pictures.len() as u64);
                for picture in pictures {
                    picture.encode(&mut w);
                }
                return w.into_bytes();
            }
            GridOp::Pivots(ps) => {
                w.u8(7);
                w.var(ps.len() as u64);
                for (p, area) in ps {
                    p.encode(&mut w, *area);
                }
                return w.into_bytes();
            }
            GridOp::Validations(b) => {
                w.u8(20);
                w.bytes(b);
                return w.into_bytes();
            }
            GridOp::CondFormats(b) => {
                w.u8(8);
                w.bytes(b);
                return w.into_bytes();
            }
            GridOp::Names(b) => {
                w.u8(9);
                w.bytes(b);
                return w.into_bytes();
            }
            GridOp::AddSheet(name) => {
                w.u8(11);
                w.str(name);
                return w.into_bytes();
            }
            GridOp::AddIdentifiedSheet { name, identity } => {
                w.u8(25);
                w.str(name);
                w.u64((*identity >> 64) as u64);
                w.u64(*identity as u64);
                return w.into_bytes();
            }
            GridOp::DropSheet => {
                w.u8(12);
                return w.into_bytes();
            }
            GridOp::MoveSheet { from, to } => {
                w.u8(14);
                w.u16(*from);
                w.u16(*to);
                return w.into_bytes();
            }
            GridOp::HideSheet { sheet, hidden } => {
                w.u8(27);
                w.u16(*sheet);
                w.u8(u8::from(*hidden));
                return w.into_bytes();
            }
            GridOp::TabColor { sheet, color } => {
                w.u8(15);
                w.u16(*sheet);
                match color {
                    Some(c) => {
                        w.u8(1);
                        w.u32(*c & 0xFF_FFFF);
                    }
                    None => w.u8(0),
                }
                return w.into_bytes();
            }
            GridOp::RenameSheet { sheet, name } => {
                w.u8(13);
                w.u16(*sheet);
                w.str(name);
                return w.into_bytes();
            }
            GridOp::Outline { sheet, axis, outline } => {
                w.u8(26);
                w.u16(*sheet);
                w.u8(axis.index() as u8);
                outline.encode(&mut w);
                return w.into_bytes();
            }
            GridOp::Structure(p) => {
                w.u8(31);
                match p {
                    Some(p) => {
                        w.u8(1);
                        p.encode(&mut w);
                    }
                    None => w.u8(0),
                }
                return w.into_bytes();
            }
            GridOp::Sparklines { sheet, list } => {
                w.u8(30);
                w.u16(*sheet);
                crate::sparkline::encode(list, &mut w);
                return w.into_bytes();
            }
            GridOp::Scenarios { sheet, list } => {
                w.u8(29);
                w.u16(*sheet);
                crate::scenario::encode(list, &mut w);
                return w.into_bytes();
            }
            GridOp::Calculation(manual) => {
                w.u8(32);
                w.u8(u8::from(*manual));
                return w.into_bytes();
            }
            GridOp::EditRanges { sheet, list } => {
                w.u8(34);
                w.u16(*sheet);
                crate::sheetareas::encode_ranges(list, &mut w);
                return w.into_bytes();
            }
            GridOp::Protection { sheet, protection } => {
                w.u8(36);
                w.u16(*sheet);
                match protection {
                    Some(p) => {
                        w.u8(1);
                        p.encode(&mut w);
                    }
                    None => w.u8(0),
                }
                return w.into_bytes();
            }
            GridOp::DateSystem1904(on) => {
                w.u8(38);
                w.u8(u8::from(*on));
                return w.into_bytes();
            }
            GridOp::CustomViews(list) => {
                w.u8(39);
                crate::views::encode(list, &mut w);
                return w.into_bytes();
            }
            GridOp::Properties(p) => {
                w.u8(37);
                p.encode(&mut w);
                return w.into_bytes();
            }
            GridOp::IgnoredErrors { sheet, list } => {
                w.u8(35);
                w.u16(*sheet);
                crate::sheetareas::encode_ignored(list, &mut w);
                return w.into_bytes();
            }
            GridOp::Watches { sheet, list } => {
                w.u8(33);
                w.u16(*sheet);
                crate::watch::encode(list, &mut w);
                return w.into_bytes();
            }
            GridOp::Iteration(it) => {
                w.u8(28);
                match it {
                    Some(it) => {
                        w.u8(1);
                        w.u32(it.count);
                        w.f64(it.change);
                    }
                    None => w.u8(0),
                }
                return w.into_bytes();
            }
            GridOp::AxisIdentities { sheet, axis, ids } => {
                w.u8(22);
                w.u16(*sheet);
                w.u8(axis.index() as u8);
                ids.encode(&mut w);
                return w.into_bytes();
            }
            GridOp::HiddenWidths { sheet, list } => {
                w.u8(21);
                w.u16(*sheet);
                w.var(list.len() as u64);
                for (i, v) in list {
                    w.var(*i as u64);
                    w.f64(*v);
                }
                return w.into_bytes();
            }
            GridOp::Freeze { sheet, rows, cols, from } => {
                w.u8(19);
                w.u16(*sheet);
                w.u32(*rows);
                w.u32(*cols);
                if *from != (0, 0) {
                    w.u32(from.0);
                    w.u32(from.1);
                }
                return w.into_bytes();
            }
            GridOp::Links { sheet, set } => {
                w.u8(18);
                w.u16(*sheet);
                crate::links::encode(&mut w, set);
                return w.into_bytes();
            }
            GridOp::Comments { sheet, set } => {
                w.u8(17);
                w.u16(*sheet);
                crate::comments::encode(&mut w, set);
                return w.into_bytes();
            }
            GridOp::Merges { sheet, list } => {
                w.u8(16);
                w.u16(*sheet);
                crate::merges::encode(&mut w, list);
                return w.into_bytes();
            }
            GridOp::Sizes { sheet, rows, over } | GridOp::LayoutSizes { sheet, rows, over, .. } => {
                let default = match self {
                    GridOp::LayoutSizes { default, .. } => Some(*default),
                    _ => None,
                };
                w.u8(if default.is_some() { 24 } else { 10 });
                w.u16(*sheet);
                w.u8(u8::from(*rows));
                if let Some(default) = default { w.f64(default); }
                w.var(over.len() as u64);
                for (i, v) in over {
                    w.var(*i as u64);
                    w.f64(*v);
                }
                return w.into_bytes();
            }
            GridOp::Batch(ops) => {
                w.u8(3);
                w.var(ops.len() as u64);
                for op in ops {
                    w.bytes(&op.encode());
                }
                return w.into_bytes();
            }
            GridOp::Replace(r) => r,
            GridOp::Restyle(s) => {
                w.u8(2);
                w.u16(s.rect.sheet);
                for x in [s.rect.r0, s.rect.c0, s.rect.r1, s.rect.c1] {
                    w.var(x as u64);
                }
                w.var(s.spans.len() as u64);
                for (c, a, b, st) in &s.spans {
                    w.var(*c as u64);
                    w.var(*a as u64);
                    w.var(*b as u64);
                    st.encode(&mut w);
                }
                return w.into_bytes();
            }
        };
        w.u8(1);
        w.u16(r.rect.sheet);
        w.var(r.rect.r0 as u64);
        w.var(r.rect.c0 as u64);
        w.var(r.rect.r1 as u64);
        w.var(r.rect.c1 as u64);
        w.var(r.values.len() as u64);
        for (row, col, v) in &r.values {
            w.var(*row as u64);
            w.var(*col as u64);
            write_value(&mut w, v);
        }
        w.var(r.fills.len() as u64);
        for f in &r.fills {
            w.var(f.col as u64);
            w.var(f.r0 as u64);
            w.var(f.r1 as u64);
            w.str(&f.src);
        }
        w.into_bytes()
    }

    /// Reads an op written by [`encode`](Self::encode). Bounds everything; never panics.
    pub fn decode(b: &[u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(b);
        let kind = r.u8()?;
        if kind == 5 || kind == 6 {
            let op = if kind == 5 {
                let n = r.count("tables", crate::tables::MAX_TABLES as u64, 8)?;
                let mut v = Vec::with_capacity(n);
                for _ in 0..n {
                    v.push(crate::tables::Table::decode(&mut r)?);
                }
                GridOp::Tables(v)
            } else {
                let n = r.count("charts", crate::charts::MAX_CHARTS as u64, 30)?;
                let mut v = Vec::with_capacity(n);
                for _ in 0..n {
                    v.push(crate::charts::Chart::decode(&mut r)?);
                }
                GridOp::Charts(v)
            };
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(op);
        }
        if kind == 23 {
            use crate::charts::{Picture, MAX_PICTURES, MAX_PICTURE_TOTAL};
            let count = r.count("pictures", MAX_PICTURES as u64, 51)?;
            let mut pictures = Vec::with_capacity(count);
            let mut room = MAX_PICTURE_TOTAL;
            for _ in 0..count {
                let picture = Picture::decode(&mut r, room)?;
                room -= picture.bytes.len();
                pictures.push(picture);
            }
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(GridOp::Pictures(pictures));
        }
        if kind == 7 {
            let n = r.count("pivot tables", crate::pivot::MAX_PIVOTS as u64, 8)?;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                v.push(crate::pivot::Pivot::decode(&mut r)?);
            }
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(GridOp::Pivots(v));
        }
        if kind == 20 {
            let v = r.bytes("validation rules", b.len() as u64)?;
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(GridOp::Validations(v.into()));
        }
        if kind == 8 || kind == 9 {
            let v = r.bytes("conditional formats", b.len() as u64)?;
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(if kind == 8 { GridOp::CondFormats(v.into()) } else { GridOp::Names(v.into()) });
        }
        if kind == 26 {
            let sheet = r.u16()?;
            let axis = match r.u8()? {
                0 => crate::shift::Axis::Rows,
                1 => crate::shift::Axis::Cols,
                _ => return Err(WireError::Invalid("outline axis")),
            };
            let outline = crate::outline::Outline::decode(&mut r, axis)?;
            if !r.is_empty() { return Err(WireError::Invalid("trailing bytes")); }
            return Ok(GridOp::Outline { sheet, axis, outline });
        }
        if kind == 25 {
            let name = r.str("sheet name", crate::book::MAX_SHEET_NAME as u64)?.to_string();
            let identity = (u128::from(r.u64()?) << 64) | u128::from(r.u64()?);
            if !r.is_empty() { return Err(WireError::Invalid("trailing bytes")); }
            return Ok(GridOp::AddIdentifiedSheet { name, identity });
        }
        if (11..=15).contains(&kind) {
            let op = match kind {
                15 => {
                    let sheet = r.u16()?;
                    let color = match r.u8()? {
                        0 => None,
                        1 => Some(r.u32()? & 0xFF_FFFF),
                        _ => return Err(WireError::Invalid("tab colour")),
                    };
                    GridOp::TabColor { sheet, color }
                }
                14 => GridOp::MoveSheet { from: r.u16()?, to: r.u16()? },
                11 => GridOp::AddSheet(r.str("sheet name", crate::book::MAX_SHEET_NAME as u64)?.to_string()),
                12 => GridOp::DropSheet,
                _ => {
                    let sheet = r.u16()?;
                    GridOp::RenameSheet { sheet, name: r.str("sheet name", crate::book::MAX_SHEET_NAME as u64)?.to_string() }
                }
            };
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(op);
        }
        if kind == 27 {
            let sheet = r.u16()?;
            let hidden = match r.u8()? {
                0 => false,
                1 => true,
                _ => return Err(WireError::Invalid("sheet hidden flag")),
            };
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(GridOp::HideSheet { sheet, hidden });
        }
        if kind == 31 {
            let p = match r.u8()? {
                0 => None,
                1 => Some(crate::protection::SheetProtection::decode(&mut r)?),
                _ => return Err(WireError::Invalid("structure protection")),
            };
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(GridOp::Structure(p));
        }
        if kind == 32 {
            let manual = match r.u8()? {
                0 => false,
                1 => true,
                _ => return Err(WireError::Invalid("calculation mode")),
            };
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(GridOp::Calculation(manual));
        }
        if kind == 34 {
            let sheet = r.u16()?;
            let list = crate::sheetareas::decode_ranges(&mut r)?;
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(GridOp::EditRanges { sheet, list });
        }
        if kind == 36 {
            let sheet = r.u16()?;
            let protection = match r.u8()? {
                0 => None,
                1 => Some(crate::protection::SheetProtection::decode(&mut r)?),
                _ => return Err(WireError::Invalid("sheet protection")),
            };
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(GridOp::Protection { sheet, protection });
        }
        if kind == 38 {
            let on = match r.u8()? {
                0 => false,
                1 => true,
                _ => return Err(WireError::Invalid("date system")),
            };
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(GridOp::DateSystem1904(on));
        }
        if kind == 39 {
            let list = crate::views::decode(&mut r)?;
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(GridOp::CustomViews(list));
        }
        if kind == 37 {
            let p = crate::docprops::DocProps::decode(&mut r)?;
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(GridOp::Properties(p));
        }
        if kind == 35 {
            let sheet = r.u16()?;
            let list = crate::sheetareas::decode_ignored(&mut r)?;
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(GridOp::IgnoredErrors { sheet, list });
        }
        if kind == 33 {
            let sheet = r.u16()?;
            let list = crate::watch::decode(&mut r)?;
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(GridOp::Watches { sheet, list });
        }
        if kind == 30 {
            let sheet = r.u16()?;
            let list = crate::sparkline::decode(&mut r)?;
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(GridOp::Sparklines { sheet, list });
        }
        if kind == 29 {
            let sheet = r.u16()?;
            let list = crate::scenario::decode(&mut r)?;
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(GridOp::Scenarios { sheet, list });
        }
        if kind == 28 {
            let it = match r.u8()? {
                0 => None,
                1 => {
                    let it = crate::book::Iteration { count: r.u32()?, change: r.f64()? };
                    if !it.valid() {
                        return Err(WireError::Invalid("iteration settings"));
                    }
                    Some(it)
                }
                _ => return Err(WireError::Invalid("iteration")),
            };
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(GridOp::Iteration(it));
        }
        if kind == 22 {
            let sheet = r.u16()?;
            let axis = match r.u8()? {
                0 => crate::shift::Axis::Rows,
                1 => crate::shift::Axis::Cols,
                _ => return Err(WireError::Invalid("identity axis")),
            };
            let ids = crate::axis_ids::AxisIds::decode(&mut r)?;
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(GridOp::AxisIdentities { sheet, axis, ids });
        }
        if kind == 21 {
            let sheet = r.u16()?;
            let list = decode_widths(&mut r)?;
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(GridOp::HiddenWidths { sheet, list });
        }
        if kind == 19 {
            let (sheet, rows, cols) = (r.u16()?, r.u32()?, r.u32()?);
            let from = if r.is_empty() { (0, 0) } else { (r.u32()?, r.u32()?) };
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            if rows >= crate::addr::MAX_ROWS || cols >= crate::addr::MAX_COLS || !crate::book::freeze_from_fits((rows, cols), from) {
                return Err(WireError::Invalid("frozen panes"));
            }
            return Ok(GridOp::Freeze { sheet, rows, cols, from });
        }
        if kind == 18 {
            let sheet = r.u16()?;
            let set = crate::links::decode(&mut r)?;
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(GridOp::Links { sheet, set });
        }
        if kind == 17 {
            let sheet = r.u16()?;
            let set = crate::comments::decode(&mut r)?;
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(GridOp::Comments { sheet, set });
        }
        if kind == 16 {
            let sheet = r.u16()?;
            let list = crate::merges::decode(&mut r, sheet)?;
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(GridOp::Merges { sheet, list });
        }
        if kind == 10 || kind == 24 {
            let sheet = r.u16()?;
            let axis = r.u8()?;
            let rows = axis != 0;
            let default = if kind == 24 {
                let value = r.f64()?;
                if axis > 1 || !value.is_finite() || value <= 0.0 {
                    return Err(WireError::Invalid("default size"));
                }
                Some(value)
            } else { None };
            let max = if default.is_some() && !rows { MAX_COLS } else { MAX_ROWS };
            let n = r.count("sizes", max as u64, 9)?;
            let mut over = Vec::with_capacity(n);
            let mut last = None;
            for _ in 0..n {
                let i = r.var()?;
                let v = r.f64()?;
                if i >= max as u64 || last.is_some_and(|l| i <= l) || !v.is_finite() || v < 0.0 {
                    return Err(WireError::Invalid("size"));
                }
                last = Some(i);
                over.push((i as u32, v));
            }
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(match default {
                Some(default) => GridOp::LayoutSizes { sheet, rows, default, over },
                None => GridOp::Sizes { sheet, rows, over },
            });
        }
        if kind == 4 {
            let f = crate::filter::FilterState::decode(&mut r)?;
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(GridOp::Filter(f));
        }
        if kind == 3 {
            let n = r.count("ops", 1 << 16, 1)?;
            let mut ops = Vec::with_capacity(n);
            for _ in 0..n {
                let b = r.bytes("op", b.len() as u64)?;
                if b.first() == Some(&3) {
                    return Err(WireError::Invalid("nested batch"));
                }
                ops.push(GridOp::decode(b)?);
            }
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(GridOp::Batch(ops));
        }
        if kind != 1 && kind != 2 {
            return Err(WireError::Invalid("op kind"));
        }
        let sheet = r.u16()?;
        let row = |r: &mut Reader| -> Result<u32, WireError> { r.len("row", MAX_ROWS as u64 - 1).map(|x| x as u32) };
        let col = |r: &mut Reader| -> Result<u32, WireError> { r.len("column", MAX_COLS as u64 - 1).map(|x| x as u32) };
        let (r0, c0, r1, c1) = (row(&mut r)?, col(&mut r)?, row(&mut r)?, col(&mut r)?);
        if r0 > r1 || c0 > c1 {
            return Err(WireError::Invalid("rectangle"));
        }
        let rect = Rect { sheet, r0, c0, r1, c1 };
        if kind == 2 {
            let n = r.count("style spans", u32::MAX as u64, 8)?;
            let mut spans = Vec::with_capacity(n);
            for _ in 0..n {
                let (x, a, b) = (col(&mut r)?, row(&mut r)?, row(&mut r)?);
                if a > b || x < c0 || x > c1 || a < r0 || b > r1 {
                    return Err(WireError::Invalid("style span outside rectangle"));
                }
                spans.push((x, a, b, Style::decode(&mut r)?));
            }
            if !r.is_empty() {
                return Err(WireError::Invalid("trailing bytes"));
            }
            return Ok(GridOp::Restyle(StyleRegion { rect, spans }));
        }
        let n = r.count("values", u32::MAX as u64, 3)?;
        let mut values = Vec::with_capacity(n);
        for _ in 0..n {
            let (y, x) = (row(&mut r)?, col(&mut r)?);
            if !rect.contains(CellAddr::new(sheet, y, x)) {
                return Err(WireError::Invalid("value outside rectangle"));
            }
            values.push((y, x, read_value(&mut r)?));
        }
        let n = r.count("fills", u32::MAX as u64, 4)?;
        let mut fills = Vec::with_capacity(n);
        for _ in 0..n {
            let (x, a, b) = (col(&mut r)?, row(&mut r)?, row(&mut r)?);
            if a > b || x < c0 || x > c1 || a < r0 || b > r1 {
                return Err(WireError::Invalid("fill outside rectangle"));
            }
            let src = r.str("formula", formula::MAX_STORED_FORMULA_LEN as u64)?.to_string();
            fills.push(Fill { col: x, r0: a, r1: b, src });
        }
        if !r.is_empty() {
            return Err(WireError::Invalid("trailing bytes"));
        }
        Ok(GridOp::Replace(Region { rect, values, fills }))
    }
}

impl Op<Workbook> for GridOp {
    fn apply(self, wb: &mut Workbook) -> Self {
        match self {
            GridOp::Replace(region) => GridOp::Replace(wb.replace(region)),
            GridOp::Restyle(region) => GridOp::Restyle(wb.replace_styles(region)),
            GridOp::Filter(f) => GridOp::Filter(wb.replace_filter(f)),
            GridOp::Tables(ts) => GridOp::Tables(wb.replace_tables(ts)),
            GridOp::Charts(cs) => GridOp::Charts(wb.replace_charts(cs)),
            GridOp::Pictures(pictures) => GridOp::Pictures(wb.replace_pictures(pictures)),
            GridOp::Pivots(ps) => GridOp::Pivots(wb.replace_pivots(ps)),
            GridOp::Validations(b) => {
                let old = wb.encode_validations(wb.validations());
                if let Ok(vs) = wb.decode_validations(&b) {
                    wb.replace_validations(vs);
                }
                GridOp::Validations(old.into())
            }
            GridOp::CondFormats(b) => {
                // A list that no longer reads against the workbook (a sheet it
                // names is gone) changes nothing.
                let old = wb.encode_cond_formats(wb.cond_formats());
                if let Ok(fs) = wb.decode_cond_formats(&b) {
                    wb.replace_cond_formats(fs);
                }
                GridOp::CondFormats(old.into())
            }
            GridOp::Names(b) => {
                let old = wb.encode_names(&wb.names.iter().cloned().collect::<Vec<_>>());
                if let Ok(defs) = wb.decode_names(&b) {
                    wb.replace_names(defs);
                }
                GridOp::Names(old.into())
            }
            GridOp::AddSheet(name) => match wb.add_sheet(&name) {
                Ok(_) => GridOp::DropSheet,
                Err(_) => GridOp::Batch(Vec::new()),
            },
            GridOp::AddIdentifiedSheet { name, identity } => match wb.add_identified_sheet(&name, identity) {
                Ok(_) => GridOp::DropSheet,
                Err(_) => GridOp::Batch(Vec::new()),
            },
            GridOp::DropSheet => {
                let identity = wb.sheets.last().map_or(0, |sheet| sheet.identity);
                match wb.drop_last_sheet() {
                    Some(name) => GridOp::AddIdentifiedSheet { name, identity },
                    None => GridOp::Batch(Vec::new()),
                }
            }
            GridOp::MoveSheet { from, to } => {
                if wb.move_sheet(from, to) {
                    GridOp::MoveSheet { from: to, to: from }
                } else {
                    GridOp::Batch(Vec::new())
                }
            }
            GridOp::HideSheet { sheet, hidden } => {
                if sheet as usize >= wb.sheet_count() || wb.sheet(sheet).sheet_hidden == hidden {
                    return GridOp::Batch(Vec::new());
                }
                if hidden && wb.shown_sheets() <= 1 {
                    return GridOp::Batch(Vec::new());
                }
                wb.sheet_mut(sheet).sheet_hidden = hidden;
                GridOp::HideSheet { sheet, hidden: !hidden }
            }
            GridOp::TabColor { sheet, color } => {
                if sheet as usize >= wb.sheet_count() {
                    return GridOp::Batch(Vec::new());
                }
                let old = std::mem::replace(&mut wb.sheet_mut(sheet).tab_color, color);
                GridOp::TabColor { sheet, color: old }
            }
            GridOp::RenameSheet { sheet, name } => match wb.rename_sheet(sheet, &name) {
                Ok(old) => GridOp::RenameSheet { sheet, name: old },
                Err(_) => GridOp::Batch(Vec::new()),
            },
            GridOp::Links { sheet, set } => {
                if sheet as usize >= wb.sheet_count() {
                    return GridOp::Batch(Vec::new());
                }
                GridOp::Links { sheet, set: wb.set_links(sheet, set) }
            }
            GridOp::Outline { sheet, axis, mut outline } => {
                if let Some(sh) = wb.sheets.get_mut(sheet as usize) {
                    std::mem::swap(&mut sh.outlines[axis.index()], &mut outline);
                    if axis == crate::shift::Axis::Rows {
                        wb.invalidate_subtotals();
                    }
                }
                GridOp::Outline { sheet, axis, outline }
            }
            GridOp::Structure(p) => {
                let old = wb.structure.take();
                wb.structure = p;
                GridOp::Structure(old)
            }
            GridOp::Sparklines { sheet, mut list } => {
                match wb.sheets.get_mut(sheet as usize) {
                    Some(sh) if crate::sparkline::valid_list(&list) => std::mem::swap(&mut sh.sparklines, &mut list),
                    _ => return GridOp::Batch(Vec::new()),
                }
                GridOp::Sparklines { sheet, list }
            }
            GridOp::Scenarios { sheet, mut list } => {
                match wb.sheets.get_mut(sheet as usize) {
                    Some(sh) if crate::scenario::valid_list(&list) => std::mem::swap(&mut sh.scenarios, &mut list),
                    _ => return GridOp::Batch(Vec::new()),
                }
                GridOp::Scenarios { sheet, list }
            }
            GridOp::Calculation(manual) => {
                let old = wb.manual_calc();
                wb.set_manual_calc(manual);
                // Back to automatic: what waited is worked out now.
                if old && !manual {
                    wb.mark_all(None);
                }
                GridOp::Calculation(old)
            }
            GridOp::EditRanges { sheet, mut list } => {
                match wb.sheets.get_mut(sheet as usize) {
                    Some(sh) if crate::sheetareas::valid_ranges(&list) => std::mem::swap(&mut sh.edit_ranges, &mut list),
                    _ => return GridOp::Batch(Vec::new()),
                }
                GridOp::EditRanges { sheet, list }
            }
            GridOp::Protection { sheet, mut protection } => {
                // A sheet's protection is Excel's or ODF's, never the workbook's.
                if protection.as_ref().is_some_and(|p| p.format() == crate::protection::ProtectionFormat::ExcelWorkbook) {
                    return GridOp::Batch(Vec::new());
                }
                match wb.sheets.get_mut(sheet as usize) {
                    Some(sh) => std::mem::swap(&mut sh.protection, &mut protection),
                    None => return GridOp::Batch(Vec::new()),
                }
                GridOp::Protection { sheet, protection }
            }
            GridOp::Properties(p) => GridOp::Properties(std::mem::replace(&mut wb.props, p)),
            GridOp::CustomViews(list) => {
                if !crate::views::valid(&list) {
                    return GridOp::Batch(Vec::new());
                }
                GridOp::CustomViews(std::mem::replace(&mut wb.views, list))
            }
            GridOp::DateSystem1904(on) => {
                use crate::eval::dates::DateSystem;
                let old = wb.date_system == DateSystem::Excel1904;
                wb.date_system = if on { DateSystem::Excel1904 } else { DateSystem::Excel1900 };
                if old != on {
                    wb.mark_all(None);
                }
                GridOp::DateSystem1904(old)
            }
            GridOp::IgnoredErrors { sheet, mut list } => {
                match wb.sheets.get_mut(sheet as usize) {
                    Some(sh) if crate::sheetareas::valid_ignored(&list) => std::mem::swap(&mut sh.ignored, &mut list),
                    _ => return GridOp::Batch(Vec::new()),
                }
                GridOp::IgnoredErrors { sheet, list }
            }
            GridOp::Watches { sheet, mut list } => {
                match wb.sheets.get_mut(sheet as usize) {
                    Some(sh) if crate::watch::valid_list(&list) => std::mem::swap(&mut sh.watches, &mut list),
                    _ => return GridOp::Batch(Vec::new()),
                }
                GridOp::Watches { sheet, list }
            }
            GridOp::Iteration(it) => {
                let old = wb.iteration();
                if !wb.set_iteration(it) {
                    return GridOp::Batch(Vec::new());
                }
                GridOp::Iteration(old)
            }
            GridOp::AxisIdentities { sheet, axis, ids } => {
                if sheet as usize >= wb.sheet_count() {
                    return GridOp::Batch(Vec::new());
                }
                let ids = std::mem::replace(&mut wb.sheet_mut(sheet).axis_ids[axis.index()], ids);
                GridOp::AxisIdentities { sheet, axis, ids }
            }
            GridOp::HiddenWidths { sheet, list } => {
                if sheet as usize >= wb.sheet_count() {
                    return GridOp::Batch(Vec::new());
                }
                let old = std::mem::replace(&mut wb.sheet_mut(sheet).hidden_widths, list);
                GridOp::HiddenWidths { sheet, list: old }
            }
            GridOp::Freeze { sheet, rows, cols, from } => {
                if sheet as usize >= wb.sheet_count() {
                    return GridOp::Batch(Vec::new());
                }
                let new = (rows.min(crate::addr::MAX_ROWS - 1), cols.min(crate::addr::MAX_COLS - 1));
                let from = if crate::book::freeze_from_fits(new, from) { from } else { (0, 0) };
                let sh = wb.sheet_mut(sheet);
                let (rows, cols) = std::mem::replace(&mut sh.freeze, new);
                let from = std::mem::replace(&mut sh.freeze_from, from);
                GridOp::Freeze { sheet, rows, cols, from }
            }
            GridOp::Comments { sheet, set } => {
                if sheet as usize >= wb.sheet_count() {
                    return GridOp::Batch(Vec::new());
                }
                GridOp::Comments { sheet, set: wb.set_comments(sheet, set) }
            }
            GridOp::Merges { sheet, list } => {
                if sheet as usize >= wb.sheet_count() {
                    return GridOp::Batch(Vec::new());
                }
                GridOp::Merges { sheet, list: wb.set_merges(sheet, list) }
            }
            GridOp::LayoutSizes { sheet, rows, default, over } => {
                if sheet as usize >= wb.sheet_count() {
                    return GridOp::Batch(Vec::new());
                }
                let sh = wb.sheet_mut(sheet);
                let sizes = if rows { &mut sh.row_sizes } else { &mut sh.col_sizes };
                let old = GridOp::LayoutSizes {
                    sheet, rows, default: sizes.default, over: sizes.overrides().to_vec(),
                };
                *sizes = crate::column::Sizes::from_parts(default, over);
                if rows {
                    wb.invalidate_subtotals();
                }
                old
            }
            GridOp::Sizes { sheet, rows, over } => {
                if sheet as usize >= wb.sheet_count() {
                    return GridOp::Sizes { sheet, rows, over };
                }
                let sh = wb.sheet_mut(sheet);
                let sizes = if rows { &mut sh.row_sizes } else { &mut sh.col_sizes };
                let old = sizes.overrides().to_vec();
                *sizes = crate::column::Sizes::from_overrides(sizes.default, over);
                if rows {
                    wb.invalidate_subtotals();
                }
                GridOp::Sizes { sheet, rows, over: old }
            }
            GridOp::Batch(ops) => {
                // The inverse undoes them in reverse order.
                let mut inv: Vec<GridOp> = ops.into_iter().map(|op| op.apply(wb)).collect();
                inv.reverse();
                GridOp::Batch(inv)
            }
        }
    }
}

impl Workbook {
    /// The styles of `rect` now, as a [`StyleRegion`].
    pub fn style_snapshot(&self, rect: &Rect) -> StyleRegion {
        let mut spans = Vec::new();
        if let Some(sh) = self.sheets.get(rect.sheet as usize) {
            let last = rect.c1.min(sh.styles.len() as u32);
            for c in rect.c0..=last {
                let Some(runs) = sh.styles.get(c as usize) else { continue };
                for (a, b, id) in runs.spans(rect.r0, rect.r1) {
                    if id != 0 {
                        spans.push((c, a, b, self.styles.get(id).clone()));
                    }
                }
            }
        }
        StyleRegion { rect: *rect, spans }
    }

    /// Replaces the styles of `region.rect`; returns what they were.
    pub fn replace_styles(&mut self, region: StyleRegion) -> StyleRegion {
        let old = self.style_snapshot(&region.rect);
        self.set_style_id(&region.rect, 0);
        for (c, a, b, st) in region.spans {
            let id = self.styles.intern(st);
            self.set_style_id(&Rect { sheet: region.rect.sheet, r0: a, c0: c, r1: b, c1: c }, id);
        }
        old
    }

    /// What `rect` holds now, as a [`Region`].
    pub fn snapshot(&self, rect: &Rect) -> Region {
        let mut values = Vec::new();
        let mut fills = Vec::new();
        let Some(sheet) = self.sheets.get(rect.sheet as usize) else {
            return Region { rect: *rect, values, fills };
        };
        let last = rect.c1.min(sheet.cols.len().max(sheet.fidx.len()) as u32);
        for c in rect.c0..=last {
            // Fills first: they decide which stored cells are formula results.
            let start = fills.len();
            if let Some(f) = sheet.fidx.get(c as usize) {
                let mut hit: Vec<u32> = f.range(..=rect.r1).rev().take_while(|(_, &g)| self.group(g).r1 >= rect.r0).map(|(_, &g)| g).collect();
                hit.reverse();
                for g in hit {
                    let grp = self.group(g);
                    let (a, b) = (grp.r0.max(rect.r0), grp.r1.min(rect.r1));
                    let src = formula::to_a1(&grp.expr, a, c, &|s| self.sheet_name(s));
                    fills.push(Fill { col: c, r0: a, r1: b, src });
                }
            }
            let Some(column) = sheet.cols.get(c as usize) else { continue };
            let mine = &fills[start..];
            let mut k = 0;
            column.each(rect.r0, rect.r1, &mut |r, cell| {
                while k < mine.len() && mine[k].r1 < r {
                    k += 1;
                }
                if k < mine.len() && mine[k].r0 <= r {
                    return;
                }
                values.push((r, c, self.cell_value(cell)));
            });
        }
        Region { rect: *rect, values, fills }
    }

    /// Replaces `region.rect` with `region`; returns what was there.
    pub fn replace(&mut self, region: Region) -> Region {
        let rect = region.rect;
        let old = self.snapshot(&rect);
        if (rect.sheet as usize) >= self.sheets.len() {
            return old;
        }
        // Empty the rectangle.
        let last = rect.c1.min(self.sheets[rect.sheet as usize].cols.len().max(self.sheets[rect.sheet as usize].fidx.len()) as u32);
        for c in rect.c0..=last {
            self.clear_formulas_in(rect.sheet, c, rect.r0, rect.r1);
            let mut rows = Vec::new();
            if let Some(col) = self.sheets[rect.sheet as usize].cols.get(c as usize) {
                col.each(rect.r0, rect.r1, &mut |r, _| rows.push(r));
            }
            for r in rows {
                self.store_cell(rect.sheet, r, c, Cell::Empty);
            }
        }
        self.changed.push(rect);
        for (r, c, v) in &region.values {
            self.store(rect.sheet, *r, *c, v);
        }
        for f in &region.fills {
            if self.fill_formula(rect.sheet, f.col, f.r0, f.r1, &f.src).is_err() {
                // The text no longer parses (a sheet it named is gone): keep the cells
                // visibly broken rather than silently empty.
                for r in f.r0..=f.r1.min(f.r0 + 65_535) {
                    self.store(rect.sheet, r, f.col, &Value::Err(ErrorKind::Ref));
                }
            }
        }
        old
    }
}

// ---------- values on the wire (shared with the native format) ----------

pub(crate) fn write_value(w: &mut Writer, v: &Value) {
    match v {
        Value::Empty => w.u8(0),
        Value::Num(x) => {
            w.u8(1);
            w.f64(*x);
        }
        Value::Str(s) => {
            w.u8(2);
            w.str(s);
        }
        Value::Bool(b) => w.u8(if *b { 4 } else { 3 }),
        Value::Err(e) => {
            w.u8(5);
            w.u8(*e as u8);
        }
    }
}

pub(crate) fn read_value(r: &mut Reader) -> Result<Value, WireError> {
    Ok(match r.u8()? {
        0 => Value::Empty,
        1 => Value::num(r.f64()?),
        2 => Value::str(r.str("text", crate::strings::MAX_TEXT as u64)?),
        3 => Value::Bool(false),
        4 => Value::Bool(true),
        5 => Value::Err(ErrorKind::from_u8(r.u8()?).ok_or(WireError::Invalid("error value"))?),
        _ => return Err(WireError::Invalid("value tag")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use officina_core::history::History;

    fn a(s: &str) -> CellAddr {
        let (r, c) = crate::addr::parse_a1(s).unwrap();
        CellAddr::new(0, r, c)
    }

    #[test]
    fn the_1904_date_system_is_an_op_that_undoes_and_recalculates() {
        use crate::eval::dates::DateSystem;
        let mut wb = Workbook::new();
        wb.add_sheet("One").unwrap();
        let op = GridOp::DateSystem1904(true);
        assert_eq!(GridOp::decode(&op.encode()).unwrap(), op);
        assert_eq!(op.encode(), [38, 1]);
        assert!(GridOp::decode(&[38, 2]).is_err() && GridOp::decode(&[38, 1, 0]).is_err() && GridOp::decode(&[38]).is_err());
        wb.set_input(CellAddr::new(0, 0, 0), "=DATE(1904,1,2)").unwrap();
        wb.recalc();
        assert_eq!(wb.value(0, 0, 0), Value::Num(1463.0));
        let mut h: History<GridOp> = History::new();
        h.run(&mut wb, "Date System", vec![op]);
        assert_eq!(wb.date_system(), DateSystem::Excel1904);
        wb.recalc();
        assert_eq!(wb.value(0, 0, 0), Value::Num(1.0));
        h.undo(&mut wb);
        wb.recalc();
        assert_eq!((wb.date_system(), wb.value(0, 0, 0)), (DateSystem::Excel1900, Value::Num(1463.0)));
    }

    #[test]
    fn sheets_are_hidden_shown_undone_and_saved_but_one_always_shows() {
        let mut wb = Workbook::new();
        wb.add_sheet("One").unwrap();
        wb.add_sheet("Two").unwrap();
        let op = GridOp::HideSheet { sheet: 1, hidden: true };
        assert_eq!(GridOp::decode(&op.encode()).unwrap(), op);
        assert!(GridOp::decode(&[27, 0, 0, 2]).is_err());
        let mut h: History<GridOp> = History::new();
        h.run(&mut wb, "Hide Sheet", vec![op]);
        assert!(wb.sheet(1).sheet_hidden);
        assert_eq!(wb.shown_sheets(), 1);
        // The last sheet showing stays.
        h.run(&mut wb, "Hide Sheet", vec![GridOp::HideSheet { sheet: 0, hidden: true }]);
        assert!(!wb.sheet(0).sheet_hidden);
        // Into the file and back; it moves with its sheet.
        let back = crate::native::load(&crate::native::save_to_vec(&wb)).unwrap();
        assert_eq!((back.sheet(0).sheet_hidden, back.sheet(1).sheet_hidden), (false, true));
        h.run(&mut wb, "Move Sheet", vec![GridOp::MoveSheet { from: 1, to: 0 }]);
        assert_eq!((wb.sheet(0).sheet_hidden, wb.sheet(1).sheet_hidden), (true, false));
        // Undo: the move, the refused hide, then the hide.
        h.undo(&mut wb);
        h.undo(&mut wb);
        assert!(wb.sheet(1).sheet_hidden);
        h.undo(&mut wb);
        assert!(!wb.sheet(1).sheet_hidden);
        // A file hiding every sheet is refused.
        wb.sheet_mut(0).sheet_hidden = true;
        wb.sheet_mut(1).sheet_hidden = true;
        assert!(crate::native::load(&crate::native::save_to_vec(&wb)).is_err());
    }

    #[test]
    fn tab_colours_are_set_undone_and_saved() {
        let mut wb = Workbook::new();
        wb.add_sheet("One").unwrap();
        wb.add_sheet("Two").unwrap();
        let op = GridOp::TabColor { sheet: 1, color: Some(0x2E7D32) };
        assert_eq!(GridOp::decode(&op.encode()).unwrap(), op);
        let none = GridOp::TabColor { sheet: 0, color: None };
        assert_eq!(GridOp::decode(&none.encode()).unwrap(), none);
        let mut h: History<GridOp> = History::new();
        h.run(&mut wb, "Tab Colour", vec![op]);
        assert_eq!(wb.sheet(1).tab_color, Some(0x2E7D32));
        // It goes with the sheet when sheets move, and into the file.
        h.run(&mut wb, "Move Sheet", vec![GridOp::MoveSheet { from: 1, to: 0 }]);
        assert_eq!((wb.sheet(0).tab_color, wb.sheet(1).tab_color), (Some(0x2E7D32), None));
        let back = crate::native::load(&crate::native::save_to_vec(&wb)).unwrap();
        assert_eq!((back.sheet(0).tab_color, back.sheet(1).tab_color), (Some(0x2E7D32), None));
        // A coloured sheet isn't bare: DropSheet leaves it.
        h.undo(&mut wb);
        assert_eq!(wb.drop_last_sheet(), None);
        h.undo(&mut wb);
        assert_eq!(wb.sheet(1).tab_color, None);
        // A sheet that doesn't exist: nothing.
        h.run(&mut wb, "Tab Colour", vec![GridOp::TabColor { sheet: 9, color: Some(1) }]);
        assert_eq!(wb.sheet_count(), 2);
    }

    #[test]
    fn frozen_panes_are_set_undone_and_saved() {
        let mut wb = Workbook::new();
        wb.add_sheet("One").unwrap();
        wb.add_sheet("Two").unwrap();
        let op = GridOp::Freeze { sheet: 1, rows: 1, cols: 2, from: (0, 0) };
        assert_eq!(GridOp::decode(&op.encode()).unwrap(), op);
        assert!(GridOp::decode(&GridOp::Freeze { sheet: 0, rows: crate::addr::MAX_ROWS, cols: 0, from: (0, 0) }.encode()).is_err());
        // The first frozen row must be one of the frozen rows.
        assert!(GridOp::decode(&GridOp::Freeze { sheet: 0, rows: 5, cols: 0, from: (5, 0) }.encode()).is_err());
        assert!(GridOp::decode(&GridOp::Freeze { sheet: 0, rows: 5, cols: 0, from: (2, 1) }.encode()).is_err());
        let mut h: History<GridOp> = History::new();
        h.run(&mut wb, "Freeze Panes", vec![op]);
        assert_eq!(wb.sheet(1).freeze, (1, 2));
        let back = crate::native::load(&crate::native::save_to_vec(&wb)).unwrap();
        assert_eq!((back.sheet(0).freeze, back.sheet(1).freeze), ((0, 0), (1, 2)));
        // A frozen sheet isn't bare; undo unfreezes it.
        assert_eq!(wb.drop_last_sheet(), None);
        h.undo(&mut wb);
        assert_eq!(wb.sheet(1).freeze, (0, 0));
        // Deleting a frozen sheet, then undoing, brings the panes back.
        // Frozen from row 2 (the view started there): kept, saved, undone.
        let op = GridOp::Freeze { sheet: 1, rows: 3, cols: 0, from: (2, 0) };
        assert_eq!(GridOp::decode(&op.encode()).unwrap(), op);
        h.run(&mut wb, "Freeze Panes", vec![op]);
        assert_eq!(wb.sheet(1).freeze_from, (2, 0));
        let back = crate::native::load(&crate::native::save_to_vec(&wb)).unwrap();
        assert_eq!((back.sheet(1).freeze, back.sheet(1).freeze_from), ((3, 0), (2, 0)));
        let del = crate::sheetops::delete_sheet_op(&wb, 1).unwrap();
        h.run(&mut wb, "Delete Sheet", vec![del]);
        assert_eq!(wb.sheet_count(), 1);
        h.undo(&mut wb);
        assert_eq!((wb.sheet(1).freeze, wb.sheet(1).freeze_from), ((3, 0), (2, 0)));
        h.undo(&mut wb);
        assert_eq!((wb.sheet(1).freeze, wb.sheet(1).freeze_from), ((0, 0), (0, 0)));
    }

    #[test]
    fn hidden_widths_are_kept_undone_saved_and_shifted() {
        let mut wb = Workbook::new();
        wb.add_sheet("One").unwrap();
        let op = GridOp::HiddenWidths { sheet: 0, list: vec![(2, 120.0), (5, 30.5)] };
        assert_eq!(GridOp::decode(&op.encode()).unwrap(), op);
        assert!(GridOp::decode(&GridOp::HiddenWidths { sheet: 0, list: vec![(3, 1.0), (2, 1.0)] }.encode()).is_err());
        let mut h: History<GridOp> = History::new();
        h.run(&mut wb, "Hide Columns", vec![op]);
        let back = crate::native::load(&crate::native::save_to_vec(&wb)).unwrap();
        assert_eq!(back.sheet(0).hidden_widths, vec![(2, 120.0), (5, 30.5)]);
        // A column inserted before them moves them on.
        let ins = crate::shift::shift_op(&wb, 0, crate::shift::Axis::Cols, 0, 1).unwrap();
        h.run(&mut wb, "Insert Column", vec![ins]);
        assert_eq!(wb.sheet(0).hidden_widths, vec![(3, 120.0), (6, 30.5)]);
        h.undo(&mut wb);
        h.undo(&mut wb);
        assert!(wb.sheet(0).hidden_widths.is_empty());
    }

    #[test]
    fn sheets_are_added_renamed_and_undone() {
        let mut wb = Workbook::new();
        wb.add_sheet("Sheet1").unwrap();
        let mut h: History<GridOp> = History::new();
        for op in [GridOp::AddSheet("Two".into()), GridOp::DropSheet, GridOp::RenameSheet { sheet: 1, name: "Zwei".into() }] {
            assert_eq!(GridOp::decode(&op.encode()).unwrap(), op);
        }
        h.run(&mut wb, "Add Sheet", vec![GridOp::AddSheet("Two".into())]);
        assert_eq!(wb.sheet_count(), 2);
        wb.set_value(CellAddr::new(1, 0, 0), Value::Num(5.0));
        wb.set_formula(a("A1"), "=Two!A1*2").unwrap();
        wb.recalc();
        h.run(&mut wb, "Rename Sheet", vec![GridOp::RenameSheet { sheet: 1, name: "Zwei".into() }]);
        assert_eq!(wb.formula_text(a("A1")).as_deref(), Some("=Zwei!A1*2"));
        // A name taken (case aside) or bad changes nothing.
        h.run(&mut wb, "Rename Sheet", vec![GridOp::RenameSheet { sheet: 1, name: "sheet1".into() }]);
        h.run(&mut wb, "Rename Sheet", vec![GridOp::RenameSheet { sheet: 1, name: "a/b".into() }]);
        assert_eq!(wb.sheet_name(1), "Zwei");
        h.undo(&mut wb);
        h.undo(&mut wb);
        h.undo(&mut wb);
        assert_eq!(wb.sheet_name(1), "Two");
        // The first sheet stays; a sheet with cells isn't taken away.
        assert_eq!(wb.drop_last_sheet(), None);
        wb.set_value(CellAddr::new(1, 0, 0), Value::Empty);
        h.undo(&mut wb);
        assert_eq!(wb.sheet_count(), 1);
        assert_eq!(wb.drop_last_sheet(), None);
        h.redo(&mut wb);
        assert_eq!(wb.sheet_name(1), "Two");
    }

    #[test]
    fn styles_undo_and_encode() {
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        let mut h: History<GridOp> = History::new();
        wb.set_value(a("B2"), Value::Num(0.256));
        let all = Rect { sheet: 0, r0: 0, c0: 0, r1: 1_048_575, c1: 3 };
        let op = GridOp::restyle(&wb, all, |s| s.clone().with_num_fmt("0.0%"));
        h.run(&mut wb, "Percent", vec![op]);
        assert_eq!(wb.formatted(a("B2")).text, "25.6%");
        let bold = Rect { sheet: 0, r0: 1, c0: 1, r1: 2, c1: 1 };
        let op = GridOp::restyle(&wb, bold, |s| {
            let mut s = s.clone();
            s.font.bold = true;
            s
        });
        assert_eq!(GridOp::decode(&op.encode()).unwrap(), op);
        h.run(&mut wb, "Bold", vec![op]);
        assert!(wb.style(a("B2")).font.bold && wb.style(a("B3")).font.bold && !wb.style(a("B4")).font.bold);
        assert_eq!(&*wb.style(a("B2")).num_fmt, "0.0%");
        // Whole columns stay a few runs.
        assert_eq!(wb.sheet(0).styles[1].runs().len(), 4, "{:?}", wb.sheet(0).styles[1].runs());
        h.undo(&mut wb);
        assert!(!wb.style(a("B2")).font.bold);
        assert_eq!(wb.formatted(a("B2")).text, "25.6%");
        h.undo(&mut wb);
        assert_eq!(wb.formatted(a("B2")).text, "0.256");
        assert!(wb.sheet(0).styles.is_empty());
        h.redo(&mut wb);
        h.redo(&mut wb);
        assert!(wb.style(a("B3")).font.bold);
        assert!(GridOp::decode(&[2, 0, 0]).is_err());
    }

    #[test]
    fn undo_redo_inside_a_fill() {
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        let mut h: History<GridOp> = History::new();
        let n = 10_000u32;
        let vals: Vec<f64> = (0..n).map(|i| i as f64).collect();
        wb.set_numbers(0, 0, 0, &vals);
        h.run(&mut wb, "Fill", vec![GridOp::fill(0, 1, 0, n - 1, "=A1*2")]);
        wb.recalc();
        assert_eq!(wb.groups().count(), 1);
        assert_eq!(wb.get(a("B5000")), Value::Num(9998.0));

        h.run(&mut wb, "Typing", vec![GridOp::set(a("B5000"), Input::Value(Value::str("x")))]);
        wb.recalc();
        assert_eq!(wb.groups().count(), 2);
        assert_eq!(wb.get(a("B5000")), Value::str("x"));

        assert_eq!(h.undo(&mut wb).as_deref(), Some("Typing"));
        wb.recalc();
        assert_eq!(wb.get(a("B5000")), Value::Num(9998.0));
        assert_eq!(wb.groups().count(), 1, "the fill joins up again");
        assert_eq!(wb.formula_text(a("B5000")).as_deref(), Some("=A5000*2"));

        h.redo(&mut wb);
        wb.recalc();
        assert_eq!(wb.get(a("B5000")), Value::str("x"));

        h.undo(&mut wb);
        h.undo(&mut wb);
        wb.recalc();
        assert_eq!(wb.groups().count(), 0);
        assert_eq!(wb.get(a("B5000")), Value::Empty);
        assert_eq!(wb.get(a("A5000")), Value::Num(4999.0));
    }

    #[test]
    fn tables_and_charts_undo_and_encode() {
        use crate::charts::*;
        use crate::tables::Table;
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        for r in 0..5 {
            wb.set_input(CellAddr::new(0, r, 0), &format!("{r}")).unwrap();
        }
        wb.set_input(a("A1"), "Qty").unwrap();
        wb.set_input(a("C1"), "=SUM(T[Qty])").unwrap();
        wb.recalc();
        assert_eq!(wb.get(a("C1")), Value::Err(ErrorKind::Ref));
        let t = Table { name: "T".into(), rect: Rect { sheet: 0, r0: 0, c0: 0, r1: 4, c1: 0 }, header: true, totals: false, columns: vec!["Qty".into()], style: None, stripes: true, filter: None };
        let ch = Chart {
            kind: ChartKind::Column,
            grouping: Grouping::Standard,
            title: None,
            series: vec![Series { name: None, cats: None, vals: Rect { sheet: 0, r0: 1, c0: 0, r1: 4, c1: 0 }, kind: None }],
            legend: None,
            x_title: None,
            y_title: None,
            anchor: Anchor { sheet: 0, from: (1, 3, 0.0, 0.0), to: (10, 8, 0.0, 0.0) },
        };
        let ops = vec![GridOp::Tables(vec![t.clone()]), GridOp::Charts(vec![ch.clone()])];
        for op in &ops {
            assert_eq!(&GridOp::decode(&op.encode()).unwrap(), op);
        }
        let mut h: History<GridOp> = History::new();
        h.run(&mut wb, "Format as Table", vec![ops[0].clone()]);
        h.run(&mut wb, "Insert Chart", vec![ops[1].clone()]);
        wb.recalc();
        assert_eq!(wb.get(a("C1")), Value::Num(10.0), "the formula found the new table");
        assert_eq!(wb.charts(), &[ch]);
        h.undo(&mut wb);
        assert!(wb.charts().is_empty());
        h.undo(&mut wb);
        wb.recalc();
        assert!(wb.tables().is_empty());
        assert_eq!(wb.get(a("C1")), Value::Err(ErrorKind::Ref));
        h.redo(&mut wb);
        wb.recalc();
        assert_eq!(wb.get(a("C1")), Value::Num(10.0));
        // A table that doesn't fit (a taken name) is left out, not applied half.
        let mut bad = t.clone();
        bad.name = "Qty Table".into();
        assert!(matches!(GridOp::Tables(vec![bad]).apply(&mut wb), GridOp::Tables(old) if old.len() == 1));
        assert!(wb.tables().is_empty());
    }

    #[test]
    fn block_paste_and_undo() {
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        wb.set_input(a("A1"), "old").unwrap();
        wb.set_input(a("C3"), "=1+1").unwrap();
        let before = wb.snapshot(&Rect { sheet: 0, r0: 0, c0: 0, r1: 9, c1: 9 });
        let mut h: History<GridOp> = History::new();
        let rows = vec![
            vec![Input::Value(Value::Num(1.0)), Input::Formula("=A1+1".into())],
            vec![Input::Empty, Input::Formula("=A2+1".into())],
        ];
        h.run(&mut wb, "Paste", vec![GridOp::block(0, 0, 0, rows)]);
        wb.recalc();
        assert_eq!(wb.get(a("B1")), Value::Num(2.0));
        assert_eq!(wb.get(a("B2")), Value::Num(1.0));
        assert_eq!(wb.groups().count(), 2, "B1:B2 is one fill, plus C3");
        h.undo(&mut wb);
        wb.recalc();
        assert_eq!(wb.snapshot(&Rect { sheet: 0, r0: 0, c0: 0, r1: 9, c1: 9 }), before);
    }

    #[test]
    fn journal_round_trip_and_garbage() {
        let op = GridOp::block(0, 3, 4, vec![vec![Input::Value(Value::str("é")), Input::Formula("=SUM(A1:A9)".into())], vec![Input::Value(Value::Err(ErrorKind::NA)), Input::Value(Value::Bool(true))]]);
        let b = op.encode();
        assert_eq!(GridOp::decode(&b).unwrap(), op);
        for i in 0..b.len() {
            let _ = GridOp::decode(&b[..i]);
            let mut c = b.clone();
            c[i] ^= 0x5a;
            let _ = GridOp::decode(&c);
        }
    }
}
