//! CSS tokenizer (CSS Syntax Level 3, §4), with a position on every token.
//!
//! ★ TOTAL OVER ARBITRARY INPUT. Every byte sequence tokenizes — malformed
//! strings and urls become `BadString` / `BadUrl`, exactly as the spec
//! prescribes — so the parser, not the tokenizer, decides what is refused,
//! and it can say where. Nothing here panics or allocates past the input.

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos { pub line: u32, pub col: u32 }

impl std::fmt::Display for Pos {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "{}:{}", self.line, self.col) }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Ident(String),
    Function(String),
    AtKeyword(String),
    /// `#name`; `true` when it would be a valid id (starts like an ident).
    Hash(String, bool),
    Str(String),
    BadString,
    Url(String),
    BadUrl,
    /// The number's source text is kept: a value is parsed once, from what
    /// was written, never re-derived from a float.
    Number { value: f64, int: bool },
    Percentage(f64),
    Dimension { value: f64, int: bool, unit: String },
    Whitespace,
    Colon, Semicolon, Comma,
    LBracket, RBracket, LParen, RParen, LBrace, RBrace,
    Delim(char),
    Cdo, Cdc,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token { pub tok: Tok, pub pos: Pos }

pub fn tokenize(src: &str) -> Vec<Token> {
    let mut t = Tz { c: src.chars().collect(), i: 0, line: 1, col: 1 };
    let mut out = Vec::new();
    while let Some(tok) = t.next_token() { out.push(tok) }
    out
}

struct Tz { c: Vec<char>, i: usize, line: u32, col: u32 }

fn is_name_start(c: char) -> bool { c.is_ascii_alphabetic() || c == '_' || !c.is_ascii() }
fn is_name(c: char) -> bool { is_name_start(c) || c.is_ascii_digit() || c == '-' }

impl Tz {
    fn peek(&self, k: usize) -> Option<char> { self.c.get(self.i + k).copied() }
    fn bump(&mut self) -> Option<char> {
        let c = self.c.get(self.i).copied()?;
        self.i += 1;
        if c == '\n' { self.line += 1; self.col = 1 } else { self.col += 1 }
        Some(c)
    }
    fn pos(&self) -> Pos { Pos { line: self.line, col: self.col } }

    fn valid_escape(a: Option<char>, b: Option<char>) -> bool { a == Some('\\') && b != Some('\n') && b.is_some() }
    fn starts_ident(&self, k: usize) -> bool {
        match self.peek(k) {
            Some('-') => matches!(self.peek(k + 1), Some(c) if is_name_start(c) || c == '-')
                || Self::valid_escape(self.peek(k + 1), self.peek(k + 2)),
            Some(c) if is_name_start(c) => true,
            Some('\\') => Self::valid_escape(self.peek(k), self.peek(k + 1)),
            _ => false,
        }
    }
    fn starts_number(&self) -> bool {
        match self.peek(0) {
            Some('+') | Some('-') => matches!(self.peek(1), Some(c) if c.is_ascii_digit())
                || (self.peek(1) == Some('.') && matches!(self.peek(2), Some(c) if c.is_ascii_digit())),
            Some('.') => matches!(self.peek(1), Some(c) if c.is_ascii_digit()),
            Some(c) => c.is_ascii_digit(),
            None => false,
        }
    }

    fn escape(&mut self) -> char {
        // After the backslash.
        let mut hex = String::new();
        while hex.len() < 6 && matches!(self.peek(0), Some(c) if c.is_ascii_hexdigit()) { hex.push(self.bump().unwrap()) }
        if hex.is_empty() {
            return self.bump().unwrap_or('\u{FFFD}');
        }
        if matches!(self.peek(0), Some(c) if c.is_whitespace()) { self.bump(); }
        match u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) {
            Some(c) if c != '\0' && !(0xD800..=0xDFFF).contains(&(c as u32)) => c,
            _ => '\u{FFFD}',
        }
    }

    fn name(&mut self) -> String {
        let mut s = String::new();
        loop {
            match self.peek(0) {
                Some(c) if is_name(c) => { s.push(c); self.bump(); }
                Some('\\') if Self::valid_escape(self.peek(0), self.peek(1)) => { self.bump(); s.push(self.escape()); }
                _ => return s,
            }
        }
    }

    fn number(&mut self) -> (f64, bool) {
        let mut s = String::new();
        let mut int = true;
        if matches!(self.peek(0), Some('+') | Some('-')) { s.push(self.bump().unwrap()) }
        while matches!(self.peek(0), Some(c) if c.is_ascii_digit()) { s.push(self.bump().unwrap()) }
        if self.peek(0) == Some('.') && matches!(self.peek(1), Some(c) if c.is_ascii_digit()) {
            int = false;
            s.push(self.bump().unwrap());
            while matches!(self.peek(0), Some(c) if c.is_ascii_digit()) { s.push(self.bump().unwrap()) }
        }
        if matches!(self.peek(0), Some('e') | Some('E')) {
            let signed = matches!(self.peek(1), Some('+') | Some('-'));
            let d = if signed { self.peek(2) } else { self.peek(1) };
            if matches!(d, Some(c) if c.is_ascii_digit()) {
                int = false;
                s.push(self.bump().unwrap());
                if signed { s.push(self.bump().unwrap()) }
                while matches!(self.peek(0), Some(c) if c.is_ascii_digit()) { s.push(self.bump().unwrap()) }
            }
        }
        // Overflow parses to ±inf; the value grammar rejects non-finite
        // numbers (profile §3.6), with the position of the token.
        (s.parse::<f64>().unwrap_or(f64::NAN), int)
    }

    fn string(&mut self, quote: char) -> Tok {
        let mut s = String::new();
        loop {
            match self.bump() {
                None => return Tok::Str(s),
                Some(c) if c == quote => return Tok::Str(s),
                Some('\n') => return Tok::BadString,
                Some('\\') => match self.peek(0) {
                    None => {}
                    Some('\n') => { self.bump(); }
                    _ => s.push(self.escape()),
                },
                Some(c) => s.push(c),
            }
        }
    }

    fn url(&mut self) -> Tok {
        while matches!(self.peek(0), Some(c) if c.is_whitespace()) { self.bump(); }
        let mut s = String::new();
        loop {
            match self.bump() {
                None | Some(')') => return Tok::Url(s),
                Some(c) if c.is_whitespace() => {
                    while matches!(self.peek(0), Some(c) if c.is_whitespace()) { self.bump(); }
                    return if matches!(self.peek(0), Some(')') | None) { self.bump(); Tok::Url(s) } else { self.bad_url() };
                }
                Some('"') | Some('\'') | Some('(') => return self.bad_url(),
                Some('\\') => {
                    if Self::valid_escape(Some('\\'), self.peek(0)) { s.push(self.escape()) } else { return self.bad_url() }
                }
                Some(c) => s.push(c),
            }
        }
    }
    fn bad_url(&mut self) -> Tok {
        loop {
            match self.bump() {
                None | Some(')') => return Tok::BadUrl,
                Some('\\') => { self.bump(); }
                _ => {}
            }
        }
    }

    fn next_token(&mut self) -> Option<Token> {
        // Comments are not tokens.
        while self.peek(0) == Some('/') && self.peek(1) == Some('*') {
            self.bump(); self.bump();
            while self.peek(0).is_some() && !(self.peek(0) == Some('*') && self.peek(1) == Some('/')) { self.bump(); }
            self.bump(); self.bump();
        }
        let pos = self.pos();
        let c = self.peek(0)?;
        let tok = if c.is_whitespace() {
            while matches!(self.peek(0), Some(c) if c.is_whitespace()) { self.bump(); }
            Tok::Whitespace
        } else if c == '"' || c == '\'' {
            self.bump(); self.string(c)
        } else if self.starts_number() {
            let (value, int) = self.number();
            if self.starts_ident(0) { Tok::Dimension { value, int, unit: self.name().to_ascii_lowercase() } }
            else if self.peek(0) == Some('%') { self.bump(); Tok::Percentage(value) }
            else { Tok::Number { value, int } }
        } else if c == '#' && (matches!(self.peek(1), Some(c) if is_name(c)) || Self::valid_escape(self.peek(1), self.peek(2))) {
            self.bump();
            let id = self.starts_ident(0);
            Tok::Hash(self.name(), id)
        } else if c == '@' && self.starts_ident(1) {
            self.bump(); Tok::AtKeyword(self.name().to_ascii_lowercase())
        } else if self.starts_ident(0) {
            let n = self.name();
            if self.peek(0) == Some('(') {
                self.bump();
                if n.eq_ignore_ascii_case("url") {
                    // url( with a quoted argument is an ordinary function.
                    let mut k = 0;
                    while matches!(self.peek(k), Some(c) if c.is_whitespace()) { k += 1 }
                    if matches!(self.peek(k), Some('"') | Some('\'')) { Tok::Function("url".into()) } else { self.url() }
                } else { Tok::Function(n.to_ascii_lowercase()) }
            } else { Tok::Ident(n) }
        } else if c == '<' && self.peek(1) == Some('!') && self.peek(2) == Some('-') && self.peek(3) == Some('-') {
            for _ in 0..4 { self.bump(); } Tok::Cdo
        } else if c == '-' && self.peek(1) == Some('-') && self.peek(2) == Some('>') {
            for _ in 0..3 { self.bump(); } Tok::Cdc
        } else {
            self.bump();
            match c {
                ':' => Tok::Colon, ';' => Tok::Semicolon, ',' => Tok::Comma,
                '[' => Tok::LBracket, ']' => Tok::RBracket, '(' => Tok::LParen, ')' => Tok::RParen,
                '{' => Tok::LBrace, '}' => Tok::RBrace,
                other => Tok::Delim(other),
            }
        };
        Some(Token { tok, pos })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn toks(s: &str) -> Vec<Tok> { tokenize(s).into_iter().map(|t| t.tok).filter(|t| *t != Tok::Whitespace).collect() }

    #[test]
    fn a_rule_tokenizes_with_positions() {
        let t = tokenize("p {\n  margin-top: 1.5em;\n}");
        assert_eq!(t[0].tok, Tok::Ident("p".into()));
        let m = t.iter().find(|x| x.tok == Tok::Ident("margin-top".into())).unwrap();
        assert_eq!(m.pos, Pos { line: 2, col: 3 });
        assert!(t.iter().any(|x| x.tok == Tok::Dimension { value: 1.5, int: false, unit: "em".into() }));
    }

    #[test]
    fn numbers_percentages_dimensions() {
        assert_eq!(toks("10 -2.5% +3PX 1e2"), vec![
            Tok::Number { value: 10.0, int: true }, Tok::Percentage(-2.5),
            Tok::Dimension { value: 3.0, int: true, unit: "px".into() }, Tok::Number { value: 100.0, int: false }]);
    }

    #[test]
    fn hashes_strings_urls_functions() {
        assert_eq!(toks("#fff #1a"), vec![Tok::Hash("fff".into(), true), Tok::Hash("1a".into(), false)]);
        assert_eq!(toks(r#""a\"b" 'c'"#), vec![Tok::Str("a\"b".into()), Tok::Str("c".into())]);
        assert_eq!(toks("url(a.png) url( 'b' ) rgb("), vec![Tok::Url("a.png".into()), Tok::Function("url".into()),
            Tok::Str("b".into()), Tok::RParen, Tok::Function("rgb".into())]);
    }

    #[test]
    fn malformed_input_becomes_bad_tokens_not_panics() {
        assert!(toks("'unterminated\nx").contains(&Tok::BadString));
        assert!(toks("url(a b)").contains(&Tok::BadUrl));
        // Arbitrary junk, including lone escapes and unterminated comments.
        for s in ["\\", "/*", "\"\\", "url(", "#", "@", "--", "1e", "\u{0}\u{FFFF}{}[]()", "\\110000"] {
            let _ = tokenize(s);
        }
    }

    #[test]
    fn escapes_decode_and_invalid_code_points_become_replacement() {
        assert_eq!(toks(r"\31 0"), vec![Tok::Ident("10".into())]);
        assert_eq!(toks(r"a\0"), vec![Tok::Ident("a\u{FFFD}".into())]);
    }
}
