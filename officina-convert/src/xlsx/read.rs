//! `.xlsx` / `.xlsm` in (ECMA-376 SpreadsheetML, transitional), for the converter
//! process.
//!
//! Read: sheets and their names, values (numbers, shared and inline strings, booleans,
//! errors), formulas (plain, shared fills, array formulas as their first cell), column
//! widths and row heights, the 1904 date system. Formula cells keep the file's cached
//! value until recalculation; the converter compares the two (a differential against
//! whatever wrote the file, Excel included) and reports differences.
//!
//! Styles and number formats come from `xl/styles.xml` ([`super::styles`]).
//!
//! Reported, not carried (O4c–O4e): charts and
//! drawings, defined names, tables, conditional formats, data validation,
//! Kept inert: macros (`vbaProject.bin`, see [`super::macros`]), external links.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use officina_grid::eval::dates::DateSystem;
use officina_grid::filter::{valid_calendar_type, AppearanceFilter, AutoFilter, CalendarSelection, Cond, DateBounds, DateGroup, IconFilter,
    DynamicDate, Rank, Test, TextOp, TextTest, GROUP_FIELDS, MAX_SELECTIONS};
use officina_grid::tables::Table;
use officina_grid::addr::{a1, parse_a1, CellAddr, Rect, MAX_COLS, MAX_ROWS};
use officina_grid::column::Sizes;
use officina_grid::eval::arrays;
use officina_grid::formula::{self, Expr};
use officina_grid::strings::MAX_TEXT;
use officina_grid::style::{StyleId, StyleRuns};
use officina_grid::{ErrorKind, Value, Workbook};

use crate::report::{Kind, Report};
use crate::zip::{Archive, ZipError};

pub use super::MDW;
/// Shared strings a file may hold.
pub const MAX_SHARED: usize = 1 << 26;
/// Formula cells per workbook the reader takes.
pub const MAX_FORMULAS: usize = 1 << 27;

#[derive(Debug)]
pub enum XlsxError {
    Zip(ZipError),
    Xml(String),
    Bad(String),
}

impl std::fmt::Display for XlsxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            XlsxError::Zip(e) => write!(f, "{e}"),
            XlsxError::Xml(e) => write!(f, "damaged spreadsheet XML: {e}"),
            XlsxError::Bad(e) => write!(f, "not a spreadsheet this can read: {e}"),
        }
    }
}

impl std::error::Error for XlsxError {}

impl From<ZipError> for XlsxError {
    fn from(e: ZipError) -> Self {
        XlsxError::Zip(e)
    }
}
impl From<quick_xml::Error> for XlsxError {
    fn from(e: quick_xml::Error) -> Self {
        XlsxError::Xml(e.to_string())
    }
}
impl From<quick_xml::events::attributes::AttrError> for XlsxError {
    fn from(e: quick_xml::events::attributes::AttrError) -> Self {
        XlsxError::Xml(e.to_string())
    }
}

fn bad(s: impl Into<String>) -> XlsxError {
    XlsxError::Bad(s.into())
}

/// An attribute's value by local name.
pub(crate) fn attr(e: &BytesStart, name: &[u8]) -> Result<Option<String>, XlsxError> {
    for a in e.attributes() {
        let a = a?;
        if a.key.local_name().as_ref() == name {
            return Ok(Some(a.unescape_value().map_err(|e| XlsxError::Xml(e.to_string()))?.into_owned()));
        }
    }
    Ok(None)
}

pub(crate) fn xml(data: &[u8]) -> Reader<&[u8]> {
    let mut r = Reader::from_reader(data);
    r.config_mut().trim_text(false);
    r
}

/// `xl/workbook.xml` → `xl/worksheets/sheet1.xml`.
fn resolve(base: &str, target: &str) -> String {
    if let Some(t) = target.strip_prefix('/') {
        return t.to_string();
    }
    let mut parts: Vec<&str> = base.split('/').collect();
    parts.pop();
    for seg in target.split('/') {
        match seg {
            ".." => {
                parts.pop();
            }
            "." | "" => {}
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// A part's relationships: id → (type, resolved target).
pub(crate) fn rels(z: &Archive, part: &str) -> Result<HashMap<String, (String, String)>, XlsxError> {
    read_rels(z, part, None)
}

pub(crate) fn internal_rels(z: &Archive, part: &str) -> Result<HashMap<String, (String, String)>, XlsxError> {
    read_rels(z, part, Some(false))
}

pub(crate) fn external_rels(z: &Archive, part: &str) -> Result<HashMap<String, (String, String)>, XlsxError> {
    read_rels(z, part, Some(true))
}

fn read_rels(z: &Archive, part: &str, external_only: Option<bool>) -> Result<HashMap<String, (String, String)>, XlsxError> {
    let (dir, file) = part.rsplit_once('/').unwrap_or(("", part));
    let path = if dir.is_empty() { format!("_rels/{file}.rels") } else { format!("{dir}/_rels/{file}.rels") };
    let mut out = HashMap::new();
    let Some(data) = z.read_name(&path)? else { return Ok(out) };
    let mut r = xml(&data);
    let mut buf = Vec::new();
    loop {
        match r.read_event_into(&mut buf)? {
            Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == b"Relationship" => {
                let (Some(id), Some(target)) = (attr(&e, b"Id")?, attr(&e, b"Target")?) else { continue };
                let ty = attr(&e, b"Type")?.unwrap_or_default();
                let external = attr(&e, b"TargetMode")?.is_some_and(|m| m == "External");
                if external_only.is_some_and(|want| want != external) { continue; }
                let t = if external { target } else { resolve(part, &target) };
                out.insert(id, (ty, t));
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    Ok(out)
}

/// The sheet's links that name a relationship, given its target (an external
/// address, with `#place` after it when the link has a location too).
fn resolve_links(wb: &mut Workbook, s: u16, z: &Archive, part: &str) -> Result<(), XlsxError> {
    use officina_grid::links::Link;
    let pending: Vec<_> = wb.links(s).iter().filter(|(_, l)| l.target.starts_with('\u{1}')).map(|(&k, l)| (k, l.target.to_string())).collect();
    if pending.is_empty() {
        return Ok(());
    }
    let rel = rels(z, part)?;
    let set = pending
        .into_iter()
        .map(|((r, c), t)| {
            let t = &t[1..];
            let (id, place) = t.split_once('#').map_or((t, None), |(a, b)| (a, Some(b)));
            let to = rel.get(id).filter(|(ty, _)| ty.ends_with("/hyperlink")).map(|(_, target)| match place {
                Some(p) => format!("{target}#{p}"),
                None => target.clone(),
            });
            (r, c, to.and_then(|t| Link::new(&t)))
        })
        .collect();
    wb.set_links(s, set);
    Ok(())
}

/// What came out of a file, besides the workbook.
#[derive(Debug, Default)]
pub struct Loaded {
    pub report: Report,
    /// Formula cells whose recalculated value differs from the file's cached value.
    pub differ: u64,
    pub formulas: u64,
    pub cells: u64,
    pub date1904: bool,
}

fn text_of(r: &mut Reader<&[u8]>, end: &[u8], buf: &mut Vec<u8>, out: &mut String, cut: &mut bool) -> Result<(), XlsxError> {
    // Collects all <t> text under the current element (rich runs included, phonetic
    // runs skipped) until `end` closes.
    let mut in_t = false;
    let mut skip = 0u32;
    loop {
        buf.clear();
        match r.read_event_into(buf)? {
            Event::Start(e) => match e.local_name().as_ref() {
                b"t" if skip == 0 => in_t = true,
                b"rPh" => skip += 1,
                _ => {}
            },
            Event::End(e) => {
                let n = e.local_name();
                if n.as_ref() == end {
                    return Ok(());
                }
                match n.as_ref() {
                    b"t" => in_t = false,
                    b"rPh" => skip = skip.saturating_sub(1),
                    _ => {}
                }
            }
            Event::Text(t) if in_t => {
                let s = t.unescape().map_err(|e| XlsxError::Xml(e.to_string()))?;
                push_capped(out, &s, cut);
            }
            Event::CData(t) if in_t => push_capped(out, &String::from_utf8_lossy(&t), cut),
            Event::Eof => return Err(bad("text ended early")),
            _ => {}
        }
    }
}

fn push_capped(out: &mut String, s: &str, cut: &mut bool) {
    let room = MAX_TEXT.saturating_sub(out.len());
    if s.len() <= room {
        out.push_str(&decode_escapes(s));
    } else {
        let mut end = room;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        out.push_str(&s[..end]);
        *cut = true;
    }
}

/// OOXML's `_xHHHH_` escapes for characters XML can't hold.
pub(crate) fn decode_escapes(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.contains("_x") {
        return s.into();
    }
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'_' && i + 7 <= b.len() && b[i + 1] == b'x' && b[i + 6] == b'_' {
            if let Ok(v) = u32::from_str_radix(&s[i + 2..i + 6], 16) {
                if let Some(c) = char::from_u32(v) {
                    out.push(c);
                    i += 7;
                    continue;
                }
            }
        }
        let c = s[i..].chars().next().unwrap();
        out.push(c);
        i += c.len_utf8();
    }
    out.into()
}

fn shared_strings(z: &Archive, path: Option<&str>, rep: &mut Report) -> Result<Vec<Arc<str>>, XlsxError> {
    let Some(path) = path else { return Ok(Vec::new()) };
    let Some(data) = z.read_name(path)? else { return Ok(Vec::new()) };
    let mut r = xml(&data);
    let mut buf = Vec::new();
    let mut tbuf = Vec::new();
    let mut out = Vec::new();
    loop {
        buf.clear();
        match r.read_event_into(&mut buf)? {
            Event::Start(e) if e.local_name().as_ref() == b"si" => {
                let mut s = String::new();
                let mut cut = false;
                text_of(&mut r, b"si", &mut tbuf, &mut s, &mut cut)?;
                if cut {
                    rep.note(Kind::Approximate, "text longer than a cell holds was cut");
                }
                if out.len() >= MAX_SHARED {
                    return Err(bad("too many shared strings"));
                }
                out.push(Arc::from(s));
            }
            Event::Empty(e) if e.local_name().as_ref() == b"si" => out.push(Arc::from("")),
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

/// Excel's formula text to ours: the `_xlfn.` and `_xlws.` prefixes newer functions
/// carry in files, and `_xlpm.` on LET names and LAMBDA parameters, are dropped.
pub fn formula_in(f: &str) -> String {
    // LibreOffice before 24.8 copies OpenFormula's names for Excel's newer
    // functions (`COM.MICROSOFT.XLOOKUP`) into the `.xlsx` it saves
    let owned;
    // (24.2 in lower case).
    let body = if f.contains("COM.MICROSOFT.") || f.contains("com.microsoft.") {
        owned = f.trim().replace("COM.MICROSOFT.", "").replace("com.microsoft.", "");
        owned.as_str()
    } else {
        f.trim()
    };
    let mut s = String::with_capacity(body.len() + 1);
    s.push('=');
    if !body.contains("_xl") {
        s.push_str(body);
        return s;
    }
    let mut rest = body;
    while let Some(i) = rest.find("_xl") {
        s.push_str(&rest[..i]);
        let tail = &rest[i..];
        if let Some(t) = tail.strip_prefix("_xlfn.").or_else(|| tail.strip_prefix("_xlws.")).or_else(|| tail.strip_prefix("_xlpm.")).or_else(|| tail.strip_prefix("_xleta.")) {
            rest = t;
        } else {
            s.push_str("_xl");
            rest = &tail[3..];
        }
    }
    s.push_str(rest);
    s
}

struct Pending {
    /// Per column: a run of numbers being collected (first row, values).
    runs: Vec<Option<(u32, Vec<f64>)>>,
}

type Nums = Vec<(u32, u32, Vec<f64>)>;

impl Pending {
    fn flush(&mut self, out: &mut Nums, col: u32) {
        if let Some((r0, v)) = self.runs.get_mut(col as usize).and_then(Option::take) {
            out.push((col, r0, v));
        }
    }
    fn num(&mut self, out: &mut Nums, row: u32, col: u32, x: f64) {
        let c = col as usize;
        if self.runs.len() <= c {
            self.runs.resize_with(c + 1, || None);
        }
        match &mut self.runs[c] {
            Some((r0, v)) if *r0 + v.len() as u32 == row && v.len() < 1 << 16 => v.push(x),
            _ => {
                self.flush(out, col);
                self.runs[c] = Some((row, vec![x]));
            }
        }
    }
    fn all(&mut self, out: &mut Nums) {
        for c in 0..self.runs.len() {
            self.flush(out, c as u32);
        }
    }
}

pub(crate) enum Src {
    Text(String),
    Shared(u32),
    /// An array formula (dynamic, or Ctrl+Shift+Enter): its text and the area its
    /// result holds in the file.
    Array(String, Option<(u32, u32, u32, u32)>),
}

pub(crate) struct FCell {
    pub(crate) row: u32,
    pub(crate) col: u32,
    pub(crate) src: Src,
    pub(crate) cached: Value,
}

/// What one piece of a sheet's XML holds (whole rows; the first piece also what comes
/// before them, the last what comes after).
#[derive(Default)]
struct Seg {
    nums: Nums,
    vals: Vec<(u32, u32, Value)>,
    fcells: Vec<FCell>,
    shared: HashMap<u32, (u32, u32, String)>,
    widths: Vec<(u32, f64)>,
    heights: Vec<(u32, f64)>,
    default_w: Option<f64>,
    default_h: Option<f64>,
    /// Per column: runs of cells' `cellXfs` index, (first row, last row, xf).
    sruns: Vec<Vec<(u32, u32, u32)>>,
    /// `<col style>`: (first column, last column, xf), 0-based.
    col_styles: Vec<(u32, u32, u32)>,
    row_styles: Vec<(u32, u32)>,
    cells: u64,
    rep: Report,
    first_row: Option<u32>,
    last_row: Option<u32>,
    outline: Vec<(u32, super::outline::Row)>,
    summary_above: bool,
    /// Rows with `hidden="1"`.
    hidden: Vec<u32>,
    /// `<autoFilter ref>` and its columns' conditions.
    filter: Option<(String, Vec<(u32, Cond)>)>,
    /// `<conditionalFormatting>`s, read once the workbook's formulas can be.
    cond: Vec<super::condfmt::Raw>,
    /// Validation XML, read once formulas can be.
    valid: super::validation::Raw,
    /// Default formats for successful ISO date cells.
    iso_dates: Vec<(u32, u32, &'static str)>,
    protection: Option<officina_grid::protection::SheetProtection>,
    /// `<sheetPr><tabColor>`, 0xRRGGBB.
    tab: Option<u32>,
    /// A frozen `<pane>`: the first unfrozen row and column, and the first
    /// frozen ones (the `<sheetView topLeftCell>`, where the view started).
    freeze: Option<((u32, u32), (u32, u32))>,
    /// The `<sheetView topLeftCell>`, for a frozen pane after it.
    view_top: (u32, u32),
    /// The `<sheetView zoomScale>`, when in Excel's 10–400.
    zoom: Option<u16>,
    /// `<sheetView showGridLines="0">` and `showRowColHeaders="0"`.
    no_grid: bool,
    no_headings: bool,
    /// `<mergeCell ref>`s, (r0, c0, r1, c1).
    merges: Vec<(u32, u32, u32, u32)>,
    /// `<hyperlink>`s: cells, relationship id, location.
    links: Vec<((u32, u32, u32, u32), Option<String>, Option<String>)>,
}

/// Parses one piece of a sheet. Pieces after the first must number their rows.
fn segment(data: &[u8], sst: &[Arc<str>], theme: &[u32], first: bool, system: DateSystem, dxfs: &[super::condfmt::DxfIn]) -> Result<Seg, XlsxError> {
    let mut seg = Seg::default();
    let mut r = xml(data);
    r.config_mut().check_end_names = false;
    r.config_mut().allow_unmatched_ends = true;
    let mut buf = Vec::new();
    let mut tbuf = Vec::new();
    let mut pend = Pending { runs: Vec::new() };
    let mut row_xf = 0;
    let (mut row, mut col): (i64, i64) = (-1, -1);
    let mut vtext: Vec<u8> = Vec::new();
    let mut vbuf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        let tag_start = r.buffer_position() as usize;
        let ev = r.read_event_into(&mut buf)?;
        let (e, empty) = match ev {
            Event::Start(e) => (e, false),
            Event::Empty(e) => (e, true),
            Event::Eof => break,
            _ => continue,
        };
        match e.local_name().as_ref() {
            b"worksheet" if first => {
                for a in e.attributes().flatten().filter(|a| a.key.as_ref() == b"xmlns" || a.key.as_ref().starts_with(b"xmlns:")) {
                    seg.valid.root_namespaces.push(b' ');
                    seg.valid.root_namespaces.extend_from_slice(a.key.as_ref());
                    seg.valid.root_namespaces.extend_from_slice(b"=\"");
                    let value = a.unescape_value()?;
                    seg.valid.root_namespaces.extend_from_slice(crate::xml::esc(&value).as_bytes());
                    seg.valid.root_namespaces.push(b'"');
                }
            }
            b"outlinePr" if first => seg.summary_above = attr(&e, b"summaryBelow")?.is_some_and(|v| v == "0" || v == "false"),
            b"tabColor" if first => seg.tab = super::styles::color_of(&e, theme),
            b"sheetFormatPr" => {
                seg.default_h = attr(&e, b"defaultRowHeight")?.and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite() && *v > 0.0 && *v < 410.0);
                seg.default_w = attr(&e, b"defaultColWidth")?.and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite() && *v > 0.0 && *v <= 255.0);
            }
            b"col" => {
                let (Some(min), Some(max)) = (attr(&e, b"min")?.and_then(|v| v.parse::<u32>().ok()), attr(&e, b"max")?.and_then(|v| v.parse::<u32>().ok())) else { continue };
                let hidden = attr(&e, b"hidden")?.is_some_and(|v| v == "1" || v == "true");
                let given = attr(&e, b"width")?.and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite() && *v >= 0.0 && *v <= 255.0);
                let w = if hidden { Some(0.0) } else { given };
                if let Some(xf) = attr(&e, b"style")?.and_then(|v| v.parse::<u32>().ok()).filter(|&x| x != 0) {
                    seg.col_styles.push((min.clamp(1, MAX_COLS) - 1, max.clamp(1, MAX_COLS) - 1, xf));
                }
                if let Some(w) = w {
                    // Hidden: no width (Sheets hides columns so).
                    // Hidden: its width kept as a negative, to remember.
                    let px = if hidden { given.filter(|g| *g > 0.0).map_or(0.0, |g| -(g * MDW).round().max(2.0)) } else { (w * MDW).round().max(2.0) };
                    let (min, max) = (min.clamp(1, MAX_COLS), max.clamp(1, MAX_COLS));
                    // A range over every column is a default, not 16,384 overrides.
                    if max - min > 4096 {
                        if !hidden {
                            seg.default_w = Some(w);
                        }
                    } else {
                        for c in min..=max {
                            seg.widths.push((c - 1, px));
                        }
                    }
                }
            }
            b"row" => {
                let rr = match attr(&e, b"r")?.and_then(|v| v.parse::<i64>().ok()) {
                    Some(v) => v - 1,
                    None if first || row >= 0 => row + 1,
                    None => return Err(bad("a row without its number in a large sheet")),
                };
                if rr < 0 || rr >= MAX_ROWS as i64 || rr <= row {
                    return Err(bad("rows out of order"));
                }
                row = rr;
                let level = attr(&e, b"outlineLevel")?.map_or(0, |v| v.parse::<u8>().unwrap_or(255));
                let summary = attr(&e, b"collapsed")?.is_some_and(|v| v == "1" || v == "true");
                if (level != 0 || summary) && seg.outline.len() <= super::outline::MAX_ROW_RECORDS {
                    seg.outline.push((rr as u32, super::outline::Row { level, summary }));
                }
                col = -1;
                seg.first_row.get_or_insert(rr as u32);
                seg.last_row = Some(rr as u32);
                row_xf = if attr(&e, b"customFormat")?.is_some_and(|v| v == "1" || v == "true") {
                    attr(&e, b"s")?.and_then(|v| v.parse().ok()).unwrap_or(0)
                } else {
                    0
                };
                if row_xf != 0 {
                    seg.row_styles.push((row as u32, row_xf));
                }
                if attr(&e, b"customHeight")?.is_some_and(|v| v == "1" || v == "true") {
                    if let Some(h) = attr(&e, b"ht")?.and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite() && *v >= 0.0 && *v < 410.0) {
                        seg.heights.push((row as u32, (h * 96.0 / 72.0).round().max(2.0)));
                    }
                }
                if attr(&e, b"hidden")?.is_some_and(|v| v == "1" || v == "true") {
                    seg.hidden.push(row as u32);
                }
                let _ = empty;
            }
            b"c" => {
                // One pass over the attributes, no allocation unless escaped.
                let (mut r_at, mut t_at, mut xf) = (None::<std::borrow::Cow<[u8]>>, None::<std::borrow::Cow<[u8]>>, row_xf);
                for a in e.attributes().with_checks(false) {
                    let a = a?;
                    match a.key.local_name().as_ref() {
                        b"r" => r_at = Some(a.value),
                        b"t" => t_at = Some(a.value),
                        b"s" => xf = std::str::from_utf8(&a.value).ok().and_then(|v| v.parse().ok()).unwrap_or(0),
                        _ => {}
                    }
                }
                let (rr, cc) = match r_at.as_deref() {
                    Some(a) => std::str::from_utf8(a).ok().and_then(parse_a1).map(|(r, c)| (r as i64, c as i64)).ok_or_else(|| bad(format!("bad cell reference {}", String::from_utf8_lossy(a))))?,
                    None => (row.max(0), col + 1),
                };
                if rr != row.max(0) || cc <= col || cc >= MAX_COLS as i64 {
                    return Err(bad("cells out of order"));
                }
                if row < 0 {
                    row = rr;
                }
                col = cc;
                let (rr, cc) = (rr as u32, cc as u32);
                let t: Option<String> = match t_at.as_deref() {
                    None | Some(b"n") => None,
                    Some(x) => Some(String::from_utf8_lossy(x).into_owned()),
                };
                // Runs of one style down a column (style 0 too: it ends a run).
                let ci = cc as usize;
                if seg.sruns.len() <= ci {
                    seg.sruns.resize_with(ci + 1, Vec::new);
                }
                let col_runs = &mut seg.sruns[ci];
                match col_runs.last_mut() {
                    Some(l) if l.2 == xf && l.1 + 1 == rr => l.1 = rr,
                    _ => col_runs.push((rr, rr, xf)),
                }
                if empty {
                    continue;
                }
                // Children: <f>, <v>, <is>.
                let mut has_v = false;
                let mut inline: Option<String> = None;
                let mut f: Option<Src> = None;
                loop {
                    tbuf.clear();
                    match r.read_event_into(&mut tbuf)? {
                        Event::Start(ce) => match ce.local_name().as_ref() {
                            b"v" => {
                                // The hot path: a number's text, read in place.
                                vtext.clear();
                                loop {
                                    vbuf.clear();
                                    match r.read_event_into(&mut vbuf)? {
                                        Event::Text(t) => {
                                            if t.contains(&b'&') {
                                                vtext.extend_from_slice(t.unescape().map_err(|e| XlsxError::Xml(e.to_string()))?.as_bytes());
                                            } else {
                                                vtext.extend_from_slice(&t);
                                            }
                                            if vtext.len() > MAX_TEXT {
                                                return Err(bad("a value too long"));
                                            }
                                        }
                                        Event::End(_) => break,
                                        Event::Eof => return Err(bad("value ended early")),
                                        _ => {}
                                    }
                                }
                                has_v = true;
                            }
                            b"is" => {
                                let mut s = String::new();
                                let mut cut = false;
                                let mut b2 = Vec::new();
                                text_of(&mut r, b"is", &mut b2, &mut s, &mut cut)?;
                                inline = Some(s);
                            }
                            b"f" => {
                                let ft = attr(&ce, b"t")?;
                                let si = attr(&ce, b"si")?.and_then(|v| v.parse::<u32>().ok());
                                let area = attr(&ce, b"ref")?.as_deref().and_then(parse_ref);
                                let mut s = String::new();
                                read_text(&mut r, b"f", &mut s)?;
                                f = formula_src(ft.as_deref(), si, area, s, rr, cc, &mut seg.shared, &mut seg.rep);
                            }
                            _ => skip(&mut r, &ce)?,
                        },
                        Event::Empty(ce) if ce.local_name().as_ref() == b"f" => {
                            let ft = attr(&ce, b"t")?;
                            let si = attr(&ce, b"si")?.and_then(|v| v.parse::<u32>().ok());
                            f = formula_src(ft.as_deref(), si, None, String::new(), rr, cc, &mut seg.shared, &mut seg.rep);
                        }
                        Event::End(ce) if ce.local_name().as_ref() == b"c" => break,
                        Event::Eof => return Err(bad("cell ended early")),
                        _ => {}
                    }
                }
                let v: Option<&str> = if has_v { Some(std::str::from_utf8(&vtext).map_err(|_| bad("a value that isn't UTF-8"))?) } else { None };
                let value = match t.as_deref().unwrap_or("n") {
                    "n" => match v.map(str::trim) {
                        Some(x) if !x.is_empty() => Value::num(x.parse::<f64>().map_err(|_| bad(format!("bad number in {}", a1(rr, cc))))?),
                        _ => Value::Empty,
                    },
                    "s" => {
                        let i: usize = v.and_then(|x| x.trim().parse().ok()).ok_or_else(|| bad("bad shared string index"))?;
                        Value::Str(sst.get(i).cloned().ok_or_else(|| bad("shared string index out of range"))?)
                    }
                    "str" => Value::str(v.unwrap_or("")),
                    "inlineStr" => Value::str(inline.as_deref().unwrap_or("")),
                    "b" => Value::Bool(v.map(str::trim) == Some("1")),
                    "e" => Value::Err(v.and_then(|x| ErrorKind::from_text(x.trim())).unwrap_or(ErrorKind::Value)),
                    "d" => match v.filter(|s| !s.trim().is_empty()) {
                        Some(text) => match system.iso(text) {
                            Some((serial, format)) => {
                                seg.iso_dates.push((rr, cc, format));
                                Value::Num(serial)
                            }
                            None => {
                                seg.rep.note_at(Kind::Approximate, "invalid or out-of-range ISO date cells kept as text", || a1(rr, cc));
                                Value::str(text)
                            }
                        },
                        None => Value::Empty,
                    },
                    _ => return Err(bad("unknown cell type")),
                };
                seg.cells += 1;
                if let Some(src) = f {
                    if seg.fcells.len() >= MAX_FORMULAS {
                        return Err(bad("too many formulas"));
                    }
                    seg.fcells.push(FCell { row: rr, col: cc, src, cached: value.clone() });
                }
                match value {
                    Value::Num(x) => pend.num(&mut seg.nums, rr, cc, x),
                    Value::Empty => {}
                    v => {
                        pend.flush(&mut seg.nums, cc);
                        seg.vals.push((rr, cc, v));
                    }
                }
            }
            b"sheetProtection" if attr(&e, b"sheet")?.is_some_and(|v| v == "1" || v == "true") => {
                use officina_grid::protection::ProtectionFormat;
                let attrs = e.attributes().with_checks(false).flatten().filter_map(|a| {
                    let key = std::str::from_utf8(a.key.local_name().as_ref()).ok()?.to_string();
                    Some((key, a.unescape_value().ok()?.into_owned()))
                }).collect::<Vec<_>>();
                seg.protection = Some(crate::sheet_protection::read(
                    ProtectionFormat::Excel,
                    attrs.iter().map(|(key, value)| (key.as_str(), value.as_str())),
                    &mut seg.rep,
                ));
            }
            b"mergeCell" => {
                if let Some(rect) = attr(&e, b"ref")?.as_deref().and_then(parse_ref) {
                    if seg.merges.len() < officina_grid::merges::MAX_MERGES {
                        seg.merges.push(rect);
                    }
                }
            }
            b"conditionalFormatting" if !empty && e.name().as_ref() == b"conditionalFormatting" => {
                let sqref = attr(&e, b"sqref")?.unwrap_or_default();
                let end = e.name().as_ref().to_vec();
                let span = r.read_to_end(quick_xml::name::QName(&end))?;
                let inner = data.get(span.start as usize..span.end as usize).unwrap_or_default().to_vec();
                if seg.cond.len() < 1 << 16 {
                    seg.cond.push(super::condfmt::Raw { sqref, inner, extended: false });
                }
            }
            b"ext" if !empty && attr(&e, b"uri")?.is_some_and(|uri| uri.eq_ignore_ascii_case(super::condfmt::EXT_URI)) => {
                let span = r.read_to_end(e.name())?;
                let inner = data.get(span.start as usize..span.end as usize).unwrap_or_default();
                super::condfmt::read_extension(inner, &mut seg.cond)?;
            }
            b"conditionalFormatting" | b"conditionalFormattings" => seg.rep.note(Kind::Dropped, "conditional formats from Excel 2010's extensions"),
            b"dataValidations" if !empty && e.name().as_ref() == b"dataValidations" => {
                let end = e.name().as_ref().to_vec();
                r.read_to_end(quick_xml::name::QName(&end))?;
                if seg.valid.base.is_empty() {
                    seg.valid.base = data[tag_start..r.buffer_position() as usize].to_vec();
                }
            }
            b"ext" if !empty && attr(&e, b"uri")?.is_some_and(|uri| uri.eq_ignore_ascii_case(super::validation::EXT_URI)) => {
                let end = e.name().as_ref().to_vec();
                r.read_to_end(quick_xml::name::QName(&end))?;
                if seg.valid.extension.is_none() {
                    seg.valid.extension = Some(data[tag_start..r.buffer_position() as usize].to_vec());
                }
            }
            b"dataValidation" => seg.rep.note(Kind::Dropped, "data validation rules outside a recognized worksheet container"),
            b"hyperlink" => {
                if let Some(rect) = attr(&e, b"ref")?.as_deref().and_then(parse_ref) {
                    if seg.links.len() < officina_grid::links::MAX_LINKS {
                        seg.links.push((rect, attr(&e, b"id")?, attr(&e, b"location")?));
                    }
                }
            }
            b"autoFilter" => seg.filter = read_auto_filter(&mut r, &e, empty, system, dxfs, &mut seg.rep)?,
            b"sheetView" => {
                seg.view_top = attr(&e, b"topLeftCell")?.as_deref().and_then(parse_a1).unwrap_or((0, 0));
                seg.zoom = attr(&e, b"zoomScale")?.and_then(|v| v.parse::<u16>().ok()).filter(|z| (officina_grid::book::ZOOM_MIN..=officina_grid::book::ZOOM_MAX).contains(z));
                let off = |v: Option<String>| v.is_some_and(|v| v == "0" || v == "false");
                seg.no_grid = off(attr(&e, b"showGridLines")?);
                seg.no_headings = off(attr(&e, b"showRowColHeaders")?);
            }
            b"pane" if attr(&e, b"state")?.is_some_and(|v| v.starts_with("frozen")) => {
                // xSplit and ySplit count the frozen columns and rows, from
                // the view's top-left cell.
                let split = |v: Option<String>| v.and_then(|v| v.parse::<f64>().ok()).filter(|x| x.is_finite() && *x >= 0.0).map_or(0, |x| x as u32);
                let (n_rows, n_cols) = (split(attr(&e, b"ySplit")?), split(attr(&e, b"xSplit")?));
                let from = (if n_rows > 0 { seg.view_top.0 } else { 0 }, if n_cols > 0 { seg.view_top.1 } else { 0 });
                let rows = from.0.saturating_add(n_rows).min(officina_grid::addr::MAX_ROWS - 1);
                let cols = from.1.saturating_add(n_cols).min(officina_grid::addr::MAX_COLS - 1);
                if (rows, cols) != (0, 0) {
                    let from = if officina_grid::book::freeze_from_fits((rows, cols), from) { from } else { (0, 0) };
                    seg.freeze = Some(((rows, cols), from));
                }
            }
            b"pane" if attr(&e, b"xSplit")?.is_some() || attr(&e, b"ySplit")?.is_some() => seg.rep.note(Kind::Dropped, "split (not frozen) panes"),
            _ => {}
        }
    }
    pend.all(&mut seg.nums);
    Ok(seg)
}

/// A sheet part's size past which it is parsed in pieces on every core.
const PIECE: usize = 8 << 20;

/// Reads one sheet part into sheet `s`: inflated on one thread and cut into pieces of
/// whole rows, the pieces parsed on all the others, merged here in order.
#[allow(clippy::too_many_arguments)]
fn sheet(wb: &mut Workbook, s: u16, z: &Archive, part: &crate::zip::Entry, sst: &[Arc<str>], theme: &[u32], xf_ids: &[StyleId], dxfs: &[super::condfmt::DxfIn], out: &mut Loaded, rep: &mut Report, fcells: &mut Vec<FCell>, shared: &mut HashMap<u32, (u32, u32, String)>, cond: &mut Vec<super::condfmt::Raw>, valid: &mut super::validation::Raw) -> Result<(), XlsxError> {
    use std::io::Read;
    use std::sync::mpsc::{channel, sync_channel};
    use std::sync::Mutex;
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, 16);
    let mut reader = z.reader(part)?;
    let system = wb.date_system();
    let (piece_tx, piece_rx) = sync_channel::<(usize, Vec<u8>)>(workers * 2);
    let piece_rx = Mutex::new(piece_rx);
    let (seg_tx, seg_rx) = channel::<(usize, Result<Seg, XlsxError>)>();
    std::thread::scope(|sc| -> Result<(), XlsxError> {
        // Inflate and cut at row ends.
        let cutter = sc.spawn(move || -> Result<(), XlsxError> {
            let mut carry: Vec<u8> = Vec::new();
            let mut idx = 0;
            loop {
                let mut block = std::mem::take(&mut carry);
                let start = block.len();
                block.resize(start + PIECE, 0);
                let mut got = 0;
                while start + got < block.len() {
                    let n = reader.read(&mut block[start + got..]).map_err(|e| XlsxError::Zip(ZipError::Io(e)))?;
                    if n == 0 {
                        break;
                    }
                    got += n;
                }
                block.truncate(start + got);
                if got == 0 {
                    if !block.is_empty() && piece_tx.send((idx, block)).is_err() {
                        return Ok(());
                    }
                    return Ok(());
                }
                // Cut after the last whole row; the rest starts the next piece.
                let cut = memrchr_row_end(&block).unwrap_or(0);
                if cut == 0 {
                    // No row ends here yet: read on (a row bigger than a piece).
                    if block.len() > 64 * PIECE {
                        return Err(bad("a row too big"));
                    }
                    carry = block;
                    continue;
                }
                carry = block.split_off(cut);
                if piece_tx.send((idx, block)).is_err() {
                    return Ok(());
                }
                idx += 1;
            }
        });
        let piece_rx = &piece_rx;
        for _ in 0..workers {
            let seg_tx = seg_tx.clone();
            sc.spawn(move || loop {
                let next = piece_rx.lock().unwrap().recv();
                let Ok((i, data)) = next else { return };
                if seg_tx.send((i, segment(&data, sst, theme, i == 0, system, dxfs))).is_err() {
                    return;
                }
            });
        }
        drop(seg_tx);
        // Merge in order as pieces come back.
        let mut waiting: std::collections::BTreeMap<usize, Seg> = std::collections::BTreeMap::new();
        let mut next = 0;
        let mut last_row: Option<u32> = None;
        let mut widths = Vec::new();
        let mut heights = Vec::new();
        let mut outline_rows = std::collections::BTreeMap::new();
        let mut outline_too_large = false;
        let mut summary_above = false;
        let mut hidden: Vec<u32> = Vec::new();
        let mut merges: Vec<(u32, u32, u32, u32)> = Vec::new();
        let mut links = Vec::new();
        let mut filter = None;
        let mut sruns: Vec<Vec<(u32, u32, u32)>> = Vec::new();
        let mut iso_dates = Vec::new();
        let mut col_styles: Vec<(u32, u32, u32)> = Vec::new();
        let mut row_styles = Vec::new();
        let (mut dw, mut dh) = (None, None);
        let mut failed = None;
        for (i, seg) in seg_rx {
            match seg {
                Ok(seg) => {
                    waiting.insert(i, seg);
                }
                Err(e) => {
                    failed.get_or_insert(e);
                    continue;
                }
            }
            while let Some(seg) = waiting.remove(&next) {
                next += 1;
                if failed.is_some() {
                    continue;
                }
                if let (Some(a), Some(b)) = (last_row, seg.first_row) {
                    if b <= a {
                        failed = Some(bad("rows out of order"));
                        continue;
                    }
                }
                last_row = seg.last_row.or(last_row);
                for (col, r0, v) in seg.nums {
                    wb.set_numbers(s, col, r0, &v);
                }
                for (r, c, v) in seg.vals {
                    wb.set_value(CellAddr::new(s, r, c), v);
                }
                fcells.extend(seg.fcells);
                iso_dates.extend(seg.iso_dates);
                shared.extend(seg.shared);
                widths.extend(seg.widths);
                heights.extend(seg.heights);
                hidden.extend(seg.hidden);
                summary_above |= seg.summary_above;
                // At most the native detail-row limit plus one summary per group.
                outline_too_large |= outline_rows.len() + seg.outline.len() > super::outline::MAX_ROW_RECORDS;
                if !outline_too_large { outline_rows.extend(seg.outline); }
                merges.extend(seg.merges);
                links.extend(seg.links);
                filter = seg.filter.or(filter);
                if let Some(protection) = seg.protection {
                    wb.sheet_mut(s).protection = Some(protection);
                }
                if seg.tab.is_some() {
                    wb.sheet_mut(s).tab_color = seg.tab;
                }
                if seg.no_grid {
                    wb.sheet_mut(s).show_grid = false;
                }
                if seg.no_headings {
                    wb.sheet_mut(s).show_headings = false;
                }
                if let Some(z) = seg.zoom {
                    wb.sheet_mut(s).zoom = z;
                }
                if let Some((f, from)) = seg.freeze {
                    wb.sheet_mut(s).freeze = f;
                    wb.sheet_mut(s).freeze_from = from;
                }
                cond.extend(seg.cond);
                valid.root_namespaces.extend(seg.valid.root_namespaces);
                if valid.base.is_empty() {
                    valid.base = seg.valid.base;
                }
                if valid.extension.is_none() {
                    valid.extension = seg.valid.extension;
                }
                dw = seg.default_w.or(dw);
                dh = seg.default_h.or(dh);
                out.cells += seg.cells;
                row_styles.extend(seg.row_styles);
                if sruns.len() < seg.sruns.len() {
                    sruns.resize_with(seg.sruns.len(), Vec::new);
                }
                for (c, runs) in seg.sruns.into_iter().enumerate() {
                    let dst = &mut sruns[c];
                    let mut it = runs.into_iter().peekable();
                    if let (Some(l), Some(f)) = (dst.last_mut(), it.peek()) {
                        if l.2 == f.2 && l.1 + 1 == f.0 {
                            l.1 = f.1;
                            it.next();
                        }
                    }
                    dst.extend(it);
                }
                col_styles.extend(seg.col_styles);
                rep.merge(seg.rep);
            }
        }
        cutter.join().map_err(|_| bad("the reader stopped"))??;
        if let Some(e) = failed {
            return Err(e);
        }
        // Links for now name their relationship (`\u{1}rId3#place`); resolved
        // once the sheet's relationships are read ([`resolve_links`]).
        if !links.is_empty() {
            let mut set = Vec::new();
            'all: for ((r0, c0, r1, c1), rid, loc) in links {
                let t = match (rid, loc) {
                    (Some(id), loc) => format!("\u{1}{id}{}", loc.map(|l| format!("#{l}")).unwrap_or_default()),
                    (None, Some(l)) => format!("#{l}"),
                    (None, None) => continue,
                };
                let Some(link) = officina_grid::links::Link::new(&t) else { continue };
                for r in r0..=r1 {
                    for c in c0..=c1 {
                        if set.len() >= officina_grid::links::MAX_LINKS {
                            break 'all;
                        }
                        set.push((r, c, Some(link.clone())));
                    }
                }
            }
            wb.set_links(s, set);
        }
        if !merges.is_empty() {
            let n = merges.len();
            let list = merges.into_iter().map(|(r0, c0, r1, c1)| officina_grid::addr::Rect { sheet: s, r0, c0, r1, c1 }).collect();
            wb.set_merges(s, list);
            let kept = wb.merges(s).len();
            if kept < n {
                rep.note_n(Kind::Dropped, "merged cells overlapping others or of one cell", (n - kept) as u64);
            }
        }
        if outline_too_large || !outline_rows.is_empty() {
            use officina_core::history::Op;
            use officina_grid::{ops::GridOp, shift::Axis};
            if let Some(outline) = (!outline_too_large).then(|| super::outline::decode(&outline_rows, !summary_above)).flatten() {
                GridOp::Outline { sheet: s, axis: Axis::Rows, outline }.apply(wb);
            } else {
                rep.note(Kind::Dropped, "row outline beyond the supported group limits");
            }
        }
        let sh = wb.sheet_mut(s);
        if let Some(w) = dw {
            sh.col_sizes = Sizes::new((w * MDW).round().max(2.0));
        }
        if let Some(h) = dh {
            sh.row_sizes = Sizes::new((h * 96.0 / 72.0).round().max(2.0));
        }
        if !widths.is_empty() {
            let (hw, widths) = remembered(widths);
            sh.hidden_widths = hw;
            sh.col_sizes = Sizes::from_overrides(sh.col_sizes.default, widths);
        }
        if !heights.is_empty() {
            sh.row_sizes = Sizes::from_overrides(sh.row_sizes.default, heights);
        }
        // Hidden rows as the file hid them; the filter kept with its conditions (the
        // file's rows are what Excel last computed, so they are not recomputed).
        sh.hidden = officina_grid::filter::merge(hidden.into_iter().map(|r| (r, r)).collect());
        if let Some((rf, mut conds)) = filter {
            if let Some((r0, c0, r1, c1)) = parse_ref(&rf) {
                conds.retain_mut(|(c, _)| {
                    if *c > c1 - c0 {
                        return false;
                    }
                    *c += c0;
                    true
                });
                sh.filter = Some(AutoFilter { rect: officina_grid::addr::Rect { sheet: s, r0, c0, r1, c1 }, conds });
            }
        }
        // Styles: a column's own style under its cells' runs.
        apply_styles(wb, s, col_styles, row_styles, MAX_COLS, &sruns, xf_ids, rep);
        for (row, col, format) in iso_dates {
            let address = CellAddr::new(s, row, col);
            if wb.styles.format(wb.style_id(address)).is_general() {
                let style = wb.style(address).clone().with_num_fmt(format);
                wb.set_style(&officina_grid::addr::Rect::cell(address), style);
            }
        }
        Ok(())
    })
}

/// Just past the last `</row>` in `b`.
fn memrchr_row_end(b: &[u8]) -> Option<usize> {
    const END: &[u8] = b"</row>";
    let mut i = b.len();
    while i >= END.len() {
        match b[..i].iter().rposition(|&c| c == b'>') {
            Some(p) if p + 1 >= END.len() => {
                if &b[p + 1 - END.len()..=p] == END {
                    return Some(p + 1);
                }
                i = p;
            }
            _ => return None,
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn formula_src(ft: Option<&str>, si: Option<u32>, area: Option<(u32, u32, u32, u32)>, text: String, row: u32, col: u32, shared: &mut HashMap<u32, (u32, u32, String)>, rep: &mut Report) -> Option<Src> {
    match ft.unwrap_or("normal") {
        "normal" => (!text.trim().is_empty()).then_some(Src::Text(text)),
        "shared" => {
            let si = si?;
            if !text.trim().is_empty() {
                shared.insert(si, (row, col, text));
            }
            Some(Src::Shared(si))
        }
        "array" => {
            let _ = (row, col);
            (!text.trim().is_empty()).then_some(Src::Array(text, area))
        }
        _ => {
            rep.note(Kind::Approximate, "data tables kept as their values");
            None
        }
    }
}

fn read_text(r: &mut Reader<&[u8]>, end: &[u8], out: &mut String) -> Result<(), XlsxError> {
    let mut buf = Vec::new();
    let mut cut = false;
    loop {
        buf.clear();
        match r.read_event_into(&mut buf)? {
            Event::Text(t) => {
                let s = t.unescape().map_err(|e| XlsxError::Xml(e.to_string()))?;
                push_capped(out, &s, &mut cut);
            }
            Event::CData(t) => push_capped(out, &String::from_utf8_lossy(&t), &mut cut),
            Event::End(e) if e.local_name().as_ref() == end => return Ok(()),
            Event::Start(e) => skip(r, &e)?,
            Event::Eof => return Err(bad("text ended early")),
            _ => {}
        }
    }
}

type FilterColumns = (String, Vec<(u32, Cond)>);

fn read_auto_filter(
    r: &mut Reader<&[u8]>, tag: &BytesStart, empty: bool, system: DateSystem,
    dxfs: &[super::condfmt::DxfIn], rep: &mut Report,
) -> Result<Option<FilterColumns>, XlsxError> {
    let range = attr(tag, b"ref")?;
    let mut columns = Vec::new();
    let mut valid = true;
    let mut seen = std::collections::HashSet::new();
    if !empty {
        loop {
            let (child, empty) = match r.read_event()? {
                Event::Start(child) => (child, false),
                Event::Empty(child) => (child, true),
                Event::End(end) if end.name() == tag.name() => break,
                Event::Eof => return Err(bad("auto-filter ended early")),
                _ => continue,
            };
            match child.local_name().as_ref() {
                b"filterColumn" => {
                    if attr(&child, b"hiddenButton")?.is_some_and(|v| matches!(v.as_str(), "1" | "true")) {
                        rep.note(Kind::Approximate, "hidden filter buttons shown");
                    }
                    let col = attr(&child, b"colId")?.and_then(|v| v.parse::<u32>().ok());
                    let cond = if empty { Ok(None) } else { read_filter_column(r, &child, system, dxfs)? };
                    match (col, cond) {
                        (Some(c), Ok(cond)) if c < MAX_COLS && seen.insert(c) => {
                            if let Some(cond) = cond { columns.push((c, cond)); }
                        }
                        (_, Err(reason)) => {
                            valid = false;
                            rep.note(Kind::Approximate, reason);
                        }
                        _ => {
                            valid = false;
                            rep.note(Kind::Approximate, "duplicate or invalid filter column (rows stay as the file hid them)");
                        }
                    }
                }
                b"sortState" => {
                    rep.note(Kind::Approximate, "saved filter sort settings (rows stay as saved)");
                    if !empty { skip(r, &child)?; }
                }
                _ if !empty => skip(r, &child)?,
                _ => {}
            }
        }
    }
    Ok(range.filter(|_| valid).map(|range| (range, columns)))
}

fn read_filter_column(
    r: &mut Reader<&[u8]>, tag: &BytesStart, system: DateSystem, dxfs: &[super::condfmt::DxfIn],
) -> Result<Result<Option<Cond>, &'static str>, XlsxError> {
    let mut selected = Ok(None);
    let mut chose_icon = false;
    loop {
        let (e, empty) = match r.read_event()? {
            Event::Start(e) => (e, false),
            Event::Empty(e) => (e, true),
            Event::End(e) if e.name() == tag.name() => break,
            Event::Eof => return Err(bad("filter column ended early")),
            _ => continue,
        };
        let cond = match e.local_name().as_ref() {
            b"Fallback" if chose_icon && !empty => {
                skip(r, &e)?;
                continue;
            }
            b"filters" => read_value_filter(r, &e, empty)?.map(Some)
                .ok_or("unsupported calendar or value filters (rows stay as the file hid them)"),
            b"customFilters" => read_custom_filter(r, &e, empty)?.map(Some)
                .ok_or("invalid custom filters (rows stay as the file hid them)"),
            b"dynamicFilter" => {
                match attr(&e, b"type")?.as_deref() {
                    Some("null") => Ok(None),
                    Some("aboveAverage") => Ok(Some(Cond::Average { below: false })),
                    Some("belowAverage") => Ok(Some(Cond::Average { below: true })),
                    name => read_dynamic_date(&e, name.unwrap_or_default(), system)?.map(|rule| Some(Cond::DynamicDate(rule)))
                        .ok_or("unsupported dynamic filter (rows stay as the file hid them)"),
                }
            }
            b"colorFilter" | b"iconFilter" => {
                chose_icon = e.local_name().as_ref() == b"iconFilter" && e.name().prefix().is_some();
                appearance_filter(&e, dxfs)?.map(|rule| rule.map(Cond::Appearance))
                    .ok_or("unsupported color or icon filter (rows stay as the file hid them)")
            }
            b"top10" => {
                let percent = attr(&e, b"percent")?.is_some_and(|v| matches!(v.as_str(), "1" | "true"));
                let bottom = attr(&e, b"top")?.is_some_and(|v| matches!(v.as_str(), "0" | "false"));
                attr(&e, b"val")?.and_then(|v| Rank::parse(&v, percent, bottom)).map(|rule| Some(Cond::Rank(rule)))
                    .ok_or("invalid top/bottom filter (rows stay as the file hid them)")
            }
            b"extLst" if !empty => {
                skip(r, &e)?;
                continue;
            }
            _ => continue,
        };
        match (&selected, cond) {
            (Ok(None), c) => selected = c,
            (Ok(Some(_)), Ok(Some(_))) => selected = Err("multiple rules in one filter column (rows stay as the file hid them)"),
            (Ok(_), Err(reason)) => selected = Err(reason),
            _ => {}
        }
    }
    Ok(selected)
}

fn appearance_filter(tag: &BytesStart, dxfs: &[super::condfmt::DxfIn]) -> Result<Option<Option<AppearanceFilter>>, XlsxError> {
    if tag.local_name().as_ref() == b"colorFilter" {
        let cell = match attr(tag, b"cellColor")?.as_deref() {
            None | Some("1" | "true") => true,
            Some("0" | "false") => false,
            _ => return Ok(None),
        };
        let color = attr(tag, b"dxfId")?.and_then(|i| i.parse::<usize>().ok())
            .and_then(|i| dxfs.get(i)).and_then(|dxf| dxf.filter_color(cell));
        return Ok(color.map(Some));
    }
    let name = attr(tag, b"iconSet")?;
    let index = attr(tag, b"iconId")?;
    if name.as_deref() == Some("NoIcons") && index.as_deref() == Some("0") {
        return Ok(Some(None));
    }
    let index = match index {
        None => None,
        Some(i) => match i.parse::<u8>() {
            Ok(i) => Some(i),
            Err(_) => return Ok(None),
        },
    };
    let rule = name.as_deref().and_then(officina_grid::condfmt::icon_set)
        .and_then(|set| IconFilter::new(set, index));
    Ok(rule.map(|rule| Some(AppearanceFilter::Icon(rule))))
}

fn date_group(tag: &BytesStart) -> Result<Option<DateGroup>, XlsxError> {
    let kind = attr(tag, b"dateTimeGrouping")?;
    let Some(precision) = GROUP_FIELDS.iter().position(|&field| Some(field) == kind.as_deref()) else {
        return Ok(None);
    };
    let mut fields = [0; 6];
    for (index, name) in GROUP_FIELDS[..=precision].iter().enumerate() {
        let Some(value) = attr(tag, name.as_bytes())?.and_then(|v| v.parse().ok()) else {
            return Ok(None);
        };
        fields[index] = value;
    }
    Ok(DateGroup::new(&fields[..=precision]))
}

fn read_value_filter(r: &mut Reader<&[u8]>, tag: &BytesStart, empty: bool) -> Result<Option<Cond>, XlsxError> {
    let calendar = attr(tag, b"calendarType")?;
    let blank = attr(tag, b"blank")?.is_some_and(|v| v == "1" || v == "true");
    let mut values = if blank { vec![String::new()] } else { Vec::new() };
    let mut groups = Vec::new();
    let mut valid = true;
    if !empty {
        loop {
            let (child, empty) = match r.read_event()? {
                Event::Start(child) => (child, false),
                Event::Empty(child) => (child, true),
                Event::End(end) if end.name() == tag.name() => break,
                Event::Eof => return Err(bad("value filters ended early")),
                _ => continue,
            };
            if valid {
                match child.local_name().as_ref() {
                    b"filter" if values.len() < MAX_SELECTIONS => {
                        match attr(&child, b"val")? {
                            Some(value) => values.push(value),
                            None => valid = false,
                        }
                    }
                    b"dateGroupItem" if groups.len() < MAX_SELECTIONS => {
                        match date_group(&child)? {
                            Some(group) => groups.push(group),
                            None => valid = false,
                        }
                    }
                    _ => valid = false,
                }
            }
            if !empty {
                skip(r, &child)?;
            }
        }
    }
    if !valid || (!groups.is_empty() && calendar.as_deref().is_some_and(|name| !valid_calendar_type(name))) {
        return Ok(None);
    }
    Ok(Some(if groups.is_empty() {
        Cond::Values(values)
    } else {
        Cond::Calendar(CalendarSelection { groups, values, calendar })
    }))
}

fn read_dynamic_date(tag: &BytesStart, name: &str, system: DateSystem) -> Result<Option<DynamicDate>, XlsxError> {
    let bound = |name, iso| -> Result<Option<f64>, XlsxError> {
        if let Some(x) = attr(tag, name)?.and_then(|v| v.parse::<f64>().ok()).filter(|x| x.is_finite()) {
            return Ok(Some(x));
        }
        Ok(attr(tag, iso)?.and_then(|v| system.iso(&v)).map(|(serial, _)| serial))
    };
    let cached = bound(b"val", b"valIso")?.zip(bound(b"maxVal", b"maxValIso")?)
        .and_then(|(start, end)| DateBounds::new(start, end));
    Ok(DynamicDate::parse(name, cached))
}

fn custom_test(tag: &BytesStart) -> Result<Option<Test>, XlsxError> {
    let op = match attr(tag, b"operator")?.as_deref() {
        None | Some("equal") => "=",
        Some("notEqual") => "<>",
        Some("lessThan") => "<",
        Some("lessThanOrEqual") => "<=",
        Some("greaterThan") => ">",
        Some("greaterThanOrEqual") => ">=",
        Some(_) => return Ok(None),
    };
    let Some(value) = attr(tag, b"val")? else { return Ok(None) };
    if let Some(x) = value.parse::<f64>().ok().filter(|x| x.is_finite()) {
        return Ok(Some(Test::Number(op.to_string(), x)));
    }
    let op = match op {
        "=" => TextOp::Wildcard,
        "<>" => TextOp::NotWildcard,
        _ => TextOp::comparison(op).unwrap(),
    };
    let text = TextTest { op, value, case_sensitive: false };
    Ok(text.valid().then_some(Test::Text(text)))
}

fn read_custom_filter(r: &mut Reader<&[u8]>, tag: &BytesStart, empty: bool) -> Result<Option<Cond>, XlsxError> {
    if empty {
        return Ok(None);
    }
    let and = attr(tag, b"and")?.is_some_and(|v| v == "1" || v == "true");
    let mut tests = Vec::new();
    let mut valid = true;
    loop {
        let (child, empty) = match r.read_event()? {
            Event::Start(child) => (child, false),
            Event::Empty(child) => (child, true),
            Event::End(end) if end.name() == tag.name() => break,
            Event::Eof => return Err(bad("custom filters ended early")),
            _ => continue,
        };
        if valid && tests.len() < 2 && child.local_name().as_ref() == b"customFilter" {
            match custom_test(&child)? {
                Some(test) => tests.push(test),
                None => valid = false,
            }
        } else {
            valid = false;
        }
        if !empty {
            skip(r, &child)?;
        }
    }
    Ok((valid && !tests.is_empty()).then(|| Cond::custom(tests, and)))
}

fn skip(r: &mut Reader<&[u8]>, e: &BytesStart) -> Result<(), XlsxError> {
    let mut buf = Vec::new();
    r.read_to_end_into(e.name(), &mut buf)?;
    Ok(())
}

/// Reads an `.xlsx` (or `.xlsm`) package into a workbook.
pub fn read(data: &[u8]) -> Result<(Workbook, Loaded), XlsxError> {
    let z = Archive::open(data)?;
    let mut out = Loaded::default();
    let root = rels(&z, "")?;
    let wb_part = root
        .values()
        .find(|(t, _)| t.ends_with("/officeDocument"))
        .map(|(_, p)| p.clone())
        .unwrap_or_else(|| "xl/workbook.xml".into());
    let wb_xml = z.read_name(&wb_part)?.ok_or_else(|| bad("no workbook part"))?;
    let wb_rels = rels(&z, &wb_part)?;
    if wb_rels.values().any(|(t, _)| t.ends_with("/externalLink")) {
        out.report.note(Kind::Inert, "links to other workbooks (cached values kept, never refreshed)");
    }

    // Sheets in order.
    let mut sheets: Vec<(String, String)> = Vec::new();
    let mut hidden_sheets: Vec<bool> = Vec::new();
    let mut pivot_caches: HashMap<u32, Option<String>> = HashMap::new();
    let mut external_ids = Vec::new();
    // Defined names: (name, sheet index it is local to, formula text).
    let mut names: Vec<(String, Option<u32>, String)> = Vec::new();
    let mut iteration: Option<officina_grid::book::Iteration> = None;
    {
        let mut r = xml(&wb_xml);
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match r.read_event_into(&mut buf)? {
                Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                    b"sheet" => {
                        let name = attr(&e, b"name")?.unwrap_or_default();
                        let Some(id) = attr(&e, b"id")? else { continue };
                        // Hidden and very hidden sheets both stay hidden; Unhide shows either.
                        let hide = attr(&e, b"state")?.is_some_and(|v| v == "hidden" || v == "veryHidden");
                        match wb_rels.get(&id) {
                            Some((t, p)) if t.ends_with("/worksheet") => {
                                sheets.push((name, p.clone()));
                                hidden_sheets.push(hide);
                            }
                            Some(_) => out.report.note_at(Kind::Dropped, "chart sheets and dialog sheets", || name.clone()),
                            None => return Err(bad("a sheet without its part")),
                        }
                    }
                    b"externalReference" if external_ids.len() < super::external::MAX_LINKS => {
                        external_ids.push(attr(&e, b"id")?);
                    }
                    b"pivotCache" => {
                        if let (Some(cache), Some(rel)) = (attr(&e, b"cacheId")?.and_then(|id| id.parse::<u32>().ok()), attr(&e, b"id")?) {
                            pivot_caches.entry(cache).and_modify(|known| {
                                if known.as_deref() != Some(rel.as_str()) {
                                    *known = None;
                                }
                            }).or_insert(Some(rel));
                        }
                    }
                    b"calcPr" if attr(&e, b"iterate")?.is_some_and(|v| v == "1" || v == "true") => {
                        // ECMA-376's defaults: 100 times, 0.001.
                        let count = attr(&e, b"iterateCount")?.and_then(|v| v.parse::<u32>().ok()).unwrap_or(100);
                        let change = attr(&e, b"iterateDelta")?.and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.001);
                        iteration = Some(officina_grid::book::Iteration { count, change });
                    }
                    b"workbookPr" => out.date1904 = attr(&e, b"date1904")?.is_some_and(|v| v == "1" || v == "true"),
                    b"definedName" => {
                        let name = attr(&e, b"name")?.unwrap_or_default();
                        let local = attr(&e, b"localSheetId")?.and_then(|v| v.parse::<u32>().ok());
                        let mut text = String::new();
                        let mut tb = Vec::new();
                        if let Event::Text(t) = r.read_event_into(&mut tb)? {
                            text = t.unescape().map(|x| x.into_owned()).unwrap_or_default();
                        }
                        let cut = text.len() > 8192;
                        if names.len() < officina_grid::names::MAX_NAMES && !cut {
                            names.push((name, local, text));
                        }
                    }
                    _ => {}
                },
                Event::Eof => break,
                _ => {}
            }
        }
    }
    if sheets.is_empty() {
        return Err(bad("no worksheets"));
    }
    let sst_path = wb_rels.values().find(|(t, _)| t.ends_with("/sharedStrings")).map(|(_, p)| p.as_str());
    let sst = shared_strings(&z, sst_path, &mut out.report)?;

    use officina_grid::eval::dates::DateSystem;
    let system = if out.date1904 { DateSystem::Excel1904 } else { DateSystem::Excel1900 };
    let mut wb = Workbook::with_date_system(system);
    if let Some(it) = iteration {
        if !wb.set_iteration(Some(it)) {
            out.report.note(Kind::Dropped, "iteration settings");
        }
    }
    for (name, _) in &sheets {
        // Names Excel allows and we don't get a safe stand-in.
        let mut n: String = name.chars().filter(|c| !"[]:*?/\\".contains(*c)).collect();
        if n.is_empty() || wb.sheet_id(&n).is_some() {
            n = format!("Sheet{}", wb.sheet_count() + 1);
        }
        let id = wb.add_sheet(&n).map_err(bad)?;
        if hidden_sheets.get(id as usize) == Some(&true) {
            wb.sheet_mut(id).sheet_hidden = true;
        }
    }
    // A workbook needs a sheet that shows: a file hiding them all shows the first.
    if wb.shown_sheets() == 0 && wb.sheet_count() > 0 {
        wb.sheet_mut(0).sheet_hidden = false;
        out.report.note(Kind::Approximate, "every sheet hidden; the first is shown");
    }
    let mut rep = Report::default();
    // Styles, before the cells that name them.
    let theme = wb_rels.values().find(|(t, _)| t.ends_with("/theme")).map(|(_, p)| p.clone());
    let theme = match theme {
        Some(p) => super::styles::theme_colors(&z.read_name(&p)?.unwrap_or_default()),
        None => super::styles::theme_colors(b""),
    };
    let mut dxfs = Vec::new();
    let mut indexed = officina_grid::numfmt::PALETTE;
    let xf_ids: Vec<StyleId> = match wb_rels.values().find(|(t, _)| t.ends_with("/styles")).map(|(_, p)| p.clone()) {
        Some(p) => match z.read_name(&p)? {
            Some(x) => {
                let (styles, palette) = super::styles::read_with_palette(&x, &theme, &mut rep);
                indexed = palette;
                dxfs = super::condfmt::read_dxfs(&x, &super::styles::Colors { theme: &theme, indexed: &indexed });
                styles.into_iter().map(|st| wb.styles.intern(st)).collect()
            }
            None => vec![0],
        },
        None => vec![0],
    };
    let external_cache = if external_ids.is_empty() {
        super::external::Cache::default()
    } else {
        let internal = internal_rels(&z, &wb_part)?;
        let paths = external_ids.iter().enumerate().filter_map(|(index, id)| {
            let (kind, part) = internal.get(id.as_ref()?)?;
            kind.ends_with("/externalLink").then(|| (index + 1, part.clone()))
        });
        super::external::Cache::read(&z, paths)
    };
    // Names, before the formulas that use them.
    for (name, local, text) in &names {
        let scope = match local {
            Some(i) if (*i as usize) < wb.sheet_count() => Some(*i as u16),
            Some(_) => continue,
            None => None,
        };
        if name.eq_ignore_ascii_case(officina_grid::print::TITLES) {
            if !scope.is_some_and(|sheet| define_print_titles(&mut wb, sheet, text)) {
                rep.note(Kind::Dropped, "unsupported print titles");
            }
            continue;
        }
        if super::external::has_reference(text) {
            let cached = external_cache.cell(text, true);
            let value = match cached.as_ref() {
                Some(Value::Num(x)) => format!("={x}"),
                Some(Value::Bool(true)) => "=TRUE".into(),
                Some(Value::Bool(false)) => "=FALSE".into(),
                Some(Value::Str(text)) => format!("=\"{}\"", text.replace('"', "\"\"")),
                Some(Value::Err(error)) => format!("={}", error.text()),
                _ => "=#REF!".into(),
            };
            if wb.define_name(name, scope, &value).is_ok() && cached.is_some() {
                rep.note_at(Kind::Inert, "linked workbook names kept as cached constants", || name.clone());
            } else {
                // Keep scope shadowing and give dependents a reference error.
                let _ = wb.define_name(name, scope, "=#REF!");
                rep.note_at(Kind::Approximate, "linked workbook names without a usable cached cell", || name.clone());
            }
            continue;
        }
        if wb.define_name(name, scope, &formula_in(text)).is_err() {
            rep.note_at(Kind::Dropped, "defined names this engine can't use", || format!("{name}: {}", text.chars().take(60).collect::<String>()));
        }
    }
    let mut all_f: Vec<(u16, Vec<FCell>, HashMap<u32, (u32, u32, String)>)> = Vec::new();
    let mut conds: Vec<(u16, Vec<super::condfmt::Raw>)> = Vec::new();
    let mut valids = Vec::new();
    for (i, (_, part)) in sheets.iter().enumerate() {
        let e = z.find(part).ok_or_else(|| bad(format!("missing {part}")))?.clone();
        let mut fcells = Vec::new();
        let mut shared = HashMap::new();
        let mut cond = Vec::new();
        let mut valid = super::validation::Raw::default();
        sheet(&mut wb, i as u16, &z, &e, &sst, &theme, &xf_ids, &dxfs, &mut out, &mut rep, &mut fcells, &mut shared, &mut cond, &mut valid)?;
        all_f.push((i as u16, fcells, shared));
        conds.push((i as u16, cond));
        valids.push((i as u16, valid));
    }

    // Tables, before the formulas that use them.
    for (i, (_, part)) in sheets.iter().enumerate() {
        let mut parts: Vec<String> = rels(&z, part)?.into_values().filter(|(t, _)| t.ends_with("/table")).map(|(_, p)| p).collect();
        parts.sort();
        for p in parts {
            if let Some(data) = z.read_name(&p)? {
                table(&mut wb, i as u16, &data, &dxfs, &mut rep)?;
            }
        }
    }

    // Charts and comments. The threaded author directory belongs to the workbook.
    let person_path = wb_rels.values().find(|(t, _)| t == super::threaded::PERSON_REL).map(|(_, path)| path.as_str());
    let mut people = super::threaded::persons(&z, person_path, &mut rep)?;
    for (i, (_, part)) in sheets.iter().enumerate() {
        super::chart::read_sheet(&mut wb, i as u16, &z, part, &mut rep)?;
        super::comments::read_sheet_with_people(&mut wb, i as u16, &z, part, &mut people, &mut rep)?;
        resolve_links(&mut wb, i as u16, &z, part)?;
    }

    // Formulas, once every sheet exists (they name each other).
    let mut cached: Vec<(CellAddr, Value)> = Vec::new();
    for (s, mut fcells, shared) in all_f {
        preserve_external_results(&mut wb, s, &mut fcells, &shared, &external_cache, &mut rep);
        place_formulas(&mut wb, s, fcells, &shared, &mut rep, &mut out, &mut cached);
    }
    // Pivot tables, over the cells they wrote.
    let parts: Vec<String> = sheets.iter().map(|(_, p)| p.clone()).collect();
    super::pivot::read_all(&mut wb, &z, &parts, &wb_part, &pivot_caches, &mut rep)?;
    // Conditional formats, once formulas (and the names they use) can be read.
    for (s, raws) in &conds {
        super::condfmt::read_sheet(&mut wb, *s, raws, &dxfs, &super::styles::Colors { theme: &theme, indexed: &indexed }, &mut rep);
    }
    for (s, raw) in &valids {
        super::validation::read_sheet(&mut wb, *s, raw, &mut rep);
    }
    wb.recalc();
    for sheet in 0..wb.sheet_count() as u16 {
        wb.restore_table_visibility(sheet, None);
        wb.restore_outline_visibility(sheet);
    }
    if wb.recalc_pending() {
        wb.recalc();
    }
    // The differential: our results against the file's.
    differential(&wb, cached, &mut out, &mut rep);
    // Macros, kept inert.
    let parts: Vec<String> = sheets.iter().map(|(_, p)| p.clone()).collect();
    super::macros::read(&z, &mut wb, &wb_xml, &wb_rels, &parts, &mut rep)?;
    out.report.merge(rep);
    Ok((wb, out))
}

/// One `xl/tables/tableN.xml`.
fn table(wb: &mut Workbook, sheet: u16, data: &[u8], dxfs: &[super::condfmt::DxfIn], rep: &mut Report) -> Result<(), XlsxError> {
    let mut r = xml(data);
    let mut buf = Vec::new();
    let mut t: Option<Table> = None;
    let mut filter = None;
    let system = wb.date_system();
    loop {
        let (e, empty) = match r.read_event_into(&mut buf)? {
            Event::Start(e) => (e, false),
            Event::Empty(e) => (e, true),
            Event::Eof => break,
            _ => { buf.clear(); continue; }
        };
        match e.local_name().as_ref() {
                // Excel puts x14:table accessibility metadata inside extLst.
                b"extLst" if !empty => skip(&mut r, &e)?,
                b"table" => {
                    let name = attr(&e, b"displayName")?.or(attr(&e, b"name")?).unwrap_or_default();
                    let Some((r0, c0, r1, c1)) = attr(&e, b"ref")?.as_deref().and_then(parse_ref) else {
                        rep.note(Kind::Dropped, "tables without a range");
                        return Ok(());
                    };
                    let header = attr(&e, b"headerRowCount")?.map_or(true, |v| v != "0");
                    let totals = attr(&e, b"totalsRowCount")?.is_some_and(|v| v != "0");
                    t = Some(Table { name: name.into(), rect: Rect { sheet, r0, c0, r1, c1 }, header, totals, columns: Vec::new(), style: None, stripes: false, filter: None });
                }
                b"tableColumn" => {
                    if let (Some(t), Some(n)) = (t.as_mut(), attr(&e, b"name")?) {
                        if t.columns.len() < MAX_COLS as usize {
                            t.columns.push(decode_escapes(&n).as_ref().into());
                        }
                    }
                }
                b"autoFilter" => filter = read_auto_filter(&mut r, &e, empty, system, dxfs, rep)?,
                b"sortState" => rep.note(Kind::Approximate, "a table's saved sort order (rows stay as saved)"),
                b"tableStyleInfo" => {
                    if let Some(t) = t.as_mut() {
                        t.style = attr(&e, b"name")?.filter(|n| !n.is_empty() && n.len() <= 255).map(Into::into);
                        t.stripes = attr(&e, b"showRowStripes")?.is_some_and(|v| v == "1" || v == "true");
                    }
                }
                _ => {}

        }
        buf.clear();
    }
    let Some(mut t) = t else { return Ok(()) };
    if let Some((range, conds)) = filter {
        if t.header && t.filter_rect().is_some_and(|r| {
            parse_ref(&range) == Some((r.r0, r.c0, r.r1, r.c1))
                && conds.iter().all(|(c, _)| *c <= r.c1 - r.c0)
        }) {
            t.filter = Some(officina_grid::tables::TableFilter { conds, hidden: Vec::new() });
        } else {
            rep.note(Kind::Approximate, "unsupported table filter range (rows stay as the file hid them)");
        }
    }
    if t.style.is_some() {
        rep.note(Kind::Approximate, "table styles kept but not drawn");
    }
    let name = t.name.clone();
    if let Err(why) = wb.add_table(t) {
        rep.note_at(Kind::Dropped, "tables this engine can't use (kept as plain cells)", || format!("{name}: {why}"));
    }
    Ok(())
}

fn wb_parse(wb: &Workbook, src: &str, s: u16, row: u32, col: u32) -> Option<Expr> {
    wb.parse(src, s, row, col).ok()
}

/// A sheet's cell styles: per column, the column's own style (`col_styles`:
/// first column, last column, xf) under its cells' runs (`sruns`: per column,
/// first row, last row, xf), xfs mapped through `xf_ids`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_styles(wb: &mut Workbook, s: u16, col_styles: Vec<(u32, u32, u32)>, row_styles: Vec<(u32, u32)>, format_cols: u32, sruns: &[Vec<(u32, u32, u32)>], xf_ids: &[StyleId], rep: &mut Report) {
    // Sparse rows otherwise expand to tens of thousands of column runs per
    // source record. Keep a visible prefix and the used extent when it fits.
    const ROW_STYLE_CELLS: usize = 262_144;
    let extent = sruns.len().max(col_styles.iter().map(|p| p.1 as usize + 1).max().unwrap_or(0));
    let row_width = if row_styles.is_empty() { 0 } else {
        let width = extent.max(256).min(format_cols as usize).min(ROW_STYLE_CELLS / row_styles.len());
        if width < format_cols as usize {
            rep.note(Kind::Approximate, "row formats on distant blank cells were limited");
        }
        width
    };
    let mut id = |xf: u32| -> StyleId {
        match xf_ids.get(xf as usize) {
            Some(&i) => i,
            None => {
                rep.note(Kind::Approximate, "cells naming a style the file does not have, shown unstyled");
                0
            }
        }
    };
    let mut base: Vec<StyleId> = Vec::new();
    for (c0, c1, xf) in col_styles {
        let i = id(xf);
        if i != 0 && base.len() <= c1 as usize {
            base.resize(c1 as usize + 1, 0);
        }
        if i != 0 {
            for b in &mut base[c0 as usize..=c1 as usize] {
                *b = i;
            }
        }
    }
    let mut row_styles: Vec<_> = row_styles.into_iter().map(|(row, xf)| (row, id(xf))).collect();
    row_styles.sort_by_key(|&(row, _)| row);
    row_styles.dedup_by(|later, earlier| {
        if later.0 == earlier.0 {
            earlier.1 = later.1;
            true
        } else {
            false
        }
    });
    let ncols = sruns.len().max(base.len()).max(row_width);
    let mut cols: Vec<StyleRuns> = Vec::with_capacity(ncols);
    for c in 0..ncols {
        let b = base.get(c).copied().unwrap_or(0);
        let mut v: Vec<(u32, StyleId)> = Vec::new();
        let mut at = 0u32;
        if b != 0 {
            v.push((0, b));
        }
        let explicit = sruns.get(c).map(Vec::as_slice).unwrap_or(&[]);
        for &(r0, r1, xf) in explicit {
            if r0 > at && at != 0 {
                v.push((at, b));
            }
            v.push((r0, id(xf)));
            at = r1 + 1;
        }
        if (at as u64) < MAX_ROWS as u64 && at > 0 {
            v.push((at, b));
        }
        if c < row_width && !row_styles.is_empty() {
            let mut overlay = Vec::new();
            for &(row, style) in &row_styles {
                let k = explicit.partition_point(|r| r.0 <= row);
                if k > 0 && explicit[k - 1].1 >= row {
                    continue;
                }
                overlay.push((row, style));
                if row + 1 < MAX_ROWS {
                    let next = v.partition_point(|r| r.0 <= row + 1);
                    overlay.push((row + 1, next.checked_sub(1).map_or(0, |i| v[i].1)));
                }
            }
            v.extend(overlay);
            v.sort_by_key(|r| r.0);
        }
        v.dedup_by(|later, earlier| {
            if later.0 == earlier.0 {
                earlier.1 = later.1;
                true
            } else {
                false
            }
        });
        cols.push(StyleRuns::from_runs(v));
    }
    while cols.last().is_some_and(|c| c.is_empty()) {
        cols.pop();
    }
    wb.sheet_mut(s).styles = cols;
}

/// A file can have a cached result from a function provided by another app or
/// add-in. A direct unavailable call returns #NAME?, so keep its saved value.
pub(crate) fn missing_top_level_function(wb: &Workbook, sheet: u16, mut expr: &Expr) -> bool {
    use officina_grid::formula::Func;
    while let Expr::At(inner) | Expr::Paren(inner) = expr {
        expr = inner;
    }
    matches!(expr, Expr::Call(Func::Unknown(name), _) if wb.names().get(sheet, name).is_none())
}

pub(crate) fn define_print_titles(wb: &mut Workbook, sheet: u16, text: &str) -> bool {
    let refs = crate::ods::formula::split_outside_quotes(text, ',');
    if refs.len() > 2 || refs.len() + wb.names().len() > officina_grid::names::MAX_NAMES {
        return false;
    }
    let mut axes = Vec::new();
    for part in refs {
        let src = formula_in(part.trim());
        let Some((axis, _)) = wb.parse(&src, sheet, 0, 0).ok()
            .and_then(|expr| officina_grid::print::title(&expr, sheet)) else { return false };
        if axes.iter().any(|(seen, _)| *seen == axis) {
            return false;
        }
        axes.push((axis, src));
    }
    axes.into_iter().all(|(axis, src)| wb.define_name(axis, Some(sheet), &src).is_ok())
}

/// A link is inert in Sheets. Keep Excel's consuming formula result before
/// recalculation can replace it with #REF!, including shared or array anchors.
fn preserve_external_results(
    wb: &mut Workbook,
    sheet: u16,
    cells: &mut Vec<FCell>,
    shared: &HashMap<u32, (u32, u32, String)>,
    cache: &super::external::Cache,
    report: &mut Report,
) {
    let external_shared: HashSet<_> = shared.iter()
        .filter(|(_, (_, _, text))| super::external::has_reference(text))
        .map(|(id, _)| *id)
        .collect();
    cells.retain(|cell| {
        let (text, absolute) = match &cell.src {
            Src::Text(text) | Src::Array(text, _) if super::external::has_reference(text) => (text, false),
            Src::Shared(id) if external_shared.contains(id) => {
                let (row, col, text) = &shared[id];
                (text, (*row, *col) != (cell.row, cell.col))
            }
            _ => return true,
        };
        if cell.cached.is_empty() {
            let address = CellAddr::new(sheet, cell.row, cell.col);
            if let Some(value) = cache.cell(text, absolute) {
                wb.set_value(address, value);
            } else {
                wb.set_value(address, Value::Err(ErrorKind::Ref));
                report.note_at(Kind::Approximate, "linked workbook formulas without a usable cached result", || a1(cell.row, cell.col));
                return false;
            }
        }
        report.note_at(Kind::Inert, "linked workbook formulas kept as cached values", || a1(cell.row, cell.col));
        false
    });
}

/// Places one sheet's formula cells: each parsed at its cell (a shared formula
/// once, at its master cell), runs of one expression down a column as one fill.
/// Cells whose formula parses go into `cached` with the file's value.
pub(crate) fn place_formulas(wb: &mut Workbook, s: u16, fcells: Vec<FCell>, shared: &HashMap<u32, (u32, u32, String)>, rep: &mut Report, out: &mut Loaded, cached: &mut Vec<(CellAddr, Value)>) {
    // A shared formula's expression, parsed once at its master cell; relative, so
    // the same for every cell of the fill.
    let mut exprs: HashMap<u32, Option<Arc<Expr>>> = HashMap::new();
    let mut cells: Vec<(u32, u32, Arc<Expr>)> = Vec::with_capacity(fcells.len());
    for fc in fcells {
        // Formulas from before dynamic arrays keep their one value (`@`); array
        // formulas spill, over the area the file kept for them.
        let e = match &fc.src {
            Src::Text(t) => wb_parse(&wb, &formula_in(t), s, fc.row, fc.col).map(|e| Arc::new(arrays::legacy(e))),
            Src::Shared(si) => exprs
                .entry(*si)
                .or_insert_with(|| shared.get(si).and_then(|(r, c, t)| wb_parse(&wb, &formula_in(t), s, *r, *c)).map(|e| Arc::new(arrays::legacy(e))))
                .clone(),
            Src::Array(t, _) => wb_parse(&wb, &formula_in(t), s, fc.row, fc.col).map(Arc::new),
        };
        let e = e.filter(|expr| matches!(fc.cached, Value::Empty | Value::Err(ErrorKind::Name)) || !missing_top_level_function(wb, s, expr));
        match e {
            Some(e) => {
                if let Src::Array(_, Some((r0, c0, r1, c1))) = &fc.src {
                    if (*r0, *c0) == (fc.row, fc.col) && (*r1, *c1) != (*r0, *c0) {
                        wb.claim_spill(CellAddr::new(s, *r0, *c0), officina_grid::Rect { sheet: s, r0: *r0, c0: *c0, r1: *r1, c1: *c1 });
                    }
                }
                cells.push((fc.col, fc.row, e));
                cached.push((CellAddr::new(s, fc.row, fc.col), fc.cached));
            }
            None => {
                let text = match &fc.src {
                    Src::Text(t) | Src::Array(t, _) => t.clone(),
                    Src::Shared(si) => shared.get(si).map(|x| x.2.clone()).unwrap_or_default(),
                };
                rep.note_at(Kind::Approximate, "formulas this engine can't calculate yet, kept as their values", || format!("{}: ={}", a1(fc.row, fc.col), text.chars().take(60).collect::<String>()));
            }
        }
    }
    out.formulas += cells.len() as u64;
    // Runs of the same expression down a column become one fill.
    cells.sort_by_key(|c| (c.0, c.1));
    let mut i = 0;
    while i < cells.len() {
        let (col, r0, e) = (cells[i].0, cells[i].1, cells[i].2.clone());
        let mut j = i + 1;
        while j < cells.len() && cells[j].0 == col && cells[j].1 == cells[j - 1].1 + 1 && (Arc::ptr_eq(&cells[j].2, &e) || *cells[j].2 == *e) {
            j += 1;
        }
        let r1 = cells[j - 1].1;
        let text = formula::to_a1(&e, r0, col, &|id| wb.sheet_name(id));
        if wb.fill_formula(s, col, r0, r1, &text).is_err() {
            rep.note_at(Kind::Approximate, "formulas this engine can't calculate yet, kept as their values", || a1(r0, col));
        }
        i = j;
    }
}

/// The differential: our results against the file's, after recalculation.
pub(crate) fn differential(wb: &Workbook, cached: Vec<(CellAddr, Value)>, out: &mut Loaded, rep: &mut Report) {
for (a, want) in cached {
    let got = wb.get(a);
    if !same(&got, &want) {
        out.differ += 1;
        rep.note_at(Kind::Approximate, "formula results that differ from the file's saved results", || format!("{}: {} vs {}", a1(a.row, a.col), got.display(), want.display()));
    }
}
}

/// Equal as a spreadsheet shows them: numbers to 15 significant digits.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Num(x), Value::Num(y)) => x == y || ((x - y).abs() <= 1e-14 * x.abs().max(y.abs())),
        (Value::Empty, Value::Str(s)) | (Value::Str(s), Value::Empty) => s.is_empty(),
        (Value::Empty, Value::Num(x)) | (Value::Num(x), Value::Empty) => *x == 0.0,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formula_prefixes() {
        assert_eq!(formula_in("_xlfn.XLOOKUP(A1,B:B,C:C)+_xlfn.IFS(A1>0,1)"), "=XLOOKUP(A1,B:B,C:C)+IFS(A1>0,1)");
        assert_eq!(formula_in("_xlfn.BYROW(A1:B2,_xleta.SUM)"), "=BYROW(A1:B2,SUM)");
        assert_eq!(formula_in("SUM(A1:A3)"), "=SUM(A1:A3)");
    }

    #[test]
    fn escapes() {
        assert_eq!(decode_escapes("a_x000D_b"), "a\rb");
        assert_eq!(decode_escapes("_x_y"), "_x_y");
    }

    #[test]
    fn paths() {
        assert_eq!(resolve("xl/workbook.xml", "worksheets/sheet1.xml"), "xl/worksheets/sheet1.xml");
        assert_eq!(resolve("xl/worksheets/sheet1.xml", "../drawings/d.xml"), "xl/drawings/d.xml");
        assert_eq!(resolve("", "xl/workbook.xml"), "xl/workbook.xml");
        assert_eq!(resolve("xl/workbook.xml", "/xl/s.xml"), "xl/s.xml");
    }
}

/// `A1:D10` (or one cell) as (r0, c0, r1, c1).
pub(crate) fn parse_ref(s: &str) -> Option<(u32, u32, u32, u32)> {
    let s = s.replace('$', "");
    let (a, b) = s.split_once(':').unwrap_or((&s, &s));
    let ((r0, c0), (r1, c1)) = (parse_a1(a)?, parse_a1(b)?);
    Some((r0.min(r1), c0.min(c1), r0.max(r1), c0.max(c1)))
}

/// Column widths read, hidden ones as negatives: the widths to remember for
/// hidden columns, and the sizes (hidden ones 0).
pub(crate) fn remembered(widths: Vec<(u32, f64)>) -> (Vec<(u32, f64)>, Vec<(u32, f64)>) {
    let mut hw: Vec<(u32, f64)> = widths.iter().filter(|w| w.1 < 0.0).map(|&(c, w)| (c, (-w).min(8192.0))).collect();
    hw.sort_by_key(|w| w.0);
    hw.dedup_by_key(|w| w.0);
    (hw, widths.into_iter().map(|(c, w)| (c, w.max(0.0))).collect())
}
