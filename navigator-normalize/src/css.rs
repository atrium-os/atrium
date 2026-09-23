//! A LENIENT CSS parser. The profile's own parser (navigator-style) refuses
//! anything it does not admit, which is right for the renderer and useless
//! here: this one accepts whatever the page contains and keeps it as tokens,
//! because the normalizer's job is to understand real CSS well enough to
//! rewrite it (§6 — "it may be as lenient as it likes").

use navigator_style::token::{tokenize, Pos, Tok, Token};

#[derive(Debug, Clone)]
pub struct Decl {
    pub name: String,
    /// The value, as written. Kept as tokens: `var()` is resolved later, and
    /// a shorthand is expanded from these.
    pub value: Vec<Token>,
    pub important: bool,
}

#[derive(Debug, Clone)]
pub struct Rule {
    /// The selector list, as tokens — parsed by `sel`, not here.
    pub selector: Vec<Token>,
    pub decls: Vec<Decl>,
    /// Source order across every sheet, which is what breaks specificity ties.
    pub order: usize,
    /// `Some(query)` when the rule came from an `@media` block, as written.
    pub media: Option<String>,
    pub pos: Pos,
}

#[derive(Debug, Default)]
pub struct Sheet {
    pub rules: Vec<Rule>,
    /// `@font-face` blocks, kept verbatim: the profile admits them.
    pub font_faces: Vec<Vec<Decl>>,
    /// What was dropped, and why — the normalizer reports rather than hides.
    pub dropped: Vec<(&'static str, String)>,
}

fn skip_ws(t: &[Token], mut i: usize) -> usize {
    while i < t.len() && t[i].tok == Tok::Whitespace { i += 1 }
    i
}

/// Consume a `{…}` block, returning its inner tokens and the index after it.
fn block(t: &[Token], mut i: usize) -> (Vec<Token>, usize) {
    let mut depth = 0usize;
    let mut out = vec![];
    while i < t.len() {
        match &t[i].tok {
            Tok::LBrace => { depth += 1; if depth > 1 { out.push(t[i].clone()) } }
            Tok::RBrace => {
                depth -= 1;
                if depth == 0 { return (out, i + 1) }
                out.push(t[i].clone());
            }
            _ => if depth > 0 { out.push(t[i].clone()) },
        }
        i += 1;
    }
    (out, i)
}

/// Declarations from a block's inner tokens. Unterminated or malformed ones
/// are skipped, never guessed at.
pub fn declarations(t: &[Token]) -> Vec<Decl> {
    let mut out = vec![];
    let mut i = 0;
    while i < t.len() {
        i = skip_ws(t, i);
        if i >= t.len() { break }
        // name
        let name = match &t[i].tok {
            Tok::Ident(n) => n.to_ascii_lowercase(),
            Tok::Semicolon => { i += 1; continue }
            _ => { // skip to the next semicolon at depth 0
                let mut d = 0usize;
                while i < t.len() { match &t[i].tok {
                    Tok::LParen | Tok::Function(_) => d += 1,
                    Tok::RParen => d = d.saturating_sub(1),
                    Tok::Semicolon if d == 0 => break,
                    _ => {} } i += 1 }
                i += 1; continue
            }
        };
        i = skip_ws(t, i + 1);
        if i >= t.len() || t[i].tok != Tok::Colon { // not a declaration
            while i < t.len() && t[i].tok != Tok::Semicolon { i += 1 }
            i += 1; continue
        }
        i += 1;
        let mut value = vec![];
        let mut d = 0usize;
        while i < t.len() {
            match &t[i].tok {
                Tok::LParen | Tok::Function(_) => { d += 1; value.push(t[i].clone()) }
                Tok::RParen => { d = d.saturating_sub(1); value.push(t[i].clone()) }
                Tok::Semicolon if d == 0 => { i += 1; break }
                _ => value.push(t[i].clone()),
            }
            i += 1;
        }
        // `!important`, as the last two significant tokens.
        let mut important = false;
        let mut sig: Vec<usize> = (0..value.len()).filter(|k| value[*k].tok != Tok::Whitespace).collect();
        if sig.len() >= 2 {
            let (a, b) = (sig[sig.len() - 2], sig[sig.len() - 1]);
            if value[a].tok == Tok::Delim('!') {
                if let Tok::Ident(w) = &value[b].tok {
                    if w.eq_ignore_ascii_case("important") {
                        important = true;
                        value.truncate(a);
                        sig.clear();
                    }
                }
            }
        }
        while value.last().map(|t| t.tok == Tok::Whitespace).unwrap_or(false) { value.pop(); }
        if !value.is_empty() { out.push(Decl { name, value, important }) }
    }
    out
}

/// Parse a whole stylesheet leniently. `order` counts rules across every
/// sheet the document has, in the order the document names them.
pub fn parse(src: &str, order: &mut usize, sheet: &mut Sheet) {
    let t = tokenize(src);
    parse_rules(&t, order, sheet, None);
}

/// ★ A `<link media="...">` conditions the WHOLE sheet, exactly as if its
/// rules were wrapped in that `@media`. Ignoring it applies a dark-mode or
/// print sheet unconditionally — which is how a spec page came out black.
pub fn parse_in_media(src: &str, media: &str, order: &mut usize, sheet: &mut Sheet) {
    let t = tokenize(src);
    let m = media.trim();
    parse_rules(&t, order, sheet, (!m.is_empty() && m != "all" && m != "screen").then_some(m));
}

fn parse_rules(t: &[Token], order: &mut usize, sheet: &mut Sheet, media: Option<&str>) {
    let mut i = 0;
    while i < t.len() {
        i = skip_ws(t, i);
        if i >= t.len() { break }
        match &t[i].tok {
            Tok::AtKeyword(name) => {
                let name = name.to_ascii_lowercase();
                let pos = t[i].pos;
                // the prelude, up to `{` or `;`
                let start = i + 1;
                let mut j = start;
                while j < t.len() && !matches!(t[j].tok, Tok::LBrace | Tok::Semicolon) { j += 1 }
                let prelude = write_tokens(&t[start..j]);
                if j < t.len() && t[j].tok == Tok::Semicolon {
                    sheet.dropped.push(("at-rule", format!("@{name} {prelude}")));
                    i = j + 1;
                    continue;
                }
                let (inner, next) = block(t, j);
                match name.as_str() {
                    // Nested rules, conditioned on the query.
                    // A nested `@media` inside a conditioned sheet keeps the
                    // outer condition too: both must hold.
                    "media" => {
                        let inner_q = prelude.trim();
                        let combined = match media { Some(outer) => format!("{outer} and {inner_q}"), None => inner_q.to_string() };
                        parse_rules(&inner, order, sheet, Some(&combined))
                    }
                    // @supports: take the branch, since the profile decides
                    // what is supported, not the page.
                    "supports" => parse_rules(&inner, order, sheet, media),
                    "font-face" => sheet.font_faces.push(declarations(&inner)),
                    _ => sheet.dropped.push(("at-rule", format!("@{name} {}", prelude.trim()))),
                }
                let _ = pos;
                i = next;
            }
            _ => {
                let start = i;
                let mut j = i;
                let mut d = 0usize;
                while j < t.len() {
                    match &t[j].tok {
                        Tok::LBracket | Tok::LParen | Tok::Function(_) => d += 1,
                        Tok::RBracket | Tok::RParen => d = d.saturating_sub(1),
                        Tok::LBrace if d == 0 => break,
                        _ => {}
                    }
                    j += 1;
                }
                if j >= t.len() { break }
                let selector: Vec<Token> = t[start..j].to_vec();
                let (inner, next) = block(t, j);
                let decls = declarations(&inner);
                if !decls.is_empty() {
                    sheet.rules.push(Rule { selector, decls, order: *order, media: media.map(str::to_string), pos: t[start].pos });
                    *order += 1;
                }
                i = next;
            }
        }
    }
}

/// Tokens back to source text — used for a media query's own words and for
/// any value the normalizer passes through unchanged.
pub fn write_tokens(t: &[Token]) -> String {
    let mut s = String::new();
    for tk in t {
        match &tk.tok {
            Tok::Ident(v) => s.push_str(v),
            Tok::Function(v) => { s.push_str(v); s.push('(') }
            Tok::AtKeyword(v) => { s.push('@'); s.push_str(v) }
            Tok::Hash(v, _) => { s.push('#'); s.push_str(v) }
            Tok::Str(v) => { s.push('"'); for c in v.chars() { if c == '"' || c == '\\' { s.push('\\') } s.push(c) } s.push('"') }
            Tok::Url(v) => { s.push_str("url("); s.push_str(v); s.push(')') }
            Tok::Number { value, int } => { if *int { s.push_str(&format!("{}", *value as i64)) } else { s.push_str(&fmt_f(*value)) } }
            Tok::Percentage(v) => { s.push_str(&fmt_f(*v)); s.push('%') }
            Tok::Dimension { value, int, unit } => {
                if *int { s.push_str(&format!("{}", *value as i64)) } else { s.push_str(&fmt_f(*value)) }
                s.push_str(unit);
            }
            Tok::Whitespace => s.push(' '),
            Tok::Colon => s.push(':'),
            Tok::Semicolon => s.push(';'),
            Tok::Comma => s.push(','),
            Tok::LBracket => s.push('['), Tok::RBracket => s.push(']'),
            Tok::LParen => s.push('('), Tok::RParen => s.push(')'),
            Tok::LBrace => s.push('{'), Tok::RBrace => s.push('}'),
            Tok::Delim(c) => s.push(*c),
            Tok::BadString | Tok::BadUrl | Tok::Cdo | Tok::Cdc => {}
        }
    }
    s
}

fn fmt_f(v: f64) -> String {
    let s = format!("{v}");
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
}
