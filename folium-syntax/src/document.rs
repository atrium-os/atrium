//! A document opened for reading: the cross-reference, the trailer, the
//! security handler, and a bounded cache of parsed objects. Objects are
//! parsed on demand; nothing is read that isn't asked for.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use crate::crypt::Handler;
use crate::filter::{self, Stage};
use crate::lexer::Lexer;
use crate::limits::*;
use crate::object::{Dict, Object, Ref, Stream, StreamData};
use crate::source::Source;
use crate::xref::{self, find, Entry, Xref};
use crate::{repair, Error, Result};

/// A stream's bytes with every filter undone up to the first image codec;
/// `rest` is what is left for the image decoders.
#[derive(Debug, Clone, PartialEq)]
pub struct Decoded {
    pub data: Vec<u8>,
    pub rest: Vec<Stage>,
}

struct ObjStm {
    data: Vec<u8>,
    first: usize,
    index: Vec<(u32, usize)>,
}

#[derive(Default)]
struct Cache {
    objects: HashMap<Ref, Arc<Object>>,
    objstms: Vec<(u32, Arc<ObjStm>)>,
}

/// Which cross-reference a lookup uses: the document's own, or the one
/// the repair scan built (used when an entry of the first is wrong).
#[derive(Clone, Copy)]
struct Ctx<'a> {
    xref: &'a Xref,
    cached: bool,
    fallback: bool,
}

pub struct Document {
    src: Box<dyn Source>,
    xref: Xref,
    crypt: Option<Handler>,
    encrypt_ref: Option<Ref>,
    repaired: bool,
    strict: bool,
    /// From the header, `(1, 7)` for `%PDF-1.7`; the catalog's
    /// `/Version` is the doc layer's business.
    pub version: (u8, u8),
    cache: Mutex<Cache>,
    fallback: OnceLock<Option<Xref>>,
}

impl Document {
    pub fn open(src: Box<dyn Source>) -> Result<Document> { Self::open_with_password(src, b"") }

    pub fn from_bytes(b: Vec<u8>) -> Result<Document> { Self::open(Box::new(crate::source::Bytes(b.into()))) }

    /// Open with a password (UTF-8 for AES-256 files, Latin-1/PDFDoc for
    /// older ones). [`Error::NeedsPassword`] when it doesn't open.
    pub fn open_with_password(src: Box<dyn Source>, password: &[u8]) -> Result<Document> {
        Self::read(src, password, false)
    }

    /// Export must not substitute scanned objects or guess a stream boundary.
    pub fn open_for_export(src: Box<dyn Source>) -> Result<Document> {
        Self::read(src, b"", true)
    }

    fn read(src: Box<dyn Source>, password: &[u8], strict: bool) -> Result<Document> {
        let len = src.len();
        if len > FILE_MAX {
            return Err(Error::Limit("the file's size"));
        }
        let head = src.read_vec(0, 1024)?;
        let version = find(&head, b"%PDF-")
            .and_then(|p| {
                let v = head.get(p + 5..p + 8)?;
                (v[0].is_ascii_digit() && v[1] == b'.' && v[2].is_ascii_digit()).then(|| (v[0] - b'0', v[2] - b'0'))
            })
            .unwrap_or((1, 4));
        let mut doc = Document {
            src,
            xref: Xref::default(),
            crypt: None,
            encrypt_ref: None,
            repaired: false,
            strict,
            version,
            cache: Mutex::new(Cache::default()),
            fallback: OnceLock::new(),
        };
        let read = xref::startxref(&*doc.src).and_then(|s| xref::read(&*doc.src, s));
        match read {
            Ok(x) => {
                doc.xref = x;
                if !doc.root_ok() {
                    doc.repair()?;
                }
            }
            Err(Error::Limit(w)) => return Err(Error::Limit(w)),
            Err(_) => doc.repair()?,
        }
        if let Some(e) = doc.xref.trailer.get("Encrypt").cloned() {
            if strict { return Err(Error::NeedsPassword); }
            doc.encrypt_ref = e.as_ref();
            let enc = doc.resolve(&e)?;
            if let Some(enc) = enc.as_dict() {
                let id0 = doc
                    .xref
                    .trailer
                    .get("ID")
                    .and_then(Object::as_array)
                    .and_then(|a| a.first())
                    .and_then(Object::as_str)
                    .map(|s| s.to_vec())
                    .unwrap_or_default();
                doc.crypt = Some(Handler::open(enc, &id0, password)?);
                // Objects parsed so far were read without decryption.
                *doc.cache.lock().unwrap() = Cache::default();
            }
        }
        Ok(doc)
    }

    fn ctx(&self) -> Ctx<'_> { Ctx { xref: &self.xref, cached: true, fallback: !self.strict } }

    fn root_ok(&self) -> bool {
        let Some(r) = self.xref.trailer.get("Root").and_then(Object::as_ref) else { return false };
        let ctx = Ctx { xref: &self.xref, cached: false, fallback: false };
        matches!(self.load(r, ctx, 0), Ok(o) if o.as_dict().is_some_and(|d| d.type_is("Catalog") || d.get("Pages").is_some()))
    }

    fn repair(&mut self) -> Result<()> {
        if self.strict { return Err(Error::Unreadable("cross-reference needs repair")); }
        let x = self.scan()?;
        if x.trailer.get("Root").is_none() {
            return Err(Error::Unreadable("no catalog found"));
        }
        self.xref = x;
        self.repaired = true;
        Ok(())
    }

    /// Run the repair scan and register what object streams hold.
    fn scan(&self) -> Result<Xref> {
        let s = repair::scan(&*self.src)?;
        let mut x = s.xref;
        let mut add = Vec::new();
        {
            let ctx = Ctx { xref: &x, cached: false, fallback: false };
            for &(num, _) in &s.objstms {
                let Ok(st) = self.objstm(num, ctx, 0) else { continue };
                for (i, &(n, _)) in st.index.iter().enumerate() {
                    if x.get(n) == Entry::Missing {
                        add.push((n, Entry::InStream { stream: num, index: i as u32 }));
                    }
                }
            }
        }
        for (n, e) in add {
            let n = n as usize;
            if n < OBJECTS_MAX {
                if x.entries.len() <= n {
                    x.entries.resize(n + 1, Entry::Missing);
                }
                if x.entries[n] == Entry::Missing {
                    x.entries[n] = e;
                }
            }
        }
        Ok(x)
    }

    pub fn trailer(&self) -> &Dict { &self.xref.trailer }
    /// Opened by the repair scan.
    pub fn repaired(&self) -> bool { self.repaired }
    pub fn security(&self) -> Option<&Handler> { self.crypt.as_ref() }
    /// One past the highest object number.
    pub fn object_count(&self) -> usize { self.xref.entries.len() }
    pub fn source(&self) -> &dyn Source { &*self.src }

    pub fn catalog(&self) -> Result<Dict> {
        let r = self.xref.trailer.get("Root").cloned().unwrap_or(Object::Null);
        match self.resolve(&r)? {
            Object::Dict(d) => Ok(d),
            _ => Err(Error::Unreadable("the catalog isn't a dictionary")),
        }
    }

    /// Object `r`; a reference to nothing is `null` (§7.3.10).
    pub fn get(&self, r: Ref) -> Result<Arc<Object>> {
        if let Some(o) = self.cache.lock().unwrap().objects.get(&r) {
            return Ok(o.clone());
        }
        let o = Arc::new(self.load(r, self.ctx(), 0)?);
        let mut c = self.cache.lock().unwrap();
        if c.objects.len() >= CACHE_OBJECTS_MAX {
            c.objects.clear();
        }
        c.objects.insert(r, o.clone());
        Ok(o)
    }

    /// Follow references to a direct object.
    pub fn resolve(&self, o: &Object) -> Result<Object> { self.resolve_d(o, 0) }

    fn resolve_d(&self, o: &Object, depth: usize) -> Result<Object> {
        let mut cur = o.clone();
        for _ in 0..REF_CHAIN_MAX {
            match cur {
                Object::Ref(r) => cur = (*self.get_d(r, depth)?).clone(),
                o => return Ok(o),
            }
        }
        Err(Error::Limit("references that point at references"))
    }

    fn get_d(&self, r: Ref, depth: usize) -> Result<Arc<Object>> {
        if depth == 0 {
            return self.get(r);
        }
        Ok(Arc::new(self.load(r, self.ctx(), depth)?))
    }

    /// `d[key]`, resolved; missing is `null`.
    pub fn get_in(&self, d: &Dict, key: &str) -> Result<Object> {
        match d.get(key) {
            Some(o) => self.resolve(o),
            None => Ok(Object::Null),
        }
    }

    fn load(&self, r: Ref, ctx: Ctx, depth: usize) -> Result<Object> {
        if depth > REF_CHAIN_MAX {
            return Err(Error::Limit("references followed to parse one object"));
        }
        let res = match ctx.xref.get(r.num) {
            Entry::Free => return Ok(Object::Null),
            Entry::Missing => Ok(None),
            Entry::Offset { gen, .. } if self.strict && gen != r.gen => Err(Error::Unreadable("wrong reference generation")),
            Entry::InStream { .. } if self.strict && r.gen != 0 => Err(Error::Unreadable("wrong compressed reference generation")),
            Entry::Offset { offset, .. } => self.at_offset(r, offset, ctx, depth).map(Some),
            Entry::InStream { stream, index } => {
                if stream == r.num {
                    Ok(None)
                } else {
                    self.objstm(stream, ctx, depth + 1).map(|s| Some(in_objstm(&s, r.num, index as usize)))
                }
            }
        };
        match res {
            Ok(Some(o)) => Ok(o),
            // Missing, or the entry is wrong: ask the repair scan.
            other if ctx.fallback => {
                let fb = self.fallback.get_or_init(|| self.scan().ok());
                match fb {
                    Some(x) => self.load(r, Ctx { xref: x, cached: false, fallback: false }, depth),
                    None => other.map(|o| o.unwrap_or(Object::Null)),
                }
            }
            other => other.map(|o| o.unwrap_or(Object::Null)),
        }
    }

    fn at_offset(&self, r: Ref, offset: u64, ctx: Ctx, depth: usize) -> Result<Object> {
        let (n, g, o, data) = xref::object_header(&*self.src, offset)?;
        if n != r.num || (self.strict && g != r.gen) {
            return Err(Error::Syntax { offset, what: "the cross-reference points at another object" });
        }
        let mut o = match (o, data) {
            (Object::Dict(dict), Some(start)) => {
                let len = match dict.get("Length") {
                    Some(Object::Ref(lr)) if *lr != r => self.load(*lr, ctx, depth + 1).ok().and_then(|o| o.as_int()),
                    Some(o) => o.as_int(),
                    None => None,
                };
                let len = self.stream_len(start, len)?;
                Object::Stream(Stream { dict, data: StreamData::File { offset: start, len }, owner: Some(r) })
            }
            (o, _) => o,
        };
        if let Some(h) = &self.crypt {
            if self.encrypt_ref != Some(r) && !o.as_dict().is_some_and(|d| d.type_is("XRef")) {
                h.object(r, &mut o);
            }
        }
        Ok(o)
    }

    /// Check `/Length` against `endstream`; search for it when wrong.
    fn stream_len(&self, start: u64, len: Option<i64>) -> Result<u64> {
        if let Some(l) = len.filter(|l| *l >= 0).map(|l| l as u64) {
            if l > STREAM_ENCODED_MAX {
                return Err(Error::Limit("a stream's encoded length"));
            }
            let tail = self.src.read_vec(start + l, 64)?;
            let mut lx = Lexer::new(&tail, 0);
            lx.skip_white();
            let rest = &tail[lx.pos..];
            if self.strict {
                if lx.keyword(b"endstream").is_ok() && lx.keyword(b"endobj").is_ok() { return Ok(l); }
                return Err(Error::Unreadable("incorrect stream boundary"));
            }
            if rest.starts_with(b"endstream") || rest.starts_with(b"endobj") {
                return Ok(l);
            }
            // At the end of the file with the length in bounds: accept.
            if start + l == self.src.len() {
                return Ok(l);
            }
        }
        if self.strict { return Err(Error::Unreadable("missing stream length")); }
        // Search forward for `endstream`.
        let mut off = start;
        let end = (start + ENDSTREAM_SEARCH_MAX).min(self.src.len());
        while off < end {
            let buf = self.src.read_vec(off, 1 << 20)?;
            if buf.is_empty() {
                break;
            }
            if let Some(p) = find(&buf, b"endstream") {
                let mut e = p;
                if e > 0 && buf[e - 1] == b'\n' {
                    e -= 1;
                }
                if e > 0 && buf[e - 1] == b'\r' {
                    e -= 1;
                }
                return Ok(off - start + e as u64);
            }
            if buf.len() < 9 {
                break;
            }
            off += (buf.len() - 8) as u64;
        }
        Err(Error::Syntax { offset: start, what: "a stream with no end" })
    }

    fn objstm(&self, num: u32, ctx: Ctx, depth: usize) -> Result<Arc<ObjStm>> {
        if ctx.cached {
            if let Some((_, s)) = self.cache.lock().unwrap().objstms.iter().find(|(n, _)| *n == num) {
                return Ok(s.clone());
            }
        }
        let o = self.load(Ref::new(num, 0), Ctx { fallback: false, ..ctx }, depth + 1)?;
        let Object::Stream(st) = o else { return Err(Error::Syntax { offset: 0, what: "an object stream that isn't a stream" }) };
        let n = st.dict.int("N").unwrap_or(0).clamp(0, OBJECTS_MAX as i64) as usize;
        let first = st.dict.int("First").unwrap_or(0).max(0) as usize;
        let data = self.decode_d(&st, depth + 1)?.data;
        let first = first.min(data.len());
        let mut lx = Lexer::new(&data[..first], 0);
        let mut index = Vec::with_capacity(n.min(first / 4 + 1));
        for _ in 0..n {
            let (Ok(a), Ok(b)) = (lx.int(), lx.int()) else { break };
            if (0..=u32::MAX as i64).contains(&a) && b >= 0 {
                index.push((a as u32, b as usize));
            }
        }
        let s = Arc::new(ObjStm { data, first, index });
        if ctx.cached {
            let mut c = self.cache.lock().unwrap();
            if c.objstms.len() >= CACHE_OBJSTM_MAX {
                c.objstms.remove(0);
            }
            c.objstms.push((num, s.clone()));
        }
        Ok(s)
    }

    /// A stream's bytes as stored, decrypted, filters not undone.
    pub fn raw(&self, s: &Stream) -> Result<Vec<u8>> {
        let mut raw = match &s.data {
            StreamData::File { offset, len } => {
                if *len > STREAM_ENCODED_MAX {
                    return Err(Error::Limit("a stream's encoded length"));
                }
                self.src.read_vec(*offset, *len as usize)?
            }
            StreamData::Bytes(b) => return Ok(b.to_vec()),
        };
        if let (Some(h), Some(r)) = (&self.crypt, s.owner) {
            let d = &s.dict;
            let identity = matches!(d.get("Filter"), Some(Object::Name(n)) if n.is("Crypt"))
                || matches!(d.get("Filter").and_then(Object::as_array).and_then(|a| a.first()), Some(Object::Name(n)) if n.is("Crypt"));
            let skip_meta = d.type_is("Metadata") && !h.encrypt_metadata;
            if !d.type_is("XRef") && !identity && !skip_meta {
                raw = h.stream(r, &raw);
            }
        }
        Ok(raw)
    }

    /// A stream with its filters undone up to the first image codec,
    /// under [`STREAM_DECODED_MAX`].
    pub fn decode(&self, s: &Stream) -> Result<Decoded> { self.decode_d(s, 0) }

    fn decode_d(&self, s: &Stream, depth: usize) -> Result<Decoded> {
        let raw = self.raw(s)?;
        let res = |k: &str| -> Result<Option<Object>> {
            let Some(o) = s.dict.get(k) else { return Ok(None) };
            Ok(Some(match self.resolve_d(o, depth)? {
                Object::Array(a) => Object::Array(a.iter().map(|x| self.resolve_d(x, depth)).collect::<Result<_>>()?),
                o => o,
            }))
        };
        let f = res("Filter")?;
        let p = res("DecodeParms")?;
        let stages = filter::chain(f.as_ref(), p.as_ref())?;
        let (data, rest) = filter::decode(raw, &stages, STREAM_DECODED_MAX)?;
        Ok(Decoded { data, rest })
    }
}

fn in_objstm(s: &ObjStm, num: u32, index: usize) -> Object {
    let off = match s.index.get(index) {
        Some(&(n, o)) if n == num => o,
        _ => match s.index.iter().find(|(n, _)| *n == num) {
            Some(&(_, o)) => o,
            None => return Object::Null,
        },
    };
    let Some(at) = s.first.checked_add(off).filter(|a| *a < s.data.len()) else { return Object::Null };
    Lexer::new(&s.data[at..], 0).object().unwrap_or(Object::Null)
}
