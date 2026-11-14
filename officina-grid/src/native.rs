//! The native spreadsheet file, version 0 (docs/spec/officina.md §5.1): an
//! `officina_core::container` with these chunks.
//!
//! | kind | chunk | payload |
//! |---|---|---|
//! | 1 | WORKBOOK | sheet count; per sheet: name, default row height and column width, overrides |
//! | 2 | STRINGS | the texts cells use, renumbered densely on save |
//! | 3 | COLUMN | one 4096-row chunk of one column: sheet, column, chunk number, encoding, cells |
//! | 4 | FORMULAS | one sheet's formula groups: column, first and last row, A1 text at the first row |
//! | 5 | STYLES | the style table: every style after the default (id 0), in id order |
//! | 6 | STYLE_RUNS | one sheet's style runs: per styled column, its runs (first row, style id) |
//!
//! Formula results are stored, so a load does no recalculation except for volatile
//! formulas. Column chunks are independent, so a reader can load the visible part of a
//! huge sheet first (O2's job; v0 loads everything).
//!
//! The reader is for hostile files: every count is bounded by what the bytes left could
//! hold, every index is range-checked, groups may not overlap, and nothing panics
//! (fuzzed in tests/fuzz.rs).

use crate::addr::{MAX_COLS, MAX_ROWS};
use crate::book::{Workbook, MAX_SHEETS, MAX_SHEET_NAME};
use crate::column::{Cell, Chunk, CHUNK_ROWS};
use crate::strings::{StrId, MAX_STRINGS, MAX_TEXT};
use crate::value::ErrorKind;
use officina_core::container::{ContainerError, ContainerReader, ContainerWriter, DocKind, Limits};
use officina_core::wire::{Reader, WireError, Writer};
use std::io::{self, Write};
use std::sync::Arc;

pub const WORKBOOK: u16 = 1;
pub const STRINGS: u16 = 2;
pub const COLUMN: u16 = 3;
pub const FORMULAS: u16 = 4;
pub const STYLES: u16 = 5;
pub const STYLE_RUNS: u16 = 6;

const ENC_LIST: u8 = 0;
const ENC_NUM: u8 = 1;

/// Largest row height or column width a file may set, in points.
const MAX_SIZE: f64 = 8192.0;
/// Formula groups per FORMULAS chunk.
const FORMULAS_PER_CHUNK: usize = 65_536;

#[derive(Debug)]
pub enum LoadError {
    Container(ContainerError),
    Wire(WireError),
    Bad(&'static str),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Container(e) => write!(f, "{e}"),
            LoadError::Wire(e) => write!(f, "{e}"),
            LoadError::Bad(w) => write!(f, "bad spreadsheet file: {w}"),
        }
    }
}

impl std::error::Error for LoadError {}

impl From<ContainerError> for LoadError {
    fn from(e: ContainerError) -> Self {
        LoadError::Container(e)
    }
}
impl From<WireError> for LoadError {
    fn from(e: WireError) -> Self {
        LoadError::Wire(e)
    }
}

// ---------- save ----------

/// Writes `wb` as a native file. Streams: chunks go to `out` as they are made.
pub fn save<W: Write>(wb: &Workbook, out: W) -> io::Result<W> {
    let mut c = ContainerWriter::new(out, DocKind::Sheets)?;

    let mut w = Writer::new();
    w.var(wb.sheets.len() as u64);
    for s in &wb.sheets {
        w.str(&s.name);
        for sz in [&s.row_sizes, &s.col_sizes] {
            w.f64(sz.default);
            w.var(sz.overrides().len() as u64);
            for &(i, v) in sz.overrides() {
                w.var(i as u64);
                w.f64(v);
            }
        }
    }
    c.add(WORKBOOK, &w.into_bytes())?;

    // Cells, renumbering the strings they use (the pool also holds texts no cell
    // uses any more).
    let mut remap: Vec<u32> = vec![u32::MAX; wb.strings.len()];
    let mut used: Vec<StrId> = Vec::new();
    let mut id = |s: StrId| -> u32 {
        let m = &mut remap[s as usize];
        if *m == u32::MAX {
            *m = used.len() as u32;
            used.push(s);
        }
        *m
    };
    for (si, s) in wb.sheets.iter().enumerate() {
        for (ci, col) in s.cols.iter().enumerate() {
            for (n, chunk) in col.chunks() {
                let mut w = Writer::new();
                w.u16(si as u16);
                w.var(ci as u64);
                w.var(n as u64);
                match chunk {
                    Chunk::Num { vals, present, .. } => {
                        w.u8(ENC_NUM);
                        for &b in present.iter() {
                            w.u64(b);
                        }
                        for (off, &x) in vals.iter().enumerate() {
                            if present[off / 64] >> (off % 64) & 1 != 0 {
                                w.f64(x);
                            }
                        }
                    }
                    _ => {
                        w.u8(ENC_LIST);
                        let mut cells = Vec::new();
                        chunk.each(0, CHUNK_ROWS as usize - 1, &mut |off, cell| cells.push((off, cell)));
                        w.var(cells.len() as u64);
                        for (off, cell) in cells {
                            w.var(off as u64);
                            match cell {
                                Cell::Empty => unreachable!("each skips empty cells"),
                                Cell::Num(x) => {
                                    w.u8(1);
                                    w.f64(x);
                                }
                                Cell::Str(s) => {
                                    w.u8(2);
                                    w.var(id(s) as u64);
                                }
                                Cell::Bool(b) => w.u8(if b { 4 } else { 3 }),
                                Cell::Err(e) => {
                                    w.u8(5);
                                    w.u8(e as u8);
                                }
                            }
                        }
                    }
                }
                c.add(COLUMN, &w.into_bytes())?;
            }
        }
    }

    let mut w = Writer::new();
    w.var(used.len() as u64);
    for &s in &used {
        w.str(wb.strings.get(s));
    }
    c.add(STRINGS, &w.into_bytes())?;

    for si in 0..wb.sheets.len() {
        let mut groups: Vec<_> = wb.groups().filter(|(_, g)| g.sheet as usize == si).map(|(_, g)| g).collect();
        groups.sort_by_key(|g| (g.col, g.r0));
        for part in groups.chunks(FORMULAS_PER_CHUNK) {
            let mut w = Writer::new();
            w.u16(si as u16);
            w.var(part.len() as u64);
            for g in part {
                w.var(g.col as u64);
                w.var(g.r0 as u64);
                w.var(g.r1 as u64);
                w.str(&crate::formula::to_a1(&g.expr, g.r0, g.col, &|s| wb.sheet_name(s)));
            }
            c.add(FORMULAS, &w.into_bytes())?;
        }
    }
    if wb.styles.len() > 1 {
        let mut w = Writer::new();
        w.var(wb.styles.len() as u64 - 1);
        for (_, st) in wb.styles.iter().skip(1) {
            st.encode(&mut w);
        }
        c.add(STYLES, &w.into_bytes())?;
        for (si, sh) in wb.sheets.iter().enumerate() {
            let cols: Vec<_> = sh.styles.iter().enumerate().filter(|(_, r)| !r.is_empty()).collect();
            if cols.is_empty() {
                continue;
            }
            let mut w = Writer::new();
            w.u16(si as u16);
            w.var(cols.len() as u64);
            for (ci, runs) in cols {
                w.var(ci as u64);
                w.var(runs.runs().len() as u64);
                for &(r, st) in runs.runs() {
                    w.var(r as u64);
                    w.var(st as u64);
                }
            }
            c.add(STYLE_RUNS, &w.into_bytes())?;
        }
    }
    c.finish()
}

/// [`save`] into a byte vector.
pub fn save_to_vec(wb: &Workbook) -> Vec<u8> {
    save(wb, Vec::new()).expect("writing to a Vec does not fail")
}

// ---------- load ----------

/// Reads a native file. Never panics on any input.
pub fn load(data: &[u8]) -> Result<Workbook, LoadError> {
    let c = ContainerReader::open(data, Limits::default())?;
    if c.kind != DocKind::Sheets {
        return Err(LoadError::Bad("not a spreadsheet"));
    }
    let of = |k: u16| c.entries().iter().enumerate().filter(move |(_, e)| e.kind == k).map(|(i, _)| i);
    let mut wb = Workbook::new();

    // Sheets.
    let mut it = of(WORKBOOK);
    let i = it.next().ok_or(LoadError::Bad("no WORKBOOK chunk"))?;
    if it.next().is_some() {
        return Err(LoadError::Bad("two WORKBOOK chunks"));
    }
    let mut r = Reader::new(c.chunk(i)?);
    let n = r.count("sheets", MAX_SHEETS as u64, 1)?;
    if n == 0 {
        return Err(LoadError::Bad("no sheets"));
    }
    for _ in 0..n {
        let name = r.str("sheet name", MAX_SHEET_NAME as u64)?;
        let s = wb.add_sheet(name).map_err(LoadError::Bad)?;
        for (axis, max) in [(0, MAX_ROWS), (1, MAX_COLS)] {
            let def = size(&mut r)?;
            let k = r.count("size overrides", max as u64, 9)?;
            let mut over = Vec::with_capacity(k);
            for _ in 0..k {
                let at = r.len("size index", max as u64 - 1)? as u32;
                if over.last().is_some_and(|&(p, _)| p >= at) {
                    return Err(LoadError::Bad("size overrides out of order"));
                }
                over.push((at, size(&mut r)?));
            }
            let sz = crate::column::Sizes::from_parts(def, over);
            if axis == 0 {
                wb.sheets[s as usize].row_sizes = sz;
            } else {
                wb.sheets[s as usize].col_sizes = sz;
            }
        }
    }
    done(&r)?;

    // Strings.
    let mut strings: Vec<Arc<str>> = Vec::new();
    let mut it = of(STRINGS);
    if let Some(i) = it.next() {
        if it.next().is_some() {
            return Err(LoadError::Bad("two STRINGS chunks"));
        }
        let mut r = Reader::new(c.chunk(i)?);
        let k = r.count("strings", MAX_STRINGS as u64, 1)?;
        strings.reserve(k);
        for _ in 0..k {
            strings.push(Arc::from(r.str("text", MAX_TEXT as u64)?));
        }
        done(&r)?;
    }
    let ids: Vec<StrId> = strings.iter().map(|s| wb.strings.intern_arc(s)).collect();
    drop(strings);

    // Cells.
    let max_chunk = (MAX_ROWS - 1) / CHUNK_ROWS;
    for i in of(COLUMN) {
        let mut r = Reader::new(c.chunk(i)?);
        let s = r.u16()?;
        if s as usize >= wb.sheets.len() {
            return Err(LoadError::Bad("cells for a sheet that does not exist"));
        }
        let col = r.len("column", MAX_COLS as u64 - 1)? as u32;
        let n = r.len("chunk number", max_chunk as u64)? as u32;
        if wb.sheets[s as usize].cols.get(col as usize).is_some_and(|c| c.chunk(n * CHUNK_ROWS).is_some()) {
            return Err(LoadError::Bad("a column chunk twice"));
        }
        let base = n * CHUNK_ROWS;
        match r.u8()? {
            ENC_NUM => {
                let mut present = vec![0u64; CHUNK_ROWS as usize / 64].into_boxed_slice();
                for b in present.iter_mut() {
                    *b = r.u64()?;
                }
                let count: u32 = present.iter().map(|b| b.count_ones()).sum();
                if count == 0 {
                    return Err(LoadError::Bad("empty column chunk"));
                }
                if base as u64 + CHUNK_ROWS as u64 > MAX_ROWS as u64 {
                    // The last chunk of a column is short; rows past the limit must be absent.
                    let ok = (MAX_ROWS - base) as usize;
                    if (ok..CHUNK_ROWS as usize).any(|off| present[off / 64] >> (off % 64) & 1 != 0) {
                        return Err(LoadError::Bad("row past the last row"));
                    }
                }
                let mut vals = vec![0.0f64; CHUNK_ROWS as usize].into_boxed_slice();
                let mut last = 0;
                for (off, v) in vals.iter_mut().enumerate() {
                    if present[off / 64] >> (off % 64) & 1 != 0 {
                        let x = r.f64()?;
                        if !x.is_finite() {
                            return Err(LoadError::Bad("number not finite"));
                        }
                        *v = x;
                        last = off;
                    }
                }
                let sh = &mut wb.sheets[s as usize];
                sh.rows_used = sh.rows_used.max(base + last as u32 + 1);
                wb.col_mut(s, col).put_chunk(n, Chunk::Num { vals, present, count });
            }
            ENC_LIST => {
                let k = r.count("cells", CHUNK_ROWS as u64, 2)?;
                if k == 0 {
                    return Err(LoadError::Bad("empty column chunk"));
                }
                let mut prev: Option<u32> = None;
                for _ in 0..k {
                    let off = r.len("row in chunk", CHUNK_ROWS as u64 - 1)? as u32;
                    if prev.is_some_and(|p| p >= off) {
                        return Err(LoadError::Bad("cells out of order"));
                    }
                    prev = Some(off);
                    let row = base + off;
                    if row >= MAX_ROWS {
                        return Err(LoadError::Bad("row past the last row"));
                    }
                    let cell = match r.u8()? {
                        1 => {
                            let x = r.f64()?;
                            if !x.is_finite() {
                                return Err(LoadError::Bad("number not finite"));
                            }
                            Cell::Num(x)
                        }
                        2 => {
                            let j = r.var()?;
                            Cell::Str(*ids.get(j as usize).ok_or(LoadError::Bad("string id"))?)
                        }
                        3 => Cell::Bool(false),
                        4 => Cell::Bool(true),
                        5 => Cell::Err(ErrorKind::from_u8(r.u8()?).ok_or(LoadError::Bad("error value"))?),
                        _ => return Err(LoadError::Bad("cell tag")),
                    };
                    wb.store_cell(s, row, col, cell);
                }
            }
            _ => return Err(LoadError::Bad("column encoding")),
        }
        done(&r)?;
    }

    // Formulas.
    for i in of(FORMULAS) {
        let mut r = Reader::new(c.chunk(i)?);
        let s = r.u16()?;
        if s as usize >= wb.sheets.len() {
            return Err(LoadError::Bad("formulas for a sheet that does not exist"));
        }
        let k = r.count("formula groups", FORMULAS_PER_CHUNK as u64, 4)?;
        for _ in 0..k {
            let col = r.len("column", MAX_COLS as u64 - 1)? as u32;
            let r0 = r.len("row", MAX_ROWS as u64 - 1)? as u32;
            let r1 = r.len("row", MAX_ROWS as u64 - 1)? as u32;
            if r0 > r1 {
                return Err(LoadError::Bad("formula rows"));
            }
            let src = r.str("formula", crate::formula::MAX_STORED_FORMULA_LEN as u64)?;
            let overlaps = wb.sheets[s as usize].fidx.get(col as usize).is_some_and(|f| {
                f.range(r0..=r1).next().is_some() || f.range(..r0).next_back().is_some_and(|(_, &g)| wb.group(g).r1 >= r0)
            });
            if overlaps {
                return Err(LoadError::Bad("formula groups overlap"));
            }
            let e = wb.parse_stored(src, r0, col).map_err(|_| LoadError::Bad("formula does not parse"))?;
            wb.add_group(s, col, r0, r1, Arc::new(e), None);
            let sh = &mut wb.sheets[s as usize];
            sh.rows_used = sh.rows_used.max(r1 + 1);
        }
        done(&r)?;
    }

    // Styles: the file's ids map to this workbook's (equal styles intern to one).
    let mut style_ids: Vec<u32> = vec![0];
    let mut it = of(STYLES);
    if let Some(i) = it.next() {
        if it.next().is_some() {
            return Err(LoadError::Bad("two STYLES chunks"));
        }
        let mut r = Reader::new(c.chunk(i)?);
        let k = r.count("styles", crate::style::MAX_STYLES as u64 - 1, 20)?;
        for _ in 0..k {
            let st = crate::style::Style::decode(&mut r)?;
            style_ids.push(wb.styles.intern(st));
        }
        done(&r)?;
    }
    for i in of(STYLE_RUNS) {
        let mut r = Reader::new(c.chunk(i)?);
        let s = r.u16()? as usize;
        if s >= wb.sheets.len() {
            return Err(LoadError::Bad("styles for a sheet that does not exist"));
        }
        let k = r.count("styled columns", MAX_COLS as u64, 2)?;
        for _ in 0..k {
            let col = r.len("column", MAX_COLS as u64 - 1)?;
            let n = r.count("style runs", MAX_ROWS as u64, 2)?;
            let mut runs = Vec::with_capacity(n);
            for _ in 0..n {
                let row = r.len("row", MAX_ROWS as u64)? as u32;
                if runs.last().is_some_and(|&(p, _)| p >= row) {
                    return Err(LoadError::Bad("style runs out of order"));
                }
                let id = r.len("style id", style_ids.len() as u64 - 1)?;
                runs.push((row, style_ids[id]));
            }
            let sh = &mut wb.sheets[s];
            if sh.styles.len() <= col {
                sh.styles.resize_with(col + 1, crate::style::StyleRuns::default);
            }
            if !sh.styles[col].runs().is_empty() {
                return Err(LoadError::Bad("a column's styles twice"));
            }
            sh.styles[col] = crate::style::StyleRuns::from_runs(runs);
        }
        done(&r)?;
    }

    // Stored results are current; only volatile formulas need a recalculation, and
    // `recalc` always takes those.
    wb.changed.clear();
    wb.pending.clear();
    Ok(wb)
}

fn size(r: &mut Reader) -> Result<f64, LoadError> {
    let x = r.f64()?;
    if x.is_finite() && x > 0.0 && x <= MAX_SIZE {
        Ok(x)
    } else {
        Err(LoadError::Bad("row height or column width"))
    }
}

fn done(r: &Reader) -> Result<(), LoadError> {
    if r.is_empty() {
        Ok(())
    } else {
        Err(LoadError::Bad("trailing bytes in a chunk"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr::CellAddr;
    use crate::value::Value;

    #[test]
    fn styles_round_trip() {
        let mut wb = Workbook::new();
        wb.add_sheet("A").unwrap();
        wb.add_sheet("B").unwrap();
        let mut st = crate::style::Style::default().with_num_fmt("#,##0.00");
        st.font.bold = true;
        st.fill = Some(0xDDEEFF);
        wb.set_style(&crate::addr::Rect { sheet: 1, r0: 0, c0: 2, r1: 1_048_575, c1: 4 }, st.clone());
        wb.set_style(&crate::addr::Rect { sheet: 1, r0: 5, c0: 3, r1: 9, c1: 3 }, crate::style::Style::default().with_num_fmt("0%"));
        wb.set_value(CellAddr::new(1, 6, 3), Value::Num(0.5));
        let back = load(&save_to_vec(&wb)).unwrap();
        for (r, c) in [(0, 2), (6, 3), (10, 3), (1_048_575, 4), (0, 5), (0, 1)] {
            let a = CellAddr::new(1, r, c);
            assert_eq!(back.style(a), wb.style(a), "{r},{c}");
        }
        assert_eq!(back.formatted(CellAddr::new(1, 6, 3)).text, "50%");
        assert_eq!(back.sheet(0).styles.len(), 0);
    }

    #[test]
    fn round_trip() {
        let mut wb = Workbook::new();
        wb.add_sheet("Data").unwrap();
        wb.add_sheet("Sum it").unwrap();
        let vals: Vec<f64> = (0..10_000).map(|i| i as f64 * 0.5).collect();
        wb.set_numbers(0, 0, 0, &vals);
        wb.set_input(CellAddr::new(0, 3, 1), "hello").unwrap();
        wb.set_input(CellAddr::new(0, 4, 1), "TRUE").unwrap();
        wb.set_input(CellAddr::new(0, 5, 1), "#N/A").unwrap();
        wb.set_input(CellAddr::new(0, 6, 1), "gone").unwrap();
        wb.set_input(CellAddr::new(0, 6, 1), "").unwrap();
        wb.set_input(CellAddr::new(0, 2_000_000_000, 7), "far").unwrap();
        wb.fill_formula(0, 2, 0, 9_999, "=A1*2").unwrap();
        wb.set_input(CellAddr::new(1, 0, 0), "='Data'!B4&\" world\"").unwrap();
        wb.set_input(CellAddr::new(1, 1, 0), "=SUM(Data!C:C)").unwrap();
        wb.set_input(CellAddr::new(1, 2, 0), "=RAND()").unwrap();
        wb.sheets[0].row_sizes.set(5, 31.5);
        wb.sheets[1].col_sizes.set(0, 200.0);
        wb.recalc();

        let bytes = save_to_vec(&wb);
        let mut back = load(&bytes).unwrap();
        assert_eq!(back.sheet_name(1), "Sum it");
        assert_eq!(back.get(CellAddr::new(1, 0, 0)), Value::str("hello world"));
        assert_eq!(back.get(CellAddr::new(1, 1, 0)), Value::Num(49_995_000.0));
        assert_eq!(back.get(CellAddr::new(0, 2_000_000_000, 7)), Value::str("far"));
        assert_eq!(back.get(CellAddr::new(0, 6, 1)), Value::Empty);
        assert_eq!(back.strings.len(), 3, "unused texts are not saved"); // hello, far, hello world
        assert_eq!(back.sheets[0].row_sizes.size(5), 31.5);
        assert_eq!(back.sheets[1].col_sizes.size(0), 200.0);
        assert_eq!(back.groups().count(), 4);
        let st = back.recalc();
        assert_eq!(st.groups, 1, "only RAND() recalculates after a load");
        for r in [0u32, 4, 9_999] {
            for c in 0..3 {
                let a = CellAddr::new(0, r, c);
                assert_eq!(back.get(a), wb.get(a));
                assert_eq!(back.input(a), wb.input(a));
            }
        }
        // Edits after a load still propagate.
        back.set_input(CellAddr::new(0, 9_999, 0), "0").unwrap();
        back.recalc();
        assert_eq!(back.get(CellAddr::new(1, 1, 0)), Value::Num(49_995_000.0 - 2.0 * 4_999.5));
        // Saving again gives the same bytes (deterministic), RAND aside.
        assert_eq!(save_to_vec(&load(&bytes).unwrap()), bytes);
    }

    #[test]
    fn corrupt_files_are_refused_not_panics() {
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        wb.set_input(CellAddr::new(0, 0, 0), "x").unwrap();
        wb.set_input(CellAddr::new(0, 1, 0), "=A1&A1").unwrap();
        wb.set_numbers(0, 1, 0, &[1.0, 2.0, 3.0]);
        wb.recalc();
        let b = save_to_vec(&wb);
        for i in 0..b.len() {
            let _ = load(&b[..i]);
            for bit in [0x01, 0x80] {
                let mut c = b.clone();
                c[i] ^= bit;
                let _ = load(&c);
            }
        }
    }
}
