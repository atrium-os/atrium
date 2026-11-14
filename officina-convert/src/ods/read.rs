//! `.ods` in (ODF 1.3 spreadsheet), for the converter process.
//!
//! Read: sheets, values (floats, percentages, currencies, dates, times, booleans,
//! text with its spaces, tabs and line breaks), repeated rows and cells, formulas
//! (OpenFormula, [`super::formula`]), column widths and row heights, and cell styles:
//! number formats from ODF data styles (turned into Excel format codes), bold, italic,
//! underline, strike, font colour, background, borders, alignment, wrap.
//!
//! Reported, not carried: merged cells, comments, charts and images, conditional
//! formats, validation, named ranges, hidden rows and columns (shown), other formula
//! namespaces (kept as values).

use std::collections::HashMap;
use std::sync::Arc;

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use officina_grid::addr::{CellAddr, MAX_COLS, MAX_ROWS};
use officina_grid::column::Sizes;
use officina_grid::eval::dates;
use officina_grid::strings::MAX_TEXT;
use officina_grid::style::{Edge, HAlign, Line, Style, StyleId, StyleRuns, VAlign};
use officina_grid::filter::Cond;
use officina_grid::{ErrorKind, Value, Workbook};

use crate::report::{Kind, Report};
use crate::xlsx::Loaded;
use crate::zip::{Archive, ZipError};

/// Cells a file may expand to through repeats (a repeated cell with a value is that
/// many cells).
pub const MAX_CELLS: u64 = 200_000_000;

#[derive(Debug)]
pub enum OdsError {
    Zip(ZipError),
    Xml(String),
    Bad(String),
}

impl std::fmt::Display for OdsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OdsError::Zip(e) => write!(f, "{e}"),
            OdsError::Xml(e) => write!(f, "damaged spreadsheet XML: {e}"),
            OdsError::Bad(e) => write!(f, "not a spreadsheet this can read: {e}"),
        }
    }
}

impl std::error::Error for OdsError {}

impl From<ZipError> for OdsError {
    fn from(e: ZipError) -> Self {
        OdsError::Zip(e)
    }
}
impl From<quick_xml::Error> for OdsError {
    fn from(e: quick_xml::Error) -> Self {
        OdsError::Xml(e.to_string())
    }
}

fn bad(s: impl Into<String>) -> OdsError {
    OdsError::Bad(s.into())
}

/// Attributes by qualified name (`office:value`), unescaped.
fn attrs(e: &BytesStart) -> HashMap<String, String> {
    let mut m = HashMap::new();
    for a in e.attributes().with_checks(false).flatten() {
        if let Ok(v) = a.unescape_value() {
            m.insert(String::from_utf8_lossy(a.key.as_ref()).into_owned(), v.into_owned());
        }
    }
    m
}

/// A length (`0.889in`, `2.258cm`, `12pt`, `30mm`, `96px`) in px at 96 dpi.
pub fn length_px(s: &str) -> Option<f64> {
    let s = s.trim();
    let split = s.find(|c: char| c.is_ascii_alphabetic() || c == '%')?;
    let (n, u) = s.split_at(split);
    let n: f64 = n.trim().parse().ok()?;
    let px = match u {
        "in" => n * 96.0,
        "cm" => n * 96.0 / 2.54,
        "mm" => n * 96.0 / 25.4,
        "pt" => n * 96.0 / 72.0,
        "pc" => n * 16.0,
        "px" => n,
        _ => return None,
    };
    px.is_finite().then_some(px)
}

/// `2026-11-14` or `2026-11-14T15:30:00.5` as a serial (Excel's 1900 system).
pub fn date_value(s: &str) -> Option<f64> {
    let (d, t) = s.split_once('T').unwrap_or((s, ""));
    let mut it = d.splitn(3, '-');
    let (y, m, dd) = (it.next()?.parse::<f64>().ok()?, it.next()?.parse::<f64>().ok()?, it.next()?.parse::<f64>().ok()?);
    let day = dates::date_serial(y, m, dd)?;
    let frac = if t.is_empty() {
        0.0
    } else {
        let mut p = t.splitn(3, ':');
        let h: f64 = p.next()?.parse().ok()?;
        let mi: f64 = p.next().unwrap_or("0").parse().ok()?;
        let se: f64 = p.next().unwrap_or("0").trim_end_matches('Z').parse().ok()?;
        (h * 3600.0 + mi * 60.0 + se) / 86400.0
    };
    Some(day + frac)
}

/// `PT12H30M15.5S` (an ISO 8601 duration, as ODF times are) as a fraction of a day.
pub fn time_value(s: &str) -> Option<f64> {
    let neg = s.starts_with('-');
    let s = s.trim_start_matches('-').strip_prefix('P')?;
    let (days, t) = s.split_once('T').unwrap_or((s, ""));
    let mut total = 0.0;
    if let Some(d) = days.strip_suffix('D') {
        total += d.parse::<f64>().ok()? * 86400.0;
    } else if !days.is_empty() {
        return None;
    }
    let mut num = String::new();
    for c in t.chars() {
        match c {
            'H' => total += num.parse::<f64>().ok()? * 3600.0,
            'M' => total += num.parse::<f64>().ok()? * 60.0,
            'S' => total += num.parse::<f64>().ok()?,
            _ => {
                num.push(c);
                continue;
            }
        }
        num.clear();
    }
    let v = total / 86400.0;
    Some(if neg { -v } else { v })
}

/// An ODF border (`0.06pt solid #000000`) as an edge.
fn border(s: &str) -> Edge {
    if s.trim() == "none" || s.trim().is_empty() {
        return Edge::default();
    }
    let mut width = 1.0;
    let mut kind = "solid";
    let mut color = None;
    for p in s.split_whitespace() {
        if let Some(h) = p.strip_prefix('#') {
            color = u32::from_str_radix(h, 16).ok();
        } else if let Some(w) = length_px(p) {
            width = w;
        } else {
            kind = p;
        }
    }
    // Widths in px (LibreOffice: thin 0.74pt, medium 1.76pt, thick 2.49pt, hair 0.1pt).
    let medium = width > 1.6;
    let line = match kind {
        k if k.starts_with("double") => Line::Double,
        "dashed" | "fine-dashed" | "dash" => if medium { Line::MediumDashed } else { Line::Dashed },
        "dash-dot" => if medium { Line::MediumDashDot } else { Line::DashDot },
        "dash-dot-dot" => if medium { Line::MediumDashDotDot } else { Line::DashDotDot },
        "dotted" => Line::Dotted,
        _ if width > 2.8 => Line::Thick,
        _ if medium => Line::Medium,
        _ if width < 0.5 => Line::Hair,
        _ => Line::Thin,
    };
    Edge { line, color: color.filter(|&c| c != 0) }
}

/// A data style being read: its parts as an Excel format code.
#[derive(Default, Clone)]
struct DataStyle {
    code: String,
    color: Option<String>,
    /// `style:map`: (condition, other style's name).
    maps: Vec<(String, String)>,
    /// A percentage style: its `%` text scales.
    pct: bool,
    /// `number:truncate-on-overflow="false"`: hours past a day (`[h]`).
    elapsed: bool,
    /// `loext:blank-width-char` of the `number:text` being read (Excel's `_x`).
    blank: Option<char>,
}

fn color_name(rgb: u32) -> Option<&'static str> {
    Some(match rgb {
        0xFF0000 | 0xC9211E => "[Red]",
        0x0000FF => "[Blue]",
        0x00FF00 => "[Green]",
        0x000000 => "[Black]",
        0xFFFFFF => "[White]",
        0xFFFF00 => "[Yellow]",
        0xFF00FF => "[Magenta]",
        0x00FFFF => "[Cyan]",
        _ => return None,
    })
}

/// Quotes literal text for a format code.
fn lit(s: &str) -> String {
    if s.is_empty() {
        return String::new();
    }
    if s.chars().all(|c| " -/:,.()$€£¥".contains(c)) {
        return s.chars().map(|c| if c == '$' || c == ' ' || c == '-' || c == '/' || c == ':' || c == '(' || c == ')' { c.to_string() } else { format!("\\{c}") }).collect();
    }
    format!("\"{}\"", s.replace('"', ""))
}

/// The cell-level look of a `style:style` of family `table-cell`.
#[derive(Default, Clone)]
struct CellStyle {
    parent: Option<String>,
    data: Option<String>,
    bold: Option<bool>,
    italic: Option<bool>,
    underline: Option<bool>,
    strike: Option<bool>,
    color: Option<u32>,
    size: Option<u16>,
    font: Option<String>,
    fill: Option<Option<u32>>,
    borders: [Option<Edge>; 4],
    h: Option<HAlign>,
    v: Option<VAlign>,
    wrap: Option<bool>,
}

#[derive(Default)]
struct Styles {
    cells: HashMap<String, CellStyle>,
    data: HashMap<String, DataStyle>,
    col_width: HashMap<String, f64>,
    row_height: HashMap<String, f64>,
    /// Font face name → family.
    faces: HashMap<String, String>,
}

impl Styles {
    /// Reads `office:font-face-decls`, `office:styles` and `office:automatic-styles`
    /// from one part.
    fn read(&mut self, xml: &[u8], rep: &mut Report) {
        let mut r = Reader::from_reader(xml);
        let mut buf = Vec::new();
        let mut cur_cell: Option<(String, CellStyle)> = None;
        let mut cur_data: Option<(String, DataStyle)> = None;
        let mut cur_family = String::new();
        let mut cur_name = String::new();
        let mut in_text = false;
        let mut text = String::new();
        let mut in_body = false;
        loop {
            buf.clear();
            let ev = match r.read_event_into(&mut buf) {
                Ok(e) => e,
                Err(_) => {
                    rep.note(Kind::Approximate, "a damaged styles part: some cells shown unstyled");
                    return;
                }
            };
            let empty = matches!(ev, Event::Empty(_));
            match ev {
                Event::Start(e) | Event::Empty(e) => {
                    let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                    if name == "office:body" {
                        in_body = true;
                    }
                    if in_body {
                        continue;
                    }
                    let a = attrs(&e);
                    match name.as_str() {
                        "style:font-face" => {
                            if let (Some(n), Some(f)) = (a.get("style:name"), a.get("svg:font-family")) {
                                self.faces.insert(n.clone(), f.trim_matches('\'').to_string());
                            }
                        }
                        "style:style" => {
                            cur_family = a.get("style:family").cloned().unwrap_or_default();
                            cur_name = a.get("style:name").cloned().unwrap_or_default();
                            if cur_family == "table-cell" {
                                cur_cell = Some((cur_name.clone(), CellStyle { parent: a.get("style:parent-style-name").cloned(), data: a.get("style:data-style-name").cloned(), ..CellStyle::default() }));
                            }
                        }
                        "style:default-style" if a.get("style:family").is_some_and(|f| f == "table-cell") => {
                            cur_family = "table-cell".into();
                            cur_cell = Some(("".into(), CellStyle::default()));
                        }
                        "style:table-column-properties" if cur_family == "table-column" => {
                            if let Some(w) = a.get("style:column-width").and_then(|w| length_px(w)) {
                                self.col_width.insert(cur_name.clone(), w);
                            }
                        }
                        "style:table-row-properties" if cur_family == "table-row" => {
                            if let Some(h) = a.get("style:row-height").and_then(|h| length_px(h)) {
                                self.row_height.insert(cur_name.clone(), h);
                            }
                        }
                        "style:text-properties" => {
                            if let Some((_, c)) = cur_cell.as_mut() {
                                if let Some(w) = a.get("fo:font-weight") {
                                    c.bold = Some(w == "bold" || w.parse::<u32>().is_ok_and(|n| n >= 600));
                                }
                                if let Some(s) = a.get("fo:font-style") {
                                    c.italic = Some(s == "italic" || s == "oblique");
                                }
                                if let Some(u) = a.get("style:text-underline-style") {
                                    c.underline = Some(u != "none");
                                }
                                if let Some(u) = a.get("style:text-line-through-style") {
                                    c.strike = Some(u != "none");
                                }
                                if let Some(col) = a.get("fo:color").and_then(|v| v.strip_prefix('#')).and_then(|v| u32::from_str_radix(v, 16).ok()) {
                                    c.color = Some(col);
                                }
                                if let Some(sz) = a.get("fo:font-size").and_then(|v| length_px(v)) {
                                    c.size = Some((sz * 72.0 / 96.0 * 20.0).round().clamp(20.0, 409.0 * 20.0) as u16);
                                }
                                if let Some(f) = a.get("style:font-name") {
                                    c.font = Some(f.clone());
                                }
                            } else if let Some((_, d)) = cur_data.as_mut() {
                                d.color = a.get("fo:color").cloned();
                            }
                        }
                        "style:table-cell-properties" => {
                            if let Some((_, c)) = cur_cell.as_mut() {
                                if let Some(bg) = a.get("fo:background-color") {
                                    c.fill = Some(bg.strip_prefix('#').and_then(|v| u32::from_str_radix(v, 16).ok()));
                                }
                                if let Some(b) = a.get("fo:border") {
                                    let e = border(b);
                                    c.borders = [Some(e); 4];
                                }
                                for (k, n) in ["fo:border-left", "fo:border-right", "fo:border-top", "fo:border-bottom"].iter().enumerate() {
                                    if let Some(b) = a.get(*n) {
                                        c.borders[k] = Some(border(b));
                                    }
                                }
                                if let Some(v) = a.get("style:vertical-align") {
                                    c.v = Some(match v.as_str() {
                                        "top" => VAlign::Top,
                                        "middle" => VAlign::Center,
                                        _ => VAlign::Bottom,
                                    });
                                }
                                if let Some(w) = a.get("fo:wrap-option") {
                                    c.wrap = Some(w == "wrap");
                                }
                                if a.get("style:rotation-angle").is_some_and(|v| v != "0") {
                                    rep.note(Kind::Approximate, "rotated text shown level");
                                }
                            }
                        }
                        "style:paragraph-properties" => {
                            if let Some((_, c)) = cur_cell.as_mut() {
                                if let Some(t) = a.get("fo:text-align") {
                                    c.h = Some(match t.as_str() {
                                        "start" | "left" => HAlign::Left,
                                        "center" => HAlign::Center,
                                        "end" | "right" => HAlign::Right,
                                        "justify" => HAlign::Justify,
                                        _ => HAlign::General,
                                    });
                                }
                            }
                        }
                        n if n.starts_with("number:") && n.ends_with("-style") => {
                            let d = DataStyle { pct: n == "number:percentage-style", elapsed: a.get("number:truncate-on-overflow").is_some_and(|v| v == "false"), ..DataStyle::default() };
                            cur_data = Some((a.get("style:name").cloned().unwrap_or_default(), d));
                        }
                        "style:map" => {
                            if let (Some((_, d)), Some(c), Some(s)) = (cur_data.as_mut(), a.get("style:condition"), a.get("style:apply-style-name")) {
                                d.maps.push((c.clone(), s.clone()));
                            } else if cur_cell.is_some() {
                                rep.note(Kind::Dropped, "conditional formats");
                            }
                        }
                        "number:number" | "number:scientific-number" | "number:fraction" => {
                            if let Some((_, d)) = cur_data.as_mut() {
                                if name == "number:number" && !a.contains_key("number:decimal-places") && !a.contains_key("number:grouping") && !a.contains_key("number:display-factor") {
                                    // No fixed decimals: the standard format.
                                    d.code.push_str("General");
                                    continue;
                                }
                                let int = a.get("number:min-integer-digits").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1).min(30);
                                let dec = a.get("number:decimal-places").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0).min(30);
                                let min_dec = a.get("number:min-decimal-places").and_then(|v| v.parse::<usize>().ok()).unwrap_or(dec).min(dec);
                                let group = a.get("number:grouping").is_some_and(|v| v == "true");
                                let mut s = if group {
                                    // Excel's grouping needs at least four integer places.
                                    let mut z = "#".repeat(4usize.saturating_sub(int)) + &"0".repeat(int);
                                    z.insert(z.len() - 3, ',');
                                    z
                                } else if int == 0 {
                                    "#".into()
                                } else {
                                    "0".repeat(int)
                                };
                                if name == "number:fraction" {
                                    let dd = a.get("number:min-denominator-digits").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1).clamp(1, 7);
                                    let nd = a.get("number:min-numerator-digits").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1).clamp(1, 7);
                                    let whole = if a.contains_key("number:min-integer-digits") { "# " } else { "" };
                                    let den = match a.get("number:denominator-value") {
                                        Some(v) => v.clone(),
                                        None => "?".repeat(dd),
                                    };
                                    d.code.push_str(&format!("{whole}{}/{den}", "?".repeat(nd)));
                                    continue;
                                }
                                if dec > 0 {
                                    s.push('.');
                                    s.push_str(&"0".repeat(min_dec));
                                    s.push_str(&"#".repeat(dec - min_dec));
                                }
                                if name == "number:scientific-number" {
                                    let ed = a.get("number:min-exponent-digits").and_then(|v| v.parse::<usize>().ok()).unwrap_or(2).clamp(1, 4);
                                    s.push_str("E+");
                                    s.push_str(&"0".repeat(ed));
                                }
                                if let Some(f) = a.get("number:display-factor").and_then(|v| v.parse::<f64>().ok()) {
                                    let mut k = f;
                                    while k >= 1000.0 {
                                        s.push(',');
                                        k /= 1000.0;
                                    }
                                }
                                d.code.push_str(&s);
                            }
                        }
                        "number:year" | "number:month" | "number:day" | "number:day-of-week" | "number:hours" | "number:minutes" | "number:seconds" | "number:am-pm" | "number:quarter" | "number:week-of-year" | "number:era" => {
                            if let Some((_, d)) = cur_data.as_mut() {
                                let long = a.get("number:style").is_some_and(|v| v == "long");
                                let textual = a.get("number:textual").is_some_and(|v| v == "true");
                                let part = match name.as_str() {
                                    "number:year" => if long { "yyyy" } else { "yy" }.to_string(),
                                    "number:month" => match (textual, long) {
                                        (true, true) => "mmmm",
                                        (true, false) => "mmm",
                                        (false, true) => "mm",
                                        (false, false) => "m",
                                    }
                                    .to_string(),
                                    "number:day" => if long { "dd" } else { "d" }.to_string(),
                                    "number:day-of-week" => if long { "dddd" } else { "ddd" }.to_string(),
                                    "number:hours" => {
                                        let h = if long { "hh" } else { "h" };
                                        if std::mem::take(&mut d.elapsed) { format!("[{h}]") } else { h.to_string() }
                                    }
                                    "number:minutes" => if long { "mm" } else { "m" }.to_string(),
                                    "number:seconds" => {
                                        let dp = a.get("number:decimal-places").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0).min(3);
                                        let mut s = if long { "ss" } else { "s" }.to_string();
                                        if dp > 0 {
                                            s.push('.');
                                            s.push_str(&"0".repeat(dp));
                                        }
                                        s
                                    }
                                    "number:am-pm" => "AM/PM".to_string(),
                                    _ => {
                                        rep.note(Kind::Approximate, "quarters, week numbers and eras in date formats shown plainly");
                                        String::new()
                                    }
                                };
                                d.code.push_str(&part);
                            }
                        }
                        "number:text" | "number:currency-symbol" => {
                            in_text = !empty;
                            text.clear();
                            if let Some((_, d)) = cur_data.as_mut() {
                                d.blank = a.get("loext:blank-width-char").and_then(|v| v.chars().next());
                            }
                        }
                        "number:text-content" => {
                            if let Some((_, d)) = cur_data.as_mut() {
                                d.code.push('@');
                            }
                        }
                        _ => {}
                    }
                }
                Event::Text(t) if in_text => {
                    if let Ok(s) = t.unescape() {
                        text.push_str(&s);
                    }
                }
                Event::End(e) => {
                    let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                    if name == "office:body" {
                        in_body = false;
                    }
                    match name.as_str() {
                        "number:text" | "number:currency-symbol" => {
                            in_text = false;
                            if let Some((_, d)) = cur_data.as_mut() {
                                let mut t = text.as_str();
                                if let Some(b) = d.blank.take() {
                                    // The blank stands in for the first character.
                                    let mut it = t.chars();
                                    it.next();
                                    d.code.push('_');
                                    d.code.push(b);
                                    t = it.as_str();
                                }
                                if d.pct && t.contains('%') {
                                    for (i, part) in t.split('%').enumerate() {
                                        if i > 0 {
                                            d.code.push('%');
                                        }
                                        d.code.push_str(&lit(part));
                                    }
                                } else {
                                    d.code.push_str(&lit(t));
                                }
                            }
                        }
                        "style:style" | "style:default-style" => {
                            if let Some((n, c)) = cur_cell.take() {
                                self.cells.insert(n, c);
                            }
                            cur_family.clear();
                        }
                        n if n.starts_with("number:") && n.ends_with("-style") => {
                            if let Some((n, d)) = cur_data.take() {
                                self.data.insert(n, d);
                            }
                        }
                        _ => {}
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
    }

    /// The Excel code of a data style, its `style:map`s as sections.
    fn code(&self, name: &str) -> Option<String> {
        let d = self.data.get(name)?;
        let own = |d: &DataStyle| {
            let mut c = String::new();
            if let Some(col) = d.color.as_ref().and_then(|v| v.strip_prefix('#')).and_then(|v| u32::from_str_radix(v, 16).ok()).and_then(color_name) {
                c.push_str(col);
            }
            c.push_str(&d.code);
            c
        };
        let mut secs: Vec<String> = Vec::new();
        for (cond, other) in &d.maps {
            let Some(o) = self.data.get(other) else { continue };
            let cond = cond.trim().strip_prefix("value()").unwrap_or("").trim();
            match cond {
                ">=0" | ">0" => secs.insert(0, own(o)),
                _ => secs.push(format!("[{}]{}", cond.replace("!=", "<>"), own(o))),
            }
        }
        if secs.is_empty() {
            return Some(own(d)).filter(|c| !c.is_empty());
        }
        // The style itself takes what no map took (negatives, usually), with its own
        // signs: ODF writes the minus as text, as an Excel section that has one.
        let mut code = secs.join(";");
        code.push(';');
        code.push_str(&own(d));
        Some(code)
    }

    /// The full style for a cell style name (its parents' look under its own).
    fn resolve(&self, name: &str) -> Style {
        let mut chain = Vec::new();
        let mut n = Some(name.to_string());
        while let Some(k) = n {
            if chain.len() > 16 {
                break;
            }
            match self.cells.get(&k) {
                Some(c) => {
                    chain.push(c.clone());
                    n = c.parent.clone();
                }
                None => break,
            }
        }
        if let Some(d) = self.cells.get("") {
            chain.push(d.clone());
        }
        let mut s = Style::default();
        let mut base_size = None;
        let mut base_font = None;
        for k in ["", "Default"] {
            if let Some(d) = self.cells.get(k) {
                base_size = d.size.or(base_size);
                base_font = d.font.clone().or(base_font);
            }
        }
        for c in chain.iter().rev() {
            if let Some(d) = &c.data {
                if let Some(code) = self.code(d) {
                    s.num_fmt = Arc::from(code.as_str());
                }
            }
            if let Some(b) = c.bold {
                s.font.bold = b;
            }
            if let Some(b) = c.italic {
                s.font.italic = b;
            }
            if let Some(b) = c.underline {
                s.font.underline = b;
            }
            if let Some(b) = c.strike {
                s.font.strike = b;
            }
            if c.color.is_some() {
                s.font.color = c.color.filter(|&c| c != 0);
            }
            if c.size.is_some() {
                s.font.size = c.size.filter(|z| Some(*z) != base_size && *z != 200);
            }
            if let Some(f) = &c.font {
                let fam = self.faces.get(f).cloned().unwrap_or_else(|| f.clone());
                s.font.name = if Some(f) == base_font.as_ref() || fam == "Liberation Sans" || fam == "Calibri" { None } else { Some(Arc::from(fam.as_str())) };
            }
            if let Some(f) = c.fill {
                s.fill = f;
            }
            for k in 0..4 {
                if let Some(e) = c.borders[k] {
                    s.borders[k] = e;
                }
            }
            if let Some(h) = c.h {
                s.h_align = h;
            }
            if let Some(v) = c.v {
                s.v_align = v;
            }
            if let Some(w) = c.wrap {
                s.wrap = w;
            }
        }
        s
    }
}

/// A cell being read.
#[derive(Default)]
struct CellIn {
    vtype: String,
    value: Option<String>,
    formula: Option<String>,
    style: Option<String>,
    repeat: u32,
    text: String,
    paras: u32,
}

/// Reads an `.ods` package into a workbook.
pub fn read(data: &[u8]) -> Result<(Workbook, Loaded), OdsError> {
    let z = Archive::open(data)?;
    let mut out = Loaded::default();
    let mut rep = Report::default();
    if let Some(m) = z.read_name("mimetype")? {
        let m = String::from_utf8_lossy(&m);
        if m.trim() != "application/vnd.oasis.opendocument.spreadsheet" && m.trim() != "application/vnd.oasis.opendocument.spreadsheet-template" {
            return Err(bad(format!("not an OpenDocument spreadsheet ({})", m.trim())));
        }
    }
    if z.entries.iter().any(|e| e.name.starts_with("Basic/") || e.name.starts_with("Scripts/")) {
        rep.note(Kind::Inert, "macros (never run; not kept in an Atrium file yet)");
    }
    let content = z.read_name("content.xml")?.ok_or_else(|| bad("no content.xml"))?;
    let mut styles = Styles::default();
    if let Some(s) = z.read_name("styles.xml")? {
        styles.read(&s, &mut rep);
    }
    styles.read(&content, &mut rep);

    let mut wb = Workbook::new();
    let mut style_ids: HashMap<String, StyleId> = HashMap::new();
    let mut r = Reader::from_reader(&content[..]);
    r.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut sheet: Option<u16> = None;
    // Hidden rows of the sheet being read; an auto-filter being read (target range,
    // conditions by field) and the filters read.
    let mut hidden_rows: Vec<(u32, u32)> = Vec::new();
    let mut db: Option<(String, Vec<(u32, Cond)>)> = None;
    let mut filters: Vec<(String, Vec<(u32, Cond)>)> = Vec::new();
    let mut filter_and = true;
    let mut in_set = false;
    // Defined names: (name, the sheet it is local to, OpenFormula, base cell).
    let mut names: Vec<(String, Option<u16>, String, Option<String>)> = Vec::new();
    let (mut row, mut col): (u64, u64) = (0, 0);
    let mut row_repeat = 1u32;
    let mut row_style: Option<String> = None;
    let mut col_defaults: Vec<(u64, u64, Option<String>)> = Vec::new();
    let mut col_x: u64 = 0;
    let mut widths: Vec<(u32, f64)> = Vec::new();
    let mut heights: Vec<(u32, f64)> = Vec::new();
    let mut cell: Option<CellIn> = None;
    let mut in_p = false;
    let mut row_cells: Vec<(u64, u32, CellIn)> = Vec::new();
    let mut fcells: Vec<(u16, u32, u32, String, Value)> = Vec::new();
    let mut runs: Vec<Vec<(u32, u32, StyleId)>> = Vec::new();
    let mut expanded: u64 = 0;
    let mut annotation_depth = 0u32;

    let finish_sheet = |wb: &mut Workbook, s: u16, widths: &mut Vec<(u32, f64)>, heights: &mut Vec<(u32, f64)>, runs: &mut Vec<Vec<(u32, u32, StyleId)>>| {
        let sh = wb.sheet_mut(s);
        if !widths.is_empty() {
            widths.sort_by_key(|w| w.0);
            widths.dedup_by_key(|w| w.0);
            sh.col_sizes = Sizes::from_overrides(sh.col_sizes.default, std::mem::take(widths));
        }
        if !heights.is_empty() {
            sh.row_sizes = Sizes::from_overrides(sh.row_sizes.default, std::mem::take(heights));
        }
        let mut cols = Vec::with_capacity(runs.len());
        for rs in runs.drain(..) {
            let mut v: Vec<(u32, StyleId)> = Vec::new();
            let mut at = 0u32;
            for (r0, r1, id) in rs {
                if r0 > at {
                    v.push((at, 0));
                }
                v.push((r0, id));
                at = r1.saturating_add(1);
            }
            if (at as u64) < MAX_ROWS as u64 {
                v.push((at, 0));
            }
            cols.push(StyleRuns::from_runs(v));
        }
        while cols.last().is_some_and(|c: &StyleRuns| c.is_empty()) {
            cols.pop();
        }
        sh.styles = cols;
    };

    loop {
        buf.clear();
        let ev = r.read_event_into(&mut buf)?;
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let empty = matches!(ev, Event::Empty(_));
                let name = e.name();
                let name = name.as_ref();
                if annotation_depth > 0 {
                    if !empty {
                        annotation_depth += 1;
                    }
                    continue;
                }
                match name {
                    b"table:table" => {
                        let a = attrs(e);
                        let mut n: String = a.get("table:name").cloned().unwrap_or_default().chars().filter(|c| !"[]:*?/\\".contains(*c)).collect();
                        if n.is_empty() || wb.sheet_id(&n).is_some() {
                            n = format!("Sheet{}", wb.sheet_count() + 1);
                        }
                        sheet = Some(wb.add_sheet(&n).map_err(bad)?);
                        row = 0;
                        col_x = 0;
                        col_defaults.clear();
                        if a.get("table:protected").is_some_and(|v| v == "true") {
                            rep.note(Kind::Dropped, "sheet protection");
                        }
                    }
                    b"table:table-column" => {
                        let a = attrs(e);
                        let n = a.get("table:number-columns-repeated").and_then(|v| v.parse::<u64>().ok()).unwrap_or(1).clamp(1, MAX_COLS as u64);
                        if let Some(w) = a.get("table:style-name").and_then(|s| styles.col_width.get(s)) {
                            for c in col_x..(col_x + n).min(MAX_COLS as u64).min(col_x + 4096) {
                                widths.push((c as u32, w.round().max(2.0)));
                            }
                        }
                        if a.get("table:visibility").is_some_and(|v| v != "visible") {
                            rep.note(Kind::Approximate, "hidden columns shown");
                        }
                        col_defaults.push((col_x, col_x + n - 1, a.get("table:default-cell-style-name").cloned()));
                        col_x += n;
                    }
                    b"table:table-row" => {
                        let a = attrs(e);
                        row_repeat = a.get("table:number-rows-repeated").and_then(|v| v.parse::<u32>().ok()).unwrap_or(1).clamp(1, MAX_ROWS);
                        row_style = a.get("table:default-cell-style-name").cloned();
                        if let Some(h) = a.get("table:style-name").and_then(|s| styles.row_height.get(s)) {
                            if (row as u32) < MAX_ROWS && (h - 17.0).abs() > 0.6 {
                                for k in 0..row_repeat.min(4096) as u64 {
                                    if row + k < MAX_ROWS as u64 {
                                        heights.push(((row + k) as u32, h.round().max(2.0)));
                                    }
                                }
                            }
                        }
                        if a.get("table:visibility").is_some_and(|v| v != "visible") && row < MAX_ROWS as u64 {
                            hidden_rows.push((row as u32, (row + row_repeat as u64 - 1).min(MAX_ROWS as u64 - 1) as u32));
                        }
                        col = 0;
                        row_cells.clear();
                        if empty {
                            row += row_repeat as u64;
                        }
                    }
                    b"table:table-cell" | b"table:covered-table-cell" => {
                        let a = attrs(e);
                        let repeat = a.get("table:number-columns-repeated").and_then(|v| v.parse::<u32>().ok()).unwrap_or(1).clamp(1, MAX_COLS);
                        if name == b"table:covered-table-cell" {
                            rep.note(Kind::Approximate, "merged cells shown unmerged");
                        }
                        if a.contains_key("table:number-columns-spanned") || a.contains_key("table:number-rows-spanned") {
                            rep.note(Kind::Approximate, "merged cells shown unmerged");
                        }
                        let vtype = if a.get("calcext:value-type").is_some_and(|v| v == "error") { "error".to_string() } else { a.get("office:value-type").or_else(|| a.get("calcext:value-type")).cloned().unwrap_or_default() };
                        let value = match vtype.as_str() {
                            "float" | "percentage" | "currency" => a.get("office:value").cloned(),
                            "date" => a.get("office:date-value").cloned(),
                            "time" => a.get("office:time-value").cloned(),
                            "boolean" => a.get("office:boolean-value").cloned(),
                            "string" => a.get("office:string-value").cloned(),
                            _ => None,
                        };
                        let c = CellIn { vtype, value, formula: a.get("table:formula").cloned(), style: a.get("table:style-name").cloned(), repeat, text: String::new(), paras: 0 };
                        if empty {
                            row_cells.push((col, repeat, c));
                            col += repeat as u64;
                        } else {
                            cell = Some(c);
                        }
                    }
                    b"text:p" | b"text:h" => {
                        if let Some(c) = cell.as_mut() {
                            if c.paras > 0 {
                                c.text.push('\n');
                            }
                            c.paras += 1;
                            in_p = !empty;
                        }
                    }
                    b"text:s" => {
                        if let (true, Some(c)) = (in_p, cell.as_mut()) {
                            let n = attrs(e).get("text:c").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1).min(1000);
                            c.text.extend(std::iter::repeat_n(' ', n));
                        }
                    }
                    b"text:tab" => {
                        if let (true, Some(c)) = (in_p, cell.as_mut()) {
                            c.text.push('\t');
                        }
                    }
                    b"text:line-break" => {
                        if let (true, Some(c)) = (in_p, cell.as_mut()) {
                            c.text.push('\n');
                        }
                    }
                    b"office:annotation" => {
                        rep.note(Kind::Dropped, "comments");
                        if !empty {
                            annotation_depth = 1;
                        }
                    }
                    b"draw:frame" | b"draw:custom-shape" | b"draw:g" => rep.note(Kind::Dropped, "charts, images and drawings"),
                    b"table:named-range" | b"table:named-expression" => {
                        let a = attrs(e);
                        let src = if name == b"table:named-range" { a.get("table:cell-range-address").map(|r| format!("of:=[{r}]")) } else { a.get("table:expression").cloned() };
                        if let (Some(n), Some(src)) = (a.get("table:name"), src) {
                            if names.len() < officina_grid::names::MAX_NAMES {
                                names.push((n.clone(), sheet, src, a.get("table:base-cell-address").cloned()));
                            }
                        }
                    }
                    b"calcext:conditional-formats" => rep.note(Kind::Dropped, "conditional formats"),
                    b"table:content-validations" => rep.note(Kind::Dropped, "data validation"),
                    b"table:database-range" => {
                        let a = attrs(e);
                        db = None;
                        if a.get("table:display-filter-buttons").is_some_and(|v| v == "true") {
                            if let Some(t) = a.get("table:target-range-address") {
                                db = Some((t.clone(), Vec::new()));
                            }
                        } else {
                            rep.note(Kind::Dropped, "database ranges");
                        }
                    }
                    b"table:filter-and" => filter_and = true,
                    b"table:filter-or" => filter_and = false,
                    b"table:filter-condition" | b"table:filter-set-item" => {
                        let a = attrs(e);
                        let Some((_, conds)) = db.as_mut() else { continue };
                        if name == b"table:filter-set-item" {
                            if let (Some((_, Cond::Values(v))), Some(x)) = (conds.last_mut(), a.get("table:value")) {
                                // The condition's own value repeats the first item.
                                if !std::mem::replace(&mut in_set, true) {
                                    v.clear();
                                }
                                if v.len() < 1 << 20 {
                                    v.push(x.clone());
                                }
                            }
                            continue;
                        }
                        in_set = false;
                        let Some(field) = a.get("table:field-number").and_then(|v| v.parse::<u32>().ok()) else { continue };
                        let op = a.get("table:operator").map(String::as_str).unwrap_or("=");
                        let value = a.get("table:value").cloned().unwrap_or_default();
                        // LibreOffice leaves the type out of its numeric tests: an order
                        // test on a number is numeric unless the file says text.
                        let number = match a.get("table:data-type").map(String::as_str) {
                            Some(t) => t == "number",
                            None => !matches!(op, "=" | "!=") && value.parse::<f64>().is_ok(),
                        };
                        let cmp = ["=", "!=", "<", "<=", ">", ">="].contains(&op);
                        match (number, cmp, value.parse::<f64>()) {
                            (true, true, Ok(x)) if x.is_finite() => {
                                let op = if op == "!=" { "<>" } else { op }.to_string();
                                match conds.iter_mut().find(|(c, k)| *c == field && matches!(k, Cond::Compare(..))) {
                                    Some((_, Cond::Compare(v, _))) if v.len() < 2 => v.push((op, x)),
                                    Some(_) => rep.note(Kind::Approximate, "filters with more than two tests on a column (rows stay as the file hid them)"),
                                    None => conds.push((field, Cond::Compare(vec![(op, x)], filter_and))),
                                }
                            }
                            (false, _, _) if op == "=" => match conds.iter_mut().find(|(c, k)| *c == field && matches!(k, Cond::Values(_))) {
                                Some((_, Cond::Values(v))) => {
                                    if !value.is_empty() || empty {
                                        v.push(value);
                                    }
                                }
                                _ => conds.push((field, Cond::Values(if empty || !value.is_empty() { vec![value] } else { Vec::new() }))),
                            },
                            _ => rep.note(Kind::Approximate, "text, pattern and top-N filters (rows stay as the file hid them)"),
                        }
                    }
                    _ => {}
                }
            }
            Event::Text(t) if in_p && annotation_depth == 0 => {
                if let Some(c) = cell.as_mut() {
                    let s = t.unescape().map_err(|e| OdsError::Xml(e.to_string()))?;
                    if c.text.len() + s.len() <= MAX_TEXT {
                        c.text.push_str(&s);
                    }
                }
            }
            Event::End(e) => {
                let name = e.name();
                let name = name.as_ref();
                if annotation_depth > 0 {
                    annotation_depth -= 1;
                    continue;
                }
                match name {
                    b"table:database-range" => filters.extend(db.take()),
                    b"text:p" | b"text:h" => in_p = false,
                    b"table:table-cell" | b"table:covered-table-cell" => {
                        if let Some(c) = cell.take() {
                            let n = c.repeat;
                            row_cells.push((col, n, c));
                            col += n as u64;
                        }
                    }
                    b"table:table-row" => {
                        let s = sheet.ok_or_else(|| bad("a row outside a table"))?;
                        // Cells of this row, repeated down `row_repeat` rows.
                        let rows_here = if row_cells.iter().any(|(_, _, c)| c.value.is_some() || c.formula.is_some() || !c.text.is_empty()) { row_repeat.min(MAX_ROWS) as u64 } else { 1 };
                        // Refuse a bomb before expanding it.
                        let per_row: u64 = row_cells.iter().filter(|(_, _, c)| c.value.is_some() || c.formula.is_some() || !c.text.is_empty()).map(|(c0, n, _)| (*n as u64).min((MAX_COLS as u64).saturating_sub(*c0))).sum();
                        if expanded + per_row.saturating_mul(rows_here) > MAX_CELLS {
                            return Err(bad("more cells than a workbook holds"));
                        }
                        for k in 0..rows_here {
                            let rr = row + k;
                            if rr >= MAX_ROWS as u64 {
                                break;
                            }
                            for (c0, n, c) in &row_cells {
                                for j in 0..*n as u64 {
                                    let cc = c0 + j;
                                    if cc >= MAX_COLS as u64 {
                                        break;
                                    }
                                    let has = c.value.is_some() || c.formula.is_some() || !c.text.is_empty();
                                    if has {
                                        expanded += 1;
                                        if expanded > MAX_CELLS {
                                            return Err(bad("more cells than a workbook holds"));
                                        }
                                        put_cell(&mut wb, s, rr as u32, cc as u32, c, &mut fcells, &mut rep);
                                        out.cells += 1;
                                    }
                                }
                            }
                        }
                        // Styles: the cell's, or the row's, or the column's default.
                        if runs.len() < col as usize {
                            runs.resize_with((col as usize).min(MAX_COLS as usize), Vec::new);
                        }
                        let r0 = row.min(MAX_ROWS as u64 - 1) as u32;
                        let r1 = (row + row_repeat as u64 - 1).min(MAX_ROWS as u64 - 1) as u32;
                        let mut id_of = |nm: &str| -> StyleId {
                            if nm == "Default" {
                                return 0;
                            }
                            *style_ids.entry(nm.to_string()).or_insert_with(|| wb.styles.intern(styles.resolve(nm)))
                        };
                        // Column defaults that style anything, as (first, last, id).
                        let col_ids: Vec<(u64, u64, StyleId)> = col_defaults.iter().filter_map(|(a, b, n)| n.as_deref().map(|n| (*a, *b, id_of(n)))).filter(|x| x.2 != 0).collect();
                        for (c0, n, c) in &row_cells {
                            let own = c.style.as_deref().or(row_style.as_deref()).map(&mut id_of);
                            let last = (c0 + *n as u64).min(MAX_COLS as u64);
                            let col_styled = col_ids.iter().any(|(a, b, _)| *a < last && *b >= *c0);
                            // Trailing filler cells repeated to the last column carry the
                            // default style; skip runs past 1,024 columns of nothing.
                            if own.unwrap_or(0) == 0 && !col_styled && last - c0 > 1024 {
                                continue;
                            }
                            for cc in *c0..last {
                                let id = own.unwrap_or_else(|| col_ids.iter().find(|(a, b, _)| *a <= cc && cc <= *b).map_or(0, |x| x.2));
                                if runs.len() <= cc as usize {
                                    if id == 0 {
                                        continue;
                                    }
                                    runs.resize_with(cc as usize + 1, Vec::new);
                                }
                                let v = &mut runs[cc as usize];
                                match v.last_mut() {
                                    Some(l) if l.2 == id && l.1.saturating_add(1) == r0 => l.1 = r1,
                                    _ => {
                                        if id != 0 || !v.is_empty() {
                                            v.push((r0, r1, id));
                                        }
                                    }
                                }
                            }
                        }
                        row += row_repeat as u64;
                        row_cells.clear();
                    }
                    b"table:table" => {
                        if let Some(s) = sheet.take() {
                            finish_sheet(&mut wb, s, &mut widths, &mut heights, &mut runs);
                            wb.sheet_mut(s).hidden = officina_grid::filter::merge(std::mem::take(&mut hidden_rows));
                        }
                    }
                    _ => {}
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if wb.sheet_count() == 0 {
        return Err(bad("no tables"));
    }

    // Auto-filters: their range names the sheet.
    for (target, mut conds) in filters {
        let Some(a1) = super::formula::from_of(&format!("of:=[{}]", target.replace(':', ":"))) else { continue };
        let (sh, rf) = match a1.trim_start_matches('=').rsplit_once('!') {
            Some((sh, rf)) => (sh.trim_matches('\'').replace("''", "'"), rf.to_string()),
            None => continue,
        };
        let (Some(s), Some((r0, c0, r1, c1))) = (wb.sheet_id(&sh), crate::xlsx::read::parse_ref(&rf)) else { continue };
        conds.retain_mut(|(c, _)| {
            *c += c0;
            *c <= c1
        });
        wb.sheet_mut(s).filter = Some(officina_grid::filter::AutoFilter { rect: officina_grid::addr::Rect { sheet: s, r0, c0, r1, c1 }, conds });
    }

    // Names, before the formulas that use them; relative parts are from the base cell.
    for (n, scope, src, base) in names {
        let at = base.and_then(|b| super::formula::from_of(&format!("of:=[{b}]"))).and_then(|a| {
            let cell = a.rsplit('!').next().unwrap_or(&a).trim_start_matches('=').replace('$', "");
            officina_grid::addr::parse_a1(&cell)
        });
        let (row, col) = at.unwrap_or((0, 0));
        // LibreOffice writes a named expression's formula without its namespace.
        let src = if src.starts_with("of:") || src.starts_with("oooc:") || src.starts_with('=') { src } else { format!("of:={src}") };
        let ok = super::formula::from_of(&src).is_some_and(|a1| wb.define_name_at(&n, scope, &a1, row, col).is_ok());
        if !ok {
            rep.note_at(Kind::Dropped, "defined names this engine can't use", || format!("{n}: {}", src.chars().take(60).collect::<String>()));
        }
    }

    // Formulas, once every sheet exists.
    let mut cached = Vec::new();
    for (s, rr, cc, src, v) in fcells {
        match super::formula::from_of(&src) {
            Some(a1) => {
                if wb.set_formula(CellAddr::new(s, rr, cc), &a1).is_ok() {
                    out.formulas += 1;
                    cached.push((CellAddr::new(s, rr, cc), v));
                } else {
                    wb.set_value(CellAddr::new(s, rr, cc), v);
                    rep.note_at(Kind::Approximate, "formulas this engine can't calculate yet, kept as their values", || a1.chars().take(60).collect());
                }
            }
            None => {
                wb.set_value(CellAddr::new(s, rr, cc), v);
                rep.note_at(Kind::Approximate, "formulas in another syntax, kept as their values", || src.chars().take(60).collect());
            }
        }
    }
    wb.recalc();
    for (a, want) in cached {
        let got = wb.get(a);
        if !same(&got, &want) {
            out.differ += 1;
            rep.note_at(Kind::Approximate, "formula results that differ from the file's saved results", || format!("{}: {} vs {}", officina_grid::addr::a1(a.row, a.col), got.display(), want.display()));
        }
    }
    out.report.merge(rep);
    Ok((wb, out))
}

fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Num(x), Value::Num(y)) => x == y || ((x - y).abs() <= 1e-14 * x.abs().max(y.abs())),
        (Value::Empty, Value::Str(s)) | (Value::Str(s), Value::Empty) => s.is_empty(),
        (Value::Empty, Value::Num(x)) | (Value::Num(x), Value::Empty) => *x == 0.0,
        _ => a == b,
    }
}

fn error_of(s: &str) -> Option<ErrorKind> {
    ErrorKind::from_text(s)
}

/// The value a cell holds (for a formula cell, its cached result).
fn value_of(c: &CellIn, rep: &mut Report) -> Value {
    let num = |s: &Option<String>| s.as_deref().and_then(|v| v.trim().parse::<f64>().ok()).filter(|x| x.is_finite());
    match c.vtype.as_str() {
        "float" | "percentage" | "currency" => num(&c.value).map(Value::Num).unwrap_or(Value::Empty),
        "date" => match c.value.as_deref().and_then(date_value) {
            Some(x) => Value::Num(x),
            None => {
                rep.note(Kind::Approximate, "dates before 1900 kept as text");
                Value::str(&c.text)
            }
        },
        "time" => c.value.as_deref().and_then(time_value).map(Value::Num).unwrap_or(Value::Empty),
        "boolean" => Value::Bool(c.value.as_deref().is_some_and(|v| v == "true" || v == "1")),
        "error" => Value::Err(error_of(c.text.trim()).unwrap_or(ErrorKind::Value)),
        "string" => {
            let t = c.value.clone().unwrap_or_else(|| c.text.clone());
            if c.formula.is_some() {
                if let Some(e) = error_of(&t) {
                    return Value::Err(e);
                }
            }
            Value::str(&t)
        }
        _ => {
            if c.formula.is_some() {
                if let Some(e) = error_of(&c.text) {
                    return Value::Err(e);
                }
            }
            if c.text.is_empty() {
                Value::Empty
            } else {
                Value::str(&c.text)
            }
        }
    }
}

fn put_cell(wb: &mut Workbook, s: u16, r: u32, c: u32, cell: &CellIn, fcells: &mut Vec<(u16, u32, u32, String, Value)>, rep: &mut Report) {
    let v = value_of(cell, rep);
    match &cell.formula {
        Some(f) => fcells.push((s, r, c, f.clone(), v)),
        None => wb.set_value(CellAddr::new(s, r, c), v),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn units_and_times() {
        assert_eq!(length_px("1in"), Some(96.0));
        assert!((length_px("2.54cm").unwrap() - 96.0).abs() < 1e-9);
        assert_eq!(length_px("12pt"), Some(16.0));
        assert_eq!(length_px("x"), None);
        assert_eq!(date_value("2026-11-14"), Some(46340.0));
        assert_eq!(date_value("2026-11-14T12:00:00"), Some(46340.5));
        assert_eq!(date_value("1900-02-28"), Some(59.0));
        assert_eq!(time_value("PT12H00M00S"), Some(0.5));
        assert_eq!(time_value("PT36H"), Some(1.5));
        assert_eq!(time_value("P1DT6H"), Some(1.25));
        assert_eq!(time_value("nope"), None);
    }
}
