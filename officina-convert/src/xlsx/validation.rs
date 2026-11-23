//! `<dataValidations>`: data validation rules (docs/spec/officina.md §16.83).

use std::fmt::Write as _;

use officina_grid::formula;
use officina_grid::validation::{Allow, ErrorStyle, Message, Validation};
use officina_grid::Workbook;

use crate::xml::esc;

const LIST_NS: &str = "http://schemas.microsoft.com/office/spreadsheetml/2011/1/ac";
const MC_NS: &str = "http://schemas.openxmlformats.org/markup-compatibility/2006";

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
        let list = v.literal_list_items();
        if let Some(items) = &list {
            write_literal_list(&mut x, items);
        }
        for (tag, expr) in [("formula1", &v.f1), ("formula2", &v.f2)] {
            if tag == "formula1" && list.is_some() {
                continue;
            }
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

fn write_literal_list(out: &mut String, items: &[&str]) {
    let _ = write!(out, r#"<mc:AlternateContent xmlns:mc="{MC_NS}" xmlns:x12ac="{LIST_NS}"><mc:Choice Requires="x12ac"><x12ac:list>"#);
    let quote = |item: &str, encoded: &mut Vec<u8>| {
        encoded.push(b'"');
        let text = item.replace('"', "\"\"");
        for (i, part) in text.split('\r').enumerate() {
            if i != 0 {
                encoded.extend_from_slice(b"_x000D_");
            }
            super::write::esc(encoded, part);
        }
        encoded.push(b'"');
    };
    let mut encoded = Vec::new();
    for (idx, item) in items.iter().enumerate() {
        if idx != 0 {
            encoded.push(b',');
        }
        quote(item, &mut encoded);
    }
    out.push_str(std::str::from_utf8(&encoded).expect("XML escape writes UTF-8"));
    out.push_str("</x12ac:list></mc:Choice>");
    // Only provide the compatibility formula if older readers can enforce
    // exactly the same choices using ordinary comma splitting.
    if items.iter().all(|item| !item.contains([',', '"', '\r', '\n']) && *item == item.trim()) {
        let text = items.join(",");
        if text.encode_utf16().count() <= 255 {
            let _ = write!(out, "<mc:Fallback><formula1>\"{}\"</formula1></mc:Fallback>", esc(&text));
        }
    }
    out.push_str("</mc:AlternateContent>");
}

#[cfg(feature = "read")]
fn read_literal_list(mut text: &str) -> Result<std::sync::Arc<formula::Expr>, ()> {
    use officina_grid::validation::MAX_ITEMS;
    let mut items = Vec::new();
    loop {
        if items.len() == MAX_ITEMS {
            return Err(());
        }
        text = text.trim_start().strip_prefix('"').ok_or(())?;
        let mut item = String::new();
        loop {
            let (part, tail) = text.split_once('"').ok_or(())?;
            item.push_str(part);
            if let Some(rest) = tail.strip_prefix('"') {
                item.push('"');
                text = rest;
            } else {
                text = tail.trim_start();
                break;
            }
        }
        items.push(formula::Expr::Str(super::read::decode_escapes(&item).as_ref().into()));
        if text.is_empty() {
            return Ok(std::sync::Arc::new(formula::Expr::Array(vec![items])));
        }
        text = text.strip_prefix(',').ok_or(())?;
    }
}

#[cfg(feature = "read")]
#[derive(Default)]
pub struct Raw {
    pub root_namespaces: Vec<u8>,
    pub base: Vec<u8>,
    pub extension: Option<Vec<u8>>,
}

#[cfg(feature = "read")]
pub fn read_sheet(wb: &mut Workbook, s: u16, raw: &Raw, rep: &mut crate::report::Report) {
    for (part, extended) in [(&raw.base[..], false), (raw.extension.as_deref().unwrap_or_default(), true)] {
        if part.is_empty() {
            continue;
        }
        // The sheet parser saves these sections for when workbook names exist.
        // Restore the worksheet namespace bindings for the detached XML.
        let mut section = b"<worksheet".to_vec();
        section.extend_from_slice(&raw.root_namespaces);
        section.push(b'>');
        section.extend_from_slice(part);
        section.extend_from_slice(b"</worksheet>");
        read_rules(wb, s, &section, extended, rep);
    }
}

#[cfg(feature = "read")]
fn read_rules(wb: &mut Workbook, s: u16, inner: &[u8], extended: bool, rep: &mut crate::report::Report) {
    use crate::report::Kind;
    use officina_grid::addr::Rect;
    use officina_grid::condfmt::CmpOp;
    use quick_xml::events::Event;
    use quick_xml::name::{QName, ResolveResult};
    let mut r = quick_xml::NsReader::from_reader(inner);
    let mut buf = Vec::new();
    // The rule being read: its attributes and formulas; which formula is open.
    type Attributes = std::collections::HashMap<Vec<u8>, String>;
    type Fields = [String; 4];
    const MAX_SQREF: usize = officina_grid::validation::MAX_RANGES * 32;
    let mut cur: Option<(Attributes, Fields, bool)> = None;
    let mut open: Option<usize> = None;
    let mut alternate = false;
    let mut list_present = false;
    let mut skip_depth = 0;
    let attrs = |e: &quick_xml::events::BytesStart| -> std::collections::HashMap<Vec<u8>, String> {
        e.attributes().with_checks(false).flatten().filter_map(|a| a.unescape_value().ok().map(|v| (a.key.local_name().as_ref().to_vec(), v.into_owned()))).collect()
    };
    let mut done = |wb: &mut Workbook, a: Attributes, fs: Fields, oversized: bool, list_present: bool| {
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
        let first = if allow == Allow::List && list_present {
            read_literal_list(&fs[3]).map(Some)
        } else if allow == Allow::Custom && list_present {
            fx(&fs[3])
        } else {
            fx(&fs[0])
        };
        let (Ok(f1), Ok(f2)) = (first, fx(if ranged { &fs[1] } else { "" })) else {
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
        let ev = match r.read_event_into(&mut buf) {
            Ok(ev) => ev,
            Err(_) => {
                rep.note(Kind::Dropped, "malformed Excel validation XML");
                break;
            }
        };
        if skip_depth > 0 {
            match ev {
                Event::Start(_) => skip_depth += 1,
                Event::End(_) => skip_depth -= 1,
                Event::Eof => break,
                _ => {}
            }
            continue;
        }
        match ev {
            Event::Start(e) => match e.local_name().as_ref() {
                b"dataValidation" => {
                    cur = Some((attrs(&e), Fields::default(), false));
                    open = None;
                    alternate = false;
                    list_present = false;
                }
                b"AlternateContent" if cur.is_some() && matches!(r.resolve_element(e.name()).0, ResolveResult::Bound(ns) if ns.as_ref() == MC_NS.as_bytes()) => {
                    alternate = true;
                }
                b"Choice" if alternate && matches!(r.resolve_element(e.name()).0, ResolveResult::Bound(ns) if ns.as_ref() == MC_NS.as_bytes()) => {
                    let supports_list = attrs(&e).get(b"Requires".as_slice()).is_some_and(|value| {
                        let mut requires = value.split_whitespace();
                        requires.next().is_some_and(|prefix| {
                            let name = format!("{prefix}:list");
                            matches!(r.resolve_element(QName(name.as_bytes())).0, ResolveResult::Bound(ns) if ns.as_ref() == LIST_NS.as_bytes())
                        }) && requires.next().is_none()
                    });
                    if !supports_list || list_present {
                        skip_depth = 1;
                    }
                }
                b"Fallback" if alternate && list_present && matches!(r.resolve_element(e.name()).0, ResolveResult::Bound(ns) if ns.as_ref() == MC_NS.as_bytes()) => {
                    skip_depth = 1;
                }
                b"list" if cur.is_some() && matches!(r.resolve_element(e.name()).0, ResolveResult::Bound(ns) if ns.as_ref() == LIST_NS.as_bytes()) => {
                    list_present = true;
                    open = Some(3);
                }
                b"formula1" => open = Some(0),
                b"formula2" => open = Some(1),
                b"sqref" if extended => open = Some(2),
                _ => {}
            },
            Event::Empty(e) if e.local_name().as_ref() == b"dataValidation" => {
                done(wb, attrs(&e), Fields::default(), false, false);
            }
            Event::Empty(e) if cur.is_some() && e.local_name().as_ref() == b"list"
                && matches!(r.resolve_element(e.name()).0, ResolveResult::Bound(ns) if ns.as_ref() == LIST_NS.as_bytes()) => {
                list_present = true;
            }
            Event::Text(t) => {
                if let (Some(i), Some((_, fs, oversized))) = (open, cur.as_mut()) {
                    let limit = match i {
                        2 => MAX_SQREF,
                        3 => formula::MAX_STORED_FORMULA_LEN - 3, // native array braces and leading '='
                        _ => formula::MAX_STORED_FORMULA_LEN,
                    };
                    match t.unescape() {
                        Ok(text) if fs[i].len() + text.len() <= limit => fs[i].push_str(&text),
                        _ => *oversized = true,
                    }
                }
            }
            Event::End(e) => match e.local_name().as_ref() {
                b"AlternateContent" => alternate = false,
                b"formula1" | b"formula2" | b"sqref" | b"list" => open = None,
                b"dataValidation" => {
                    if let Some((a, fs, oversized)) = cur.take() {
                        done(wb, a, fs, oversized, list_present);
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
}
