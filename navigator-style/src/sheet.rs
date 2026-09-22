//! Stylesheet parsing — strict, positioned, bounded.
//!
//! ★ NOTHING IS DROPPED SILENTLY (profile §5.1). Every refusal — an unknown
//! property, a shorthand, a value outside the row's grammar, an unadmitted
//! selector or at-rule, `!important` — is a `Diagnostic` with the source
//! position of what it refuses. A ceiling (§3.12) refuses the WHOLE sheet:
//! a truncated stylesheet renders wrongly and silently, which is exactly the
//! failure the ceilings exist to prevent.

use crate::selector::{self, Complex};
use crate::token::{tokenize, Pos, Tok, Token};
use crate::values::{self, Specified, SHORTHANDS};

pub const MAX_SHEET_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_RULES: usize = 32_768;

#[derive(Debug, Clone, PartialEq)]
pub struct Diagnostic { pub pos: Pos, pub code: &'static str, pub msg: String }

#[derive(Debug, Clone, PartialEq)]
pub struct Decl { pub prop: &'static str, pub value: Specified, pub pos: Pos }

#[derive(Debug, Clone, PartialEq)]
pub struct Rule {
    pub selectors: Vec<Complex>,
    pub decls: Vec<Decl>,
    /// `--name: tokens` — kept raw, substituted at cascade time.
    pub customs: Vec<(String, Vec<Token>)>,
    pub media: Option<Vec<MediaQuery>>,
    pub pos: Pos,
}

/// One media query: all features must hold. A list of them is an OR.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaQuery(pub Vec<Feature>);

#[derive(Debug, Clone, PartialEq)]
pub enum Feature {
    MinWidth(values::Length), MaxWidth(values::Length), MinHeight(values::Length), MaxHeight(values::Length),
    Orientation(&'static str), ColorScheme(&'static str), ReducedMotion(&'static str), Contrast(&'static str),
    /// `resolution` is bucketed (§3.11): only these values are admitted.
    Resolution(u8),
}

#[derive(Debug, Clone, PartialEq)]
pub struct FontFace { pub family: String, pub src: String, pub weight: u16, pub italic: bool, pub pos: Pos }

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Stylesheet { pub rules: Vec<Rule>, pub font_faces: Vec<FontFace>, pub imports: Vec<(String, Pos)> }

#[derive(Debug, Default)]
pub struct Parsed {
    pub sheet: Stylesheet,
    pub diagnostics: Vec<Diagnostic>,
    /// Set when a ceiling refused the whole sheet (then `sheet` is empty).
    pub refused: Option<Diagnostic>,
}

/// A raw rule: prelude tokens and the tokens inside its `{}` block.
struct Raw<'a> { at: Option<(String, Pos)>, prelude: &'a [Token], block: Option<&'a [Token]>, pos: Pos }

fn raw_rules(toks: &[Token]) -> Vec<Raw<'_>> {
    let mut out = vec![];
    let mut i = 0;
    while i < toks.len() {
        match &toks[i].tok {
            Tok::Whitespace | Tok::Cdo | Tok::Cdc => { i += 1; continue }
            _ => {}
        }
        let pos = toks[i].pos;
        let at = if let Tok::AtKeyword(a) = &toks[i].tok { i += 1; Some((a.clone(), pos)) } else { None };
        let start = i;
        // Prelude runs to `{` (a block) or, for an at-rule, `;` (no block).
        let mut depth = 0i32;
        while i < toks.len() {
            match toks[i].tok {
                Tok::LParen | Tok::Function(_) | Tok::LBracket => depth += 1,
                Tok::RParen | Tok::RBracket => depth -= 1,
                Tok::LBrace if depth <= 0 => break,
                Tok::Semicolon if depth <= 0 && at.is_some() => break,
                _ => {}
            }
            i += 1;
        }
        let prelude = &toks[start..i.min(toks.len())];
        if i >= toks.len() { out.push(Raw { at, prelude, block: None, pos }); break }
        if toks[i].tok == Tok::Semicolon { i += 1; out.push(Raw { at, prelude, block: None, pos }); continue }
        // A `{}` block, braces balanced.
        let bstart = i + 1;
        let mut d = 0i32;
        while i < toks.len() {
            match toks[i].tok { Tok::LBrace => d += 1, Tok::RBrace => { d -= 1; if d == 0 { break } } _ => {} }
            i += 1;
        }
        let block = &toks[bstart..i.min(toks.len())];
        i += 1;
        out.push(Raw { at, prelude, block: Some(block), pos });
    }
    out
}

/// Declarations in a block: `name : value [!important] ;`
fn raw_decls(block: &[Token]) -> Vec<(String, Pos, &[Token])> {
    let mut out = vec![];
    for part in split_semis(block) {
        let p = part.iter().position(|t| t.tok != Tok::Whitespace);
        let Some(p) = p else { continue };
        if let Tok::Ident(name) = &part[p].tok {
            let colon = part[p + 1..].iter().position(|t| t.tok != Tok::Whitespace).map(|k| k + p + 1);
            if let Some(c) = colon.filter(|&c| part[c].tok == Tok::Colon) {
                out.push((name.clone(), part[p].pos, &part[c + 1..]));
                continue;
            }
        }
        out.push((String::new(), part[p].pos, part));
    }
    out
}

fn split_semis(toks: &[Token]) -> Vec<&[Token]> {
    let (mut parts, mut start, mut depth) = (vec![], 0, 0i32);
    for (i, t) in toks.iter().enumerate() {
        match t.tok {
            Tok::LParen | Tok::Function(_) | Tok::LBracket | Tok::LBrace => depth += 1,
            Tok::RParen | Tok::RBracket | Tok::RBrace => depth -= 1,
            Tok::Semicolon if depth <= 0 => { parts.push(&toks[start..i]); start = i + 1 }
            _ => {}
        }
    }
    parts.push(&toks[start..]);
    parts
}

fn strip_important(v: &[Token]) -> (&[Token], bool) {
    let n: Vec<usize> = v.iter().enumerate().filter(|(_, t)| t.tok != Tok::Whitespace).map(|(i, _)| i).collect();
    if n.len() >= 2 {
        let (a, b) = (n[n.len() - 2], n[n.len() - 1]);
        if v[a].tok == Tok::Delim('!') && matches!(&v[b].tok, Tok::Ident(i) if i.eq_ignore_ascii_case("important")) {
            return (&v[..a], true);
        }
    }
    (v, false)
}

pub fn parse_sheet(src: &str) -> Parsed {
    let mut out = Parsed::default();
    let refuse = |code, msg: String| Diagnostic { pos: Pos { line: 1, col: 1 }, code, msg };
    if src.len() > MAX_SHEET_BYTES {
        out.refused = Some(refuse("sheet.too-large", format!("{} bytes exceeds {MAX_SHEET_BYTES} (§3.12)", src.len())));
        return out;
    }
    let toks = tokenize(src);
    let raws = raw_rules(&toks);
    if raws.len() > MAX_RULES {
        out.refused = Some(refuse("sheet.too-many-rules", format!("{} rules exceeds {MAX_RULES} (§3.12)", raws.len())));
        return out;
    }
    // @font-face first: font-family values may name the families it declares.
    let mut faces = vec![];
    for r in &raws {
        if matches!(&r.at, Some((a, _)) if a == "font-face") {
            if let Some(f) = font_face(r, &mut out.diagnostics) { faces.push(f) }
        }
    }
    let families: Vec<String> = faces.iter().map(|f| f.family.clone()).collect();
    out.sheet.font_faces = faces;
    let mut rule_count = 0usize;
    interpret(&raws, None, &families, &mut out, &mut rule_count);
    if rule_count > MAX_RULES {
        out.sheet = Stylesheet::default();
        out.refused = Some(refuse("sheet.too-many-rules", format!("{rule_count} rules exceeds {MAX_RULES} (§3.12)")));
    }
    out
}

fn interpret(raws: &[Raw], media: Option<&Vec<MediaQuery>>, families: &[String], out: &mut Parsed, count: &mut usize) {
    for r in raws {
        *count += 1;
        match &r.at {
            Some((a, pos)) => match a.as_str() {
                "font-face" => {}
                "media" => {
                    if media.is_some() { out.diagnostics.push(Diagnostic { pos: *pos, code: "at-rule.nested-media", msg: "nested @media is not admitted".into() }); continue }
                    let Some(block) = r.block else { continue };
                    match media_list(r.prelude) {
                        Ok(q) => { let inner = raw_rules(block); interpret(&inner, Some(&q), families, out, count) }
                        Err(e) => out.diagnostics.push(Diagnostic { pos: *pos, code: "media.unadmitted", msg: format!("@media {e}; its rules are dropped") }),
                    }
                }
                "import" => match r.prelude.iter().find_map(|t| match &t.tok { Tok::Str(s) | Tok::Url(s) => Some(s.clone()), _ => None }) {
                    // Resolved by whoever supplies the document's inputs;
                    // depth is bounded there (§3.12). Listed, not fetched.
                    Some(u) => out.sheet.imports.push((u, *pos)),
                    None => out.diagnostics.push(Diagnostic { pos: *pos, code: "at-rule.import", msg: "@import without a URL".into() }),
                },
                other => out.diagnostics.push(Diagnostic { pos: *pos, code: "at-rule.unadmitted", msg: format!("@{other} is not admitted; dropped") }),
            },
            None => {
                let Some(block) = r.block else {
                    out.diagnostics.push(Diagnostic { pos: r.pos, code: "syntax", msg: "rule without a block".into() });
                    continue
                };
                let selectors = match selector::parse_list(r.prelude) {
                    Ok(s) => s,
                    Err(e) => { out.diagnostics.push(Diagnostic { pos: r.pos, code: "selector.unadmitted", msg: format!("{e}; rule dropped") }); continue }
                };
                let mut rule = Rule { selectors, decls: vec![], customs: vec![], media: media.cloned(), pos: r.pos };
                for (name, pos, value) in raw_decls(block) {
                    if name.is_empty() { out.diagnostics.push(Diagnostic { pos, code: "syntax", msg: "not a declaration".into() }); continue }
                    let (value, important) = strip_important(value);
                    if important {
                        out.diagnostics.push(Diagnostic { pos, code: "important.excluded", msg: format!("`!important` is excluded (§3.10); `{name}` dropped") });
                        continue;
                    }
                    if let Some(custom) = name.strip_prefix("--") {
                        rule.customs.push((custom.to_string(), value.to_vec()));
                        continue;
                    }
                    let lname = name.to_ascii_lowercase();
                    let Some((prop, _)) = values::ROWS.iter().flat_map(|r| r.props.iter()).find(|p| **p == lname).map(|p| (*p, ())) else {
                        let (code, msg) = if SHORTHANDS.contains(&lname.as_str()) {
                            ("property.shorthand", format!("`{lname}` is a shorthand — longhands only (§3.2)"))
                        } else {
                            ("property.unknown", format!("`{lname}` is not in the profile"))
                        };
                        out.diagnostics.push(Diagnostic { pos, code, msg });
                        continue;
                    };
                    match values::parse_value(prop, value, families) {
                        Ok(v) => rule.decls.push(Decl { prop, value: v, pos }),
                        Err(e) => out.diagnostics.push(Diagnostic { pos, code: "value.invalid", msg: format!("{prop}: {e}") }),
                    }
                }
                out.sheet.rules.push(rule);
            }
        }
    }
}

fn media_list(prelude: &[Token]) -> Result<Vec<MediaQuery>, String> {
    let mut list = vec![];
    for q in prelude.split(|t| t.tok == Tok::Comma) {
        let toks: Vec<&Token> = q.iter().filter(|t| t.tok != Tok::Whitespace).collect();
        let mut feats = vec![];
        let mut i = 0;
        // Optional media type: only `screen` or `all` (a document has no other media).
        if let Some(Tok::Ident(t)) = toks.first().map(|t| &t.tok) {
            match t.to_ascii_lowercase().as_str() {
                "screen" | "all" => i = 1,
                "only" | "not" => return Err(format!("`{t}` is not admitted")),
                other => return Err(format!("media type `{other}` is not admitted")),
            }
        }
        while i < toks.len() {
            match &toks[i].tok {
                Tok::Ident(a) if a.eq_ignore_ascii_case("and") => { i += 1; continue }
                Tok::LParen => {
                    let end = toks[i..].iter().position(|t| t.tok == Tok::RParen).map(|k| k + i).ok_or("unclosed feature")?;
                    feats.push(feature(&toks[i + 1..end])?);
                    i = end + 1;
                }
                other => return Err(format!("unexpected {other:?}")),
            }
        }
        list.push(MediaQuery(feats));
    }
    Ok(list)
}

fn feature(t: &[&Token]) -> Result<Feature, String> {
    let name = match t.first().map(|x| &x.tok) { Some(Tok::Ident(n)) => n.to_ascii_lowercase(), _ => return Err("feature needs a name".into()) };
    let val = t.get(2).map(|x| &x.tok);
    if t.get(1).map(|x| &x.tok) != Some(&Tok::Colon) { return Err(format!("`{name}`: range syntax is not admitted, use min-/max-")) }
    let len = || -> Result<values::Length, String> {
        let tok = t.get(2).map(|x| (*x).clone()).ok_or("missing value")?;
        match values::parse_value("width", &[tok], &[]) {
            Ok(Specified::Value(values::V::Len(l))) if l.v >= 0.0 && !matches!(l.unit, values::Unit::Vw | values::Unit::Vh) => Ok(l),
            _ => Err(format!("`{name}` needs a non-negative length")),
        }
    };
    let kw = |allowed: &[&'static str]| -> Result<&'static str, String> {
        match val { Some(Tok::Ident(v)) => allowed.iter().find(|a| a.eq_ignore_ascii_case(v)).copied().ok_or(format!("`{name}: {v}` is not admitted")),
            _ => Err(format!("`{name}` needs a keyword")) }
    };
    Ok(match name.as_str() {
        "min-width" => Feature::MinWidth(len()?), "max-width" => Feature::MaxWidth(len()?),
        "min-height" => Feature::MinHeight(len()?), "max-height" => Feature::MaxHeight(len()?),
        "orientation" => Feature::Orientation(kw(&["portrait", "landscape"])?),
        "prefers-color-scheme" => Feature::ColorScheme(kw(&["light", "dark"])?),
        "prefers-reduced-motion" => Feature::ReducedMotion(kw(&["no-preference", "reduce"])?),
        "prefers-contrast" => Feature::Contrast(kw(&["no-preference", "more", "less"])?),
        "min-resolution" | "resolution" => match val {
            Some(Tok::Dimension { value, unit, .. }) if unit == "dppx" && [1.0, 2.0, 3.0].contains(value) => Feature::Resolution(*value as u8),
            _ => return Err("resolution is bucketed: 1dppx, 2dppx or 3dppx (§3.11)".into()),
        },
        other => return Err(format!("media feature `{other}` is not admitted (§3.11)")),
    })
}

fn font_face(r: &Raw, diags: &mut Vec<Diagnostic>) -> Option<FontFace> {
    let block = r.block?;
    let (mut family, mut src, mut weight, mut italic) = (None, None, 400u16, false);
    for (name, pos, value) in raw_decls(block) {
        let v: Vec<&Tok> = value.iter().map(|t| &t.tok).filter(|t| **t != Tok::Whitespace).collect();
        match name.to_ascii_lowercase().as_str() {
            "font-family" => match v.as_slice() { [Tok::Str(s)] | [Tok::Ident(s)] => family = Some(s.clone()), _ => diags.push(Diagnostic { pos, code: "font-face.invalid", msg: "font-family is one name".into() }) },
            "src" => match v.as_slice() { [Tok::Url(u)] => src = Some(u.clone()), [Tok::Function(f), Tok::Str(u), Tok::RParen] if f == "url" => src = Some(u.clone()),
                _ => diags.push(Diagnostic { pos, code: "font-face.invalid", msg: "src is exactly one url() — a content hash, no format() lists (web fonts, condition 2)".into() }) },
            "font-weight" => match v.as_slice() { [Tok::Number { value, int: true }] if (100.0..=900.0).contains(value) && (*value as u16) % 100 == 0 => weight = *value as u16,
                _ => diags.push(Diagnostic { pos, code: "font-face.invalid", msg: "font-weight is 100…900 in hundreds".into() }) },
            "font-style" => match v.as_slice() { [Tok::Ident(s)] if s == "normal" || s == "italic" => italic = s == "italic",
                _ => diags.push(Diagnostic { pos, code: "font-face.invalid", msg: "font-style is normal or italic".into() }) },
            other => diags.push(Diagnostic { pos, code: "font-face.descriptor", msg: format!("@font-face `{other}` is not admitted (font-family, src, font-weight, font-style only)") }),
        }
    }
    match (family, src) {
        (Some(family), Some(src)) => Some(FontFace { family, src, weight, italic, pos: r.pos }),
        _ => { diags.push(Diagnostic { pos: r.pos, code: "font-face.invalid", msg: "@font-face needs font-family and src; dropped".into() }); None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(p: &Parsed) -> Vec<&'static str> { p.diagnostics.iter().map(|d| d.code).collect() }

    #[test]
    fn a_clean_sheet_parses_without_diagnostics() {
        let p = parse_sheet("body > main { margin-top: 1rem; color: #222; --gap: 4px }\n.card { padding-left: 8px }");
        assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);
        assert_eq!(p.sheet.rules.len(), 2);
        assert_eq!(p.sheet.rules[0].decls.len(), 2);
        assert_eq!(p.sheet.rules[0].customs[0].0, "gap");
    }

    #[test]
    fn every_refusal_is_reported_with_its_position() {
        let p = parse_sheet("p {\n  margin: 0;\n  colour: red;\n  width: 3pt;\n  color: red !important;\n}\nnav a { color: red }\n@keyframes k { }");
        assert_eq!(codes(&p), vec!["property.shorthand", "property.unknown", "value.invalid", "important.excluded", "selector.unadmitted", "at-rule.unadmitted"]);
        assert_eq!(p.diagnostics[0].pos, Pos { line: 2, col: 3 });
        assert_eq!(p.diagnostics[4].pos.line, 7);
    }

    #[test]
    fn media_queries_admitted_and_refused() {
        let p = parse_sheet("@media screen and (min-width: 600px) and (prefers-color-scheme: dark) { p { color: white } }\n@media print { p { color: black } }\n@media (hover: hover) { p { color: red } }");
        assert_eq!(p.sheet.rules.len(), 1);
        assert_eq!(p.sheet.rules[0].media.as_ref().unwrap()[0].0.len(), 2);
        assert_eq!(codes(&p), vec!["media.unadmitted", "media.unadmitted"]);
    }

    #[test]
    fn font_face_declares_families_font_family_may_use() {
        let p = parse_sheet("@font-face { font-family: \"Inter\"; src: url(\"blake3:abc\"); font-weight: 700 }\nh1 { font-family: \"Inter\", sans }\nh2 { font-family: \"Roboto\", sans }");
        assert_eq!(p.sheet.font_faces.len(), 1);
        assert_eq!(p.sheet.font_faces[0].weight, 700);
        assert_eq!(codes(&p), vec!["value.invalid"], "Roboto was never declared");
    }

    #[test]
    fn ceilings_refuse_the_whole_sheet() {
        let big = "a{}".repeat(MAX_RULES + 1);
        let p = parse_sheet(&big);
        assert_eq!(p.refused.as_ref().map(|d| d.code), Some("sheet.too-many-rules"));
        assert!(p.sheet.rules.is_empty(), "refused, not truncated");
        let huge = " ".repeat(MAX_SHEET_BYTES + 1);
        assert_eq!(parse_sheet(&huge).refused.map(|d| d.code), Some("sheet.too-large"));
    }

    #[test]
    fn garbage_never_panics() {
        for s in ["{", "}", "@media", "@media (", "a{b:", "a{:;}", "a{b:c", "@font-face{", "a[", ":is(", "{{{{}}}}", "a{b:url(}"] {
            let _ = parse_sheet(s);
        }
    }
}
