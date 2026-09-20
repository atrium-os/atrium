//! A CSS selector subset for the converter.
//!
//! ★ Deliberately MORE permissive than the document profile's §3.10, which
//! admits no descendant combinator. That restriction is an authoring rule for
//! content targeting the profile; real pages being converted use descendant
//! selectors constantly, and tolerance belongs in the converter (profile §6).
//!
//! Matching is right-to-left from a candidate element, which is how browsers
//! do it and avoids walking the whole tree per combinator.

use crate::dom::{Dom, Handle, Kind};

#[derive(Debug, Clone, PartialEq)]
pub enum AttrOp { Exists, Eq, Prefix, Suffix, Contains, Word }

#[derive(Debug, Clone, PartialEq)]
pub struct AttrPred { pub name: String, pub op: AttrOp, pub value: String }

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Compound {
    pub tag: Option<String>,          // None = universal
    pub id: Option<String>,
    pub classes: Vec<String>,
    pub attrs: Vec<AttrPred>,
    pub pseudos: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Comb { Descendant, Child, Adjacent, Sibling }

#[derive(Debug, Clone, PartialEq)]
pub struct Complex {
    pub key: Compound,
    /// Leftwards from the key: (combinator joining to the thing on its right).
    pub rest: Vec<(Comb, Compound)>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SelectorList(pub Vec<Complex>);

pub fn parse(input: &str) -> Option<SelectorList> {
    let mut out = vec![];
    for part in split_top(input, ',') {
        let c = parse_complex(part.trim())?;
        out.push(c);
    }
    if out.is_empty() { None } else { Some(SelectorList(out)) }
}

/// Split on a delimiter that is not inside brackets or parentheses.
fn split_top(s: &str, d: char) -> Vec<String> {
    let (mut out, mut cur, mut depth) = (vec![], String::new(), 0i32);
    for c in s.chars() {
        match c {
            '[' | '(' => { depth += 1; cur.push(c) }
            ']' | ')' => { depth -= 1; cur.push(c) }
            _ if c == d && depth == 0 => { out.push(std::mem::take(&mut cur)) }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() { out.push(cur) }
    out
}

fn parse_complex(s: &str) -> Option<Complex> {
    // Tokenise into compounds and combinators, left to right.
    let mut items: Vec<Result<Compound, Comb>> = vec![];
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    let mut depth = 0i32;
    while let Some(c) = chars.next() {
        match c {
            '[' | '(' => { depth += 1; cur.push(c) }
            ']' | ')' => { depth -= 1; cur.push(c) }
            '>' | '+' | '~' if depth == 0 => {
                if !cur.trim().is_empty() { items.push(Ok(parse_compound(cur.trim())?)); }
                cur.clear();
                items.push(Err(match c { '>' => Comb::Child, '+' => Comb::Adjacent, _ => Comb::Sibling }));
            }
            c if c.is_whitespace() && depth == 0 => {
                if !cur.trim().is_empty() { items.push(Ok(parse_compound(cur.trim())?)); cur.clear(); }
                // a descendant combinator unless the next non-space is > + ~
                let mut it = chars.clone();
                while let Some(&n) = it.peek() { if n.is_whitespace() { it.next(); } else { break } }
                // A space is a descendant combinator only BETWEEN compounds.
                // After an explicit `>`/`+`/`~` the space is just layout —
                // treating it as a combinator made `#a > p` parse as
                // child-then-descendant and fail outright.
                let after_compound = matches!(items.last(), Some(Ok(_)));
                if after_compound
                    && !matches!(it.peek(), Some('>') | Some('+') | Some('~') | None)
                {
                    items.push(Err(Comb::Descendant));
                }
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() { items.push(Ok(parse_compound(cur.trim())?)); }

    let key = match items.pop()? { Ok(c) => c, Err(_) => return None };
    let mut rest = vec![];
    while let Some(it) = items.pop() {
        let comb = match it { Err(c) => c, Ok(_) => return None };
        let comp = match items.pop()? { Ok(c) => c, Err(_) => return None };
        rest.push((comb, comp));
    }
    Some(Complex { key, rest })
}

fn parse_compound(s: &str) -> Option<Compound> {
    let mut c = Compound::default();
    let b: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    // leading type or universal
    if i < b.len() && (b[i].is_ascii_alphabetic() || b[i] == '*') {
        let st = i;
        while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == '-' || b[i] == '_' || b[i] == '*') { i += 1 }
        let t: String = b[st..i].iter().collect();
        if t != "*" { c.tag = Some(t.to_ascii_lowercase()) }
    }
    while i < b.len() {
        match b[i] {
            '#' | '.' => {
                let kind = b[i]; i += 1; let st = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == '-' || b[i] == '_') { i += 1 }
                let v: String = b[st..i].iter().collect();
                if v.is_empty() { return None }
                if kind == '#' { c.id = Some(v) } else { c.classes.push(v) }
            }
            '[' => {
                let st = i + 1;
                while i < b.len() && b[i] != ']' { i += 1 }
                if i >= b.len() { return None }
                let inner: String = b[st..i].iter().collect();
                i += 1;
                c.attrs.push(parse_attr(&inner)?);
            }
            ':' => {
                let st = i; i += 1;
                if i < b.len() && b[i] == ':' { i += 1 }  // ::before — no match, but parse
                let mut depth = 0i32;
                while i < b.len() {
                    match b[i] { '(' => depth += 1, ')' => depth -= 1, _ => {} }
                    if depth == 0 && (b[i] == '#' || b[i] == '.' || b[i] == '[') { break }
                    i += 1;
                    if depth == 0 && i < b.len() && b[i] == ':' { break }
                }
                c.pseudos.push(b[st..i].iter().collect::<String>().to_ascii_lowercase());
            }
            _ => return None,
        }
    }
    Some(c)
}

fn parse_attr(s: &str) -> Option<AttrPred> {
    for (tok, op) in [("^=", AttrOp::Prefix), ("$=", AttrOp::Suffix), ("*=", AttrOp::Contains),
                      ("~=", AttrOp::Word), ("=", AttrOp::Eq)] {
        if let Some(p) = s.find(tok) {
            let name = s[..p].trim().to_ascii_lowercase();
            let mut v = s[p + tok.len()..].trim().to_string();
            if (v.starts_with('"') && v.ends_with('"') && v.len() >= 2)
                || (v.starts_with('\'') && v.ends_with('\'') && v.len() >= 2) {
                v = v[1..v.len() - 1].to_string();
            }
            if name.is_empty() { return None }
            return Some(AttrPred { name, op, value: v });
        }
    }
    let n = s.trim().to_ascii_lowercase();
    if n.is_empty() { None } else { Some(AttrPred { name: n, op: AttrOp::Exists, value: String::new() }) }
}

fn classes_of(d: &Dom, h: Handle) -> Vec<String> {
    d.attr(h, "class").map(|c| c.split_whitespace().map(|s| s.to_string()).collect()).unwrap_or_default()
}

fn matches_compound(d: &Dom, h: Handle, c: &Compound) -> bool {
    let Some(tag) = d.tag(h) else { return false };
    if let Some(t) = &c.tag { if !tag.eq_ignore_ascii_case(t) { return false } }
    if let Some(id) = &c.id { if d.attr(h, "id") != Some(id.as_str()) { return false } }
    if !c.classes.is_empty() {
        let have = classes_of(d, h);
        if !c.classes.iter().all(|k| have.iter().any(|x| x == k)) { return false }
    }
    for a in &c.attrs {
        let Some(v) = d.attr(h, &a.name) else { return false };
        let ok = match a.op {
            AttrOp::Exists => true,
            AttrOp::Eq => v == a.value,
            AttrOp::Prefix => v.starts_with(&a.value),
            AttrOp::Suffix => v.ends_with(&a.value),
            AttrOp::Contains => v.contains(&a.value),
            AttrOp::Word => v.split_whitespace().any(|w| w == a.value),
        };
        if !ok { return false }
    }
    for p in &c.pseudos {
        // Unknown pseudos do not match, rather than erroring the whole query:
        // a converter should lose one rule, not the document.
        let sibs: Vec<Handle> = d.get(h).and_then(|n| n.parent)
            .map(|p| d.get(p).map(|n| n.children.clone()).unwrap_or_default())
            .unwrap_or_default();
        let el_sibs: Vec<Handle> = sibs.iter().copied().filter(|&s| d.tag(s).is_some()).collect();
        let ok = match p.as_str() {
            ":first-child" => el_sibs.first() == Some(&h),
            ":last-child" => el_sibs.last() == Some(&h),
            ":only-child" => el_sibs.len() == 1 && el_sibs.first() == Some(&h),
            ":root" => d.get(h).and_then(|n| n.parent) == Some(d.root()),
            ":empty" => d.get(h).map(|n| n.children.is_empty()).unwrap_or(false),
            _ => false,
        };
        if !ok { return false }
    }
    true
}

fn matches_chain(d: &Dom, h: Handle, rest: &[(Comb, Compound)]) -> bool {
    let Some(((comb, comp), tail)) = rest.split_first() else { return true };
    match comb {
        Comb::Child => d.get(h).and_then(|n| n.parent)
            .map(|p| matches_compound(d, p, comp) && matches_chain(d, p, tail))
            .unwrap_or(false),
        Comb::Descendant => {
            let mut cur = d.get(h).and_then(|n| n.parent);
            while let Some(p) = cur {
                if matches_compound(d, p, comp) && matches_chain(d, p, tail) { return true }
                cur = d.get(p).and_then(|n| n.parent);
            }
            false
        }
        Comb::Adjacent | Comb::Sibling => {
            let Some(par) = d.get(h).and_then(|n| n.parent) else { return false };
            let sibs: Vec<Handle> = d.get(par).map(|n| n.children.clone()).unwrap_or_default();
            let els: Vec<Handle> = sibs.into_iter().filter(|&s| d.tag(s).is_some()).collect();
            let Some(idx) = els.iter().position(|&s| s == h) else { return false };
            if *comb == Comb::Adjacent {
                idx > 0 && matches_compound(d, els[idx - 1], comp) && matches_chain(d, els[idx - 1], tail)
            } else {
                els[..idx].iter().rev().any(|&s| matches_compound(d, s, comp) && matches_chain(d, s, tail))
            }
        }
    }
}

pub fn matches(d: &Dom, h: Handle, sel: &SelectorList) -> bool {
    sel.0.iter().any(|c| matches_compound(d, h, &c.key) && matches_chain(d, h, &c.rest))
}

/// All elements in document order under `scope` (exclusive) matching `sel`.
pub fn select(d: &Dom, scope: Handle, sel: &SelectorList) -> Vec<Handle> {
    let mut out = vec![];
    fn go(d: &Dom, h: Handle, sel: &SelectorList, out: &mut Vec<Handle>) {
        for &c in &d.nodes[h as usize].children {
            if matches!(d.nodes[c as usize].kind, Kind::Element(_)) && matches(d, c, sel) {
                out.push(c);
            }
            go(d, c, sel, out);
        }
    }
    go(d, scope, sel, &mut out);
    out
}
