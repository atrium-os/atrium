//! Whole files: classic tables, cross-reference streams with object
//! streams, incremental updates, broken cross-references, encryption.

use folium_syntax::{Document, Error, Object, Ref};

const CONTENT: &[u8] = b"BT /F1 24 Tf 72 700 Td (Folium secret) Tj ET";

/// A PDF with a classic table: catalog, pages, page, content.
fn classic(objs: &[&[u8]], break_offsets: bool) -> Vec<u8> {
    let mut f = b"%PDF-1.4\n%\xe2\xe3\xcf\xd3\n".to_vec();
    let mut offs = Vec::new();
    for (i, o) in objs.iter().enumerate() {
        offs.push(f.len());
        f.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
        f.extend_from_slice(o);
        f.extend_from_slice(b"\nendobj\n");
    }
    let x = f.len();
    f.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).as_bytes());
    for o in offs {
        let o = if break_offsets { o + 3 } else { o };
        f.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
    }
    f.extend_from_slice(format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{x}\n%%EOF\n", objs.len() + 1).as_bytes());
    f
}

fn content_obj() -> Vec<u8> {
    let mut v = format!("<< /Length {} >>\nstream\n", CONTENT.len()).into_bytes();
    v.extend_from_slice(CONTENT);
    v.extend_from_slice(b"\nendstream");
    v
}

fn basic(break_offsets: bool) -> Vec<u8> {
    let c = content_obj();
    classic(
        &[
            b"<< /Type /Catalog /Pages 2 0 R >>",
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R >>",
            &c,
        ],
        break_offsets,
    )
}

fn page_content(d: &Document) -> Vec<u8> {
    let cat = d.catalog().unwrap();
    let pages = d.get_in(&cat, "Pages").unwrap();
    let kids = d.get_in(pages.as_dict().unwrap(), "Kids").unwrap();
    let page = d.resolve(&kids.as_array().unwrap()[0]).unwrap();
    let c = d.get_in(page.as_dict().unwrap(), "Contents").unwrap();
    d.decode(c.as_stream().unwrap()).unwrap().data
}

#[test]
fn classic_table() {
    let d = Document::from_bytes(basic(false)).unwrap();
    assert!(!d.repaired());
    assert_eq!(d.version, (1, 4));
    assert_eq!(page_content(&d), CONTENT);
    // A reference to nothing is null.
    assert_eq!(*d.get(Ref::new(99, 0)).unwrap(), Object::Null);
}

#[test]
fn broken_offsets_repair() {
    // Every offset is 3 bytes off: the root doesn't parse, so the file is
    // opened by the scan.
    let d = Document::from_bytes(basic(true)).unwrap();
    assert!(d.repaired());
    assert_eq!(page_content(&d), CONTENT);
}

#[test]
fn no_xref_at_all() {
    let mut f = basic(false);
    let x = folium_syntax::xref::rfind(&f, b"xref\n0").unwrap();
    f.truncate(x);
    let d = Document::from_bytes(f).unwrap();
    assert!(d.repaired());
    assert_eq!(page_content(&d), CONTENT);
}

#[test]
fn wrong_length_is_found() {
    let f = basic(false);
    let s = String::from_utf8_lossy(&f).replace(&format!("/Length {}", CONTENT.len()), "/Length 9999");
    let d = Document::from_bytes(s.into_bytes()).unwrap();
    assert_eq!(page_content(&d), CONTENT);
}

#[test]
fn indirect_length() {
    let mut c = b"<< /Length 5 0 R >>\nstream\n".to_vec();
    c.extend_from_slice(CONTENT);
    c.extend_from_slice(b"\nendstream");
    let n = CONTENT.len().to_string();
    let f = classic(
        &[
            b"<< /Type /Catalog /Pages 2 0 R >>",
            b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R >>",
            &c,
            n.as_bytes(),
        ],
        false,
    );
    let d = Document::from_bytes(f).unwrap();
    assert_eq!(page_content(&d), CONTENT);
}

/// A cross-reference stream (Flate, PNG Up predictor) with the page
/// objects in an object stream.
fn with_objstm() -> Vec<u8> {
    use miniz_oxide::deflate::compress_to_vec_zlib;
    let mut f = b"%PDF-1.5\n".to_vec();
    let inner: [&[u8]; 3] = [
        b"<< /Type /Catalog /Pages 2 0 R >>",
        b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
        b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R >>",
    ];
    let mut head = String::new();
    let mut body = Vec::new();
    for (i, o) in inner.iter().enumerate() {
        head += &format!("{} {} ", i + 1, body.len());
        body.extend_from_slice(o);
        body.push(b' ');
    }
    let mut stm = head.clone().into_bytes();
    stm.extend_from_slice(&body);
    let z = compress_to_vec_zlib(&stm, 6);
    let o5 = f.len();
    f.extend_from_slice(format!("5 0 obj\n<< /Type /ObjStm /N 3 /First {} /Filter /FlateDecode /Length {} >>\nstream\n", head.len(), z.len()).as_bytes());
    f.extend_from_slice(&z);
    f.extend_from_slice(b"\nendstream\nendobj\n");
    let o4 = f.len();
    f.extend_from_slice(b"4 0 obj\n");
    f.extend_from_slice(&content_obj());
    f.extend_from_slice(b"\nendobj\n");
    let o6 = f.len();
    // Records: type(1) field2(2) field3(1), PNG Up rows.
    let recs: Vec<[u8; 4]> = vec![
        [0, 0, 0, 0],
        [2, 0, 5, 0],
        [2, 0, 5, 1],
        [2, 0, 5, 2],
        [1, (o4 >> 8) as u8, o4 as u8, 0],
        [1, (o5 >> 8) as u8, o5 as u8, 0],
        [1, (o6 >> 8) as u8, o6 as u8, 0],
    ];
    let mut raw = Vec::new();
    let mut prev = [0u8; 4];
    for r in &recs {
        raw.push(2);
        for j in 0..4 {
            raw.push(r[j].wrapping_sub(prev[j]));
        }
        prev = *r;
    }
    let z = compress_to_vec_zlib(&raw, 6);
    f.extend_from_slice(
        format!(
            "6 0 obj\n<< /Type /XRef /Size 7 /W [1 2 1] /Root 1 0 R /Filter /FlateDecode /DecodeParms << /Predictor 12 /Columns 4 >> /Length {} >>\nstream\n",
            z.len()
        )
        .as_bytes(),
    );
    f.extend_from_slice(&z);
    f.extend_from_slice(format!("\nendstream\nendobj\nstartxref\n{o6}\n%%EOF\n").as_bytes());
    f
}

#[test]
fn xref_stream_and_objstm() {
    let d = Document::from_bytes(with_objstm()).unwrap();
    assert!(!d.repaired());
    assert_eq!(d.version, (1, 5));
    assert_eq!(page_content(&d), CONTENT);
}

#[test]
fn objstm_by_repair() {
    let mut f = with_objstm();
    let x = folium_syntax::xref::rfind(&f, b"6 0 obj").unwrap();
    f.truncate(x);
    // The xref stream is gone, so there's no trailer: the catalog in the
    // object stream can't be seen by the scan, and the file is unreadable
    // unless a trailer names it. Add one.
    f.extend_from_slice(b"trailer\n<< /Root 1 0 R >>\n%%EOF\n");
    let d = Document::from_bytes(f).unwrap();
    assert!(d.repaired());
    assert_eq!(page_content(&d), CONTENT);
}

#[test]
fn incremental_update_wins() {
    let mut f = basic(false);
    let prev = folium_syntax::xref::startxref(&folium_syntax::Bytes(f.clone().into())).unwrap();
    let o = f.len();
    let new = b"BT (updated) Tj ET";
    f.extend_from_slice(format!("4 0 obj\n<< /Length {} >>\nstream\n", new.len()).as_bytes());
    f.extend_from_slice(new);
    f.extend_from_slice(b"\nendstream\nendobj\n");
    let x = f.len();
    f.extend_from_slice(format!("xref\n4 1\n{o:010} 00000 n \ntrailer\n<< /Size 5 /Root 1 0 R /Prev {prev} >>\nstartxref\n{x}\n%%EOF\n").as_bytes());
    let d = Document::from_bytes(f).unwrap();
    assert_eq!(page_content(&d), new);
}

#[test]
fn prev_loop_stops() {
    let f = basic(false);
    let x = folium_syntax::xref::startxref(&folium_syntax::Bytes(f.clone().into())).unwrap();
    let s = String::from_utf8_lossy(&f).replace("/Root 1 0 R", &format!("/Root 1 0 R /Prev {x}"));
    let d = Document::from_bytes(s.into_bytes()).unwrap();
    assert_eq!(page_content(&d), CONTENT);
}

#[test]
fn garbage_is_an_error_not_a_panic() {
    for f in [&b""[..], b"%PDF-1.7\n", b"%PDF-1.7\nstartxref\n999999\n%%EOF", b"not a pdf at all"] {
        assert!(Document::from_bytes(f.to_vec()).is_err());
    }
}

fn fixture(name: &str) -> Vec<u8> { std::fs::read(format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap() }

fn title(d: &Document) -> Vec<u8> {
    let info = d.get_in(d.trailer(), "Info").unwrap();
    d.get_in(info.as_dict().unwrap(), "Title").unwrap().as_str().unwrap().to_vec()
}

#[test]
fn encrypted_empty_user_password() {
    for n in ["rc4-40", "rc4-128", "aes-128", "aes-256"] {
        let d = Document::from_bytes(fixture(&format!("enc-{n}.pdf"))).unwrap_or_else(|e| panic!("{n}: {e}"));
        assert_eq!(page_content(&d), CONTENT, "{n}");
        assert_eq!(title(&d), b"Folium title", "{n}");
        assert!(!d.security().unwrap().owner);
    }
}

#[test]
fn encrypted_with_password() {
    for n in ["rc4-128-user", "aes-256-user"] {
        let f = fixture(&format!("enc-{n}.pdf"));
        assert_eq!(Document::from_bytes(f.clone()).err(), Some(Error::NeedsPassword), "{n}");
        let src = || Box::new(folium_syntax::Bytes(f.clone().into()));
        assert_eq!(Document::open_with_password(src(), b"wrong").err(), Some(Error::NeedsPassword), "{n}");
        let d = Document::open_with_password(src(), b"secret").unwrap();
        assert_eq!(page_content(&d), CONTENT, "{n}");
        assert!(!d.security().unwrap().owner);
        let d = Document::open_with_password(src(), b"owner").unwrap();
        assert_eq!(title(&d), b"Folium title", "{n}");
        assert!(d.security().unwrap().owner);
    }
}
