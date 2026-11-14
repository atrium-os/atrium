//! CSV and TSV in and out (officina.md §5.2): RFC 4180 with the variants found in the
//! wild.
//!
//! Reading is for untrusted files: bounded (cells per row, rows, text per cell), never
//! panics, and **never evaluates formulas** by default. A cell that reads `=...` in a CSV
//! is text unless the caller asks otherwise, since a downloaded CSV that computes
//! (`=WEBSERVICE(...)`, or a formula a later export turns into a command for another
//! program) is a known attack.
//!
//! - Separator: comma, semicolon or tab, detected from the first lines when not given
//!   (the one that splits them most evenly, outside quotes).
//! - Encoding: UTF-8 (a BOM is skipped); bytes that are not UTF-8 are read as
//!   Windows-1252, as Excel does for "ANSI" CSV.
//! - Line ends: CRLF, LF or CR; newlines inside quotes are kept in the cell.
//! - A cell becomes a number when it reads as one (`1e3`, `-2.5`, `12%`), `TRUE` or
//!   `FALSE`, an error value, or else text. Dates stay text until number formats (O4).
//!
//! Writing quotes a field when it holds the separator, a quote, a newline, or leading or
//! trailing spaces; numbers are written as General shows them (15 significant digits).

use crate::addr::{Rect, MAX_COLS, MAX_ROWS};
use crate::book::Workbook;
use crate::column::Cell;
use crate::eval::parse_number;
use crate::strings::MAX_TEXT;
use crate::value::{ErrorKind, Value};
use std::io::{self, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CsvOptions {
    /// `None`: detect.
    pub separator: Option<u8>,
    /// Read `=...` cells as formulas (only for files the user made).
    pub formulas: bool,
}

impl Default for CsvOptions {
    fn default() -> Self {
        Self { separator: None, formulas: false }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CsvStats {
    pub rows: u32,
    pub cols: u32,
    pub cells: u64,
    pub separator: u8,
    /// The file was not UTF-8 and was read as Windows-1252.
    pub latin: bool,
    /// Cells whose text was longer than a cell holds and was cut.
    pub cut: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CsvError {
    TooManyRows,
    TooManyColumns,
}

impl std::fmt::Display for CsvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CsvError::TooManyRows => write!(f, "more rows than a sheet holds"),
            CsvError::TooManyColumns => write!(f, "more columns than a sheet holds"),
        }
    }
}

impl std::error::Error for CsvError {}

/// The separator that splits the first lines of `text` most evenly.
pub fn detect_separator(text: &str) -> u8 {
    let lines: Vec<&str> = text.lines().take(20).collect();
    let mut best = (b',', 0usize, false);
    for sep in [b',', b';', b'\t'] {
        let counts: Vec<usize> = lines
            .iter()
            .map(|l| {
                let mut quoted = false;
                l.bytes()
                    .filter(|&b| {
                        if b == b'"' {
                            quoted = !quoted;
                        }
                        b == sep && !quoted
                    })
                    .count()
            })
            .collect();
        let first = counts.first().copied().unwrap_or(0);
        let even = !counts.is_empty() && counts.iter().all(|&c| c == first);
        // Prefer a separator present on every line the same number of times.
        if first > 0 && ((even && !best.2) || (even == best.2 && first > best.1)) {
            best = (sep, first, even);
        }
    }
    best.0
}

/// Windows-1252 to Unicode for bytes 0x80–0x9F; the rest are Latin-1.
fn cp1252(b: u8) -> char {
    const HIGH: [u16; 32] = [
        0x20AC, 0x81, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039, 0x0152, 0x8D, 0x017D, 0x8F, 0x90, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013,
        0x2014, 0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x9D, 0x017E, 0x0178,
    ];
    if (0x80..0xA0).contains(&b) {
        char::from_u32(HIGH[(b - 0x80) as usize] as u32).unwrap_or('\u{FFFD}')
    } else {
        b as char
    }
}

/// The file's text: UTF-8 without a BOM, or Windows-1252.
pub fn decode(data: &[u8]) -> (std::borrow::Cow<'_, str>, bool) {
    let data = data.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(data);
    match std::str::from_utf8(data) {
        Ok(s) => (std::borrow::Cow::Borrowed(s), false),
        Err(_) => (std::borrow::Cow::Owned(data.iter().map(|&b| cp1252(b)).collect()), true),
    }
}

/// Reads CSV `data` into `sheet` from its top left, replacing what is there.
pub fn read(wb: &mut Workbook, sheet: u16, data: &[u8], opts: CsvOptions) -> Result<CsvStats, CsvError> {
    let (text, latin) = decode(data);
    let sep = opts.separator.unwrap_or_else(|| detect_separator(&text));
    let mut st = CsvStats { separator: sep, latin, ..Default::default() };
    let b = text.as_bytes();
    let (mut i, mut row, mut col) = (0usize, 0u32, 0u32);
    let mut field = String::new();
    let mut max_col = 0u32;
    let mut formulas: Vec<(u32, u32, String)> = Vec::new();
    let mut at_line_start = true;
    while i < b.len() {
        // One field.
        field.clear();
        let mut quoted_field = false;
        if b[i] == b'"' {
            quoted_field = true;
            i += 1;
            let start = i;
            let mut seg = start;
            loop {
                if i >= b.len() {
                    field.push_str(&text[seg..i]);
                    break;
                }
                if b[i] == b'"' {
                    field.push_str(&text[seg..i]);
                    if i + 1 < b.len() && b[i + 1] == b'"' {
                        field.push('"');
                        i += 2;
                        seg = i;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            // Anything after the closing quote up to the separator is kept (lenient).
            let s = i;
            while i < b.len() && b[i] != sep && b[i] != b'\n' && b[i] != b'\r' {
                i += 1;
            }
            field.push_str(&text[s..i]);
        } else {
            let s = i;
            while i < b.len() && b[i] != sep && b[i] != b'\n' && b[i] != b'\r' {
                i += 1;
            }
            field.push_str(&text[s..i]);
        }
        let blank_line = at_line_start && field.is_empty() && !quoted_field && (i >= b.len() || b[i] == b'\n' || b[i] == b'\r');
        at_line_start = false;
        if !blank_line {
            if row >= MAX_ROWS {
                return Err(CsvError::TooManyRows);
            }
            if col >= MAX_COLS {
                return Err(CsvError::TooManyColumns);
            }
            if field.len() > MAX_TEXT {
                st.cut += 1;
            }
            store(wb, sheet, row, col, &field, quoted_field, opts.formulas, &mut formulas);
            if !field.is_empty() {
                st.cells += 1;
            }
            max_col = max_col.max(col + 1);
        }
        // What ended it.
        if i < b.len() && b[i] == sep {
            i += 1;
            col += 1;
            // A separator at the very end of a line leaves an empty last field.
            if i >= b.len() || b[i] == b'\n' || b[i] == b'\r' {
                if col >= MAX_COLS {
                    return Err(CsvError::TooManyColumns);
                }
                max_col = max_col.max(col + 1);
            }
            continue;
        }
        if i < b.len() && b[i] == b'\r' {
            i += 1;
            if i < b.len() && b[i] == b'\n' {
                i += 1;
            }
        } else if i < b.len() && b[i] == b'\n' {
            i += 1;
        }
        if !blank_line {
            row += 1;
        }
        col = 0;
        at_line_start = true;
    }
    if !at_line_start {
        row += 1;
    }
    st.rows = row;
    st.cols = max_col;
    for (r, c, f) in formulas {
        if wb.set_formula(crate::addr::CellAddr::new(sheet, r, c), &f).is_err() {
            wb.store(sheet, r, c, &Value::str(&f));
        }
    }
    if st.rows > 0 && st.cols > 0 {
        wb.changed.push(Rect { sheet, r0: 0, c0: 0, r1: st.rows - 1, c1: st.cols - 1 });
    }
    Ok(st)
}

#[allow(clippy::too_many_arguments)]
fn store(wb: &mut Workbook, sheet: u16, row: u32, col: u32, f: &str, quoted: bool, formulas: bool, later: &mut Vec<(u32, u32, String)>) {
    let cell = if f.is_empty() {
        Cell::Empty
    } else if quoted {
        // Quoted: text as written (Excel still reads quoted numbers as numbers).
        match parse_number(f) {
            Some(x) => Cell::Num(x),
            None => Cell::Str(wb.strings.intern(f)),
        }
    } else if let Some(x) = parse_number(f) {
        Cell::Num(x)
    } else if f.eq_ignore_ascii_case("TRUE") {
        Cell::Bool(true)
    } else if f.eq_ignore_ascii_case("FALSE") {
        Cell::Bool(false)
    } else if let Some(e) = ErrorKind::from_text(f) {
        Cell::Err(e)
    } else if formulas && f.starts_with('=') && f.len() > 1 {
        later.push((row, col, f.to_string()));
        return;
    } else {
        Cell::Str(wb.strings.intern(f))
    };
    wb.clear_formulas_in(sheet, col, row, row);
    wb.store_cell(sheet, row, col, cell);
}

/// Writes `sheet`'s values (formulas as their results) as CSV with `sep`, up to its
/// last used row and column.
pub fn write<W: Write>(wb: &Workbook, sheet: u16, sep: u8, mut out: W) -> io::Result<W> {
    let (rows, cols) = wb.extent(sheet);
    let mut line = Vec::new();
    for r in 0..rows {
        line.clear();
        // Trailing empty cells of a row are not written.
        let mut vals: Vec<Value> = (0..cols).map(|c| wb.value(sheet, r, c)).collect();
        while vals.last().is_some_and(Value::is_empty) {
            vals.pop();
        }
        for (k, v) in vals.iter().enumerate() {
            if k > 0 {
                line.push(sep);
            }
            let s = v.display();
            let quote = s.bytes().any(|b| b == sep || b == b'"' || b == b'\n' || b == b'\r') || s.starts_with(' ') || s.ends_with(' ');
            if quote {
                line.push(b'"');
                for b in s.bytes() {
                    if b == b'"' {
                        line.push(b'"');
                    }
                    line.push(b);
                }
                line.push(b'"');
            } else {
                line.extend_from_slice(s.as_bytes());
            }
        }
        line.extend_from_slice(b"\r\n");
        out.write_all(&line)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::addr::CellAddr;

    fn book() -> Workbook {
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        wb
    }

    fn get(wb: &Workbook, r: u32, c: u32) -> Value {
        wb.get(CellAddr::new(0, r, c))
    }

    #[test]
    fn reads_the_usual_shapes() {
        let mut wb = book();
        let st = read(&mut wb, 0, b"\xEF\xBB\xBFname,amount,ok\r\n\"Smith, J\",12.5,TRUE\n\"say \"\"hi\"\"\",-3e2,\n\"two\nlines\",12%,#N/A\n", CsvOptions::default()).unwrap();
        assert_eq!((st.rows, st.cols, st.separator), (4, 3, b','));
        assert_eq!(get(&wb, 0, 0), Value::str("name"));
        assert_eq!(get(&wb, 1, 0), Value::str("Smith, J"));
        assert_eq!(get(&wb, 1, 1), Value::Num(12.5));
        assert_eq!(get(&wb, 1, 2), Value::Bool(true));
        assert_eq!(get(&wb, 2, 0), Value::str("say \"hi\""));
        assert_eq!(get(&wb, 2, 1), Value::Num(-300.0));
        assert_eq!(get(&wb, 2, 2), Value::Empty);
        assert_eq!(get(&wb, 3, 0), Value::str("two\nlines"));
        assert_eq!(get(&wb, 3, 1), Value::Num(0.12));
        assert_eq!(get(&wb, 3, 2), Value::Err(ErrorKind::NA));
    }

    #[test]
    fn detects_semicolons_and_tabs_and_reads_ansi() {
        let mut wb = book();
        let st = read(&mut wb, 0, b"a;b;c\n1,5;2;3\n", CsvOptions::default()).unwrap();
        assert_eq!(st.separator, b';');
        assert_eq!(get(&wb, 1, 0), Value::str("1,5"), "a decimal comma stays text (no locale yet)");
        let mut wb = book();
        let st = read(&mut wb, 0, b"x\ty\n\x80 5\tcaf\xe9\n", CsvOptions::default()).unwrap();
        assert_eq!(st.separator, b'\t');
        assert!(st.latin);
        assert_eq!(get(&wb, 1, 0), Value::str("€ 5"));
        assert_eq!(get(&wb, 1, 1), Value::str("café"));
    }

    #[test]
    fn formulas_are_text_unless_asked() {
        let mut wb = book();
        read(&mut wb, 0, b"1,2,=A1+B1,=WEBSERVICE(\"http://x\")\n", CsvOptions::default()).unwrap();
        wb.recalc();
        assert_eq!(get(&wb, 0, 2), Value::str("=A1+B1"));
        assert!(wb.formula_text(CellAddr::new(0, 0, 2)).is_none());
        let mut wb = book();
        read(&mut wb, 0, b"1,2,=A1+B1\n", CsvOptions { formulas: true, ..Default::default() }).unwrap();
        wb.recalc();
        assert_eq!(get(&wb, 0, 2), Value::Num(3.0));
    }

    #[test]
    fn writes_and_reads_back() {
        let mut wb = book();
        for (a, s) in [((0, 0), "plain"), ((0, 1), "a,b"), ((0, 2), "quote \" in"), ((1, 0), "=1/3"), ((1, 1), "TRUE"), ((2, 3), " padded "), ((3, 0), "line\nbreak")] {
            wb.set_input(CellAddr::new(0, a.0, a.1), s).unwrap();
        }
        wb.recalc();
        let out = write(&wb, 0, b',', Vec::new()).unwrap();
        let text = String::from_utf8(out.clone()).unwrap();
        assert!(text.starts_with("plain,\"a,b\",\"quote \"\" in\"\r\n0.333333333333333,TRUE\r\n"), "{text}");
        let mut back = book();
        let st = read(&mut back, 0, &out, CsvOptions::default()).unwrap();
        assert_eq!((st.rows, st.cols), (4, 4));
        for r in 0..4 {
            for c in 0..4 {
                assert_eq!(get(&back, r, c).display(), get(&wb, r, c).display(), "({r}, {c}): numbers keep the 15 digits General shows");
            }
        }
    }

    #[test]
    fn edge_cases() {
        let mut wb = book();
        let st = read(&mut wb, 0, b"", CsvOptions::default()).unwrap();
        assert_eq!((st.rows, st.cols), (0, 0));
        let st = read(&mut wb, 0, b"a,b,\n\n\nc\r", CsvOptions::default()).unwrap();
        assert_eq!((st.rows, st.cols), (2, 3), "blank lines skipped, trailing separator counts");
        assert_eq!(get(&wb, 1, 0), Value::str("c"));
        let st = read(&mut wb, 0, b"\"unterminated,1\n2", CsvOptions::default()).unwrap();
        assert_eq!(st.rows, 1);
        assert_eq!(get(&wb, 0, 0), Value::str("unterminated,1\n2"));
    }

    #[test]
    fn garbage_never_panics() {
        let mut g = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..3000 {
            let n = (g % 200) as usize;
            let mut b = Vec::with_capacity(n);
            for _ in 0..n {
                g ^= g << 13;
                g ^= g >> 7;
                g ^= g << 17;
                b.push(b"a1,;\t\"\r\n=e.%-"[(g % 13) as usize]);
            }
            let mut wb = book();
            let _ = read(&mut wb, 0, &b, CsvOptions { separator: None, formulas: g % 2 == 0 });
            wb.recalc();
            let _ = write(&wb, 0, b',', Vec::new());
        }
    }
}
