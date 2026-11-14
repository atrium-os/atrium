//! `xl/styles.xml` in and out: number formats, fonts, fills, borders and alignment
//! (ECMA-376 Part 1 §18.8). In, a cell's `s` is an index into `cellXfs`, read here
//! as one [`Style`] per entry; colours given by theme are resolved through
//! `xl/theme/theme1.xml`. Out, every style of the workbook becomes one `cellXfs` entry
//! (its index is the style id).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

use officina_grid::numfmt::{self, PALETTE};
use officina_grid::style::{Edge, Font, HAlign, Line, Style, VAlign};
use officina_grid::Workbook;

use crate::report::{Kind, Report};

/// `cellXfs` entries a file may have (Excel's limit is 64,000).
pub const MAX_XFS: usize = 65_536;

fn attrs(e: &BytesStart) -> HashMap<Vec<u8>, String> {
    let mut m = HashMap::new();
    for a in e.attributes().with_checks(false).flatten() {
        if let Ok(v) = a.unescape_value() {
            m.insert(a.key.local_name().as_ref().to_vec(), v.into_owned());
        }
    }
    m
}

fn truthy(v: Option<&String>) -> bool {
    v.is_some_and(|v| v != "0" && v != "false")
}

/// The theme's twelve colours, in `theme="n"` order (light 1 and dark 1 swapped, as
/// Excel reads them: 0 is lt1, 1 dk1, 2 lt2, 3 dk2, then accents and links).
pub fn theme_colors(xml: &[u8]) -> Vec<u32> {
    let mut r = Reader::from_reader(xml);
    let mut buf = Vec::new();
    let names = ["dk1", "lt1", "dk2", "lt2", "accent1", "accent2", "accent3", "accent4", "accent5", "accent6", "hlink", "folHlink"];
    let mut found: [Option<u32>; 12] = [None; 12];
    let mut cur: Option<usize> = None;
    let mut in_scheme = false;
    loop {
        buf.clear();
        match r.read_event_into(&mut buf) {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let n = e.local_name();
                let n = n.as_ref();
                if n == b"clrScheme" {
                    in_scheme = true;
                } else if in_scheme {
                    if let Some(i) = names.iter().position(|x| x.as_bytes() == n) {
                        cur = Some(i);
                    } else if let Some(i) = cur {
                        let a = attrs(&e);
                        let v = match n {
                            b"srgbClr" => a.get(b"val".as_slice()),
                            b"sysClr" => a.get(b"lastClr".as_slice()),
                            _ => None,
                        };
                        if let Some(v) = v.and_then(|v| u32::from_str_radix(v, 16).ok()) {
                            found[i].get_or_insert(v & 0xFF_FFFF);
                        }
                    }
                }
            }
            Ok(Event::End(e)) if e.local_name().as_ref() == b"clrScheme" => break,
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    // Office's default theme where the file says nothing.
    const OFFICE: [u32; 12] = [0x000000, 0xFFFFFF, 0x44546A, 0xE7E6E6, 0x4472C4, 0xED7D31, 0xA5A5A5, 0xFFC000, 0x5B9BD5, 0x70AD47, 0x0563C1, 0x954F72];
    let c: Vec<u32> = (0..12).map(|i| found[i].unwrap_or(OFFICE[i])).collect();
    let mut out = c.clone();
    out.swap(0, 1);
    out.swap(2, 3);
    out
}

/// Excel's tint: lighten (positive) or darken (negative) in HLS.
pub fn tint(rgb: u32, t: f64) -> u32 {
    if t == 0.0 || !t.is_finite() {
        return rgb;
    }
    let t = t.clamp(-1.0, 1.0);
    let (r, g, b) = (((rgb >> 16) & 255) as f64 / 255.0, ((rgb >> 8) & 255) as f64 / 255.0, (rgb & 255) as f64 / 255.0);
    let (mx, mn) = (r.max(g).max(b), r.min(g).min(b));
    let mut l = (mx + mn) / 2.0;
    let d = mx - mn;
    let (h, s) = if d == 0.0 {
        (0.0, 0.0)
    } else {
        let s = if l > 0.5 { d / (2.0 - mx - mn) } else { d / (mx + mn) };
        let h = if mx == r {
            (g - b) / d + if g < b { 6.0 } else { 0.0 }
        } else if mx == g {
            (b - r) / d + 2.0
        } else {
            (r - g) / d + 4.0
        } / 6.0;
        (h, s)
    };
    l = if t < 0.0 { l * (1.0 + t) } else { l * (1.0 - t) + t };
    let hue = |p: f64, q: f64, mut t: f64| {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        if t < 1.0 / 6.0 {
            p + (q - p) * 6.0 * t
        } else if t < 0.5 {
            q
        } else if t < 2.0 / 3.0 {
            p + (q - p) * (2.0 / 3.0 - t) * 6.0
        } else {
            p
        }
    };
    let (r, g, b) = if s == 0.0 {
        (l, l, l)
    } else {
        let q = if l < 0.5 { l * (1.0 + s) } else { l + s - l * s };
        let p = 2.0 * l - q;
        (hue(p, q, h + 1.0 / 3.0), hue(p, q, h), hue(p, q, h - 1.0 / 3.0))
    };
    let c = |x: f64| ((x * 255.0).round().clamp(0.0, 255.0)) as u32;
    c(r) << 16 | c(g) << 8 | c(b)
}

struct Colors<'a> {
    theme: &'a [u32],
    indexed: &'a [u32],
}

impl Colors<'_> {
    /// A `<color>`-like element's colour; `None` for automatic.
    fn of(&self, a: &HashMap<Vec<u8>, String>) -> Option<u32> {
        let t = a.get(b"tint".as_slice()).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
        if truthy(a.get(b"auto".as_slice())) {
            return None;
        }
        let base = if let Some(v) = a.get(b"rgb".as_slice()) {
            let v = u32::from_str_radix(v.trim_start_matches('#'), 16).ok()?;
            v & 0xFF_FFFF
        } else if let Some(i) = a.get(b"theme".as_slice()).and_then(|v| v.parse::<usize>().ok()) {
            *self.theme.get(i)?
        } else if let Some(i) = a.get(b"indexed".as_slice()).and_then(|v| v.parse::<usize>().ok()) {
            match i {
                64 => return None, // system foreground: automatic
                65 => 0xFFFFFF,
                _ => *self.indexed.get(i)?,
            }
        } else {
            return None;
        };
        Some(tint(base, t))
    }
}

#[derive(Default, Clone)]
struct FontIn {
    name: Option<String>,
    size: Option<f64>,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
    color: Option<u32>,
}

/// One [`Style`] per `cellXfs` entry. Never fails: what it cannot read is the default,
/// with a note.
pub fn read(xml: &[u8], theme: &[u32], rep: &mut Report) -> Vec<Style> {
    let mut r = Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut fmts: HashMap<u32, String> = HashMap::new();
    let mut fonts: Vec<FontIn> = Vec::new();
    let mut fills: Vec<Option<u32>> = Vec::new();
    let mut borders: Vec<[Edge; 4]> = Vec::new();
    let mut indexed: Vec<u32> = PALETTE.to_vec();
    let mut custom_indexed: Vec<u32> = Vec::new();
    // Raw xfs: (numFmtId, fontId, fillId, borderId, alignment attrs)
    let mut xfs: Vec<(u32, usize, usize, usize, HashMap<Vec<u8>, String>)> = Vec::new();
    #[derive(PartialEq, Clone, Copy)]
    enum In {
        None,
        Fonts,
        Fills,
        Borders,
        CellXfs,
        Indexed,
        Other,
    }
    let mut sect = In::None;
    let mut font = FontIn::default();
    let mut fill: Option<u32> = None;
    let mut fill_kind = String::new();
    let mut border = [Edge::default(); 4];
    let mut edge: Option<usize> = None;
    let mut in_xf = false;
    // Colours are resolved after the custom palette is known; keep raw attrs meanwhile.
    let mut font_colors: Vec<Option<HashMap<Vec<u8>, String>>> = Vec::new();
    let mut fill_colors: Vec<Option<HashMap<Vec<u8>, String>>> = Vec::new();
    let mut border_colors: Vec<[Option<HashMap<Vec<u8>, String>>; 4]> = Vec::new();
    let mut fcol: Option<HashMap<Vec<u8>, String>> = None;
    let mut fillcol: Option<HashMap<Vec<u8>, String>> = None;
    let mut bcol: [Option<HashMap<Vec<u8>, String>>; 4] = Default::default();
    loop {
        buf.clear();
        let ev = match r.read_event_into(&mut buf) {
            Ok(ev) => ev,
            Err(_) => {
                rep.note(Kind::Approximate, "a damaged styles part: some cells shown unstyled");
                break;
            }
        };
        let (e, empty) = match ev {
            Event::Start(e) => (e, false),
            Event::Empty(e) => (e, true),
            Event::End(e) => {
                match e.local_name().as_ref() {
                    b"fonts" | b"fills" | b"borders" | b"cellXfs" | b"indexedColors" | b"cellStyleXfs" | b"dxfs" => sect = In::None,
                    b"font" if sect == In::Fonts => {
                        fonts.push(std::mem::take(&mut font));
                        font_colors.push(fcol.take());
                    }
                    b"fill" if sect == In::Fills => {
                        fills.push(fill.take());
                        fill_colors.push(fillcol.take());
                        fill_kind.clear();
                    }
                    b"border" if sect == In::Borders => {
                        borders.push(std::mem::take(&mut border));
                        border_colors.push(std::mem::take(&mut bcol));
                    }
                    b"left" | b"start" | b"right" | b"end" | b"top" | b"bottom" => edge = None,
                    b"xf" => in_xf = false,
                    _ => {}
                }
                continue;
            }
            Event::Eof => break,
            _ => continue,
        };
        let n = e.local_name();
        let n = n.as_ref();
        match (sect, n) {
            (_, b"numFmt") => {
                let a = attrs(&e);
                if let (Some(id), Some(code)) = (a.get(b"numFmtId".as_slice()).and_then(|v| v.parse().ok()), a.get(b"formatCode".as_slice())) {
                    fmts.insert(id, code.chars().take(officina_grid::style::MAX_CODE).collect());
                }
            }
            (_, b"fonts") => sect = In::Fonts,
            (_, b"fills") => sect = In::Fills,
            (_, b"borders") => sect = In::Borders,
            (_, b"cellXfs") => sect = In::CellXfs,
            (_, b"indexedColors") => sect = In::Indexed,
            (_, b"cellStyleXfs") | (_, b"dxfs") => sect = In::Other,
            (In::Indexed, b"rgbColor") => {
                if let Some(v) = attrs(&e).get(b"rgb".as_slice()).and_then(|v| u32::from_str_radix(v, 16).ok()) {
                    custom_indexed.push(v & 0xFF_FFFF);
                }
            }
            (In::Fonts, b"font") if empty => {
                fonts.push(FontIn::default());
                font_colors.push(None);
            }
            (In::Fonts, _) => {
                let a = attrs(&e);
                let val = a.get(b"val".as_slice());
                match n {
                    b"name" => font.name = val.cloned(),
                    b"sz" => font.size = val.and_then(|v| v.parse().ok()).filter(|v: &f64| v.is_finite() && *v > 0.0 && *v < 410.0),
                    b"b" => font.bold = val.is_none_or(|v| v != "0" && v != "false"),
                    b"i" => font.italic = val.is_none_or(|v| v != "0" && v != "false"),
                    b"strike" => font.strike = val.is_none_or(|v| v != "0" && v != "false"),
                    b"u" => font.underline = val.is_none_or(|v| v != "none"),
                    b"color" => fcol = Some(a),
                    _ => {}
                }
            }
            (In::Fills, b"fill") if empty => {
                fills.push(None);
                fill_colors.push(None);
            }
            (In::Fills, b"patternFill") => {
                fill_kind = attrs(&e).get(b"patternType".as_slice()).cloned().unwrap_or_else(|| "none".into());
                if fill_kind != "none" && fill_kind != "solid" && fill_kind != "gray125" {
                    rep.note(Kind::Approximate, "patterned fills shown solid");
                }
            }
            (In::Fills, b"fgColor") if fill_kind != "none" && fill_kind != "gray125" => fillcol = Some(attrs(&e)),
            (In::Fills, b"bgColor") if fill_kind != "none" && fill_kind != "solid" && fillcol.is_none() => fillcol = Some(attrs(&e)),
            (In::Fills, b"gradientFill") => {
                rep.note(Kind::Approximate, "gradient fills shown in their first colour");
                fill_kind = "gradient".into();
            }
            (In::Fills, b"color") if fill_kind == "gradient" && fillcol.is_none() => fillcol = Some(attrs(&e)),
            (In::Borders, b"border") if empty => {
                borders.push([Edge::default(); 4]);
                border_colors.push(Default::default());
            }
            (In::Borders, b"left" | b"start" | b"right" | b"end" | b"top" | b"bottom") => {
                let i = match n {
                    b"left" | b"start" => 0,
                    b"right" | b"end" => 1,
                    b"top" => 2,
                    _ => 3,
                };
                border[i].line = attrs(&e).get(b"style".as_slice()).and_then(|s| Line::from_name(s)).unwrap_or(Line::None);
                edge = if empty { None } else { Some(i) };
            }
            (In::Borders, b"diagonal") => {
                if attrs(&e).get(b"style".as_slice()).is_some_and(|s| s != "none") {
                    rep.note(Kind::Dropped, "diagonal borders");
                }
            }
            (In::Borders, b"color") => {
                if let Some(i) = edge {
                    bcol[i] = Some(attrs(&e));
                }
            }
            (In::CellXfs, b"xf") => {
                let a = attrs(&e);
                let id = |k: &[u8]| a.get(k).and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
                xfs.push((id(b"numFmtId") as u32, id(b"fontId"), id(b"fillId"), id(b"borderId"), HashMap::new()));
                in_xf = !empty;
                if xfs.len() > MAX_XFS {
                    rep.note(Kind::Approximate, "more cell styles than a workbook holds: the rest shown unstyled");
                    xfs.pop();
                    break;
                }
            }
            (In::CellXfs, b"alignment") if in_xf => {
                if let Some(x) = xfs.last_mut() {
                    x.4 = attrs(&e);
                }
            }
            (In::CellXfs, b"protection") => {
                // Locked and shown is every cell's default; only a change is lost.
                let a = attrs(&e);
                let unlocked = a.get(b"locked".as_slice()).is_some_and(|v| v == "0" || v == "false");
                if unlocked || truthy(a.get(b"hidden".as_slice())) {
                    rep.note(Kind::Dropped, "cell protection settings (unlocked or hidden formulas)");
                }
            }
            _ => {}
        }
    }
    if !custom_indexed.is_empty() {
        for (i, c) in custom_indexed.into_iter().enumerate().take(64) {
            indexed[i] = c;
        }
    }
    let colors = Colors { theme, indexed: &indexed };
    let col = |a: &Option<HashMap<Vec<u8>, String>>| a.as_ref().and_then(|a| colors.of(a));
    let fonts: Vec<FontIn> = fonts.into_iter().zip(font_colors.iter()).map(|(mut f, c)| {
        f.color = col(c);
        f
    }).collect();
    let fills: Vec<Option<u32>> = fill_colors.iter().map(col).collect();
    let borders: Vec<[Edge; 4]> = borders
        .into_iter()
        .zip(border_colors.iter())
        .map(|(mut b, c)| {
            for i in 0..4 {
                if b[i].line != Line::None {
                    b[i].color = col(&c[i]);
                }
            }
            b
        })
        .collect();
    let base = fonts.first().cloned().unwrap_or_default();
    let mut out = Vec::with_capacity(xfs.len());
    for (nf, fo, fi, bo, al) in xfs {
        let mut s = Style::default();
        let code = fmts.get(&nf).cloned().or_else(|| numfmt::builtin(nf).map(str::to_string));
        match code {
            Some(c) => {
                if !numfmt::NumFmt::parse(&c).is_exact() {
                    rep.note_at(Kind::Approximate, "number formats with parts shown plainly", || c.clone());
                }
                s.num_fmt = Arc::from(c.as_str());
            }
            None => rep.note_at(Kind::Approximate, "locale-specific built-in number formats shown as General", || format!("numFmtId {nf}")),
        }
        if let Some(f) = fonts.get(fo) {
            s.font = Font {
                name: f.name.as_ref().filter(|n| Some(*n) != base.name.as_ref()).map(|n| Arc::from(n.as_str())),
                size: f.size.filter(|z| Some(*z) != base.size).map(|z| (z * 20.0).round() as u16),
                bold: f.bold,
                italic: f.italic,
                underline: f.underline,
                strike: f.strike,
                color: f.color.filter(|c| Some(*c) != base.color),
            };
        }
        s.fill = fills.get(fi).copied().flatten();
        if let Some(b) = borders.get(bo) {
            s.borders = *b;
        }
        if let Some(h) = al.get(b"horizontal".as_slice()) {
            s.h_align = match h.as_str() {
                "left" => HAlign::Left,
                "center" => HAlign::Center,
                "right" => HAlign::Right,
                "fill" => HAlign::Fill,
                "justify" => HAlign::Justify,
                "centerContinuous" => HAlign::CenterAcross,
                "distributed" => HAlign::Distributed,
                _ => HAlign::General,
            };
        }
        if let Some(v) = al.get(b"vertical".as_slice()) {
            s.v_align = match v.as_str() {
                "top" => VAlign::Top,
                "center" => VAlign::Center,
                "justify" => VAlign::Justify,
                "distributed" => VAlign::Distributed,
                _ => VAlign::Bottom,
            };
        }
        s.wrap = truthy(al.get(b"wrapText".as_slice()));
        s.indent = al.get(b"indent".as_slice()).and_then(|v| v.parse::<u32>().ok()).map_or(0, |v| v.min(250) as u8);
        s.rotation = al.get(b"textRotation".as_slice()).and_then(|v| v.parse::<i32>().ok()).map_or(0, |v| match v {
            0..=90 => v as i16,
            91..=180 => -((v - 90) as i16),
            255 => 255,
            _ => 0,
        });
        if s.rotation != 0 {
            rep.note(Kind::Approximate, "rotated text shown level");
        }
        if truthy(al.get(b"shrinkToFit".as_slice())) {
            rep.note(Kind::Approximate, "shrink-to-fit text shown at its size");
        }
        out.push(s);
    }
    if out.is_empty() {
        out.push(Style::default());
    }
    out
}

fn hex(c: u32) -> String {
    format!("FF{:06X}", c & 0xFF_FFFF)
}

fn esc_attr(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            c if (c as u32) < 0x20 => {}
            c => o.push(c),
        }
    }
    o
}

/// `xl/styles.xml` for the workbook: `cellXfs` entry `i` is style id `i`.
pub fn write(wb: &Workbook) -> Vec<u8> {
    let mut fmts: Vec<(u32, String)> = Vec::new();
    let mut fonts: Vec<Font> = vec![Font::default()];
    let mut fills: Vec<Option<u32>> = vec![None, None]; // none, gray125 (required)
    let mut borders: Vec<[Edge; 4]> = vec![[Edge::default(); 4]];
    let mut xfs = String::new();
    let n = wb.styles.len();
    for (_, s) in wb.styles.iter() {
        let nf = match numfmt::builtin_id(&s.num_fmt) {
            Some(id) => id,
            None => match fmts.iter().find(|(_, c)| *c == *s.num_fmt) {
                Some((id, _)) => *id,
                None => {
                    let id = 164 + fmts.len() as u32;
                    fmts.push((id, s.num_fmt.to_string()));
                    id
                }
            },
        };
        let fo = match fonts.iter().position(|f| *f == s.font) {
            Some(i) => i,
            None => {
                fonts.push(s.font.clone());
                fonts.len() - 1
            }
        };
        let fi = match s.fill {
            None => 0,
            Some(c) => match fills.iter().skip(2).position(|f| *f == Some(c)) {
                Some(i) => i + 2,
                None => {
                    fills.push(Some(c));
                    fills.len() - 1
                }
            },
        };
        let bo = match borders.iter().position(|b| *b == s.borders) {
            Some(i) => i,
            None => {
                borders.push(s.borders);
                borders.len() - 1
            }
        };
        let align = s.h_align != HAlign::General || s.v_align != VAlign::Bottom || s.wrap || s.indent > 0 || s.rotation != 0;
        let _ = write!(xfs, r#"<xf numFmtId="{nf}" fontId="{fo}" fillId="{fi}" borderId="{bo}" xfId="0""#);
        if nf != 0 {
            xfs.push_str(r#" applyNumberFormat="1""#);
        }
        if fo != 0 {
            xfs.push_str(r#" applyFont="1""#);
        }
        if fi != 0 {
            xfs.push_str(r#" applyFill="1""#);
        }
        if bo != 0 {
            xfs.push_str(r#" applyBorder="1""#);
        }
        if align {
            xfs.push_str(r#" applyAlignment="1"><alignment"#);
            let h = match s.h_align {
                HAlign::General => "",
                HAlign::Left => "left",
                HAlign::Center => "center",
                HAlign::Right => "right",
                HAlign::Fill => "fill",
                HAlign::Justify => "justify",
                HAlign::CenterAcross => "centerContinuous",
                HAlign::Distributed => "distributed",
            };
            if !h.is_empty() {
                let _ = write!(xfs, r#" horizontal="{h}""#);
            }
            let v = match s.v_align {
                VAlign::Top => "top",
                VAlign::Center => "center",
                VAlign::Bottom => "",
                VAlign::Justify => "justify",
                VAlign::Distributed => "distributed",
            };
            if !v.is_empty() {
                let _ = write!(xfs, r#" vertical="{v}""#);
            }
            if s.wrap {
                xfs.push_str(r#" wrapText="1""#);
            }
            if s.indent > 0 {
                let _ = write!(xfs, r#" indent="{}""#, s.indent);
            }
            if s.rotation != 0 {
                let r = if s.rotation < 0 && s.rotation >= -90 { 90 - s.rotation as i32 } else { s.rotation as i32 };
                let _ = write!(xfs, r#" textRotation="{r}""#);
            }
            xfs.push_str("/></xf>");
        } else {
            xfs.push_str("/>");
        }
    }
    let mut o = String::from(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">"#);
    if !fmts.is_empty() {
        let _ = write!(o, r#"<numFmts count="{}">"#, fmts.len());
        for (id, c) in &fmts {
            let _ = write!(o, r#"<numFmt numFmtId="{id}" formatCode="{}"/>"#, esc_attr(c));
        }
        o.push_str("</numFmts>");
    }
    let _ = write!(o, r#"<fonts count="{}">"#, fonts.len());
    for f in &fonts {
        o.push_str("<font>");
        if f.bold {
            o.push_str("<b/>");
        }
        if f.italic {
            o.push_str("<i/>");
        }
        if f.strike {
            o.push_str("<strike/>");
        }
        if f.underline {
            o.push_str("<u/>");
        }
        let sz = f.size.map_or(11.0, |z| z as f64 / 20.0);
        let _ = write!(o, r#"<sz val="{sz}"/>"#);
        match f.color {
            Some(c) => {
                let _ = write!(o, r#"<color rgb="{}"/>"#, hex(c));
            }
            None => o.push_str(r#"<color theme="1"/>"#),
        }
        let _ = write!(o, r#"<name val="{}"/><family val="2"/>"#, esc_attr(f.name.as_deref().unwrap_or("Calibri")));
        if f.name.is_none() {
            o.push_str(r#"<scheme val="minor"/>"#);
        }
        o.push_str("</font>");
    }
    o.push_str("</fonts>");
    let _ = write!(o, r#"<fills count="{}"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill>"#, fills.len());
    for f in fills.iter().skip(2) {
        let _ = write!(o, r#"<fill><patternFill patternType="solid"><fgColor rgb="{}"/><bgColor indexed="64"/></patternFill></fill>"#, hex(f.unwrap_or(0xFFFFFF)));
    }
    o.push_str("</fills>");
    let _ = write!(o, r#"<borders count="{}">"#, borders.len());
    for b in &borders {
        o.push_str("<border>");
        for (i, tag) in ["left", "right", "top", "bottom"].iter().enumerate() {
            let e = b[i];
            if e.line == Line::None {
                let _ = write!(o, "<{tag}/>");
            } else {
                let _ = write!(o, r#"<{tag} style="{}">"#, e.line.name());
                match e.color {
                    Some(c) => {
                        let _ = write!(o, r#"<color rgb="{}"/>"#, hex(c));
                    }
                    None => o.push_str(r#"<color indexed="64"/>"#),
                }
                let _ = write!(o, "</{tag}>");
            }
        }
        o.push_str("<diagonal/></border>");
    }
    o.push_str("</borders>");
    o.push_str(r#"<cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs>"#);
    let _ = write!(o, r#"<cellXfs count="{n}">{xfs}</cellXfs>"#);
    o.push_str(r#"<cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#);
    o.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tints() {
        // Excel's "White, Background 1, Darker 15%" and "Blue, Accent 1, Lighter 80%".
        assert_eq!(tint(0xFFFFFF, -0.1499984740745262), 0xD9D9D9);
        assert_eq!(tint(0x4472C4, 0.7999816888943144), 0xDAE3F3);
        assert_eq!(tint(0x123456, 0.0), 0x123456);
    }

    #[test]
    fn write_then_read() {
        let mut wb = Workbook::new();
        wb.add_sheet("S").unwrap();
        let mut a = Style::default().with_num_fmt("#,##0.00;[Red]-#,##0.00");
        a.font.bold = true;
        a.font.color = Some(0x336699);
        a.font.size = Some(280);
        a.font.name = Some(Arc::from("Georgia"));
        a.fill = Some(0xFFF2CC);
        a.h_align = HAlign::Center;
        a.v_align = VAlign::Top;
        a.wrap = true;
        a.indent = 2;
        a.borders = [Edge { line: Line::Thin, color: None }, Edge { line: Line::Double, color: Some(0xFF0000) }, Edge::default(), Edge { line: Line::Medium, color: Some(0x00FF00) }];
        let b = Style::default().with_num_fmt("yyyy-mm-dd");
        let c = Style::default().with_num_fmt("0%");
        let ia = wb.styles.intern(a.clone());
        let ib = wb.styles.intern(b.clone());
        let ic = wb.styles.intern(c.clone());
        let xml = write(&wb);
        let mut rep = Report::default();
        let theme = theme_colors(b"");
        let back = read(&xml, &theme, &mut rep);
        assert_eq!(back.len(), 4);
        assert_eq!(back[0], Style::default());
        assert_eq!(back[ia as usize], a);
        assert_eq!(back[ib as usize], b);
        assert_eq!(back[ic as usize], c);
        assert!(rep.is_empty(), "{:?}", rep.human());
    }

    #[test]
    fn hostile_styles_never_panic() {
        let theme = theme_colors(b"<a:clrScheme><a:dk1><a:srgbClr val=\"zz\"/></a:dk1>");
        for x in [&b"<styleSheet><cellXfs><xf numFmtId=\"999999999999\" fontId=\"-1\"/></cellXfs>"[..], b"<fonts><font><sz val=\"NaN\"/><color theme=\"99\" tint=\"inf\"/></font></fonts><cellXfs><xf fontId=\"0\"><alignment textRotation=\"9999\" indent=\"-4\"/></xf></cellXfs>", b"\xff\xfe<", b""] {
            let mut rep = Report::default();
            let s = read(x, &theme, &mut rep);
            assert!(!s.is_empty());
        }
    }
}
