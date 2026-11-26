//! `.ods` in (ODF 1.3 spreadsheet), for the converter process.
//!
//! Read: sheets, values (floats, percentages, currencies, dates, times, booleans,
//! text with its spaces, tabs and line breaks), repeated rows and cells, formulas
//! (OpenFormula, [`super::formula`]), column widths and row heights, and cell styles:
//! number formats from ODF data styles (turned into Excel format codes), bold, italic,
//! underline, strike, font colour, background, borders, alignment, wrap.
//!
//! Reported, not carried: some drawings, conditional
//! formats, validation, named ranges, hidden rows and columns (shown), other formula
//! namespaces (kept as values).

use std::collections::HashMap;
use std::sync::Arc;

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use officina_grid::comments::{Comment, CommentRun, CommentStyle};
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
pub(super) fn attrs(e: &BytesStart) -> HashMap<String, String> {
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

/// A typed OpenDocument date or timestamp in the workbook's epoch.
pub fn date_value(system: dates::DateSystem, text: &str) -> Option<f64> {
    system.iso(text).map(|(value, _)| value)
}

fn date_system(content: &[u8], rep: &mut Report) -> Result<dates::DateSystem, OdsError> {
    use dates::DateSystem::*;
    let mut reader = Reader::from_reader(content);
    loop {
        match reader.read_event()? {
            Event::Start(tag) | Event::Empty(tag) if tag.name().as_ref() == b"table:null-date" => {
                return Ok(match attrs(&tag).get("table:date-value").map(String::as_str) {
                    Some("1904-01-01") => Excel1904,
                    None | Some("1899-12-30") => Excel1900,
                    Some(other) => {
                        rep.note_at(Kind::Approximate, "unsupported OpenDocument null date (using the 1900 calendar)", || other.chars().take(32).collect());
                        Excel1900
                    }
                });
            }
            Event::Start(tag) if tag.name().as_ref() == b"table:table" => return Ok(Excel1900),
            Event::Eof => return Ok(Excel1900),
            _ => {}
        }
    }
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
    (v.is_finite() && num.is_empty()).then_some(if neg { -v } else { v })
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
    diagonals: [Option<Edge>; 2],
    shrink_to_fit: Option<bool>,
    rotation: Option<i16>,
    stacked: Option<bool>,
    h: Option<HAlign>,
    v: Option<VAlign>,
    wrap: Option<bool>,
    protection: Option<(bool, bool, bool)>,
}

#[derive(Default)]
struct Styles {
    cells: HashMap<String, CellStyle>,
    data: HashMap<String, DataStyle>,
    col_width: HashMap<String, f64>,
    row_height: HashMap<String, f64>,
    /// Table styles' `tableooo:tab-color`, 0xRRGGBB.
    tab_color: HashMap<String, u32>,
    /// Table styles with table:display="false": hidden sheets.
    hidden_tables: std::collections::HashSet<String>,
    /// Font face name → family.
    faces: HashMap<String, String>,
    comment_text: HashMap<String, CommentStyle>,
    /// Cell styles with `style:map` conditions.
    cond_maps: bool,
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
                        "style:table-properties" if cur_family == "table" => {
                            if let Some(c) = a.get("tableooo:tab-color").and_then(|v| v.strip_prefix('#')).and_then(|v| u32::from_str_radix(v, 16).ok()) {
                                self.tab_color.insert(cur_name.clone(), c & 0xFF_FFFF);
                            }
                            if a.get("table:display").is_some_and(|v| v == "false") {
                                self.hidden_tables.insert(cur_name.clone());
                            }
                        }
                        "style:table-row-properties" if cur_family == "table-row" => {
                            if let Some(h) = a.get("style:row-height").and_then(|h| length_px(h)) {
                                self.row_height.insert(cur_name.clone(), h);
                            }
                        }
                        "style:text-properties" => {
                            if cur_family == "text" {
                                let enabled = |key: &str| a.get(key).is_some_and(|value| value != "none");
                                self.comment_text.insert(cur_name.clone(), CommentStyle {
                                    bold: a.get("fo:font-weight").is_some_and(|v| v == "bold" || v.parse::<u32>().is_ok_and(|n| n >= 600)),
                                    italic: a.get("fo:font-style").is_some_and(|v| matches!(v.as_str(), "italic" | "oblique")),
                                    underline: enabled("style:text-underline-style"),
                                    strike: enabled("style:text-line-through-style"),
                                });
                            }
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
                                if let Some(protection) = a.get("style:cell-protect") {
                                    c.protection = match protection.as_str() {
                                        "none" => Some((true, false, false)),
                                        "hidden-and-protected" => Some((false, true, true)),
                                        flags => {
                                            let mut locked = false;
                                            let mut hidden = false;
                                            let mut known = false;
                                            for flag in flags.split_whitespace() {
                                                match flag {
                                                    "protected" => locked = true,
                                                    "formula-hidden" => hidden = true,
                                                    _ => continue,
                                                }
                                                known = true;
                                            }
                                            if !known {
                                                rep.note(Kind::Approximate, "unknown cell protection settings shown locked");
                                            }
                                            Some((known && !locked, hidden, false))
                                        }
                                    };
                                }
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
                                if let Some(value) = a.get("style:shrink-to-fit") {
                                    c.shrink_to_fit = Some(matches!(value.as_str(), "true" | "1"));
                                }
                                if let Some(value) = a.get("style:direction") {
                                    c.stacked = Some(value == "ttb");
                                }
                                if let Some(value) = a.get("style:rotation-angle") {
                                    c.rotation = value.parse::<i32>().ok().and_then(|angle| match angle.rem_euclid(360) {
                                        n @ 0..=90 => Some(n as i16),
                                        n @ 270..=359 => Some((n - 360) as i16),
                                        _ => {
                                            rep.note(Kind::Approximate, "ODS rotation outside the supported range");
                                            None
                                        }
                                    });
                                }
                                for (i, key) in ["style:diagonal-tl-br", "style:diagonal-bl-tr"].into_iter().enumerate() {
                                    if let Some(value) = a.get(key) {
                                        c.diagonals[i] = Some(border(value));
                                    }
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
                                // LibreOffice writes these beside its own conditional
                                // formats: noted only if the file has none of those.
                                self.cond_maps = true;
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

    /// What a conditional format's cell style sets (not what it inherits from
    /// `Default`).
    fn dxf(&self, name: &str) -> Dxf {
        let mut chain = Vec::new();
        // LibreOffice names a condition's style by its display name.
        let mut n = Some(if self.cells.contains_key(name) { name.to_string() } else { name.replace(' ', "_20_") });
        while let Some(k) = n {
            if chain.len() > 16 || k == "Default" {
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
        let mut d = Dxf::default();
        for c in chain.iter().rev() {
            if let Some(x) = &c.data {
                if let Some(code) = self.code(x) {
                    d.num_fmt = Some(Arc::from(code.as_str()));
                }
            }
            d.bold = c.bold.or(d.bold);
            d.italic = c.italic.or(d.italic);
            d.underline = c.underline.or(d.underline);
            d.strike = c.strike.or(d.strike);
            d.color = c.color.or(d.color);
            if let Some(f) = c.fill {
                d.fill = f;
            }
        }
        d
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
            if let Some(rotation) = c.rotation {
                s.rotation = rotation;
            }
            if let Some(stacked) = c.stacked {
                if stacked {
                    s.rotation = 255;
                } else if s.rotation == 255 {
                    s.rotation = 0;
                }
            }
            if let Some(shrink) = c.shrink_to_fit {
                s.shrink_to_fit = shrink;
            }
            for (target, edge) in s.diagonals.iter_mut().zip(c.diagonals) {
                if let Some(edge) = edge {
                    *target = edge;
                }
            }
            if let Some(flags) = c.protection {
                (s.unlocked, s.hide_formula, s.hide_value) = flags;
            }
        }
        s
    }
}

use super::condfmt::{EntryIn as CfEntry, FormatIn as CfIn};
use officina_grid::condfmt::Dxf;

/// A cell being read.
#[derive(Default)]
struct CellIn {
    /// The address of the first `text:a` in its text.
    link: Option<String>,
    vtype: String,
    value: Option<String>,
    formula: Option<String>,
    live_formula: Option<String>,
    style: Option<String>,
    repeat: u32,
    text: String,
    paras: u32,
    /// A matrix (array) formula's rows and columns.
    matrix: Option<(u32, u32)>,
    /// The data validation rule it names.
    validation: Option<String>,
}

struct StoredFormula {
    at: CellAddr,
    source: String,
    live: Option<String>,
    value: Value,
    matrix: Option<(u32, u32)>,
}

/// An ODF link target as ours: a place `#Sheet2.A1` becomes `#Sheet2!A1`
/// when what follows the last `.` is a cell (sheets may not exist yet).
fn from_odf_href(h: &str) -> String {
    if let Some((sheet, cell)) = h.strip_prefix('#').and_then(|p| p.rsplit_once('.')) {
        let first = cell.split(':').next().unwrap_or("").replace('$', "");
        if !sheet.is_empty() && officina_grid::addr::parse_a1(&first).is_some() {
            return format!("#{}!{cell}", sheet.trim_start_matches('$'));
        }
    }
    h.to_string()
}

/// An `office:annotation` being read.
#[derive(Default)]
struct Annotation {
    thread: Option<Comment>,
    author: String,
    text: String,
    in_creator: bool,
    paras: usize,
    paragraph_depth: Option<u32>,
    spans: Vec<(u32, CommentStyle)>,
    runs: Vec<CommentRun>,
}

impl Annotation {
    fn push(&mut self, text: &str) {
        let start = self.text.len();
        let remaining = officina_grid::comments::MAX_TEXT.saturating_sub(start);
        let end = text.floor_char_boundary(remaining.min(text.len()));
        if end == 0 {
            return;
        }
        self.text.push_str(&text[..end]);
        if let Some((_, style)) = self.spans.last().filter(|(_, style)| style.mask() != 0) {
            self.runs.push(CommentRun { start, end: self.text.len(), style: *style });
        }
    }
}

// A row default can reach every column even when its row has no cells in
// content.xml. Keep the visible prefix without expanding the entire sheet.
fn trailing_row_style(
    runs: &mut Vec<Vec<(u32, u32, StyleId)>>,
    rows: (u32, u32),
    col: usize,
    id: StyleId,
    budget: &mut usize,
    rep: &mut Report,
) {
    if id == 0 || col >= 256 {
        return;
    }
    let width = (256 - col).min(*budget);
    *budget -= width;
    if runs.len() < col + width {
        runs.resize_with(col + width, Vec::new);
    }
    for spans in &mut runs[col..col + width] {
        match spans.last_mut() {
            Some(last) if last.2 == id && last.1 + 1 == rows.0 => last.1 = rows.1,
            _ => spans.push((rows.0, rows.1, id)),
        }
    }
    rep.note(Kind::Approximate, "ODS row formats on distant blank cells were limited");
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
    let content = z.read_name("content.xml")?.ok_or_else(|| bad("no content.xml"))?;
    let mut styles = Styles::default();
    if let Some(s) = z.read_name("styles.xml")? {
        styles.read(&s, &mut rep);
    }
    styles.read(&content, &mut rep);

    let mut wb = Workbook::with_date_system(date_system(&content, &mut rep)?);
    let mut style_ids: HashMap<String, StyleId> = HashMap::new();
    let mut r = Reader::from_reader(&content[..]);
    r.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut sheet: Option<u16> = None;
    // Hidden rows of the sheet being read; an auto-filter being read (target range,
    // visible buttons, conditions by field) and the filters read.
    let mut hidden_rows: Vec<(u32, u32)> = Vec::new();
    let mut filtered_rows = Vec::new();
    let mut saved_filter_rows = HashMap::new();
    // Merged cells of the sheet being read (`table:number-*-spanned`).
    let mut merges: Vec<officina_grid::addr::Rect> = Vec::new();
    let mut db: Option<(Option<String>, String, bool, Vec<(u32, Cond)>)> = None;
    let mut filters: Vec<(Option<String>, String, Vec<(u32, Cond)>)> = Vec::new();
    // Named database ranges: (name, range, header row).
    let mut db_tables: Vec<(String, String, bool)> = Vec::new();
    // Conditional formats (LibreOffice's) and the one being read.
    let mut cf: Option<CfIn> = None;
    let mut cond_formats: Vec<CfIn> = Vec::new();
    // Data pilots (pivot tables) and the one being read.
    let mut pilot: Option<super::pivot::TableIn> = None;
    let mut pilots: Vec<super::pivot::TableIn> = Vec::new();
    let mut in_pilot_group = false;
    let mut whole_cell_filter = true;
    // Defined names: name, scope, OpenFormula, base cell and optional live formula.
    let mut names = Vec::new();
    let (mut row, mut col): (u64, u64) = (0, 0);
    let mut row_repeat = 1u32;
    let mut row_style: Option<String> = None;
    let mut row_style_budget = 262_144;
    let mut col_defaults: Vec<(u64, u64, Option<String>)> = Vec::new();
    let mut col_x: u64 = 0;
    let mut widths: Vec<(u32, f64)> = Vec::new();
    let mut heights: Vec<(u32, f64)> = Vec::new();
    let mut cell: Option<CellIn> = None;
    let mut in_p = false;
    let mut row_cells: Vec<(u64, u32, CellIn)> = Vec::new();
    let mut fcells = Vec::new();
    let mut runs: Vec<Vec<(u32, u32, StyleId)>> = Vec::new();
    let mut expanded: u64 = 0;
    let mut annotation_depth = 0u32;
    // The comment being read, and the sheet's comments so far.
    let mut ann: Option<Annotation> = None;
    // Links of the sheet being read: cell and `xlink:href`.
    let mut links: Vec<(u32, u32, String)> = Vec::new();
    let mut comments: Vec<(u32, u32, Option<officina_grid::comments::Comment>)> = Vec::new();
    // Frames (charts, images) and the one being read, with its depth.
    let mut frames: Vec<super::chart::FrameIn> = Vec::new();
    let mut frame: Option<super::chart::FrameIn> = None;
    let mut frame_depth = 0u32;
    let mut print_areas = Vec::new();
    let (mut header_row, mut header_col) = (None, None);
    // Data validation rules, the ranges whose cells name each, and the range
    // still growing down for (name, sheet, first column, last column).
    let mut val_rules: Vec<super::validation::RuleIn> = Vec::new();
    let mut val_cells: HashMap<String, Vec<officina_grid::addr::Rect>> = HashMap::new();
    let mut val_open: HashMap<(String, u16, u32, u32), usize> = HashMap::new();

    let finish_sheet = |wb: &mut Workbook, s: u16, widths: &mut Vec<(u32, f64)>, heights: &mut Vec<(u32, f64)>, runs: &mut Vec<Vec<(u32, u32, StyleId)>>| {
        let sh = wb.sheet_mut(s);
        if !widths.is_empty() {
            widths.sort_by_key(|w| w.0);
            widths.dedup_by_key(|w| w.0);
            let (hw, sizes) = crate::xlsx::read::remembered(std::mem::take(widths));
            sh.hidden_widths = hw;
            sh.col_sizes = Sizes::from_overrides(sh.col_sizes.default, sizes);
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
                if frame_depth > 0 {
                    if frame_depth == 1 {
                        if let Some(f) = frame.as_mut() {
                            match name {
                                b"draw:object" => f.href = attrs(e).remove("xlink:href"),
                                b"draw:image" if f.image.is_none() => f.image = attrs(e).remove("xlink:href"),
                                _ => {}
                            }
                        }
                    }
                    if !empty {
                        frame_depth += 1;
                    }
                    continue;
                }
                if annotation_depth > 0 {
                    if let Some(a) = ann.as_mut() {
                        match name {
                            b"dc:creator" => a.in_creator = !empty,
                            b"text:p" | b"text:h" => {
                                if a.paras > 0 {
                                    a.push("\n");
                                }
                                a.paras += 1;
                                a.paragraph_depth = (!empty).then_some(annotation_depth);
                            }
                            b"text:span" if !empty => {
                                let attrs = attrs(e);
                                let inherited = a.spans.last().map_or(0, |(_, style)| style.mask());
                                let own = attrs.get("text:style-name").and_then(|name| styles.comment_text.get(name)).map_or(0, |style| style.mask());
                                a.spans.push((annotation_depth, CommentStyle::from_mask(inherited | own)));
                            }
                            b"text:line-break" => a.push("\n"),
                            b"text:s" => {
                                let count = attrs(e).get("text:c").and_then(|n| n.parse::<usize>().ok()).unwrap_or(1);
                                let remaining = officina_grid::comments::MAX_TEXT.saturating_sub(a.text.len());
                                a.push(&" ".repeat(count.min(remaining)));
                            }
                            b"text:tab" => a.push("\t"),
                            _ => {}
                        }
                    }
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
                        let id = wb.add_sheet(&n).map_err(bad)?;
                        wb.sheet_mut(id).tab_color = a.get("table:style-name").and_then(|t| styles.tab_color.get(t)).copied();
                        wb.sheet_mut(id).sheet_hidden = a.get("table:style-name").is_some_and(|t| styles.hidden_tables.contains(t));
                        sheet = Some(id);
                        if let Some(area) = a.get("table:print-ranges").filter(|a| !a.is_empty()) {
                            print_areas.push((id, area.clone()));
                        }
                        (header_row, header_col) = (None, None);
                        row = 0;
                        col_x = 0;
                        col_defaults.clear();
                        if a.get("table:protected").is_some_and(|v| matches!(v.as_str(), "true" | "1")) {
                            wb.sheet_mut(id).protection = Some(crate::sheet_protection::read(
                                officina_grid::protection::ProtectionFormat::Ods,
                                a.iter().map(|(key, value)| (key.as_str(), value.as_str())),
                                &mut rep,
                            ));
                        }
                    }
                    b"table:table-header-rows" if !empty => header_row = Some(row),
                    b"table:table-header-columns" if !empty => header_col = Some(col_x),
                    b"table:table-column" => {
                        let a = attrs(e);
                        let n = a.get("table:number-columns-repeated").and_then(|v| v.parse::<u64>().ok()).unwrap_or(1).clamp(1, MAX_COLS as u64);
                        let shown = a.get("table:visibility").is_none_or(|v| v == "visible");
                        if let Some(w) = a.get("table:style-name").and_then(|s| styles.col_width.get(s)).filter(|_| shown) {
                            for c in col_x..(col_x + n).min(MAX_COLS as u64).min(col_x + 4096) {
                                widths.push((c as u32, w.round().max(2.0)));
                            }
                        }
                        // Hidden columns: no width, as Sheets hides them; the
                        // width their style gives kept (as a negative) to remember.
                        if !shown {
                            let keep = a.get("table:style-name").and_then(|s| styles.col_width.get(s)).map_or(0.0, |w| -w.round().max(2.0));
                            for c in col_x..(col_x + n).min(MAX_COLS as u64).min(col_x + 4096) {
                                widths.push((c as u32, keep));
                            }
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
                            let span = (row as u32, (row + row_repeat as u64 - 1).min(MAX_ROWS as u64 - 1) as u32);
                            hidden_rows.push(span);
                            if a.get("table:visibility").is_some_and(|v| v == "filter") {
                                filtered_rows.push(span);
                            }
                        }
                        col = 0;
                        row_cells.clear();
                        if empty {
                            if let Some(nm) = row_style.as_deref().filter(|&nm| nm != "Default" && row < u64::from(MAX_ROWS)) {
                                let id = *style_ids.entry(nm.to_string()).or_insert_with(|| wb.styles.intern(styles.resolve(nm)));
                                let end = (row + u64::from(row_repeat) - 1).min(u64::from(MAX_ROWS) - 1);
                                trailing_row_style(&mut runs, (row as u32, end as u32), 0, id, &mut row_style_budget, &mut rep);
                            }
                            row += row_repeat as u64;
                        }
                    }
                    b"table:table-cell" | b"table:covered-table-cell" => {
                        let a = attrs(e);
                        let repeat = a.get("table:number-columns-repeated").and_then(|v| v.parse::<u32>().ok()).unwrap_or(1).clamp(1, MAX_COLS);
                        let spanned = |k: &str| a.get(k).and_then(|v| v.parse::<u64>().ok()).filter(|&n| n >= 1).unwrap_or(1);
                        let (rs, cs) = (spanned("table:number-rows-spanned"), spanned("table:number-columns-spanned"));
                        if (rs > 1 || cs > 1) && name == b"table:table-cell" && merges.len() < officina_grid::merges::MAX_MERGES {
                            let (r1, c1) = (row + rs - 1, col + cs - 1);
                            if r1 < MAX_ROWS as u64 && c1 < MAX_COLS as u64 {
                                merges.push(officina_grid::addr::Rect { sheet: sheet.unwrap_or(0), r0: row as u32, c0: col as u32, r1: r1 as u32, c1: c1 as u32 });
                            }
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
                        let span = |k: &str| a.get(k).and_then(|v| v.parse::<u32>().ok()).filter(|&n| n >= 1);
                        let matrix = match (span("table:number-matrix-rows-spanned"), span("table:number-matrix-columns-spanned")) {
                            (None, None) => None,
                            (r, c) => Some((r.unwrap_or(1), c.unwrap_or(1))),
                        };
                        let formula = a.get("table:formula").cloned();
                        let live_formula = super::formula::live_formula(&a, formula.as_deref());
                        let c = CellIn { link: None, vtype, value, formula, live_formula, style: a.get("table:style-name").cloned(), repeat, text: String::new(), paras: 0, matrix, validation: a.get("table:content-validation-name").cloned() };
                        if empty {
                            row_cells.push((col, repeat, c));
                            col += repeat as u64;
                        } else {
                            cell = Some(c);
                        }
                    }
                    b"text:a" => {
                        if let Some(c) = cell.as_mut() {
                            if c.link.is_none() {
                                c.link = attrs(e).get("xlink:href").cloned();
                            }
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
                        if !empty {
                            annotation_depth = 1;
                            let thread = match super::comments::read_thread(e) {
                                Some(Some(thread)) => Some(thread),
                                Some(None) => {
                                    rep.note(Kind::Dropped, "malformed ODS comment thread metadata (kept the visible note)");
                                    None
                                }
                                None => None,
                            };
                            ann = Some(Annotation { thread, ..Annotation::default() });
                        }
                    }
                    b"draw:frame" if sheet.is_some() && frames.len() < officina_grid::charts::MAX_CHARTS + officina_grid::charts::MAX_PICTURES => {
                        let a = attrs(e);
                        let len = |k: &str| a.get(k).and_then(|v| length_px(v)).unwrap_or(0.0);
                        let f = super::chart::FrameIn {
                            sheet: sheet.unwrap_or(0),
                            cell: cell.is_some().then(|| ((row.min(MAX_ROWS as u64 - 1)) as u32, (col.min(MAX_COLS as u64 - 1)) as u32)),
                            x: len("svg:x"),
                            y: len("svg:y"),
                            w: len("svg:width"),
                            h: len("svg:height"),
                            end: a.get("table:end-cell-address").cloned(),
                            end_x: len("table:end-x"),
                            end_y: len("table:end-y"),
                            href: None,
                            image: None,
                            transformed: a.contains_key("draw:transform"),
                        };
                        if empty {
                            frames.push(f);
                        } else {
                            frame = Some(f);
                            frame_depth = 1;
                        }
                    }
                    b"draw:frame" | b"draw:custom-shape" | b"draw:g" => rep.note(Kind::Dropped, "images and drawings"),
                    b"script:event-listener" | b"form:button" => rep.note(Kind::Dropped, "macros bound to events and buttons (the macros are kept; nothing starts them)"),
                    b"table:named-range" | b"table:named-expression" => {
                        let a = attrs(e);
                        let src = if name == b"table:named-range" { a.get("table:cell-range-address").map(|r| format!("of:=[{r}]")) } else { a.get("table:expression").cloned() };
                        if let (Some(n), Some(src)) = (a.get("table:name"), src) {
                            if names.len() < officina_grid::names::MAX_NAMES {
                                let live = super::formula::live_formula(&a, Some(&src));
                                names.push((n.clone(), sheet, src, a.get("table:base-cell-address").cloned(), live));
                            }
                        }
                    }
                    b"table:data-pilot-table" => {
                        let a = attrs(e);
                        pilot = Some(super::pivot::TableIn {
                            name: a.get("table:name").cloned().unwrap_or_default(),
                            target: a.get("table:target-range-address").cloned().unwrap_or_default(),
                            grand: a.get("table:grand-total").cloned().unwrap_or_default(),
                            ..Default::default()
                        });
                    }
                    b"table:source-cell-range" => {
                        if let Some(t) = pilot.as_mut() {
                            t.source = attrs(e).get("table:cell-range-address").cloned().unwrap_or_default();
                        }
                    }
                    b"table:database-source-sql" | b"table:database-source-table" | b"table:database-source-query" | b"table:source-service" => {
                        if let Some(t) = pilot.as_mut() {
                            t.other_source = true;
                        }
                    }
                    b"table:data-pilot-field" => {
                        let a = attrs(e);
                        if let Some(t) = pilot.as_mut().filter(|t| t.fields.len() < 16_384) {
                            t.fields.push(super::pivot::FieldIn {
                                name: a.get("table:source-field-name").cloned().unwrap_or_default(),
                                orient: a.get("table:orientation").cloned().unwrap_or_default(),
                                function: a.get("table:function").cloned().unwrap_or_default(),
                                display: a.get("tableooo:display-name").cloned(),
                                data_layout: a.get("table:is-data-layout-field").is_some_and(|v| v == "true"),
                                selected: a.get("table:selected-page").cloned(),
                                ignore_selected: a.get("loext:ignore-selected-page").is_some_and(|v| v == "true"),
                                ..Default::default()
                            });
                        }
                    }
                    b"table:data-pilot-layout-info" => {
                        if let Some(f) = pilot.as_mut().and_then(|t| t.fields.last_mut()) {
                            let at = attrs(e);
                            f.compact |= at.get("loext:layout-mode").is_some_and(|v| v == "compact-layout");
                            f.outline_top = match at.get("table:layout-mode").map(String::as_str) {
                                Some("outline-subtotals-top") => Some(true),
                                Some("outline-subtotals-bottom") => Some(false),
                                _ => None,
                            };
                        }
                    }
                    b"table:data-pilot-subtotal" => {
                        if let Some(f) = pilot.as_mut().and_then(|t| t.fields.last_mut()) {
                            if let Some(function) = attrs(e).get("table:function") {
                                f.subtotals |= function != "none";
                                f.custom_subtotal |= !matches!(function.as_str(), "auto" | "none");
                            }
                        }
                    }
                    b"table:data-pilot-member" | b"table:data-pilot-group-member" if in_pilot_group => {
                        if let Some(group) = pilot.as_mut().and_then(|t| t.fields.last_mut()).and_then(|f| f.group.as_mut()) {
                            if group.size >= officina_grid::pivot::MAX_GROUP_MEMBERS {
                                group.clipped = true;
                            } else if let (Some(item), Some(name)) = (group.members.last_mut(), attrs(e).get("table:name")) {
                                item.members.push(Arc::from(name.as_str()));
                                group.size += 1;
                            }
                        }
                    }
                    b"table:data-pilot-member" => {
                        let a = attrs(e);
                        if let Some(f) = pilot.as_mut().and_then(|t| t.fields.last_mut()) {
                            if let Some(name) = a.get("table:name").filter(|_| f.members.len() < officina_grid::pivot::MAX_ITEMS) {
                                f.members.push(name.clone());
                                if a.get("table:display").is_some_and(|v| v == "false") {
                                    f.hidden.push(name.clone());
                                }
                            }
                        }
                    }
                    b"table:data-pilot-sort-info" => {
                        if let Some(f) = pilot.as_mut().and_then(|t| t.fields.last_mut()) {
                            let a = attrs(e);
                            f.sort = Some(super::pivot::SortIn {
                                mode: a.get("table:sort-mode").cloned().unwrap_or_default(),
                                descending: a.get("table:order").is_some_and(|v| v == "descending"),
                                caption: a.get("table:data-field").cloned().unwrap_or_default(),
                            });
                        }
                    }
                    b"table:data-pilot-field-reference" => {
                        if let Some(field) = pilot.as_mut().and_then(|t| t.fields.last_mut()) {
                            let a = attrs(e);
                            field.reference = Some(super::pivot::ReferenceIn {
                                kind: a.get("table:type").cloned().unwrap_or_default(),
                                field: a.get("table:field-name").cloned().unwrap_or_default(),
                                member_type: a.get("table:member-type").cloned().unwrap_or_default(),
                                member: a.get("table:member-name").cloned().unwrap_or_default(),
                            });
                        }
                    }
                    b"table:data-pilot-groups" => {
                        in_pilot_group = false;
                        if let Some(field) = pilot.as_mut().and_then(|t| t.fields.last_mut()) {
                            let a = attrs(e);
                            let dates = ["table:date-start", "table:date-end", "table:grouped-by"]
                                .iter().any(|key| a.contains_key(*key));
                            field.group = Some(super::pivot::GroupIn {
                                source: a.get("table:source-field-name").cloned().unwrap_or_default(),
                                dates,
                                part: a.get("table:grouped-by").cloned(),
                                start: a.get(if dates { "table:date-start" } else { "table:start" }).cloned(),
                                end: a.get(if dates { "table:date-end" } else { "table:end" }).cloned(),
                                step: a.get("table:step").cloned(),
                                ..Default::default()
                            });
                        }
                    }
                    b"table:data-pilot-group" => {
                        in_pilot_group = !empty;
                        if let Some(group) = pilot.as_mut().and_then(|t| t.fields.last_mut()).and_then(|f| f.group.as_mut()) {
                            if group.members.len() >= officina_grid::pivot::MAX_GROUP_MEMBERS {
                                group.clipped = true;
                            } else if let Some(name) = attrs(e).get("table:name") {
                                group.members.push(officina_grid::pivot::ItemGroup { name: Arc::from(name.as_str()), members: Vec::new() });
                            } else {
                                group.clipped = true;
                            }
                        }
                    }
                    b"calcext:conditional-format" => {
                        let a = attrs(e);
                        cf = Some(CfIn { target: a.get("calcext:target-range-address").cloned().unwrap_or_default(), entries: Vec::new() });
                    }
                    b"calcext:condition" => {
                        let a = attrs(e);
                        if let Some(f) = cf.as_mut() {
                            f.entries.push(CfEntry::Condition {
                                style: a.get("calcext:apply-style-name").cloned().unwrap_or_default(),
                                value: a.get("calcext:value").cloned().unwrap_or_default(),
                                base: a.get("calcext:base-cell-address").cloned().unwrap_or_default(),
                            });
                        }
                    }
                    b"calcext:color-scale" => {
                        if let Some(f) = cf.as_mut() {
                            f.entries.push(CfEntry::Scale(Vec::new()));
                        }
                    }
                    b"calcext:color-scale-entry" => {
                        let a = attrs(e);
                        if let Some(CfEntry::Scale(v)) = cf.as_mut().and_then(|f| f.entries.last_mut()) {
                            v.push((a.get("calcext:type").cloned().unwrap_or_default(), a.get("calcext:value").cloned().unwrap_or_default(), a.get("calcext:color").cloned().unwrap_or_default()));
                        }
                    }
                    b"calcext:data-bar" => {
                        let a = attrs(e);
                        if let Some(f) = cf.as_mut() {
                            f.entries.push(CfEntry::Bar { color: a.get("calcext:positive-color").cloned().unwrap_or_default(), points: Vec::new() });
                        }
                    }
                    b"calcext:formatting-entry" => {
                        let a = attrs(e);
                        let p = (a.get("calcext:type").cloned().unwrap_or_default(), a.get("calcext:value").cloned().unwrap_or_default());
                        match cf.as_mut().and_then(|f| f.entries.last_mut()) {
                            Some(CfEntry::Bar { points, .. }) => points.push(p),
                            Some(CfEntry::Icons { points, show_value, .. }) if points.len() < 5 => {
                                // LibreOffice puts the set's show-value on an entry.
                                if a.get("calcext:show-value").is_some_and(|v| v == "false") {
                                    *show_value = false;
                                }
                                points.push(p);
                            }
                            _ => {}
                        }
                    }
                    b"calcext:icon-set" => {
                        let a = attrs(e);
                        if let Some(f) = cf.as_mut() {
                            f.entries.push(CfEntry::Icons { set: a.get("calcext:icon-set-type").cloned().unwrap_or_default(), show_value: a.get("calcext:show-value").is_none_or(|v| v != "false"), points: Vec::new() });
                        }
                    }
                    b"calcext:date-is" => {
                        if let Some(f) = cf.as_mut() {
                            f.entries.push(CfEntry::Other(String::from_utf8_lossy(name).into_owned()));
                        }
                    }
                    b"table:content-validations" if !empty => {
                        let end = name.to_vec();
                        let mut b2 = Vec::new();
                        let span = r.read_to_end_into(quick_xml::name::QName(&end), &mut b2)?;
                        let inner = content.get(span.start as usize..span.end as usize).unwrap_or_default();
                        val_rules = super::validation::read_rules(inner);
                    }
                    b"table:database-range" => {
                        let a = attrs(e);
                        db = None;
                        let named = a.get("table:name").filter(|n| !n.starts_with("__Anonymous_Sheet_DB__") && officina_grid::names::valid_name(n));
                        if let Some(target) = a.get("table:target-range-address") {
                            let buttons = a.get("table:display-filter-buttons").is_some_and(|v| matches!(v.as_str(), "true" | "1"));
                            if let Some(n) = named {
                                if db_tables.len() >= officina_grid::tables::MAX_TABLES {
                                    continue;
                                }
                                db_tables.push((n.clone(), target.clone(), a.get("table:contains-header").map_or(true, |v| !matches!(v.as_str(), "false" | "0"))));
                            }
                            if !empty {
                                db = Some((named.cloned(), target.clone(), buttons, Vec::new()));
                            } else if buttons {
                                filters.push((named.cloned(), target.clone(), Vec::new()));
                            } else if named.is_none() {
                                rep.note(Kind::Dropped, "database ranges");
                            }
                        } else {
                            rep.note(Kind::Dropped, "database ranges");
                        }
                    }
                    b"table:iteration" => {
                        // OpenFormula's defaults: 100 steps, 0.001.
                        let a = attrs(e);
                        if a.get("table:status").is_some_and(|v| v == "enable") {
                            let count = a.get("table:steps").and_then(|v| v.parse::<u32>().ok()).unwrap_or(100);
                            let change = a.get("table:minimum-difference").and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.001);
                            if !wb.set_iteration(Some(officina_grid::book::Iteration { count, change })) {
                                rep.note(Kind::Dropped, "iteration settings");
                            }
                        }
                    }
                    b"table:calculation-settings" => {
                        if let Some(value) = attrs(e).get("table:search-criteria-must-apply-to-whole-cell") {
                            whole_cell_filter = !matches!(value.as_str(), "false" | "0");
                        }
                    }
                    b"table:filter" if !empty && db.is_some() => {
                        let a = attrs(e);
                        let end = name.to_vec();
                        let span = r.read_to_end_into(quick_xml::name::QName(&end), &mut Vec::new())?;
                        let inner = content.get(span.start as usize..span.end as usize).unwrap_or_default();
                        let local = !a.contains_key("table:target-range-address")
                            && !a.contains_key("table:condition-source-range-address")
                            && !a.get("table:display-duplicates").is_some_and(|v| matches!(v.as_str(), "false" | "0"));
                        match local.then(|| super::filter::parse(inner, whole_cell_filter)).flatten() {
                            Some(conds) => {
                                if let Some((_, _, _, current)) = &mut db {
                                    *current = conds;
                                }
                            }
                            None => {
                                db = None;
                                rep.note(Kind::Approximate, "unsupported filter expression (rows stay as the file hid them)");
                            }
                        }
                    }
                    _ => {}
                }
            }
            Event::Text(t) if annotation_depth > 0 => {
                if let Some(a) = ann.as_mut() {
                    let s = t.unescape().map_err(|e| OdsError::Xml(e.to_string()))?;
                    if a.in_creator {
                        let remaining = officina_grid::comments::MAX_AUTHOR.saturating_sub(a.author.len());
                        a.author.push_str(&s[..s.floor_char_boundary(s.len().min(remaining))]);
                    } else if a.paragraph_depth.is_some() {
                        a.push(&s);
                    }
                }
            }
            Event::Text(t) if in_p && annotation_depth == 0 && frame_depth == 0 => {
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
                if frame_depth > 0 {
                    frame_depth -= 1;
                    if frame_depth == 0 {
                        frames.extend(frame.take());
                    }
                    continue;
                }
                if annotation_depth > 0 {
                    annotation_depth -= 1;
                    if let Some(a) = ann.as_mut() {
                        if name == b"dc:creator" {
                            a.in_creator = false;
                        }
                        if a.paragraph_depth == Some(annotation_depth) {
                            a.paragraph_depth = None;
                        }
                        if a.spans.last().is_some_and(|(depth, _)| *depth == annotation_depth) {
                            a.spans.pop();
                        }
                    }
                    if annotation_depth == 0 {
                        if let (Some(a), Some(s)) = (ann.take(), sheet) {
                            if row < MAX_ROWS as u64 && col < MAX_COLS as u64 && comments.len() < officina_grid::comments::MAX_COMMENTS {
                                let visible = Comment::with_runs(a.author.trim(), &a.text, a.runs);
                                let comment = match a.thread {
                                    Some(thread) if thread.author.trim() == visible.author.as_ref()
                                        && thread.text == visible.text && visible.runs().is_empty() => thread,
                                    Some(_) => {
                                        rep.note(Kind::Dropped, "ODS comment threads whose visible notes changed (kept the edited note)");
                                        visible
                                    }
                                    None => visible,
                                };
                                comments.push((row as u32, col as u32, Some(comment)));
                            }
                            let _ = s;
                        }
                    }
                    continue;
                }
                match name {
                    b"table:data-pilot-group" | b"table:data-pilot-groups" | b"table:data-pilot-field" => {
                        in_pilot_group = false;
                    }
                    b"table:data-pilot-table" => {
                        if pilots.len() < officina_grid::pivot::MAX_PIVOTS {
                            pilots.extend(pilot.take());
                        }
                    }
                    b"calcext:conditional-format" => {
                        if cond_formats.len() < 4096 {
                            cond_formats.extend(cf.take());
                        }
                    }
                    b"table:database-range" => {
                        if let Some((name, target, buttons, conds)) = db.take() {
                            if buttons || !conds.is_empty() {
                                if !buttons {
                                    rep.note(Kind::Approximate, "filter buttons shown for a standard filter");
                                }
                                filters.push((name, target, conds));
                            } else if name.is_none() {
                                rep.note(Kind::Dropped, "database ranges");
                            }
                        }
                    }
                    b"text:p" | b"text:h" => in_p = false,
                    b"table:table-cell" | b"table:covered-table-cell" => {
                        if let Some(mut c) = cell.take() {
                            if let (Some(href), true) = (c.link.take(), row < MAX_ROWS as u64 && col < MAX_COLS as u64) {
                                if links.len() < officina_grid::links::MAX_LINKS {
                                    links.push((row as u32, col as u32, href));
                                }
                            }
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
                        // Cells naming a validation rule: runs across, grown down.
                        if !val_rules.is_empty() && row < MAX_ROWS as u64 {
                            let r1 = (row + row_repeat as u64 - 1).min(MAX_ROWS as u64 - 1) as u32;
                            let mut across: Vec<(&str, u32, u32)> = Vec::new();
                            for (c0, n, c) in &row_cells {
                                let Some(v) = c.validation.as_deref() else { continue };
                                if *c0 >= MAX_COLS as u64 {
                                    break;
                                }
                                let c1 = (c0 + *n as u64 - 1).min(MAX_COLS as u64 - 1) as u32;
                                match across.last_mut() {
                                    Some(l) if l.0 == v && l.2 as u64 + 1 == *c0 => l.2 = c1,
                                    _ => across.push((v, *c0 as u32, c1)),
                                }
                            }
                            for (v, c0, c1) in across {
                                let list = val_cells.entry(v.to_string()).or_default();
                                let key = (v.to_string(), s, c0, c1);
                                match val_open.get(&key) {
                                    Some(&i) if list[i].r1 + 1 == row as u32 => list[i].r1 = r1,
                                    _ if list.len() <= officina_grid::validation::MAX_RANGES => {
                                        val_open.insert(key, list.len());
                                        list.push(officina_grid::addr::Rect { sheet: s, r0: row as u32, c0, r1, c1 });
                                    }
                                    _ => {}
                                }
                            }
                        }
                        // Styles: the cell's, or the row's, or the column's default.
                        if runs.len() < col as usize {
                            runs.resize_with((col as usize).min(MAX_COLS as usize), Vec::new);
                        }
                        let r0 = row.min(MAX_ROWS as u64 - 1) as u32;
                        let r1 = (row + row_repeat as u64 - 1).min(MAX_ROWS as u64 - 1) as u32;
                        let mut id_of = |wb: &mut Workbook, nm: &str| -> StyleId {
                            if nm == "Default" {
                                return 0;
                            }
                            *style_ids.entry(nm.to_string()).or_insert_with(|| wb.styles.intern(styles.resolve(nm)))
                        };
                        // Column defaults that style anything, as (first, last, id).
                        let col_ids: Vec<(u64, u64, StyleId)> = col_defaults.iter().filter_map(|(a, b, n)| n.as_deref().map(|n| (*a, *b, id_of(&mut wb, n)))).filter(|x| x.2 != 0).collect();
                        for (c0, n, c) in &row_cells {
                            let own = c.style.as_deref().or(row_style.as_deref()).map(|name| id_of(&mut wb, name));
                            let date_format = (c.vtype == "date").then(|| c.value.as_deref().and_then(|text| wb.date_system().iso(text)))
                                .flatten().map(|(_, format)| format);
                            let last = (c0 + *n as u64).min(MAX_COLS as u64);
                            let col_styled = col_ids.iter().any(|(a, b, _)| *a < last && *b >= *c0);
                            // Trailing filler cells repeated to the last column carry the
                            // default style; skip runs past 1,024 columns of nothing.
                            if own.unwrap_or(0) == 0 && !col_styled && date_format.is_none() && last - c0 > 1024 {
                                continue;
                            }
                            for cc in *c0..last {
                                let base = own.unwrap_or_else(|| col_ids.iter().find(|(a, b, _)| *a <= cc && cc <= *b).map_or(0, |x| x.2));
                                let id = match date_format.filter(|_| wb.styles.format(base).is_general()) {
                                    Some(format) => wb.styles.intern(wb.styles.get(base).clone().with_num_fmt(format)),
                                    None => base,
                                };
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
                        if let Some(nm) = row_style.as_deref().filter(|_| row < u64::from(MAX_ROWS)) {
                            let id = id_of(&mut wb, nm);
                            trailing_row_style(&mut runs, (r0, r1), col.min(u64::from(MAX_COLS)) as usize, id, &mut row_style_budget, &mut rep);
                        }
                        row += row_repeat as u64;
                        row_cells.clear();
                    }
                    b"table:table-header-rows" => {
                        if let Some((sheet, start)) = sheet.zip(header_row.take()) {
                            let end = row.min(u64::from(MAX_ROWS));
                            if start < end && wb.define_name(officina_grid::print::ROWS, Some(sheet), &format!("=${}:${end}", start + 1)).is_err() {
                                rep.note(Kind::Dropped, "unsupported print titles");
                            }
                        }
                    }
                    b"table:table-header-columns" => {
                        if let Some((sheet, start)) = sheet.zip(header_col.take()) {
                            let end = col_x.min(u64::from(MAX_COLS));
                            if start < end {
                                use officina_grid::addr::col_name;
                                if wb.define_name(officina_grid::print::COLS, Some(sheet), &format!("=${}:${}", col_name(start as u32), col_name(end as u32 - 1))).is_err() {
                                    rep.note(Kind::Dropped, "unsupported print titles");
                                }
                            }
                        }
                    }
                    b"table:table" => {
                        if let Some(s) = sheet.take() {
                            finish_sheet(&mut wb, s, &mut widths, &mut heights, &mut runs);
                            row_style_budget = 262_144;
                            wb.sheet_mut(s).hidden = officina_grid::filter::merge(std::mem::take(&mut hidden_rows));
                            saved_filter_rows.insert(s, officina_grid::filter::merge(std::mem::take(&mut filtered_rows)));
                            wb.set_merges(s, std::mem::take(&mut merges));
                            wb.set_comments(s, std::mem::take(&mut comments));
                            let set = std::mem::take(&mut links).into_iter().map(|(r, c, h)| (r, c, officina_grid::links::Link::new(&from_odf_href(&h)))).filter(|x| x.2.is_some()).collect();
                            wb.set_links(s, set);
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

    for (sheet, target) in print_areas {
        let valid = print_area_formula(&wb.sheet_name(sheet), &target).is_some_and(|src| {
            wb.define_name(officina_grid::print::AREA, Some(sheet), &src).is_ok()
        });
        if !valid {
            rep.note(Kind::Dropped, "unsupported print areas");
        }
    }

    // Tables, from named database ranges; their columns are named by the header row.
    for (name, target, header) in db_tables {
        let Some((s, r0, c0, r1, c1)) = target_rect(&wb, &target) else { continue };
        let mut columns: Vec<Arc<str>> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for c in c0..=c1.min(c0 + 16_383) {
            let mut n = if header { wb.formatted(CellAddr::new(s, r0, c)).text.trim().to_string() } else { String::new() };
            if n.is_empty() || n.len() > officina_grid::tables::MAX_COL_NAME || !seen.insert(n.to_lowercase()) {
                n = format!("Column{}", c - c0 + 1);
                seen.insert(n.to_lowercase());
            }
            columns.push(n.as_str().into());
        }
        let t = officina_grid::tables::Table { name: name.as_str().into(), rect: officina_grid::addr::Rect { sheet: s, r0, c0, r1, c1 }, header, totals: false, columns, style: None, stripes: false, filter: None };
        if let Err(why) = wb.add_table(t) {
            rep.note_at(Kind::Dropped, "tables this engine can't use (kept as plain cells)", || format!("{name}: {why}"));
        }
    }

    // Auto-filters: their range names the sheet.
    for (name, target, mut conds) in filters {
        let Some((s, r0, c0, r1, c1)) = target_rect(&wb, &target) else { continue };
        if conds.iter().any(|(c, cond)| *c > c1 - c0 || matches!(cond, Cond::Expression(expr) if !expr.fits(c1 - c0 + 1))) {
            rep.note(Kind::Approximate, "filter field outside its range (rows stay as the file hid them)");
            continue;
        }
        if let Some(name) = name {
            let rect = officina_grid::addr::Rect { sheet: s, r0, c0, r1, c1 };
            if wb.table(&name).is_some_and(|t| t.filter_rect() == Some(rect)) {
                let rule = officina_grid::tables::TableFilter { conds, hidden: Vec::new() };
                let _ = wb.set_table_filter(&name, Some(rule));
            }
            continue;
        }
        for (c, _) in &mut conds {
            *c += c0;
        }
        wb.sheet_mut(s).filter = Some(officina_grid::filter::AutoFilter { rect: officina_grid::addr::Rect { sheet: s, r0, c0, r1, c1 }, conds });
    }

    // Names, before the formulas that use them; relative parts are from the base cell.
    for (n, scope, src, base, live) in names {
        let at = base.and_then(|b| super::formula::from_of(&format!("of:=[{b}]"))).and_then(|a| {
            let cell = a.rsplit('!').next().unwrap_or(&a).trim_start_matches('=').replace('$', "");
            officina_grid::addr::parse_a1(&cell)
        });
        let (row, col) = at.unwrap_or((0, 0));
        // LibreOffice writes a named expression's formula without its namespace.
        let src = if src.starts_with("of:") || src.starts_with("oooc:") || src.starts_with('=') { src } else { format!("of:={src}") };
        let sheet = scope.unwrap_or(0);
        let original = live.filter(|formula| {
            wb.parse(formula, sheet, row, col).is_ok_and(|expr| super::formula::requires_fallback(&wb, sheet, &expr))
        });
        let ok = original.or_else(|| super::formula::from_of(&src))
            .is_some_and(|a1| wb.define_name_at(&n, scope, &a1, row, col).is_ok());
        if !ok {
            rep.note_at(Kind::Dropped, "defined names this engine can't use", || format!("{n}: {}", src.chars().take(60).collect::<String>()));
        }
    }

    // Conditional formats, once every sheet exists.
    if styles.cond_maps && cond_formats.is_empty() {
        rep.note(Kind::Dropped, "conditional formats");
    }
    for f in cond_formats {
        match super::condfmt::make(&wb, &f, &|n| styles.dxf(n)) {
            Ok(c) => {
                if wb.add_cond_format(c).is_err() {
                    rep.note(Kind::Dropped, "conditional formats past the engine's limits");
                }
            }
            Err(why) => rep.note_at(Kind::Dropped, "conditional formats this engine can't use", || format!("{}: {why}", f.target)),
        }
    }

    // Data validation, once every sheet exists.
    super::validation::make(&mut wb, val_rules, val_cells, &mut rep);

    // Formulas, once every sheet exists.
    let mut cached = Vec::new();
    for StoredFormula { at, source, live, value, matrix } in fcells {
        let CellAddr { sheet: s, row: rr, col: cc } = at;
        // The foreign fallback remains authoritative when another app changes it.
        let original = live.filter(|formula| {
            wb.parse(formula, s, rr, cc).is_ok_and(|expr| super::formula::requires_fallback(&wb, s, &expr))
        });
        let formula = original.or_else(|| {
            let a1 = super::formula::from_of(&source)?;
            let expr = wb.parse(&a1, s, rr, cc).ok();
            Some(expr.map_or(a1, |expr| {
                let expr = if matrix.is_some() { expr } else { officina_grid::eval::arrays::legacy(expr) };
                officina_grid::formula::to_a1(&expr, rr, cc, &|i| wb.sheet_name(i))
            }))
        });
        let Some(formula) = formula else {
            wb.set_value(at, value);
            rep.note_at(Kind::Approximate, "formulas in another syntax, kept as their values", || source.chars().take(60).collect());
            continue;
        };
        if !matches!(value, Value::Empty | Value::Err(ErrorKind::Name)) && wb.parse(&formula, s, rr, cc).is_ok_and(|expr| {
            crate::xlsx::read::missing_top_level_function(&wb, s, &expr)
        }) {
            wb.set_value(at, value);
            rep.note_at(Kind::Approximate, "formulas this engine can't calculate yet, kept as their values", || formula.chars().take(60).collect());
            continue;
        }
        if wb.set_formula(at, &formula).is_ok() {
            if let Some((nr, nc)) = matrix.filter(|&(r, c)| r > 1 || c > 1) {
                let (r1, c1) = (rr.saturating_add(nr - 1).min(MAX_ROWS - 1), cc.saturating_add(nc - 1).min(MAX_COLS - 1));
                wb.claim_spill(at, officina_grid::Rect { sheet: s, r0: rr, c0: cc, r1, c1 });
            }
            out.formulas += 1;
            cached.push((at, value));
        } else {
            wb.set_value(at, value);
            rep.note_at(Kind::Approximate, "formulas this engine can't calculate yet, kept as their values", || formula.chars().take(60).collect());
        }
    }
    wb.recalc();
    for (sheet, rows) in saved_filter_rows {
        wb.restore_table_visibility(sheet, Some(&rows));
    }
    if wb.recalc_pending() {
        wb.recalc();
    }
    for (a, want) in cached {
        let got = wb.get(a);
        if !same(&got, &want) {
            out.differ += 1;
            rep.note_at(Kind::Approximate, "formula results that differ from the file's saved results", || format!("{}: {} vs {}", officina_grid::addr::a1(a.row, a.col), got.display(), want.display()));
        }
    }
    // Pivot tables, over the cells they wrote.
    super::pivot::make(&mut wb, pilots, &mut rep);
    // Charts, once every sheet and its sizes exist.
    super::chart::read_frames(&mut wb, &z, frames, &mut rep);
    // Macros, kept inert.
    super::macros::read(&z, &mut wb, &mut rep)?;
    // Frozen panes, from the view settings.
    if let Some(s) = z.read_name("settings.xml")? {
        super::settings::read(&mut wb, &s);
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
    if s == "Err:522" { Some(ErrorKind::Circ) } else { ErrorKind::from_text(s) }
}

/// The value a cell holds (for a formula cell, its cached result).
fn value_of(c: &CellIn, system: dates::DateSystem, rep: &mut Report) -> Value {
    let num = |s: &Option<String>| s.as_deref().and_then(|v| v.trim().parse::<f64>().ok()).filter(|x| x.is_finite());
    match c.vtype.as_str() {
        "float" | "percentage" | "currency" => num(&c.value).map(Value::Num).unwrap_or(Value::Empty),
        "date" => match c.value.as_deref().and_then(|text| date_value(system, text)) {
            Some(x) => Value::Num(x),
            None => {
                rep.note(Kind::Approximate, "invalid or out-of-range OpenDocument dates kept as text");
                Value::str(if c.text.is_empty() { c.value.as_deref().unwrap_or_default() } else { &c.text })
            }
        },
        "time" => c.value.as_deref().and_then(time_value).map(Value::Num).unwrap_or(Value::Empty),
        "boolean" => Value::Bool(c.value.as_deref().is_some_and(|v| v == "true" || v == "1")),
        "error" => Value::Err(error_of(c.text.trim()).unwrap_or(ErrorKind::Value)),
        "string" => {
            let t = c.value.clone().unwrap_or_else(|| c.text.clone());
            if c.formula.is_some() && c.value.as_deref().is_none_or(str::is_empty) {
                if let Some(e) = error_of(if t.is_empty() { &c.text } else { &t }) {
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

fn put_cell(wb: &mut Workbook, s: u16, r: u32, c: u32, cell: &CellIn, fcells: &mut Vec<StoredFormula>, rep: &mut Report) {
    let value = value_of(cell, wb.date_system(), rep);
    let at = CellAddr::new(s, r, c);
    match &cell.formula {
        Some(f) => fcells.push(StoredFormula { at, source: f.clone(), live: cell.live_formula.clone(), value, matrix: cell.matrix }),
        None => wb.set_value(at, value),
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
        assert_eq!(date_value(dates::DateSystem::Excel1900, "2026-11-14"), Some(46340.0));
        assert_eq!(date_value(dates::DateSystem::Excel1900, "2026-11-14T12:00:00"), Some(46340.5));
        assert_eq!(date_value(dates::DateSystem::Excel1900, "1900-02-28"), Some(59.0));
        assert_eq!(time_value("PT12H00M00S"), Some(0.5));
        assert_eq!(time_value("PT36H"), Some(1.5));
        assert_eq!(time_value("P1DT6H"), Some(1.25));
        assert_eq!(time_value("nope"), None);
    }
}

/// ODF print addresses describe positions even when the '$' markers are
/// omitted. Read both endpoints so a cross-sheet range cannot be misapplied.
fn print_area_formula(sheet: &str, address: &str) -> Option<String> {
    use officina_grid::addr::{col_name, parse_a1};
    if address.len() > 8192 { return None; }
    let parts = super::formula::split_outside_quotes(address, ':');
    if parts.len() > 2 { return None; }
    let mut cells = parts.iter().map(|part| {
        let (owner, cell) = super::formula::split_sheet(part)?;
        if owner.is_some_and(|owner| owner != sheet) { return None; }
        let (r, c) = parse_a1(&cell.replace('$', ""))?;
        Some(format!("${}${}", col_name(c), r + 1))
    });
    let start = cells.next()??;
    match cells.next() {
        Some(end) => Some(format!("={start}:{}", end?)),
        None => Some(format!("={start}")),
    }
}

/// A database range's `target-range-address`: sheet and range.
pub(super) fn target_rect(wb: &Workbook, target: &str) -> Option<(u16, u32, u32, u32, u32)> {
    let a1 = super::formula::from_of(&format!("of:=[{target}]"))?;
    let (sh, rf) = a1.trim_start_matches('=').rsplit_once('!')?;
    let sh = sh.trim_matches('\'').replace("''", "'");
    let (r0, c0, r1, c1) = crate::xlsx::read::parse_ref(rf)?;
    Some((wb.sheet_id(&sh)?, r0, c0, r1, c1))
}
