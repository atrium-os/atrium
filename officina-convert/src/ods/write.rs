//! `.ods` out (ODF 1.3 spreadsheet), from our own model (runs in the document
//! process).
//!
//! Writes values (numbers, text with its spaces, tabs and line breaks, booleans,
//! dates and times typed by their cell's format), formulas as OpenFormula with their
//! cached results, column widths, row heights, and cell styles: Excel format codes
//! become ODF data styles (sections as `style:map`s), fonts, fills, borders,
//! alignment and wrap. A whole-column style is the column's default cell style.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::io::{self, Write};

use officina_grid::addr::Rect;
use officina_grid::eval::dates;
use officina_grid::formula::{self, Expr};
use officina_grid::numfmt::PALETTE;
use officina_grid::filter::Cond;
use officina_grid::style::{HAlign, Line, Style, StyleId, VAlign};
use officina_grid::{ErrorKind, Value, Workbook};

use crate::zip::ZipWriter;

/// Rows an `.ods` sheet holds (LibreOffice's limit).
pub const ODS_ROWS: u32 = 1 << 20;
pub const ODS_COLS: u32 = 1 << 14;
/// Styled empty cells written as cells of their own, per sheet, at most.
const MAX_EXTRA: usize = 4_000_000;

const MIME: &str = "application/vnd.oasis.opendocument.spreadsheet";

const NS: &str = concat!(
    r#"xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" "#,
    r#"xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0" "#,
    r#"xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0" "#,
    r#"xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0" "#,
    r#"xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0" "#,
    r#"xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0" "#,
    r#"xmlns:svg="urn:oasis:names:tc:opendocument:xmlns:svg-compatible:1.0" "#,
    r#"xmlns:draw="urn:oasis:names:tc:opendocument:xmlns:drawing:1.0" "#,
    r#"xmlns:xlink="http://www.w3.org/1999/xlink" "#,
    r#"xmlns:of="urn:oasis:names:tc:opendocument:xmlns:of:1.2" "#,
    r#"xmlns:calcext="urn:org:documentfoundation:names:experimental:calc:xmlns:calcext:1.0" "#,
    r#"xmlns:loext="urn:org:documentfoundation:names:experimental:office:xmlns:loext:1.0" "#,
    r#"office:version="1.3""#
);

fn esc(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            // XML 1.0 can't hold the other control characters.
            c if (c as u32) < 0x20 && c != '\t' && c != '\n' => {}
            c => out.push(c),
        }
    }
}

/// A number as `xsd:double`, shortest round trip.
fn num(x: f64) -> String {
    if x == x.trunc() && x.abs() < 1e15 {
        format!("{}", x as i64)
    } else {
        format!("{x:?}")
    }
}

/// Text as `text:p` paragraphs: one per line, runs of spaces as `text:s`, tabs as
/// `text:tab` (ODF collapses plain whitespace).
fn paragraphs(out: &mut String, s: &str) {
    for line in s.split('\n') {
        out.push_str("<text:p>");
        let mut spaces = 0usize;
        let mut start = true;
        let flush = |out: &mut String, n: &mut usize, start: bool| {
            if *n == 0 {
                return;
            }
            let (lit, rest) = if start { (0, *n) } else { (1, *n - 1) };
            if lit == 1 {
                out.push(' ');
            }
            if rest == 1 {
                out.push_str("<text:s/>");
            } else if rest > 1 {
                let _ = write!(out, r#"<text:s text:c="{rest}"/>"#);
            }
            *n = 0;
        };
        for c in line.chars() {
            match c {
                ' ' => spaces += 1,
                '\t' => {
                    flush(out, &mut spaces, start);
                    out.push_str("<text:tab/>");
                    start = false;
                }
                '\r' => {}
                c => {
                    flush(out, &mut spaces, start);
                    start = false;
                    let mut b = [0u8; 4];
                    esc(out, c.encode_utf8(&mut b));
                }
            }
        }
        // Trailing spaces: all as text:s.
        if spaces > 0 {
            flush(out, &mut spaces, true);
        }
        out.push_str("</text:p>");
    }
}

fn len_in(px: f64) -> String {
    format!("{:.4}in", px / 96.0)
}

fn hex(c: u32) -> String {
    format!("#{:06x}", c & 0xFF_FFFF)
}

// ---------------------------------------------------------------------------
// Excel format codes as ODF data styles.

/// Splits a code into sections at `;` outside quotes and escapes.
fn sections(code: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut q = false;
    let mut esc_next = false;
    for c in code.chars() {
        let cur = out.last_mut().unwrap();
        if esc_next {
            cur.push(c);
            esc_next = false;
            continue;
        }
        match c {
            '"' => {
                q = !q;
                cur.push(c);
            }
            '\\' if !q => {
                cur.push(c);
                esc_next = true;
            }
            ';' if !q => out.push(String::new()),
            c => cur.push(c),
        }
    }
    out
}

/// One section: its colour, its condition (`<0`, `>=100`) and its body.
struct Section {
    color: Option<u32>,
    cond: Option<String>,
    body: String,
}

fn color_named(n: &str) -> Option<u32> {
    let l = n.to_ascii_lowercase();
    Some(match l.as_str() {
        "black" => 0x000000,
        "white" => 0xFFFFFF,
        "red" => 0xFF0000,
        "green" => 0x00FF00,
        "blue" => 0x0000FF,
        "yellow" => 0xFFFF00,
        "magenta" => 0xFF00FF,
        "cyan" => 0x00FFFF,
        _ => {
            let k: usize = l.strip_prefix("color")?.parse().ok()?;
            *PALETTE.get(k.checked_sub(1)?)?
        }
    })
}

fn section(s: &str) -> Section {
    let mut sec = Section { color: None, cond: None, body: String::new() };
    let mut rest = s;
    // Leading bracketed parts: colours and conditions (elapsed [h] and locale [$..]
    // stay in the body).
    while let Some(r) = rest.strip_prefix('[') {
        let Some(end) = r.find(']') else { break };
        let inner = &r[..end];
        if let Some(c) = color_named(inner) {
            sec.color = Some(c);
        } else if inner.starts_with(['<', '>', '=']) {
            sec.cond = Some(inner.to_string());
        } else {
            break;
        }
        rest = &r[end + 1..];
    }
    sec.body = rest.to_string();
    sec
}

/// A body broken into literal text and the rest, with quotes and escapes resolved:
/// `(true, text)` is literal.
fn pieces(body: &str) -> Vec<(bool, String)> {
    let mut out: Vec<(bool, String)> = Vec::new();
    let push = |out: &mut Vec<(bool, String)>, lit: bool, c: &str| match out.last_mut() {
        Some((l, s)) if *l == lit => s.push_str(c),
        _ => out.push((lit, c.to_string())),
    };
    let ch: Vec<char> = body.chars().collect();
    let mut i = 0;
    while i < ch.len() {
        let c = ch[i];
        match c {
            '"' => {
                let mut j = i + 1;
                let mut t = String::new();
                while j < ch.len() && ch[j] != '"' {
                    t.push(ch[j]);
                    j += 1;
                }
                push(&mut out, true, &t);
                i = j + 1;
            }
            '\\' => {
                if let Some(&d) = ch.get(i + 1) {
                    push(&mut out, true, &d.to_string());
                }
                i += 2;
            }
            '_' => {
                // A blank as wide as the next character.
                out.push((true, format!("\u{0}{}", ch.get(i + 1).copied().unwrap_or(' '))));
                out.push((false, String::new()));
                i += 2;
            }
            '*' => i += 2, // repeat-fill: not in ODF
            'E' | 'e' if matches!(ch.get(i + 1), Some('+' | '-')) && i > 0 && matches!(ch[i - 1], '0' | '#' | '?' | '.') => {
                // An exponent: E+ / E- stay with the placeholders.
                let e: String = ch[i..i + 2].iter().collect();
                push(&mut out, false, &e);
                i += 2;
            }
            '[' => {
                let end = ch[i..].iter().position(|&d| d == ']').map(|p| i + p);
                match end {
                    Some(e) => {
                        let inner: String = ch[i + 1..e].iter().collect();
                        if let Some(sym) = inner.strip_prefix('$') {
                            // [$€-407]: the symbol part.
                            push(&mut out, true, sym.split('-').next().unwrap_or(""));
                        } else {
                            push(&mut out, false, &format!("[{inner}]"));
                        }
                        i = e + 1;
                    }
                    None => i += 1,
                }
            }
            '$' | '-' | '+' | '(' | ')' | ':' | '^' | '\'' | '{' | '}' | '<' | '>' | '=' | ' ' | '!' | '&' | '~' => {
                push(&mut out, true, &c.to_string());
                i += 1;
            }
            c => {
                push(&mut out, false, &c.to_string());
                i += 1;
            }
        }
    }
    out.retain(|(l, s)| *l || !s.is_empty());
    out
}

/// `<number:text>` for literal text (a blank-width marker as LibreOffice writes it).
fn text_el(out: &mut String, t: &str) {
    if let Some(b) = t.strip_prefix('\u{0}') {
        out.push_str(r#"<number:text loext:blank-width-char=""#);
        esc(out, b);
        out.push_str(r#""> </number:text>"#);
        return;
    }
    if t.is_empty() {
        return;
    }
    out.push_str("<number:text>");
    esc(out, t);
    out.push_str("</number:text>");
}

fn is_date_body(p: &[(bool, String)]) -> bool {
    p.iter().any(|(l, s)| !l && (s.contains(['y', 'Y', 'd', 'D', 'h', 'H', 's', 'S']) || s.contains("AM/PM") || s.contains("A/P") || s.starts_with("[h") || s.starts_with("[m") || s.starts_with("[s")) && !s.contains("General"))
}

/// A date or time body: (element children, is it a date-style, elapsed hours).
fn date_children(p: &[(bool, String)]) -> (String, bool, bool) {
    // Tokens: (kind letter, run length, decimals) or literal text.
    enum T {
        Lit(String),
        Part(char, usize, usize),
        AmPm,
    }
    let mut toks: Vec<T> = Vec::new();
    let mut elapsed = false;
    for (lit, s) in p {
        if *lit {
            toks.push(T::Lit(s.clone()));
            continue;
        }
        let ch: Vec<char> = s.chars().collect();
        let mut i = 0;
        while i < ch.len() {
            let rest: String = ch[i..].iter().collect();
            let lower = ch[i].to_ascii_lowercase();
            if rest.starts_with("AM/PM") || rest.starts_with("am/pm") {
                toks.push(T::AmPm);
                i += 5;
            } else if rest.starts_with("A/P") || rest.starts_with("a/p") {
                toks.push(T::AmPm);
                i += 3;
            } else if ch[i] == '[' {
                // [h], [hh], [mm], [ss]: elapsed.
                let e = ch[i..].iter().position(|&d| d == ']').map_or(ch.len(), |p| i + p);
                let inner: String = ch[i + 1..e].iter().collect();
                if let Some(k) = inner.chars().next() {
                    elapsed = true;
                    toks.push(T::Part(k.to_ascii_lowercase(), inner.len(), 0));
                }
                i = e + 1;
            } else if "ymdhse".contains(lower) {
                let mut j = i;
                while j < ch.len() && ch[j].to_ascii_lowercase() == lower {
                    j += 1;
                }
                let mut n = j - i;
                let mut dec = 0;
                if lower == 's' && ch.get(j) == Some(&'.') {
                    let mut k = j + 1;
                    while k < ch.len() && ch[k] == '0' {
                        k += 1;
                    }
                    dec = k - j - 1;
                    j = k;
                }
                if lower == 'e' {
                    n = 4;
                }
                toks.push(T::Part(if lower == 'e' { 'y' } else { lower }, n, dec));
                i = j;
            } else {
                // Anything else in a date body ('.', '/', ',') is literal.
                toks.push(T::Lit(ch[i].to_string()));
                i += 1;
            }
        }
    }
    // m after h or before s is minutes.
    let kinds: Vec<Option<char>> = toks.iter().map(|t| if let T::Part(k, _, _) = t { Some(*k) } else { None }).collect();
    let mut out = String::new();
    let mut date = false;
    for (i, t) in toks.iter().enumerate() {
        match t {
            T::Lit(s) => text_el(&mut out, s),
            T::AmPm => out.push_str("<number:am-pm/>"),
            T::Part(k, n, dec) => {
                let long = *n >= 2;
                let st = if long { r#" number:style="long""# } else { "" };
                match k {
                    'y' => {
                        date = true;
                        let _ = write!(out, "<number:year{}/>", if *n >= 3 { r#" number:style="long""# } else { "" });
                    }
                    'm' => {
                        let prev = kinds[..i].iter().rev().flatten().next();
                        let next = kinds[i + 1..].iter().flatten().next();
                        let minute = *n <= 2 && (prev == Some(&'h') || next == Some(&'s'));
                        if minute {
                            let _ = write!(out, "<number:minutes{st}/>");
                        } else {
                            date = true;
                            match n {
                                1 => out.push_str("<number:month/>"),
                                2 => out.push_str(r#"<number:month number:style="long"/>"#),
                                3 | 5 => out.push_str(r#"<number:month number:textual="true"/>"#),
                                _ => out.push_str(r#"<number:month number:style="long" number:textual="true"/>"#),
                            }
                        }
                    }
                    'd' => {
                        date = true;
                        match n {
                            1 => out.push_str("<number:day/>"),
                            2 => out.push_str(r#"<number:day number:style="long"/>"#),
                            3 => out.push_str("<number:day-of-week/>"),
                            _ => out.push_str(r#"<number:day-of-week number:style="long"/>"#),
                        }
                    }
                    'h' => {
                        let _ = write!(out, "<number:hours{st}/>");
                    }
                    's' => {
                        if *dec > 0 {
                            let _ = write!(out, r#"<number:seconds{st} number:decimal-places="{dec}"/>"#);
                        } else {
                            let _ = write!(out, "<number:seconds{st}/>");
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    (out, date, elapsed)
}

/// A number body's children and the element they belong in.
fn number_children(p: &[(bool, String)]) -> (String, &'static str) {
    let mut out = String::new();
    let mut kind = "number:number-style";
    for (lit, s) in p {
        if *lit {
            text_el(&mut out, s);
            continue;
        }
        if s.eq_ignore_ascii_case("General") || s.to_ascii_lowercase().contains("general") {
            let (a, b) = s.split_at(s.to_ascii_lowercase().find("general").unwrap_or(0));
            text_el(&mut out, a);
            out.push_str(r#"<number:number number:min-integer-digits="1"/>"#);
            text_el(&mut out, &b[7.min(b.len())..]);
            continue;
        }
        if s.contains('@') {
            kind = "number:text-style";
            for (k, part) in s.split('@').enumerate() {
                if k > 0 {
                    out.push_str("<number:text-content/>");
                }
                text_el(&mut out, part);
            }
            continue;
        }
        // The placeholders: from the first 0 # ? . to the last.
        let is_ph = |c: char| "0#?.,".contains(c) || c.is_ascii_digit();
        let ch: Vec<char> = s.chars().collect();
        let mut i = 0;
        while i < ch.len() {
            if !(matches!(ch[i], '0' | '#' | '?') || (ch[i] == '.' && ch.get(i + 1).is_some_and(|d| matches!(d, '0' | '#' | '?')))) {
                if ch[i] == '%' {
                    kind = "number:percentage-style";
                }
                text_el(&mut out, &ch[i].to_string());
                i += 1;
                continue;
            }
            let mut j = i;
            loop {
                while j < ch.len() && is_ph(ch[j]) {
                    j += 1;
                }
                // Scientific: E+00 / E-00.
                if j + 1 < ch.len() && (ch[j] == 'E' || ch[j] == 'e') && (ch[j + 1] == '+' || ch[j + 1] == '-') {
                    j += 2;
                    continue;
                }
                // Fraction: "# ?/?" — a space then more placeholders and a slash.
                if j < ch.len() && ch[j] == ' ' && ch[j + 1..].iter().take_while(|c| **c != ' ').any(|c| *c == '/') {
                    j += 1;
                    continue;
                }
                if j < ch.len() && ch[j] == '/' {
                    j += 1;
                    continue;
                }
                break;
            }
            let pat: String = ch[i..j].iter().collect();
            number_el(&mut out, &pat);
            i = j;
        }
    }
    (out, kind)
}

fn number_el(out: &mut String, pat: &str) {
    if let Some(slash) = pat.find('/') {
        // [whole ]num/den
        let (left, den) = (&pat[..slash], &pat[slash + 1..]);
        let (whole, numer) = match left.rfind(' ') {
            Some(sp) => (Some(&left[..sp]), &left[sp + 1..]),
            None => (None, left),
        };
        let nd = numer.chars().filter(|c| matches!(c, '0' | '#' | '?')).count().max(1);
        let _ = write!(out, r#"<number:fraction number:min-numerator-digits="{nd}""#);
        if let Some(w) = whole {
            let _ = write!(out, r#" number:min-integer-digits="{}""#, w.chars().filter(|&c| c == '0').count());
        }
        if den.chars().all(|c| c.is_ascii_digit()) && !den.is_empty() {
            let _ = write!(out, r#" number:denominator-value="{den}""#);
        } else {
            let dd = den.chars().filter(|c| matches!(c, '0' | '#' | '?')).count().max(1);
            let _ = write!(out, r#" number:min-denominator-digits="{dd}" number:max-denominator-value="{}""#, "9".repeat(dd));
        }
        out.push_str("/>");
        return;
    }
    let (mant, exp) = match pat.find(['E', 'e']) {
        Some(e) => (&pat[..e], Some(&pat[e + 2..])),
        None => (pat, None),
    };
    let (int, frac) = match mant.find('.') {
        Some(d) => (&mant[..d], &mant[d + 1..]),
        None => (mant, ""),
    };
    // Trailing commas scale by a thousand each.
    let trimmed = int.trim_end_matches(',');
    let mut scale = int.len() - trimmed.len();
    let frac_t = frac.trim_end_matches(',');
    scale += frac.len() - frac_t.len();
    let group = trimmed.contains(',');
    let min_int = trimmed.chars().filter(|&c| c == '0').count();
    let dec = frac_t.chars().filter(|c| matches!(c, '0' | '#' | '?')).count();
    let min_dec = frac_t.chars().filter(|&c| c == '0').count();
    let el = if exp.is_some() { "number:scientific-number" } else { "number:number" };
    let _ = write!(out, r#"<{el} number:decimal-places="{dec}" number:min-decimal-places="{min_dec}" number:min-integer-digits="{min_int}""#);
    if group {
        out.push_str(r#" number:grouping="true""#);
    }
    if scale > 0 {
        let _ = write!(out, r#" number:display-factor="1{}""#, "000".repeat(scale.min(5)));
    }
    if let Some(e) = exp {
        let _ = write!(out, r#" number:min-exponent-digits="{}""#, e.len().clamp(1, 4));
    }
    out.push_str("/>");
}

/// One section as a data style element named `name`.
fn data_style(out: &mut String, name: &str, sec: &Section, volatile: bool, maps: &[(String, String)]) {
    let p = pieces(&sec.body);
    let (children, el, elapsed) = if is_date_body(&p) {
        let (c, date, elapsed) = date_children(&p);
        (c, if date { "number:date-style" } else { "number:time-style" }, elapsed)
    } else {
        let (c, el) = number_children(&p);
        (c, el, false)
    };
    let _ = write!(out, r#"<{el} style:name="{name}""#);
    if volatile {
        out.push_str(r#" style:volatile="true""#);
    }
    if elapsed {
        out.push_str(r#" number:truncate-on-overflow="false""#);
    }
    out.push('>');
    if let Some(c) = sec.color {
        let _ = write!(out, r#"<style:text-properties fo:color="{}"/>"#, hex(c));
    }
    out.push_str(&children);
    for (cond, other) in maps {
        out.push_str(r#"<style:map style:condition="value()"#);
        esc(out, cond);
        let _ = write!(out, r#"" style:apply-style-name="{other}"/>"#);
    }
    let _ = write!(out, "</{el}>");
}

/// The data style(s) for an Excel code, the main one named `name`. `None` for
/// General (no data style).
fn data_styles(code: &str, name: &str) -> Option<String> {
    if code.eq_ignore_ascii_case("General") || code.is_empty() {
        return None;
    }
    let mut secs: Vec<Section> = sections(code).iter().map(|s| section(s)).collect();
    // A text section (4th, or any with @ after numbers) is not modelled as a map.
    if secs.len() == 4 {
        secs.pop();
    }
    let mut out = String::new();
    if secs.len() == 1 {
        data_style(&mut out, name, &secs[0], false, &[]);
        return Some(out);
    }
    let explicit = secs.iter().any(|s| s.cond.is_some());
    let mut maps = Vec::new();
    let n = secs.len();
    for (k, s) in secs.iter().enumerate().take(n - 1) {
        let cond = match (&s.cond, explicit, n, k) {
            (Some(c), _, _, _) => c.replace("<>", "!="),
            (None, false, 2, 0) => ">=0".to_string(),
            (None, false, _, 0) => ">0".to_string(),
            (None, false, _, 1) => "<0".to_string(),
            _ => continue,
        };
        let sub = format!("{name}P{k}");
        data_style(&mut out, &sub, s, true, &[]);
        maps.push((cond, sub));
    }
    data_style(&mut out, name, &secs[n - 1], false, &maps);
    Some(out)
}

// ---------------------------------------------------------------------------
// Cell styles.

fn border(line: Line, color: Option<u32>) -> String {
    let c = hex(color.unwrap_or(0));
    let (w, kind) = match line {
        Line::None => return "none".into(),
        Line::Thin => ("0.74pt", "solid"),
        Line::Medium => ("1.76pt", "solid"),
        Line::Thick => ("2.49pt", "solid"),
        Line::Double => return format!("1.76pt double-thin {c}"),
        Line::Hair => ("0.1pt", "solid"),
        Line::Dashed => ("0.74pt", "dashed"),
        Line::MediumDashed => ("1.76pt", "dashed"),
        Line::Dotted => ("0.74pt", "dotted"),
        Line::DashDot => ("0.74pt", "dash-dot"),
        Line::MediumDashDot | Line::SlantDashDot => ("1.76pt", "dash-dot"),
        Line::DashDotDot => ("0.74pt", "dash-dot-dot"),
        Line::MediumDashDotDot => ("1.76pt", "dash-dot-dot"),
    };
    format!("{w} {kind} {c}")
}

fn cell_style(out: &mut String, name: &str, st: &Style, data: Option<&str>) {
    let _ = write!(out, r#"<style:style style:name="{name}" style:family="table-cell" style:parent-style-name="Default""#);
    if let Some(d) = data {
        let _ = write!(out, r#" style:data-style-name="{d}""#);
    }
    out.push('>');
    // table-cell-properties
    let mut tc = String::new();
    if let Some(f) = st.fill {
        let _ = write!(tc, r#" fo:background-color="{}""#, hex(f));
    }
    for (k, n) in ["fo:border-left", "fo:border-right", "fo:border-top", "fo:border-bottom"].iter().enumerate() {
        let e = st.borders[k];
        if e.line != Line::None {
            let _ = write!(tc, r#" {n}="{}""#, border(e.line, e.color));
            if e.line == Line::Double {
                // The two lines and the gap (LibreOffice's double-thin).
                let side = &n[10..];
                let _ = write!(tc, r#" style:border-line-width-{side}="0.0071in 0.0102in 0.0071in""#);
            }
        }
    }
    match st.v_align {
        VAlign::Top => tc.push_str(r#" style:vertical-align="top""#),
        VAlign::Center => tc.push_str(r#" style:vertical-align="middle""#),
        VAlign::Bottom => {}
        #[allow(unreachable_patterns)]
        _ => {}
    }
    if st.wrap {
        tc.push_str(r#" fo:wrap-option="wrap""#);
    }
    if st.rotation != 0 {
        let _ = write!(tc, r#" style:rotation-angle="{}""#, st.rotation);
    }
    if !tc.is_empty() {
        let _ = write!(out, "<style:table-cell-properties{tc}/>");
    }
    let align = match st.h_align {
        HAlign::Left => Some("start"),
        HAlign::Center => Some("center"),
        HAlign::Right => Some("end"),
        HAlign::Justify => Some("justify"),
        _ => None,
    };
    if align.is_some() || st.indent > 0 {
        out.push_str("<style:paragraph-properties");
        if let Some(a) = align {
            let _ = write!(out, r#" fo:text-align="{a}""#);
        }
        if st.indent > 0 {
            // Excel's indent step is about three characters.
            let _ = write!(out, r#" fo:margin-left="{}""#, len_in(st.indent as f64 * 21.0));
        }
        out.push_str("/>");
    }
    let f = &st.font;
    let mut tp = String::new();
    if f.bold {
        tp.push_str(r#" fo:font-weight="bold""#);
    }
    if f.italic {
        tp.push_str(r#" fo:font-style="italic""#);
    }
    if f.underline {
        tp.push_str(r#" style:text-underline-style="solid" style:text-underline-width="auto" style:text-underline-color="font-color""#);
    }
    if f.strike {
        tp.push_str(r#" style:text-line-through-style="solid""#);
    }
    if let Some(c) = f.color {
        let _ = write!(tp, r#" fo:color="{}""#, hex(c));
    }
    if let Some(z) = f.size {
        let _ = write!(tp, r#" fo:font-size="{}pt""#, z as f64 / 20.0);
    }
    if let Some(n) = &f.name {
        tp.push_str(r#" style:font-name=""#);
        esc(&mut tp, n);
        tp.push('"');
    }
    if !tp.is_empty() {
        let _ = write!(out, "<style:text-properties{tp}/>");
    }
    out.push_str("</style:style>");
}

// ---------------------------------------------------------------------------
// Cells.

/// The value type a number is written as, from its cell's format.
enum NumKind {
    Float,
    Percent,
    Date,
    Time,
}

fn num_kind(wb: &Workbook, id: StyleId) -> NumKind {
    let f = wb.styles.format(id);
    if f.is_date() {
        let first = sections(f.code()).into_iter().next().unwrap_or_default();
        let p = pieces(&section(&first).body);
        let (_, date, _) = date_children(&p);
        return if date { NumKind::Date } else { NumKind::Time };
    }
    let code = f.code();
    if !f.is_general() && pieces(code).iter().any(|(l, s)| !l && s.contains('%')) {
        return NumKind::Percent;
    }
    NumKind::Float
}

fn date_text(x: f64) -> Option<String> {
    let (y, m, d) = dates::ymd(x)?;
    if d == 0 || (y == 1900 && m == 2 && d == 29) {
        return None;
    }
    let ms = ((x - x.floor()) * 86_400_000.0).round() as i64;
    let (h, mi, s, frac) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    let mut t = format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}");
    if frac > 0 {
        let _ = write!(t, ".{frac:03}");
    }
    Some(t)
}

fn time_text(x: f64) -> String {
    let neg = x < 0.0;
    let ms = (x.abs() * 86_400_000.0).round() as i64;
    let (h, mi, s, frac) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    let mut t = format!("{}PT{h}H{mi}M{s}", if neg { "-" } else { "" });
    if frac > 0 {
        let _ = write!(t, ".{frac:03}");
    }
    t.push('S');
    t
}

/// One cell's attributes and content (without the element's name).
fn cell_xml(out: &mut String, wb: &Workbook, s: u16, r: u32, c: u32, v: &Value, style: Option<StyleId>, bools: &mut BTreeSet<StyleId>) {
    out.push_str("<table:table-cell");
    let id = wb.sheet(s).style_id(r, c);
    // A logical value needs a boolean format to stay one (LibreOffice shows 1).
    let style = if matches!(v, Value::Bool(_)) && wb.styles.format(id).is_general() {
        bools.insert(id);
        let _ = write!(out, r#" table:style-name="ce{id}b""#);
        None
    } else {
        style
    };
    match style {
        Some(0) => out.push_str(r#" table:style-name="Default""#),
        Some(id) => {
            let _ = write!(out, r#" table:style-name="ce{id}""#);
        }
        None => {}
    }
    if let Some(g) = wb.group_at(s, r, c) {
        let grp = wb.group(g);
        // OpenFormula has no structured references: the ranges they reach.
        let inl;
        let e = if wb.tables().is_empty() || !Workbook::uses_tables(&grp.expr) {
            &*grp.expr
        } else {
            inl = wb.inline_tables(&grp.expr, s, r, c);
            &inl
        };
        let a1 = formula::to_a1(e, r, c, &|id| wb.sheet_name(id));
        out.push_str(r#" table:formula=""#);
        esc(out, &super::formula::to_of(&a1));
        out.push('"');
    } else if let Value::Err(e) = v {
        // A constant error: a formula of just the error.
        out.push_str(r#" table:formula="of:="#);
        esc(out, e.text());
        out.push('"');
    }
    match v {
        Value::Empty => {
            out.push_str("/>");
            return;
        }
        Value::Num(x) => {
            let x = *x;
            let kind = num_kind(wb, id);
            let date = matches!(kind, NumKind::Date).then(|| date_text(x)).flatten();
            match (kind, date) {
                (NumKind::Date, Some(d)) => {
                    let _ = write!(out, r#" office:value-type="date" office:date-value="{d}" calcext:value-type="date">"#);
                }
                (NumKind::Time, _) => {
                    let _ = write!(out, r#" office:value-type="time" office:time-value="{}" calcext:value-type="time">"#, time_text(x));
                }
                (NumKind::Percent, _) => {
                    let _ = write!(out, r#" office:value-type="percentage" office:value="{}" calcext:value-type="percentage">"#, num(x));
                }
                _ => {
                    let _ = write!(out, r#" office:value-type="float" office:value="{}" calcext:value-type="float">"#, num(x));
                }
            }
            paragraphs(out, &wb.formatted(officina_grid::CellAddr::new(s, r, c)).text);
        }
        Value::Str(t) => {
            out.push_str(r#" office:value-type="string""#);
            if wb.group_at(s, r, c).is_some() {
                out.push_str(r#" office:string-value=""#);
                esc(out, t);
                out.push('"');
            }
            out.push_str(r#" calcext:value-type="string">"#);
            paragraphs(out, t);
        }
        Value::Bool(b) => {
            let _ = write!(out, r#" office:value-type="boolean" office:boolean-value="{b}" calcext:value-type="boolean">"#);
            paragraphs(out, if *b { "TRUE" } else { "FALSE" });
        }
        Value::Err(e) => {
            let t = match e {
                ErrorKind::Circ => "Err:522",
                e => e.text(),
            };
            out.push_str(r#" office:value-type="string" office:string-value="" calcext:value-type="error">"#);
            paragraphs(out, t);
        }
    }
    out.push_str("</table:table-cell>");
}

fn sheet_xml(out: &mut String, wb: &Workbook, s: u16, row_style: &mut BTreeMap<u64, usize>, col_style: &mut BTreeMap<u64, usize>, bools: &mut BTreeSet<StyleId>) -> io::Result<()> {
    let (rows, cols) = wb.extent(s);
    let name = wb.sheet_name(s);
    if rows > ODS_ROWS || cols > ODS_COLS {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("the sheet “{name}” has {rows} rows × {cols} columns; .ods holds at most 1,048,576 × 16,384")));
    }
    let sh = wb.sheet(s);
    let last = ODS_ROWS - 1;
    // Column defaults: a run to the last row.
    let tails: Vec<StyleId> = sh.styles.iter().take(ODS_COLS as usize).map(|r| r.get(last)).collect();
    let tail = |c: u32| tails.get(c as usize).copied().unwrap_or(0);
    // Cells to write: values, formula cells, and cells styled unlike their column.
    let mut cells: Vec<(u32, u32)> = Vec::new();
    let rect = Rect { sheet: s, r0: 0, c0: 0, r1: rows.saturating_sub(1), c1: cols.saturating_sub(1) };
    if rows > 0 && cols > 0 {
        wb.each_rowmajor(&rect, &mut |r, c, v| {
            if !v.is_empty() {
                cells.push((r, c));
            }
        });
    }
    for (_, g) in wb.groups() {
        if g.sheet == s {
            for r in g.r0..=g.r1.min(last) {
                cells.push((r, g.col));
            }
        }
    }
    let mut extra = 0usize;
    for (c, runs) in sh.styles.iter().enumerate().take(ODS_COLS as usize) {
        let t = tail(c as u32);
        for (a, b, id) in runs.spans(0, last) {
            if id == t || b - a > MAX_EXTRA as u32 || extra > MAX_EXTRA {
                continue;
            }
            for r in a..=b {
                cells.push((r, c as u32));
            }
            extra += (b - a + 1) as usize;
        }
    }
    cells.sort_unstable();
    cells.dedup();

    out.push_str("<table:table table:name=\"");
    esc(out, &name);
    out.push_str("\">");
    // Charts on this sheet, as frames anchored to it.
    let mut shapes = String::new();
    for (i, ch) in wb.charts().iter().enumerate().filter(|(_, c)| c.anchor.sheet == s) {
        shapes.push_str(&super::chart::frame_xml(wb, ch, i));
    }
    if !shapes.is_empty() {
        let _ = write!(out, "<table:shapes>{shapes}</table:shapes>");
    }
    // Columns: (width, default style) grouped.
    let ncols = cols.max(tails.iter().rposition(|&t| t != 0).map_or(0, |p| p as u32 + 1)).max(sh.col_sizes.overrides().last().map_or(0, |o| o.0 + 1)).min(ODS_COLS);
    let width = |c: u32| sh.col_sizes.overrides().binary_search_by_key(&c, |o| o.0).map(|i| sh.col_sizes.overrides()[i].1).unwrap_or(sh.col_sizes.default);
    let mut style_of_width = |px: f64| {
        let key = (px * 64.0).round() as u64;
        let n = col_style.len();
        *col_style.entry(key).or_insert(n)
    };
    let mut c = 0;
    while c < ncols.max(1) {
        let (w, t) = (width(c), tail(c));
        let mut n = 1;
        while c + n < ncols && width(c + n) == w && tail(c + n) == t {
            n += 1;
        }
        let _ = write!(out, r#"<table:table-column table:style-name="co{}""#, style_of_width(w));
        if n > 1 {
            let _ = write!(out, r#" table:number-columns-repeated="{n}""#);
        }
        let _ = write!(out, r#" table:default-cell-style-name="{}"/>"#, if t == 0 { "Default".to_string() } else { format!("ce{t}") });
        c += n;
    }
    // Rows.
    let heights = sh.row_sizes.overrides();
    let mut row_of_height = |px: f64| {
        let key = (px * 64.0).round() as u64;
        let n = row_style.len();
        *row_style.entry(key).or_insert(n)
    };
    let def_row = row_of_height(sh.row_sizes.default);
    let mut row = 0u32; // next row to write
    let mut k = 0; // into cells
    let mut hi = 0; // into heights
    let fill_to = tails.iter().rposition(|&t| t != 0).map_or(0, |p| p as u32 + 1);
    // Hidden rows: "filter" inside the auto-filter's range (LibreOffice shows them
    // again when the filter is cleared), "collapse" elsewhere.
    let frange = sh.filter.as_ref().map(|f| (f.rect.r0, f.rect.r1));
    let vis = |r: u32| -> &'static str {
        if !sh.row_hidden(r) {
            ""
        } else if frange.is_some_and(|(a, b)| a <= r && r <= b) {
            r#" table:visibility="filter""#
        } else {
            r#" table:visibility="collapse""#
        }
    };
    // Empty rows `start .. start + n`, split where hidden ranges begin and end.
    let gap = |out: &mut String, start: u32, n: u32| {
        let end = start as u64 + n as u64;
        let mut r = start as u64;
        while r < end {
            // The run of rows that share `r`'s visibility.
            let v = vis(r as u32);
            let k = sh.hidden.partition_point(|h| (h.1 as u64) < r);
            let next = match sh.hidden.get(k) {
                Some(&(a, b)) if (a as u64) <= r => b as u64 + 1,
                Some(&(a, _)) => a as u64,
                None => end,
            }
            .min(end);
            let m = next - r;
            let _ = write!(out, r#"<table:table-row table:style-name="ro{def_row}"{v}"#);
            if m > 1 {
                let _ = write!(out, r#" table:number-rows-repeated="{m}""#);
            }
            // Cells through the last styled column: LibreOffice gives a column's
            // default style only to cells the row has.
            if fill_to > 1 {
                let _ = write!(out, r#"><table:table-cell table:number-columns-repeated="{fill_to}"/></table:table-row>"#);
            } else {
                out.push_str("><table:table-cell/></table:table-row>");
            }
            r = next;
        }
    };
    loop {
        let next_cell = cells.get(k).map(|x| x.0);
        let next_h = heights.get(hi).map(|x| x.0).filter(|&r| r <= last);
        let r = match (next_cell, next_h) {
            (Some(a), Some(b)) => a.min(b),
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => break,
        };
        gap(out, row, r - row);
        let mut rs = def_row;
        while hi < heights.len() && heights[hi].0 <= r {
            if heights[hi].0 == r {
                rs = row_of_height(heights[hi].1);
            }
            hi += 1;
        }
        let _ = write!(out, r#"<table:table-row table:style-name="ro{rs}"{}>"#, vis(r));
        let mut col = 0u32;
        let mut any = false;
        while k < cells.len() && cells[k].0 == r {
            let cc = cells[k].1;
            if cc >= ODS_COLS {
                k += 1;
                continue;
            }
            if cc > col {
                let n = cc - col;
                if n > 1 {
                    let _ = write!(out, r#"<table:table-cell table:number-columns-repeated="{n}"/>"#);
                } else {
                    out.push_str("<table:table-cell/>");
                }
            }
            let id = sh.style_id(r, cc);
            let style = (id != tail(cc)).then_some(id);
            cell_xml(out, wb, s, r, cc, &wb.value(s, r, cc), style, bools);
            col = cc + 1;
            any = true;
            k += 1;
        }
        if fill_to > col {
            let n = fill_to - col;
            if n > 1 {
                let _ = write!(out, r#"<table:table-cell table:number-columns-repeated="{n}"/>"#);
            } else {
                out.push_str("<table:table-cell/>");
            }
        } else if !any {
            out.push_str("<table:table-cell/>");
        }
        out.push_str("</table:table-row>");
        row = r + 1;
    }
    let hid_end = sh.hidden.last().map_or(0, |h| h.1 + 1).min(ODS_ROWS);
    if tails.iter().any(|&t| t != 0) {
        // LibreOffice applies a column's default style only to rows the table has:
        // run the table to its last row.
        gap(out, row, ODS_ROWS - row);
    } else if hid_end > row {
        gap(out, row, hid_end - row);
    } else if row == 0 {
        gap(out, 0, 1);
    }
    out.push_str(&names_xml(wb, Some(s)));
    out.push_str("</table:table>");
    Ok(())
}

/// `table:database-ranges` holding each sheet's auto-filter, as LibreOffice writes
/// them: columns joined by and; a column's values as one condition with set items.
fn filters_xml(wb: &Workbook) -> String {
    let mut out = String::new();
    for s in 0..wb.sheet_count() as u16 {
        let Some(f) = &wb.sheet(s).filter else { continue };
        let sh = super::formula::sheet_a1(&wb.sheet_name(s));
        let addr = format!("{sh}.{}{}:{sh}.{}{}", officina_grid::addr::col_name(f.rect.c0), f.rect.r0 + 1, officina_grid::addr::col_name(f.rect.c1), f.rect.r1 + 1);
        let _ = write!(out, r#"<table:database-range table:name="__Anonymous_Sheet_DB__{s}" table:target-range-address=""#);
        esc(&mut out, &addr);
        out.push_str(r#"" table:display-filter-buttons="true">"#);
        if !f.conds.is_empty() {
            out.push_str("<table:filter><table:filter-and>");
            for (c, cond) in &f.conds {
                let field = c - f.rect.c0;
                match cond {
                    Cond::Values(v) => {
                        let first = v.first().cloned().unwrap_or_default();
                        let _ = write!(out, r#"<table:filter-condition table:field-number="{field}" table:value=""#);
                        esc(&mut out, &first);
                        out.push_str(r#"" table:operator="=""#);
                        if v.len() > 1 {
                            out.push('>');
                            for x in v {
                                out.push_str(r#"<table:filter-set-item table:value=""#);
                                esc(&mut out, x);
                                out.push_str(r#""/>"#);
                            }
                            out.push_str("</table:filter-condition>");
                        } else {
                            out.push_str("/>");
                        }
                    }
                    Cond::Compare(cs, and) => {
                        let or = !and && cs.len() > 1;
                        if or {
                            out.push_str("<table:filter-or>");
                        }
                        for (op, x) in cs {
                            let op = if op == "<>" { "!=" } else { op.as_str() };
                            // Typed only where untyped would read as text: LibreOffice
                            // drops the value of a typed order test.
                            let ty = if matches!(op, "=" | "!=") { r#" table:data-type="number""# } else { "" };
                            let _ = write!(out, r#"<table:filter-condition table:field-number="{field}" table:value="{}" table:operator="{}"{ty}/>"#, num(*x), esc_attr(op));
                        }
                        if or {
                            out.push_str("</table:filter-or>");
                        }
                    }
                }
            }
            out.push_str("</table:filter-and></table:filter>");
        }
        out.push_str("</table:database-range>");
    }
    // Tables as named database ranges, header and data rows (a totals row stays
    // plain cells).
    for t in wb.tables() {
        let r = &t.rect;
        let last = r.r1 - (t.totals && r.r1 > r.r0) as u32;
        let sh = super::formula::sheet_a1(&wb.sheet_name(r.sheet));
        let addr = format!("{sh}.{}{}:{sh}.{}{}", officina_grid::addr::col_name(r.c0), r.r0 + 1, officina_grid::addr::col_name(r.c1), last + 1);
        out.push_str(r#"<table:database-range table:name=""#);
        esc(&mut out, &t.name);
        out.push_str(r#"" table:target-range-address=""#);
        esc(&mut out, &addr);
        let _ = write!(out, r#"" table:contains-header="{}"/>"#, t.header);
    }
    if out.is_empty() {
        return out;
    }
    format!("<table:database-ranges>{out}</table:database-ranges>")
}

fn esc_attr(s: &str) -> String {
    let mut o = String::new();
    esc(&mut o, s);
    o
}

/// `table:named-expressions` for the names of one scope (a sheet's, or the
/// workbook's): ranges as `table:named-range`, the rest as `table:named-expression`,
/// relative to A1 of the scope's sheet.
fn names_xml(wb: &Workbook, scope: Option<u16>) -> String {
    let mut out = String::new();
    let base_sheet = super::formula::sheet_a1(&wb.sheet_name(scope.unwrap_or(0)));
    let base = format!("${}.$A$1", base_sheet.trim_start_matches('$'));
    for d in wb.names().iter().filter(|d| d.scope == scope) {
        let of = super::formula::to_of(&wb.name_text(d));
        let range = matches!(*d.expr, Expr::Ref(_) | Expr::Range(_)).then(|| of.strip_prefix("of:=[").and_then(|r| r.strip_suffix(']'))).flatten().filter(|r| !r.contains('['));
        out.push_str(if range.is_some() { r#"<table:named-range table:name=""# } else { r#"<table:named-expression table:name=""# });
        esc(&mut out, &d.name);
        out.push_str(r#"" table:base-cell-address=""#);
        esc(&mut out, &base);
        match range {
            Some(r) => {
                out.push_str(r#"" table:cell-range-address=""#);
                esc(&mut out, r);
            }
            None => {
                out.push_str(r#"" table:expression=""#);
                esc(&mut out, &of);
            }
        }
        out.push_str(r#""/>"#);
    }
    if out.is_empty() {
        return out;
    }
    format!("<table:named-expressions>{out}</table:named-expressions>")
}

/// Writes `wb` as `.ods`.
pub fn write<W: Write>(wb: &Workbook, out: W) -> io::Result<W> {
    let mut row_style: BTreeMap<u64, usize> = BTreeMap::new();
    let mut col_style: BTreeMap<u64, usize> = BTreeMap::new();
    let mut tables = String::with_capacity(1 << 16);
    let mut bools: BTreeSet<StyleId> = BTreeSet::new();
    for s in 0..wb.sheet_count() {
        sheet_xml(&mut tables, wb, s as u16, &mut row_style, &mut col_style, &mut bools)?;
    }
    // Styles: data styles and cell styles for every style in the table.
    let mut auto = String::new();
    for (&key, &i) in &col_style {
        let _ = write!(auto, r#"<style:style style:name="co{i}" style:family="table-column"><style:table-column-properties fo:break-before="auto" style:column-width="{}"/></style:style>"#, len_in(key as f64 / 64.0));
    }
    for (&key, &i) in &row_style {
        let _ = write!(auto, r#"<style:style style:name="ro{i}" style:family="table-row"><style:table-row-properties style:row-height="{}" fo:break-before="auto" style:use-optimal-row-height="false"/></style:style>"#, len_in(key as f64 / 64.0));
    }
    let mut fonts: HashMap<String, ()> = HashMap::new();
    let mut cells = String::new();
    let mut data_names: HashMap<String, String> = HashMap::new();
    for (id, st) in wb.styles.iter() {
        if id == 0 {
            continue;
        }
        let code = st.num_fmt.to_string();
        let data = match data_names.get(&code) {
            Some(n) => Some(n.clone()),
            None => {
                let n = format!("N{}", data_names.len() + 1);
                match data_styles(&code, &n) {
                    Some(x) => {
                        auto.push_str(&x);
                        data_names.insert(code.clone(), n.clone());
                        Some(n)
                    }
                    None => None,
                }
            }
        };
        if let Some(n) = &st.font.name {
            fonts.insert(n.to_string(), ());
        }
        cell_style(&mut cells, &format!("ce{id}"), st, data.as_deref());
    }
    if !bools.is_empty() {
        auto.push_str(r#"<number:boolean-style style:name="NB"><number:boolean/></number:boolean-style>"#);
        for &id in &bools {
            cell_style(&mut cells, &format!("ce{id}b"), wb.styles.get(id), Some("NB"));
        }
    }
    auto.push_str(&cells);
    let mut faces = String::new();
    let mut names: Vec<&String> = fonts.keys().collect();
    names.sort();
    for n in names {
        faces.push_str(r#"<style:font-face style:name=""#);
        esc(&mut faces, n);
        faces.push_str(r#"" svg:font-family=""#);
        esc(&mut faces, &format!("'{n}'"));
        faces.push_str(r#""/>"#);
    }
    let mut content = String::with_capacity(tables.len() + auto.len() + 1024);
    let _ = write!(content, r#"<?xml version="1.0" encoding="UTF-8"?><office:document-content {NS}><office:font-face-decls>{faces}</office:font-face-decls><office:automatic-styles>{auto}</office:automatic-styles><office:body><office:spreadsheet><table:calculation-settings table:case-sensitive="false" table:search-criteria-must-apply-to-whole-cell="true" table:use-wildcards="true" table:use-regular-expressions="false" table:automatic-find-labels="false"/>"#);
    content.push_str(&tables);
    content.push_str(&names_xml(wb, None));
    content.push_str(&filters_xml(wb));
    content.push_str("</office:spreadsheet></office:body></office:document-content>");
    let styles = format!(r#"<?xml version="1.0" encoding="UTF-8"?><office:document-styles {NS}><office:styles><style:default-style style:family="table-cell"><style:paragraph-properties style:tab-stop-distance="0.5in"/></style:default-style><style:style style:name="Default" style:family="table-cell"/></office:styles></office:document-styles>"#);
    let mut objects = String::new();
    for i in 0..wb.charts().len() {
        let n = i + 1;
        let _ = write!(objects, r#"<manifest:file-entry manifest:full-path="Object {n}/" manifest:version="1.3" manifest:media-type="application/vnd.oasis.opendocument.chart"/><manifest:file-entry manifest:full-path="Object {n}/content.xml" manifest:media-type="text/xml"/>"#);
    }
    let manifest = format!(r#"<?xml version="1.0" encoding="UTF-8"?><manifest:manifest xmlns:manifest="urn:oasis:names:tc:opendocument:xmlns:manifest:1.0" manifest:version="1.3"><manifest:file-entry manifest:full-path="/" manifest:version="1.3" manifest:media-type="{MIME}"/><manifest:file-entry manifest:full-path="content.xml" manifest:media-type="text/xml"/><manifest:file-entry manifest:full-path="styles.xml" manifest:media-type="text/xml"/><manifest:file-entry manifest:full-path="meta.xml" manifest:media-type="text/xml"/>{objects}</manifest:manifest>"#);
    let meta = format!(r#"<?xml version="1.0" encoding="UTF-8"?><office:document-meta xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0" xmlns:meta="urn:oasis:names:tc:opendocument:xmlns:meta:1.0" office:version="1.3"><office:meta><meta:generator>Officina</meta:generator></office:meta></office:document-meta>"#);
    let mut z = ZipWriter::new(out);
    // The mimetype first and stored, so the file can be recognised by its bytes.
    z.add_stored("mimetype", MIME.as_bytes())?;
    z.add("META-INF/manifest.xml", manifest.as_bytes())?;
    z.add("meta.xml", meta.as_bytes())?;
    z.add("styles.xml", styles.as_bytes())?;
    z.add("content.xml", content.as_bytes())?;
    for (i, ch) in wb.charts().iter().enumerate() {
        z.add(&format!("Object {}/content.xml", i + 1), super::chart::object_xml(wb, ch).as_bytes())?;
    }
    z.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_as_data_styles() {
        let d = data_styles("#,##0.00", "N1").unwrap();
        assert!(d.contains(r#"<number:number number:decimal-places="2" number:min-decimal-places="2" number:min-integer-digits="1" number:grouping="true"/>"#), "{d}");
        let d = data_styles("0%", "N1").unwrap();
        assert!(d.starts_with("<number:percentage-style") && d.contains("<number:text>%</number:text>"), "{d}");
        let d = data_styles("[h]:mm:ss", "N1").unwrap();
        assert!(d.contains(r#"number:truncate-on-overflow="false""#) && d.contains("<number:minutes number:style=\"long\"/>"), "{d}");
        let d = data_styles("yyyy-mm-dd", "N1").unwrap();
        assert!(d.starts_with("<number:date-style") && d.contains(r#"<number:month number:style="long"/>"#), "{d}");
        let d = data_styles("#,##0_);[Red](#,##0)", "N1").unwrap();
        assert!(d.contains(r#"style:name="N1P0" style:volatile="true""#) && d.contains(r##"fo:color="#ff0000""##) && d.contains(r#"style:condition="value()&gt;=0""#), "{d}");
        assert!(d.contains(r#"loext:blank-width-char=")""#), "{d}");
        let d = data_styles("# ?/8", "N1").unwrap();
        assert!(d.contains(r#"number:denominator-value="8""#), "{d}");
        let d = data_styles("0.00E+00", "N1").unwrap();
        assert!(d.contains("<number:scientific-number") && d.contains(r#"number:min-exponent-digits="2""#), "{d}");
        assert!(data_styles("General", "N1").is_none());
        let d = data_styles("@", "N1").unwrap();
        assert!(d.starts_with("<number:text-style") && d.contains("<number:text-content/>"), "{d}");
    }

    #[test]
    fn text_keeps_its_spaces() {
        let mut s = String::new();
        paragraphs(&mut s, "  a  b\tc \nd");
        assert_eq!(s, r#"<text:p><text:s text:c="2"/>a <text:s/>b<text:tab/>c<text:s/></text:p><text:p>d</text:p>"#);
    }
}
