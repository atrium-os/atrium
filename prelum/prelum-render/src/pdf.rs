//! Prelum's conversion step, PDF half (printing.md §6.2, decision 5): the
//! app's PDF is never sent as is. A new file is written with Folium's object
//! writer that holds only what draws:
//!
//! - each sheet is a new page whose content places the source pages as Form
//!   XObjects (pages per sheet, fit or actual size, turned to match the
//!   paper: the same layout as the raster path);
//! - every content stream is re-tokenised by Folium's lexer and written back
//!   operator by operator, keeping only PDF's content operators (unknown
//!   operators and compatibility sections are dropped);
//! - resources are deep-copied from the source pages with everything that
//!   isn't drawing removed: actions, JavaScript, links, annotations, forms,
//!   XFA, attached files, metadata, structure; pages, catalogs and the page
//!   tree can't be reached through a resource;
//! - every stream is decoded and written again (Flate), JPEG kept as JPEG;
//! - no incremental updates, object streams, encryption or anything from the
//!   original trailer.
//!
//! What this subset can't express returns [`Error::NeedsRaster`], and the job
//! goes to the printer as pages of pixels instead (§6.2): inline images,
//! Type 3 fonts, JPEG 2000, JBIG2 and CCITT images for now.
//!
//! Annotations (including filled form fields) are not printed by either path
//! until Folium draws them (folium.md P3/P5).

use std::collections::HashMap;
use std::io::Write;

use folium_content::{Matrix, Rect};
use folium_doc::{Doc, Page};
use folium_syntax::lexer::{Lexer, Token};
use folium_syntax::object::{Dict, Name, Object, Ref, Stream};
use folium_syntax::write::{self as w, Writer};

use crate::{cells, media_size_pt, place, Settings};

#[derive(Debug)]
pub enum Error {
    /// The document uses something the rewrite doesn't carry; print it as
    /// raster instead. Says what.
    NeedsRaster(&'static str),
    Document(String),
    Settings(&'static str),
    Io(std::io::Error),
    Cancelled,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NeedsRaster(w) => write!(f, "needs raster: {w}"),
            Error::Document(e) => write!(f, "document: {e}"),
            Error::Settings(w) => write!(f, "settings: {w}"),
            Error::Io(e) => write!(f, "{e}"),
            Error::Cancelled => write!(f, "cancelled"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// PDF content operators kept (ISO 32000-2 Annex A), less inline images
/// (`BI`/`ID`/`EI`, refused) and compatibility sections (`BX`/`EX`, dropped).
const OPERATORS: &[&[u8]] = &[
    b"b", b"B", b"b*", b"B*", b"BDC", b"BMC", b"BT", b"c", b"cm", b"CS", b"cs", b"d", b"d0", b"d1", b"Do", b"DP", b"EMC", b"ET", b"f",
    b"F", b"f*", b"G", b"g", b"gs", b"h", b"i", b"j", b"J", b"K", b"k", b"l", b"m", b"M", b"MP", b"n", b"q", b"Q", b"re", b"RG", b"rg",
    b"ri", b"s", b"S", b"SC", b"sc", b"SCN", b"scn", b"sh", b"T*", b"Tc", b"Td", b"TD", b"Tf", b"Tj", b"TJ", b"TL", b"Tm", b"Tr", b"Ts",
    b"Tw", b"Tz", b"v", b"w", b"W", b"W*", b"y", b"'", b"\"",
];

/// Keys never copied, at any depth.
const DROP_KEYS: &[&str] = &[
    "JS", "JavaScript", "AA", "A", "OpenAction", "URI", "Launch", "EmbeddedFile", "EmbeddedFiles", "EF", "RichMedia", "Annots",
    "AcroForm", "XFA", "Metadata", "PieceInfo", "Parent", "StructParent", "StructParents", "StructTreeRoot", "Names", "Outlines",
    "Threads", "Dests", "Thumb", "B", "Collection", "AF", "OPI",
];

/// Dictionaries never copied (replaced by null) by `/Type`.
const DROP_TYPES: &[&str] = &["Page", "Pages", "Catalog", "Annot", "Action", "Filespec", "EmbeddedFile", "Sig", "ObjStm", "XRef"];

/// Bounds on the copy.
const MAX_OBJECTS: usize = 200_000;
const MAX_DEPTH: usize = 256;
const MAX_CONTENT_BYTES: usize = 256 << 20;
const MAX_ARGS: usize = 64;

/// Re-tokenises a content stream and writes back the operators it keeps.
pub fn sanitize_content(data: &[u8], out: &mut Vec<u8>) -> Result<(), Error> {
    let mut lx = Lexer::new(data, 0);
    let mut args: Vec<Object> = Vec::new();
    let mut overflow = false;
    loop {
        let t = match lx.next() {
            Ok(Some(t)) => t,
            Ok(None) => break,
            Err(folium_syntax::Error::Truncated) => break,
            Err(_) => {
                lx.pos += 1;
                args.clear();
                continue;
            }
        };
        match t {
            Token::Keyword(op) => {
                if op == b"BI" || op == b"ID" || op == b"EI" {
                    return Err(Error::NeedsRaster("inline image"));
                }
                if !overflow && OPERATORS.contains(&op) {
                    for a in &args {
                        w::write_object(out, a);
                        out.push(b' ');
                    }
                    out.extend_from_slice(op);
                    out.push(b'\n');
                    if out.len() > MAX_CONTENT_BYTES {
                        return Err(Error::NeedsRaster("content too large"));
                    }
                }
                args.clear();
                overflow = false;
            }
            t => match lx.object_from(t, 1) {
                // Content operands are direct objects; a reference has no
                // meaning there.
                Ok(Object::Ref(_)) => overflow = true,
                Ok(o) => {
                    if args.len() < MAX_ARGS {
                        args.push(o);
                    } else {
                        overflow = true;
                    }
                }
                Err(folium_syntax::Error::Truncated) => break,
                Err(_) => args.clear(),
            },
        }
    }
    Ok(())
}

struct Copier<'a, W: Write> {
    doc: &'a Doc,
    w: Writer<W>,
    map: HashMap<Ref, Ref>,
    copied: usize,
    cancel: &'a dyn Fn() -> bool,
}

fn type_dropped(d: &Dict) -> bool {
    DROP_TYPES.iter().any(|t| d.type_is(t))
}

impl<'a, W: Write> Copier<'a, W> {
    fn object(&mut self, o: &Object, depth: usize) -> Result<Object, Error> {
        if depth > MAX_DEPTH {
            return Err(Error::NeedsRaster("objects nested too deep"));
        }
        Ok(match o {
            Object::Ref(r) => self.reference(*r, depth + 1)?,
            Object::Array(a) => Object::Array(a.iter().map(|x| self.object(x, depth + 1)).collect::<Result<_, _>>()?),
            Object::Dict(d) => {
                if type_dropped(d) {
                    Object::Null
                } else {
                    Object::Dict(self.dict(d, depth + 1)?)
                }
            }
            // A direct stream can only be the value of an indirect object.
            Object::Stream(_) => Object::Null,
            other => other.clone(),
        })
    }

    fn dict(&mut self, d: &Dict, depth: usize) -> Result<Dict, Error> {
        if d.type_is("Font") && d.name("Subtype").is_some_and(|s| s.is("Type3")) {
            return Err(Error::NeedsRaster("Type 3 font"));
        }
        let mut out = Dict::new();
        for (k, v) in d.iter() {
            if DROP_KEYS.iter().any(|x| k.is(x)) {
                continue;
            }
            let v = self.object(v, depth + 1)?;
            out.0.push((k.clone(), v));
        }
        Ok(out)
    }

    fn reference(&mut self, r: Ref, depth: usize) -> Result<Object, Error> {
        if let Some(n) = self.map.get(&r) {
            return Ok(Object::Ref(*n));
        }
        if (self.cancel)() {
            return Err(Error::Cancelled);
        }
        self.copied += 1;
        if self.copied > MAX_OBJECTS {
            return Err(Error::NeedsRaster("too many objects"));
        }
        let o = match self.doc.syntax.get(r) {
            Ok(o) => o,
            Err(_) => return Ok(Object::Null),
        };
        if let Object::Dict(d) = &*o {
            if type_dropped(d) {
                return Ok(Object::Null);
            }
        }
        let n = self.w.reserve();
        self.map.insert(r, n);
        match &*o {
            Object::Stream(s) => self.stream(n, s, depth)?,
            other => {
                let c = self.object(other, depth)?;
                self.w.obj(n, &c)?;
            }
        }
        Ok(Object::Ref(n))
    }

    fn stream(&mut self, n: Ref, s: &Stream, depth: usize) -> Result<(), Error> {
        let dec = self.doc.syntax.decode(s).map_err(|e| Error::Document(format!("stream: {e:?}")))?;
        let mut dict = self.dict(&s.dict, depth + 1)?;
        for k in ["Filter", "DecodeParms", "Length", "DL", "F", "FFilter", "FDecodeParms"] {
            dict.0.retain(|(name, _)| !name.is(k));
        }
        let is_form = s.dict.name("Subtype").is_some_and(|t| t.is("Form"));
        let is_pattern = s.dict.int("PatternType") == Some(1);
        match dec.rest.as_slice() {
            [] if is_form || is_pattern => {
                let mut clean = Vec::new();
                sanitize_content(&dec.data, &mut clean)?;
                self.w.stream(n, dict, &clean, true)?;
            }
            [] => self.w.stream(n, dict, &dec.data, true)?,
            [stage] if stage.name.is("DCTDecode") || stage.name.is("DCT") => {
                dict.insert("Filter", Object::Name(Name::new(b"DCTDecode")));
                if let Some(p) = &stage.params {
                    let p = self.dict(p, depth + 1)?;
                    dict.insert("DecodeParms", Object::Dict(p));
                }
                self.w.stream(n, dict, &dec.data, false)?;
            }
            _ => return Err(Error::NeedsRaster("image codec other than JPEG")),
        }
        Ok(())
    }
}

/// Page space to sheet space (PDF, y up) for a page placed on the sheet.
fn sheet_matrix(view_matrix: Matrix, x: f64, y: f64, sheet_h: f64) -> Matrix {
    view_matrix.then(&Matrix::new(1.0, 0.0, 0.0, 1.0, x, y)).then(&Matrix::new(1.0, 0.0, 0.0, -1.0, 0.0, sheet_h))
}

/// Writes the rewritten PDF of `s.pages` laid out per `s` (resolution and
/// colour are the printer's business for PDF and are ignored here).
pub fn rewrite<W: Write>(doc: &Doc, s: &Settings, out: W, cancel: &dyn Fn() -> bool) -> Result<W, Error> {
    if s.pages.is_empty() {
        return Err(Error::Settings("no pages"));
    }
    if s.pages.iter().any(|p| *p >= doc.page_count()) {
        return Err(Error::Settings("page out of range"));
    }
    if !matches!(s.per_sheet, 1 | 2 | 4) {
        return Err(Error::Settings("pages per sheet"));
    }
    let (mw, mh) = media_size_pt(&s.media).ok_or(Error::Settings("media"))?;
    let mut c = Copier { doc, w: Writer::new(out, "1.7")?, map: HashMap::new(), copied: 0, cancel };
    let (catalog, pages_ref) = (c.w.reserve(), c.w.reserve());
    let grid = cells(mw, mh, s.per_sheet);
    // A source page used on two sheets (never, today) would get one form.
    let mut forms: HashMap<usize, Ref> = HashMap::new();
    let mut kids = Vec::new();
    for chunk in s.pages.chunks(s.per_sheet as usize) {
        let mut content = Vec::new();
        let mut xobjects = Dict::new();
        for (slot, &index) in chunk.iter().enumerate() {
            if cancel() {
                return Err(Error::Cancelled);
            }
            let page = doc.page(index).map_err(|e| Error::Document(format!("page {}: {e:?}", index + 1)))?;
            let form = match forms.get(&index) {
                Some(f) => *f,
                None => {
                    let f = page_form(&mut c, &page)?;
                    forms.insert(index, f);
                    f
                }
            };
            // The raster layout at 72 dpi is the layout in points; the
            // rounding there is to whole points, under a printer dot at
            // any resolution that matters.
            let p = place(&page, index, grid[slot], 72.0, s.scale);
            let m = sheet_matrix(p.view.matrix(), p.x as f64, p.y as f64, mh);
            let key = format!("P{slot}");
            xobjects.insert(&key, Object::Ref(form));
            content.extend(format!("q {} {} {} {} {} {} cm /{key} Do Q\n", w::num(m.a), w::num(m.b), w::num(m.c), w::num(m.d), w::num(m.e), w::num(m.f)).bytes());
        }
        let cref = c.w.reserve();
        c.w.stream(cref, Dict::new(), &content, true)?;
        let pref = c.w.reserve();
        let mut res = Dict::new();
        res.insert("XObject", Object::Dict(xobjects));
        let page = w::dict([
            ("Type", w::name("Page")),
            ("Parent", w::reference(pages_ref)),
            ("MediaBox", w::nums(&[0.0, 0.0, mw, mh])),
            ("Resources", Object::Dict(res)),
            ("Contents", w::reference(cref)),
        ]);
        c.w.obj(pref, &Object::Dict(page))?;
        kids.push(w::reference(pref));
    }
    let count = kids.len() as i64;
    c.w.obj(pages_ref, &Object::Dict(w::dict([("Type", w::name("Pages")), ("Kids", Object::Array(kids)), ("Count", w::int(count))])))?;
    let mut cat = w::dict([("Type", w::name("Catalog")), ("Pages", w::reference(pages_ref))]);
    // Optional content: which layers are on decides what prints.
    if let Some(oc) = doc.catalog().get("OCProperties").cloned() {
        let oc = c.object(&oc, 0)?;
        cat.insert("OCProperties", oc);
    }
    c.w.obj(catalog, &Object::Dict(cat))?;
    let (out, _) = c.w.finish(catalog, None)?;
    Ok(out)
}

/// A source page as a Form XObject: its contents re-tokenised, its
/// resources copied clean, clipped to its crop box. The form is in the
/// page's own user space; the placing matrix carries rotation, scale and the
/// user unit.
fn page_form<W: Write>(c: &mut Copier<W>, page: &Page) -> Result<Ref, Error> {
    let streams = c.doc.contents(page).map_err(|e| Error::Document(format!("page {}: {e:?}", page.index + 1)))?;
    let mut data = Vec::new();
    for st in &streams {
        let d = c.doc.syntax.decode(st).map_err(|e| Error::Document(format!("page {}: {e:?}", page.index + 1)))?;
        if !d.rest.is_empty() {
            return Err(Error::NeedsRaster("content stream with an image filter"));
        }
        data.extend_from_slice(&d.data);
        data.push(b'\n');
        if data.len() > MAX_CONTENT_BYTES {
            return Err(Error::NeedsRaster("content too large"));
        }
    }
    let mut clean = Vec::new();
    sanitize_content(&data, &mut clean)?;
    let res = c.dict(&page.resources, 1)?;
    let cb: Rect = Rect::new(page.crop_box.x0, page.crop_box.y0, page.crop_box.x1, page.crop_box.y1);
    let dict = w::dict([
        ("Type", w::name("XObject")),
        ("Subtype", w::name("Form")),
        ("BBox", w::nums(&[cb.x0, cb.y0, cb.x1, cb.y1])),
        ("Resources", Object::Dict(res)),
    ]);
    let r = c.w.reserve();
    c.w.stream(r, dict, &clean, true)?;
    Ok(r)
}
