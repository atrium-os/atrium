//! The XML that AnyConnect gateways speak (`config-auth`), read by a
//! strict, bounded parser (vpn.md §9.2): no DTD, no entities beyond the
//! five predefined ones and character references, no processing beyond
//! the XML declaration, size, depth, element and attribute caps. What it
//! returns is a small tree; anything outside the subset is an error, never
//! a guess.

use std::fmt::Write as _;

/// The largest document read.
pub const MAX_DOC: usize = 64 * 1024;
pub const MAX_DEPTH: usize = 16;
pub const MAX_NODES: usize = 1024;
pub const MAX_ATTRS: usize = 16;
pub const MAX_NAME: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Element {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Element>,
    /// The element's own text, pieces joined (mixed content is flattened).
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    TooLarge,
    TooDeep,
    TooMany,
    Doctype,
    Syntax(&'static str),
    Mismatch,
    Entity,
    NotUtf8,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::TooLarge => f.write_str("document too large"),
            Error::TooDeep => f.write_str("nested too deep"),
            Error::TooMany => f.write_str("too many elements or attributes"),
            Error::Doctype => f.write_str("a DTD (refused)"),
            Error::Syntax(s) => write!(f, "syntax: {s}"),
            Error::Mismatch => f.write_str("end tag does not match"),
            Error::Entity => f.write_str("unknown entity"),
            Error::NotUtf8 => f.write_str("not UTF-8"),
        }
    }
}

impl Element {
    pub fn new(name: &str) -> Element {
        Element { name: name.into(), attrs: Vec::new(), children: Vec::new(), text: String::new() }
    }

    pub fn attr(&self, k: &str) -> Option<&str> {
        self.attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
    }

    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }

    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// The text of a child, trimmed.
    pub fn child_text(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.trim())
    }

    pub fn with_attr(mut self, k: &str, v: &str) -> Element {
        self.attrs.push((k.into(), v.into()));
        self
    }

    pub fn with_child(mut self, c: Element) -> Element {
        self.children.push(c);
        self
    }

    pub fn with_text(mut self, t: &str) -> Element {
        self.text = t.into();
        self
    }

    /// The element as a document, with the declaration.
    pub fn to_doc(&self) -> String {
        let mut s = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
        self.write(&mut s);
        s
    }

    fn write(&self, s: &mut String) {
        let _ = write!(s, "<{}", self.name);
        for (k, v) in &self.attrs {
            let _ = write!(s, " {k}=\"{}\"", escape(v));
        }
        if self.children.is_empty() && self.text.is_empty() {
            s.push_str("/>");
            return;
        }
        s.push('>');
        s.push_str(&escape(&self.text));
        for c in &self.children {
            c.write(s);
        }
        let _ = write!(s, "</{}>", self.name);
    }
}

pub fn escape(t: &str) -> String {
    let mut o = String::with_capacity(t.len());
    for c in t.chars() {
        match c {
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '&' => o.push_str("&amp;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c => o.push(c),
        }
    }
    o
}

struct P<'a> {
    b: &'a [u8],
    i: usize,
    nodes: usize,
}

fn is_name_byte(c: u8, first: bool) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c == b':' || (!first && (c.is_ascii_digit() || c == b'-' || c == b'.'))
}

impl<'a> P<'a> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn starts(&self, s: &str) -> bool {
        self.b[self.i..].starts_with(s.as_bytes())
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.i += 1;
        }
    }

    fn name(&mut self) -> Result<String, Error> {
        let s = self.i;
        while let Some(c) = self.peek() {
            if !is_name_byte(c, self.i == s) {
                break;
            }
            self.i += 1;
        }
        if self.i == s {
            return Err(Error::Syntax("name expected"));
        }
        if self.i - s > MAX_NAME {
            return Err(Error::TooLarge);
        }
        Ok(String::from_utf8_lossy(&self.b[s..self.i]).into_owned())
    }

    /// Skip to just past `end`.
    fn skip_past(&mut self, end: &str) -> Result<(), Error> {
        match self.b[self.i..].windows(end.len()).position(|w| w == end.as_bytes()) {
            Some(p) => {
                self.i += p + end.len();
                Ok(())
            }
            None => Err(Error::Syntax("unterminated")),
        }
    }

    /// Comments, the declaration, whitespace. A DTD is refused.
    fn misc(&mut self) -> Result<(), Error> {
        loop {
            self.ws();
            if self.starts("<!--") {
                self.i += 4;
                self.skip_past("-->")?;
            } else if self.starts("<?") {
                self.i += 2;
                let n = self.name()?;
                if !n.eq_ignore_ascii_case("xml") {
                    return Err(Error::Syntax("processing instruction"));
                }
                self.skip_past("?>")?;
            } else if self.starts("<!") {
                return Err(Error::Doctype);
            } else {
                return Ok(());
            }
        }
    }

    fn element(&mut self, depth: usize) -> Result<Element, Error> {
        if depth > MAX_DEPTH {
            return Err(Error::TooDeep);
        }
        self.nodes += 1;
        if self.nodes > MAX_NODES {
            return Err(Error::TooMany);
        }
        if self.peek() != Some(b'<') {
            return Err(Error::Syntax("< expected"));
        }
        self.i += 1;
        let mut e = Element::new(&self.name()?);
        loop {
            self.ws();
            match self.peek() {
                Some(b'/') => {
                    self.i += 1;
                    if self.peek() != Some(b'>') {
                        return Err(Error::Syntax("> expected"));
                    }
                    self.i += 1;
                    return Ok(e);
                }
                Some(b'>') => {
                    self.i += 1;
                    break;
                }
                Some(_) => {
                    let k = self.name()?;
                    self.ws();
                    if self.peek() != Some(b'=') {
                        return Err(Error::Syntax("= expected"));
                    }
                    self.i += 1;
                    self.ws();
                    let q = self.peek().filter(|q| *q == b'"' || *q == b'\'').ok_or(Error::Syntax("quote expected"))?;
                    self.i += 1;
                    let s = self.i;
                    while self.peek().ok_or(Error::Syntax("unterminated attribute"))? != q {
                        if self.peek() == Some(b'<') {
                            return Err(Error::Syntax("< in attribute"));
                        }
                        self.i += 1;
                    }
                    let v = unescape(&self.b[s..self.i])?;
                    self.i += 1;
                    if e.attrs.iter().any(|(n, _)| *n == k) {
                        return Err(Error::Syntax("repeated attribute"));
                    }
                    if e.attrs.len() >= MAX_ATTRS {
                        return Err(Error::TooMany);
                    }
                    e.attrs.push((k, v));
                }
                None => return Err(Error::Syntax("unterminated tag")),
            }
        }
        // Content.
        loop {
            if self.starts("</") {
                self.i += 2;
                let n = self.name()?;
                self.ws();
                if self.peek() != Some(b'>') {
                    return Err(Error::Syntax("> expected"));
                }
                self.i += 1;
                if n != e.name {
                    return Err(Error::Mismatch);
                }
                return Ok(e);
            } else if self.starts("<!--") {
                self.i += 4;
                self.skip_past("-->")?;
            } else if self.starts("<![CDATA[") {
                self.i += 9;
                let s = self.i;
                self.skip_past("]]>")?;
                let t = std::str::from_utf8(&self.b[s..self.i - 3]).map_err(|_| Error::NotUtf8)?;
                e.text.push_str(t);
            } else if self.starts("<!") || self.starts("<?") {
                return Err(Error::Syntax("declaration in content"));
            } else if self.peek() == Some(b'<') {
                let c = self.element(depth + 1)?;
                e.children.push(c);
            } else if self.peek().is_none() {
                return Err(Error::Syntax("unterminated element"));
            } else {
                let s = self.i;
                while self.peek().is_some_and(|c| c != b'<') {
                    self.i += 1;
                }
                e.text.push_str(&unescape(&self.b[s..self.i])?);
            }
        }
    }
}

fn unescape(b: &[u8]) -> Result<String, Error> {
    let s = std::str::from_utf8(b).map_err(|_| Error::NotUtf8)?;
    if !s.contains('&') {
        return Ok(s.to_string());
    }
    let mut o = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        o.push_str(&rest[..p]);
        let after = &rest[p + 1..];
        let end = after.find(';').filter(|e| *e <= 10).ok_or(Error::Entity)?;
        let ent = &after[..end];
        let c = match ent {
            "lt" => '<',
            "gt" => '>',
            "amp" => '&',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let n = if let Some(h) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
                    u32::from_str_radix(h, 16).ok()
                } else if let Some(d) = ent.strip_prefix('#') {
                    d.parse::<u32>().ok()
                } else {
                    None
                };
                n.and_then(char::from_u32).filter(|c| *c != '\0').ok_or(Error::Entity)?
            }
        };
        o.push(c);
        rest = &after[end + 1..];
    }
    o.push_str(rest);
    Ok(o)
}

/// One document: its root element.
pub fn parse(b: &[u8]) -> Result<Element, Error> {
    if b.len() > MAX_DOC {
        return Err(Error::TooLarge);
    }
    let b = b.strip_prefix(b"\xef\xbb\xbf").unwrap_or(b);
    let mut p = P { b, i: 0, nodes: 0 };
    p.misc()?;
    let e = p.element(0)?;
    p.misc()?;
    if p.i != b.len() {
        return Err(Error::Syntax("content after the root"));
    }
    Ok(e)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_subset() {
        let d = br#"<?xml version="1.0" encoding="UTF-8"?>
<!-- hi -->
<config-auth client="vpn" type='auth-request'>
  <auth id="main"><message>Please enter your username &amp; password.</message>
  <form method="post" action="/auth"><input type="text" name="username" label="Username:"/></form></auth>
  <x><![CDATA[a<b]]>&#x41;&#66;</x>
</config-auth>"#;
        let e = parse(d).unwrap();
        assert_eq!(e.name, "config-auth");
        assert_eq!(e.attr("type"), Some("auth-request"));
        let a = e.child("auth").unwrap();
        assert_eq!(a.child_text("message"), Some("Please enter your username & password."));
        assert_eq!(a.child("form").unwrap().child("input").unwrap().attr("name"), Some("username"));
        assert_eq!(e.child("x").unwrap().text, "a<bAB");
        // Round trip of what we write.
        let w = Element::new("a").with_attr("k", "<\"&").with_child(Element::new("b").with_text("x&y"));
        assert_eq!(parse(w.to_doc().as_bytes()).unwrap(), w);
    }

    #[test]
    fn refuses_the_rest() {
        assert_eq!(parse(b"<!DOCTYPE x [<!ENTITY a 'b'>]><x>&a;</x>"), Err(Error::Doctype));
        assert_eq!(parse(b"<x>&a;</x>"), Err(Error::Entity));
        assert_eq!(parse(b"<x>&#0;</x>"), Err(Error::Entity));
        assert_eq!(parse(b"<x></y>"), Err(Error::Mismatch));
        assert!(parse(b"<x a='1' a='2'/>").is_err());
        assert!(parse(b"<x/><y/>").is_err());
        assert!(parse(b"<x><?php ?></x>").is_err());
        assert!(parse(b"<x").is_err());
        let deep = "<a>".repeat(MAX_DEPTH + 2) + &"</a>".repeat(MAX_DEPTH + 2);
        assert_eq!(parse(deep.as_bytes()), Err(Error::TooDeep));
        let many = format!("<a>{}</a>", "<b/>".repeat(MAX_NODES + 1));
        assert_eq!(parse(many.as_bytes()), Err(Error::TooMany));
        assert_eq!(parse(&vec![b' '; MAX_DOC + 1]), Err(Error::TooLarge));
        assert!(parse(b"<x>\xff</x>").is_err());
    }
}
