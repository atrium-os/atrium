//! `<dataValidations>`: data validation rules (docs/spec/officina.md §16.83).

use std::fmt::Write as _;

use officina_grid::formula;
use officina_grid::validation::{Allow, ErrorStyle, Message, Validation};
use officina_grid::Workbook;

use crate::xml::esc;

/// Sheet `s`'s rules as `<dataValidations>` into `o` (nothing when it has none).
pub fn sheet_xml(wb: &Workbook, s: u16, o: &mut Vec<u8>) {
    let vs: Vec<&Validation> = wb.validations().iter().filter(|v| v.sheet == s).collect();
    if vs.is_empty() {
        return;
    }
    let mut x = String::new();
    let _ = write!(x, r#"<dataValidations count="{}">"#, vs.len());
    for v in vs {
        let origin = v.origin();
        let f = |e: &Option<std::sync::Arc<formula::Expr>>| {
            e.as_ref().map(|e| super::write::formula_out(&formula::to_a1_file(e, origin.0, origin.1, &|id| wb.sheet_name(id), &|_, _| None)))
        };
        let _ = write!(x, r#"<dataValidation type="{}""#, v.allow.name());
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
        let _ = write!(x, r#" sqref="{}">"#, sqref.join(" "));
        if let Some(t) = f(&v.f1) {
            let _ = write!(x, "<formula1>{}</formula1>", esc(t.strip_prefix('=').unwrap_or(&t)));
        }
        if let Some(t) = f(&v.f2) {
            let _ = write!(x, "<formula2>{}</formula2>", esc(t.strip_prefix('=').unwrap_or(&t)));
        }
        x.push_str("</dataValidation>");
    }
    x.push_str("</dataValidations>");
    o.extend_from_slice(x.as_bytes());
}

/// Sheet `s`'s `<dataValidations>` (its inner XML) onto the workbook.
#[cfg(feature = "read")]
pub fn read_sheet(wb: &mut Workbook, s: u16, inner: &[u8], rep: &mut crate::report::Report) {
    use crate::report::Kind;
    use officina_grid::addr::Rect;
    use officina_grid::condfmt::CmpOp;
    use quick_xml::events::Event;
    let mut r = quick_xml::Reader::from_reader(inner);
    let mut buf = Vec::new();
    // The rule being read: its attributes and formulas; which formula is open.
    let mut cur: Option<(std::collections::HashMap<Vec<u8>, String>, [String; 2])> = None;
    let mut open: Option<usize> = None;
    let attrs = |e: &quick_xml::events::BytesStart| -> std::collections::HashMap<Vec<u8>, String> {
        e.attributes().with_checks(false).flatten().filter_map(|a| a.unescape_value().ok().map(|v| (a.key.local_name().as_ref().to_vec(), v.into_owned()))).collect()
    };
    let mut done = |wb: &mut Workbook, a: std::collections::HashMap<Vec<u8>, String>, fs: [String; 2]| {
        let get = |k: &[u8]| a.get(k).map(String::as_str);
        let flag = |k: &[u8]| get(k).is_some_and(|v| v == "1" || v == "true");
        let ranges: Vec<Rect> = get(b"sqref").unwrap_or_default().split_whitespace().filter_map(super::read::parse_ref).map(|(r0, c0, r1, c1)| Rect { sheet: s, r0, c0, r1, c1 }).take(officina_grid::validation::MAX_RANGES).collect();
        if ranges.is_empty() {
            return;
        }
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
        let (Ok(f1), Ok(f2)) = (fx(&fs[0]), fx(&fs[1])) else {
            rep.note(Kind::Dropped, "data validation rules with formulas this engine can't read");
            return;
        };
        let op = get(b"operator").and_then(CmpOp::from_name).unwrap_or(CmpOp::Between);
        // A second formula only bounds a range (LibreOffice writes "0" for any rule).
        let ranged = matches!(op, CmpOp::Between | CmpOp::NotBetween) && !matches!(allow, Allow::List | Allow::Custom | Allow::Any);
        let f2 = if ranged { f2 } else { None };
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
                b"dataValidation" => cur = Some((attrs(&e), [String::new(), String::new()])),
                b"formula1" => open = Some(0),
                b"formula2" => open = Some(1),
                _ => {}
            },
            Event::Empty(e) if e.local_name().as_ref() == b"dataValidation" => {
                done(wb, attrs(&e), [String::new(), String::new()]);
            }
            Event::Text(t) => {
                if let (Some(i), Some((_, fs))) = (open, cur.as_mut()) {
                    if fs[i].len() < formula::MAX_STORED_FORMULA_LEN {
                        fs[i].push_str(&t.unescape().unwrap_or_default());
                    }
                }
            }
            Event::End(e) => match e.local_name().as_ref() {
                b"formula1" | b"formula2" => open = None,
                b"dataValidation" => {
                    if let Some((a, fs)) = cur.take() {
                        done(wb, a, fs);
                    }
                }
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
}
