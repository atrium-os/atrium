//! Read a clipboard table in the converter. HTML supplies data and a small
//! set of presentation hints. Nothing is executed or fetched.

use std::borrow::Cow;
use std::collections::HashMap;

use navigator_dom::{Dom, Handle, Kind};
use officina_grid::addr::MAX_COLS;
use officina_grid::clip::Clip;
use officina_grid::style::{HAlign, Style, VAlign, MAX_CODE};
use officina_grid::Value;

use super::{MAX_CELLS, MAX_HTML_BYTES};

type Classes = HashMap<String, String>;

pub fn read(input: &[u8]) -> Result<Clip, &'static str> {
    if input.len() > MAX_HTML_BYTES {
        return Err("HTML clipboard data exceeds 1 MiB.");
    }
    let html = fragment_context(input).ok_or("Invalid HTML clipboard header.")?;
    let html = std::str::from_utf8(&html).map_err(|_| "Clipboard HTML must use UTF-8.")?;
    let dom = navigator_dom::parse(html.trim_end_matches('\0'));
    if dom.nodes.len() > 100_000 {
        return Err("HTML clipboard data has too many elements.");
    }
    let fragment = Fragment::new(&dom);
    let classes = classes(&dom);
    let Some(table) = fragment.table(&dom) else {
        // Google Sheets can offer only an annotated span for one cell.
        let cell = (0..dom.nodes.len() as Handle).find(|&h| {
            dom.tag(h) == Some("span") && fragment.includes(h) && dom.connected(h)
                && (dom.attr(h, "data-sheets-value").is_some() || dom.attr(h, "data-sheets-userformat").is_some())
        }).ok_or("The clipboard HTML does not contain a spreadsheet table.")?;
        return Clip::from_table(vec![vec![read_cell(&dom, cell, &classes, &fragment)?]])
            .ok_or("The clipboard cell is empty.");
    };
    let rows = dom.nodes[table as usize].children.iter().flat_map(|h| match dom.tag(*h) {
        Some("thead" | "tbody" | "tfoot") => dom.nodes[*h as usize].children.as_slice(),
        _ => std::slice::from_ref(h),
    }).copied().filter(|&row| fragment.includes(row) && dom.tag(row) == Some("tr"));

    let mut result = Vec::new();
    let mut width = 0;
    for row in rows {
        let mut values = Vec::new();
        for cell in children(&dom, row).filter(|&h| fragment.includes(h) && matches!(dom.tag(h), Some("td" | "th"))) {
            if ["rowspan", "colspan"].into_iter().any(|name| {
                dom.attr(cell, name).is_some_and(|s| s.trim().parse::<u32>() != Ok(1))
            }) {
                return Err("HTML clipboard tables with merged cells are unsupported.");
            }
            width = width.max(values.len() + 1);
            if width > MAX_COLS as usize || (result.len() + 1) as u64 * width as u64 > MAX_CELLS {
                return Err("The clipboard table exceeds 16,384 cells.");
            }
            values.push(read_cell(&dom, cell, &classes, &fragment)?);
        }
        if (result.len() + 1) as u64 * width.max(1) as u64 > MAX_CELLS {
            return Err("The clipboard table exceeds 16,384 cells.");
        }
        result.push(values);
    }
    Clip::from_table(result).ok_or("The clipboard table is empty.")
}

fn children(dom: &Dom, node: Handle) -> impl Iterator<Item = Handle> + '_ {
    dom.nodes[node as usize].children.iter().copied()
}

// A native clipboard bridge can supply Windows CF_HTML framing. Retain
// the outer document for its class rules, even when it has a fragment.
fn fragment_context(input: &[u8]) -> Option<Cow<'_, [u8]>> {
    if !input.starts_with(b"Version:") { return Some(input.into()); }
    let header = input.splitn(33, |&b| b == b'\n');
    let mut offsets = HashMap::new();
    for line in header.take(32) {
        let line = std::str::from_utf8(line).ok()?;
        if line.trim_start().starts_with('<') { break; }
        if let Some((key, value)) = line.split_once(':') {
            if matches!(key, "StartHTML" | "EndHTML" | "StartFragment" | "EndFragment") {
                offsets.insert(key, value.trim().parse::<i64>().ok()?);
            }
        }
    }
    let context = match (offsets.get("StartHTML"), offsets.get("EndHTML")) {
        (Some(&-1), Some(&-1)) => ("StartFragment", "EndFragment"),
        _ => ("StartHTML", "EndHTML"),
    };
    let start = usize::try_from(*offsets.get(context.0)?).ok()?;
    let end = usize::try_from(*offsets.get(context.1)?).ok()?;
    let html = input.get(start..end)?;
    if context.0 == "StartFragment" || html.windows(17).any(|w| w.eq_ignore_ascii_case(b"<!--StartFragment")) {
        return Some(html.into());
    }
    let offset = |name| usize::try_from(*offsets.get(name)?).ok()?.checked_sub(start);
    match (offset("StartFragment"), offset("EndFragment")) {
        (Some(first), Some(last)) if first <= last && last <= html.len() => {
            let mut out = Vec::with_capacity(html.len() + 40);
            out.extend_from_slice(&html[..first]);
            out.extend_from_slice(b"<!--StartFragment-->");
            out.extend_from_slice(&html[first..last]);
            out.extend_from_slice(b"<!--EndFragment-->");
            out.extend_from_slice(&html[last..]);
            Some(out.into())
        }
        _ if offsets.contains_key("StartFragment") || offsets.contains_key("EndFragment") => None,
        _ => Some(html.into()),
    }
}

/// A clipboard fragment can start inside a table. Its ancestor tags supply
/// structure and styles, while cells and text outside it stay out of the paste.
struct Fragment {
    first: Handle,
    end: Handle,
    ancestors: Vec<Handle>,
}

impl Fragment {
    fn new(dom: &Dom) -> Self {
        let comment = |which: &str| {
            dom.nodes.iter().position(|n| matches!(&n.kind, Kind::Comment(s) if s.trim().eq_ignore_ascii_case(which)))
                .map(|i| i as Handle)
        };
        let start = comment("StartFragment");
        let end = comment("EndFragment").unwrap_or(dom.nodes.len() as Handle);
        let mut ancestors = Vec::new();
        let mut parent = start.and_then(|h| dom.get(h)?.parent);
        while let Some(h) = parent {
            ancestors.push(h);
            parent = dom.nodes[h as usize].parent;
        }
        Self { first: start.unwrap_or(0), end, ancestors }
    }

    fn includes(&self, h: Handle) -> bool {
        (self.first..self.end).contains(&h) || (h < self.first && self.ancestors.contains(&h))
    }

    fn table(&self, dom: &Dom) -> Option<Handle> {
        let is_table = |h| dom.tag(h) == Some("table") && !dom.is_foreign(h);
        self.ancestors.iter().copied().find(|&h| is_table(h))
            .or_else(|| (self.first..self.end).find(|&h| is_table(h) && dom.connected(h)))
    }
}

// Only literal class selectors from small style blocks are recognized.
// This handles Excel's .xlNN rules without a general CSS cascade.
fn classes(dom: &Dom) -> Classes {
    let mut out = Classes::new();
    for h in dom.by_tag_anywhere("style") {
        for child in children(dom, h) {
            let Kind::Text(text) = &dom.nodes[child as usize].kind else { continue; };
            if text.len() > 64 * 1024 { continue; }
            let css = text.trim().trim_start_matches("<!--");
            for rule in css.split('}') {
                let Some((selectors, declarations)) = rule.split_once('{') else { continue; };
                for selector in selectors.split(',').map(str::trim) {
                    let Some(class) = selector.strip_prefix('.') else { continue; };
                    if class.len() > 64 || class.is_empty() || !class.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')) {
                        continue;
                    }
                    if out.len() >= 512 && !out.contains_key(class) { continue; }
                    let stored = out.entry(class.to_owned()).or_default();
                    if stored.len() + declarations.len() < 4096 {
                        stored.push_str(declarations);
                        stored.push(';');
                    }
                }
            }
        }
    }
    out
}

fn read_cell(dom: &Dom, cell: Handle, classes: &Classes, fragment: &Fragment) -> Result<(Value, Style), &'static str> {
    let mut style = Style::default();
    if dom.tag(cell) == Some("th") { style.font.bold = true; }
    attributes(dom, cell, classes, &mut style);
    let mut text = String::new();
    let mut typed = sheets_value(dom.attr(cell, "data-sheets-value"));
    let mut font = None;
    let mut mixed = false;
    let mut stack: Vec<_> = dom.nodes[cell as usize].children.iter().rev().map(|&h| (h, style.font.clone())).collect();
    while let Some((h, mut current)) = stack.pop() {
        if !fragment.includes(h) { continue; }
        match &dom.nodes[h as usize].kind {
            Kind::Text(s) => {
                text.push_str(s);
                if !s.trim().is_empty() {
                    if let Some(previous) = &font {
                        mixed |= previous != &current;
                    } else {
                        font = Some(current);
                    }
                }
            }
            Kind::Element(tag) => {
                match tag.as_str() {
                    "br" => { text.push('\n'); continue; }
                    "script" | "style" | "template" | "table" | "img" | "iframe" | "svg" | "object" => continue,
                    "b" | "strong" => current.bold = true,
                    "i" | "em" => current.italic = true,
                    "u" => current.underline = true,
                    "s" | "strike" | "del" => current.strike = true,
                    _ => {}
                }
                if typed.is_none() {
                    typed = sheets_value(dom.attr(h, "data-sheets-value"));
                }
                let mut inline = Style { font: current, ..Default::default() };
                attributes(dom, h, classes, &mut inline);
                stack.extend(dom.nodes[h as usize].children.iter().rev().map(|&c| (c, inline.font.clone())));
            }
            _ => {}
        }
    }
    if let (false, Some(font)) = (mixed, font) { style.font = font; }

    if text.len() > officina_grid::strings::MAX_TEXT {
        return Err("A clipboard cell has too much text.");
    }
    let value = typed
        .or_else(|| ["x:num", "sdval"].into_iter().find_map(|key| {
            let raw = dom.attr(cell, key)?;
            let raw = if raw.is_empty() { text.trim() } else { raw.trim() };
            raw.parse::<f64>().ok().filter(|n| n.is_finite()).map(Value::num)
        }))
        .unwrap_or_else(|| {
            let s = if dom.attr(cell, "x:str").is_some() { &text } else { text.trim() };
            if s.is_empty() { Value::Empty } else { Value::str(s) }
        });
    Ok((value, style))
}

fn sheets_value(raw: Option<&str>) -> Option<Value> {
    let raw = raw?;
    if raw.len() > officina_grid::strings::MAX_TEXT { return None; }
    let data: serde_json::Value = serde_json::from_str(raw).ok()?;
    match data.get("1")?.as_u64()? {
        2 => Some(Value::str(data.get("2")?.as_str()?)),
        3 => Some(Value::num(data.get("3")?.as_f64()?)),
        4 => data.get("4").and_then(|v| v.as_bool().or_else(|| match v.as_u64()? {
            0 => Some(false),
            1 => Some(true),
            _ => None,
        })).map(Value::Bool),
        _ => None,
    }
}

fn attributes(dom: &Dom, h: Handle, classes: &Classes, style: &mut Style) {
    if let Some(a) = dom.attr(h, "align") { horizontal(a, style); }
    if let Some(a) = dom.attr(h, "valign") { vertical(a, style); }
    if let Some(rgb) = dom.attr(h, "bgcolor").and_then(color) { style.fill = Some(rgb); }
    if dom.tag(h) == Some("font") {
        if let Some(rgb) = dom.attr(h, "color").and_then(color) { style.font.color = Some(rgb); }
        if let Some(name) = dom.attr(h, "face") { style.font.name = font_name(name); }
    }
    if let Some(names) = dom.attr(h, "class") {
        for name in names.split_ascii_whitespace().take(16) {
            if let Some(css) = classes.get(name) { declarations(css, style); }
        }
    }
    if let Some(css) = dom.attr(h, "style").filter(|s| s.len() <= 4096) {
        declarations(css, style);
    }
}

fn horizontal(s: &str, style: &mut Style) {
    match s {
        "left" => style.h_align = HAlign::Left,
        "right" => style.h_align = HAlign::Right,
        "center" => style.h_align = HAlign::Center,
        "justify" => style.h_align = HAlign::Justify,
        _ => {}
    }
}

fn vertical(s: &str, style: &mut Style) {
    match s {
        "top" => style.v_align = VAlign::Top,
        "middle" | "center" => style.v_align = VAlign::Center,
        "bottom" => style.v_align = VAlign::Bottom,
        _ => {}
    }
}

fn font_name(s: &str) -> Option<std::sync::Arc<str>> {
    let name = unquote(s.split(',').next()?.trim());
    (!name.is_empty() && name.len() <= MAX_CODE).then(|| name.into())
}

fn unquote(s: &str) -> String {
    let s = s.strip_prefix('"').and_then(|s| s.strip_suffix('"'))
        .or_else(|| s.strip_prefix('\'').and_then(|s| s.strip_suffix('\''))).unwrap_or(s);
    let mut chars = s.chars();
    let mut out = String::new();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(ch) = chars.next() { out.push(ch); }
        } else {
            out.push(ch);
        }
    }
    out
}

fn declarations(css: &str, style: &mut Style) {
    let (mut start, mut quote, mut escaped) = (0, None, false);
    for (i, ch) in css.char_indices().chain(std::iter::once((css.len(), ';'))) {
        if escaped { escaped = false; continue; }
        if ch == '\\' { escaped = true; continue; }
        if let Some(q) = quote {
            if q == ch { quote = None; }
        } else if matches!(ch, '\'' | '"') {
            quote = Some(ch);
        } else if ch == ';' {
            property(&css[start..i], style);
            start = i + 1;
        }
    }
}

fn property(decl: &str, style: &mut Style) {
    let Some((key, value)) = decl.split_once(':') else { return; };
    let value = value.trim();
    let lower = value.to_ascii_lowercase();
    let f = &mut style.font;
    match key.trim().to_ascii_lowercase().as_str() {
        "font-family" => f.name = font_name(value),
        "font-size" => {
            if let Some(pt) = lower.strip_suffix("pt").and_then(|s| s.parse::<f64>().ok()).or_else(|| {
                lower.strip_suffix("px").and_then(|s| s.parse::<f64>().ok()).map(|n| n * 0.75)
            }).filter(|pt| (1.0..=409.0).contains(pt)) {
                f.size = Some((pt * 20.0).round() as u16);
            }
        }
        "font-weight" => f.bold = matches!(lower.as_str(), "bold" | "bolder") || lower.parse::<u16>().is_ok_and(|n| n >= 600),
        "font-style" => f.italic = matches!(lower.as_str(), "italic" | "oblique"),
        "text-decoration" | "text-decoration-line" => {
            f.underline = lower.split_ascii_whitespace().any(|s| s == "underline");
            f.strike = lower.split_ascii_whitespace().any(|s| s == "line-through");
        }
        "color" => { if let Some(rgb) = color(&lower) { f.color = Some(rgb); } }
        "background" | "background-color" => { if let Some(rgb) = color(&lower) { style.fill = Some(rgb); } }
        "text-align" => horizontal(&lower, style),
        "vertical-align" => vertical(&lower, style),
        "white-space" => style.wrap = matches!(lower.as_str(), "normal" | "pre-wrap" | "pre-line" | "break-spaces"),
        "mso-number-format" => {
            let code = unquote(value);
            if !code.is_empty() && code.len() <= MAX_CODE { style.num_fmt = code.into(); }
        }
        _ => {}
    }
}

fn color(s: &str) -> Option<u32> {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix('#') {
        let n = u32::from_str_radix(hex, 16).ok()?;
        return match hex.len() {
            6 => Some(n),
            3 => Some(((n & 0xf00) << 8 | (n & 0xf0) << 4 | (n & 0xf)) * 17),
            _ => None,
        };
    }
    if let Some(rgb) = s.strip_prefix("rgb(").and_then(|s| s.strip_suffix(')')) {
        let mut parts = rgb.split(',');
        let mut next = || parts.next()?.trim().parse::<u8>().ok().map(u32::from);
        let n = next()? << 16 | next()? << 8 | next()?;
        return parts.next().is_none().then_some(n);
    }
    match s {
        "black" => Some(0),
        "white" => Some(0xffffff),
        "red" => Some(0xff0000),
        "blue" => Some(0x0000ff),
        "yellow" => Some(0xffff00),
        _ => None,
    }
}
