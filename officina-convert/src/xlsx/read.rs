//! `.xlsx` / `.xlsm` in (ECMA-376 SpreadsheetML, transitional), for the converter
//! process.
//!
//! Read: sheets and their names, values (numbers, shared and inline strings, booleans,
//! errors), formulas (plain, shared fills, array formulas as their first cell), column
//! widths and row heights, the 1904 date system. Formula cells keep the file's cached
//! value until recalculation; the converter compares the two (a differential against
//! whatever wrote the file, Excel included) and reports differences.
//!
//! Reported, not carried (O4b–O4e): styles and number formats, merged cells, charts and
//! drawings, defined names, tables, conditional formats, data validation, comments,
//! hyperlinks. Kept inert: macros (`vbaProject.bin`), external links.

use std::collections::HashMap;
use std::sync::Arc;

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use officina_grid::addr::{a1, parse_a1, CellAddr, MAX_COLS, MAX_ROWS};
use officina_grid::column::Sizes;
use officina_grid::formula::{self, Expr};
use officina_grid::strings::MAX_TEXT;
use officina_grid::{ErrorKind, Value, Workbook};

use crate::report::{Kind, Report};
use crate::zip::{Archive, ZipError};

/// Excel's maximum digit width for Calibri 11 at 96 dpi, px: column widths are in
/// these units.
pub const MDW: f64 = 7.0;
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
fn attr(e: &BytesStart, name: &[u8]) -> Result<Option<String>, XlsxError> {
    for a in e.attributes() {
        let a = a?;
        if a.key.local_name().as_ref() == name {
            return Ok(Some(a.unescape_value().map_err(|e| XlsxError::Xml(e.to_string()))?.into_owned()));
        }
    }
    Ok(None)
}

fn xml(data: &[u8]) -> Reader<&[u8]> {
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
fn rels(z: &Archive, part: &str) -> Result<HashMap<String, (String, String)>, XlsxError> {
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
fn decode_escapes(s: &str) -> std::borrow::Cow<'_, str> {
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
/// carry in files are dropped.
pub fn formula_in(f: &str) -> String {
    let body = f.trim();
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
        if let Some(t) = tail.strip_prefix("_xlfn.").or_else(|| tail.strip_prefix("_xlws.")) {
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

enum Src {
    Text(String),
    Shared(u32),
}

struct FCell {
    row: u32,
    col: u32,
    src: Src,
    cached: Value,
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
    styled: u64,
    cells: u64,
    rep: Report,
    first_row: Option<u32>,
    last_row: Option<u32>,
}

/// Parses one piece of a sheet. Pieces after the first must number their rows.
fn segment(data: &[u8], sst: &[Arc<str>], first: bool) -> Result<Seg, XlsxError> {
    let mut seg = Seg::default();
    let mut r = xml(data);
    r.config_mut().check_end_names = false;
    r.config_mut().allow_unmatched_ends = true;
    let mut buf = Vec::new();
    let mut tbuf = Vec::new();
    let mut pend = Pending { runs: Vec::new() };
    let (mut row, mut col): (i64, i64) = (-1, -1);
    let mut vtext: Vec<u8> = Vec::new();
    let mut vbuf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        let ev = r.read_event_into(&mut buf)?;
        let (e, empty) = match ev {
            Event::Start(e) => (e, false),
            Event::Empty(e) => (e, true),
            Event::Eof => break,
            _ => continue,
        };
        match e.local_name().as_ref() {
            b"sheetFormatPr" => {
                seg.default_h = attr(&e, b"defaultRowHeight")?.and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite() && *v > 0.0 && *v < 410.0);
                seg.default_w = attr(&e, b"defaultColWidth")?.and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite() && *v > 0.0 && *v <= 255.0);
            }
            b"col" => {
                let (Some(min), Some(max)) = (attr(&e, b"min")?.and_then(|v| v.parse::<u32>().ok()), attr(&e, b"max")?.and_then(|v| v.parse::<u32>().ok())) else { continue };
                let hidden = attr(&e, b"hidden")?.is_some_and(|v| v == "1" || v == "true");
                let w = if hidden { Some(0.0) } else { attr(&e, b"width")?.and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite() && *v >= 0.0 && *v <= 255.0) };
                if hidden {
                    seg.rep.note(Kind::Approximate, "hidden columns shown narrow");
                }
                if let Some(w) = w {
                    let px = (w * MDW).round().max(2.0);
                    let (min, max) = (min.clamp(1, MAX_COLS), max.clamp(1, MAX_COLS));
                    // A range over every column is a default, not 16,384 overrides.
                    if max - min > 4096 {
                        seg.default_w = Some(w);
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
                col = -1;
                seg.first_row.get_or_insert(rr as u32);
                seg.last_row = Some(rr as u32);
                if attr(&e, b"customHeight")?.is_some_and(|v| v == "1" || v == "true") {
                    if let Some(h) = attr(&e, b"ht")?.and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite() && *v >= 0.0 && *v < 410.0) {
                        seg.heights.push((row as u32, (h * 96.0 / 72.0).round().max(2.0)));
                    }
                }
                let _ = empty;
            }
            b"c" => {
                // One pass over the attributes, no allocation unless escaped.
                let (mut r_at, mut t_at, mut styled) = (None::<std::borrow::Cow<[u8]>>, None::<std::borrow::Cow<[u8]>>, false);
                for a in e.attributes().with_checks(false) {
                    let a = a?;
                    match a.key.local_name().as_ref() {
                        b"r" => r_at = Some(a.value),
                        b"t" => t_at = Some(a.value),
                        b"s" => styled = a.value.as_ref() != b"0",
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
                if styled {
                    seg.styled += 1;
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
                                let mut s = String::new();
                                read_text(&mut r, b"f", &mut s)?;
                                f = formula_src(ft.as_deref(), si, s, rr, cc, &mut seg.shared, &mut seg.rep);
                            }
                            _ => skip(&mut r, &ce)?,
                        },
                        Event::Empty(ce) if ce.local_name().as_ref() == b"f" => {
                            let ft = attr(&ce, b"t")?;
                            let si = attr(&ce, b"si")?.and_then(|v| v.parse::<u32>().ok());
                            f = formula_src(ft.as_deref(), si, String::new(), rr, cc, &mut seg.shared, &mut seg.rep);
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
                    "d" => {
                        seg.rep.note_at(Kind::Approximate, "ISO date cells kept as text", || a1(rr, cc));
                        Value::str(v.unwrap_or(""))
                    }
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
            b"mergeCell" => seg.rep.note(Kind::Approximate, "merged cells shown unmerged"),
            b"conditionalFormatting" => seg.rep.note(Kind::Dropped, "conditional formats (O4b)"),
            b"dataValidation" => seg.rep.note(Kind::Dropped, "data validation rules"),
            b"hyperlink" => seg.rep.note(Kind::Dropped, "hyperlinks"),
            b"drawing" => seg.rep.note(Kind::Dropped, "charts, pictures and shapes (O4e)"),
            b"legacyDrawing" => seg.rep.note(Kind::Dropped, "comments"),
            b"tablePart" => seg.rep.note(Kind::Approximate, "tables kept as plain cells (O4d)"),
            b"autoFilter" => seg.rep.note(Kind::Dropped, "filters (O4d)"),
            b"pane" if attr(&e, b"state")?.is_some_and(|v| v.starts_with("frozen")) => seg.rep.note(Kind::Dropped, "frozen panes"),
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
fn sheet(wb: &mut Workbook, s: u16, z: &Archive, part: &crate::zip::Entry, sst: &[Arc<str>], out: &mut Loaded, rep: &mut Report, fcells: &mut Vec<FCell>, shared: &mut HashMap<u32, (u32, u32, String)>) -> Result<(), XlsxError> {
    use std::io::Read;
    use std::sync::mpsc::{channel, sync_channel};
    use std::sync::Mutex;
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get()).clamp(1, 16);
    let mut reader = z.reader(part)?;
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
                if seg_tx.send((i, segment(&data, sst, i == 0))).is_err() {
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
                shared.extend(seg.shared);
                widths.extend(seg.widths);
                heights.extend(seg.heights);
                dw = seg.default_w.or(dw);
                dh = seg.default_h.or(dh);
                out.cells += seg.cells;
                if seg.styled > 0 {
                    out.report.note_n(Kind::Approximate, "cells with styles and number formats shown as General (dates show as serial numbers until O4b)", seg.styled);
                }
                rep.merge(seg.rep);
            }
        }
        cutter.join().map_err(|_| bad("the reader stopped"))??;
        if let Some(e) = failed {
            return Err(e);
        }
        let sh = wb.sheet_mut(s);
        if let Some(w) = dw {
            sh.col_sizes = Sizes::new((w * MDW).round().max(2.0));
        }
        if let Some(h) = dh {
            sh.row_sizes = Sizes::new((h * 96.0 / 72.0).round().max(2.0));
        }
        if !widths.is_empty() {
            sh.col_sizes = Sizes::from_overrides(sh.col_sizes.default, widths);
        }
        if !heights.is_empty() {
            sh.row_sizes = Sizes::from_overrides(sh.row_sizes.default, heights);
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

fn formula_src(ft: Option<&str>, si: Option<u32>, text: String, row: u32, col: u32, shared: &mut HashMap<u32, (u32, u32, String)>, rep: &mut Report) -> Option<Src> {
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
            rep.note_at(Kind::Approximate, "array formulas calculated in their first cell only", || a1(row, col));
            (!text.trim().is_empty()).then_some(Src::Text(text))
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
    if z.entries.iter().any(|e| e.name.to_ascii_lowercase().ends_with("vbaproject.bin")) {
        out.report.note(Kind::Inert, "macros (never run; not kept in an Atrium file yet)");
    }
    if wb_rels.values().any(|(t, _)| t.ends_with("/externalLink")) {
        out.report.note(Kind::Inert, "links to other workbooks (cached values kept, never refreshed)");
    }

    // Sheets in order.
    let mut sheets: Vec<(String, String)> = Vec::new();
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
                        if attr(&e, b"state")?.is_some_and(|v| v != "visible") {
                            out.report.note(Kind::Approximate, "hidden sheets shown");
                        }
                        match wb_rels.get(&id) {
                            Some((t, p)) if t.ends_with("/worksheet") => sheets.push((name, p.clone())),
                            Some(_) => out.report.note_at(Kind::Dropped, "chart sheets and dialog sheets", || name.clone()),
                            None => return Err(bad("a sheet without its part")),
                        }
                    }
                    b"workbookPr" => out.date1904 = attr(&e, b"date1904")?.is_some_and(|v| v == "1" || v == "true"),
                    b"definedName" => out.report.note(Kind::Dropped, "defined names (O4d)"),
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
    if out.date1904 {
        out.report.note(Kind::Approximate, "1904 date system: dates are serial numbers counted from 1904");
    }
    let sst_path = wb_rels.values().find(|(t, _)| t.ends_with("/sharedStrings")).map(|(_, p)| p.as_str());
    let sst = shared_strings(&z, sst_path, &mut out.report)?;

    let mut wb = Workbook::new();
    for (name, _) in &sheets {
        // Names Excel allows and we don't get a safe stand-in.
        let mut n: String = name.chars().filter(|c| !"[]:*?/\\".contains(*c)).collect();
        if n.is_empty() || wb.sheet_id(&n).is_some() {
            n = format!("Sheet{}", wb.sheet_count() + 1);
        }
        wb.add_sheet(&n).map_err(bad)?;
    }
    let mut rep = Report::default();
    let mut all_f: Vec<(u16, Vec<FCell>, HashMap<u32, (u32, u32, String)>)> = Vec::new();
    for (i, (_, part)) in sheets.iter().enumerate() {
        let e = z.find(part).ok_or_else(|| bad(format!("missing {part}")))?.clone();
        let mut fcells = Vec::new();
        let mut shared = HashMap::new();
        sheet(&mut wb, i as u16, &z, &e, &sst, &mut out, &mut rep, &mut fcells, &mut shared)?;
        all_f.push((i as u16, fcells, shared));
    }

    // Formulas, once every sheet exists (they name each other).
    let mut cached: Vec<(CellAddr, Value)> = Vec::new();
    for (s, fcells, shared) in all_f {
        // A shared formula's expression, parsed once at its master cell; relative, so
        // the same for every cell of the fill.
        let mut exprs: HashMap<u32, Option<Arc<Expr>>> = HashMap::new();
        let mut cells: Vec<(u32, u32, Arc<Expr>)> = Vec::with_capacity(fcells.len());
        for fc in fcells {
            let e = match &fc.src {
                Src::Text(t) => wb_parse(&wb, &formula_in(t), s, fc.row, fc.col).map(Arc::new),
                Src::Shared(si) => exprs
                    .entry(*si)
                    .or_insert_with(|| shared.get(si).and_then(|(r, c, t)| wb_parse(&wb, &formula_in(t), s, *r, *c)).map(Arc::new))
                    .clone(),
            };
            match e {
                Some(e) => {
                    cells.push((fc.col, fc.row, e));
                    cached.push((CellAddr::new(s, fc.row, fc.col), fc.cached));
                }
                None => {
                    let text = match &fc.src {
                        Src::Text(t) => t.clone(),
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
    wb.recalc();
    // The differential: our results against the file's.
    for (a, want) in cached {
        let got = wb.get(a);
        if !same(&got, &want) {
            out.differ += 1;
            rep.note_at(Kind::Approximate, "formula results that differ from the file's saved results", || format!("{}: {} vs {}", a1(a.row, a.col), got.display(), want.display()));
        }
    }
    out.report.merge(rep);
    Ok((wb, out))
}

fn wb_parse(wb: &Workbook, src: &str, s: u16, row: u32, col: u32) -> Option<Expr> {
    wb.parse(src, s, row, col).ok()
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
