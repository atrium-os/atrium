//! A GENERAL CSS selector matcher — descendant, sibling, `:not()`, `:has()`,
//! `:nth-child()`, pseudo-elements, the lot.
//!
//! ★ This is the whole trick of §6. The profile admits almost no selector
//! structure, and the corpus is full of it (692 descendant combinators in 29
//! documents). Rather than rewrite each unadmitted form into an admitted one
//! — which is impossible in general — the normalizer MATCHES the selector
//! itself and emits a flat class. Every unadmitted selector shape therefore
//! costs nothing extra: it is one matcher, not eleven rewrites.

use navigator_dom::{Dom, Handle, Kind};
use navigator_style::token::{Tok, Token};

#[derive(Debug, Clone, PartialEq)]
pub enum Simple {
    Type(String),
    Universal,
    Class(String),
    Id(String),
    Attr { name: String, op: Op, value: String, ci: bool },
    /// A state the renderer resolves at paint time (`:hover`), kept so the
    /// emitted rule can carry it.
    State(String),
    Not(Vec<Complex>),
    Is(Vec<Complex>),
    Has(Vec<Complex>),
    /// `:nth-child(an+b)`, and the simple positional forms as (a, b).
    NthChild(i64, i64),
    NthLastChild(i64, i64),
    FirstOfType,
    LastOfType,
    OnlyChild,
    Root,
    Empty,
    /// A pseudo-element: the rule cannot be represented (the profile has no
    /// generated content), so a selector carrying one is dropped.
    PseudoElement(String),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Op { Exists, Eq, Word, Dash, Prefix, Suffix, Contains }

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Comb { Descendant, Child, Next, Later }

/// Compounds, with the combinator that joins each to the previous one.
#[derive(Debug, Clone, PartialEq)]
pub struct Complex(pub Vec<(Comb, Vec<Simple>)>);

type R<T> = Result<T, String>;

pub fn parse_list(t: &[Token]) -> R<Vec<Complex>> {
    let mut out = vec![];
    for part in split_top(t, Tok::Comma) {
        let c = parse_complex(&part)?;
        if !c.0.is_empty() { out.push(c) }
    }
    if out.is_empty() { return Err("empty selector".into()) }
    Ok(out)
}

/// Split on a token at nesting depth 0.
fn split_top(t: &[Token], sep: Tok) -> Vec<Vec<Token>> {
    let (mut out, mut cur, mut d) = (vec![], vec![], 0usize);
    for tk in t {
        match &tk.tok {
            Tok::LParen | Tok::Function(_) | Tok::LBracket => { d += 1; cur.push(tk.clone()) }
            Tok::RParen | Tok::RBracket => { d = d.saturating_sub(1); cur.push(tk.clone()) }
            x if *x == sep && d == 0 => { out.push(std::mem::take(&mut cur)) }
            _ => cur.push(tk.clone()),
        }
    }
    out.push(cur);
    out
}

fn parse_complex(t: &[Token]) -> R<Complex> {
    let mut parts: Vec<(Comb, Vec<Simple>)> = vec![];
    let mut comb = Comb::Descendant;
    let mut i = 0;
    let mut pending_ws = false;
    while i < t.len() {
        match &t[i].tok {
            Tok::Whitespace => { pending_ws = !parts.is_empty(); i += 1 }
            Tok::Delim('>') => { comb = Comb::Child; pending_ws = false; i += 1 }
            Tok::Delim('+') => { comb = Comb::Next; pending_ws = false; i += 1 }
            Tok::Delim('~') => { comb = Comb::Later; pending_ws = false; i += 1 }
            _ => {
                if pending_ws && comb == Comb::Descendant { /* descendant */ }
                let (compound, next) = parse_compound(t, i)?;
                if compound.is_empty() { return Err(format!("cannot parse selector near {}", crate::css::write_tokens(&t[i..]))) }
                parts.push((if parts.is_empty() { Comb::Descendant } else { comb }, compound));
                comb = Comb::Descendant;
                pending_ws = false;
                i = next;
            }
        }
    }
    Ok(Complex(parts))
}

fn parse_compound(t: &[Token], mut i: usize) -> R<(Vec<Simple>, usize)> {
    let mut out = vec![];
    while i < t.len() {
        match &t[i].tok {
            Tok::Ident(n) => { out.push(Simple::Type(n.to_ascii_lowercase())); i += 1 }
            Tok::Delim('*') => { out.push(Simple::Universal); i += 1 }
            Tok::Delim('.') => {
                i += 1;
                match t.get(i).map(|x| &x.tok) {
                    Some(Tok::Ident(n)) => { out.push(Simple::Class(n.clone())); i += 1 }
                    _ => return Err("`.` without a class name".into()),
                }
            }
            Tok::Hash(n, _) => { out.push(Simple::Id(n.clone())); i += 1 }
            Tok::LBracket => {
                let mut j = i + 1;
                let mut inner = vec![];
                while j < t.len() && t[j].tok != Tok::RBracket { inner.push(t[j].clone()); j += 1 }
                out.push(parse_attr(&inner)?);
                i = j + 1;
            }
            Tok::Colon => {
                let double = matches!(t.get(i + 1).map(|x| &x.tok), Some(Tok::Colon));
                let k = if double { i + 2 } else { i + 1 };
                match t.get(k).map(|x| x.tok.clone()) {
                    Some(Tok::Ident(n)) => {
                        let n = n.to_ascii_lowercase();
                        if double || matches!(n.as_str(), "before" | "after" | "first-line" | "first-letter" | "marker" | "placeholder" | "selection" | "backdrop") {
                            out.push(Simple::PseudoElement(n));
                        } else {
                            out.push(match n.as_str() {
                                "root" => Simple::Root,
                                "first-child" => Simple::NthChild(0, 1),
                                "last-child" => Simple::NthLastChild(0, 1),
                                "only-child" => Simple::OnlyChild,
                                "first-of-type" => Simple::FirstOfType,
                                "last-of-type" => Simple::LastOfType,
                                "empty" => Simple::Empty,
                                _ => Simple::State(n),
                            });
                        }
                        i = k + 1;
                    }
                    Some(Tok::Function(f)) => {
                        let f = f.to_ascii_lowercase();
                        let mut j = k + 1;
                        let mut d = 1usize;
                        let mut inner = vec![];
                        while j < t.len() && d > 0 {
                            match &t[j].tok {
                                Tok::LParen | Tok::Function(_) => { d += 1; inner.push(t[j].clone()) }
                                Tok::RParen => { d -= 1; if d > 0 { inner.push(t[j].clone()) } }
                                _ => inner.push(t[j].clone()),
                            }
                            j += 1;
                        }
                        out.push(match f.as_str() {
                            "not" => Simple::Not(parse_list(&inner)?),
                            "is" | "matches" | "any" => Simple::Is(parse_list(&inner)?),
                            "where" => Simple::Is(parse_list(&inner)?),
                            "has" => Simple::Has(parse_list(&inner)?),
                            "nth-child" => { let (a, b) = parse_nth(&inner)?; Simple::NthChild(a, b) }
                            "nth-last-child" => { let (a, b) = parse_nth(&inner)?; Simple::NthLastChild(a, b) }
                            "nth-of-type" => { let (a, b) = parse_nth(&inner)?; Simple::NthChild(a, b) }
                            other => return Err(format!(":{other}() is not understood")),
                        });
                        i = j;
                    }
                    _ => return Err("`:` without a name".into()),
                }
            }
            _ => break,
        }
    }
    Ok((out, i))
}

fn parse_attr(t: &[Token]) -> R<Simple> {
    let sig: Vec<&Token> = t.iter().filter(|x| x.tok != Tok::Whitespace).collect();
    let name = match sig.first().map(|x| &x.tok) {
        Some(Tok::Ident(n)) => n.to_ascii_lowercase(),
        _ => return Err("attribute selector without a name".into()),
    };
    if sig.len() == 1 { return Ok(Simple::Attr { name, op: Op::Exists, value: String::new(), ci: false }) }
    let (op, vi) = match (&sig[1].tok, sig.get(2).map(|x| &x.tok)) {
        (Tok::Delim('='), _) => (Op::Eq, 2),
        (Tok::Delim('~'), Some(Tok::Delim('='))) => (Op::Word, 3),
        (Tok::Delim('|'), Some(Tok::Delim('='))) => (Op::Dash, 3),
        (Tok::Delim('^'), Some(Tok::Delim('='))) => (Op::Prefix, 3),
        (Tok::Delim('$'), Some(Tok::Delim('='))) => (Op::Suffix, 3),
        (Tok::Delim('*'), Some(Tok::Delim('='))) => (Op::Contains, 3),
        _ => return Err("attribute operator not understood".into()),
    };
    let value = match sig.get(vi).map(|x| &x.tok) {
        Some(Tok::Str(v)) | Some(Tok::Ident(v)) => v.clone(),
        _ => return Err("attribute selector without a value".into()),
    };
    let ci = matches!(sig.get(vi + 1).map(|x| &x.tok), Some(Tok::Ident(f)) if f.eq_ignore_ascii_case("i"));
    Ok(Simple::Attr { name, op, value, ci })
}

/// `an+b`, `odd`, `even`, or a bare integer.
fn parse_nth(t: &[Token]) -> R<(i64, i64)> {
    let s = crate::css::write_tokens(t).trim().to_ascii_lowercase();
    match s.as_str() {
        "odd" => return Ok((2, 1)),
        "even" => return Ok((2, 0)),
        _ => {}
    }
    let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if let Ok(b) = s.parse::<i64>() { return Ok((0, b)) }
    let (a_part, b_part) = match s.split_once('n') { Some(x) => x, None => return Err(format!("nth `{s}`")) };
    let a = match a_part { "" | "+" => 1, "-" => -1, v => v.parse::<i64>().map_err(|e| e.to_string())? };
    let b = if b_part.is_empty() { 0 } else { b_part.parse::<i64>().map_err(|e| e.to_string())? };
    Ok((a, b))
}

/// (ids, classes+attrs+pseudo-classes, types) — CSS specificity.
pub fn specificity(c: &Complex) -> (u32, u32, u32) {
    let mut s = (0, 0, 0);
    for (_, compound) in &c.0 {
        for simple in compound { add_spec(simple, &mut s) }
    }
    s
}

fn add_spec(s: &Simple, out: &mut (u32, u32, u32)) {
    match s {
        Simple::Id(_) => out.0 += 1,
        Simple::Class(_) | Simple::Attr { .. } | Simple::State(_) | Simple::NthChild(..)
        | Simple::NthLastChild(..) | Simple::FirstOfType | Simple::LastOfType
        | Simple::OnlyChild | Simple::Root | Simple::Empty => out.1 += 1,
        Simple::Type(_) => out.2 += 1,
        Simple::Universal | Simple::PseudoElement(_) => {}
        // :not() and :is() take the specificity of their most specific branch.
        Simple::Not(l) | Simple::Is(l) | Simple::Has(l) => {
            if let Some(m) = l.iter().map(specificity).max() {
                out.0 += m.0; out.1 += m.1; out.2 += m.2;
            }
        }
    }
}

/// States the DOM can decide by itself. `:checked` is an attribute, not a
/// mood; so are `:disabled` and `:link`. Only the genuinely dynamic ones
/// have to travel into the emitted rule.
pub fn is_static_state(n: &str) -> bool {
    matches!(n, "checked" | "disabled" | "enabled" | "link" | "visited" | "read-only" | "read-write" | "required" | "optional" | "default" | "indeterminate")
}

/// The dynamic states on the SUBJECT compound — the element the rule
/// actually styles.
///
/// ★ A dynamic state on any OTHER compound cannot be represented: in
/// `#toggle:checked ~ .page-wrapper` the state belongs to the checkbox and
/// the declarations belong to its sibling, so emitting `.nN:hover` on the
/// sibling names a mood the sibling never has. `subject_states` returns the
/// representable ones and `misplaced_state` names the rest.
pub fn subject_states(c: &Complex) -> Vec<String> {
    let mut v = vec![];
    if let Some((_, compound)) = c.0.last() {
        for s in compound {
            if let Simple::State(n) = s { if !is_static_state(n) && !v.contains(n) { v.push(n.clone()) } }
        }
    }
    v
}

pub fn misplaced_state(c: &Complex) -> Option<String> {
    for (i, (_, compound)) in c.0.iter().enumerate() {
        if i + 1 == c.0.len() { continue }
        for s in compound {
            if let Simple::State(n) = s { if !is_static_state(n) { return Some(n.clone()) } }
        }
    }
    None
}

pub fn has_pseudo_element(c: &Complex) -> bool {
    c.0.iter().any(|(_, cp)| cp.iter().any(|s| matches!(s, Simple::PseudoElement(_))))
}

pub struct Matcher<'a> { pub dom: &'a Dom }

impl<'a> Matcher<'a> {
    pub fn matches(&self, h: Handle, c: &Complex) -> bool {
        if c.0.is_empty() { return false }
        self.match_from(h, c, c.0.len() - 1)
    }

    fn match_from(&self, h: Handle, c: &Complex, i: usize) -> bool {
        if !self.match_compound(h, &c.0[i].1) { return false }
        if i == 0 { return true }
        let comb = c.0[i].0;
        match comb {
            Comb::Child => self.parent_el(h).is_some_and(|p| self.match_from(p, c, i - 1)),
            Comb::Descendant => {
                let mut p = self.parent_el(h);
                while let Some(x) = p {
                    if self.match_from(x, c, i - 1) { return true }
                    p = self.parent_el(x);
                }
                false
            }
            Comb::Next => self.prev_el(h).is_some_and(|s| self.match_from(s, c, i - 1)),
            Comb::Later => {
                let mut s = self.prev_el(h);
                while let Some(x) = s {
                    if self.match_from(x, c, i - 1) { return true }
                    s = self.prev_el(x);
                }
                false
            }
        }
    }

    fn parent_el(&self, h: Handle) -> Option<Handle> {
        let p = self.dom.get(h)?.parent?;
        matches!(self.dom.get(p).map(|n| &n.kind), Some(Kind::Element(_))).then_some(p)
    }

    fn prev_el(&self, h: Handle) -> Option<Handle> {
        let p = self.dom.get(h)?.parent?;
        let kids = self.dom.element_children(p);
        let idx = kids.iter().position(|k| *k == h)?;
        if idx == 0 { None } else { Some(kids[idx - 1]) }
    }

    fn match_compound(&self, h: Handle, compound: &[Simple]) -> bool {
        compound.iter().all(|s| self.match_simple(h, s))
    }

    fn match_simple(&self, h: Handle, s: &Simple) -> bool {
        let tag = self.dom.tag(h).unwrap_or("");
        match s {
            Simple::Universal => true,
            Simple::Type(t) => tag.eq_ignore_ascii_case(t),
            Simple::Class(c) => self.dom.attr(h, "class").is_some_and(|v| v.split_ascii_whitespace().any(|x| x == c)),
            Simple::Id(i) => self.dom.attr(h, "id") == Some(i.as_str()),
            Simple::Attr { name, op, value, ci } => {
                let Some(v) = self.dom.attr(h, name) else { return false };
                let (v, value) = if *ci { (v.to_ascii_lowercase(), value.to_ascii_lowercase()) } else { (v.to_string(), value.clone()) };
                match op {
                    Op::Exists => true,
                    Op::Eq => v == value,
                    Op::Word => v.split_ascii_whitespace().any(|x| x == value),
                    Op::Dash => v == value || v.starts_with(&format!("{value}-")),
                    Op::Prefix => !value.is_empty() && v.starts_with(&value),
                    Op::Suffix => !value.is_empty() && v.ends_with(&value),
                    Op::Contains => !value.is_empty() && v.contains(&value),
                }
            }
            // ★ A DYNAMIC state cannot be decided statically, and pretending
            // it is false would silently drop every :hover rule: the element
            // matches here and the state travels with the rule. A STATIC one
            // is just an attribute, and the DOM answers it now — which is how
            // `#toggle:checked ~ .page-wrapper` gets its margin.
            Simple::State(n) => match n.as_str() {
                "checked" => self.dom.attr(h, "checked").is_some() || self.dom.attr(h, "selected").is_some(),
                "disabled" => self.dom.attr(h, "disabled").is_some(),
                "enabled" => self.dom.attr(h, "disabled").is_none(),
                "required" => self.dom.attr(h, "required").is_some(),
                "optional" => self.dom.attr(h, "required").is_none(),
                "read-only" => self.dom.attr(h, "readonly").is_some(),
                "read-write" => self.dom.attr(h, "readonly").is_none(),
                "link" => tag.eq_ignore_ascii_case("a") && self.dom.attr(h, "href").is_some(),
                // Never visited: a document has no history, and claiming one
                // would leak what the reader has read.
                "visited" => false,
                _ => true,
            },
            Simple::PseudoElement(_) => false,
            Simple::Not(l) => !l.iter().any(|c| self.matches(h, c)),
            Simple::Is(l) => l.iter().any(|c| self.matches(h, c)),
            Simple::Has(l) => self.descendants(h).into_iter().any(|d| l.iter().any(|c| self.matches(d, c))),
            Simple::Root => self.dom.get(h).and_then(|n| n.parent).is_some_and(|p| p == self.dom.root()),
            Simple::Empty => self.dom.children_of(h).is_empty(),
            Simple::OnlyChild => self.siblings(h).len() == 1,
            Simple::FirstOfType => self.same_type(h).first() == Some(&h),
            Simple::LastOfType => self.same_type(h).last() == Some(&h),
            Simple::NthChild(a, b) => nth_ok(self.index_of(h), *a, *b),
            Simple::NthLastChild(a, b) => {
                let sibs = self.siblings(h);
                let i = sibs.iter().position(|x| *x == h).unwrap_or(0);
                nth_ok((sibs.len() - i) as i64, *a, *b)
            }
        }
    }

    fn siblings(&self, h: Handle) -> Vec<Handle> {
        match self.dom.get(h).and_then(|n| n.parent) {
            Some(p) => self.dom.element_children(p),
            None => vec![h],
        }
    }

    fn same_type(&self, h: Handle) -> Vec<Handle> {
        let tag = self.dom.tag(h).unwrap_or("").to_string();
        self.siblings(h).into_iter().filter(|x| self.dom.tag(*x) == Some(tag.as_str())).collect()
    }

    fn index_of(&self, h: Handle) -> i64 {
        let sibs = self.siblings(h);
        sibs.iter().position(|x| *x == h).map(|i| i as i64 + 1).unwrap_or(1)
    }

    fn descendants(&self, h: Handle) -> Vec<Handle> {
        let mut out = vec![];
        let mut stack = self.dom.element_children(h);
        while let Some(x) = stack.pop() {
            stack.extend(self.dom.element_children(x));
            out.push(x);
        }
        out
    }
}

/// `i` is 1-based; `an+b` matches when (i - b) / a is a non-negative integer.
fn nth_ok(i: i64, a: i64, b: i64) -> bool {
    if a == 0 { return i == b }
    let d = i - b;
    d % a == 0 && d / a >= 0
}
