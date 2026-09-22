//! Admitted selectors — profile §3.10.
//!
//! Admitted: type, `*`, `.class`, `#id`, attribute selectors, state
//! pseudo-classes, the child combinator `>`, and `:is()`/`:where()` over
//! admitted selectors. Everything else is REFUSED with a reason: the
//! descendant combinator (whitespace), `+` and `~`, structural pseudo-classes,
//! `:not()`, and pseudo-elements. A refused selector drops its rule — the
//! diagnostic says so — rather than matching some approximation of it.

use crate::token::{Tok, Token};

#[derive(Debug, Clone, PartialEq)]
pub enum AttrOp { Exists, Eq, Word, Dash, Prefix, Suffix, Contains }

#[derive(Debug, Clone, PartialEq)]
pub enum Simple {
    Type(String),
    Universal,
    Class(String),
    Id(String),
    Attr { name: String, op: AttrOp, value: String },
    State(&'static str),
    Is(Vec<Complex>),
    Where(Vec<Complex>),
}

/// Compounds joined by the child combinator, left to right.
#[derive(Debug, Clone, PartialEq)]
pub struct Complex(pub Vec<Vec<Simple>>);

/// (ids, classes+attrs+states, types) — CSS specificity, compared lexically.
pub type Specificity = (u32, u32, u32);

pub const STATES: &[&str] = &["hover", "focus", "focus-visible", "focus-within", "active",
                              "link", "visited", "checked", "disabled", "enabled", "target"];
const MAX_NEST: usize = 8;

type R<T> = Result<T, String>;

pub fn parse_list(toks: &[Token]) -> R<Vec<Complex>> { parse_list_at(toks, 0) }

fn parse_list_at(toks: &[Token], depth: usize) -> R<Vec<Complex>> {
    if depth > MAX_NEST { return Err("selector nested too deeply".into()) }
    let mut out = vec![];
    for part in split_top(toks) {
        out.push(parse_complex(part, depth)?);
    }
    if out.is_empty() { return Err("empty selector".into()) }
    Ok(out)
}

/// Split on top-level commas (not inside :is()/:where()).
fn split_top(toks: &[Token]) -> Vec<&[Token]> {
    let (mut parts, mut start, mut depth) = (vec![], 0, 0i32);
    for (i, t) in toks.iter().enumerate() {
        match t.tok {
            Tok::Function(_) | Tok::LParen => depth += 1,
            Tok::RParen => depth -= 1,
            Tok::Comma if depth == 0 => { parts.push(&toks[start..i]); start = i + 1 }
            _ => {}
        }
    }
    parts.push(&toks[start..]);
    parts
}

fn trim(toks: &[Token]) -> &[Token] {
    let s = toks.iter().position(|t| t.tok != Tok::Whitespace).unwrap_or(toks.len());
    let e = toks.iter().rposition(|t| t.tok != Tok::Whitespace).map(|i| i + 1).unwrap_or(s);
    &toks[s..e.max(s)]
}

fn parse_complex(toks: &[Token], depth: usize) -> R<Complex> {
    let toks = trim(toks);
    if toks.is_empty() { return Err("empty selector".into()) }
    let mut compounds = vec![vec![]];
    let mut i = 0;
    let mut pending_ws = false;
    while i < toks.len() {
        match &toks[i].tok {
            Tok::Whitespace => { pending_ws = true; i += 1; continue }
            Tok::Delim('>') => {
                if compounds.last().map(|c: &Vec<Simple>| c.is_empty()).unwrap_or(true) { return Err("`>` with nothing on its left".into()) }
                compounds.push(vec![]); pending_ws = false; i += 1; continue
            }
            Tok::Delim(c @ ('+' | '~')) => return Err(format!("the `{c}` combinator is not admitted (§3.10)")),
            _ => {}
        }
        if pending_ws && !compounds.last().expect("non-empty").is_empty() {
            return Err("the descendant combinator (whitespace) is not admitted — use `>` (§3.10)".into());
        }
        pending_ws = false;
        let (s, used) = simple(&toks[i..], depth)?;
        compounds.last_mut().expect("non-empty").push(s);
        i += used;
    }
    if compounds.iter().any(|c| c.is_empty()) { return Err("`>` with nothing on its right".into()) }
    Ok(Complex(compounds))
}

fn simple(t: &[Token], depth: usize) -> R<(Simple, usize)> {
    match &t[0].tok {
        Tok::Ident(n) => Ok((Simple::Type(n.to_ascii_lowercase()), 1)),
        Tok::Delim('*') => Ok((Simple::Universal, 1)),
        Tok::Hash(h, true) => Ok((Simple::Id(h.clone()), 1)),
        Tok::Delim('.') => match t.get(1).map(|x| &x.tok) {
            Some(Tok::Ident(c)) => Ok((Simple::Class(c.clone()), 2)),
            _ => Err("`.` must be followed by a class name".into()),
        },
        Tok::LBracket => {
            let end = t.iter().position(|x| x.tok == Tok::RBracket).ok_or("unclosed attribute selector")?;
            let inner: Vec<&Tok> = t[1..end].iter().map(|x| &x.tok).filter(|x| **x != Tok::Whitespace).collect();
            let name = match inner.first() { Some(Tok::Ident(n)) => n.to_ascii_lowercase(), _ => return Err("attribute selector needs a name".into()) };
            let (op, rest) = match &inner[1..] {
                [] => (AttrOp::Exists, &inner[1..1]),
                [Tok::Delim('='), r @ ..] => (AttrOp::Eq, r),
                [Tok::Delim(c), Tok::Delim('='), r @ ..] => (match c { '~' => AttrOp::Word, '|' => AttrOp::Dash, '^' => AttrOp::Prefix,
                    '$' => AttrOp::Suffix, '*' => AttrOp::Contains, _ => return Err(format!("unknown attribute operator `{c}=`")) }, r),
                _ => return Err("malformed attribute selector".into()),
            };
            let value = match rest {
                [] if op == AttrOp::Exists => String::new(),
                [Tok::Str(v)] | [Tok::Ident(v)] => v.clone(),
                [Tok::Str(v), Tok::Ident(f)] | [Tok::Ident(v), Tok::Ident(f)] if f.eq_ignore_ascii_case("i") || f.eq_ignore_ascii_case("s") =>
                    return Err(format!("the `{f}` attribute flag is not admitted; value {v:?}")),
                _ => return Err("malformed attribute value".into()),
            };
            Ok((Simple::Attr { name, op, value }, end + 1))
        }
        Tok::Colon => match t.get(1).map(|x| &x.tok) {
            Some(Tok::Colon) => Err("pseudo-elements are not admitted (§3.10)".into()),
            Some(Tok::Ident(p)) => {
                let p = p.to_ascii_lowercase();
                STATES.iter().find(|s| **s == p).map(|s| (Simple::State(s), 2))
                    .ok_or_else(|| format!("`:{p}` is not admitted — state pseudo-classes only (§3.10)"))
            }
            Some(Tok::Function(f)) if f == "is" || f == "where" => {
                // Find the matching close paren.
                let mut d = 0;
                let mut end = None;
                for (k, x) in t.iter().enumerate().skip(1) {
                    match x.tok { Tok::Function(_) | Tok::LParen => d += 1, Tok::RParen => { d -= 1; if d == 0 { end = Some(k); break } } _ => {} }
                }
                let end = end.ok_or("unclosed :is()/:where()")?;
                let list = parse_list_at(&t[2..end], depth + 1)?;
                Ok((if f == "is" { Simple::Is(list) } else { Simple::Where(list) }, end + 1))
            }
            Some(Tok::Function(f)) => Err(format!("`:{f}()` is not admitted (§3.10)")),
            _ => Err("`:` must be followed by a pseudo-class".into()),
        },
        other => Err(format!("unexpected {other:?} in a selector")),
    }
}

pub fn specificity(c: &Complex) -> Specificity {
    let mut s = (0, 0, 0);
    for comp in &c.0 { for simple in comp { add(&mut s, simple) } }
    s
}

fn add(s: &mut Specificity, simple: &Simple) {
    match simple {
        Simple::Id(_) => s.0 += 1,
        Simple::Class(_) | Simple::Attr { .. } | Simple::State(_) => s.1 += 1,
        Simple::Type(_) => s.2 += 1,
        Simple::Universal | Simple::Where(_) => {}
        Simple::Is(list) => { let m = list.iter().map(specificity).max().unwrap_or((0, 0, 0)); s.0 += m.0; s.1 += m.1; s.2 += m.2 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::token::tokenize;
    fn sel(s: &str) -> R<Vec<Complex>> { parse_list(&tokenize(s)) }

    #[test]
    fn admitted_forms() {
        let l = sel("nav > ul > li.item#x[data-k^=\"a\"]:hover, :is(h1, .t) > a, :where(p)").unwrap();
        assert_eq!(l.len(), 3);
        assert_eq!(l[0].0.len(), 3);
        assert_eq!(specificity(&l[0]), (1, 3, 3));
        assert_eq!(specificity(&l[1]), (0, 1, 1), ":is() takes its most specific argument");
        assert_eq!(specificity(&l[2]), (0, 0, 0), ":where() contributes nothing");
    }

    #[test]
    fn excluded_forms_are_refused_with_reasons() {
        assert!(sel("nav a").unwrap_err().contains("descendant"));
        assert!(sel("h1 + p").unwrap_err().contains("`+`"));
        assert!(sel("h1 ~ p").unwrap_err().contains("`~`"));
        assert!(sel("li:first-child").unwrap_err().contains("state pseudo-classes only"));
        assert!(sel("p:not(.x)").unwrap_err().contains(":not"));
        assert!(sel("p::before").unwrap_err().contains("pseudo-elements"));
        assert!(sel("> p").is_err());
        assert!(sel("p >").is_err());
        assert!(sel("[a=b i]").unwrap_err().contains("flag"));
    }

    #[test]
    fn whitespace_around_child_combinator_is_fine() {
        assert!(sel("a >  b").is_ok());
        assert!(sel("  a,b  ").is_ok());
    }

    #[test]
    fn nesting_is_bounded() {
        let deep = format!("{}a{}", ":is(".repeat(20), ")".repeat(20));
        assert!(sel(&deep).is_err());
    }
}
