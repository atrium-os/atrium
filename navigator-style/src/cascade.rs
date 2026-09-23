//! Cascade, inheritance and computed values — profile §3.10.
//!
//! Resolution order: the UA layer, then author layers (one per stylesheet) in
//! declared order, then source order. **Specificity is compared within a
//! layer and never across one** — which is what makes a rule's effect
//! determinable from a bounded context (G4). No `!important`, no descendant
//! combinator: the parser has already refused both.
//!
//! ★ STATIC RENDER, SO STATE IS FIXED. `:hover`/`:focus*`/`:active`/`:target`
//! never match; `:link` matches `a[href]`; `:visited` NEVER matches — a render
//! must not be able to reveal history. `:checked`/`:disabled`/`:enabled`
//! follow the attributes the document declares.

use crate::selector::{AttrOp, Complex, Simple};
use crate::sheet::{parse_sheet, Diagnostic, Feature, MediaQuery, Stylesheet};
use crate::token::{Pos, Tok, Token};
use crate::values::{self, Calc, Length, Specified, Unit, V, ROWS};
use navigator_dom::{Dom, Handle, Kind};
use std::collections::BTreeMap;

pub const MAX_VAR_DEPTH: usize = 16;

/// The declared rendering environment (G6: never the host's).
#[derive(Debug, Clone)]
pub struct Env {
    pub width_px: f64,
    pub height_px: f64,
    pub dark: bool,
    pub reduced_motion: bool,
    pub contrast: &'static str,
    pub dppx: u8,
}
impl Default for Env {
    fn default() -> Self { Env { width_px: 800.0, height_px: 600.0, dark: false, reduced_motion: false, contrast: "no-preference", dppx: 1 } }
}

/// Computed style of one element: every longhand, resolved (no inherit,
/// initial or var() left), lengths in px.
#[derive(Debug, Clone, PartialEq)]
pub struct Style {
    pub values: Vec<V>,
    pub font_size_px: f64,
    pub customs: BTreeMap<String, Vec<Token>>,
}

pub struct Styled {
    /// Indexed by handle; `None` for non-elements.
    pub styles: Vec<Option<Style>>,
    pub diagnostics: Vec<Diagnostic>,
}

/// Every longhand, in row order — the index of a value in `Style::values`.
pub fn longhands() -> Vec<&'static str> { ROWS.iter().flat_map(|r| r.props.iter().copied()).collect() }

pub fn index_of(prop: &str) -> Option<usize> { longhands().iter().position(|p| *p == prop) }

impl Style {
    pub fn get(&self, prop: &str) -> &V { &self.values[index_of(prop).expect("a profile longhand")] }
}

/// The UA layer, written in profile CSS and parsed by the same parser — so a
/// UA rule outside the profile is a test failure, not a privilege.
pub const UA_CSS: &str = "
html, body, main, article, section, nav, aside, header, footer, div, p, h1, h2, h3, h4, h5, h6,
ul, ol, li, dl, dt, dd, figure, figcaption, blockquote, pre, details, summary, form, fieldset, hr, address,
center, dir, menu, legend, search, hgroup, optgroup, marquee
  { display: block }
head, script, style, title, meta, link, template { display: none }
/* ★ `noscript` is RAW TEXT when scripting is enabled, so its markup parses
   as a text node — and the converter DID run the scripts, so the no-script
   fallback is both redundant and, displayed, literal angle brackets on the
   page. */
noscript { display: none }
table { display: table; border-spacing: 2px 2px }
tr { display: table-row }
td, th { display: table-cell; padding-top: 1px; padding-right: 1px; padding-bottom: 1px; padding-left: 1px }
th { font-weight: 700 }
img { display: inline-block }
body { margin-top: 8px; margin-right: 8px; margin-bottom: 8px; margin-left: 8px }
h1 { font-size: 2em; font-weight: 700; margin-top: 0.67em; margin-bottom: 0.67em }
h2 { font-size: 1.5em; font-weight: 700; margin-top: 0.83em; margin-bottom: 0.83em }
h3 { font-size: 1.17em; font-weight: 700; margin-top: 1em; margin-bottom: 1em }
h4 { font-weight: 700; margin-top: 1.33em; margin-bottom: 1.33em }
h5 { font-size: 0.83em; font-weight: 700; margin-top: 1.67em; margin-bottom: 1.67em }
h6 { font-size: 0.67em; font-weight: 700; margin-top: 2.33em; margin-bottom: 2.33em }
p, blockquote, figure, dl { margin-top: 1em; margin-bottom: 1em }
ul, ol { margin-top: 1em; margin-bottom: 1em; padding-left: 40px }
ol { list-style-type: decimal }
blockquote, figure { margin-left: 40px; margin-right: 40px }
dd { margin-left: 40px }
pre { white-space: pre; margin-top: 1em; margin-bottom: 1em }
pre, code, kbd, samp { font-family: mono }
strong, b { font-weight: 700 }
em, i, cite, dfn, var { font-style: italic }
a:link { color: #0969da; text-decoration-line: underline }
hr { border-top-width: 1px; border-top-style: solid; margin-top: 0.5em; margin-bottom: 0.5em }
";

pub fn ua_sheet() -> Stylesheet { parse_sheet(UA_CSS).sheet }

fn media_holds(qs: &[MediaQuery], env: &Env) -> bool {
    qs.iter().any(|q| q.0.iter().all(|f| match f {
        Feature::MinWidth(l) => env.width_px >= px_abs(l, env),
        Feature::MaxWidth(l) => env.width_px <= px_abs(l, env),
        Feature::MinHeight(l) => env.height_px >= px_abs(l, env),
        Feature::MaxHeight(l) => env.height_px <= px_abs(l, env),
        Feature::Orientation(o) => (*o == "portrait") == (env.height_px >= env.width_px),
        Feature::ColorScheme(c) => (*c == "dark") == env.dark,
        Feature::ReducedMotion(m) => (*m == "reduce") == env.reduced_motion,
        Feature::Contrast(c) => *c == env.contrast,
        Feature::Resolution(d) => *d == env.dppx,
    }))
}

/// Lengths in media queries: em/rem against the initial 16px (CSS).
fn px_abs(l: &Length, env: &Env) -> f64 {
    match l.unit { Unit::Px => l.v, Unit::Em | Unit::Rem => l.v * 16.0, Unit::Ch => l.v * 8.0,
                   Unit::Vw => l.v * env.width_px / 100.0, Unit::Vh => l.v * env.height_px / 100.0 }
}

fn matches_complex(dom: &Dom, h: Handle, c: &Complex) -> bool {
    let mut node = Some(h);
    for comp in c.0.iter().rev() {
        let Some(n) = node else { return false };
        if !matches_compound(dom, n, comp) { return false }
        node = dom.get(n).and_then(|x| x.parent).filter(|p| dom.tag(*p).is_some());
    }
    true
}

fn matches_compound(dom: &Dom, h: Handle, comp: &[Simple]) -> bool {
    let Some(tag) = dom.tag(h) else { return false };
    comp.iter().all(|s| match s {
        Simple::Type(t) => tag.eq_ignore_ascii_case(t),
        Simple::Universal => true,
        Simple::Class(c) => dom.attr(h, "class").is_some_and(|v| v.split_ascii_whitespace().any(|x| x == c)),
        Simple::Id(i) => dom.attr(h, "id") == Some(i.as_str()),
        Simple::Attr { name, op, value } => match dom.attr(h, name) {
            None => false,
            Some(v) => match op {
                AttrOp::Exists => true,
                AttrOp::Eq => v == value,
                AttrOp::Word => v.split_ascii_whitespace().any(|x| x == value),
                AttrOp::Dash => v == value || v.starts_with(&format!("{value}-")),
                AttrOp::Prefix => !value.is_empty() && v.starts_with(value.as_str()),
                AttrOp::Suffix => !value.is_empty() && v.ends_with(value.as_str()),
                AttrOp::Contains => !value.is_empty() && v.contains(value.as_str()),
            },
        },
        Simple::State(st) => match *st {
            "link" => (tag == "a" || tag == "area") && dom.attr(h, "href").is_some(),
            "checked" => dom.attr(h, "checked").is_some(),
            "disabled" => dom.attr(h, "disabled").is_some(),
            "enabled" => matches!(tag, "input" | "button" | "select" | "textarea") && dom.attr(h, "disabled").is_none(),
            _ => false, // hover, focus*, active, target, visited: never in a static render
        },
        Simple::Is(l) | Simple::Where(l) => l.iter().any(|c| matches_complex(dom, h, c)),
    })
}

/// Substitute `var()` references using `customs`, bounded and cycle-checked.
fn substitute(toks: &[Token], customs: &BTreeMap<String, Vec<Token>>, depth: usize, stack: &mut Vec<String>) -> Result<Vec<Token>, String> {
    if depth > MAX_VAR_DEPTH { return Err(format!("var() substitution deeper than {MAX_VAR_DEPTH} (§3.12)")) }
    let mut out = Vec::with_capacity(toks.len());
    let mut i = 0;
    while i < toks.len() {
        if !matches!(&toks[i].tok, Tok::Function(f) if f == "var") { out.push(toks[i].clone()); i += 1; continue }
        // Find the matching close paren.
        let (mut d, mut end) = (0i32, None);
        for (k, t) in toks.iter().enumerate().skip(i) {
            match t.tok { Tok::Function(_) | Tok::LParen => d += 1, Tok::RParen => { d -= 1; if d == 0 { end = Some(k); break } } _ => {} }
        }
        let end = end.ok_or("unclosed var()")?;
        let inner = &toks[i + 1..end];
        let name_at = inner.iter().position(|t| t.tok != Tok::Whitespace).ok_or("empty var()")?;
        let name = match &inner[name_at].tok { Tok::Ident(n) if n.starts_with("--") => n[2..].to_string(), _ => return Err("var() needs a --name".into()) };
        let fallback = inner[name_at + 1..].iter().position(|t| t.tok == Tok::Comma).map(|c| &inner[name_at + 1 + c + 1..]);
        if stack.contains(&name) { return Err(format!("custom property cycle through --{name} (§3.6)")) }
        let replacement = match customs.get(&name) {
            Some(v) => { stack.push(name.clone()); let r = substitute(v, customs, depth + 1, stack); stack.pop(); r? }
            None => match fallback { Some(f) => substitute(f, customs, depth + 1, stack)?, None => return Err(format!("--{name} is not defined and var() has no fallback")) },
        };
        out.extend(replacement);
        i = end + 1;
    }
    Ok(out)
}

struct Base { font_px: f64, root_px: f64, vw: f64, vh: f64 }

fn len_px(l: &Length, b: &Base) -> f64 {
    match l.unit { Unit::Px => l.v, Unit::Em => l.v * b.font_px, Unit::Rem => l.v * b.root_px,
                   // `ch` without the font's measured "0" advance: CSS's 0.5em fallback.
                   Unit::Ch => l.v * b.font_px * 0.5, Unit::Vw => l.v * b.vw / 100.0, Unit::Vh => l.v * b.vh / 100.0 }
}

fn calc_px(c: &Calc, b: &Base) -> Calc {
    let r = |x: &Calc| Box::new(calc_px(x, b));
    match c {
        Calc::Len(l) => Calc::Len(Length { v: len_px(l, b), unit: Unit::Px }),
        Calc::Num(_) | Calc::Pct(_) => c.clone(),
        Calc::Add(x, y) => Calc::Add(r(x), r(y)), Calc::Sub(x, y) => Calc::Sub(r(x), r(y)),
        Calc::Mul(x, y) => Calc::Mul(r(x), r(y)), Calc::Div(x, y) => Calc::Div(r(x), r(y)),
        Calc::Min(v) => Calc::Min(v.iter().map(|x| calc_px(x, b)).collect()),
        Calc::Max(v) => Calc::Max(v.iter().map(|x| calc_px(x, b)).collect()),
        Calc::Clamp(x, y, z) => Calc::Clamp(r(x), r(y), r(z)),
    }
}

fn to_px(v: V, b: &Base) -> V {
    match v {
        V::Len(l) => V::Len(Length { v: len_px(&l, b), unit: Unit::Px }),
        V::Calc(c) => V::Calc(Box::new(calc_px(&c, b))),
        V::Pair(x, y) => V::Pair(Box::new(to_px(*x, b)), Box::new(to_px(*y, b))),
        V::Shadow { x, y, blur, spread, color } => V::Shadow {
            x: Length { v: len_px(&x, b), unit: Unit::Px }, y: Length { v: len_px(&y, b), unit: Unit::Px },
            blur: Length { v: len_px(&blur, b), unit: Unit::Px }, spread: Length { v: len_px(&spread, b), unit: Unit::Px }, color },
        other => other,
    }
}

/// Evaluate a length-typed calc whose lengths are already px, with no
/// percentage basis (font-size resolves % first). `None` if it uses %.
fn calc_eval_px(c: &Calc) -> Option<f64> {
    Some(match c {
        Calc::Num(n) => *n, Calc::Len(l) => l.v, Calc::Pct(_) => return None,
        Calc::Add(a, b) => calc_eval_px(a)? + calc_eval_px(b)?, Calc::Sub(a, b) => calc_eval_px(a)? - calc_eval_px(b)?,
        Calc::Mul(a, b) => calc_eval_px(a)? * calc_eval_px(b)?, Calc::Div(a, b) => calc_eval_px(a)? / calc_eval_px(b)?,
        // ★ One unresolvable argument makes the whole comparison
        // unresolvable: `min(10px, 50%)` is not 10px until the percentage is
        // known, and answering early would be a guess.
        Calc::Min(v) => v.iter().map(calc_eval_px).collect::<Option<Vec<_>>>()?.into_iter().fold(f64::INFINITY, f64::min),
        Calc::Max(v) => v.iter().map(calc_eval_px).collect::<Option<Vec<_>>>()?.into_iter().fold(f64::NEG_INFINITY, f64::max),
        Calc::Clamp(lo, val, hi) => calc_eval_px(val)?.clamp(calc_eval_px(lo)?, calc_eval_px(hi)?.max(calc_eval_px(lo)?)),
    })
}

pub fn cascade(dom: &Dom, authors: &[Stylesheet], env: &Env) -> Styled {
    let ua = ua_sheet();
    let layers: Vec<&Stylesheet> = std::iter::once(&ua).chain(authors.iter()).collect();
    let props = longhands();
    let mut diagnostics = vec![];
    let mut styles: Vec<Option<Style>> = vec![None; dom.nodes.len()];
    let initial: Vec<V> = props.iter().map(|p| {
        let (_, init, _) = values::grammar(p).expect("row");
        match values::parse_value(p, &crate::token::tokenize(init), &[]) { Ok(Specified::Value(v)) => v, other => panic!("initial {p}: {other:?}") }
    }).collect();
    let web: Vec<String> = authors.iter().flat_map(|s| s.font_faces.iter().map(|f| f.family.clone())).collect();

    // Document order, parents before children.
    let mut order = vec![];
    let mut stack = vec![dom.root()];
    while let Some(h) = stack.pop() {
        order.push(h);
        if let Some(n) = dom.get(h) { for &c in n.children.iter().rev() { stack.push(c) } }
    }
    let mut root_px = 16.0;
    for h in order {
        if dom.tag(h).is_none() || !matches!(dom.get(h).map(|n| &n.kind), Some(Kind::Element(_))) { continue }
        let parent = dom.get(h).and_then(|n| n.parent).and_then(|p| styles.get(p as usize).cloned().flatten());
        // Matching declarations, in (layer, specificity, source) order.
        // ★ G4: the key is (layer, specificity, SOURCE POSITION) — what the
        // author wrote — never the order rules happen to be iterated in.
        let mut hits: Vec<((usize, (u32, u32, u32), Pos), &crate::sheet::Rule)> = vec![];
        for (li, sheet) in layers.iter().enumerate() {
            for rule in sheet.rules.iter() {
                if let Some(m) = &rule.media { if !media_holds(m, env) { continue } }
                let best = rule.selectors.iter().filter(|c| matches_complex(dom, h, c)).map(crate::selector::specificity).max();
                if let Some(spec) = best { hits.push(((li, spec, rule.pos), rule)) }
            }
        }
        hits.sort_by_key(|(k, _)| *k);
        let mut customs = parent.as_ref().map(|p| p.customs.clone()).unwrap_or_default();
        let mut specified: Vec<Option<(Specified, Pos)>> = vec![None; props.len()];
        for (_, rule) in &hits {
            for (name, toks) in &rule.customs { customs.insert(name.clone(), toks.clone()); }
            for d in &rule.decls {
                if let Some(i) = props.iter().position(|p| *p == d.prop) { specified[i] = Some((d.value.clone(), d.pos)) }
            }
        }
        // font-size first: every other em resolves against it.
        let parent_font = parent.as_ref().map(|p| p.font_size_px).unwrap_or(16.0);
        let is_root = parent.is_none();
        let fs_i = props.iter().position(|p| *p == "font-size").expect("row 34");
        let mut values = vec![V::Kw("unset"); props.len()];
        let resolve = |i: usize, spec: Option<&(Specified, Pos)>, diags: &mut Vec<Diagnostic>| -> Option<V> {
            let (_, _, inherited) = values::grammar(props[i]).expect("row");
            let from_parent = || parent.as_ref().map(|p| p.values[i].clone());
            match spec {
                None => if inherited { from_parent() } else { None },
                Some((Specified::Inherit, _)) => from_parent(),
                Some((Specified::Initial, _)) => None,
                Some((Specified::Value(v), _)) => Some(v.clone()),
                Some((Specified::Unresolved(toks), pos)) => {
                    let parsed = substitute(toks, &customs, 0, &mut vec![])
                        .and_then(|t| values::parse_value(props[i], &t, &web));
                    match parsed {
                        Ok(Specified::Value(v)) => Some(v),
                        Ok(Specified::Inherit) => from_parent(),
                        Ok(_) => None,
                        Err(e) => {
                            diags.push(Diagnostic { pos: *pos, code: "value.var-invalid", msg: format!("{}: {e}; using the {} value", props[i], if inherited { "inherited" } else { "initial" }) });
                            if inherited { from_parent() } else { None }
                        }
                    }
                }
            }
        };
        let fs_v = resolve(fs_i, specified[fs_i].as_ref(), &mut diagnostics);
        let font_px = match &fs_v {
            None => if is_root { 16.0 } else { parent_font },
            Some(V::Len(l)) => len_px(l, &Base { font_px: parent_font, root_px, vw: env.width_px, vh: env.height_px }),
            Some(V::Pct(p)) => parent_font * p / 100.0,
            Some(V::Calc(c)) => {
                let c = calc_px(c, &Base { font_px: parent_font, root_px, vw: env.width_px, vh: env.height_px });
                calc_eval_px(&c).unwrap_or(parent_font)
            }
            Some(_) => parent_font,
        };
        if is_root { root_px = font_px }
        let base = Base { font_px, root_px, vw: env.width_px, vh: env.height_px };
        for i in 0..props.len() {
            values[i] = if i == fs_i { V::Len(Length { v: font_px, unit: Unit::Px }) } else {
                let inherited_already = specified[i].is_none() && values::grammar(props[i]).expect("row").2;
                match resolve(i, specified[i].as_ref(), &mut diagnostics) {
                    // An inherited value is already computed (px) by the parent.
                    Some(v) if inherited_already || matches!(specified[i], Some((Specified::Inherit, _))) => v,
                    Some(v) => to_px(v, &base),
                    None => to_px(initial[i].clone(), &base),
                }
            };
        }
        styles[h as usize] = Some(Style { values, font_size_px: font_px, customs });
    }
    Styled { styles, diagnostics }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::values::{Color, Rgba};

    fn styled(html: &str, css: &str) -> (Dom, Styled) {
        let dom = navigator_dom::parse(html);
        let p = parse_sheet(css);
        assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);
        let s = cascade(&dom, &[p.sheet], &Env::default());
        (dom, s)
    }
    fn el(dom: &Dom, id: &str) -> Handle { dom.by_id(id).expect("id") }
    fn px(v: &V) -> f64 { match v { V::Len(l) => l.v, other => panic!("{other:?}") } }

    #[test]
    fn the_ua_layer_is_profile_css() {
        let p = parse_sheet(UA_CSS);
        assert!(p.diagnostics.is_empty(), "UA sheet outside the profile: {:?}", p.diagnostics);
    }

    #[test]
    fn specificity_within_a_layer_source_order_breaks_ties() {
        let (d, s) = styled("<p id=a class=c>x</p>", "#a { color: red } .c { color: blue } p { color: green } .c { color: black }");
        assert_eq!(s.styles[el(&d, "a") as usize].as_ref().unwrap().get("color"), &V::Color(Color::Rgba(Rgba { r: 255, g: 0, b: 0, a: 255 })));
    }

    #[test]
    fn a_later_layer_wins_regardless_of_specificity() {
        let dom = navigator_dom::parse("<p id=a>x</p>");
        let l1 = parse_sheet("#a { color: red }").sheet;
        let l2 = parse_sheet("p { color: blue }").sheet;
        let s = cascade(&dom, &[l1, l2], &Env::default());
        assert_eq!(s.styles[el(&dom, "a") as usize].as_ref().unwrap().get("color"),
                   &V::Color(Color::Rgba(Rgba { r: 0, g: 0, b: 255, a: 255 })), "specificity cannot cross a layer (§3.10)");
    }

    #[test]
    fn inheritance_and_em_resolution() {
        let (d, s) = styled("<div id=o><p id=i>x</p></div>", "#o { font-size: 20px; color: #00f } #i { font-size: 1.5em; margin-top: 2em; padding-left: 10% }");
        let i = s.styles[el(&d, "i") as usize].as_ref().unwrap();
        assert_eq!(i.font_size_px, 30.0);
        assert_eq!(px(i.get("margin-top")), 60.0, "em against the element's own font-size");
        assert_eq!(i.get("padding-left"), &V::Pct(10.0), "percentages wait for layout");
        assert_eq!(i.get("color"), &V::Color(Color::Rgba(Rgba { r: 0, g: 0, b: 255, a: 255 })), "color inherits");
        let o = s.styles[el(&d, "o") as usize].as_ref().unwrap();
        assert_eq!(px(o.get("margin-top")), 0.0, "margin does not inherit");
    }

    #[test]
    fn child_combinator_only_matches_parents() {
        let (d, s) = styled("<nav id=n><ul><li id=deep>x</li></ul><li id=direct>y</li></nav>", "nav > li { color: red }");
        let red = V::Color(Color::Rgba(Rgba { r: 255, g: 0, b: 0, a: 255 }));
        assert_eq!(s.styles[el(&d, "direct") as usize].as_ref().unwrap().get("color"), &red);
        assert_ne!(s.styles[el(&d, "deep") as usize].as_ref().unwrap().get("color"), &red);
    }

    #[test]
    fn custom_properties_inherit_and_substitute() {
        let (d, s) = styled("<div id=o><p id=i>x</p></div>", "#o { --gap: 12px; --c: var(--gap) } #i { margin-top: var(--c); margin-left: var(--nope, 3px) }");
        let i = s.styles[el(&d, "i") as usize].as_ref().unwrap();
        assert_eq!(px(i.get("margin-top")), 12.0);
        assert_eq!(px(i.get("margin-left")), 3.0);
    }

    #[test]
    fn a_var_cycle_is_a_diagnostic_and_falls_back() {
        let (d, s) = styled("<p id=i>x</p>", "#i { --a: var(--b); --b: var(--a); margin-top: var(--a) }");
        assert!(s.diagnostics.iter().any(|x| x.code == "value.var-invalid" && x.msg.contains("cycle")));
        assert_eq!(px(s.styles[el(&d, "i") as usize].as_ref().unwrap().get("margin-top")), 0.0, "initial");
    }

    #[test]
    fn media_queries_use_the_declared_environment() {
        let dom = navigator_dom::parse("<p id=i>x</p>");
        let css = parse_sheet("@media (min-width: 1000px) { #i { color: red } } @media (prefers-color-scheme: dark) { #i { color: white } }").sheet;
        let narrow = cascade(&dom, std::slice::from_ref(&css), &Env::default());
        assert_eq!(narrow.styles[el(&dom, "i") as usize].as_ref().unwrap().get("color"), &V::Color(Color::Rgba(Rgba { r: 0x1f, g: 0x23, b: 0x28, a: 255 })));
        let dark = cascade(&dom, &[css], &Env { dark: true, width_px: 1200.0, ..Env::default() });
        assert_eq!(dark.styles[el(&dom, "i") as usize].as_ref().unwrap().get("color"), &V::Color(Color::Rgba(Rgba { r: 255, g: 255, b: 255, a: 255 })));
    }

    #[test]
    fn visited_never_matches() {
        let (d, s) = styled("<a id=a href=x>l</a>", "a:visited { color: red } a:link { color: blue }");
        assert_eq!(s.styles[el(&d, "a") as usize].as_ref().unwrap().get("color"), &V::Color(Color::Rgba(Rgba { r: 0, g: 0, b: 255, a: 255 })));
    }

    /// G4: resolution does not depend on the order rules are EVALUATED in —
    /// only on (layer, specificity, source position). Every element, every
    /// longhand, identical under reversed and interleaved rule vectors.
    #[test]
    fn resolution_is_invariant_under_evaluation_order() {
        let css = "p { color: red; margin-top: 1px }\n.c { color: green }\n#a { margin-top: 5px }\n\
                   p { margin-top: 2px; padding-left: 1px }\n.c { margin-top: 3px; padding-left: 2px }\n\
                   @media (min-width: 100px) { p { padding-left: 9px } }\nli { color: blue }\nul > li.c { color: black }";
        let dom = navigator_dom::parse("<p id=a class=c>x</p><p class=c>y</p><p>z</p><ul><li class=c>1</li><li>2</li></ul>");
        let sheet = parse_sheet(css).sheet;
        let base = cascade(&dom, std::slice::from_ref(&sheet), &Env::default());
        let mut rev = sheet.clone();
        rev.rules.reverse();
        let mut inter = sheet.clone();
        let n = inter.rules.len();
        inter.rules = (0..n).map(|i| sheet.rules[(i * 3) % n].clone()).collect(); // 3 is coprime with 8
        assert_eq!(n % 3 != 0, true);
        for other in [rev, inter] {
            let r = cascade(&dom, &[other], &Env::default());
            assert_eq!(base.styles, r.styles, "a style changed with evaluation order");
        }
        // Control: the test can see a real change — drop one rule.
        let mut fewer = sheet.clone();
        fewer.rules.remove(2);
        assert_ne!(base.styles, cascade(&dom, &[fewer], &Env::default()).styles, "the comparison can detect a difference");
    }
}
