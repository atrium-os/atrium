//! The cross-reference (ISO 32000-2 §7.5.4–§7.5.8): classic tables,
//! cross-reference streams, hybrid files (`/XRefStm`) and the `/Prev`
//! chain of incremental updates. Newer sections win.

use std::collections::HashSet;

use crate::filter;
use crate::lexer::{Lexer, Token};
use crate::limits::{OBJECTS_MAX, OBJECT_BYTES_MAX, STREAM_DECODED_MAX, XREF_CHAIN_MAX};
use crate::object::{Dict, Object};
use crate::source::Source;
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Entry {
    /// Not yet seen in any section.
    #[default]
    Missing,
    Free,
    /// An object at a byte offset.
    Offset { offset: u64, gen: u16 },
    /// The `index`th object of object stream `stream`.
    InStream { stream: u32, index: u32 },
}

#[derive(Debug, Default)]
pub struct Xref {
    pub entries: Vec<Entry>,
    /// The newest trailer, with keys older trailers had and it lacks
    /// filled in (`/Prev` and `/XRefStm` left out).
    pub trailer: Dict,
}

impl Xref {
    pub fn get(&self, num: u32) -> Entry { self.entries.get(num as usize).copied().unwrap_or_default() }

    /// Set an entry unless a newer section already did.
    fn offer(&mut self, num: u64, e: Entry) {
        if num as usize >= OBJECTS_MAX {
            return;
        }
        let n = num as usize;
        if self.entries.len() <= n {
            self.entries.resize(n + 1, Entry::Missing);
        }
        if self.entries[n] == Entry::Missing {
            self.entries[n] = e;
        }
    }

    fn merge_trailer(&mut self, t: &Dict) {
        for (k, v) in t.iter() {
            if k.is("Prev") || k.is("XRefStm") || k.is("Type") || k.is("W") || k.is("Index") || k.is("Filter")
                || k.is("DecodeParms") || k.is("Length")
            {
                continue;
            }
            if self.trailer.get(std::str::from_utf8(k.as_bytes()).unwrap_or("")).is_none() {
                self.trailer.0.push((k.clone(), v.clone()));
            }
        }
    }
}

/// Parse with a window that starts small and doubles while the parse runs
/// off its end, up to `max` bytes. `f` gets the lexer and whether the
/// window reaches the end of the file.
pub fn parse_at<T>(src: &dyn Source, offset: u64, max: usize, mut f: impl FnMut(&mut Lexer, bool) -> Result<T>) -> Result<T> {
    let len = src.len();
    if offset >= len {
        return Err(Error::Syntax { offset, what: "an offset past the end of the file" });
    }
    let mut size = 4096usize;
    loop {
        let buf = src.read_vec(offset, size)?;
        let eof = offset + buf.len() as u64 >= len;
        let mut lx = Lexer::new(&buf, offset);
        match f(&mut lx, eof) {
            Err(Error::Truncated) if !eof && size < max => size = (size * 2).min(max),
            r => return r,
        }
    }
}

/// The offset `startxref` names.
pub fn startxref(src: &dyn Source) -> Result<u64> {
    let len = src.len();
    let tail_len = 2048.min(len) as usize;
    let tail = src.read_vec(len - tail_len as u64, tail_len)?;
    let at = rfind(&tail, b"startxref").ok_or(Error::Unreadable("no startxref"))?;
    let mut lx = Lexer::new(&tail[at + 9..], 0);
    match lx.next()? {
        Some(Token::Int(o)) if o >= 0 && (o as u64) < len => Ok(o as u64),
        _ => Err(Error::Unreadable("startxref names no offset in the file")),
    }
}

pub fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> { hay.windows(needle.len()).rposition(|w| w == needle) }
pub fn find(hay: &[u8], needle: &[u8]) -> Option<usize> { hay.windows(needle.len()).position(|w| w == needle) }

/// Read the whole chain from `start`.
pub fn read(src: &dyn Source, start: u64) -> Result<Xref> {
    let mut x = Xref::default();
    let mut seen = HashSet::new();
    let mut next = Some(start);
    let mut first = true;
    while let Some(off) = next.take() {
        if !seen.insert(off) {
            break; // a loop in /Prev
        }
        if seen.len() > XREF_CHAIN_MAX {
            return Err(Error::Limit("the /Prev chain"));
        }
        let trailer = match section(src, off, &mut x) {
            Ok(t) => t,
            // An older section that's broken: keep what the newer ones gave.
            Err(e) if !first => {
                let _ = e;
                break;
            }
            Err(e) => return Err(e),
        };
        first = false;
        if let Some(s) = trailer.int("XRefStm") {
            if s >= 0 && seen.insert(s as u64) {
                let _ = section(src, s as u64, &mut x);
            }
        }
        x.merge_trailer(&trailer);
        next = trailer.int("Prev").filter(|p| *p >= 0).map(|p| p as u64);
    }
    Ok(x)
}

/// One section at `off`: a table or a stream. Returns its trailer.
fn section(src: &dyn Source, off: u64, x: &mut Xref) -> Result<Dict> {
    let head = src.read_vec(off, 16)?;
    let mut lx = Lexer::new(&head, off);
    lx.skip_white();
    if head[lx.pos..].starts_with(b"xref") {
        table(src, off, x)
    } else {
        stream(src, off, x)
    }
}

fn table(src: &dyn Source, off: u64, x: &mut Xref) -> Result<Dict> {
    // A table for OBJECTS_MAX objects at 20 bytes each, plus slack.
    let max = (OBJECTS_MAX * 20 + (1 << 20)).min(src.len().saturating_sub(off) as usize + 1);
    let mut found: Vec<(u64, Entry)> = Vec::new();
    let trailer = parse_at(src, off, max, |lx, _| {
        found.clear();
        lx.keyword(b"xref")?;
        loop {
            match lx.next()? {
                None => return Err(Error::Truncated),
                Some(Token::Keyword(b"trailer")) => break,
                Some(Token::Int(start)) => {
                    let count = lx.int()?;
                    if start < 0 || count < 0 || (start + count) as usize > OBJECTS_MAX {
                        return Err(Error::Limit("objects in a cross-reference table"));
                    }
                    for i in 0..count {
                        let o = lx.int()?;
                        let g = lx.int()?;
                        let e = match lx.next()? {
                            None => return Err(Error::Truncated),
                            Some(Token::Keyword(b"n")) => {
                                Entry::Offset { offset: o.max(0) as u64, gen: g.clamp(0, u16::MAX as i64) as u16 }
                            }
                            Some(Token::Keyword(b"f")) => Entry::Free,
                            _ => return Err(Error::Syntax { offset: lx.base + lx.pos as u64, what: "a table entry" }),
                        };
                        found.push(((start + i) as u64, e));
                    }
                }
                _ => return Err(Error::Syntax { offset: lx.base + lx.pos as u64, what: "a table subsection" }),
            }
        }
        match lx.next()? {
            Some(Token::DictOpen) => lx.dict_body(0),
            None => Err(Error::Truncated),
            _ => Err(Error::Syntax { offset: lx.base + lx.pos as u64, what: "a trailer dictionary" }),
        }
    })?;
    // Offset 0 for an in-use object is a common writer bug meaning "free".
    for (n, e) in found {
        let e = if matches!(e, Entry::Offset { offset: 0, .. }) { Entry::Free } else { e };
        x.offer(n, e);
    }
    Ok(trailer)
}

/// `n g obj << … >> stream` at `off`: the dictionary and where the data
/// starts. `/Length` must be direct here (it is for cross-reference
/// streams); the caller handles indirect lengths for other streams.
pub fn object_header(src: &dyn Source, off: u64) -> Result<(u32, u16, Object, Option<u64>)> {
    parse_at(src, off, OBJECT_BYTES_MAX, |lx, eof| {
        let n = lx.int()?;
        let g = lx.int()?;
        lx.keyword(b"obj")?;
        let o = lx.object()?;
        let data = match lx.next() {
            Ok(Some(Token::Keyword(b"stream"))) => Some(off + stream_data_start(lx.buf, lx.pos) as u64),
            Ok(None) if !eof => return Err(Error::Truncated),
            Err(Error::Truncated) if !eof => return Err(Error::Truncated),
            _ => None,
        };
        if !(0..=u32::MAX as i64).contains(&n) {
            return Err(Error::Syntax { offset: off, what: "an object number" });
        }
        Ok((n as u32, g.clamp(0, u16::MAX as i64) as u16, o, data))
    })
}

/// Past the end-of-line after `stream` (CRLF or LF; a lone CR or stray
/// spaces before it are tolerated).
pub fn stream_data_start(buf: &[u8], mut p: usize) -> usize {
    let mut q = p;
    while buf.get(q) == Some(&b' ') {
        q += 1;
    }
    if matches!(buf.get(q), Some(b'\r' | b'\n')) {
        p = q;
    }
    if buf.get(p) == Some(&b'\r') {
        p += 1;
    }
    if buf.get(p) == Some(&b'\n') {
        p += 1;
    }
    p
}

fn stream(src: &dyn Source, off: u64, x: &mut Xref) -> Result<Dict> {
    let (_, _, o, data) = object_header(src, off)?;
    let Object::Dict(d) = o else { return Err(Error::Syntax { offset: off, what: "a cross-reference stream" }) };
    let Some(start) = data else { return Err(Error::Syntax { offset: off, what: "a cross-reference stream" }) };
    if !d.type_is("XRef") {
        return Err(Error::Syntax { offset: off, what: "a cross-reference stream without /Type /XRef" });
    }
    let len = d.int("Length").filter(|l| *l >= 0).ok_or(Error::Syntax { offset: off, what: "a cross-reference stream's /Length" })?;
    if len as u64 > crate::limits::STREAM_ENCODED_MAX {
        return Err(Error::Limit("a stream's encoded length"));
    }
    let raw = src.read_vec(start, len as usize)?;
    let stages = filter::chain(d.get("Filter"), d.get("DecodeParms"))?;
    let (bytes, rest) = filter::decode(raw, &stages, STREAM_DECODED_MAX)?;
    if !rest.is_empty() {
        return Err(Error::Syntax { offset: off, what: "a cross-reference stream with an image filter" });
    }
    let w: Vec<usize> = d
        .get("W")
        .and_then(Object::as_array)
        .map(|a| a.iter().map(|o| o.as_int().unwrap_or(-1)).collect::<Vec<_>>())
        .filter(|v| v.len() == 3 && v.iter().all(|&n| (0..=8).contains(&n)))
        .ok_or(Error::Syntax { offset: off, what: "a cross-reference stream's /W" })?
        .into_iter()
        .map(|n| n as usize)
        .collect();
    let size = d.int("Size").unwrap_or(0).max(0);
    let index: Vec<i64> = match d.get("Index").and_then(Object::as_array) {
        Some(a) => a.iter().filter_map(Object::as_int).collect(),
        None => vec![0, size],
    };
    let rec = w[0] + w[1] + w[2];
    if rec == 0 {
        return Ok(d);
    }
    let field = |s: &[u8]| s.iter().fold(0u64, |a, &b| a << 8 | b as u64);
    let mut recs = bytes.chunks_exact(rec);
    for pair in index.chunks_exact(2) {
        let (start, count) = (pair[0], pair[1]);
        if start < 0 || count < 0 || (start.saturating_add(count)) as usize > OBJECTS_MAX {
            return Err(Error::Limit("objects in a cross-reference stream"));
        }
        for i in 0..count {
            let Some(r) = recs.next() else { return Ok(d) };
            let t = if w[0] == 0 { 1 } else { field(&r[..w[0]]) };
            let f2 = field(&r[w[0]..w[0] + w[1]]);
            let f3 = field(&r[w[0] + w[1]..]);
            let e = match t {
                0 => Entry::Free,
                1 if f2 == 0 => Entry::Free,
                1 => Entry::Offset { offset: f2, gen: f3.min(u16::MAX as u64) as u16 },
                2 if f2 <= u32::MAX as u64 => Entry::InStream { stream: f2 as u32, index: f3.min(u32::MAX as u64) as u32 },
                // Unknown types are null references (§7.5.8.3).
                _ => Entry::Free,
            };
            x.offer((start + i) as u64, e);
        }
    }
    Ok(d)
}
