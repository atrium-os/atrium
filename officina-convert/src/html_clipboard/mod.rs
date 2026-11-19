//! A bounded HTML table for spreadsheet clipboard interchange. The writer
//! uses trusted cell data. The reader is linked only into the converter.

use std::fmt::Write as _;

use officina_grid::addr::{CellAddr, Rect};
use officina_grid::style::{HAlign, Style, VAlign};
use officina_grid::{Value, Workbook};

#[cfg(feature = "read")]
mod read;
#[cfg(feature = "read")]
pub use read::read;

pub const WORKER_KIND: &str = "clipboard-html";
pub const MIME: &str = "text/html";
pub const MAX_HTML_BYTES: usize = 1024 * 1024;
pub const MAX_CELLS: u64 = 16_384;
/// Upper bound on the worker’s native range output.
pub const MAX_NATIVE_BYTES: usize = 2 * MAX_HTML_BYTES;

/// An HTML table alternative for a copied range, or none beyond its limits.
pub fn write(wb: &Workbook, rect: Rect) -> Option<String> {
    if rect.cells() > MAX_CELLS { return None; }
    let mut html = String::from(r#"<html xmlns:x="urn:schemas-microsoft-com:office:excel"><head><meta charset="utf-8"></head><body><!--StartFragment--><table>"#);
    for row in rect.r0..=rect.r1 {
        html.push_str("<tr>");
        for col in rect.c0..=rect.c1 {
            let addr = CellAddr::new(rect.sheet, row, col);
            html.push_str("<td");
            let value = wb.value(rect.sheet, row, col);
            match &value {
                Value::Num(n) if n.is_finite() => write!(html, r#" x:num="{n}" sdval="{n}""#).ok()?,
                Value::Bool(b) => write!(html, r#" data-sheets-value="{{&quot;1&quot;:4,&quot;4&quot;:{}}}""#, u8::from(*b)).ok()?,
                Value::Str(_) => html.push_str(" x:str"),
                _ => {}
            }
            let cell_style = wb.style(addr);
            if !cell_style.num_fmt.eq_ignore_ascii_case("General") {
                // Calc uses a pair of locale IDs followed by the format code.
                write!(html, r#" sdnum="1033;1033;{}""#, crate::xml::esc(&cell_style.num_fmt)).ok()?;
            }
            // Calc's clipboard HTML importer reads the legacy cell attributes.
            if let Some(color) = cell_style.fill {
                write!(html, r##" bgcolor="#{:06x}""##, color & 0xffffff).ok()?;
            }
            if let Some(align) = horizontal(cell_style.h_align) {
                write!(html, r#" align="{align}""#).ok()?;
            }
            if let Some(align) = vertical(cell_style.v_align) {
                write!(html, r#" valign="{align}""#).ok()?;
            }
            let style = css(cell_style);
            if !style.is_empty() {
                write!(html, r#" style="{}""#, crate::xml::esc(&style)).ok()?;
            }
            html.push('>');
            let shown = match value {
                Value::Str(text) => text.to_string(),
                _ => wb.formatted(addr).text,
            };
            write_text(&mut html, &shown, cell_style)?;
            html.push_str("</td>");
            if html.len() > MAX_HTML_BYTES { return None; }
        }
        html.push_str("</tr>");
    }
    html.push_str("</table><!--EndFragment--></body></html>");
    (html.len() <= MAX_HTML_BYTES).then_some(html)
}

fn write_text(out: &mut String, shown: &str, style: &Style) -> Option<()> {
    let font = &style.font;
    let tags = [("b", font.bold), ("i", font.italic), ("u", font.underline), ("strike", font.strike)];
    let font_tag = font.name.is_some() || font.color.is_some();
    if font_tag {
        out.push_str("<font");
        if let Some(name) = &font.name {
            write!(out, r#" face="{}""#, crate::xml::esc(name)).ok()?;
        }
        if let Some(color) = font.color {
            write!(out, r##" color="#{:06x}""##, color & 0xffffff).ok()?;
        }
        out.push('>');
    }
    for (tag, enabled) in tags {
        if enabled { write!(out, "<{tag}>").ok()?; }
    }
    for (i, line) in shown.split('\n').enumerate() {
        if i > 0 { out.push_str("<br>"); }
        out.push_str(&crate::xml::esc(line));
    }
    for (tag, enabled) in tags.into_iter().rev() {
        if enabled { write!(out, "</{tag}>").ok()?; }
    }
    if font_tag { out.push_str("</font>"); }
    Some(())
}

fn horizontal(align: HAlign) -> Option<&'static str> {
    match align {
        HAlign::Left => Some("left"),
        HAlign::Center | HAlign::CenterAcross => Some("center"),
        HAlign::Right => Some("right"),
        HAlign::Justify => Some("justify"),
        _ => None,
    }
}

fn vertical(align: VAlign) -> Option<&'static str> {
    match align {
        VAlign::Top => Some("top"),
        VAlign::Center => Some("middle"),
        _ => None,
    }
}

fn css_string(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        if matches!(ch, '"' | '\\') { out.push('\\'); }
        if !ch.is_control() { out.push(ch); }
    }
    out.push('"');
    out
}

fn css(style: &Style) -> String {
    let mut out = String::new();
    let font = &style.font;
    if let Some(name) = &font.name {
        let _ = write!(out, "font-family:{};", css_string(name));
    }
    if let Some(size) = font.size {
        let _ = write!(out, "font-size:{}pt;", f64::from(size) / 20.0);
    }
    if font.bold { out.push_str("font-weight:bold;"); }
    if font.italic { out.push_str("font-style:italic;"); }
    if font.underline || font.strike {
        let _ = write!(out, "text-decoration:{}{};", if font.underline { "underline" } else { "" }, if font.strike { " line-through" } else { "" });
    }
    if let Some(color) = font.color {
        let _ = write!(out, "color:#{:06x};", color & 0xffffff);
    }
    if let Some(color) = style.fill {
        let _ = write!(out, "background-color:#{:06x};", color & 0xffffff);
    }
    if let Some(align) = horizontal(style.h_align) {
        let _ = write!(out, "text-align:{align};");
    }
    if let Some(align) = vertical(style.v_align) {
        let _ = write!(out, "vertical-align:{align};");
    }
    if style.wrap { out.push_str("white-space:pre-wrap;"); }
    if !style.num_fmt.eq_ignore_ascii_case("General") {
        let _ = write!(out, "mso-number-format:{};", css_string(&style.num_fmt));
    }
    out
}
