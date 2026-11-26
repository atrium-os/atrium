//! The parts of a workbook's look (`officina_grid::look`) that a file format has
//! no place for, as one `officina:look` element: the theme (in `.ods`), custom
//! table styles, shapes (where the format has none of ours) and Camera pictures.
//! Other programs ignore the element; we read it back. Sheets are named, not
//! numbered, so a reordered file still lands each part on its sheet.
//!
//! Reading is bounded: the element is found by its tag, parsed as a small tree
//! under the crate's XML limits, and each part is kept only if the look stays
//! valid with it (`Look::valid`), so a hostile file can't add a part the model
//! would refuse.

use std::fmt::Write as _;
use std::sync::Arc;

use officina_grid::charts::Anchor;
use officina_grid::look::{Camera, Look, Shape, TablePart, TableStyleDef, PART_NAMES};
use officina_grid::Workbook;

use crate::xml::esc;

pub(crate) const NS: &str = "urn:atrium:officina:look:1";

/// Which parts to write.
#[derive(Clone, Copy)]
pub(crate) struct Parts {
    pub theme: bool,
    pub shapes: bool,
}

fn hex(c: Option<u32>) -> String {
    c.map_or_else(|| "none".into(), |c| format!("{c:06X}"))
}

fn corner(c: (u32, u32, f32, f32)) -> String {
    format!("{} {} {} {}", c.0, c.1, c.2, c.3)
}

/// The `officina:look` element, or nothing when there is nothing to write.
pub(crate) fn write(wb: &Workbook, parts: Parts) -> String {
    let look = wb.workbook_look();
    let theme = parts.theme && look.theme != officina_grid::look::Theme::default();
    let shapes = parts.shapes && !look.shapes.is_empty();
    if !theme && !shapes && look.table_styles.is_empty() && look.cameras.is_empty() {
        return String::new();
    }
    let mut o = format!(r#"<officina:look xmlns:officina="{NS}">"#);
    if theme {
        let t = &look.theme;
        let colors: Vec<String> = t.colors.iter().map(|c| format!("{c:06X}")).collect();
        let _ = write!(o, r#"<officina:theme officina:name="{}" officina:heading="{}" officina:body="{}" officina:colors="{}"/>"#, esc(&t.name), esc(&t.heading), esc(&t.body), colors.join(" "));
    }
    for s in &look.table_styles {
        let _ = write!(o, r#"<officina:table-style officina:name="{}">"#, esc(&s.name));
        for (k, p) in s.parts.iter().enumerate() {
            if let Some(p) = p {
                let _ = write!(o, r#"<officina:part officina:kind="{}" officina:fill="{}" officina:text="{}" officina:bold="{}" officina:border="{}"/>"#,
                    PART_NAMES[k], hex(p.fill), hex(p.text), p.bold as u8, hex(p.border));
            }
        }
        o.push_str("</officina:table-style>");
    }
    if shapes {
        for s in &look.shapes {
            let alt = s.alt.as_deref().map_or(String::new(), |a| format!(r#" officina:alt="{}""#, esc(a).replace('\n', "&#10;")));
            let _ = write!(o, r#"<officina:shape officina:name="{}" officina:sheet="{}" officina:from="{}" officina:to="{}" officina:geom="{}" officina:fill="{}" officina:line="{}" officina:text-color="{}" officina:size="{}" officina:bold="{}"{alt}>{}</officina:shape>"#,
                esc(&s.name), esc(&wb.sheet_name(s.anchor.sheet)), corner(s.anchor.from), corner(s.anchor.to), esc(&s.geom), hex(s.fill), hex(s.line), hex(s.text_color), s.text_size, s.bold as u8, esc(&s.text));
        }
    }
    for c in &look.cameras {
        let r = c.source;
        let _ = write!(o, r#"<officina:camera officina:name="{}" officina:sheet="{}" officina:from="{}" officina:to="{}" officina:source-sheet="{}" officina:source="{} {} {} {}"/>"#,
            esc(&c.name), esc(&wb.sheet_name(c.anchor.sheet)), corner(c.anchor.from), corner(c.anchor.to), esc(&wb.sheet_name(r.sheet)), r.r0, r.c0, r.r1, r.c1);
    }
    o.push_str("</officina:look>");
    o
}

/// The workbook's Custom Views as an `officina:views` element: their wire
/// form (`GridOp::CustomViews`) in hex, which carries each view's hidden
/// rows, filter and columns exactly. Nothing when there are none.
pub(crate) fn views(wb: &Workbook) -> String {
    let list = wb.custom_views();
    if list.is_empty() {
        return String::new();
    }
    let bytes = officina_grid::ops::GridOp::CustomViews(list.to_vec()).encode();
    let mut o = format!(r#"<officina:views xmlns:officina="{NS}">"#);
    for b in bytes {
        let _ = write!(o, "{b:02x}");
    }
    o.push_str("</officina:views>");
    o
}

#[cfg(feature = "read")]
mod read {
    use super::*;
    use crate::report::{Kind, Report};
    use crate::xml::El;

    fn color(v: Option<&str>) -> Result<Option<u32>, ()> {
        match v {
            None | Some("none") => Ok(None),
            Some(v) if v.len() == 6 => u32::from_str_radix(v, 16).map(Some).map_err(|_| ()),
            _ => Err(()),
        }
    }

    fn corner(v: Option<&str>) -> Option<(u32, u32, f32, f32)> {
        let mut it = v?.split(' ');
        let r = it.next()?.parse().ok()?;
        let c = it.next()?.parse().ok()?;
        let x: f32 = it.next()?.parse().ok()?;
        let y: f32 = it.next()?.parse().ok()?;
        (it.next().is_none() && x.is_finite() && y.is_finite()).then_some((r, c, x, y))
    }

    fn anchor(wb: &Workbook, e: &El) -> Option<Anchor> {
        let sheet = wb.sheet_id(e.attr("officina:sheet")?)?;
        Some(Anchor { sheet, from: corner(e.attr("officina:from"))?, to: corner(e.attr("officina:to"))? })
    }

    fn text(s: &str) -> Arc<str> {
        s.into()
    }

    fn shape(wb: &Workbook, e: &El) -> Option<Shape> {
        Some(Shape {
            name: text(e.attr("officina:name")?),
            anchor: anchor(wb, e)?,
            geom: text(e.attr("officina:geom")?),
            fill: color(e.attr("officina:fill")).ok()?,
            line: color(e.attr("officina:line")).ok()?,
            text: e.text().into(),
            text_color: color(e.attr("officina:text-color")).ok()?,
            text_size: e.attr("officina:size")?.parse().ok()?,
            bold: e.attr("officina:bold") == Some("1"),
            alt: e.attr("officina:alt").map(text),
        })
    }

    fn camera(wb: &Workbook, e: &El) -> Option<Camera> {
        let sheet = wb.sheet_id(e.attr("officina:source-sheet")?)?;
        let n: Vec<u32> = e.attr("officina:source")?.split(' ').map(|v| v.parse().ok()).collect::<Option<_>>()?;
        let [r0, c0, r1, c1] = n[..] else { return None };
        Some(Camera { name: text(e.attr("officina:name")?), anchor: anchor(wb, e)?, source: officina_grid::addr::Rect { sheet, r0, c0, r1, c1 } })
    }

    fn table_style(e: &El) -> Option<TableStyleDef> {
        let mut parts = [None; 7];
        for p in e.children("officina:part") {
            let k = PART_NAMES.iter().position(|n| Some(*n) == p.attr("officina:kind"))?;
            parts[k] = Some(TablePart {
                fill: color(p.attr("officina:fill")).ok()?,
                text: color(p.attr("officina:text")).ok()?,
                bold: p.attr("officina:bold") == Some("1"),
                border: color(p.attr("officina:border")).ok()?,
            });
        }
        Some(TableStyleDef { name: text(e.attr("officina:name")?), parts })
    }

    fn theme(e: &El) -> Option<officina_grid::look::Theme> {
        let c: Vec<u32> = e.attr("officina:colors")?.split(' ').map(|v| color(Some(v)).ok().flatten()).collect::<Option<_>>()?;
        Some(officina_grid::look::Theme {
            name: text(e.attr("officina:name")?),
            colors: c.try_into().ok()?,
            heading: text(e.attr("officina:heading")?),
            body: text(e.attr("officina:body")?),
        })
    }

    /// Finds the `officina:look` element in `part` and adds its parts to `look`,
    /// each only if the look stays valid for `wb`'s sheets with it.
    pub(crate) fn read(part: &[u8], wb: &Workbook, look: &mut Look, rep: &mut Report) {
        let Some(at) = find(part, b"<officina:look") else { return };
        let end = find(&part[at..], b"</officina:look>").map(|e| at + e + b"</officina:look>".len());
        let span = match end {
            Some(end) => &part[at..end],
            None => {
                rep.note(Kind::Dropped, "a malformed Officina look (theme, table styles, shapes, cameras)");
                return;
            }
        };
        let root = match crate::xml::parse(span) {
            Ok(r) => r,
            Err(_) => {
                rep.note(Kind::Dropped, "a malformed Officina look (theme, table styles, shapes, cameras)");
                return;
            }
        };
        let sheets = wb.sheet_count();
        let mut dropped = false;
        for e in root.els() {
            match e.name.as_str() {
                "officina:theme" => match theme(e) {
                    Some(t) => {
                        let old = std::mem::replace(&mut look.theme, t);
                        if look.valid(sheets).is_err() {
                            look.theme = old;
                            dropped = true;
                        }
                    }
                    None => dropped = true,
                },
                "officina:table-style" => match table_style(e) {
                    Some(s) => {
                        look.table_styles.push(s);
                        if look.valid(sheets).is_err() {
                            look.table_styles.pop();
                            dropped = true;
                        }
                    }
                    None => dropped = true,
                },
                "officina:shape" => match shape(wb, e) {
                    Some(s) => {
                        look.shapes.push(s);
                        if look.valid(sheets).is_err() {
                            look.shapes.pop();
                            dropped = true;
                        }
                    }
                    None => dropped = true,
                },
                "officina:camera" => match camera(wb, e) {
                    Some(c) => {
                        look.cameras.push(c);
                        if look.valid(sheets).is_err() {
                            look.cameras.pop();
                            dropped = true;
                        }
                    }
                    None => dropped = true,
                },
                _ => {}
            }
        }
        if dropped {
            rep.note(Kind::Dropped, "invalid or excess themes, table styles, shapes or cameras");
        }
    }

    /// The Custom Views in `part`'s `officina:views` element, if any: views on
    /// sheets the workbook doesn't have are dropped, and a malformed element
    /// is reported and gives none.
    pub(crate) fn views(part: &[u8], wb: &mut Workbook, rep: &mut Report) {
        let Some(at) = find(part, b"<officina:views") else { return };
        let Some(start) = part[at..].iter().position(|&b| b == b'>').map(|p| at + p + 1) else { return };
        let Some(end) = find(&part[start..], b"</officina:views>").map(|e| start + e) else {
            rep.note(Kind::Dropped, "malformed custom views");
            return;
        };
        let hex = &part[start..end];
        let digit = |b: u8| (b as char).to_digit(16);
        let bytes: Option<Vec<u8>> = (hex.len() % 2 == 0).then(|| hex.chunks(2).map(|p| Some((digit(p[0])? * 16 + digit(p[1])?) as u8)).collect()).flatten();
        let Some(op) = bytes.and_then(|b| officina_grid::ops::GridOp::decode(&b).ok()) else {
            rep.note(Kind::Dropped, "malformed custom views");
            return;
        };
        let officina_grid::ops::GridOp::CustomViews(mut list) = op else {
            rep.note(Kind::Dropped, "malformed custom views");
            return;
        };
        let n = list.len();
        list.retain(|v| (v.sheet as usize) < wb.sheet_count());
        if list.len() < n {
            rep.note(Kind::Dropped, "custom views of sheets the file doesn't have");
        }
        officina_core::history::Op::apply(officina_grid::ops::GridOp::CustomViews(list), wb);
    }

    fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
        hay.windows(needle.len()).position(|w| w == needle)
    }
}

#[cfg(feature = "read")]
pub(crate) use read::{read, views as read_views};
