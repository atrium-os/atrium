//! Floating pictures through .docx and .odt: wrap, place, gap.

use officina_convert::{docx, odt};
use officina_text::model::{CharAttrs, Float, HAlign, ImageData, ObjectRef, Rel, TextDoc, Wrap, OBJECT_CHAR};
use std::sync::Arc;

/// A small valid PNG, `w` × `h`, one colour.
pub fn png(w: u32, h: u32) -> Vec<u8> {
    let chunk = |out: &mut Vec<u8>, kind: &[u8], data: &[u8]| {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut c = kind.to_vec();
        c.extend_from_slice(data);
        out.extend_from_slice(&c);
        out.extend_from_slice(&officina_convert::zip::crc32(&c).to_be_bytes());
    };
    let mut raw = Vec::new();
    for _ in 0..h {
        raw.push(0);
        for _ in 0..w {
            raw.extend_from_slice(&[40, 120, 200]);
        }
    }
    let z = {
        let d = miniz_oxide::deflate::compress_to_vec_zlib(&raw, 6);
        d
    };
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

/// Paragraphs of text, each of the first five anchoring a picture with its
/// own wrap and place.
pub fn floats_doc() -> TextDoc {
    let mut d = TextDoc::new();
    d.blocks.remove(0);
    let bytes = png(64, 48);
    let img = d.images.add(ImageData { bytes: Arc::from(bytes), width: 64, height: 48 });
    let floats = [
        Float { wrap: Wrap::Square, h_rel: Rel::Text, h_align: HAlign::End, x: 0, v_rel: Rel::Text, y: 120, gap: 180 },
        Float { wrap: Wrap::Square, h_rel: Rel::Margin, h_align: HAlign::Start, x: 0, v_rel: Rel::Text, y: 0, gap: 144 },
        Float { wrap: Wrap::TopBottom, h_rel: Rel::Text, h_align: HAlign::Center, x: 0, v_rel: Rel::Text, y: 240, gap: 120 },
        Float { wrap: Wrap::Behind, h_rel: Rel::Page, h_align: HAlign::Offset, x: 1800, v_rel: Rel::Page, y: 7200, gap: 0 },
        Float { wrap: Wrap::InFront, h_rel: Rel::Margin, h_align: HAlign::Offset, x: 2880, v_rel: Rel::Margin, y: 4320, gap: 0 },
    ];
    let body = "Text goes round the picture this paragraph anchors, beside it, above and below it, or over it. ".repeat(4);
    for (i, f) in floats.iter().enumerate() {
        let a = d.attrs.intern(CharAttrs { object: Some(ObjectRef { image: img, width: 2160 - 200 * i as u32, height: 1440, float: Some(*f), shape: false }), ..Default::default() });
        let text = format!("{OBJECT_CHAR}{body}");
        d.push_para(&text, 0, vec![(0, a), (OBJECT_CHAR.len_utf8() as u32, 0)]);
    }
    d.push_para(&body, 0, vec![]);
    d
}

/// Each picture: its paragraph, frame and float.
pub fn shape(d: &TextDoc) -> Vec<(usize, u32, u32, Option<Float>)> {
    let mut v = Vec::new();
    for (k, b) in d.blocks.iter().enumerate() {
        let Some(p) = b.as_para() else { continue };
        for (s, _, a) in p.spans() {
            if let Some(o) = d.attrs.get(a).object {
                v.push((k, o.width, o.height, o.float));
                let _ = s;
            }
        }
    }
    v
}

#[test]
fn floats_round_trip() {
    let d = floats_doc();
    let want = shape(&d);
    assert_eq!(want.len(), 5);
    for fmt in ["docx", "odt"] {
        let (r, rep) = if fmt == "docx" { docx::read(&docx::write(&d, Vec::new()).unwrap()).unwrap() } else { odt::read(&odt::write(&d, Vec::new()).unwrap()).unwrap() };
        assert_eq!(shape(&r), want, "{fmt}");
        assert!(!format!("{:?}", rep.report).contains("float"), "{fmt}: no note about floats: {:?}", rep.report);
    }
}

/// The same pictures, sizes and offsets within 2 twips (LibreOffice rounds
/// through centimetres).
pub fn same(a: &[(usize, u32, u32, Option<Float>)], b: &[(usize, u32, u32, Option<Float>)]) -> bool {
    let near = |x: i64, y: i64| (x - y).abs() <= 2;
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| {
            x.0 == y.0
                && near(x.1 as i64, y.1 as i64)
                && near(x.2 as i64, y.2 as i64)
                && match (x.3, y.3) {
                    (Some(f), Some(g)) => f.wrap == g.wrap && f.h_rel == g.h_rel && f.h_align == g.h_align && f.v_rel == g.v_rel && near(f.x as i64, g.x as i64) && near(f.y as i64, g.y as i64) && near(f.gap as i64, g.gap as i64),
                    (f, g) => f == g,
                }
        })
}
