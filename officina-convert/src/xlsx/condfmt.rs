//! Conditional formats in `.xlsx` (ECMA-376 Part 1 §18.3.1.18, `<conditionalFormatting>`
//! with `<cfRule>`s; their formats are `<dxfs>` in the styles part).

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use officina_grid::addr::{a1, Rect};
use officina_grid::condfmt::{Cfvo, CmpOp, CondFormat, Dxf, Rule, RuleKind, TextOp};
use officina_grid::formula;
use officina_grid::Workbook;
#[cfg(feature = "read")]
use quick_xml::events::Event;

use crate::report::{Kind, Report};

/// Every rule format of the workbook, once each, in order: `dxfId` is the index.
pub fn dxfs(wb: &Workbook) -> Vec<Dxf> {
    let mut out: Vec<Dxf> = Vec::new();
    for f in wb.cond_formats() {
        for r in &f.rules {
            if !matches!(r.kind, RuleKind::ColorScale(_) | RuleKind::DataBar { .. } | RuleKind::IconSet { .. }) && !out.contains(&r.dxf) {
                out.push(r.dxf.clone());
            }
        }
    }
    out
}

fn tesc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// `<dxfs>` for the styles part (`num_fmt_ids` gives each code its id).
pub fn dxfs_xml(list: &[Dxf], num_fmt_id: &mut dyn FnMut(&str) -> u32) -> String {
    let mut o = format!(r#"<dxfs count="{}">"#, list.len());
    for d in list {
        o.push_str("<dxf>");
        if d.color.is_some() || d.bold.is_some() || d.italic.is_some() || d.underline.is_some() || d.strike.is_some() {
            o.push_str("<font>");
            for (b, tag) in [(d.bold, "b"), (d.italic, "i"), (d.strike, "strike")] {
                if let Some(b) = b {
                    let _ = write!(o, r#"<{tag} val="{}"/>"#, b as u8);
                }
            }
            if let Some(u) = d.underline {
                let _ = write!(o, r#"<u val="{}"/>"#, if u { "single" } else { "none" });
            }
            if let Some(c) = d.color {
                let _ = write!(o, r#"<color rgb="FF{c:06X}"/>"#);
            }
            o.push_str("</font>");
        }
        if let Some(code) = &d.num_fmt {
            let _ = write!(o, r#"<numFmt numFmtId="{}" formatCode="{}"/>"#, num_fmt_id(code), esc(code));
        }
        if let Some(c) = d.fill {
            let _ = write!(o, r#"<fill><patternFill patternType="solid"><fgColor rgb="FF{c:06X}"/><bgColor rgb="FF{c:06X}"/></patternFill></fill>"#);
        }
        o.push_str("</dxf>");
    }
    o.push_str("</dxfs>");
    o
}

fn sqref(ranges: &[Rect]) -> String {
    ranges
        .iter()
        .map(|r| if r.r0 == r.r1 && r.c0 == r.c1 { a1(r.r0, r.c0) } else { format!("{}:{}", a1(r.r0, r.c0), a1(r.r1, r.c1)) })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The `<conditionalFormatting>` elements of sheet `s`.
pub fn sheet_xml(wb: &Workbook, s: u16, dxfs: &[Dxf], o: &mut Vec<u8>) {
    let mut priority = 1;
    for f in wb.cond_formats().iter().filter(|f| f.sheet == s) {
        let origin = f.origin();
        let ftext = |e: &officina_grid::formula::Expr| {
            let t = super::write::formula_out(&formula::to_a1_file(e, origin.0, origin.1, &|id| wb.sheet_name(id), &|_, _| None));
            tesc(&t)
        };
        let first = a1(origin.0, origin.1);
        let mut x = format!(r#"<conditionalFormatting sqref="{}">"#, sqref(&f.ranges));
        for rule in &f.rules {
            let dxf = dxfs.iter().position(|d| *d == rule.dxf).unwrap_or(0);
            let stop = if rule.stop { r#" stopIfTrue="1""# } else { "" };
            let head = |x: &mut String, ty: &str, extra: &str, with_dxf: bool| {
                let d = if with_dxf { format!(r#" dxfId="{dxf}""#) } else { String::new() };
                let _ = write!(x, r#"<cfRule type="{ty}"{d} priority="{priority}"{stop}{extra}>"#);
            };
            let cfvo = |x: &mut String, p: &Cfvo| {
                let (t, v) = match p {
                    Cfvo::Min => ("min", None),
                    Cfvo::Max => ("max", None),
                    Cfvo::Num(n) => ("num", Some(n.to_string())),
                    Cfvo::Percent(n) => ("percent", Some(n.to_string())),
                    Cfvo::Percentile(n) => ("percentile", Some(n.to_string())),
                    Cfvo::Formula(e) => ("formula", Some(ftext(e).replace('"', "&quot;"))),
                };
                match v {
                    Some(v) => {
                        let _ = write!(x, r#"<cfvo type="{t}" val="{v}"/>"#);
                    }
                    None => {
                        let _ = write!(x, r#"<cfvo type="{t}"/>"#);
                    }
                }
            };
            match &rule.kind {
                RuleKind::CellIs { op, a, b } => {
                    head(&mut x, "cellIs", &format!(r#" operator="{}""#, op.name()), true);
                    let _ = write!(x, "<formula>{}</formula>", ftext(a));
                    if let Some(b) = b {
                        let _ = write!(x, "<formula>{}</formula>", ftext(b));
                    }
                }
                RuleKind::Expression(e) => {
                    head(&mut x, "expression", "", true);
                    let _ = write!(x, "<formula>{}</formula>", ftext(e));
                }
                RuleKind::Text { op, text } => {
                    let (opn, f) = match op {
                        TextOp::Contains => ("containsText", format!(r#"NOT(ISERROR(SEARCH("{t}",{first})))"#, t = text.replace('"', "\"\""))),
                        TextOp::NotContains => ("notContains", format!(r#"ISERROR(SEARCH("{t}",{first}))"#, t = text.replace('"', "\"\""))),
                        TextOp::BeginsWith => ("beginsWith", format!(r#"LEFT({first},LEN("{t}"))="{t}""#, t = text.replace('"', "\"\""))),
                        TextOp::EndsWith => ("endsWith", format!(r#"RIGHT({first},LEN("{t}"))="{t}""#, t = text.replace('"', "\"\""))),
                    };
                    head(&mut x, op.name(), &format!(r#" operator="{opn}" text="{}""#, esc(text)), true);
                    let _ = write!(x, "<formula>{}</formula>", tesc(&f));
                }
                RuleKind::Blanks(b) => {
                    head(&mut x, if *b { "containsBlanks" } else { "notContainsBlanks" }, "", true);
                    let _ = write!(x, "<formula>{}LEN(TRIM({first}))=0{}</formula>", if *b { "" } else { "NOT(" }, if *b { "" } else { ")" });
                }
                RuleKind::Errors(b) => {
                    head(&mut x, if *b { "containsErrors" } else { "notContainsErrors" }, "", true);
                    let _ = write!(x, "<formula>{}ISERROR({first}){}</formula>", if *b { "" } else { "NOT(" }, if *b { "" } else { ")" });
                }
                RuleKind::Top { n, percent, bottom } => {
                    let extra = format!(r#"{}{} rank="{n}""#, if *percent { r#" percent="1""# } else { "" }, if *bottom { r#" bottom="1""# } else { "" });
                    head(&mut x, "top10", &extra, true);
                }
                RuleKind::Average { below, equal, std_dev } => {
                    let mut extra = String::new();
                    if *below {
                        extra.push_str(r#" aboveAverage="0""#);
                    }
                    if *equal {
                        extra.push_str(r#" equalAverage="1""#);
                    }
                    if *std_dev > 0 {
                        let _ = write!(extra, r#" stdDev="{std_dev}""#);
                    }
                    head(&mut x, "aboveAverage", &extra, true);
                }
                RuleKind::Duplicates { unique } => head(&mut x, if *unique { "uniqueValues" } else { "duplicateValues" }, "", true),
                RuleKind::ColorScale(ps) => {
                    head(&mut x, "colorScale", "", false);
                    x.push_str("<colorScale>");
                    for (p, _) in ps {
                        cfvo(&mut x, p);
                    }
                    for (_, c) in ps {
                        let _ = write!(x, r#"<color rgb="FF{c:06X}"/>"#);
                    }
                    x.push_str("</colorScale>");
                }
                RuleKind::DataBar { min, max, color } => {
                    head(&mut x, "dataBar", "", false);
                    x.push_str("<dataBar>");
                    cfvo(&mut x, min);
                    cfvo(&mut x, max);
                    let _ = write!(x, r#"<color rgb="FF{color:06X}"/></dataBar>"#);
                }
                RuleKind::IconSet { set, points, reverse, show_value } => {
                    head(&mut x, "iconSet", "", false);
                    let name = officina_grid::condfmt::ICON_SETS[*set as usize].0;
                    let _ = write!(x, r#"<iconSet iconSet="{name}""#);
                    if *reverse {
                        x.push_str(r#" reverse="1""#);
                    }
                    if !*show_value {
                        x.push_str(r#" showValue="0""#);
                    }
                    x.push('>');
                    for (p, gte) in points {
                        cfvo(&mut x, p);
                        if !*gte {
                            x.truncate(x.len() - 2);
                            x.push_str(r#" gte="0"/>"#);
                        }
                    }
                    x.push_str("</iconSet>");
                }
            }
            x.push_str("</cfRule>");
            priority += 1;
        }
        x.push_str("</conditionalFormatting>");
        o.extend_from_slice(x.as_bytes());
    }
}

#[cfg(feature = "read")]
fn attrs(e: &quick_xml::events::BytesStart) -> HashMap<Vec<u8>, String> {
    e.attributes().flatten().map(|a| (a.key.local_name().as_ref().to_vec(), a.unescape_value().map(|v| v.into_owned()).unwrap_or_default())).collect()
}

#[cfg(feature = "read")]
fn rgb(a: &HashMap<Vec<u8>, String>, theme: &[u32]) -> Option<u32> {
    let t = a.get(b"tint".as_slice()).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    let base = if let Some(v) = a.get(b"rgb".as_slice()) {
        u32::from_str_radix(v.trim_start_matches('#'), 16).ok()? & 0xFF_FFFF
    } else if let Some(i) = a.get(b"theme".as_slice()).and_then(|v| v.parse::<usize>().ok()) {
        *theme.get(i)?
    } else if let Some(i) = a.get(b"indexed".as_slice()).and_then(|v| v.parse::<usize>().ok()) {
        *officina_grid::numfmt::PALETTE.get(i)?
    } else {
        return None;
    };
    Some(super::styles::tint(base, t))
}

#[cfg(feature = "read")]
/// The `<dxfs>` of a styles part.
pub fn read_dxfs(xml: &[u8], theme: &[u32]) -> Vec<Dxf> {
    let mut out = Vec::new();
    let mut r = quick_xml::Reader::from_reader(xml);
    let mut buf = Vec::new();
    let (mut in_dxfs, mut cur, mut where_) = (false, None::<Dxf>, 0u8);
    let mut fmts: HashMap<u32, String> = HashMap::new();
    loop {
        buf.clear();
        let (e, empty) = match r.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => (e, false),
            Ok(Event::Empty(e)) => (e, true),
            Ok(Event::End(e)) => {
                match e.local_name().as_ref() {
                    b"dxfs" => in_dxfs = false,
                    b"dxf" if in_dxfs => out.extend(cur.take()),
                    b"font" | b"fill" => where_ = 0,
                    _ => {}
                }
                continue;
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => continue,
        };
        let a = attrs(&e);
        let val = a.get(b"val".as_slice());
        let on = || val.is_none_or(|v| v != "0" && v != "false");
        match e.local_name().as_ref() {
            b"dxfs" if !empty => in_dxfs = true,
            b"dxf" if in_dxfs => {
                if empty {
                    out.push(Dxf::default());
                } else {
                    cur = Some(Dxf::default());
                }
            }
            b"numFmt" if !in_dxfs => {
                if let (Some(id), Some(code)) = (a.get(b"numFmtId".as_slice()).and_then(|v| v.parse().ok()), a.get(b"formatCode".as_slice())) {
                    fmts.insert(id, code.clone());
                }
            }
            n => {
                let Some(d) = cur.as_mut() else { continue };
                match n {
                    b"font" => where_ = 1,
                    b"fill" => where_ = 2,
                    b"b" if where_ == 1 => d.bold = Some(on()),
                    b"i" if where_ == 1 => d.italic = Some(on()),
                    b"strike" if where_ == 1 => d.strike = Some(on()),
                    b"u" if where_ == 1 => d.underline = Some(val.is_none_or(|v| v != "none")),
                    b"color" if where_ == 1 => d.color = rgb(&a, theme),
                    // In a rule's format a solid fill's colour is its background.
                    b"bgColor" if where_ == 2 => d.fill = rgb(&a, theme).or(d.fill),
                    b"fgColor" if where_ == 2 && d.fill.is_none() => d.fill = rgb(&a, theme),
                    b"numFmt" => {
                        d.num_fmt = a.get(b"formatCode".as_slice()).map(|c| Arc::from(c.as_str())).or_else(|| {
                            let id: u32 = a.get(b"numFmtId".as_slice())?.parse().ok()?;
                            fmts.get(&id).map(|c| Arc::from(c.as_str()))
                        })
                    }
                    _ => {}
                }
            }
        }
    }
    out
}

/// One `<conditionalFormatting>` read from a sheet: its `sqref` and inner XML.
pub struct Raw {
    pub sqref: String,
    pub inner: Vec<u8>,
}

#[cfg(feature = "read")]
/// Turns a sheet's `<conditionalFormatting>`s into the workbook's formats.
pub fn read_sheet(wb: &mut Workbook, s: u16, raws: &[Raw], dxfs: &[Dxf], theme: &[u32], rep: &mut Report) {
    use officina_grid::condfmt::{MAX_RANGES, MAX_RULES};

    let mut sheet_rules = Vec::new();
    let mut sheet_ranges = Vec::new();
    for raw in raws {
        let ranges: Vec<Rect> = raw
            .sqref
            .split_whitespace()
            .filter_map(super::read::parse_ref)
            .map(|(r0, c0, r1, c1)| Rect { sheet: s, r0, c0, r1, c1 })
            .take(MAX_RANGES)
            .collect();
        if ranges.is_empty() {
            continue;
        }
        let origin = (ranges[0].r0, ranges[0].c0);
        let mut rules: Vec<(u32, Rule)> = Vec::new();
        let mut r = quick_xml::Reader::from_reader(&raw.inner[..]);
        let mut buf = Vec::new();
        // The rule being read: its attributes, formulas, points and colours.
        let mut rule: Option<HashMap<Vec<u8>, String>> = None;
        let (mut formulas, mut cfvos, mut colors): (Vec<String>, Vec<(String, Option<String>)>, Vec<u32>) = (Vec::new(), Vec::new(), Vec::new());
        // Each point's gte (icon sets), as read.
        let mut gtes: Vec<bool> = Vec::new();
        let mut in_formula = false;
        loop {
            buf.clear();
            match r.read_event_into(&mut buf) {
                Ok(Event::Start(e)) if e.local_name().as_ref() == b"cfRule" => {
                    rule = Some(attrs(&e));
                    formulas.clear();
                    cfvos.clear();
                    colors.clear();
                    gtes.clear();
                }
                Ok(Event::Empty(e)) if e.local_name().as_ref() == b"cfRule" => {
                    if let Some(x) = make(wb, s, origin, attrs(&e), &[], &[], &[], &[], dxfs, rep) {
                        rules.push(x);
                    }
                }
                Ok(Event::Start(e)) if e.local_name().as_ref() == b"formula" => {
                    in_formula = true;
                    formulas.push(String::new());
                }
                Ok(Event::Text(t)) if in_formula => {
                    if let (Some(f), Ok(t)) = (formulas.last_mut(), t.unescape()) {
                        f.push_str(&t);
                    }
                }
                Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                    let a = attrs(&e);
                    match e.local_name().as_ref() {
                        b"cfvo" => {
                            cfvos.push((a.get(b"type".as_slice()).cloned().unwrap_or_default(), a.get(b"val".as_slice()).cloned()));
                            gtes.push(!a.get(b"gte".as_slice()).is_some_and(|v| v == "0" || v == "false"));
                        }
                        b"color" => colors.push(rgb(&a, theme).unwrap_or(0)),
                        // The set's own attributes join the rule's (cfRule has none of these names).
                        b"iconSet" => {
                            if let Some(r) = rule.as_mut() {
                                for k in ["iconSet", "reverse", "showValue"] {
                                    if let Some(v) = a.get(k.as_bytes()) {
                                        r.insert(format!("icon.{k}").into_bytes(), v.clone());
                                    }
                                }
                                r.entry(b"icon.iconSet".to_vec()).or_insert_with(|| "3TrafficLights1".into());
                            }
                        }
                        _ => {}
                    }
                }
                Ok(Event::End(e)) => match e.local_name().as_ref() {
                    b"formula" => in_formula = false,
                    b"cfRule" => {
                        if let Some(a) = rule.take() {
                            if let Some(x) = make(wb, s, origin, a, &formulas, &cfvos, &gtes, &colors, dxfs, rep) {
                                rules.push(x);
                            }
                        }
                    }
                    _ => {}
                },
                Ok(Event::Eof) | Err(_) => break,
                _ => {}
            }
        }
        if rules.is_empty() {
            continue;
        }
        rules.sort_by_key(|(priority, _)| *priority);
        let group = sheet_ranges.len();
        sheet_ranges.push(ranges);
        sheet_rules.extend(rules.into_iter().take(MAX_RULES).map(|(priority, rule)| (priority, group, rule)));
    }

    // The priority applies to the whole sheet. Keep consecutive rules with the
    // same source together, splitting blocks where another block interleaves.
    sheet_rules.sort_by_key(|(priority, _, _)| *priority);
    let mut ordered = sheet_rules.into_iter().peekable();
    while let Some((_, group, first)) = ordered.next() {
        let mut rules = vec![first];
        while let Some((_, _, rule)) = ordered.next_if(|(_, next, _)| *next == group) {
            rules.push(rule);
        }
        let ranges = sheet_ranges[group].clone();
        if wb.add_cond_format(CondFormat { sheet: s, ranges, rules }).is_err() {
            rep.note(Kind::Dropped, "conditional formats past the limits");
        }
    }
}

#[cfg(feature = "read")]
#[allow(clippy::too_many_arguments)]
fn make(wb: &Workbook, s: u16, origin: (u32, u32), a: HashMap<Vec<u8>, String>, formulas: &[String], cfvos: &[(String, Option<String>)], gtes: &[bool], colors: &[u32], dxfs: &[Dxf], rep: &mut Report) -> Option<(u32, Rule)> {
    let get = |k: &str| a.get(k.as_bytes()).map(String::as_str);
    let flag = |k: &str| get(k).is_some_and(|v| v == "1" || v == "true");
    let priority = get("priority").and_then(|v| v.parse().ok()).unwrap_or(u32::MAX);
    let dxf = get("dxfId").and_then(|v| v.parse::<usize>().ok()).and_then(|i| dxfs.get(i)).cloned().unwrap_or_default();
    let fx = |t: &str| wb.cond_formula(&super::read::formula_in(t), s, origin).ok();
    let point = |(t, v): &(String, Option<String>)| -> Option<Cfvo> {
        let num = || v.as_deref().and_then(|v| v.parse::<f64>().ok()).filter(|x| x.is_finite());
        Some(match t.as_str() {
            "min" => Cfvo::Min,
            "max" => Cfvo::Max,
            "num" => match num() {
                Some(x) => Cfvo::Num(x),
                None => Cfvo::Formula(fx(v.as_deref()?)?),
            },
            "percent" => Cfvo::Percent(num()?),
            "percentile" => Cfvo::Percentile(num()?),
            "formula" => Cfvo::Formula(fx(v.as_deref()?)?),
            _ => return None,
        })
    };
    let kind = match get("type")? {
        "cellIs" => {
            let op = CmpOp::from_name(get("operator")?)?;
            let x = fx(formulas.first()?)?;
            let b = formulas.get(1).and_then(|t| fx(t)).map(Some).unwrap_or(None);
            RuleKind::CellIs { op, a: x, b }
        }
        "expression" => RuleKind::Expression(fx(formulas.first()?)?),
        t @ ("containsText" | "notContainsText" | "beginsWith" | "endsWith") => RuleKind::Text { op: TextOp::from_name(t)?, text: Arc::from(get("text").unwrap_or("")) },
        "containsBlanks" => RuleKind::Blanks(true),
        "notContainsBlanks" => RuleKind::Blanks(false),
        "containsErrors" => RuleKind::Errors(true),
        "notContainsErrors" => RuleKind::Errors(false),
        "top10" => RuleKind::Top { n: get("rank").and_then(|v| v.parse().ok()).unwrap_or(10), percent: flag("percent"), bottom: flag("bottom") },
        "aboveAverage" => RuleKind::Average { below: get("aboveAverage").is_some_and(|v| v == "0" || v == "false"), equal: flag("equalAverage"), std_dev: get("stdDev").and_then(|v| v.parse::<u8>().ok()).unwrap_or(0).min(3) },
        "duplicateValues" => RuleKind::Duplicates { unique: false },
        "uniqueValues" => RuleKind::Duplicates { unique: true },
        "colorScale" => {
            let ps: Option<Vec<Cfvo>> = cfvos.iter().map(point).collect();
            let ps = ps?;
            if !(2..=3).contains(&ps.len()) || colors.len() < ps.len() {
                return None;
            }
            RuleKind::ColorScale(ps.into_iter().zip(colors.iter().copied()).collect())
        }
        "dataBar" => {
            let min = point(cfvos.first()?)?;
            let max = point(cfvos.get(1)?)?;
            RuleKind::DataBar { min, max, color: *colors.first()? }
        }
        "iconSet" => {
            let name = get("icon.iconSet").unwrap_or("3TrafficLights1");
            let Some(set) = officina_grid::condfmt::icon_set(name) else {
                rep.note_at(Kind::Dropped, "icon sets Officina doesn't have", || name.to_string());
                return None;
            };
            let n = officina_grid::condfmt::ICON_SETS[set as usize].1 as usize;
            let points: Option<Vec<(Cfvo, bool)>> = cfvos.iter().zip(gtes.iter().chain(std::iter::repeat(&true))).map(|(c, g)| point(c).map(|p| (p, *g))).collect();
            let points = match points {
                Some(p) if p.len() == n => p,
                _ => officina_grid::condfmt::icon_points(n as u8),
            };
            let off = |k: &str| get(k).is_some_and(|v| v == "0" || v == "false");
            RuleKind::IconSet { set, points, reverse: flag("icon.reverse"), show_value: !off("icon.showValue") }
        }
        other => {
            rep.note_at(Kind::Dropped, "conditional format rules of kinds Officina doesn't have", || other.to_string());
            return None;
        }
    };
    Some((priority, Rule { kind, dxf, stop: flag("stopIfTrue") }))
}
