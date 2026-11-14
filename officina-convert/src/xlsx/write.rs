//! `.xlsx` out, from our own model (runs in the document process).
//!
//! Writes values, formulas (a fill as one shared formula, with its cached results),
//! column widths and row heights, one part per sheet, shared strings. A sheet past
//! `.xlsx`'s limits (1,048,576 rows, 16,384 columns) is refused with its name: the
//! format can't hold it.

use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::Arc;

use officina_grid::addr::{a1, Rect};
use officina_grid::formula;
use officina_grid::{ErrorKind, Value, Workbook};

use crate::xlsx::read::MDW;
use crate::zip::ZipWriter;

pub const XLSX_ROWS: u32 = 1 << 20;
pub const XLSX_COLS: u32 = 1 << 14;

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

fn sheet_xml(wb: &Workbook, s: u16, strings: &mut Strings) -> io::Result<Vec<u8>> {
    let (rows, cols) = wb.extent(s);
    let name = wb.sheet_name(s);
    if rows > XLSX_ROWS || cols > XLSX_COLS {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("the sheet “{name}” has {rows} rows × {cols} columns; .xlsx holds at most 1,048,576 × 16,384")));
    }
    let sh = wb.sheet(s);
    let mut o: Vec<u8> = Vec::with_capacity(1 << 16);
    let _ = write!(o, r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><worksheet {NS}>"#);
    if rows > 0 && cols > 0 {
        let _ = write!(o, r#"<dimension ref="A1:{}"/>"#, a1(rows - 1, cols - 1));
    }
    let dw = sh.col_sizes.default / MDW;
    let dh = sh.row_sizes.default * 72.0 / 96.0;
    let _ = write!(o, r#"<sheetFormatPr defaultRowHeight="{dh}" defaultColWidth="{dw}" customHeight="1"/>"#);
    let over = sh.col_sizes.overrides();
    if !over.is_empty() {
        o.extend_from_slice(b"<cols>");
        for &(c, px) in over {
            if c < XLSX_COLS {
                let _ = write!(o, r#"<col min="{0}" max="{0}" width="{1}" customWidth="1"/>"#, c + 1, px / MDW);
            }
        }
        o.extend_from_slice(b"</cols>");
    }
    o.extend_from_slice(b"<sheetData>");
    let heights = sh.row_sizes.overrides();
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
            // A sized row with no cells.
            let (hr, px) = heights[*hi];
            let _ = write!(o, r#"<row r="{}" ht="{}" customHeight="1"/>"#, hr + 1, px * 72.0 / 96.0);
            *hi += 1;
        }
        if *hi < heights.len() && heights[*hi].0 == r {
            let _ = write!(o, r#"<row r="{}" ht="{}" customHeight="1">"#, r + 1, heights[*hi].1 * 72.0 / 96.0);
            *hi += 1;
        } else {
            let _ = write!(o, r#"<row r="{}">"#, r + 1);
        }
        *cur = Some(r);
    };
    if rows > 0 && cols > 0 {
        // Formula cells whose value is empty are still written.
        let mut each = |r: u32, c: u32, v: Value| {
            let g = wb.group_at(s, r, c);
            if v.is_empty() && g.is_none() {
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
            let _ = write!(o, r#"<c r="{at}"{t}>"#);
            if let Some(g) = g {
                let grp = wb.group(g);
                if grp.r0 == grp.r1 {
                    o.extend_from_slice(b"<f>");
                    esc(&mut o, &formula_out(&formula::to_a1(&grp.expr, r, c, &|id| wb.sheet_name(id))));
                    o.extend_from_slice(b"</f>");
                } else if r == grp.r0 {
                    let id = next_si;
                    next_si += 1;
                    si.insert(g, id);
                    let _ = write!(o, r#"<f t="shared" ref="{}:{}" si="{id}">"#, a1(grp.r0, c), a1(grp.r1, c));
                    esc(&mut o, &formula_out(&formula::to_a1(&grp.expr, r, c, &|id| wb.sheet_name(id))));
                    o.extend_from_slice(b"</f>");
                } else if let Some(id) = si.get(&g) {
                    let _ = write!(o, r#"<f t="shared" si="{id}"/>"#);
                } else {
                    // A fill whose first row was cut off: write the cell's own text.
                    o.extend_from_slice(b"<f>");
                    esc(&mut o, &formula_out(&formula::to_a1(&grp.expr, r, c, &|id| wb.sheet_name(id))));
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
        each_cell_rowmajor(wb, &rect, &mut each);
    }
    if cur_row.is_some() {
        o.extend_from_slice(b"</row>");
    }
    while hi < heights.len() {
        let (hr, px) = heights[hi];
        if hr < XLSX_ROWS {
            let _ = write!(o, r#"<row r="{}" ht="{}" customHeight="1"/>"#, hr + 1, px * 72.0 / 96.0);
        }
        hi += 1;
    }
    o.extend_from_slice(b"</sheetData></worksheet>");
    Ok(o)
}

/// Every cell in row-major order, formula cells included even when their value is
/// empty: values come from `each_rowmajor`, formula cells it skipped are added.
fn each_cell_rowmajor(wb: &Workbook, r: &Rect, f: &mut dyn FnMut(u32, u32, Value)) {
    // Formula cells by row, for the ones with empty values.
    let mut empty_f: Vec<(u32, u32)> = Vec::new();
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
    let mut k = 0;
    wb.each_rowmajor(r, &mut |row, col, v| {
        while k < empty_f.len() && empty_f[k] < (row, col) {
            f(empty_f[k].0, empty_f[k].1, Value::Empty);
            k += 1;
        }
        f(row, col, v);
    });
    while k < empty_f.len() {
        f(empty_f[k].0, empty_f[k].1, Value::Empty);
        k += 1;
    }
}

/// Writes `wb` as `.xlsx`.
pub fn write<W: Write>(wb: &Workbook, out: W) -> io::Result<W> {
    let mut z = ZipWriter::new(out);
    let n = wb.sheet_count();
    let mut strings = Strings { ids: HashMap::new(), list: Vec::new(), uses: 0 };
    let mut parts = Vec::with_capacity(n);
    for s in 0..n {
        parts.push(sheet_xml(wb, s as u16, &mut strings)?);
    }
    let mut ct = String::from(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/><Override PartName="/xl/sharedStrings.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml"/>"#);
    for s in 0..n {
        ct.push_str(&format!(r#"<Override PartName="/xl/worksheets/sheet{}.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>"#, s + 1));
    }
    ct.push_str("</Types>");
    z.add("[Content_Types].xml", ct.as_bytes())?;
    z.add("_rels/.rels", br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#)?;
    let mut w: Vec<u8> = Vec::new();
    let _ = write!(w, r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><workbook {NS}><sheets>"#);
    for s in 0..n {
        w.extend_from_slice(b"<sheet name=\"");
        esc(&mut w, &wb.sheet_name(s as u16));
        let _ = write!(w, r#"" sheetId="{}" r:id="rId{}"/>"#, s + 1, s + 1);
    }
    w.extend_from_slice(b"</sheets></workbook>");
    z.add("xl/workbook.xml", &w)?;
    let mut r = String::from(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#);
    for s in 0..n {
        r.push_str(&format!(r#"<Relationship Id="rId{0}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet{0}.xml"/>"#, s + 1));
    }
    r.push_str(&format!(r#"<Relationship Id="rId{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/>"#, n + 1));
    r.push_str(&format!(r#"<Relationship Id="rId{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/sharedStrings" Target="sharedStrings.xml"/>"#, n + 2));
    r.push_str("</Relationships>");
    z.add("xl/_rels/workbook.xml.rels", r.as_bytes())?;
    // The least styles part Excel opens without repair.
    z.add("xl/styles.xml", br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><fonts count="1"><font><sz val="11"/><name val="Calibri"/><family val="2"/></font></fonts><fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill></fills><borders count="1"><border><left/><right/><top/><bottom/><diagonal/></border></borders><cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs><cellXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#)?;
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
