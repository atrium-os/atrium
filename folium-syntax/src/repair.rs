//! The repair scan: when the cross-reference is missing or wrong, find
//! the objects by reading the file front to back for `n g obj` (later
//! definitions win, as incremental updates do) and the last `trailer`.
//! Bounded by the file's size (≤ FILE_MAX) and read in chunks.

use crate::lexer::{is_regular, is_white, Token};
use crate::limits::{OBJECTS_MAX, OBJECT_BYTES_MAX};
use crate::object::{Dict, Object, Ref};
use crate::source::Source;
use crate::xref::{find, parse_at, Entry, Xref};
use crate::Result;

const CHUNK: usize = 1 << 20;
const BACK: usize = 64;
const AHEAD: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Other,
    Catalog,
    ObjStm,
    XRef,
}

#[derive(Debug, Default)]
pub struct Scan {
    pub xref: Xref,
    /// Object streams found, with their offsets, for the document to
    /// register what they hold (it needs decryption to read them).
    pub objstms: Vec<(u32, u64)>,
}

pub fn scan(src: &dyn Source) -> Result<Scan> {
    let len = src.len();
    let mut found: Vec<(u32, u16, u64, Kind)> = Vec::new();
    let mut trailers: Vec<u64> = Vec::new();
    let mut off = 0u64;
    while off < len {
        let start = off.saturating_sub(BACK as u64);
        let buf = src.read_vec(start, CHUNK + BACK + AHEAD)?;
        let lo = (off - start) as usize;
        let hi = (lo + CHUNK).min(buf.len());
        for i in lo..hi {
            match buf[i] {
                b'o' if buf[i..].starts_with(b"obj") && !buf.get(i + 3).is_some_and(|&b| is_regular(b)) => {
                    if let Some((num, gen, at)) = header_before(&buf, i) {
                        let kind = kind_after(&buf[i + 3..(i + 3 + AHEAD).min(buf.len())]);
                        if (num as usize) < OBJECTS_MAX {
                            found.push((num, gen, start + at as u64, kind));
                        }
                    }
                }
                b't' if buf[i..].starts_with(b"trailer") && (i == 0 || !is_regular(buf[i - 1])) => {
                    trailers.push(start + i as u64);
                }
                _ => {}
            }
        }
        off += CHUNK as u64;
    }

    let mut s = Scan::default();
    // Later definitions win: walk backwards and offer.
    let mut entries = vec![Entry::Missing; found.iter().map(|f| f.0 as usize + 1).max().unwrap_or(0)];
    let mut root_guess = None;
    let mut xref_dicts = Vec::new();
    for &(num, gen, at, kind) in found.iter().rev() {
        let e = &mut entries[num as usize];
        if *e != Entry::Missing {
            continue;
        }
        *e = Entry::Offset { offset: at, gen };
        match kind {
            Kind::ObjStm => s.objstms.push((num, at)),
            Kind::Catalog if root_guess.is_none() => root_guess = Some(Ref::new(num, gen)),
            Kind::XRef => xref_dicts.push(at),
            _ => {}
        }
    }
    s.xref.entries = entries;

    // The last trailer that names a root; else the newest cross-reference
    // stream's dictionary; else the last catalog found.
    let mut trailer = None;
    for &t in trailers.iter().rev() {
        let d = parse_at(src, t + 7, OBJECT_BYTES_MAX, |lx, _| match lx.next()? {
            Some(Token::DictOpen) => lx.dict_body(0),
            _ => Ok(Dict::new()),
        });
        if let Ok(d) = d {
            if d.get("Root").is_some() {
                trailer = Some(d);
                break;
            }
        }
    }
    if trailer.is_none() {
        for &at in xref_dicts.iter() {
            if let Ok((_, _, Object::Dict(d), _)) = crate::xref::object_header(src, at) {
                if d.get("Root").is_some() {
                    trailer = Some(d);
                    break;
                }
            }
        }
    }
    let mut trailer = trailer.unwrap_or_default();
    // A trailer whose root isn't a catalog we found: use the catalog.
    let root_ok = trailer.get("Root").and_then(Object::as_ref).is_some_and(|r| {
        found.iter().any(|f| f.0 == r.num && f.3 == Kind::Catalog)
    });
    if !root_ok {
        if let Some(r) = root_guess {
            trailer.insert("Root", Object::Ref(r));
        }
    }
    s.xref.trailer = trailer;
    Ok(s)
}

/// `num gen ` before `obj` at `i`: the numbers and where they start.
fn header_before(buf: &[u8], i: usize) -> Option<(u32, u16, usize)> {
    let mut j = i;
    let white = |j: &mut usize| {
        let s = *j;
        while *j > 0 && is_white(buf[*j - 1]) {
            *j -= 1;
        }
        *j < s
    };
    let digits = |j: &mut usize| -> Option<u64> {
        let e = *j;
        while *j > 0 && buf[*j - 1].is_ascii_digit() && e - *j < 10 {
            *j -= 1;
        }
        if *j == e {
            return None;
        }
        std::str::from_utf8(&buf[*j..e]).ok()?.parse().ok()
    };
    if !white(&mut j) {
        return None;
    }
    let gen = digits(&mut j)?;
    if !white(&mut j) {
        return None;
    }
    let num = digits(&mut j)?;
    if j > 0 && is_regular(buf[j - 1]) {
        return None;
    }
    if num > u32::MAX as u64 || gen > u16::MAX as u64 {
        return None;
    }
    Some((num as u32, gen as u16, j))
}

fn kind_after(w: &[u8]) -> Kind {
    let end = [find(w, b"stream"), find(w, b"endobj")].into_iter().flatten().min().unwrap_or(w.len());
    let w = &w[..end];
    let has = |n: &[u8]| {
        let mut p = 0;
        while let Some(k) = find(&w[p..], n) {
            let a = p + k + n.len();
            if !w.get(a).is_some_and(|&b| is_regular(b)) {
                return true;
            }
            p = a;
        }
        false
    };
    if find(w, b"/Type").is_none() {
        return Kind::Other;
    }
    if has(b"/XRef") {
        Kind::XRef
    } else if has(b"/ObjStm") {
        Kind::ObjStm
    } else if has(b"/Catalog") {
        Kind::Catalog
    } else {
        Kind::Other
    }
}
