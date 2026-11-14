//! `.xlsx` out, from our own model (runs in the document process).
//!
//! Writes values, formulas (a fill as one shared formula, with its cached results),
//! column widths and row heights, one part per sheet, shared strings. A sheet past
//! `.xlsx`'s limits (1,048,576 rows, 16,384 columns) is refused with its name: the
//! format can't hold it.

use std::collections::{BTreeMap, HashMap};
use std::io::{self, Write};
use std::sync::Arc;

use officina_grid::addr::{a1, Rect};
use officina_grid::formula;
use officina_grid::filter::Cond;
use officina_grid::{ErrorKind, Value, Workbook};

use crate::xlsx::read::MDW;
use crate::zip::ZipWriter;

pub const XLSX_ROWS: u32 = 1 << 20;
pub const XLSX_COLS: u32 = 1 << 14;
/// Styled empty cells written as cells of their own, per sheet, at most.
const MAX_EXTRA: usize = 4_000_000;

/// Functions newer than Excel 2007 carry a prefix in files.
const FUTURE: &[&str] = &[
    "XLOOKUP", "XMATCH", "IFS", "SWITCH", "MAXIFS", "MINIFS", "CONCAT", "TEXTJOIN", "IFNA", "STDEV.S", "STDEV.P", "VAR.S", "VAR.P", "CEILING.MATH", "FLOOR.MATH", "DAYS", "ISOWEEKNUM", "SEQUENCE", "FILTER", "SORT", "SORTBY", "UNIQUE", "LET", "XOR", "IFERROR.X",
];

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'.'
}

/// Our formula text (with `=`) as a file stores it.
pub fn formula_out(f: &str) -> String {
    let body = f.strip_prefix('=').unwrap_or(f);
    let b = body.as_bytes();
    let mut out = String::with_capacity(body.len() + 8);
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'"' {
            // A string literal, "" escapes a quote.
            let start = i;
            i += 1;
            while i < b.len() {
                if b[i] == b'"' {
                    if i + 1 < b.len() && b[i + 1] == b'"' {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push_str(&body[start..i]);
            continue;
        }
        if c == b'\'' {
            let start = i;
            i += 1;
            while i < b.len() && b[i] != b'\'' {
                i += 1;
            }
            i = (i + 1).min(b.len());
            out.push_str(&body[start..i]);
            continue;
        }
        if is_ident(c) && (i == 0 || !is_ident(b[i - 1])) {
            let start = i;
            while i < b.len() && is_ident(b[i]) {
                i += 1;
            }
            let word = &body[start..i];
            if i < b.len() && b[i] == b'(' && FUTURE.iter().any(|f| f.eq_ignore_ascii_case(word)) {
                out.push_str("_xlfn.");
            }
            out.push_str(word);
            continue;
        }
        let ch = body[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

fn esc(out: &mut Vec<u8>, s: &str) {
    for (i, ch) in s.char_indices() {
        match ch {
            '&' => out.extend_from_slice(b"&amp;"),
            '<' => out.extend_from_slice(b"&lt;"),
            '>' => out.extend_from_slice(b"&gt;"),
            '"' => out.extend_from_slice(b"&quot;"),
            // Characters XML 1.0 can't hold go as OOXML's _xHHHH_.
            c if (c as u32) < 0x20 && c != '\t' && c != '\n' && c != '\r' => {
                out.extend_from_slice(format!("_x{:04X}_", c as u32).as_bytes());
            }
            // A literal "_xHHHH_" would read back as an escape: escape its underscore.
            '_' if looks_escaped(&s[i..]) => out.extend_from_slice(b"_x005F_"),
            c => {
                let mut b = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
            }
        }
    }
}

fn looks_escaped(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 7 && b[1] == b'x' && b[6] == b'_' && b[2..6].iter().all(u8::is_ascii_hexdigit)
}

/// A number as `xsd:double`, shortest round trip.
fn num(out: &mut Vec<u8>, x: f64) {
    if x == x.trunc() && x.abs() < 1e15 {
        let _ = write!(out, "{}", x as i64);
    } else {
        let _ = write!(out, "{x:?}");
    }
}

struct Strings {
    ids: HashMap<Arc<str>, u32>,
    list: Vec<Arc<str>>,
    uses: u64,
}

impl Strings {
    fn id(&mut self, s: &Arc<str>) -> u32 {
        self.uses += 1;
        if let Some(&i) = self.ids.get(s) {
            return i;
        }
        let i = self.list.len() as u32;
        self.ids.insert(s.clone(), i);
        self.list.push(s.clone());
        i
    }
}

const NS: &str = r#"xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships""#;

fn sheet_xml(wb: &Workbook, s: u16, strings: &mut Strings, code: Option<&str>) -> io::Result<Vec<u8>> {
    let (rows, cols) = wb.extent(s);
    let name = wb.sheet_name(s);
    if rows > XLSX_ROWS || cols > XLSX_COLS {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("the sheet “{name}” has {rows} rows × {cols} columns; .xlsx holds at most 1,048,576 × 16,384")));
    }
    let sh = wb.sheet(s);
    let mut o: Vec<u8> = Vec::with_capacity(1 << 16);
    let _ = write!(o, r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><worksheet {NS}>"#);
    if let Some(c) = code {
        // Code names are letters, digits and `_` (checked by the grid).
        let _ = write!(o, r#"<sheetPr codeName="{c}"/>"#);
    }
    if rows > 0 && cols > 0 {
        let _ = write!(o, r#"<dimension ref="A1:{}"/>"#, a1(rows - 1, cols - 1));
    }
    let dw = sh.col_sizes.default / MDW;
    let dh = sh.row_sizes.default * 72.0 / 96.0;
    let _ = write!(o, r#"<sheetFormatPr defaultRowHeight="{dh}" defaultColWidth="{dw}" customHeight="1"/>"#);
    // Styles: a run to the last row is the column's style (it covers the empty
    // cells below); other styled empty cells are written as cells of their own.
    let last = XLSX_ROWS - 1;
    let mut col_style: BTreeMap<u32, u32> = BTreeMap::new();
    let mut extra: Vec<(u32, u32, u32)> = Vec::new();
    for (c, runs) in sh.styles.iter().enumerate().take(XLSX_COLS as usize) {
        let c = c as u32;
        let tail = runs.get(last);
        if tail != 0 {
            col_style.insert(c, tail);
        }
        for (a, b, id) in runs.spans(0, last) {
            if id == tail {
                continue;
            }
            // Cells whose style differs from the column's: empty ones need writing
            // (style 0 included, under a styled column).
            if b - a > MAX_EXTRA as u32 || extra.len() > MAX_EXTRA {
                continue;
            }
            for r in a..=b {
                if wb.value(s, r, c).is_empty() && wb.group_at(s, r, c).is_none() {
                    extra.push((r, c, id));
                }
            }
        }
    }
    extra.sort_unstable();
    let over = sh.col_sizes.overrides();
    if !over.is_empty() || !col_style.is_empty() {
        let mut cols: BTreeMap<u32, (Option<f64>, u32)> = BTreeMap::new();
        for &(c, px) in over {
            cols.entry(c).or_insert((None, 0)).0 = Some(px);
        }
        for (&c, &id) in &col_style {
            cols.entry(c).or_insert((None, 0)).1 = id;
        }
        o.extend_from_slice(b"<cols>");
        for (c, (px, id)) in cols {
            if c >= XLSX_COLS {
                continue;
            }
            let _ = write!(o, r#"<col min="{0}" max="{0}" width="{1}""#, c + 1, px.unwrap_or(sh.col_sizes.default) / MDW);
            if px.is_some() {
                o.extend_from_slice(br#" customWidth="1""#);
            }
            if id != 0 {
                let _ = write!(o, r#" style="{id}""#);
            }
            o.extend_from_slice(b"/>");
        }
        o.extend_from_slice(b"</cols>");
    }
    o.extend_from_slice(b"<sheetData>");
    // Rows with attributes: their height, hidden.
    let mut meta: BTreeMap<u32, (Option<f64>, bool)> = BTreeMap::new();
    for &(r, px) in sh.row_sizes.overrides() {
        meta.entry(r).or_insert((None, false)).0 = Some(px);
    }
    for &(a, b) in &sh.hidden {
        for r in a..=b.min(XLSX_ROWS - 1) {
            meta.entry(r).or_insert((None, false)).1 = true;
        }
    }
    let heights: Vec<(u32, (Option<f64>, bool))> = meta.into_iter().collect();
    let row_attrs = |o: &mut Vec<u8>, m: &(Option<f64>, bool)| {
        if let Some(px) = m.0 {
            let _ = write!(o, r#" ht="{}" customHeight="1""#, px * 72.0 / 96.0);
        }
        if m.1 {
            o.extend_from_slice(br#" hidden="1""#);
        }
    };
    let mut hi = 0;
    let mut cur_row: Option<u32> = None;
    // Shared-formula ids: one per fill longer than a cell.
    let mut si: HashMap<u32, u32> = HashMap::new();
    let mut next_si = 0u32;
    let rect = Rect { sheet: s, r0: 0, c0: 0, r1: rows.saturating_sub(1), c1: cols.saturating_sub(1) };
    let open_row = |o: &mut Vec<u8>, r: u32, cur: &mut Option<u32>, hi: &mut usize| {
        if *cur == Some(r) {
            return;
        }
        if cur.is_some() {
            o.extend_from_slice(b"</row>");
        }
        while *hi < heights.len() && heights[*hi].0 < r {
            // A sized or hidden row with no cells.
            let _ = write!(o, r#"<row r="{}""#, heights[*hi].0 + 1);
            row_attrs(o, &heights[*hi].1);
            o.extend_from_slice(b"/>");
            *hi += 1;
        }
        let _ = write!(o, r#"<row r="{}""#, r + 1);
        if *hi < heights.len() && heights[*hi].0 == r {
            row_attrs(o, &heights[*hi].1);
            *hi += 1;
        }
        o.push(b'>');
        *cur = Some(r);
    };
    if (rows > 0 && cols > 0) || !extra.is_empty() {
        // Formula cells whose value is empty are still written.
        let mut each = |r: u32, c: u32, v: Value| {
            let g = wb.group_at(s, r, c);
            let xf = sh.style_id(r, c);
            let styled = xf != col_style.get(&c).copied().unwrap_or(0);
            if v.is_empty() && g.is_none() && !styled {
                return;
            }
            // Errors Excel doesn't have: a cycle shows 0 there (with a warning), a
            // result over our limits is #NUM!.
            let v = match v {
                Value::Err(ErrorKind::Circ) => Value::Num(0.0),
                Value::Err(ErrorKind::Limit) => Value::Err(ErrorKind::Num),
                v => v,
            };
            open_row(&mut o, r, &mut cur_row, &mut hi);
            let at = a1(r, c);
            let t = match &v {
                Value::Num(_) | Value::Empty => "",
                Value::Str(_) if g.is_some() => r#" t="str""#,
                Value::Str(_) => r#" t="s""#,
                Value::Bool(_) => r#" t="b""#,
                Value::Err(_) => r#" t="e""#,
            };
            if xf != 0 || styled {
                let _ = write!(o, r#"<c r="{at}" s="{xf}"{t}>"#);
            } else {
                let _ = write!(o, r#"<c r="{at}"{t}>"#);
            }
            if let Some(g) = g {
                let grp = wb.group(g);
                if grp.r0 == grp.r1 {
                    o.extend_from_slice(b"<f>");
                    esc(&mut o, &formula_out(&formula::to_a1_file(&grp.expr, r, c, &|id| wb.sheet_name(id), &|rr, cc| wb.table_at(s, rr, cc).map(|t| t.name.clone()))));
                    o.extend_from_slice(b"</f>");
                } else if r == grp.r0 {
                    let id = next_si;
                    next_si += 1;
                    si.insert(g, id);
                    let _ = write!(o, r#"<f t="shared" ref="{}:{}" si="{id}">"#, a1(grp.r0, c), a1(grp.r1, c));
                    esc(&mut o, &formula_out(&formula::to_a1_file(&grp.expr, r, c, &|id| wb.sheet_name(id), &|rr, cc| wb.table_at(s, rr, cc).map(|t| t.name.clone()))));
                    o.extend_from_slice(b"</f>");
                } else if let Some(id) = si.get(&g) {
                    let _ = write!(o, r#"<f t="shared" si="{id}"/>"#);
                } else {
                    // A fill whose first row was cut off: write the cell's own text.
                    o.extend_from_slice(b"<f>");
                    esc(&mut o, &formula_out(&formula::to_a1_file(&grp.expr, r, c, &|id| wb.sheet_name(id), &|rr, cc| wb.table_at(s, rr, cc).map(|t| t.name.clone()))));
                    o.extend_from_slice(b"</f>");
                }
            }
            match &v {
                Value::Empty => {}
                Value::Num(x) => {
                    o.extend_from_slice(b"<v>");
                    num(&mut o, *x);
                    o.extend_from_slice(b"</v>");
                }
                Value::Str(sv) if g.is_some() => {
                    o.extend_from_slice(b"<v>");
                    esc(&mut o, sv);
                    o.extend_from_slice(b"</v>");
                }
                Value::Str(sv) => {
                    let _ = write!(o, "<v>{}</v>", strings.id(sv));
                }
                Value::Bool(b) => {
                    let _ = write!(o, "<v>{}</v>", u8::from(*b));
                }
                Value::Err(e) => {
                    o.extend_from_slice(b"<v>");
                    esc(&mut o, e.text());
                    o.extend_from_slice(b"</v>");
                }
            }
            o.extend_from_slice(b"</c>");
        };
        let ext = Rect { r1: rect.r1.max(extra.last().map_or(0, |e| e.0)), c1: rect.c1.max(extra.iter().map(|e| e.1).max().unwrap_or(0)), ..rect };
        each_cell_rowmajor(wb, &ext, &extra, &mut each);
    }
    if cur_row.is_some() {
        o.extend_from_slice(b"</row>");
    }
    while hi < heights.len() {
        let (hr, m) = heights[hi];
        if hr < XLSX_ROWS {
            let _ = write!(o, r#"<row r="{}""#, hr + 1);
            row_attrs(&mut o, &m);
            o.extend_from_slice(b"/>");
        }
        hi += 1;
    }
    o.extend_from_slice(b"</sheetData>");
    if let Some(f) = &sh.filter {
        if f.rect.r1 < XLSX_ROWS && f.rect.c1 < XLSX_COLS {
            let _ = write!(o, r#"<autoFilter ref="{}:{}""#, a1(f.rect.r0, f.rect.c0), a1(f.rect.r1, f.rect.c1));
            if f.conds.is_empty() {
                o.extend_from_slice(b"/>");
            } else {
                o.push(b'>');
                for (c, cond) in &f.conds {
                    let _ = write!(o, r#"<filterColumn colId="{}">"#, c - f.rect.c0);
                    match cond {
                        Cond::Values(v) => {
                            o.extend_from_slice(if v.iter().any(|x| x.is_empty()) { br#"<filters blank="1">"# as &[u8] } else { b"<filters>" });
                            for x in v.iter().filter(|x| !x.is_empty()) {
                                o.extend_from_slice(b"<filter val=\"");
                                esc(&mut o, x);
                                o.extend_from_slice(b"\"/>");
                            }
                            o.extend_from_slice(b"</filters>");
                        }
                        Cond::Compare(cs, and) => {
                            o.extend_from_slice(if *and { br#"<customFilters and="1">"# as &[u8] } else { b"<customFilters>" });
                            for (op, x) in cs {
                                let name = match op.as_str() {
                                    "<>" => "notEqual",
                                    "<" => "lessThan",
                                    "<=" => "lessThanOrEqual",
                                    ">" => "greaterThan",
                                    ">=" => "greaterThanOrEqual",
                                    _ => "equal",
                                };
                                let _ = write!(o, r#"<customFilter operator="{name}" val="{x}"/>"#);
                            }
                            o.extend_from_slice(b"</customFilters>");
                        }
                    }
                    o.extend_from_slice(b"</filterColumn>");
                }
                o.extend_from_slice(b"</autoFilter>");
            }
        }
    }
    let k = wb.tables().iter().filter(|t| t.rect.sheet == s).count();
    if wb.charts().iter().any(|c| c.anchor.sheet == s) {
        let _ = write!(o, r#"<drawing r:id="rId{}"/>"#, k + 1);
    }
    if k > 0 {
        let _ = write!(o, r#"<tableParts count="{k}">"#);
        for j in 0..k {
            let _ = write!(o, r#"<tablePart r:id="rId{}"/>"#, j + 1);
        }
        o.extend_from_slice(b"</tableParts>");
    }
    o.extend_from_slice(b"</worksheet>");
    Ok(o)
}

/// `xl/tables/tableN.xml` for table `t`, numbered `id` (from 1).
fn table_xml(t: &officina_grid::tables::Table, id: usize) -> Vec<u8> {
    let r = &t.rect;
    let mut o: Vec<u8> = Vec::new();
    let _ = write!(o, r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><table xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" id="{id}" name=""#);
    esc(&mut o, &t.name);
    o.extend_from_slice(b"\" displayName=\"");
    esc(&mut o, &t.name);
    let _ = write!(o, r#"" ref="{}:{}""#, a1(r.r0, r.c0), a1(r.r1, r.c1));
    if !t.header {
        o.extend_from_slice(br#" headerRowCount="0""#);
    }
    o.extend_from_slice(if t.totals { br#" totalsRowCount="1">"# as &[u8] } else { br#" totalsRowShown="0">"# });
    if t.header {
        let _ = write!(o, r#"<autoFilter ref="{}:{}"/>"#, a1(r.r0, r.c0), a1(r.r1 - t.totals as u32, r.c1));
    }
    let _ = write!(o, r#"<tableColumns count="{}">"#, t.columns.len());
    for (i, c) in t.columns.iter().enumerate() {
        let _ = write!(o, r#"<tableColumn id="{}" name=""#, i + 1);
        esc(&mut o, c);
        o.extend_from_slice(b"\"/>");
    }
    o.extend_from_slice(b"</tableColumns>");
    if let Some(st) = &t.style {
        o.extend_from_slice(b"<tableStyleInfo name=\"");
        esc(&mut o, st);
        let _ = write!(o, r#"" showFirstColumn="0" showLastColumn="0" showRowStripes="{}" showColumnStripes="0"/>"#, t.stripes as u8);
    }
    o.extend_from_slice(b"</table>");
    o
}

/// Every cell in row-major order, formula cells included even when their value is
/// empty: values come from `each_rowmajor`, formula cells it skipped are added.
fn each_cell_rowmajor(wb: &Workbook, r: &Rect, extra: &[(u32, u32, u32)], f: &mut dyn FnMut(u32, u32, Value)) {
    // Formula cells by row, for the ones with empty values, and styled empty cells.
    let mut empty_f: Vec<(u32, u32)> = extra.iter().map(|e| (e.0, e.1)).collect();
    for (_, g) in wb.groups() {
        if g.sheet != r.sheet {
            continue;
        }
        for row in g.r0..=g.r1 {
            if wb.value(r.sheet, row, g.col).is_empty() {
                empty_f.push((row, g.col));
            }
        }
    }
    empty_f.sort_unstable();
    empty_f.dedup();
    let mut k = 0;
    wb.each_rowmajor(r, &mut |row, col, v| {
        while k < empty_f.len() && empty_f[k] < (row, col) {
            f(empty_f[k].0, empty_f[k].1, Value::Empty);
            k += 1;
        }
        if k < empty_f.len() && empty_f[k] == (row, col) {
            k += 1;
        }
        f(row, col, v);
    });
    while k < empty_f.len() {
        f(empty_f[k].0, empty_f[k].1, Value::Empty);
        k += 1;
    }
}

/// Writes `wb` as `.xlsx`; macros are not written (a `.xlsx` can't hold them).
pub fn write<W: Write>(wb: &Workbook, out: W) -> io::Result<W> {
    write_with(wb, out, None)
}

/// Writes `wb` as `.xlsm`: as [`write`], with the macros kept from an `.xlsm`
/// byte for byte (if the workbook has any).
pub fn write_xlsm<W: Write>(wb: &Workbook, out: W) -> io::Result<W> {
    write_with(wb, out, super::macros::writable(wb))
}

fn write_with<W: Write>(wb: &Workbook, out: W, macros: Option<&officina_grid::macros::Macros>) -> io::Result<W> {
    let mut z = ZipWriter::new(out);
    let code_of = |s: usize| macros.and_then(|m| m.sheet_codes.iter().find(|(i, _)| *i as usize == s)).map(|(_, c)| &**c);
    let n = wb.sheet_count();
    let mut strings = Strings { ids: HashMap::new(), list: Vec::new(), uses: 0 };
    let mut parts = Vec::with_capacity(n);
    for s in 0..n {
        parts.push(sheet_xml(wb, s as u16, &mut strings, code_of(s))?);
    }
    let main = if macros.is_some() { super::macros::MAIN_MACRO } else { "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml" };
    let mut ct = String::from(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="{MAIN}"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/><Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/>"#).replace("{MAIN}", main);
    if let Some(m) = macros {
        ct.push_str(&super::macros::content_type_xml(m));
    }
    for s in 0..n {
        ct.push_str(&format!(r#"<Override PartName="/xl/worksheets/sheet{}.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>"#, s + 1));
    }
    for s in 0..n {
        if wb.charts().iter().any(|c| c.anchor.sheet as usize == s) {
            ct.push_str(&format!(r#"<Override PartName="/xl/drawings/drawing{}.xml" ContentType="application/vnd.openxmlformats-officedocument.drawing+xml"/>"#, s + 1));
        }
    }
    for i in 0..wb.charts().len() {
        ct.push_str(&format!(r#"<Override PartName="/xl/charts/chart{}.xml" ContentType="application/vnd.openxmlformats-officedocument.drawingml.chart+xml"/>"#, i + 1));
    }
    for i in 0..wb.tables().len() {
        ct.push_str(&format!(r#"<Override PartName="/xl/tables/table{}.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.table+xml"/>"#, i + 1));
    }
    ct.push_str("</Types>");
    z.add("[Content_Types].xml", ct.as_bytes())?;
    z.add("_rels/.rels", br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#)?;
    let mut w: Vec<u8> = Vec::new();
    let _ = write!(w, r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><workbook {NS}>"#);
    if let Some(c) = macros.and_then(|m| m.workbook_code.as_deref()) {
        let _ = write!(w, r#"<workbookPr codeName="{c}"/>"#);
    }
    w.extend_from_slice(b"<sheets>");
    for s in 0..n {
        w.extend_from_slice(b"<sheet name=\"");
        esc(&mut w, &wb.sheet_name(s as u16));
        let _ = write!(w, r#"" sheetId="{}" r:id="rId{}"/>"#, s + 1, s + 1);
    }
    w.extend_from_slice(b"</sheets>");
    let filtered: Vec<u16> = (0..n as u16).filter(|&s| wb.sheet(s).filter.is_some()).collect();
    if !wb.names().is_empty() || !filtered.is_empty() {
        w.extend_from_slice(b"<definedNames>");
        // Excel's name for each auto-filter's range.
        for &s in &filtered {
            let f = wb.sheet(s).filter.as_ref().unwrap();
            let _ = write!(w, r#"<definedName name="_xlnm._FilterDatabase" localSheetId="{s}" hidden="1">"#);
            let sheet = crate::ods::formula::sheet_a1(&wb.sheet_name(s));
            esc(&mut w, &format!("{sheet}!${}${}:${}${}", officina_grid::addr::col_name(f.rect.c0), f.rect.r0 + 1, officina_grid::addr::col_name(f.rect.c1), f.rect.r1 + 1));
            w.extend_from_slice(b"</definedName>");
        }
        for d in wb.names().iter().filter(|d| !d.name.eq_ignore_ascii_case("_xlnm._FilterDatabase")) {
            w.extend_from_slice(b"<definedName name=\"");
            esc(&mut w, &d.name);
            w.push(b'"');
            if let Some(s) = d.scope {
                let _ = write!(w, r#" localSheetId="{s}""#);
            }
            w.push(b'>');
            esc(&mut w, &formula_out(&wb.name_text(d)));
            w.extend_from_slice(b"</definedName>");
        }
        w.extend_from_slice(b"</definedNames>");
    }
    w.extend_from_slice(b"</workbook>");
    z.add("xl/workbook.xml", &w)?;
    let mut r = String::from(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#);
    for s in 0..n {
        r.push_str(&format!(r#"<Relationship Id="rId{0}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet{0}.xml"/>"#, s + 1));
    }
    r.push_str(&format!(r#"<Relationship Id="rId{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>"#, n + 1));
    r.push_str(&format!(r#"<Relationship Id="rId{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/>"#, n + 2));
    if let Some(m) = macros {
        let vba = m.parts.iter().find(|p| &*p.media_type == super::macros::VBA).expect("writable checked");
        r.push_str(&format!(r#"<Relationship Id="rId{}" Type="{}" Target="{}"/>"#, n + 3, super::macros::VBA_REL, &vba.path[3..]));
    }
    r.push_str("</Relationships>");
    z.add("xl/_rels/workbook.xml.rels", r.as_bytes())?;
    if let Some(m) = macros {
        super::macros::write_parts(&mut z, m)?;
    }
    z.add("xl/styles.xml", &super::styles::write(wb))?;
    let mut sst: Vec<u8> = Vec::new();
    let _ = write!(sst, r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><sst xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" count="{}" uniqueCount="{}">"#, strings.uses, strings.list.len());
    for s in &strings.list {
        let space = s.starts_with(char::is_whitespace) || s.ends_with(char::is_whitespace);
        sst.extend_from_slice(if space { br#"<si><t xml:space="preserve">"# as &[u8] } else { b"<si><t>" });
        esc(&mut sst, s);
        sst.extend_from_slice(b"</t></si>");
    }
    sst.extend_from_slice(b"</sst>");
    z.add("xl/sharedStrings.xml", &sst)?;
    for (s, p) in parts.into_iter().enumerate() {
        z.add(&format!("xl/worksheets/sheet{}.xml", s + 1), &p)?;
        // Its tables, in the order its tableParts name them.
        let mine: Vec<usize> = (0..wb.tables().len()).filter(|&i| wb.tables()[i].rect.sheet as usize == s).collect();
        let charts: Vec<usize> = (1..=wb.charts().len()).filter(|&i| wb.charts()[i - 1].anchor.sheet as usize == s).collect();
        if !mine.is_empty() || !charts.is_empty() {
            let mut r = String::from(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#);
            for (j, i) in mine.iter().enumerate() {
                r.push_str(&format!(r#"<Relationship Id="rId{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/table" Target="../tables/table{}.xml"/>"#, j + 1, i + 1));
            }
            if !charts.is_empty() {
                // One drawing per sheet with charts, numbered by its sheet.
                r.push_str(&format!(r#"<Relationship Id="rId{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/drawing" Target="../drawings/drawing{}.xml"/>"#, mine.len() + 1, s + 1));
                z.add(&format!("xl/drawings/drawing{}.xml", s + 1), super::chart::drawing_xml(wb, &charts).as_bytes())?;
                let mut dr = String::from(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#);
                for (j, i) in charts.iter().enumerate() {
                    dr.push_str(&format!(r#"<Relationship Id="rId{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/chart" Target="../charts/chart{i}.xml"/>"#, j + 1));
                }
                dr.push_str("</Relationships>");
                z.add(&format!("xl/drawings/_rels/drawing{}.xml.rels", s + 1), dr.as_bytes())?;
            }
            r.push_str("</Relationships>");
            z.add(&format!("xl/worksheets/_rels/sheet{}.xml.rels", s + 1), r.as_bytes())?;
        }
    }
    for (i, t) in wb.tables().iter().enumerate() {
        z.add(&format!("xl/tables/table{}.xml", i + 1), &table_xml(t, i + 1))?;
    }
    for (i, c) in wb.charts().iter().enumerate() {
        z.add(&format!("xl/charts/chart{}.xml", i + 1), super::chart::chart_xml(wb, c).as_bytes())?;
    }
    z.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn future_functions_get_their_prefix() {
        assert_eq!(formula_out("=XLOOKUP(A1,B:B,C:C)+SUM(A1)"), "_xlfn.XLOOKUP(A1,B:B,C:C)+SUM(A1)");
        assert_eq!(formula_out("=\"IFS(\"&IFS(A1,1)"), "\"IFS(\"&_xlfn.IFS(A1,1)");
        assert_eq!(formula_out("='IFS(x'!A1"), "'IFS(x'!A1");
        assert_eq!(formula_out("=MYIFS(1)"), "MYIFS(1)");
    }
}
