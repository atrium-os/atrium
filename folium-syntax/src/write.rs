//! Writing PDF (folium.md §5.1): the serializer and the file writer that
//! Officina's export uses and Folium's incremental save (P5) will. One
//! escaping path for numbers, strings and names, tested once, read back by
//! the same lexer that reads hostile files.
//!
//! [`Writer`] streams a file: objects go out as they are made, offsets are
//! kept, and [`Writer::finish`] writes the cross-reference table and the
//! trailer. Numbers are written with at most three decimals (a thousandth
//! of a point is far below a device pixel).

use std::io::{self, Write};

use crate::object::{Dict, Name, Object, Ref};

/// `v` with at most three decimals and no trailing zeros; NaN and the
/// infinities (which PDF has no spelling for) are written as 0.
pub fn num(v: f64) -> String {
    if !v.is_finite() {
        return "0".into();
    }
    let r = (v * 1000.0).round() / 1000.0;
    if r == r.trunc() && r.abs() < 9.0e15 {
        format!("{}", r as i64)
    } else {
        let s = format!("{r:.3}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// A name's spelling: `/` then the bytes, delimiters, whitespace, `#` and
/// anything outside `!`..`~` as `#xx`.
pub fn write_name(out: &mut Vec<u8>, n: &[u8]) {
    out.push(b'/');
    for &b in n {
        if (b'!'..=b'~').contains(&b) && !b"()<>[]{}/%#".contains(&b) {
            out.push(b);
        } else {
            out.extend(format!("#{b:02X}").bytes());
        }
    }
}

/// A string: literal with escapes when it is mostly printable, hex
/// otherwise.
pub fn write_string(out: &mut Vec<u8>, s: &[u8]) {
    let printable = s.iter().filter(|&&b| (0x20..0x7f).contains(&b)).count();
    if s.len() > 8 && printable * 4 < s.len() * 3 {
        out.push(b'<');
        for b in s {
            out.extend(format!("{b:02X}").bytes());
        }
        out.push(b'>');
        return;
    }
    out.push(b'(');
    for &b in s {
        match b {
            b'(' | b')' | b'\\' => out.extend([b'\\', b]),
            b'\n' => out.extend(b"\\n"),
            b'\r' => out.extend(b"\\r"),
            b'\t' => out.extend(b"\\t"),
            0x20..=0x7e => out.push(b),
            _ => out.extend(format!("\\{b:03o}").bytes()),
        }
    }
    out.push(b')');
}

/// A text string (§7.9.2.2): PDFDocEncoding's ASCII part as it is,
/// anything else as UTF-16BE with a byte-order mark.
pub fn text_string(s: &str) -> Object {
    if s.bytes().all(|b| (0x20..0x7f).contains(&b)) {
        return Object::String(s.as_bytes().into());
    }
    let mut b = vec![0xFE, 0xFF];
    for u in s.encode_utf16() {
        b.extend(u.to_be_bytes());
    }
    Object::String(b.into())
}

/// An object's spelling. A stream here is written as its dictionary only;
/// [`Writer::stream`] writes streams.
pub fn write_object(out: &mut Vec<u8>, o: &Object) {
    match o {
        Object::Null => out.extend(b"null"),
        Object::Bool(b) => out.extend(if *b { &b"true"[..] } else { b"false" }),
        Object::Int(i) => out.extend(i.to_string().bytes()),
        Object::Real(r) => out.extend(num(*r).bytes()),
        Object::String(s) => write_string(out, s),
        Object::Name(n) => write_name(out, n.as_bytes()),
        Object::Array(a) => {
            out.push(b'[');
            for (i, v) in a.iter().enumerate() {
                if i > 0 {
                    out.push(b' ');
                }
                write_object(out, v);
            }
            out.push(b']');
        }
        Object::Dict(d) => write_dict(out, d),
        Object::Stream(s) => write_dict(out, &s.dict),
        Object::Ref(r) => out.extend(format!("{} {} R", r.num, r.gen).bytes()),
    }
}

pub fn write_dict(out: &mut Vec<u8>, d: &Dict) {
    out.extend(b"<<");
    for (k, v) in d.iter() {
        write_name(out, k.as_bytes());
        out.push(b' ');
        write_object(out, v);
    }
    out.extend(b">>");
}

/// Shorthands for building objects to write.
pub fn name(s: &str) -> Object { Object::Name(Name::new(s.as_bytes())) }
pub fn real(v: f64) -> Object { Object::Real(v) }
pub fn int(v: i64) -> Object { Object::Int(v) }
pub fn reference(r: Ref) -> Object { Object::Ref(r) }
pub fn array(v: impl IntoIterator<Item = Object>) -> Object { Object::Array(v.into_iter().collect()) }
/// A rectangle or any list of numbers.
pub fn nums(v: &[f64]) -> Object { Object::Array(v.iter().map(|&x| Object::Real(x)).collect()) }
pub fn dict(entries: impl IntoIterator<Item = (&'static str, Object)>) -> Dict {
    let mut d = Dict::new();
    for (k, v) in entries {
        d.insert(k, v);
    }
    d
}

/// Writes a PDF file object by object.
pub struct Writer<W: Write> {
    w: W,
    at: u64,
    offsets: Vec<u64>,
    buf: Vec<u8>,
}

impl<W: Write> Writer<W> {
    /// Starts a file: the header and the binary comment.
    pub fn new(w: W, version: &str) -> io::Result<Self> {
        let mut x = Writer { w, at: 0, offsets: Vec::new(), buf: Vec::new() };
        x.raw(format!("%PDF-{version}\n").as_bytes())?;
        x.raw(b"%\xE2\xE3\xCF\xD3\n")?;
        Ok(x)
    }

    fn raw(&mut self, b: &[u8]) -> io::Result<()> {
        self.w.write_all(b)?;
        self.at += b.len() as u64;
        Ok(())
    }

    /// A fresh object number, to refer to before the object is written.
    pub fn reserve(&mut self) -> Ref {
        self.offsets.push(0);
        Ref::new(self.offsets.len() as u32, 0)
    }

    /// Bytes written so far.
    pub fn position(&self) -> u64 { self.at }

    fn begin(&mut self, r: Ref) {
        self.offsets[r.num as usize - 1] = self.at;
        self.buf.clear();
        self.buf.extend(format!("{} 0 obj\n", r.num).bytes());
    }

    pub fn obj(&mut self, r: Ref, o: &Object) -> io::Result<()> {
        self.begin(r);
        let mut b = std::mem::take(&mut self.buf);
        write_object(&mut b, o);
        b.extend(b"\nendobj\n");
        let res = self.raw(&b);
        self.buf = b;
        res
    }

    /// A stream object, its data deflated when `deflate` (the filter and the
    /// length are added to `dict`).
    pub fn stream(&mut self, r: Ref, mut dict: Dict, data: &[u8], deflate: bool) -> io::Result<()> {
        let z;
        let data = if deflate {
            z = miniz_oxide::deflate::compress_to_vec_zlib(data, 6);
            dict.insert("Filter", name("FlateDecode"));
            &z[..]
        } else {
            data
        };
        dict.insert("Length", int(data.len() as i64));
        self.begin(r);
        let mut b = std::mem::take(&mut self.buf);
        write_dict(&mut b, &dict);
        b.extend(b"\nstream\n");
        let res = self.raw(&b).and_then(|_| self.raw(data)).and_then(|_| self.raw(b"\nendstream\nendobj\n"));
        self.buf = b;
        res
    }

    /// The cross-reference table and the trailer; the writer back and the
    /// file's length.
    pub fn finish(mut self, root: Ref, info: Option<Ref>) -> io::Result<(W, u64)> {
        let xref = self.at;
        let mut x = format!("xref\n0 {}\n0000000000 65535 f \n", self.offsets.len() + 1);
        for off in &self.offsets {
            x.push_str(&format!("{off:010} 00000 n \n"));
        }
        let mut trailer = dict([("Size", int(self.offsets.len() as i64 + 1)), ("Root", reference(root))]);
        if let Some(i) = info {
            trailer.insert("Info", reference(i));
        }
        let mut b = x.into_bytes();
        b.extend(b"trailer\n");
        write_dict(&mut b, &trailer);
        b.extend(format!("\nstartxref\n{xref}\n%%EOF\n").bytes());
        self.raw(&b)?;
        self.w.flush()?;
        Ok((self.w, self.at))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Document;

    #[test]
    fn numbers_names_strings() {
        assert_eq!(num(1.0), "1");
        assert_eq!(num(-0.5), "-0.5");
        assert_eq!(num(12.3456), "12.346");
        assert_eq!(num(0.0004), "0");
        assert_eq!(num(f64::NAN), "0");
        let mut b = vec![];
        write_name(&mut b, b"A B#(c)");
        assert_eq!(b, b"/A#20B#23#28c#29");
        b.clear();
        write_string(&mut b, b"a(b)\\\n\x01");
        assert_eq!(b, b"(a\\(b\\)\\\\\\n\\001)");
        b.clear();
        write_string(&mut b, &[0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
        assert_eq!(b, b"<00010203040506070809>");
        assert_eq!(text_string("plain"), Object::String(b"plain"[..].into()));
        assert_eq!(text_string("é"), Object::String(vec![0xFE, 0xFF, 0, 0xE9].into()));
    }

    /// What the writer writes, the reader reads back.
    #[test]
    fn round_trip_through_the_reader() {
        let mut w = Writer::new(Vec::new(), "1.7").unwrap();
        let (cat, pages, page, content, info) = (w.reserve(), w.reserve(), w.reserve(), w.reserve(), w.reserve());
        w.stream(content, Dict::new(), b"0 0 1 rg 10 10 50 50 re f", true).unwrap();
        w.obj(page, &Object::Dict(dict([("Type", name("Page")), ("Parent", reference(pages)), ("MediaBox", nums(&[0.0, 0.0, 200.0, 100.5])), ("Contents", reference(content))]))).unwrap();
        w.obj(pages, &Object::Dict(dict([("Type", name("Pages")), ("Kids", array([reference(page)])), ("Count", int(1))]))).unwrap();
        w.obj(cat, &Object::Dict(dict([("Type", name("Catalog")), ("Pages", reference(pages))]))).unwrap();
        w.obj(info, &Object::Dict(dict([("Title", text_string("Ünïcode (title)")), ("Odd name", name("a b"))]))).unwrap();
        let (bytes, len) = w.finish(cat, Some(info)).unwrap();
        assert_eq!(len as usize, bytes.len());
        let d = Document::open(Box::new(crate::Bytes(bytes.into()))).unwrap();
        let i = d.resolve(&Object::Ref(info)).unwrap();
        let i = i.as_dict().unwrap();
        assert_eq!(i.get("Title").and_then(Object::as_str), Some(&[0xFE, 0xFF, 0, 0xDC, 0, 0x6E, 0, 0xEF, 0, 0x63, 0, 0x6F, 0, 0x64, 0, 0x65, 0, 0x20, 0, 0x28, 0, 0x74, 0, 0x69, 0, 0x74, 0, 0x6C, 0, 0x65, 0, 0x29][..]));
        assert!(i.get("Odd name").and_then(Object::as_name).is_some_and(|n| n.is("a b")));
        let s = d.resolve(&Object::Ref(content)).unwrap();
        let data = d.decode(s.as_stream().unwrap()).unwrap();
        assert_eq!(&data.data[..], b"0 0 1 rg 10 10 50 50 re f");
    }
}
