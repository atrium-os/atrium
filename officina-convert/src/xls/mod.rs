//! `.xls` in (Excel 97–2003, BIFF8 in a compound file, MS-XLS), for the
//! converter process.
//!
//! Read: sheets and their names, values (numbers, shared and inline strings,
//! booleans, errors), formulas (decompiled to text, then parsed as `.xlsx`
//! formulas are; shared formulas per cell, array formulas at their first
//! cell, data tables as their values), defined names, number formats, fonts,
//! fills, borders, alignment, column widths and row heights, hidden rows,
//! the 1904 date system. A file protected with a password (RC4 or RC4
//! CryptoAPI) is decrypted here, inside the sandbox; one protected only
//! against writing opens with Excel's default password.
//!
//! Reported, not carried: merged cells, drawings and
//! charts, conditional formats, and macros. Refused: Excel 95
//! and older (BIFF5 and before) and XOR obfuscation.

mod annotations;
pub mod biff;
pub mod formula;
mod ftab;
#[doc(hidden)]
pub mod sample;
mod validation;

use std::collections::HashMap;
use std::sync::Arc;

use officina_grid::addr::{a1, CellAddr};
use officina_grid::column::Sizes;
use officina_grid::numfmt::{self, PALETTE};
use officina_grid::style::{Edge, Font, HAlign, Line, PatternFill, Style, StyleId, VAlign};
use officina_grid::{ErrorKind, Value, Workbook};

use crate::cfb::{self, Cfb};
use crate::crypt::CryptError;
use crate::report::{Kind, Report};
use crate::xlsx::read::{apply_styles, differential, place_formulas, FCell, Loaded, Src, MDW};

use biff::{Cur, Record, Short};
use formula::{Book, Ctx, SupBook, Why};

#[derive(Debug)]
pub enum XlsError {
    NeedsPassword,
    WrongPassword,
    Bad(String),
}

impl std::fmt::Display for XlsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            XlsError::NeedsPassword => f.write_str(crate::launch::NEEDS_PASSWORD),
            XlsError::WrongPassword => f.write_str(crate::launch::WRONG_PASSWORD),
            XlsError::Bad(e) => write!(f, "not a workbook this can read: {e}"),
        }
    }
}

impl std::error::Error for XlsError {}

fn bad(s: impl Into<String>) -> XlsError {
    XlsError::Bad(s.into())
}

impl From<Short> for XlsError {
    fn from(_: Short) -> Self {
        bad("a record ended early")
    }
}

/// Records a sheet may hold.
const MAX_RECORDS: usize = 1 << 24;

/// Whether `data` is an Excel 97–2003 workbook (a compound file with a
/// `Workbook` stream, or Excel 95's `Book`).
pub fn is_xls(data: &[u8]) -> bool {
    cfb::is_cfb(data) && Cfb::open(data).is_ok_and(|c| c.has("Workbook") || c.has("Book"))
}

struct FontRec {
    twips: u16,
    italic: bool,
    strike: bool,
    color: u16,
    bold: bool,
    underline: bool,
    name: String,
}

struct NameRec {
    /// As formulas name it (`_xlnm.` for built-in names).
    name: String,
    builtin: bool,
    func: bool,
    itab: u16,
    rgce: Vec<u8>,
    rgcb: Vec<u8>,
}

struct SheetRec {
    name: String,
    at: usize,
    hidden: bool,
    dt: u8,
}

#[derive(Default)]
struct Globals {
    sheets: Vec<SheetRec>,
    sst: Vec<Arc<str>>,
    fonts: Vec<FontRec>,
    formats: HashMap<u16, String>,
    xfs: Vec<Vec<u8>>,
    palette: Vec<u32>,
    date1904: bool,
    names: Vec<NameRec>,
    xti: Vec<(u16, i16, i16)>,
    books: Vec<SupBook>,
}

const BUILTIN_NAMES: [&str; 14] = [
    "Consolidate_Area",
    "Auto_Open",
    "Auto_Close",
    "Extract",
    "Database",
    "Criteria",
    "Print_Area",
    "Print_Titles",
    "Recorder",
    "Data_Form",
    "Auto_Activate",
    "Auto_Deactivate",
    "Sheet_Title",
    "_FilterDatabase",
];

fn globals(recs: &[Record], rep: &mut Report) -> Result<Globals, XlsError> {
    let mut g = Globals { palette: PALETTE.to_vec(), ..Default::default() };
    for r in recs {
        let mut c = Cur::new(r);
        match r.kind {
            biff::BOUNDSHEET => {
                let at = c.u32()? as usize;
                let state = c.u8()? & 3;
                let dt = c.u8()?;
                let name = c.short_string()?;
                g.sheets.push(SheetRec { name, at, hidden: state != 0, dt });
            }
            0x00FC => {
                // SST: the shared strings.
                let _total = c.u32()?;
                let unique = c.u32()? as usize;
                // Each string takes at least 3 bytes.
                g.sst.reserve(unique.min(c.left() / 3));
                for _ in 0..unique {
                    if c.left() == 0 {
                        break;
                    }
                    let cch = c.u16()? as usize;
                    let s = c.rich_tail(cch)?;
                    g.sst.push(Arc::from(s));
                }
            }
            0x0031 => {
                let twips = c.u16()?;
                let grbit = c.u16()?;
                let color = c.u16()?;
                let bls = c.u16()?;
                let _sss = c.u16()?;
                let uls = c.u8()?;
                c.skip(3)?;
                let name = c.short_string()?;
                g.fonts.push(FontRec { twips, italic: grbit & 2 != 0, strike: grbit & 8 != 0, color, bold: bls >= 600, underline: uls != 0, name });
            }
            0x041E => {
                let id = c.u16()?;
                let code = c.xl_string()?;
                g.formats.insert(id, code);
            }
            0x00E0 => {
                if g.xfs.len() < crate::xlsx::styles::MAX_XFS {
                    g.xfs.push(r.data.clone());
                }
            }
            0x0092 => {
                let n = c.u16()? as usize;
                for i in 0..n.min(56) {
                    let b = c.bytes(4)?;
                    g.palette[8 + i] = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
                }
            }
            0x0022 => g.date1904 = c.u16()? == 1,
            0x0018 => {
                let flags = c.u16()?;
                let _key = c.u8()?;
                let cch = c.u8()? as usize;
                let cce = c.u16()? as usize;
                c.skip(2)?;
                let itab = c.u16()?;
                c.skip(4)?;
                let high = c.u8()? & 1 != 0;
                let raw = c.chars(cch, high)?;
                let builtin = flags & 0x20 != 0;
                let name = if builtin {
                    let k = raw.chars().next().map_or(u32::MAX, |c| c as u32) as usize;
                    format!("_xlnm.{}", BUILTIN_NAMES.get(k).copied().unwrap_or("Unknown"))
                } else {
                    raw
                };
                let rgce = c.bytes(cce.min(c.left()))?.to_vec();
                let rgcb = c.bytes(c.left())?.to_vec();
                if g.names.len() < officina_grid::names::MAX_NAMES {
                    g.names.push(NameRec { name, builtin, func: flags & 0x02 != 0, itab, rgce, rgcb });
                }
            }
            0x0017 => {
                let n = c.u16()? as usize;
                for _ in 0..n.min(c.left() / 6) {
                    g.xti.push((c.u16()?, c.i16()?, c.i16()?));
                }
            }
            0x01AE => {
                let _ctab = c.u16()?;
                let cch = c.u16()?;
                let book = match cch {
                    0x0401 => Book::Own,
                    0x3A01 => Book::AddIn,
                    _ => {
                        rep.note(Kind::Inert, "links to other workbooks (cached values kept, never refreshed)");
                        Book::Other
                    }
                };
                g.books.push(SupBook { book, names: Vec::new() });
            }
            0x0023 => {
                // EXTERNNAME, for the last SUPBOOK: its name after the options.
                c.skip(6)?;
                let name = c.short_string().unwrap_or_default();
                if let Some(b) = g.books.last_mut() {
                    if b.names.len() < 1 << 16 {
                        b.names.push(name);
                    }
                }
            }
            0x01B8 => rep.note(Kind::Dropped, "hyperlinks"),
            _ => {}
        }
    }
    Ok(g)
}

fn color(g: &Globals, icv: u16) -> Option<u32> {
    g.palette.get(icv as usize).copied()
}

fn font_at(fonts: &[FontRec], index: u16) -> Option<&FontRec> {
    let index = index as usize;
    fonts.get(if index >= 4 { index - 1 } else { index })
}

/// The workbook's cell formats, in `XF` order.
fn styles(g: &Globals, rep: &mut Report) -> Vec<Style> {
    let font = |i| font_at(&g.fonts, i);
    let base = g.fonts.first();
    let mut out = Vec::with_capacity(g.xfs.len());
    for x in &g.xfs {
        if x.len() < 20 {
            out.push(Style::default());
            continue;
        }
        let u16_at = |o: usize| u16::from_le_bytes([x[o], x[o + 1]]);
        let u32_at = |o: usize| u32::from_le_bytes([x[o], x[o + 1], x[o + 2], x[o + 3]]);
        let mut s = Style::default();
        let ifmt = u16_at(2);
        match g.formats.get(&ifmt).cloned().or_else(|| numfmt::builtin(ifmt as u32).map(str::to_string)) {
            Some(c) => {
                if !numfmt::NumFmt::parse(&c).is_exact() {
                    rep.note_at(Kind::Approximate, "number formats with parts shown plainly", || c.clone());
                }
                s.num_fmt = Arc::from(c.as_str());
            }
            None => rep.note_at(Kind::Approximate, "locale-specific built-in number formats shown as General", || format!("format {ifmt}")),
        }
        if let Some(f) = font(u16_at(0)) {
            let name = Some(&f.name).filter(|n| base.is_none_or(|b| b.name != **n));
            let size = Some(f.twips).filter(|t| base.is_none_or(|b| b.twips != *t) && (20..8200).contains(t));
            let col = if f.color >= 64 { None } else { color(g, f.color) };
            let base_col = base.and_then(|b| if b.color >= 64 { None } else { color(g, b.color) });
            s.font = Font {
                name: name.map(|n| Arc::from(n.as_str())),
                size: size.map(|t| t as u16),
                bold: f.bold,
                italic: f.italic,
                underline: f.underline,
                strike: f.strike,
                color: col.filter(|c| Some(*c) != base_col),
            };
        }
        let al = x[6];
        s.h_align = match al & 7 {
            1 => HAlign::Left,
            2 => HAlign::Center,
            3 => HAlign::Right,
            4 => HAlign::Fill,
            5 => HAlign::Justify,
            6 => HAlign::CenterAcross,
            7 => HAlign::Distributed,
            _ => HAlign::General,
        };
        s.wrap = al & 8 != 0;
        s.v_align = match (al >> 4) & 7 {
            0 => VAlign::Top,
            1 => VAlign::Center,
            3 => VAlign::Justify,
            4 => VAlign::Distributed,
            _ => VAlign::Bottom,
        };
        s.rotation = match x[7] {
            v @ 0..=90 => v as i16,
            v @ 91..=180 => -((v - 90) as i16),
            255 => 255,
            _ => 0,
        };
        s.indent = x[8] & 0x0F;
        s.shrink_to_fit = x[8] & 0x10 != 0;
        let (b1, b2, cols) = (u32_at(10), u32_at(14), u16_at(18));
        let line = |d: u32| Line::ALL.get(d as usize).copied().unwrap_or(Line::None);
        let edge = |d: u32, icv: u32| {
            let l = line(d);
            Edge { line: l, color: if l == Line::None || icv >= 64 { None } else { color(g, icv as u16) } }
        };
        s.borders = [edge(b1 & 0xF, (b1 >> 16) & 0x7F), edge((b1 >> 4) & 0xF, (b1 >> 23) & 0x7F), edge((b1 >> 8) & 0xF, b2 & 0x7F), edge((b1 >> 12) & 0xF, (b2 >> 7) & 0x7F)];
        let diagonal = edge((b2 >> 21) & 0xF, (b2 >> 14) & 0x7F);
        if b1 & (1 << 30) != 0 {
            s.diagonals[0] = diagonal;
        }
        if b1 & (1 << 31) != 0 {
            s.diagonals[1] = diagonal;
        }
        let fls = b2 >> 26;
        if fls != 0 {
            let resolve = |index: u16| (index < 64).then(|| color(g, index)).flatten();
            let fore = resolve(cols & 0x7F);
            let back = resolve((cols >> 7) & 0x7F);
            s.fill = fore;
            if fls != 1 {
                const PATTERNS: [&str; 17] = [
                    "mediumGray", "darkGray", "lightGray", "darkHorizontal", "darkVertical",
                    "darkDown", "darkUp", "darkGrid", "darkTrellis", "lightHorizontal",
                    "lightVertical", "lightDown", "lightUp", "lightGrid", "lightTrellis", "gray125", "gray0625",
                ];
                s.pattern = PATTERNS.get(fls as usize - 2).and_then(|name| PatternFill::new(name, fore, back));
                if s.pattern.is_none() {
                    rep.note(Kind::Approximate, "unsupported patterned fills shown solid");
                }
            }
        }
        out.push(s);
    }
    if out.is_empty() {
        out.push(Style::default());
    }
    out
}

/// An RK number (MS-XLS §2.5.217).
fn rk(v: u32) -> f64 {
    let x = if v & 2 != 0 { ((v as i32) >> 2) as f64 } else { f64::from_bits(((v & 0xFFFF_FFFC) as u64) << 32) };
    if v & 1 != 0 {
        x / 100.0
    } else {
        x
    }
}

fn error_of(code: u8) -> Value {
    let k = match code {
        0x00 => ErrorKind::from_text("#NULL!"),
        0x07 => ErrorKind::from_text("#DIV/0!"),
        0x0F => ErrorKind::from_text("#VALUE!"),
        0x17 => ErrorKind::from_text("#REF!"),
        0x1D => ErrorKind::from_text("#NAME?"),
        0x24 => ErrorKind::from_text("#NUM!"),
        0x2A => ErrorKind::from_text("#N/A"),
        _ => None,
    };
    Value::Err(k.unwrap_or(ErrorKind::Value))
}

/// A formula cell as the file holds it, resolved once the sheet is read.
struct RawF {
    row: u32,
    col: u32,
    rgce: Vec<u8>,
    rgcb: Vec<u8>,
    cached: Value,
}

/// A shared or array formula: its area and parsed formula.
struct Part {
    area: (u32, u32, u32, u32),
    rgce: Vec<u8>,
    rgcb: Vec<u8>,
}

#[derive(Default)]
struct SheetOut {
    nums: Vec<(u32, u32, f64)>,
    styles: Vec<(u32, u32, u32)>,
    raw: Vec<RawF>,
    shared: HashMap<(u32, u32), Part>,
    arrays: HashMap<(u32, u32), Part>,
    tables: u64,
}

/// One worksheet substream into sheet `s`.
#[allow(clippy::too_many_arguments)]
fn sheet(wb: &mut Workbook, s: u16, recs: &[Record], g: &Globals, ctx: &Ctx, xf_ids: &[StyleId], out: &mut Loaded, rep: &mut Report) -> Result<SheetOut, XlsError> {
    let mut so = SheetOut::default();
    let mut widths: Vec<(u32, f64)> = Vec::new();
    let mut heights: Vec<(u32, f64)> = Vec::new();
    let mut hidden: Vec<u32> = Vec::new();
    let mut col_styles: Vec<(u32, u32, u32)> = Vec::new();
    let mut default_w: Option<f64> = None;
    let mut default_h: Option<f64> = None;
    // The formula whose string result the next STRING record holds.
    let mut want_string: Option<usize> = None;
    let mut depth = 0u32;
    let mut row_styles = Vec::new();
    let mut nested_note = false;
    let mut note_object = None;
    let mut note_text = HashMap::new();
    let mut notes = Vec::new();
    let mut link_cells = 0u64;
    let cell_at = |r: u32, c: u32| -> bool { c < formula::COLS && r < formula::ROWS };
    for (i, r) in recs.iter().enumerate() {
        if i == 0 {
            continue;
        }
        // Substreams inside this one (charts on the sheet).
        if r.kind == biff::BOF {
            depth += 1;
            if !nested_note {
                rep.note(Kind::Dropped, "charts");
                nested_note = true;
            }
            continue;
        }
        if depth > 0 {
            if r.kind == biff::EOF {
                depth -= 1;
            }
            continue;
        }
        let mut c = Cur::new(r);
        let kind = r.kind;
        if kind != 0x0207 && kind != 0x04BC && kind != 0x0221 && kind != 0x0236 {
            want_string = None;
        }
        match kind {
            // NUMBER, RK, LABELSST, LABEL, BOOLERR, BLANK.
            0x0203 | 0x027E | 0x00FD | 0x0204 | 0x0205 | 0x0201 => {
                let (row, col, xf) = (c.u16()? as u32, c.u16()? as u32, c.u16()? as u32);
                if !cell_at(row, col) {
                    continue;
                }
                so.styles.push((col, row, xf));
                let v = match kind {
                    0x0203 => Value::num(c.f64()?),
                    0x027E => Value::num(rk(c.u32()?)),
                    0x00FD => {
                        let i = c.u32()? as usize;
                        Value::Str(g.sst.get(i).cloned().ok_or_else(|| bad("shared string index out of range"))?)
                    }
                    0x0204 => Value::str(&c.xl_string()?),
                    0x0205 => {
                        let v = c.u8()?;
                        if c.u8()? != 0 {
                            error_of(v)
                        } else {
                            Value::Bool(v != 0)
                        }
                    }
                    _ => Value::Empty,
                };
                if kind != 0x0201 {
                    out.cells += 1;
                }
                match v {
                    Value::Num(x) if x.is_finite() => so.nums.push((col, row, x)),
                    Value::Num(_) | Value::Empty => {}
                    v => wb.set_value(CellAddr::new(s, row, col), v),
                }
            }
            0x00BD | 0x00BE => {
                // MULRK, MULBLANK.
                let row = c.u16()? as u32;
                let first = c.u16()? as u32;
                let each = if kind == 0x00BD { 6 } else { 2 };
                let n = c.left().saturating_sub(2) / each;
                for k in 0..n as u32 {
                    let col = first + k;
                    let xf = c.u16()? as u32;
                    let x = if kind == 0x00BD { Some(rk(c.u32()?)) } else { None };
                    if !cell_at(row, col) {
                        continue;
                    }
                    so.styles.push((col, row, xf));
                    if let Some(x) = x {
                        out.cells += 1;
                        so.nums.push((col, row, x));
                    }
                }
            }
            0x0006 => {
                let (row, col, xf) = (c.u16()? as u32, c.u16()? as u32, c.u16()? as u32);
                let val = c.bytes(8)?;
                let _grbit = c.u16()?;
                c.skip(4)?;
                let cce = c.u16()? as usize;
                let rgce = c.bytes(cce)?.to_vec();
                let rgcb = c.bytes(c.left())?.to_vec();
                if !cell_at(row, col) {
                    continue;
                }
                so.styles.push((col, row, xf));
                out.cells += 1;
                let cached = if val[6] == 0xFF && val[7] == 0xFF {
                    match val[0] {
                        0 => Value::str(""),
                        1 => Value::Bool(val[2] != 0),
                        2 => error_of(val[2]),
                        _ => Value::str(""),
                    }
                } else {
                    Value::num(f64::from_le_bytes(val.try_into().unwrap()))
                };
                if val[6] == 0xFF && val[7] == 0xFF && val[0] == 0 {
                    want_string = Some(so.raw.len());
                }
                if so.raw.len() >= crate::xlsx::read::MAX_FORMULAS {
                    return Err(bad("too many formulas"));
                }
                so.raw.push(RawF { row, col, rgce, rgcb, cached });
            }
            0x0207 => {
                if let Some(i) = want_string.take() {
                    let t = c.xl_string()?;
                    so.raw[i].cached = Value::str(&t);
                }
            }
            0x04BC | 0x0221 => {
                let (r0, r1) = (c.u16()? as u32, c.u16()? as u32);
                let (c0, c1) = (c.u8()? as u32, c.u8()? as u32);
                c.skip(if kind == 0x04BC { 2 } else { 6 })?;
                let cce = c.u16()? as usize;
                let rgce = c.bytes(cce)?.to_vec();
                let rgcb = c.bytes(c.left())?.to_vec();
                let p = Part { area: (r0.min(r1), c0.min(c1), r0.max(r1), c0.max(c1)), rgce, rgcb };
                let key = (p.area.0, p.area.1);
                if kind == 0x04BC {
                    so.shared.insert(key, p);
                } else {
                    so.arrays.insert(key, p);
                }
            }
            0x0236 => so.tables += 1,
            0x0208 => {
                let row = c.u16()? as u32;
                c.skip(4)?;
                let miy = c.u16()?;
                c.skip(4)?;
                let grbit = c.u32()?;
                if row >= formula::ROWS {
                    continue;
                }
                if grbit & 0x40 != 0 {
                    heights.push((row, ((miy & 0x7FFF) as f64 / 20.0 * 96.0 / 72.0).round().max(2.0)));
                }
                if grbit & 0x20 != 0 {
                    hidden.push(row);
                }
                if grbit & 0x80 != 0 {
                    row_styles.push((row, (grbit >> 16) & 0xFFF));
                }
            }
            0x007D => {
                let (first, last) = (c.u16()? as u32, c.u16()? as u32);
                let dx = c.u16()?;
                let xf = c.u16()? as u32;
                let flags = c.u16()?;
                let (first, last) = (first.min(formula::COLS - 1), last.min(formula::COLS - 1));
                if first > last {
                    continue;
                }
                let w = if flags & 1 != 0 {
                    rep.note(Kind::Approximate, "hidden columns shown narrow");
                    0.0
                } else {
                    dx as f64 / 256.0
                };
                let px = (w * MDW).round().max(2.0);
                for col in first..=last {
                    widths.push((col, px));
                }
                col_styles.push((first, last, xf));
            }
            0x0055 => {
                let cch = c.u16()?;
                if cch != 8 && cch < 256 {
                    default_w = Some(cch as f64 + 0.71);
                }
            }
            0x0099 => {
                let w = c.u16()? as f64 / 256.0;
                if w > 0.0 {
                    default_w = Some(w);
                }
            }
            0x0225 => {
                let _flags = c.u16()?;
                let miy = c.u16()?;
                if (1..8200).contains(&miy) {
                    default_h = Some(miy as f64 / 20.0);
                }
            }
            0x00E5 => rep.note(Kind::Approximate, "merged cells shown unmerged"),
            0x001C => match annotations::Note::read(r) {
                Ok(note) if notes.len() < officina_grid::comments::MAX_COMMENTS => notes.push(note),
                _ => rep.note(Kind::Dropped, "malformed or excessive BIFF8 notes"),
            },
            0x01B6 => {
                if let Some(object) = note_object.take() {
                    if let Ok((text, damaged)) = annotations::Text::read(r, &g.fonts) {
                        if damaged {
                            rep.note(Kind::Approximate, "BIFF8 notes with damaged formatting (kept the text)");
                        }
                        note_text.entry(object).or_insert(text);
                    }
                }
            }
            0x01B8 => match annotations::hyperlink(s, r) {
                Ok((rect, link)) => {
                    let cells = u64::from(rect.r1 - rect.r0 + 1) * u64::from(rect.c1 - rect.c0 + 1);
                    if link_cells + cells <= officina_grid::links::MAX_LINKS as u64 {
                        let mut links = Vec::with_capacity(cells as usize);
                        for row in rect.r0..=rect.r1 {
                            for col in rect.c0..=rect.c1 {
                                links.push((row, col, Some(link.clone())));
                            }
                        }
                        wb.set_links(s, links);
                        link_cells += cells;
                    } else {
                        rep.note(Kind::Dropped, "BIFF8 hyperlink ranges past the sheet limit");
                    }
                }
                Err(_) => rep.note(Kind::Dropped, "malformed or unsupported BIFF8 hyperlinks"),
            },
            0x01BE => {
                if wb.validations().len() >= officina_grid::validation::MAX_VALIDATIONS {
                    rep.note(Kind::Dropped, "BIFF8 data validation rules past the limits");
                } else {
                    match validation::read(wb, s, r, ctx) {
                        Ok((rule, ime)) => {
                            if wb.add_validation(rule).is_err() {
                                rep.note(Kind::Dropped, "BIFF8 data validation rules past the limits");
                            } else if ime {
                                rep.note(Kind::Approximate, "BIFF8 data validation input method settings");
                            }
                        }
                        Err(reason) => rep.note(Kind::Dropped, reason),
                    }
                }
            }
            0x01B0 | 0x01B1 => rep.note(Kind::Dropped, "conditional formats"),
            0x005D => {
                // OBJ: its common data names the kind of object.
                note_object = None;
                c.skip(4)?;
                match c.u16()? {
                    0x19 => note_object = c.u16().ok(),
                    0x05 => rep.note(Kind::Dropped, "charts"),
                    0x08 => rep.note(Kind::Dropped, "pictures"),
                    0x07 | 0x0B | 0x0C | 0x0E | 0x10 | 0x11 | 0x12 | 0x14 => rep.note(Kind::Dropped, "form controls"),
                    _ => rep.note(Kind::Dropped, "shapes and text boxes"),
                }
            }
            0x009D => rep.note(Kind::Approximate, "filters (rows stay as the file hid them)"),
            0x00B0 => rep.note(Kind::Approximate, "pivot tables kept as their values"),
            0x023E => {
                // WINDOW2: frozen panes.
                if c.u16()? & 0x0008 != 0 {
                    rep.note(Kind::Dropped, "frozen panes");
                }
            }
            _ => {}
        }
    }
    let mut comments = Vec::with_capacity(notes.len());
    for note in notes {
        if let Some(text) = note_text.remove(&note.object) {
            comments.push((note.row, note.col, Some(text.comment(&note.author))));
        } else {
            rep.note(Kind::Dropped, "BIFF8 notes without their comment text");
        }
    }
    wb.set_comments(s, comments);
    // Numbers, a run down a column at a time.
    so.nums.sort_by_key(|n| (n.0, n.1));
    so.nums.dedup_by(|b, a| {
        if (a.0, a.1) == (b.0, b.1) {
            a.2 = b.2;
            true
        } else {
            false
        }
    });
    let mut i = 0;
    while i < so.nums.len() {
        let (col, r0) = (so.nums[i].0, so.nums[i].1);
        let mut v = vec![so.nums[i].2];
        let mut j = i + 1;
        while j < so.nums.len() && so.nums[j].0 == col && so.nums[j].1 == r0 + v.len() as u32 {
            v.push(so.nums[j].2);
            j += 1;
        }
        wb.set_numbers(s, col, r0, &v);
        i = j;
    }
    // Formula cells keep the file's results until they are placed.
    for f in &so.raw {
        if !matches!(f.cached, Value::Empty) {
            wb.set_value(CellAddr::new(s, f.row, f.col), f.cached.clone());
        }
    }
    let sh = wb.sheet_mut(s);
    if let Some(w) = default_w {
        sh.col_sizes = Sizes::new((w * MDW).round().max(2.0));
    }
    if let Some(h) = default_h {
        sh.row_sizes = Sizes::new((h * 96.0 / 72.0).round().max(2.0));
    }
    if !widths.is_empty() {
        sh.col_sizes = Sizes::from_overrides(sh.col_sizes.default, widths);
    }
    if !heights.is_empty() {
        sh.row_sizes = Sizes::from_overrides(sh.row_sizes.default, heights);
    }
    sh.hidden = officina_grid::filter::merge(hidden.into_iter().map(|r| (r, r)).collect());
    // Styles: runs of one format down each column.
    let mut st = std::mem::take(&mut so.styles);
    st.sort_by_key(|x| (x.0, x.1));
    let mut sruns: Vec<Vec<(u32, u32, u32)>> = Vec::new();
    for (col, row, xf) in st {
        let ci = col as usize;
        if sruns.len() <= ci {
            sruns.resize_with(ci + 1, Vec::new);
        }
        let runs = &mut sruns[ci];
        match runs.last_mut() {
            Some(l) if l.1 == row => l.2 = xf,
            Some(l) if l.2 == xf && l.1 + 1 == row => l.1 = row,
            _ => runs.push((row, row, xf)),
        }
    }
    apply_styles(wb, s, col_styles, row_styles, formula::COLS, &sruns, xf_ids, rep);
    Ok(so)
}

fn why_note(rep: &mut Report, why: Why, at: impl FnOnce() -> String) {
    match why {
        Why::Damaged => rep.note_at(Kind::Approximate, "damaged formulas kept as their values", at),
        Why::Unsupported(w) => rep.note_at(Kind::Approximate, &format!("formulas with {w}, kept as their values"), at),
    }
}

/// The `Workbook` stream, decrypted if it is protected.
fn workbook_stream(data: &[u8], password: Option<&str>) -> Result<Vec<u8>, XlsError> {
    if !cfb::is_cfb(data) {
        let head = String::from_utf8_lossy(&data[..data.len().min(512)]).to_ascii_lowercase();
        return Err(bad(if head.contains("<?xml") || head.contains("<html") || head.contains("<table") {
            "this .xls is really an XML or HTML spreadsheet, which isn't supported"
        } else if data.len() >= 2 && data[0] == 0x09 && matches!(data[1], 0x00 | 0x02 | 0x04 | 0x08) {
            "Excel 95 and older workbooks aren't supported; save it from Excel 97 or later"
        } else {
            "not an Excel 97–2003 workbook"
        }));
    }
    let c = Cfb::open(data).map_err(|e| bad(e.0))?;
    if !c.has("Workbook") {
        if c.has("Book") {
            return Err(bad("Excel 95 and older workbooks aren't supported; save it from Excel 97 or later"));
        }
        return Err(bad("no Workbook stream"));
    }
    let mut stream = c.stream("Workbook").map_err(|e| bad(e.0))?;
    let first = biff::records(&stream, 0);
    let bof = first.first().filter(|r| r.kind == biff::BOF).ok_or_else(|| bad("the workbook doesn't start with BOF"))?;
    let mut bc = Cur::new(bof);
    let (vers, dt) = (bc.u16()?, bc.u16()?);
    if vers != 0x0600 || dt != 0x0005 {
        return Err(bad("Excel 95 and older workbooks aren't supported; save it from Excel 97 or later"));
    }
    if let Some(fp) = first.iter().find(|r| r.kind == biff::FILEPASS) {
        let pw_given = password.is_some();
        let cipher = match biff::filepass(&fp.data, password.unwrap_or(biff::DEFAULT_PASSWORD)) {
            Ok(k) => k,
            Err(CryptError::WrongPassword) if !pw_given => return Err(XlsError::NeedsPassword),
            Err(CryptError::WrongPassword) => {
                // A file protected only against writing still opens.
                match biff::filepass(&fp.data, biff::DEFAULT_PASSWORD) {
                    Ok(k) => k,
                    Err(_) => return Err(XlsError::WrongPassword),
                }
            }
            Err(e) => return Err(bad(e.to_string())),
        };
        let from = fp.at + 4 + fp.data.len();
        biff::decrypt(&mut stream, from, &cipher);
    }
    Ok(stream)
}

/// Reads an `.xls` workbook (BIFF8), decrypting it with `password` when it
/// is protected.
pub fn read(data: &[u8], password: Option<&str>) -> Result<(Workbook, Loaded), XlsError> {
    read_with(data, password, true)
}

/// [`read`] with formula cells left as the results the file saved (no
/// formulas placed, nothing recalculated): what the writer computed.
pub fn read_saved(data: &[u8], password: Option<&str>) -> Result<(Workbook, Loaded), XlsError> {
    read_with(data, password, false)
}

fn read_with(data: &[u8], password: Option<&str>, formulas: bool) -> Result<(Workbook, Loaded), XlsError> {
    let stream = workbook_stream(data, password)?;
    let mut out = Loaded::default();
    let mut rep = Report::default();
    let grecs = biff::records(&stream, 0);
    let g = globals(&grecs, &mut rep)?;
    drop(grecs);
    out.date1904 = g.date1904;
    if let Ok(c) = Cfb::open(data) {
        if c.entries.iter().any(|e| e.path.to_ascii_lowercase().starts_with("_vba_project_cur")) {
            rep.note(Kind::Dropped, "macros");
        }
    }
    // Sheets: worksheets become sheets; formulas still name the others.
    use officina_grid::eval::dates::DateSystem;
    let system = if g.date1904 { DateSystem::Excel1904 } else { DateSystem::Excel1900 };
    let mut wb = Workbook::with_date_system(system);
    let mut ctx = Ctx { xti: g.xti.clone(), books: g.books.clone(), ..Default::default() };
    let mut ids: Vec<Option<u16>> = Vec::new();
    for sh in &g.sheets {
        if sh.dt != 0 {
            rep.note_at(Kind::Dropped, "chart sheets, macro sheets and dialog sheets", || sh.name.clone());
            ctx.sheets.push(None);
            ids.push(None);
            continue;
        }
        if sh.hidden {
            out.report.note(Kind::Approximate, "hidden sheets shown");
        }
        let mut n: String = sh.name.chars().filter(|c| !"[]:*?/\\".contains(*c)).collect();
        if n.is_empty() || wb.sheet_id(&n).is_some() {
            n = format!("Sheet{}", wb.sheet_count() + 1);
        }
        let id = wb.add_sheet(&n).map_err(bad)?;
        ctx.sheets.push(Some(n));
        ids.push(Some(id));
    }
    if wb.sheet_count() == 0 {
        return Err(bad("no worksheets"));
    }
    ctx.names = g.names.iter().map(|n| n.name.clone()).collect();
    let xf_ids: Vec<StyleId> = styles(&g, &mut rep).into_iter().map(|st| wb.styles.intern(st)).collect();
    // Names, before the formulas that use them.
    for n in &g.names {
        if n.func || n.rgce.is_empty() {
            continue;
        }
        let scope = match n.itab {
            0 => None,
            t => match ids.get(t as usize - 1) {
                Some(Some(id)) => Some(*id),
                _ => continue,
            },
        };
        if n.builtin {
            use officina_grid::print;
            let parsed = || formula::decompile(&n.rgce, &n.rgcb, 0, 0, false, &ctx).ok();
            let preserved = match n.name.as_str() {
                print::AREA => scope.zip(parsed()).is_some_and(|(sheet, text)| {
                    let src = crate::xlsx::read::formula_in(&text);
                    let valid = wb.parse(&src, sheet, 0, 0).ok().is_some_and(|e| print::area(&e, sheet).is_some());
                    valid && wb.define_name(print::AREA, scope, &src).is_ok()
                }),
                print::TITLES => scope.zip(parsed()).is_some_and(|(sheet, text)| {
                    crate::xlsx::read::define_print_titles(&mut wb, sheet, &text)
                }),
                "_xlnm._FilterDatabase" => true,
                _ => false,
            };
            if !preserved {
                rep.note_at(Kind::Dropped, "unsupported BIFF8 print settings or built-in names", || n.name.clone());
            }
            continue;
        }
        // Relative parts are offsets from the cell that uses the name: read
        // them from mid-grid, where BIFF8's wrapping offsets land on both sides.
        let (br, bc) = (formula::ROWS / 2, formula::COLS / 2);
        match formula::decompile(&n.rgce, &n.rgcb, br, bc, true, &ctx) {
            Ok(text) => {
                if wb.define_name_at(&n.name, scope, &crate::xlsx::read::formula_in(&text), br, bc).is_err() {
                    rep.note_at(Kind::Dropped, "defined names this engine can't use", || format!("{}: {}", n.name, text.chars().take(60).collect::<String>()));
                }
            }
            Err(_) => rep.note_at(Kind::Dropped, "defined names this engine can't use", || n.name.clone()),
        }
    }
    // Cells, sheet by sheet.
    let mut all: Vec<(u16, SheetOut)> = Vec::new();
    for (k, sh) in g.sheets.iter().enumerate() {
        let Some(s) = ids[k] else { continue };
        let recs = biff::records(&stream, sh.at);
        if recs.len() > MAX_RECORDS {
            return Err(bad("a sheet too big"));
        }
        match recs.first() {
            Some(r) if r.kind == biff::BOF && r.data.len() >= 4 && u16::from_le_bytes([r.data[2], r.data[3]]) == 0x0010 => {}
            _ => return Err(bad(format!("sheet {} isn't where the workbook says", sh.name))),
        }
        let so = sheet(&mut wb, s, &recs, &g, &ctx, &xf_ids, &mut out, &mut rep)?;
        all.push((s, so));
    }
    if !formulas {
        out.report.merge(rep);
        return Ok((wb, out));
    }
    // Formulas, once every sheet exists.
    let mut cached: Vec<(CellAddr, Value)> = Vec::new();
    let empty = HashMap::new();
    for (s, so) in all {
        if so.tables > 0 {
            rep.note_n(Kind::Approximate, "data tables kept as their values", so.tables);
        }
        let mut fcells: Vec<FCell> = Vec::with_capacity(so.raw.len());
        for f in so.raw {
            let at = || a1(f.row, f.col);
            let text = match formula::exp_of(&f.rgce) {
                Some((true, _, _)) => continue,
                Some((false, r, c)) => {
                    if let Some(p) = so.arrays.get(&(r, c)) {
                        if (r, c) != (f.row, f.col) {
                            continue;
                        }
                        match formula::decompile(&p.rgce, &p.rgcb, f.row, f.col, false, &ctx) {
                            Ok(t) => {
                                let (r0, c0, r1, c1) = p.area;
                                fcells.push(FCell { row: f.row, col: f.col, src: Src::Array(t, Some((r0, c0, r1, c1))), cached: f.cached });
                            }
                            Err(w) => why_note(&mut rep, w, at),
                        }
                        continue;
                    }
                    // By its master cell, else (a few) by area.
                    let p = so.shared.get(&(r, c)).or_else(|| (so.shared.len() <= 4096).then(|| so.shared.values().find(|p| (p.area.0..=p.area.2).contains(&f.row) && (p.area.1..=p.area.3).contains(&f.col))).flatten());
                    match p {
                        Some(p) => formula::decompile(&p.rgce, &p.rgcb, f.row, f.col, true, &ctx),
                        None => Err(Why::Damaged),
                    }
                }
                None => formula::decompile(&f.rgce, &f.rgcb, f.row, f.col, false, &ctx),
            };
            match text {
                Ok(t) => fcells.push(FCell { row: f.row, col: f.col, src: Src::Text(t), cached: f.cached }),
                Err(w) => why_note(&mut rep, w, at),
            }
        }
        place_formulas(&mut wb, s, fcells, &empty, &mut rep, &mut out, &mut cached);
    }
    wb.recalc();
    differential(&wb, cached, &mut out, &mut rep);
    out.report.merge(rep);
    Ok((wb, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rk_numbers() {
        // 1.0 as the high 30 bits of its double; 123 as an integer; 1.23 as
        // 123 / 100.
        assert_eq!(rk(0x3FF0_0000), 1.0);
        assert_eq!(rk((123 << 2) | 2), 123.0);
        assert_eq!(rk((123 << 2) | 3), 1.23);
        assert_eq!(rk(((-5i32) << 2) as u32 | 2), -5.0);
    }

    #[test]
    fn a_small_workbook_plain_and_protected() {
        let check = |data: &[u8], pw: Option<&str>| {
            let (wb, l) = read(data, pw).unwrap();
            assert_eq!(wb.sheet_name(0), "Data");
            assert_eq!(wb.get(CellAddr::new(0, 0, 0)), Value::num(2.0));
            assert_eq!(wb.get(CellAddr::new(0, 0, 1)), Value::str("hello"));
            assert_eq!(wb.formula_text(CellAddr::new(0, 0, 2)).as_deref(), Some("=A1*21"));
            assert_eq!(wb.get(CellAddr::new(0, 0, 2)), Value::num(42.0));
            assert_eq!((l.cells, l.formulas, l.differ), (3, 1, 0));
            assert!(l.report.is_empty(), "{:?}", l.report.lines());
        };
        check(&sample::workbook(None), None);
        let locked = sample::workbook(Some("pässword"));
        assert!(matches!(read(&locked, None), Err(XlsError::NeedsPassword)));
        assert!(matches!(read(&locked, Some("wrong")), Err(XlsError::WrongPassword)));
        check(&locked, Some("pässword"));
        // Cut short or bit-flipped anywhere: an error or a workbook, never a
        // panic.
        for n in (0..locked.len()).step_by(37) {
            let _ = read(&locked[..n], Some("pässword"));
            let mut b = sample::workbook(None);
            if n < b.len() {
                b[n] ^= 0xA5;
                let _ = read(&b, None);
            }
        }
    }

    #[test]
    fn not_a_workbook() {
        assert!(!is_xls(b"PK\x03\x04"));
        assert!(read(b"nonsense", None).is_err());
        let f = cfb::write(&[("Book", b"\x09\x08\x08\x00\x00\x05\x05\x00")]);
        assert!(matches!(read(&f, None), Err(XlsError::Bad(_))));
    }
}
