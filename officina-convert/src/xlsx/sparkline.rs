//! Sparklines in `.xlsx` (docs/spec/officina.md §16.143): Excel 2010's
//! `x14:sparklineGroups` in the worksheet's `extLst`. Each group shares a
//! type, colours and markers; each sparkline is a source range (`xm:f`,
//! sheet-qualified) and the cell it is drawn in (`xm:sqref`).

use std::fmt::Write as _;

use officina_grid::addr::a1;
use officina_grid::sparkline::{SparkKind, Sparkline, DEFAULT_COLOR, DEFAULT_NEGATIVE, MAX_SPARKLINES};
use officina_grid::Workbook;
use quick_xml::events::Event;

use crate::xml::esc;

pub const EXT_URI: &str = "{05C60535-1F16-4fd2-B633-F4F36F0B64E0}";

fn sheet_ref(wb: &Workbook, sheet: u16) -> String {
    let name = wb.sheet_name(sheet);
    if !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !name.starts_with(|c: char| c.is_ascii_digit()) {
        name
    } else {
        format!("'{}'", name.replace('\'', "''"))
    }
}

/// The sheet's sparklines as one `<ext>`, a group for each distinct look.
pub fn extension_xml(wb: &Workbook, s: u16, o: &mut Vec<u8>) {
    let list = wb.sheet(s).sparklines();
    if list.is_empty() {
        return;
    }
    let mut groups: Vec<(SparkKind, u32, u32, bool, Vec<&Sparkline>)> = Vec::new();
    for sp in list {
        match groups.iter_mut().find(|g| (g.0, g.1, g.2, g.3) == (sp.kind, sp.color, sp.negative, sp.markers)) {
            Some(g) => g.4.push(sp),
            None => groups.push((sp.kind, sp.color, sp.negative, sp.markers, vec![sp])),
        }
    }
    let mut x = String::new();
    let _ = write!(x, concat!(
        r#"<ext uri="{}" xmlns:x14="http://schemas.microsoft.com/office/spreadsheetml/2009/9/main">"#,
        r#"<x14:sparklineGroups xmlns:xm="http://schemas.microsoft.com/office/excel/2006/main">"#
    ), EXT_URI);
    for (kind, color, negative, markers, sps) in groups {
        x.push_str("<x14:sparklineGroup displayEmptyCellsAs=\"gap\"");
        match kind {
            SparkKind::Line => {}
            SparkKind::Column => x.push_str(r#" type="column""#),
            SparkKind::WinLoss => x.push_str(r#" type="stacked""#),
        }
        if markers {
            x.push_str(if kind == SparkKind::Line { r#" markers="1" high="1" low="1""# } else { r#" high="1" low="1""# });
        }
        let _ = write!(x, r#"><x14:colorSeries rgb="FF{color:06X}"/><x14:colorNegative rgb="FF{negative:06X}"/>"#);
        let _ = write!(x, r#"<x14:colorMarkers rgb="FF{color:06X}"/><x14:colorHigh rgb="FF{color:06X}"/><x14:colorLow rgb="FF{negative:06X}"/>"#);
        x.push_str("<x14:sparklines>");
        for sp in sps {
            let d = &sp.data;
            let _ = write!(x, "<x14:sparkline><xm:f>{}!{}:{}</xm:f><xm:sqref>{}</xm:sqref></x14:sparkline>",
                esc(&sheet_ref(wb, d.sheet)), a1(d.r0, d.c0), a1(d.r1, d.c1), a1(sp.row, sp.col));
        }
        x.push_str("</x14:sparklines></x14:sparklineGroup>");
    }
    x.push_str("</x14:sparklineGroups></ext>");
    o.extend_from_slice(x.as_bytes());
}

/// A sparkline as read, its source range not yet resolved to a sheet.
#[derive(Debug, Clone)]
pub struct Raw {
    pub kind: SparkKind,
    pub color: u32,
    pub negative: u32,
    pub markers: bool,
    pub f: String,
    pub sqref: String,
}

fn rgb(e: &quick_xml::events::BytesStart) -> Option<u32> {
    let v = super::read::attr(e, b"rgb").ok()??;
    let v = v.trim();
    let hex = if v.len() == 8 { &v[2..] } else { v };
    u32::from_str_radix(hex, 16).ok().filter(|&c| c <= 0xFF_FFFF)
}

/// The sparklines in an `<ext>`'s inner XML, at most [`MAX_SPARKLINES`].
pub fn read_extension(xml: &[u8], out: &mut Vec<Raw>) -> Result<(), quick_xml::Error> {
    let mut r = quick_xml::Reader::from_reader(xml);
    let mut look = (SparkKind::Line, DEFAULT_COLOR, DEFAULT_NEGATIVE, false);
    let (mut f, mut sqref): (Option<String>, Option<String>) = (None, None);
    loop {
        match r.read_event()? {
            Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                b"sparklineGroup" => {
                    let a = |n: &[u8]| super::read::attr(&e, n).ok().flatten();
                    let on = |n: &[u8]| a(n).is_some_and(|v| v == "1" || v == "true");
                    look = (
                        match a(b"type").as_deref() {
                            Some("column") => SparkKind::Column,
                            Some("stacked") => SparkKind::WinLoss,
                            _ => SparkKind::Line,
                        },
                        DEFAULT_COLOR,
                        DEFAULT_NEGATIVE,
                        on(b"markers") || on(b"high") || on(b"low"),
                    );
                }
                b"colorSeries" => look.1 = rgb(&e).unwrap_or(DEFAULT_COLOR),
                b"colorNegative" => look.2 = rgb(&e).unwrap_or(DEFAULT_NEGATIVE),
                b"f" => f = Some(r.read_text(e.name())?.into_owned()),
                b"sqref" => sqref = Some(r.read_text(e.name())?.into_owned()),
                _ => {}
            },
            Event::End(e) if e.local_name().as_ref() == b"sparkline" => {
                if let (Some(f), Some(sqref)) = (f.take(), sqref.take()) {
                    if out.len() >= MAX_SPARKLINES {
                        break;
                    }
                    if f.len() <= 512 && sqref.len() <= 32 {
                        out.push(Raw { kind: look.0, color: look.1, negative: look.2, markers: look.3, f, sqref });
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(())
}

/// Sheet `s`'s sparklines from what was read; `false` when any couldn't be
/// resolved (an unknown sheet, a bad range) and was left out.
pub fn resolve(wb: &Workbook, s: u16, raws: &[Raw]) -> (Vec<Sparkline>, bool) {
    let mut ok = true;
    let mut out = Vec::new();
    for raw in raws {
        let at = super::read::parse_ref(raw.sqref.trim());
        let (sheet, cells) = match raw.f.rsplit_once('!') {
            Some((name, cells)) => {
                let name = name.trim();
                let name = name.strip_prefix('\'').and_then(|n| n.strip_suffix('\'')).map_or(name.to_string(), |n| n.replace("''", "'"));
                (wb.sheet_id(&name), cells)
            }
            None => (Some(s), raw.f.as_str()),
        };
        let data = super::read::parse_ref(&cells.replace('$', ""));
        match (at, sheet, data) {
            (Some((row, col, r1, c1)), Some(sheet), Some((r0, c0, dr1, dc1))) if (row, col) == (r1, c1) => {
                let sp = Sparkline { row, col, data: officina_grid::addr::Rect { sheet, r0, c0, r1: dr1, c1: dc1 }, kind: raw.kind, color: raw.color, negative: raw.negative, markers: raw.markers };
                if sp.valid() && !out.iter().any(|o: &Sparkline| (o.row, o.col) == (row, col)) {
                    out.push(sp);
                } else {
                    ok = false;
                }
            }
            _ => ok = false,
        }
    }
    (out, ok)
}
