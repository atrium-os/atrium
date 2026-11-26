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
//! | 8 | ROWS_SHOWN | one sheet's hidden rows and auto-filter (as `GridOp::Filter` carries them) |
//! | 25 | PICTURE | one embedded raster picture: sheet, anchor, optional crop, rotation and link, and encoded bytes |
//! | 10 | CHARTS | charts: kind, grouping, legend, titles, anchor, series ranges |
//! | 11 | MACROS | macros kept inert: family, VBA code names (the parts follow) |
//! | 12 | MACRO_PART | one macro part: path, media type, bytes, never parsed |
//! | 13 | COND_FORMATS | conditional formats: ranges, rules (formulas as text), formats |
//! | 14 | PIVOTS | pivot tables: source, place, fields on each axis with hidden items, value fields |
//! | 9 | TABLES | structured tables: name, sheet, range, header/totals/stripes flags, style, column names |
//! | 15 | TAB_COLORS | sheets' tab colours: count; per sheet, its index and 0xRRGGBB |
//! | 16 | MERGES | one sheet's merged cells: sheet, count, each area's corners |
//! | 18 | LINKS | one sheet's cell hyperlinks: sheet, count, each cell and its target |
//! | 20 | VALIDATIONS | data validation rules: ranges, kind, operator, formulas as text, flags, messages |
//! | 19 | FREEZE | frozen panes: count; per sheet, its index, frozen rows and columns |
//! | 24 | AXIS_IDENTITIES | one sheet’s runs of stable row and column IDs |
//! | 23 | TRACKED_CELLS | change set author and its sparse baseline inputs by sheet name |
//! | 22 | FREEZE_FROM | panes frozen with the view scrolled: count; per sheet, its index, first frozen row and column |
//! | 21 | HIDDEN_WIDTHS | one sheet's hidden columns' remembered widths: sheet, count, each column and px |
//! | 17 | COMMENTS | one sheet's cell comments: sheet, count, each cell, a plain note, styled note or thread |
//! | 29 | ZOOM | sheets' view zoom other than 100 %: count; per sheet, its index and percent (10–400) |
//! | 30 | VIEW_HIDDEN | sheets showing no gridlines, headings or zeros: count; per sheet, its index and flags (1 gridlines off, 2 headings off, 4 zero values hidden) |
//! | 36 | PROPERTIES | document properties: title, subject, author, keywords, category, comments, then the final mark when set |
//! | 37 | GRID_COLORS | sheets' gridline colours other than automatic: count; per sheet, its index and 0xRRGGBB |
//! | 38 | CUSTOM_VIEWS | Custom Views: count; each name, sheet, cursor, zoom, its filter state, column widths and hidden widths ([`crate::views`]) |
//! | 39 | LOOK | theme (name, twelve colours, heading and body fonts), custom table styles, shapes and text boxes, Camera pictures ([`crate::look`]) |
//! | 7 | NAMES | defined names: scope (a sheet, or 0xFFFF for the workbook), name, A1 text as if in A1 |
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
pub const TAB_COLORS: u16 = 15;
pub const MERGES: u16 = 16;
pub const COMMENTS: u16 = 17;
pub const LINKS: u16 = 18;
pub const FREEZE: u16 = 19;
pub const VALIDATIONS: u16 = 20;
pub const HIDDEN_WIDTHS: u16 = 21;
pub const FREEZE_FROM: u16 = 22;
pub const TRACKED_CELLS: u16 = 23;
pub const AXIS_IDENTITIES: u16 = 24;
pub const PICTURE: u16 = 25;
pub const SHEET_IDENTITIES: u16 = 26;
pub const OUTLINES: u16 = 27;
pub const HIDDEN_SHEETS: u16 = 28;
pub const ZOOM: u16 = 29;
pub const VIEW_HIDDEN: u16 = 30;
pub const SCENARIOS: u16 = 31;
pub const SPARKLINES: u16 = 32;
pub const WATCHES: u16 = 33;
pub const EDIT_RANGES: u16 = 34;
pub const IGNORED_ERRORS: u16 = 35;
pub const PROPERTIES: u16 = 36;
pub const GRID_COLORS: u16 = 37;
pub const CUSTOM_VIEWS: u16 = 38;
pub const LOOK: u16 = 39;
pub const COLUMN: u16 = 3;
pub const FORMULAS: u16 = 4;
pub const STYLES: u16 = 5;
pub const STYLE_RUNS: u16 = 6;
pub const NAMES: u16 = 7;
pub const ROWS_SHOWN: u16 = 8;
pub const TABLES: u16 = 9;
pub const CHARTS: u16 = 10;
pub const MACROS: u16 = 11;
pub const MACRO_PART: u16 = 12;
pub const COND_FORMATS: u16 = 13;
pub const PIVOTS: u16 = 14;
const GLOBAL: u16 = 0xFFFF;

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
    /// Loading it would take more memory than the budget it was given
    /// ([`load_within`]): about this many bytes when it stopped.
    TooBig(u64),
}

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LoadError::Container(e) => write!(f, "{e}"),
            LoadError::Wire(e) => write!(f, "{e}"),
            LoadError::Bad(w) => write!(f, "bad spreadsheet file: {w}"),
            LoadError::TooBig(b) => write!(f, "it needs more than {:.1} GB of memory open, past this app's limit", *b as f64 / (1u64 << 30) as f64),
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
    // Older files end after the sheet settings and use the 1900 calendar.
    let date1904 = wb.date_system() == crate::eval::dates::DateSystem::Excel1904;
    let protected = wb.sheets.iter().filter(|s| s.protection.is_some()).count();
    let flags = u8::from(date1904) | (u8::from(protected != 0) << 1) | (u8::from(wb.iteration.is_some()) << 2) | (u8::from(wb.structure.is_some()) << 3) | (u8::from(wb.manual) << 4);
    if flags != 0 {
        w.u8(flags);
    }
    if protected != 0 {
        w.var(protected as u64);
        for (index, sheet) in wb.sheets.iter().enumerate() {
            if let Some(protection) = &sheet.protection {
                w.u16(index as u16);
                protection.encode(&mut w);
            }
        }
    }
    if let Some(it) = wb.iteration {
        w.u32(it.count);
        w.f64(it.change);
    }
    if let Some(p) = &wb.structure {
        p.encode(&mut w);
    }
    c.add(WORKBOOK, &w.into_bytes())?;
    if wb.sheets.iter().any(|sheet| sheet.identity != 0) {
        let mut w = Writer::new();
        for sheet in &wb.sheets {
            w.u64((sheet.identity >> 64) as u64);
            w.u64(sheet.identity as u64);
        }
        c.add(SHEET_IDENTITIES, &w.into_bytes())?;
    }
    if let Some(set) = wb.tracked_cells() {
        let mut w = Writer::new();
        set.encode(&mut w);
        c.add(TRACKED_CELLS, &w.into_bytes())?;
    }

    for (sheet, sh) in wb.sheets.iter().enumerate() {
        if sh.axis_ids.iter().all(crate::axis_ids::AxisIds::is_implicit) { continue; }
        let mut w = Writer::new();
        w.u16(sheet as u16);
        for ids in &sh.axis_ids { ids.encode(&mut w); }
        c.add(AXIS_IDENTITIES, &w.into_bytes())?;
    }

    for (sheet, sh) in wb.sheets.iter().enumerate() {
        if sh.outlines.iter().all(|axis| axis.groups().is_empty()) { continue; }
        let mut w = Writer::new();
        w.u16(sheet as u16);
        for outline in &sh.outlines { outline.encode(&mut w); }
        c.add(OUTLINES, &w.into_bytes())?;
    }

    for (sheet, sh) in wb.sheets.iter().enumerate() {
        if sh.sparklines.is_empty() { continue; }
        let mut w = Writer::new();
        w.u16(sheet as u16);
        crate::sparkline::encode(&sh.sparklines, &mut w);
        c.add(SPARKLINES, &w.into_bytes())?;
    }

    for (sheet, sh) in wb.sheets.iter().enumerate() {
        if sh.edit_ranges.is_empty() { continue; }
        let mut w = Writer::new();
        w.u16(sheet as u16);
        crate::sheetareas::encode_ranges(&sh.edit_ranges, &mut w);
        c.add(EDIT_RANGES, &w.into_bytes())?;
    }

    for (sheet, sh) in wb.sheets.iter().enumerate() {
        if sh.ignored.is_empty() { continue; }
        let mut w = Writer::new();
        w.u16(sheet as u16);
        crate::sheetareas::encode_ignored(&sh.ignored, &mut w);
        c.add(IGNORED_ERRORS, &w.into_bytes())?;
    }

    if !wb.views.is_empty() {
        let mut w = Writer::new();
        crate::views::encode(&wb.views, &mut w);
        c.add(CUSTOM_VIEWS, &w.into_bytes())?;
    }

    if !wb.look.is_default() {
        let mut w = Writer::new();
        wb.look.encode(&mut w);
        c.add(LOOK, &w.into_bytes())?;
    }

    if !wb.props.is_empty() {
        let mut w = Writer::new();
        wb.props.encode(&mut w);
        c.add(PROPERTIES, &w.into_bytes())?;
    }

    for (sheet, sh) in wb.sheets.iter().enumerate() {
        if sh.watches.is_empty() { continue; }
        let mut w = Writer::new();
        w.u16(sheet as u16);
        crate::watch::encode(&sh.watches, &mut w);
        c.add(WATCHES, &w.into_bytes())?;
    }

    for (sheet, sh) in wb.sheets.iter().enumerate() {
        if sh.scenarios.is_empty() { continue; }
        let mut w = Writer::new();
        w.u16(sheet as u16);
        crate::scenario::encode(&sh.scenarios, &mut w);
        c.add(SCENARIOS, &w.into_bytes())?;
    }

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
    // Spilled cells are a formula's result, not the file's: they aren't saved.
    let mut spilled: std::collections::HashMap<(u16, u32), Vec<(u32, u32)>> = std::collections::HashMap::new();
    for (a, sp) in wb.spills() {
        for c in sp.area.c0..=sp.area.c1 {
            let rows = spilled.entry((a.sheet, c)).or_default();
            if c == a.col {
                if sp.area.r1 > a.row {
                    rows.push((a.row + 1, sp.area.r1));
                }
            } else {
                rows.push((sp.area.r0, sp.area.r1));
            }
        }
    }
    for (si, s) in wb.sheets.iter().enumerate() {
        for (ci, col) in s.cols.iter().enumerate() {
            let skip = spilled.get(&(si as u16, ci as u32));
            let base_skip = |n: u32, off: usize| skip.is_some_and(|v| v.iter().any(|&(a, b)| (a..=b).contains(&(n * CHUNK_ROWS + off as u32))));
            for (n, chunk) in col.chunks() {
                if skip.is_some() {
                    // The general encoding, without the spilled cells.
                    let mut cells = Vec::new();
                    chunk.each(0, CHUNK_ROWS as usize - 1, &mut |off, cell| {
                        if !base_skip(n, off) {
                            cells.push((off, cell))
                        }
                    });
                    if cells.is_empty() {
                        continue;
                    }
                    let mut w = Writer::new();
                    w.u16(si as u16);
                    w.var(ci as u64);
                    w.var(n as u64);
                    w.u8(ENC_LIST);
                    list(&mut w, cells, &mut id);
                    c.add(COLUMN, &w.into_bytes())?;
                    continue;
                }
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
                        list(&mut w, cells, &mut id);
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
    if !wb.names.is_empty() {
        let mut w = Writer::new();
        w.var(wb.names.len() as u64);
        for d in wb.names.iter() {
            w.u16(d.scope.unwrap_or(GLOBAL));
            w.str(&d.name);
            w.str(&wb.name_text(d));
        }
        c.add(NAMES, &w.into_bytes())?;
    }
    if !wb.cond_formats.is_empty() {
        let mut w = Writer::new();
        w.var(wb.cond_formats.len() as u64);
        for f in &wb.cond_formats {
            f.encode(&mut w, wb);
        }
        c.add(COND_FORMATS, &w.into_bytes())?;
    }
    if !wb.validations.is_empty() {
        c.add(VALIDATIONS, &wb.encode_validations(&wb.validations))?;
    }
    if !wb.pivots.is_empty() {
        let mut w = Writer::new();
        w.var(wb.pivots.len() as u64);
        for (i, p) in wb.pivots.iter().enumerate() {
            p.encode(&mut w, wb.pivot_shapes[i].area);
        }
        c.add(PIVOTS, &w.into_bytes())?;
    }
    if !wb.charts.is_empty() {
        let mut w = Writer::new();
        w.var(wb.charts.len() as u64);
        for ch in &wb.charts {
            ch.encode(&mut w);
        }
        c.add(CHARTS, &w.into_bytes())?;
    }
    for picture in wb.pictures() {
        let mut w = Writer::new();
        picture.encode(&mut w);
        c.add(PICTURE, &w.into_bytes())?;
    }
    if let Some(m) = &wb.macros {
        let mut w = Writer::new();
        m.encode_head(&mut w);
        c.add(MACROS, &w.into_bytes())?;
        for p in &m.parts {
            let mut w = Writer::new();
            p.encode(&mut w);
            c.add(MACRO_PART, &w.into_bytes())?;
        }
    }
    if !wb.tables.is_empty() {
        let mut w = Writer::new();
        w.var(wb.tables.len() as u64);
        for t in &wb.tables {
            t.encode(&mut w);
        }
        c.add(TABLES, &w.into_bytes())?;
    }
    let hidden: Vec<usize> = wb.sheets.iter().enumerate().filter(|(_, sh)| sh.sheet_hidden).map(|(i, _)| i).collect();
    if !hidden.is_empty() {
        let mut w = Writer::new();
        w.var(hidden.len() as u64);
        for i in hidden {
            w.u16(i as u16);
        }
        c.add(HIDDEN_SHEETS, &w.into_bytes())?;
    }
    let zoomed: Vec<(usize, u16)> = wb.sheets.iter().enumerate().filter(|(_, sh)| sh.zoom != 100).map(|(i, sh)| (i, sh.zoom.clamp(crate::book::ZOOM_MIN, crate::book::ZOOM_MAX))).collect();
    if !zoomed.is_empty() {
        let mut w = Writer::new();
        w.var(zoomed.len() as u64);
        for (i, z) in zoomed {
            w.u16(i as u16);
            w.u16(z);
        }
        c.add(ZOOM, &w.into_bytes())?;
    }
    let plain: Vec<(usize, u8)> = wb.sheets.iter().enumerate().map(|(i, sh)| (i, u8::from(!sh.show_grid) | u8::from(!sh.show_headings) << 1 | u8::from(!sh.show_zeros) << 2)).filter(|&(_, f)| f != 0).collect();
    if !plain.is_empty() {
        let mut w = Writer::new();
        w.var(plain.len() as u64);
        for (i, f) in plain {
            w.u16(i as u16);
            w.u8(f);
        }
        c.add(VIEW_HIDDEN, &w.into_bytes())?;
    }
    let tabs: Vec<(usize, u32)> = wb.sheets.iter().enumerate().filter_map(|(i, sh)| sh.tab_color.map(|c| (i, c))).collect();
    if !tabs.is_empty() {
        let mut w = Writer::new();
        w.var(tabs.len() as u64);
        for (i, c) in tabs {
            w.u16(i as u16);
            w.u32(c & 0xFF_FFFF);
        }
        c.add(TAB_COLORS, &w.into_bytes())?;
    }
    let grids: Vec<(usize, u32)> = wb.sheets.iter().enumerate().filter_map(|(i, sh)| sh.grid_color.map(|c| (i, c))).collect();
    if !grids.is_empty() {
        let mut w = Writer::new();
        w.var(grids.len() as u64);
        for (i, c) in grids {
            w.u16(i as u16);
            w.u32(c & 0xFF_FFFF);
        }
        c.add(GRID_COLORS, &w.into_bytes())?;
    }
    let frozen: Vec<(usize, (u32, u32))> = wb.sheets.iter().enumerate().filter(|(_, sh)| sh.freeze != (0, 0)).map(|(i, sh)| (i, sh.freeze)).collect();
    if !frozen.is_empty() {
        let mut w = Writer::new();
        w.var(frozen.len() as u64);
        for (i, (r, c)) in frozen {
            w.u16(i as u16);
            w.u32(r);
            w.u32(c);
        }
        c.add(FREEZE, &w.into_bytes())?;
    }
    let from: Vec<(usize, (u32, u32))> = wb.sheets.iter().enumerate().filter(|(_, sh)| sh.freeze_from != (0, 0)).map(|(i, sh)| (i, sh.freeze_from)).collect();
    if !from.is_empty() {
        let mut w = Writer::new();
        w.var(from.len() as u64);
        for (i, (r, c)) in from {
            w.u16(i as u16);
            w.u32(r);
            w.u32(c);
        }
        c.add(FREEZE_FROM, &w.into_bytes())?;
    }
    for (si, sh) in wb.sheets.iter().enumerate() {
        if !sh.hidden_widths.is_empty() {
            let mut w = Writer::new();
            w.u16(si as u16);
            w.var(sh.hidden_widths.len() as u64);
            for (i, v) in &sh.hidden_widths {
                w.var(*i as u64);
                w.f64(*v);
            }
            c.add(HIDDEN_WIDTHS, &w.into_bytes())?;
        }
    }
    for (si, sh) in wb.sheets.iter().enumerate() {
        if !sh.merges.is_empty() {
            let mut w = Writer::new();
            w.u16(si as u16);
            crate::merges::encode(&mut w, &sh.merges);
            c.add(MERGES, &w.into_bytes())?;
        }
        if !sh.comments.is_empty() {
            let mut w = Writer::new();
            w.u16(si as u16);
            let set: Vec<_> = sh.comments.iter().map(|(&(r, c), k)| (r, c, Some(k.clone()))).collect();
            crate::comments::encode(&mut w, &set);
            c.add(COMMENTS, &w.into_bytes())?;
        }
        if !sh.links.is_empty() {
            let mut w = Writer::new();
            w.u16(si as u16);
            let set: Vec<_> = sh.links.iter().map(|(&(r, c), k)| (r, c, Some(k.clone()))).collect();
            crate::links::encode(&mut w, &set);
            c.add(LINKS, &w.into_bytes())?;
        }
    }
    for (si, sh) in wb.sheets.iter().enumerate() {
        if sh.hidden.is_empty() && sh.filter.is_none() {
            continue;
        }
        let mut w = Writer::new();
        wb.filter_state(si as u16).encode(&mut w);
        c.add(ROWS_SHOWN, &w.into_bytes())?;
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
fn list(w: &mut Writer, cells: Vec<(usize, Cell)>, id: &mut dyn FnMut(StrId) -> u32) {
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

pub fn save_to_vec(wb: &Workbook) -> Vec<u8> {
    save(wb, Vec::new()).expect("writing to a Vec does not fail")
}

// ---------- load ----------

/// Reads a native file. Never panics on any input.
pub fn load(data: &[u8]) -> Result<Workbook, LoadError> {
    load_within(data, u64::MAX)
}

/// [`load`], stopping with [`LoadError::TooBig`] once the workbook would hold
/// more than `budget` bytes ([`Workbook::memory_bytes`]). The loader counts
/// each new column chunk as it is installed and checks large sparse index
/// expansions before allocating the slots they require.
pub fn load_within(data: &[u8], budget: u64) -> Result<Workbook, LoadError> {
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
        let s = wb.add_identified_sheet(name, 0).map_err(LoadError::Bad)?;
        for (axis, max) in [(0, MAX_ROWS), (1, MAX_COLS)] {
            let def = size(&mut r, false)?;
            let k = r.count("size overrides", max as u64, 9)?;
            let mut over = Vec::with_capacity(k);
            for _ in 0..k {
                let at = r.len("size index", max as u64 - 1)? as u32;
                if over.last().is_some_and(|&(p, _)| p >= at) {
                    return Err(LoadError::Bad("size overrides out of order"));
                }
                over.push((at, size(&mut r, axis == 1)?));
            }
            let sz = crate::column::Sizes::from_parts(def, over);
            if axis == 0 {
                wb.sheets[s as usize].row_sizes = sz;
            } else {
                wb.sheets[s as usize].col_sizes = sz;
            }
        }
    }
    if r.remaining() != 0 {
        let flags = r.u8()?;
        if flags & !31 != 0 {
            return Err(LoadError::Bad("unknown workbook settings"));
        }
        if flags & 1 != 0 {
            wb.date_system = crate::eval::dates::DateSystem::Excel1904;
        }
        if flags & 2 != 0 {
            let count = r.count("protected sheets", wb.sheet_count() as u64, 4)?;
            for _ in 0..count {
                let index = r.u16()? as usize;
                let sheet = wb.sheets.get_mut(index).ok_or(LoadError::Bad("invalid protected sheet"))?;
                if sheet.protection.is_some() {
                    return Err(LoadError::Bad("duplicate protected sheet"));
                }
                sheet.protection = Some(crate::protection::SheetProtection::decode(&mut r)?);
            }
        }
        if flags & 4 != 0 {
            let it = crate::book::Iteration { count: r.u32()?, change: r.f64()? };
            if !it.valid() {
                return Err(LoadError::Bad("invalid iteration settings"));
            }
            wb.iteration = Some(it);
        }
        if flags & 8 != 0 {
            wb.structure = Some(crate::protection::SheetProtection::decode(&mut r)?);
        }
        wb.manual = flags & 16 != 0;
    }
    done(&r)?;

    let mut outlined = std::collections::BTreeSet::new();
    for chunk in of(OUTLINES) {
        let mut r = Reader::new(c.chunk(chunk)?);
        let sheet = r.u16()?;
        if sheet as usize >= wb.sheet_count() || !outlined.insert(sheet) {
            return Err(LoadError::Bad("invalid or repeated worksheet outline"));
        }
        let rows = crate::outline::Outline::decode(&mut r, crate::shift::Axis::Rows)?;
        let cols = crate::outline::Outline::decode(&mut r, crate::shift::Axis::Cols)?;
        if !r.is_empty() { return Err(LoadError::Bad("trailing worksheet outline data")); }
        wb.sheet_mut(sheet).outlines = [rows, cols];
    }

    let mut with_sparks = std::collections::BTreeSet::new();
    for chunk in of(SPARKLINES) {
        let mut r = Reader::new(c.chunk(chunk)?);
        let sheet = r.u16()?;
        if sheet as usize >= wb.sheet_count() || !with_sparks.insert(sheet) {
            return Err(LoadError::Bad("invalid or repeated worksheet sparklines"));
        }
        let list = crate::sparkline::decode(&mut r)?;
        if !r.is_empty() { return Err(LoadError::Bad("trailing sparkline data")); }
        wb.sheet_mut(sheet).sparklines = list;
    }

    let mut with_ranges = std::collections::BTreeSet::new();
    for chunk in of(EDIT_RANGES) {
        let mut r = Reader::new(c.chunk(chunk)?);
        let sheet = r.u16()?;
        if sheet as usize >= wb.sheet_count() || !with_ranges.insert(sheet) {
            return Err(LoadError::Bad("invalid or repeated worksheet edit ranges"));
        }
        let list = crate::sheetareas::decode_ranges(&mut r)?;
        if !r.is_empty() { return Err(LoadError::Bad("trailing edit range data")); }
        wb.sheet_mut(sheet).edit_ranges = list;
    }

    let mut with_ignored = std::collections::BTreeSet::new();
    for chunk in of(IGNORED_ERRORS) {
        let mut r = Reader::new(c.chunk(chunk)?);
        let sheet = r.u16()?;
        if sheet as usize >= wb.sheet_count() || !with_ignored.insert(sheet) {
            return Err(LoadError::Bad("invalid or repeated worksheet ignored errors"));
        }
        let list = crate::sheetareas::decode_ignored(&mut r)?;
        if !r.is_empty() { return Err(LoadError::Bad("trailing ignored error data")); }
        wb.sheet_mut(sheet).ignored = list;
    }

    let mut props = of(PROPERTIES);
    if let Some(chunk) = props.next() {
        if props.next().is_some() {
            return Err(LoadError::Bad("repeated document properties"));
        }
        let mut r = Reader::new(c.chunk(chunk)?);
        wb.props = crate::docprops::DocProps::decode(&mut r)?;
        if !r.is_empty() { return Err(LoadError::Bad("trailing document properties")); }
    }

    let mut with_watches = std::collections::BTreeSet::new();
    for chunk in of(WATCHES) {
        let mut r = Reader::new(c.chunk(chunk)?);
        let sheet = r.u16()?;
        if sheet as usize >= wb.sheet_count() || !with_watches.insert(sheet) {
            return Err(LoadError::Bad("invalid or repeated worksheet watches"));
        }
        let list = crate::watch::decode(&mut r)?;
        if !r.is_empty() { return Err(LoadError::Bad("trailing watch data")); }
        wb.sheet_mut(sheet).watches = list;
    }

    let mut with_scenarios = std::collections::BTreeSet::new();
    for chunk in of(SCENARIOS) {
        let mut r = Reader::new(c.chunk(chunk)?);
        let sheet = r.u16()?;
        if sheet as usize >= wb.sheet_count() || !with_scenarios.insert(sheet) {
            return Err(LoadError::Bad("invalid or repeated worksheet scenarios"));
        }
        let list = crate::scenario::decode(&mut r)?;
        if !r.is_empty() { return Err(LoadError::Bad("trailing scenario data")); }
        wb.sheet_mut(sheet).scenarios = list;
    }

    let mut sheet_ids = of(SHEET_IDENTITIES);
    if let Some(index) = sheet_ids.next() {
        if sheet_ids.next().is_some() { return Err(LoadError::Bad("two SHEET_IDENTITIES chunks")); }
        let mut r = Reader::new(c.chunk(index)?);
        let mut seen = std::collections::HashSet::new();
        for sheet in &mut wb.sheets {
            let id = (u128::from(r.u64()?) << 64) | u128::from(r.u64()?);
            if id != 0 && !seen.insert(id) { return Err(LoadError::Bad("duplicate worksheet identity")); }
            sheet.identity = id;
        }
        done(&r)?;
    }

    let mut tracked = of(TRACKED_CELLS);
    if let Some(i) = tracked.next() {
        if tracked.next().is_some() {
            return Err(LoadError::Bad("two TRACKED_CELLS chunks"));
        }
        let mut r = Reader::new(c.chunk(i)?);
        wb.set_tracked_cells(crate::tracking::TrackedCells::decode(&mut r)?);
        done(&r)?;
    }

    let mut identities = std::collections::HashSet::new();
    for i in of(AXIS_IDENTITIES) {
        let mut r = Reader::new(c.chunk(i)?);
        let sheet = r.u16()?;
        if sheet as usize >= wb.sheet_count() || !identities.insert(sheet) {
            return Err(LoadError::Bad("invalid or repeated axis identity sheet"));
        }
        wb.sheet_mut(sheet).axis_ids = [
            crate::axis_ids::AxisIds::decode(&mut r)?, crate::axis_ids::AxisIds::decode(&mut r)?,
        ];
        done(&r)?;
    }

    // Names, before formulas (whose dependencies follow them).
    let mut it = of(NAMES);
    if let Some(i) = it.next() {
        if it.next().is_some() {
            return Err(LoadError::Bad("two NAMES chunks"));
        }
        let mut r = Reader::new(c.chunk(i)?);
        let k = r.count("names", crate::names::MAX_NAMES as u64, 5)?;
        for _ in 0..k {
            let scope = match r.u16()? {
                GLOBAL => None,
                s if (s as usize) < wb.sheets.len() => Some(s),
                _ => return Err(LoadError::Bad("a name for a sheet that does not exist")),
            };
            let name = r.str("name", crate::names::MAX_NAME_LEN as u64)?;
            let src = r.str("name formula", MAX_TEXT as u64)?;
            if wb.names.get(scope.unwrap_or(0), name).is_some_and(|d| d.scope == scope) {
                return Err(LoadError::Bad("a name twice"));
            }
            wb.define_name_stored(name, scope, src).map_err(|_| LoadError::Bad("a name's formula"))?;
        }
        done(&r)?;
    }

    // Tables, also before formulas.
    let mut it = of(TABLES);
    if let Some(i) = it.next() {
        if it.next().is_some() {
            return Err(LoadError::Bad("two TABLES chunks"));
        }
        let mut r = Reader::new(c.chunk(i)?);
        let k = r.count("tables", crate::tables::MAX_TABLES as u64, 8)?;
        for _ in 0..k {
            let t = crate::tables::Table::decode(&mut r)?;
            wb.add_table(t).map_err(LoadError::Bad)?;
        }
        done(&r)?;
    }

    // Charts.
    let mut it = of(CHARTS);
    if let Some(i) = it.next() {
        if it.next().is_some() {
            return Err(LoadError::Bad("two CHARTS chunks"));
        }
        let mut r = Reader::new(c.chunk(i)?);
        let k = r.count("charts", crate::charts::MAX_CHARTS as u64, 30)?;
        for _ in 0..k {
            let ch = crate::charts::Chart::decode(&mut r)?;
            wb.add_chart(ch).map_err(LoadError::Bad)?;
        }
        done(&r)?;
    }

    let mut picture_room = crate::charts::MAX_PICTURE_TOTAL.min(usize::try_from(budget).unwrap_or(usize::MAX));
    for (index, i) in of(PICTURE).enumerate() {
        if index >= crate::charts::MAX_PICTURES {
            return Err(LoadError::Bad("too many pictures"));
        }
        let mut r = Reader::new(c.chunk(i)?);
        let picture = crate::charts::Picture::decode(&mut r, picture_room)?;
        picture_room -= picture.bytes.len();
        done(&r)?;
        wb.add_picture(picture).map_err(LoadError::Bad)?;
    }

    // Conditional formats.
    let mut it = of(COND_FORMATS);
    if let Some(i) = it.next() {
        if it.next().is_some() {
            return Err(LoadError::Bad("two COND_FORMATS chunks"));
        }
        let mut r = Reader::new(c.chunk(i)?);
        let k = r.count("conditional formats", crate::condfmt::MAX_FORMATS as u64, 8)?;
        for _ in 0..k {
            let f = crate::condfmt::CondFormat::decode(&mut r, &wb)?;
            wb.add_cond_format(f).map_err(LoadError::Bad)?;
        }
        done(&r)?;
    }

    let mut it = of(VALIDATIONS);
    if let Some(i) = it.next() {
        if it.next().is_some() {
            return Err(LoadError::Bad("two VALIDATIONS chunks"));
        }
        let vs = wb.decode_validations(c.chunk(i)?)?;
        for v in vs {
            wb.add_validation(v).map_err(LoadError::Bad)?;
        }
    }

    // Macros, kept as they are.
    let mut it = of(MACROS);
    if let Some(i) = it.next() {
        if it.next().is_some() {
            return Err(LoadError::Bad("two MACROS chunks"));
        }
        let mut r = Reader::new(c.chunk(i)?);
        let mut m = crate::macros::Macros::decode_head(&mut r)?;
        done(&r)?;
        for i in of(MACRO_PART) {
            if m.parts.len() >= crate::macros::MAX_PARTS {
                return Err(LoadError::Bad("too many macro parts"));
            }
            let mut r = Reader::new(c.chunk(i)?);
            m.parts.push(crate::macros::MacroPart::decode(&mut r)?);
            done(&r)?;
        }
        wb.set_macros(m).map_err(LoadError::Bad)?;
    } else if of(MACRO_PART).next().is_some() {
        return Err(LoadError::Bad("macro parts without MACROS"));
    }

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
    let check = |bytes| if bytes > budget { Err(LoadError::TooBig(bytes)) } else { Ok(()) };
    let over = |wb: &Workbook| check(wb.memory_bytes());
    let mut cells_bytes = wb.memory_bytes();
    check(cells_bytes)?;
    let slot_cost = std::mem::size_of::<Option<Box<Chunk>>>() as u64;
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
        let sheet = &wb.sheets[s as usize];
        let old_slots = sheet.column(col).map_or(0, crate::column::Column::slot_capacity);
        let new_indexes = (col as usize + 1).saturating_sub(sheet.fidx.len()) as u64 * 24;
        let required_slots = (n as usize + 1).saturating_sub(old_slots) as u64 * slot_cost;
        check(cells_bytes.saturating_add(new_indexes).saturating_add(required_slots))?;
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
        let column = wb.sheets[s as usize].column(col).ok_or(LoadError::Bad("column chunk missing"))?;
        let chunk = column.chunk(base).ok_or(LoadError::Bad("column chunk missing"))?;
        let slots = (column.slot_capacity() - old_slots) as u64 * slot_cost;
        cells_bytes = cells_bytes.saturating_add(new_indexes + slots + chunk.bytes() as u64);
        check(cells_bytes)?;
    }

    let mut formula_bytes = wb.memory_bytes();
    check(formula_bytes)?;

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
            formula_bytes = formula_bytes.saturating_add(256 + u64::from(r1 - r0) * 8);
            check(formula_bytes)?;
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

    over(&wb)?;

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

    for i in of(ROWS_SHOWN) {
        let mut r = Reader::new(c.chunk(i)?);
        let st = crate::filter::FilterState::decode(&mut r)?;
        done(&r)?;
        let s = st.sheet as usize;
        if s >= wb.sheets.len() {
            return Err(LoadError::Bad("rows of a sheet that does not exist"));
        }
        if !wb.sheets[s].hidden.is_empty() || wb.sheets[s].filter.is_some() {
            return Err(LoadError::Bad("a sheet's rows twice"));
        }
        wb.sheets[s].hidden = st.hidden;
        wb.sheets[s].filter = st.filter;
    }

    for i in of(HIDDEN_SHEETS) {
        let mut r = Reader::new(c.chunk(i)?);
        let n = r.var()?;
        if n >= wb.sheets.len() as u64 {
            return Err(LoadError::Bad("every sheet hidden"));
        }
        for _ in 0..n {
            let s = r.u16()? as usize;
            let Some(sh) = wb.sheets.get_mut(s) else { return Err(LoadError::Bad("a hidden sheet that does not exist")) };
            if std::mem::replace(&mut sh.sheet_hidden, true) {
                return Err(LoadError::Bad("a sheet hidden twice"));
            }
        }
        done(&r)?;
    }
    if wb.sheets.iter().all(|s| s.sheet_hidden) {
        return Err(LoadError::Bad("every sheet hidden"));
    }
    for i in of(ZOOM) {
        let mut r = Reader::new(c.chunk(i)?);
        let n = r.var()?;
        if n > wb.sheets.len() as u64 {
            return Err(LoadError::Bad("more zoomed sheets than sheets"));
        }
        for _ in 0..n {
            let (s, z) = (r.u16()? as usize, r.u16()?);
            let Some(sh) = wb.sheets.get_mut(s) else { return Err(LoadError::Bad("a zoom for a sheet that does not exist")) };
            if !(crate::book::ZOOM_MIN..=crate::book::ZOOM_MAX).contains(&z) || z == 100 || sh.zoom != 100 {
                return Err(LoadError::Bad("a sheet's zoom"));
            }
            sh.zoom = z;
        }
        done(&r)?;
    }
    for i in of(VIEW_HIDDEN) {
        let mut r = Reader::new(c.chunk(i)?);
        let n = r.var()?;
        if n > wb.sheets.len() as u64 {
            return Err(LoadError::Bad("more plain views than sheets"));
        }
        for _ in 0..n {
            let (s, f) = (r.u16()? as usize, r.u8()?);
            let Some(sh) = wb.sheets.get_mut(s) else { return Err(LoadError::Bad("a view setting for a sheet that does not exist")) };
            if f == 0 || f > 7 || !(sh.show_grid && sh.show_headings && sh.show_zeros) {
                return Err(LoadError::Bad("a sheet's view settings"));
            }
            sh.show_grid = f & 1 == 0;
            sh.show_headings = f & 2 == 0;
            sh.show_zeros = f & 4 == 0;
        }
        done(&r)?;
    }

    let mut looks = of(LOOK);
    if let Some(i) = looks.next() {
        if looks.next().is_some() {
            return Err(LoadError::Bad("repeated look"));
        }
        let mut r = Reader::new(c.chunk(i)?);
        let look = crate::look::Look::decode(&mut r)?;
        look.valid(wb.sheets.len()).map_err(LoadError::Bad)?;
        wb.look = look;
        done(&r)?;
    }

    let mut views = of(CUSTOM_VIEWS);
    if let Some(i) = views.next() {
        if views.next().is_some() {
            return Err(LoadError::Bad("repeated custom views"));
        }
        let mut r = Reader::new(c.chunk(i)?);
        let list = crate::views::decode(&mut r)?;
        if list.iter().any(|v| v.sheet as usize >= wb.sheets.len()) {
            return Err(LoadError::Bad("a custom view of a sheet that does not exist"));
        }
        wb.views = list;
        done(&r)?;
    }

    for i in of(GRID_COLORS) {
        let mut r = Reader::new(c.chunk(i)?);
        let n = r.var()?;
        if n > wb.sheets.len() as u64 {
            return Err(LoadError::Bad("more gridline colours than sheets"));
        }
        for _ in 0..n {
            let s = r.u16()? as usize;
            let color = r.u32()?;
            let Some(sh) = wb.sheets.get_mut(s) else { return Err(LoadError::Bad("a gridline colour for a sheet that does not exist")) };
            if color > 0xFF_FFFF || sh.grid_color.replace(color).is_some() {
                return Err(LoadError::Bad("a sheet's gridline colour"));
            }
        }
        done(&r)?;
    }
    for i in of(TAB_COLORS) {
        let mut r = Reader::new(c.chunk(i)?);
        let n = r.var()?;
        if n > wb.sheets.len() as u64 {
            return Err(LoadError::Bad("more tab colours than sheets"));
        }
        for _ in 0..n {
            let s = r.u16()? as usize;
            let color = r.u32()?;
            let Some(sh) = wb.sheets.get_mut(s) else { return Err(LoadError::Bad("a tab colour for a sheet that does not exist")) };
            if sh.tab_color.replace(color & 0xFF_FFFF).is_some() {
                return Err(LoadError::Bad("a sheet's tab colour twice"));
            }
        }
        done(&r)?;
    }

    for i in of(FREEZE) {
        let mut r = Reader::new(c.chunk(i)?);
        let n = r.var()?;
        if n > wb.sheets.len() as u64 {
            return Err(LoadError::Bad("more frozen panes than sheets"));
        }
        for _ in 0..n {
            let (s, rows, cols) = (r.u16()? as usize, r.u32()?, r.u32()?);
            if rows >= MAX_ROWS || cols >= MAX_COLS || (rows, cols) == (0, 0) {
                return Err(LoadError::Bad("frozen panes"));
            }
            let Some(sh) = wb.sheets.get_mut(s) else { return Err(LoadError::Bad("frozen panes on a sheet that does not exist")) };
            if sh.freeze != (0, 0) {
                return Err(LoadError::Bad("a sheet's frozen panes twice"));
            }
            sh.freeze = (rows, cols);
        }
        done(&r)?;
    }
    for i in of(FREEZE_FROM) {
        let mut r = Reader::new(c.chunk(i)?);
        let n = r.var()?;
        if n > wb.sheets.len() as u64 {
            return Err(LoadError::Bad("more frozen panes than sheets"));
        }
        for _ in 0..n {
            let (s, from) = (r.u16()? as usize, (r.u32()?, r.u32()?));
            let Some(sh) = wb.sheets.get_mut(s) else { return Err(LoadError::Bad("frozen panes on a sheet that does not exist")) };
            if from == (0, 0) || sh.freeze_from != (0, 0) || !crate::book::freeze_from_fits(sh.freeze, from) {
                return Err(LoadError::Bad("where frozen panes start"));
            }
            sh.freeze_from = from;
        }
        done(&r)?;
    }

    for i in of(HIDDEN_WIDTHS) {
        let mut r = Reader::new(c.chunk(i)?);
        let s = r.u16()? as usize;
        let list = crate::ops::decode_widths(&mut r)?;
        let Some(sh) = wb.sheets.get_mut(s) else { return Err(LoadError::Bad("hidden widths for a sheet that does not exist")) };
        if !sh.hidden_widths.is_empty() {
            return Err(LoadError::Bad("a sheet's hidden widths twice"));
        }
        sh.hidden_widths = list;
        done(&r)?;
    }

    for i in of(MERGES) {
        let mut r = Reader::new(c.chunk(i)?);
        let s = r.u16()?;
        let list = crate::merges::decode(&mut r, s)?;
        done(&r)?;
        let Some(sh) = wb.sheets.get(s as usize) else { return Err(LoadError::Bad("merged cells on a sheet that does not exist")) };
        if !sh.merges.is_empty() {
            return Err(LoadError::Bad("a sheet's merged cells twice"));
        }
        wb.set_merges(s, list);
    }

    for i in of(COMMENTS) {
        let mut r = Reader::new(c.chunk(i)?);
        let s = r.u16()?;
        let set = crate::comments::decode(&mut r)?;
        done(&r)?;
        let Some(sh) = wb.sheets.get(s as usize) else { return Err(LoadError::Bad("comments on a sheet that does not exist")) };
        if !sh.comments.is_empty() || set.iter().any(|x| x.2.is_none()) {
            return Err(LoadError::Bad("a sheet's comments twice"));
        }
        wb.set_comments(s, set);
    }

    for i in of(LINKS) {
        let mut r = Reader::new(c.chunk(i)?);
        let s = r.u16()?;
        let set = crate::links::decode(&mut r)?;
        done(&r)?;
        let Some(sh) = wb.sheets.get(s as usize) else { return Err(LoadError::Bad("links on a sheet that does not exist")) };
        if !sh.links.is_empty() || set.iter().any(|x| x.2.is_none()) {
            return Err(LoadError::Bad("a sheet's links twice"));
        }
        wb.set_links(s, set);
    }

    // Pivot tables: their output is in the cells already.
    let mut it = of(PIVOTS);
    if let Some(i) = it.next() {
        if it.next().is_some() {
            return Err(LoadError::Bad("two PIVOTS chunks"));
        }
        let mut r = Reader::new(c.chunk(i)?);
        let k = r.count("pivot tables", crate::pivot::MAX_PIVOTS as u64, 8)?;
        for _ in 0..k {
            let (p, area) = crate::pivot::Pivot::decode(&mut r)?;
            wb.add_pivot_as_is(p, area).map_err(LoadError::Bad)?;
        }
        done(&r)?;
    }

    // Stored results are current; only volatile formulas need a recalculation, and
    // `recalc` always takes those. Spills aren't stored: formulas that may spill
    // are evaluated again to write them.
    wb.changed.clear();
    wb.pending.clear();
    let spilling: Vec<(crate::deps::GroupId, u32, u32)> = wb.groups().filter(|(_, g)| crate::eval::arrays::can_array(&g.expr)).map(|(id, g)| (id, g.r0, g.r1)).collect();
    for (id, a, b) in spilling {
        wb.pending.entry(id).or_default().add(a, b);
    }
    over(&wb)?;
    Ok(wb)
}

fn size(r: &mut Reader, hidden_column_allowed: bool) -> Result<f64, LoadError> {
    let x = r.f64()?;
    if x.is_finite() && (x > 0.0 || (hidden_column_allowed && x == 0.0)) && x <= MAX_SIZE {
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
