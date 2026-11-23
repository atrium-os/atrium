//! `<dataValidations>`: data validation rules (docs/spec/officina.md §16.83).

use std::fmt::Write as _;

use officina_grid::formula;
use officina_grid::validation::{Allow, ErrorStyle, Message, Validation};
use officina_grid::Workbook;

use crate::xml::esc;

pub const EXT_URI: &str = "{CCE6A557-97BC-4B89-ADB6-D9C93CAAB3DF}";

fn cross_sheet(v: &Validation) -> bool {
    use formula::Expr;
    let mut cross = false;
    for expr in [&v.f1, &v.f2].into_iter().flatten() {
        expr.walk(&mut |node| {
            let sheet = match node {
                Expr::Ref(r) | Expr::Spill(r) => r.sheet,
                Expr::Range(r) => r.sheet,
                _ => None,
            };
            cross |= sheet.is_some_and(|s| s != v.sheet);
        });
    }
    cross
}

/// Sheet `s`'s rules as `<dataValidations>` into `o` (nothing when it has none).
pub fn sheet_xml(wb: &Workbook, s: u16, o: &mut Vec<u8>) {
    write_rules(wb, s, o, false);
}

/// Rules with direct references to another sheet use the Excel extension.
pub fn extension_xml(wb: &Workbook, s: u16, o: &mut Vec<u8>) {
    write_rules(wb, s, o, true);
}

fn write_rules(wb: &Workbook, s: u16, o: &mut Vec<u8>, extended: bool) {
    let vs: Vec<_> = wb.validations().iter().filter(|v| v.sheet == s && cross_sheet(v) == extended).collect();
    if vs.is_empty() {
        return;
    }
    let mut x = String::new();
    let prefix = if extended { "x14:" } else { "" };
    if extended {
        let _ = write!(x, concat!(
            r#"<ext uri="{}" xmlns:x14="http://schemas.microsoft.com/office/spreadsheetml/2009/9/main""#,
            r#" xmlns:xm="http://schemas.microsoft.com/office/excel/2006/main">"#
        ), EXT_URI);
    }
    let _ = write!(x, r#"<{prefix}dataValidations count="{}">"#, vs.len());
    for v in vs {
        let origin = v.origin();
        let f = |e: &Option<std::sync::Arc<formula::Expr>>| {
            e.as_ref().map(|e| super::write::formula_out(&formula::to_a1_file(e, origin.0, origin.1, &|id| wb.sheet_name(id), &|_, _| None)))
        };
        let _ = write!(x, r#"<{prefix}dataValidation type="{}""#, v.allow.name());
        if v.style != ErrorStyle::Stop {
            let _ = write!(x, r#" errorStyle="{}""#, v.style.name());
        }
        if !matches!(v.allow, Allow::List | Allow::Custom | Allow::Any) && v.op != officina_grid::condfmt::CmpOp::Between {
            let _ = write!(x, r#" operator="{}""#, v.op.name());
        }
        if v.allow_blank {
            x.push_str(r#" allowBlank="1""#);
        }
        // Excel's showDropDown means the opposite: the drop-down is hidden.
        if v.allow == Allow::List && !v.dropdown {
            x.push_str(r#" showDropDown="1""#);
        }
        if v.show_prompt {
            x.push_str(r#" showInputMessage="1""#);
        }
        if v.show_error {
            x.push_str(r#" showErrorMessage="1""#);
        }
        for (k, m) in [("error", &v.error), ("prompt", &v.prompt)] {
            if !m.title.is_empty() {
                let _ = write!(x, r#" {k}Title="{}""#, esc(&m.title));
            }
            if !m.text.is_empty() {
                let _ = write!(x, r#" {k}="{}""#, esc(&m.text));
            }
        }
        let sqref: Vec<String> = v.ranges.iter().map(|r| if r.r0 == r.r1 && r.c0 == r.c1 { officina_grid::addr::a1(r.r0, r.c0) } else { format!("{}:{}", officina_grid::addr::a1(r.r0, r.c0), officina_grid::addr::a1(r.r1, r.c1)) }).collect();
        let sqref = sqref.join(" ");
        if extended {
            x.push('>');
        } else {
            let _ = write!(x, r#" sqref="{sqref}">"#);
        }
        for (tag, expr) in [("formula1", &v.f1), ("formula2", &v.f2)] {
            if let Some(t) = f(expr) {
                let t = esc(t.strip_prefix('=').unwrap_or(&t));
                if extended {
                    let _ = write!(x, "<x14:{tag}><xm:f>{t}</xm:f></x14:{tag}>");
                } else {
                    let _ = write!(x, "<{tag}>{t}</{tag}>");
                }
            }
        }
        if extended {
            let _ = write!(x, "<xm:sqref>{sqref}</xm:sqref>");
        }
        let _ = write!(x, "</{prefix}dataValidation>");
    }
    let _ = write!(x, "</{prefix}dataValidations>");
    if extended {
        x.push_str("</ext>");
    }
    o.extend_from_slice(x.as_bytes());
}

#[cfg(feature = "read")]
#[derive(Default)]
pub struct Raw {
    pub base: Vec<u8>,
    pub extension: Option<Vec<u8>>,
}

#[cfg(feature = "read")]
pub fn read_sheet(wb: &mut Workbook, s: u16, raw: &Raw, rep: &mut crate::report::Report) {
    use quick_xml::events::Event;
    read_rules(wb, s, &raw.base, false, rep);
    if let Some(extension) = &raw.extension {
        let mut reader = quick_xml::Reader::from_reader(extension.as_slice());
        reader.config_mut().trim_text(true);
        // The validation extension has one dataValidations child.
        let section = match reader.read_event() {
            Ok(Event::Start(tag)) if tag.local_name().as_ref() == b"dataValidations" => {
                reader.read_to_end(tag.name()).ok().and_then(|span| extension.get(span.start as usize..span.end as usize))
            }
            Ok(Event::Empty(tag)) if tag.local_name().as_ref() == b"dataValidations" => Some(&[][..]),
            _ => None,
        };
        if let Some(inner) = section {
            read_rules(wb, s, inner, true, rep);
        } else {
            rep.note(crate::report::Kind::Dropped, "malformed Excel validation extension");
        }
    }
}

#[cfg(feature = "read")]
fn read_rules(wb: &mut Workbook, s: u16, inner: &[u8], extended: bool, rep: &mut crate::report::Report) {
    use crate::report::Kind;
    use officina_grid::addr::Rect;
    use officina_grid::condfmt::CmpOp;
    use quick_xml::events::Event;
    let mut r = quick_xml::Reader::from_reader(inner);
    let mut buf = Vec::new();
    // The rule being read: its attributes and formulas; which formula is open.
    type Attributes = std::collections::HashMap<Vec<u8>, String>;
    type Fields = [String; 3];
    const MAX_SQREF: usize = officina_grid::validation::MAX_RANGES * 32;
    let mut cur: Option<(Attributes, Fields, bool)> = None;
    let mut open: Option<usize> = None;
    let attrs = |e: &quick_xml::events::BytesStart| -> std::collections::HashMap<Vec<u8>, String> {
        e.attributes().with_checks(false).flatten().filter_map(|a| a.unescape_value().ok().map(|v| (a.key.local_name().as_ref().to_vec(), v.into_owned()))).collect()
    };
    let mut done = |wb: &mut Workbook, a: Attributes, fs: Fields, oversized: bool| {
        if oversized {
            rep.note(Kind::Dropped, "data validation rules with oversized formulas or ranges");
            return;
        }
        let get = |k: &[u8]| a.get(k).map(String::as_str);
        let flag = |k: &[u8]| get(k).is_some_and(|v| v == "1" || v == "true");
        let refs = if extended { &fs[2] } else { get(b"sqref").unwrap_or_default() };
        let ranges: Option<Vec<Rect>> = refs.split_whitespace().take(officina_grid::validation::MAX_RANGES + 1)
            .map(|text| super::read::parse_ref(text).map(|(r0, c0, r1, c1)| Rect { sheet: s, r0, c0, r1, c1 }))
            .collect();
        let Some(ranges) = ranges.filter(|ranges| !ranges.is_empty() && ranges.len() <= officina_grid::validation::MAX_RANGES) else {
            rep.note(Kind::Dropped, "data validation rules with invalid or excessive ranges");
            return;
        };
        let Some(allow) = Allow::from_name(get(b"type").unwrap_or("none")) else {
            rep.note(Kind::Dropped, "data validation rules of kinds Officina doesn't have");
            return;
        };
        let origin = (ranges[0].r0, ranges[0].c0);
        let fx = |t: &str| -> Result<Option<std::sync::Arc<formula::Expr>>, ()> {
            if t.trim().is_empty() {
                return Ok(None);
            }
            wb.validation_formula(&super::read::formula_in(t), s, origin).map(Some).map_err(|_| ())
        };
        let op = get(b"operator").and_then(CmpOp::from_name).unwrap_or(CmpOp::Between);
        // LibreOffice can write a second formula even for single-formula rules.
        let ranged = matches!(op, CmpOp::Between | CmpOp::NotBetween) && !matches!(allow, Allow::List | Allow::Custom | Allow::Any);
        let (Ok(f1), Ok(f2)) = (fx(&fs[0]), fx(if ranged { &fs[1] } else { "" })) else {
            rep.note(Kind::Dropped, "data validation rules with formulas this engine can't read");
            return;
        };
        if (allow != Allow::Any && f1.is_none()) || (ranged && f2.is_none()) {
            rep.note(Kind::Dropped, "data validation rules without their required formulas");
            return;
        }
        let mut v = Validation::new(s, ranges, allow, op, f1, f2);
        v.allow_blank = flag(b"allowBlank");
        v.dropdown = !flag(b"showDropDown");
        v.show_prompt = flag(b"showInputMessage");
        v.show_error = flag(b"showErrorMessage");
        v.style = get(b"errorStyle").and_then(ErrorStyle::from_name).unwrap_or_default();
        v.prompt = Message::new(get(b"promptTitle").unwrap_or(""), get(b"prompt").unwrap_or(""));
        v.error = Message::new(get(b"errorTitle").unwrap_or(""), get(b"error").unwrap_or(""));
        if wb.add_validation(v).is_err() {
            rep.note(Kind::Dropped, "data validation rules past the limits");
        }
    };
    loop {
        buf.clear();
        let Ok(ev) = r.read_event_into(&mut buf) else { break };
        match ev {
            Event::Start(e) => match e.local_name().as_ref() {
                b"dataValidation" => {
                    cur = Some((attrs(&e), Fields::default(), false));
                    open = None;
                }
                b"formula1" => open = Some(0),
                b"formula2" => open = Some(1),
                b"sqref" if extended => open = Some(2),
                _ => {}
            },
            Event::Empty(e) if e.local_name().as_ref() == b"dataValidation" => {
                done(wb, attrs(&e), Fields::default(), false);
            }
            Event::Text(t) => {
                if let (Some(i), Some((_, fs, oversized))) = (open, cur.as_mut()) {
                    let limit = if i == 2 { MAX_SQREF } else { formula::MAX_STORED_FORMULA_LEN };
                    match t.unescape() {
                        Ok(text) if fs[i].len() + text.len() <= limit => fs[i].push_str(&text),
                        _ => *oversized = true,
                    }
                }
            }
            Event::End(e) => match e.local_name().as_ref() {
                b"formula1" | b"formula2" | b"sqref" => open = None,
                b"dataValidation" => {
                    if let Some((a, fs, oversized)) = cur.take() {
                        done(wb, a, fs, oversized);
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
}
