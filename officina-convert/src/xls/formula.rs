//! BIFF8 parsed formulas (MS-XLS §2.5.198, Rgce) back to the text Excel
//! shows: the tokens are a postfix program, run here over a stack of
//! strings. The text then goes through the same parser as `.xlsx` formulas.
//!
//! References are rendered at the cell the formula is in, so a shared
//! formula's relative tokens (`PtgRefN`, `PtgAreaN`) become each cell's own
//! text. Every read is checked; a token this can't render is an error that
//! keeps the cell's saved value.

use super::biff::{Cur, Short};
use super::ftab;

/// BIFF8's grid.
pub const ROWS: u32 = 65_536;
pub const COLS: u32 = 256;
/// Stack depth and text length a formula may reach.
const MAX_DEPTH: usize = 1024;
/// Excel's own limit on a formula's length.
const MAX_TEXT: usize = 8192;

/// What a supporting link (`SUPBOOK`) points at.
#[derive(Debug, Clone, PartialEq)]
pub enum Book {
    /// This workbook.
    Own,
    /// Add-in functions, named in its `EXTERNNAME`s.
    AddIn,
    /// Another file (or DDE/OLE): never followed.
    Other,
}

#[derive(Debug, Clone)]
pub struct SupBook {
    pub book: Book,
    pub names: Vec<String>,
}

/// What formulas name: the workbook's sheets by tab index (`None` for tabs
/// that aren't worksheets here), its defined names, `EXTERNSHEET` entries
/// and supporting links.
#[derive(Debug, Default)]
pub struct Ctx {
    pub sheets: Vec<Option<String>>,
    pub names: Vec<String>,
    pub xti: Vec<(u16, i16, i16)>,
    pub books: Vec<SupBook>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    Damaged,
    /// A construct kept as the cell's value, in words.
    Unsupported(&'static str),
}

impl From<Short> for Why {
    fn from(_: Short) -> Self {
        Why::Damaged
    }
}

/// The master cell a `PtgExp` (shared or array formula) or `PtgTbl` (data
/// table) points at, when that is the formula's only token.
pub fn exp_of(rgce: &[u8]) -> Option<(bool, u32, u32)> {
    if rgce.len() == 5 && (rgce[0] == 0x01 || rgce[0] == 0x02) {
        let r = u16::from_le_bytes([rgce[1], rgce[2]]) as u32;
        let c = u16::from_le_bytes([rgce[3], rgce[4]]) as u32;
        return Some((rgce[0] == 0x02, r, c));
    }
    None
}

/// Column letters.
fn col_name(mut c: u32, out: &mut String) {
    let mut b = [0u8; 4];
    let mut n = 0;
    loop {
        b[n] = b'A' + (c % 26) as u8;
        n += 1;
        if c < 26 {
            break;
        }
        c = c / 26 - 1;
    }
    for i in (0..n).rev() {
        out.push(b[i] as char);
    }
}

/// One end of a reference: row and column with their relative flags,
/// relative offsets already applied.
#[derive(Clone, Copy)]
struct End {
    row: u32,
    col: u32,
    row_rel: bool,
    col_rel: bool,
}

impl End {
    /// `rw`, `col` as stored: absolute (`PtgRef`) or, for `n`, offsets from
    /// the formula's cell (`PtgRefN`).
    fn of(rw: u16, colw: u16, n: bool, at: (u32, u32)) -> End {
        let row_rel = colw & 0x8000 != 0;
        let col_rel = colw & 0x4000 != 0;
        let mut row = rw as u32;
        let mut col = (colw & 0x3FFF) as u32 % COLS;
        if n && row_rel {
            row = (at.0 as i64 + rw as i16 as i64).rem_euclid(ROWS as i64) as u32;
        }
        if n && col_rel {
            col = (at.1 as i64 + (colw & 0xFF) as u8 as i8 as i64).rem_euclid(COLS as i64) as u32;
        }
        End { row, col, row_rel, col_rel }
    }
    fn cell(&self, out: &mut String) {
        if !self.col_rel {
            out.push('$');
        }
        col_name(self.col, out);
        if !self.row_rel {
            out.push('$');
        }
        out.push_str(&(self.row + 1).to_string());
    }
}

fn area(a: End, b: End, out: &mut String) {
    // Whole columns and whole rows as Excel writes them.
    if a.row == 0 && b.row == ROWS - 1 && !(a.col == 0 && b.col == COLS - 1) {
        for (i, e) in [a, b].iter().enumerate() {
            if i == 1 {
                out.push(':');
            }
            if !e.col_rel {
                out.push('$');
            }
            col_name(e.col, out);
        }
        return;
    }
    if a.col == 0 && b.col == COLS - 1 {
        for (i, e) in [a, b].iter().enumerate() {
            if i == 1 {
                out.push(':');
            }
            if !e.row_rel {
                out.push('$');
            }
            out.push_str(&(e.row + 1).to_string());
        }
        return;
    }
    a.cell(out);
    if (a.row, a.col, a.row_rel, a.col_rel) != (b.row, b.col, b.row_rel, b.col_rel) {
        out.push(':');
        b.cell(out);
    }
}

/// A sheet name as a formula writes it: quoted unless it is a plain word
/// that can't be read as anything else.
pub fn sheet_ref(name: &str) -> String {
    let plain = name.chars().next().is_some_and(|c| c.is_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.')
        && officina_grid::addr::parse_a1(name).is_none()
        && !looks_like_cell(name);
    if plain {
        name.to_string()
    } else {
        format!("'{}'", name.replace('\'', "''"))
    }
}

/// `R1C1`-like or `A1`-like words (`TRUE` too) read as something else.
fn looks_like_cell(s: &str) -> bool {
    let u = s.to_ascii_uppercase();
    if u == "TRUE" || u == "FALSE" {
        return true;
    }
    let b = u.as_bytes();
    let letters = b.iter().take_while(|c| c.is_ascii_alphabetic()).count();
    letters > 0 && letters <= 3 && letters < b.len() && b[letters..].iter().all(u8::is_ascii_digit)
}

/// Precedence of what is on the stack, higher binds tighter.
const P_ATOM: u8 = 100;
const P_RANGE: u8 = 95;
const P_ISECT: u8 = 94;
const P_UNION: u8 = 93;
const P_PERCENT: u8 = 90;
const P_UNARY: u8 = 85;
const P_POW: u8 = 80;
const P_MUL: u8 = 70;
const P_ADD: u8 = 60;
const P_CAT: u8 = 50;
const P_CMP: u8 = 40;

fn wrap(t: &(String, u8), above: u8) -> String {
    if t.1 < above {
        format!("({})", t.0)
    } else {
        t.0.clone()
    }
}

fn err_text(e: u8) -> Option<&'static str> {
    Some(match e {
        0x00 => "#NULL!",
        0x07 => "#DIV/0!",
        0x0F => "#VALUE!",
        0x17 => "#REF!",
        0x1D => "#NAME?",
        0x24 => "#NUM!",
        0x2A => "#N/A",
        0x2B => "#GETTING_DATA",
        _ => return None,
    })
}

/// A number as formula text: exponent form where plain digits would run
/// long.
pub fn num_text(x: f64) -> String {
    let a = x.abs();
    if a != 0.0 && !(1e-5..1e15).contains(&a) {
        format!("{x:E}")
    } else {
        format!("{x}")
    }
}

fn str_lit(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// The prefix of a reference through `EXTERNSHEET` entry `ixti`: `Sheet1!`,
/// `'A b'!`, `Sheet1:Sheet3!`, or an error for other files and deleted sheets.
fn xti_prefix(ctx: &Ctx, ixti: u16) -> Result<String, Why> {
    let &(book, first, last) = ctx.xti.get(ixti as usize).ok_or(Why::Damaged)?;
    match ctx.books.get(book as usize).map(|b| &b.book) {
        Some(Book::Own) => {}
        Some(_) => return Err(Why::Unsupported("references to other workbooks")),
        None => return Err(Why::Damaged),
    }
    if first < 0 || last < 0 {
        return Ok("#REF!".into());
    }
    let name = |t: i16| -> Result<&String, Why> {
        match ctx.sheets.get(t as usize) {
            Some(Some(n)) => Ok(n),
            Some(None) => Err(Why::Unsupported("references to chart or macro sheets")),
            None => Err(Why::Damaged),
        }
    };
    let a = name(first)?;
    if first == last {
        Ok(format!("{}!", sheet_ref(a)))
    } else {
        let b = name(last)?;
        let both = format!("{a}:{b}");
        let plain = sheet_ref(a) == *a && sheet_ref(b) == *b;
        Ok(if plain { format!("{both}!") } else { format!("'{}'!", both.replace('\'', "''")) })
    }
}

/// Array constants (`PtgArray`'s data in `rgcb`, MS-XLS §2.5.198.8).
fn array(cb: &mut Cur) -> Result<String, Why> {
    let cols = cb.u8()? as usize + 1;
    let rows = cb.u16()? as usize + 1;
    if rows * cols > cb.left() {
        return Err(Why::Damaged);
    }
    let mut s = String::from("{");
    for r in 0..rows {
        if r > 0 {
            s.push(';');
        }
        for c in 0..cols {
            if c > 0 {
                s.push(',');
            }
            match cb.u8()? {
                0x00 => {
                    cb.skip(8)?;
                }
                0x01 => s.push_str(&num_text(cb.f64()?)),
                0x02 => s.push_str(&str_lit(&cb.xl_string()?)),
                0x04 => {
                    let b = cb.u8()?;
                    cb.skip(7)?;
                    s.push_str(if b != 0 { "TRUE" } else { "FALSE" });
                }
                0x10 => {
                    let e = cb.u8()?;
                    cb.skip(7)?;
                    s.push_str(err_text(e).ok_or(Why::Damaged)?);
                }
                _ => return Err(Why::Damaged),
            }
            if s.len() > MAX_TEXT {
                return Err(Why::Unsupported("formulas too long"));
            }
        }
    }
    s.push('}');
    Ok(s)
}

/// Strips the prefixes newer functions carry (`_xlfn.`, `_xlws.`).
fn fn_name(n: &str) -> &str {
    n.strip_prefix("_xlfn.").or_else(|| n.strip_prefix("_xlws.")).unwrap_or(n)
}

/// The formula `rgce` (with its extra data `rgcb`) in cell (`row`, `col`),
/// as text without the leading `=`. `rel3d`: 3D references are offsets from
/// the cell, as in shared formulas and names (absolute in a cell's own).
pub fn decompile(rgce: &[u8], rgcb: &[u8], row: u32, col: u32, rel3d: bool, ctx: &Ctx) -> Result<String, Why> {
    let at = (row, col);
    let mut c = Cur::of(rgce);
    let mut cb = Cur::of(rgcb);
    let mut st: Vec<(String, u8)> = Vec::new();
    let pop = |st: &mut Vec<(String, u8)>| st.pop().ok_or(Why::Damaged);
    while c.left() > 0 {
        let ptg = c.u8()?;
        if st.len() > MAX_DEPTH {
            return Err(Why::Unsupported("formulas too deep"));
        }
        // Reference tokens carry a class in bits 5–6; the base type decides.
        let base = if ptg >= 0x20 { (ptg & 0x1F) | 0x20 } else { ptg };
        match base {
            0x01 | 0x02 => return Err(Why::Damaged),
            0x03..=0x11 => {
                let b = pop(&mut st)?;
                let a = pop(&mut st)?;
                let (op, p) = match ptg {
                    0x03 => ("+", P_ADD),
                    0x04 => ("-", P_ADD),
                    0x05 => ("*", P_MUL),
                    0x06 => ("/", P_MUL),
                    0x07 => ("^", P_POW),
                    0x08 => ("&", P_CAT),
                    0x09 => ("<", P_CMP),
                    0x0A => ("<=", P_CMP),
                    0x0B => ("=", P_CMP),
                    0x0C => (">=", P_CMP),
                    0x0D => (">", P_CMP),
                    0x0E => ("<>", P_CMP),
                    0x0F => (" ", P_ISECT),
                    0x10 => (",", P_UNION),
                    _ => (":", P_RANGE),
                };
                // Left-associative: the right side needs brackets at equal
                // precedence.
                let t = format!("{}{op}{}", wrap(&a, p), wrap(&b, p + 1));
                st.push((t, p));
            }
            0x12 | 0x13 => {
                let a = pop(&mut st)?;
                let op = if ptg == 0x12 { "+" } else { "-" };
                st.push((format!("{op}{}", wrap(&a, P_UNARY)), P_UNARY));
            }
            0x14 => {
                let a = pop(&mut st)?;
                st.push((format!("{}%", wrap(&a, P_PERCENT)), P_PERCENT));
            }
            0x15 => {
                let a = pop(&mut st)?;
                st.push((format!("({})", a.0), P_ATOM));
            }
            0x16 => st.push((String::new(), P_ATOM)),
            0x17 => {
                let s = c.short_string()?;
                st.push((str_lit(&s), P_ATOM));
            }
            0x18 => return Err(Why::Unsupported("Excel 4 extended tokens")),
            0x19 => {
                let flags = c.u8()?;
                let data = c.u16()?;
                if flags & 0x04 != 0 {
                    // attrChoose: the jump table.
                    c.skip((data as usize + 1) * 2)?;
                }
                if flags & 0x10 != 0 {
                    // attrSum: SUM of the one argument.
                    let a = pop(&mut st)?;
                    st.push((format!("SUM({})", arg(&a)), P_ATOM));
                }
            }
            0x1C => {
                let e = c.u8()?;
                st.push((err_text(e).ok_or(Why::Damaged)?.into(), P_ATOM));
            }
            0x1D => {
                let b = c.u8()?;
                st.push(((if b != 0 { "TRUE" } else { "FALSE" }).into(), P_ATOM));
            }
            0x1E => st.push((c.u16()?.to_string(), P_ATOM)),
            0x1F => {
                let x = c.f64()?;
                if !x.is_finite() {
                    return Err(Why::Damaged);
                }
                // A negative literal reads back as a unary minus.
                st.push((num_text(x), if x < 0.0 { P_UNARY } else { P_ATOM }));
            }
            0x20 => {
                c.skip(7)?;
                st.push((array(&mut cb)?, P_ATOM));
            }
            0x21 | 0x22 => {
                let (argc, tab) = if base == 0x21 {
                    let tab = c.u16()?;
                    let n = match ftab::function(tab) {
                        Some((_, Some(n))) => n as usize,
                        Some((_, None)) => return Err(Why::Damaged),
                        None => return Err(Why::Unsupported("functions this file names by a number Excel doesn't use")),
                    };
                    (n, tab)
                } else {
                    let n = (c.u8()? & 0x7F) as usize;
                    (n, c.u16()? & 0x7FFF)
                };
                if st.len() < argc {
                    return Err(Why::Damaged);
                }
                let args: Vec<(String, u8)> = st.split_off(st.len() - argc);
                let (name, args) = if tab == 255 {
                    // A user-defined or newer function: its name is the first
                    // argument.
                    let (first, rest) = args.split_first().ok_or(Why::Damaged)?;
                    (fn_name(&first.0).to_string(), rest.to_vec())
                } else {
                    let (n, _) = ftab::function(tab).ok_or(Why::Unsupported("functions this file names by a number Excel doesn't use"))?;
                    (n.to_string(), args)
                };
                let list: Vec<String> = args.iter().map(arg).collect();
                st.push((format!("{name}({})", list.join(",")), P_ATOM));
            }
            0x23 => {
                let i = c.u32()? as usize;
                let n = ctx.names.get(i.wrapping_sub(1)).ok_or(Why::Damaged)?;
                st.push((n.clone(), P_ATOM));
            }
            0x24 | 0x2C => {
                let rw = c.u16()?;
                let cl = c.u16()?;
                let mut s = String::new();
                End::of(rw, cl, base == 0x2C, at).cell(&mut s);
                st.push((s, P_ATOM));
            }
            0x25 | 0x2D => {
                let (r0, r1, c0, c1) = (c.u16()?, c.u16()?, c.u16()?, c.u16()?);
                let n = base == 0x2D;
                let mut s = String::new();
                area(End::of(r0, c0, n, at), End::of(r1, c1, n, at), &mut s);
                st.push((s, P_ATOM));
            }
            0x26..=0x28 => {
                // The reference expression that follows is what counts; a
                // PtgMemArea's cached rectangles sit in rgcb.
                c.skip(6)?;
                if base == 0x26 {
                    let n = cb.u16()? as usize;
                    cb.skip(n * 8)?;
                }
            }
            0x29 => c.skip(2)?,
            0x2A => {
                c.skip(4)?;
                st.push(("#REF!".into(), P_ATOM));
            }
            0x2B => {
                c.skip(8)?;
                st.push(("#REF!".into(), P_ATOM));
            }
            0x39 => {
                let ixti = c.u16()?;
                let i = c.u32()? as usize;
                let &(book, _, _) = ctx.xti.get(ixti as usize).ok_or(Why::Damaged)?;
                let b = ctx.books.get(book as usize).ok_or(Why::Damaged)?;
                let n = match b.book {
                    Book::Own => ctx.names.get(i.wrapping_sub(1)),
                    Book::AddIn => b.names.get(i.wrapping_sub(1)),
                    Book::Other => return Err(Why::Unsupported("names in other workbooks")),
                };
                st.push((n.ok_or(Why::Damaged)?.clone(), P_ATOM));
            }
            0x3A | 0x3B | 0x3C | 0x3D => {
                let ixti = c.u16()?;
                let pre = xti_prefix(ctx, ixti)?;
                let mut s = String::new();
                if base == 0x3A || base == 0x3C {
                    let (rw, cl) = (c.u16()?, c.u16()?);
                    if base == 0x3C || pre == "#REF!" {
                        s.push_str("#REF!");
                    } else {
                        s.push_str(&pre);
                        End::of(rw, cl, rel3d, at).cell(&mut s);
                    }
                } else {
                    let (r0, r1, c0, c1) = (c.u16()?, c.u16()?, c.u16()?, c.u16()?);
                    if base == 0x3D || pre == "#REF!" {
                        s.push_str("#REF!");
                    } else {
                        s.push_str(&pre);
                        area(End::of(r0, c0, rel3d, at), End::of(r1, c1, rel3d, at), &mut s);
                    }
                }
                st.push((s, P_ATOM));
            }
            _ => return Err(Why::Damaged),
        }
        if st.last().is_some_and(|t| t.0.len() > MAX_TEXT) {
            return Err(Why::Unsupported("formulas too long"));
        }
    }
    if st.len() != 1 {
        return Err(Why::Damaged);
    }
    Ok(st.pop().unwrap().0)
}

/// An argument: a union needs its own brackets inside a function's list.
fn arg(a: &(String, u8)) -> String {
    if a.1 == P_UNION {
        format!("({})", a.0)
    } else {
        a.0.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Ctx {
        Ctx {
            sheets: vec![Some("Sheet1".into()), Some("My Data".into()), None],
            names: vec!["Rate".into(), "_xlfn.IFERROR".into()],
            xti: vec![(0, 0, 0), (0, 1, 1), (0, 0, 1), (1, -2, -2)],
            books: vec![SupBook { book: Book::Own, names: vec![] }, SupBook { book: Book::AddIn, names: vec!["MYFUNC".into()] }],
        }
    }

    fn rc(r: u16, c: u16) -> Vec<u8> {
        [r.to_le_bytes(), (c | 0xC000).to_le_bytes()].concat()
    }

    #[test]
    fn arithmetic_and_precedence() {
        // (A1+B1)*2 written without a PtgParen still gets its brackets.
        let mut f = vec![0x24];
        f.extend(rc(0, 0));
        f.push(0x24);
        f.extend(rc(0, 1));
        f.push(0x03);
        f.extend([0x1E, 2, 0, 0x05]);
        assert_eq!(decompile(&f, &[], 5, 5, false, &ctx()).unwrap(), "(A1+B1)*2");
        // -2^2 stays as written; 1-(2-3) keeps its brackets.
        let f = [0x1E, 2, 0, 0x13, 0x1E, 2, 0, 0x07];
        assert_eq!(decompile(&f, &[], 0, 0, false, &ctx()).unwrap(), "-2^2");
        let f = [0x1E, 1, 0, 0x1E, 2, 0, 0x1E, 3, 0, 0x04, 0x04];
        assert_eq!(decompile(&f, &[], 0, 0, false, &ctx()).unwrap(), "1-(2-3)");
    }

    #[test]
    fn functions_names_and_sheets() {
        // SUM(A1:B2) via PtgFuncVar, absolute area.
        let mut f = vec![0x25, 0, 0, 1, 0, 0, 0, 1, 0];
        f.extend([0x22, 1, 4, 0]);
        assert_eq!(decompile(&f, &[], 0, 0, false, &ctx()).unwrap(), "SUM($A$1:$B$2)");
        // ROUND(Rate,2) via PtgFunc.
        let f = [0x23, 1, 0, 0, 0, 0x1E, 2, 0, 0x21, 27, 0];
        assert_eq!(decompile(&f, &[], 0, 0, false, &ctx()).unwrap(), "ROUND(Rate,2)");
        // 'My Data'!C3 and Sheet1:'My Data' 3D.
        let mut f = vec![0x3A, 1, 0];
        f.extend([2, 0, 2, 0]);
        assert_eq!(decompile(&f, &[], 0, 0, false, &ctx()).unwrap(), "'My Data'!$C$3");
        let mut f = vec![0x3A, 2, 0];
        f.extend([2, 0, 2, 0]);
        assert_eq!(decompile(&f, &[], 0, 0, false, &ctx()).unwrap(), "'Sheet1:My Data'!$C$3");
        // IFERROR through a hidden name and function 255.
        let f = [0x23, 2, 0, 0, 0, 0x1E, 1, 0, 0x1E, 0, 0, 0x22, 3, 0xFF, 0];
        assert_eq!(decompile(&f, &[], 0, 0, false, &ctx()).unwrap(), "IFERROR(1,0)");
        // An add-in function through PtgNameX.
        let f = [0x39, 3, 0, 1, 0, 0, 0, 0x22, 1, 0xFF, 0];
        assert_eq!(decompile(&f, &[], 0, 0, false, &ctx()).unwrap(), "MYFUNC()");
    }

    #[test]
    fn relative_tokens_and_whole_columns() {
        // RefN one row up, same column, from C5 → C4; AreaN A:A.
        let f = [0x2C, 0xFF, 0xFF, 0x00, 0xC0];
        assert_eq!(decompile(&f, &[], 4, 2, false, &ctx()).unwrap(), "C4");
        let f = [0x25, 0, 0, 0xFF, 0xFF, 0, 0x40, 0, 0x40];
        assert_eq!(decompile(&f, &[], 0, 0, false, &ctx()).unwrap(), "A:A");
        let f = [0x25, 0, 0, 0, 0, 0, 0, 0xFF, 0];
        assert_eq!(decompile(&f, &[], 0, 0, false, &ctx()).unwrap(), "$1:$1");
    }

    #[test]
    fn constants() {
        // {1,"a";TRUE,#N/A}
        let mut cb = vec![1, 1, 0];
        cb.push(1);
        cb.extend(1.0f64.to_le_bytes());
        cb.extend([2, 1, 0, 0, b'a']);
        cb.extend([4, 1, 0, 0, 0, 0, 0, 0, 0]);
        cb.extend([0x10, 0x2A, 0, 0, 0, 0, 0, 0, 0]);
        let f = [0x40, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(decompile(&f, &cb, 0, 0, false, &ctx()).unwrap(), "{1,\"a\";TRUE,#N/A}");
        let f = [0x17, 3, 0, b'a', b'"', b'b'];
        assert_eq!(decompile(&f, &[], 0, 0, false, &ctx()).unwrap(), "\"a\"\"b\"");
        assert_eq!(num_text(1e300), "1E300");
        assert_eq!(num_text(0.5), "0.5");
    }

    #[test]
    fn hostile_programs_fail_cleanly() {
        for n in 0..300u32 {
            let f: Vec<u8> = (0..(n % 23)).map(|i| ((i * 7 + n * 13) % 256) as u8).collect();
            let cb: Vec<u8> = (0..(n % 17)).map(|i| ((i * 31 + n) % 256) as u8).collect();
            let _ = decompile(&f, &cb, n % 65536, n % 256, false, &ctx());
        }
        assert_eq!(sheet_ref("Sheet1"), "Sheet1");
        assert_eq!(sheet_ref("A1"), "'A1'");
        assert_eq!(sheet_ref("it's"), "'it''s'");
    }
}
