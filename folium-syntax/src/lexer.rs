//! Tokens (ISO 32000-2 §7.2) and objects parsed from a byte window
//! (§7.3). Lenient where real files are sloppy, strict about bounds: every
//! token is cut to the window, nesting to [`DEPTH_MAX`], and running off
//! the window's end is [`Error::Truncated`] so the caller can read more.

use std::sync::Arc;

use crate::limits::DEPTH_MAX;
use crate::object::{Dict, Name, Object, Ref};
use crate::{Error, Result};

pub fn is_white(b: u8) -> bool { matches!(b, 0 | 9 | 10 | 12 | 13 | 32) }
pub fn is_delim(b: u8) -> bool { matches!(b, b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%') }
pub fn is_regular(b: u8) -> bool { !is_white(b) && !is_delim(b) }

#[derive(Debug, Clone, PartialEq)]
pub enum Token<'a> {
    Int(i64),
    Real(f64),
    Name(Name),
    String(Vec<u8>),
    ArrayOpen,
    ArrayClose,
    DictOpen,
    DictClose,
    BraceOpen,
    BraceClose,
    Keyword(&'a [u8]),
}

pub struct Lexer<'a> {
    pub buf: &'a [u8],
    pub pos: usize,
    /// Where `buf` starts in the file, for error offsets.
    pub base: u64,
}

impl<'a> Lexer<'a> {
    pub fn new(buf: &'a [u8], base: u64) -> Self { Lexer { buf, pos: 0, base } }

    fn err(&self, what: &'static str) -> Error { Error::Syntax { offset: self.base + self.pos as u64, what } }

    pub fn skip_white(&mut self) {
        while self.pos < self.buf.len() {
            let b = self.buf[self.pos];
            if is_white(b) {
                self.pos += 1;
            } else if b == b'%' {
                while self.pos < self.buf.len() && !matches!(self.buf[self.pos], b'\r' | b'\n') {
                    self.pos += 1;
                }
            } else {
                break;
            }
        }
    }

    pub fn at_end(&mut self) -> bool {
        self.skip_white();
        self.pos >= self.buf.len()
    }

    /// The next token, or `None` at the end of the window.
    pub fn next(&mut self) -> Result<Option<Token<'a>>> {
        self.skip_white();
        let Some(&b) = self.buf.get(self.pos) else { return Ok(None) };
        let t = match b {
            b'[' => { self.pos += 1; Token::ArrayOpen }
            b']' => { self.pos += 1; Token::ArrayClose }
            b'{' => { self.pos += 1; Token::BraceOpen }
            b'}' => { self.pos += 1; Token::BraceClose }
            b'<' => {
                if self.buf.get(self.pos + 1) == Some(&b'<') {
                    self.pos += 2;
                    Token::DictOpen
                } else {
                    Token::String(self.hex_string()?)
                }
            }
            b'>' => {
                if self.buf.get(self.pos + 1) == Some(&b'>') {
                    self.pos += 2;
                    Token::DictClose
                } else if self.pos + 1 >= self.buf.len() {
                    return Err(Error::Truncated);
                } else {
                    // A stray '>': skip it, as readers do.
                    self.pos += 1;
                    return self.next();
                }
            }
            b'(' => Token::String(self.literal_string()?),
            b')' => {
                self.pos += 1;
                return self.next();
            }
            b'/' => Token::Name(self.name()),
            b'+' | b'-' | b'.' | b'0'..=b'9' => self.number(),
            _ => {
                let s = self.pos;
                while self.pos < self.buf.len() && is_regular(self.buf[self.pos]) {
                    self.pos += 1;
                }
                Token::Keyword(&self.buf[s..self.pos])
            }
        };
        Ok(Some(t))
    }

    fn number(&mut self) -> Token<'a> {
        let s = self.pos;
        while self.pos < self.buf.len() && matches!(self.buf[self.pos], b'+' | b'-' | b'.' | b'0'..=b'9') {
            self.pos += 1;
        }
        let raw = &self.buf[s..self.pos];
        // Sign characters inside the number ("-0-1", "--5") are garbage that
        // other readers drop: keep the first sign, ignore the rest.
        let neg = raw.first() == Some(&b'-');
        let body: Vec<u8> = raw.iter().copied().filter(|c| *c != b'+' && *c != b'-').collect();
        let txt = std::str::from_utf8(&body).unwrap_or("0");
        if !txt.contains('.') {
            if let Ok(v) = txt.parse::<i64>() {
                return Token::Int(if neg { -v } else { v });
            }
            if txt.is_empty() {
                return Token::Int(0);
            }
        }
        let mut seen = false;
        let t: String = txt.chars().filter(|c| if *c == '.' { !std::mem::replace(&mut seen, true) } else { true }).collect();
        let v = if t == "." || t.is_empty() { 0.0 } else { t.parse::<f64>().unwrap_or(0.0) };
        Token::Real(if neg { -v } else { v })
    }

    fn name(&mut self) -> Name {
        self.pos += 1;
        let mut out = Vec::new();
        while self.pos < self.buf.len() && is_regular(self.buf[self.pos]) {
            let b = self.buf[self.pos];
            if b == b'#' {
                let h = |c: u8| (c as char).to_digit(16);
                if let (Some(a), Some(c)) = (self.buf.get(self.pos + 1).and_then(|c| h(*c)), self.buf.get(self.pos + 2).and_then(|c| h(*c))) {
                    out.push((a * 16 + c) as u8);
                    self.pos += 3;
                    continue;
                }
            }
            out.push(b);
            self.pos += 1;
        }
        Name(Arc::from(out))
    }

    fn hex_string(&mut self) -> Result<Vec<u8>> {
        self.pos += 1;
        let mut out = Vec::new();
        let mut hi: Option<u8> = None;
        loop {
            let Some(&b) = self.buf.get(self.pos) else { return Err(Error::Truncated) };
            self.pos += 1;
            if b == b'>' {
                break;
            }
            let Some(d) = (b as char).to_digit(16) else { continue };
            match hi.take() {
                None => hi = Some(d as u8),
                Some(h) => out.push(h << 4 | d as u8),
            }
        }
        if let Some(h) = hi {
            out.push(h << 4);
        }
        Ok(out)
    }

    fn literal_string(&mut self) -> Result<Vec<u8>> {
        self.pos += 1;
        let mut out = Vec::new();
        let mut depth = 1usize;
        loop {
            let Some(&b) = self.buf.get(self.pos) else { return Err(Error::Truncated) };
            self.pos += 1;
            match b {
                b'(' => { depth += 1; out.push(b); }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    out.push(b);
                }
                b'\r' => {
                    if self.buf.get(self.pos) == Some(&b'\n') {
                        self.pos += 1;
                    }
                    out.push(b'\n');
                }
                b'\\' => {
                    let Some(&e) = self.buf.get(self.pos) else { return Err(Error::Truncated) };
                    self.pos += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'\r' => {
                            if self.buf.get(self.pos) == Some(&b'\n') {
                                self.pos += 1;
                            }
                        }
                        b'\n' => {}
                        b'0'..=b'7' => {
                            let mut v = (e - b'0') as u32;
                            for _ in 0..2 {
                                match self.buf.get(self.pos) {
                                    Some(&d @ b'0'..=b'7') => { v = v * 8 + (d - b'0') as u32; self.pos += 1; }
                                    _ => break,
                                }
                            }
                            out.push(v as u8);
                        }
                        other => out.push(other),
                    }
                }
                _ => out.push(b),
            }
        }
        Ok(out)
    }

    /// One object, references (`n g R`) included.
    pub fn object(&mut self) -> Result<Object> {
        match self.next()? {
            None => Err(Error::Truncated),
            Some(t) => self.object_from(t, 0),
        }
    }

    pub fn object_from(&mut self, t: Token<'a>, depth: usize) -> Result<Object> {
        if depth > DEPTH_MAX {
            return Err(Error::Limit("nesting of arrays and dictionaries"));
        }
        Ok(match t {
            Token::Int(a) => {
                let save = self.pos;
                if let (Ok(Some(Token::Int(g))), true) = (self.next(), true) {
                    if let Ok(Some(Token::Keyword(b"R"))) = self.next() {
                        if (0..=u32::MAX as i64).contains(&a) && (0..=u16::MAX as i64).contains(&g) {
                            return Ok(Object::Ref(Ref::new(a as u32, g as u16)));
                        }
                        return Ok(Object::Null);
                    }
                }
                self.pos = save;
                Object::Int(a)
            }
            Token::Real(r) => Object::Real(r),
            Token::Name(n) => Object::Name(n),
            Token::String(s) => Object::String(Arc::from(s)),
            Token::ArrayOpen => {
                let mut v = Vec::new();
                loop {
                    match self.next()? {
                        None => return Err(Error::Truncated),
                        Some(Token::ArrayClose) => break,
                        Some(Token::DictClose) => return Err(self.err("'>>' inside an array")),
                        Some(Token::Keyword(k)) if k == b"endobj" || k == b"stream" => {
                            return Err(self.err("an array not closed"));
                        }
                        Some(t) => v.push(self.object_from(t, depth + 1)?),
                    }
                }
                Object::Array(v)
            }
            Token::DictOpen => Object::Dict(self.dict_body(depth)?),
            Token::Keyword(b"true") => Object::Bool(true),
            Token::Keyword(b"false") => Object::Bool(false),
            Token::Keyword(b"null") => Object::Null,
            Token::Keyword(_) => return Err(self.err("a keyword where an object should be")),
            Token::ArrayClose | Token::DictClose | Token::BraceOpen | Token::BraceClose => {
                return Err(self.err("a closing bracket where an object should be"));
            }
        })
    }

    /// The rest of a dictionary after `<<`.
    pub fn dict_body(&mut self, depth: usize) -> Result<Dict> {
        let mut d = Dict::new();
        loop {
            match self.next()? {
                None => return Err(Error::Truncated),
                Some(Token::DictClose) => break,
                Some(Token::Name(k)) => {
                    let save = self.pos;
                    match self.next()? {
                        None => return Err(Error::Truncated),
                        Some(Token::DictClose) => {
                            d.0.push((k, Object::Null));
                            break;
                        }
                        Some(Token::Keyword(kw)) if kw == b"endobj" || kw == b"stream" => {
                            self.pos = save;
                            return Err(self.err("a dictionary not closed"));
                        }
                        Some(t) => {
                            let v = self.object_from(t, depth + 1)?;
                            if let Some(e) = d.0.iter_mut().find(|(n, _)| *n == k) {
                                e.1 = v;
                            } else {
                                d.0.push((k, v));
                            }
                        }
                    }
                }
                Some(Token::Keyword(kw)) if kw == b"endobj" || kw == b"stream" => {
                    return Err(self.err("a dictionary not closed"));
                }
                // A value with no key: skipped, as readers do.
                Some(t) => {
                    let _ = self.object_from(t, depth + 1)?;
                }
            }
        }
        Ok(d)
    }

    /// Expect a keyword.
    pub fn keyword(&mut self, kw: &[u8]) -> Result<()> {
        match self.next()? {
            Some(Token::Keyword(k)) if k == kw => Ok(()),
            None => Err(Error::Truncated),
            _ => Err(self.err("an expected keyword is missing")),
        }
    }

    pub fn int(&mut self) -> Result<i64> {
        match self.next()? {
            Some(Token::Int(i)) => Ok(i),
            None => Err(Error::Truncated),
            _ => Err(self.err("a number is missing")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(s: &str) -> Object { Lexer::new(s.as_bytes(), 0).object().unwrap() }

    #[test]
    fn objects() {
        assert_eq!(obj("42"), Object::Int(42));
        assert_eq!(obj("-.5"), Object::Real(-0.5));
        assert_eq!(obj("4."), Object::Real(4.0));
        assert_eq!(obj("--3"), Object::Int(-3));
        assert_eq!(obj("12 0 R"), Object::Ref(Ref::new(12, 0)));
        assert_eq!(obj("[1 2 R 3]"), Object::Array(vec![Object::Ref(Ref::new(1, 2)), Object::Int(3)]));
        assert_eq!(obj("[1 2 3]"), Object::Array(vec![Object::Int(1), Object::Int(2), Object::Int(3)]));
        assert_eq!(obj("/A#20B"), Object::Name(Name::new(b"A B")));
        assert_eq!(obj("(a\\(b\\)\\101\\\nc)"), Object::String(Arc::from(&b"a(b)Ac"[..])));
        assert_eq!(obj("(x(y)z)"), Object::String(Arc::from(&b"x(y)z"[..])));
        assert_eq!(obj("<48 65 6C6C 6F7>"), Object::String(Arc::from(&b"Hellop"[..])));
        let d = obj("<< /Type /Page /Kids [3 0 R] /N null /Empty >>");
        let d = d.as_dict().unwrap();
        assert!(d.type_is("Page"));
        assert_eq!(d.get("N"), Some(&Object::Null));
        assert_eq!(d.get("Empty"), Some(&Object::Null));
    }

    #[test]
    fn truncation_and_depth() {
        assert_eq!(Lexer::new(b"<< /A [1 2", 0).object(), Err(Error::Truncated));
        assert_eq!(Lexer::new(b"(abc", 0).object(), Err(Error::Truncated));
        let deep = "[".repeat(DEPTH_MAX + 2) + &"]".repeat(DEPTH_MAX + 2);
        assert_eq!(Lexer::new(deep.as_bytes(), 0).object(), Err(Error::Limit("nesting of arrays and dictionaries")));
        let ok = "[".repeat(DEPTH_MAX) + &"]".repeat(DEPTH_MAX);
        assert!(Lexer::new(ok.as_bytes(), 0).object().is_ok());
    }
}
