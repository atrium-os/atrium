//! A small bounded XML tree for the text document converters.
//!
//! WordprocessingML and ODF text are read as a tree (the converters walk
//! paragraphs, runs and tables, which nest). Names keep their prefix (`w:p`,
//! `text:p`): both formats are written with their standard prefixes by every
//! producer we have seen, and a file that rebinds them reads as unknown
//! elements, which are reported, not trusted. Limits: element count, depth and
//! text bytes, so a hostile part can't make the tree unbounded.

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;

/// Most elements one part may hold.
pub const MAX_ELEMENTS: usize = 1 << 24;
/// Deepest nesting.
pub const MAX_DEPTH: usize = 256;
/// Most text bytes one part may hold.
pub const MAX_TEXT: usize = 1 << 30;

#[derive(Debug, Clone, Default)]
pub struct El {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub kids: Vec<Node>,
}

#[derive(Debug, Clone)]
pub enum Node {
    El(El),
    Text(String),
}

impl El {
    /// An attribute by qualified name (`w:val`).
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
    /// Child elements.
    pub fn els(&self) -> impl Iterator<Item = &El> {
        self.kids.iter().filter_map(|n| match n {
            Node::El(e) => Some(e),
            Node::Text(_) => None,
        })
    }
    /// The first child element named `name`.
    pub fn child(&self, name: &str) -> Option<&El> {
        self.els().find(|e| e.name == name)
    }
    /// Child elements named `name`.
    pub fn children<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a El> + 'a {
        self.els().filter(move |e| e.name == name)
    }
    /// The first element named `name` anywhere below (depth first).
    pub fn find(&self, name: &str) -> Option<&El> {
        for e in self.els() {
            if e.name == name {
                return Some(e);
            }
            if let Some(f) = e.find(name) {
                return Some(f);
            }
        }
        None
    }
    /// All text below, in order.
    pub fn text(&self) -> String {
        let mut s = String::new();
        self.text_into(&mut s);
        s
    }
    fn text_into(&self, s: &mut String) {
        for k in &self.kids {
            match k {
                Node::Text(t) => s.push_str(t),
                Node::El(e) => e.text_into(s),
            }
        }
    }
}

fn start(e: &BytesStart) -> Result<El, String> {
    let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
    let mut attrs = Vec::new();
    for a in e.attributes() {
        let a = a.map_err(|e| e.to_string())?;
        let k = String::from_utf8_lossy(a.key.as_ref()).into_owned();
        let v = a.unescape_value().map_err(|e| e.to_string())?.into_owned();
        attrs.push((k, v));
    }
    Ok(El { name, attrs, kids: Vec::new() })
}

/// Parses one XML part into its root element.
pub fn parse(data: &[u8]) -> Result<El, String> {
    let mut r = Reader::from_reader(data);
    r.config_mut().trim_text(false);
    let mut stack: Vec<El> = vec![El { name: "#document".into(), ..Default::default() }];
    let mut count = 0usize;
    let mut text = 0usize;
    let mut buf = Vec::new();
    loop {
        let ev = r.read_event_into(&mut buf).map_err(|e| e.to_string())?;
        match ev {
            Event::Start(e) => {
                count += 1;
                if count > MAX_ELEMENTS || stack.len() > MAX_DEPTH {
                    return Err("too many or too deeply nested elements".into());
                }
                stack.push(start(&e)?);
            }
            Event::Empty(e) => {
                count += 1;
                if count > MAX_ELEMENTS {
                    return Err("too many elements".into());
                }
                let el = start(&e)?;
                stack.last_mut().expect("root").kids.push(Node::El(el));
            }
            Event::End(_) => {
                if stack.len() < 2 {
                    return Err("unbalanced end tag".into());
                }
                let el = stack.pop().expect("checked");
                stack.last_mut().expect("root").kids.push(Node::El(el));
            }
            Event::Text(t) => {
                let s = t.unescape().map_err(|e| e.to_string())?;
                text += s.len();
                if text > MAX_TEXT {
                    return Err("too much text".into());
                }
                if stack.len() > 1 {
                    stack.last_mut().expect("root").kids.push(Node::Text(s.into_owned()));
                }
            }
            Event::CData(t) => {
                let s = String::from_utf8_lossy(&t.into_inner()).into_owned();
                text += s.len();
                if stack.len() > 1 {
                    stack.last_mut().expect("root").kids.push(Node::Text(s));
                }
            }
            Event::DocType(_) => return Err("a DOCTYPE (refused: no DTDs)".into()),
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    if stack.len() != 1 {
        return Err("unclosed element".into());
    }
    let doc = stack.pop().expect("root");
    doc.kids.into_iter().find_map(|n| match n {
        Node::El(e) => Some(e),
        Node::Text(_) => None,
    }).ok_or_else(|| "no root element".into())
}

/// Escapes text for an XML element or attribute.
pub fn esc(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            // Characters XML 1.0 can't hold.
            c if (c as u32) < 0x20 && !matches!(c, '\t' | '\n' | '\r') => {}
            '\u{FFFE}' | '\u{FFFF}' => {}
            c => o.push(c),
        }
    }
    o
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_small_tree_and_refuses_dtds() {
        let e = parse(br#"<?xml version="1.0"?><w:p a="1&amp;2"><w:r><w:t>hi &lt;there&gt;</w:t></w:r><w:br/></w:p>"#).unwrap();
        assert_eq!(e.name, "w:p");
        assert_eq!(e.attr("a"), Some("1&2"));
        assert_eq!(e.text(), "hi <there>");
        assert!(e.child("w:br").is_some());
        assert!(parse(b"<!DOCTYPE x [<!ENTITY a 'b'>]><x>&a;</x>").is_err());
        assert!(parse(b"<a><b></a>").is_err());
    }
}
