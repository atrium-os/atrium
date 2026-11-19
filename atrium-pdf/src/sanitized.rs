//! Full-page export has a stricter input profile than interactive preview.
//! Refuse unsupported paint before interpreting it. Never export viewer tiles
//! (which can contain placeholders) or copy source objects to the writer.
use folium_content::{Interp, Matrix, Rect, Shared};
use folium_doc::Doc;
use folium_render::{Renderer, View};
use folium_syntax::lexer::{Lexer, Token};
use folium_syntax::write::{self as w, Writer};
use folium_syntax::{Dict, Error, Object, Result, Source};
use std::time::{Duration, Instant};

const UNSUPPORTED: Error = Error::Unsupported("incomplete visual copy");
const PIXELS: u64 = 24_000_000;
const SCALE: f64 = 2.0; // 144 dpi

fn require(ok: bool) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(UNSUPPORTED)
    }
}

fn allowed(d: &Dict, keys: &[&str]) -> bool {
    d.iter()
        .all(|(name, _)| keys.iter().any(|key| name.is(key)))
}

fn resource(doc: &Doc, res: &Dict, kind: &str, arg: &Object) -> Result<Object> {
    let name = arg.as_name().ok_or(UNSUPPORTED)?;
    let group = doc.syntax.get_in(res, kind)?;
    let (_, obj) = group
        .as_dict()
        .ok_or(UNSUPPORTED)?
        .iter()
        .find(|(k, _)| *k == name)
        .ok_or(UNSUPPORTED)?;
    doc.syntax.resolve(obj)
}

fn image(doc: &Doc, obj: Object) -> Result<()> {
    let s = obj.as_stream().ok_or(UNSUPPORTED)?;
    let d = &s.dict;
    require(allowed(
        d,
        &[
            "Type",
            "Subtype",
            "Width",
            "Height",
            "BitsPerComponent",
            "ColorSpace",
            "Length",
            "Filter",
        ],
    ))?;
    require(
        d.name("Subtype").is_some_and(|n| n.is("Image")) && d.int("BitsPerComponent") == Some(8),
    )?;
    let channels = match d.name("ColorSpace").map(|n| n.as_bytes()) {
        Some(b"DeviceRGB") => 3,
        Some(b"DeviceGray") => 1,
        _ => return Err(UNSUPPORTED),
    };
    let dim = |k| {
        d.int(k)
            .filter(|v| (1..=8192).contains(v))
            .ok_or(UNSUPPORTED)
    };
    let pixels = dim("Width")? * dim("Height")?;
    require(
        pixels <= 8_000_000
            && d.get("Filter")
                .is_none_or(|f| f.as_name().is_some_and(|n| n.is("FlateDecode"))),
    )?;
    let decoded = doc.syntax.decode(s)?;
    require(decoded.rest.is_empty() && decoded.data.len() == (pixels * channels) as usize)
}

// Validate operand shapes as well as operators. The preview interpreter supplies
// defaults for missing operands and substitutes fonts, colours and images.
fn check_paint(doc: &Doc, res: &Dict, data: &[u8], deadline: Instant) -> Result<()> {
    let mut lex = Lexer::new(data, 0);
    let mut args = Vec::<Object>::new();
    let (mut text, mut font, mut saved, mut path) = (false, false, false, false);
    let mut operations = 0;
    while let Some(t) = lex.next()? {
        let Token::Keyword(op) = t else {
            require(
                args.len() < 6
                    && matches!(
                        t,
                        Token::Int(_) | Token::Real(_) | Token::Name(_) | Token::String(_)
                    ),
            )?;
            args.push(lex.object_from(t, 0)?);
            continue;
        };
        operations += 1;
        require(operations <= 32_000 && Instant::now() < deadline)?;
        let nums = |n| {
            args.len() == n
                && args
                    .iter()
                    .all(|o| o.as_f64().is_some_and(|x| x.abs() <= 32768.0))
        };
        match op {
            b"q" if !saved && !text && !path && nums(0) => saved = true,
            b"Q" if saved && !text && !path && nums(0) => saved = false,
            b"cm"
                if !text
                    && !path
                    && args.iter().map(Object::as_f64).eq([
                        Some(1.),
                        Some(0.),
                        Some(0.),
                        Some(1.),
                        Some(0.),
                        Some(0.),
                    ]) => {}
            b"cm" if saved && !text && !path && nums(6) => {
                // Axis-aligned image placement. Disallow unbounded products.
                require(args[1].as_f64() == Some(0.0) && args[2].as_f64() == Some(0.0))?;
                let Some(Token::Name(name)) = lex.next()? else {
                    return Err(UNSUPPORTED);
                };
                lex.keyword(b"Do")?;
                image(doc, resource(doc, res, "XObject", &Object::Name(name))?)?;
                lex.keyword(b"Q")?;
                saved = false;
            }
            b"rg" | b"g" | b"RG" | b"G" if nums(if op.len() == 1 { 1 } else { 3 }) => {
                require(
                    args.iter()
                        .all(|o| (0.0..=1.0).contains(&o.as_f64().unwrap())),
                )?;
            }
            b"w" if !text && nums(1) && args[0].as_f64() >= Some(0.) => {}
            b"m" if !text && nums(2) => path = true,
            b"l" if path && nums(2) => {}
            b"c" if path && nums(6) => {}
            b"h" if path && nums(0) => {}
            b"re" if !text && !path && nums(4) => path = true,
            b"f" | b"f*" | b"S" | b"s" if path && nums(0) => path = false,
            b"BT" if !text && !path && nums(0) => {
                text = true;
                font = false;
            }
            b"ET" if text && nums(0) => text = false,
            b"Tf" if text && args.len() == 2 => {
                let obj = resource(doc, res, "Font", &args[0])?;
                let d = obj.as_dict().ok_or(UNSUPPORTED)?;
                require(
                    allowed(d, &["Type", "Subtype", "BaseFont", "Encoding"])
                        && d.name("Subtype").is_some_and(|n| n.is("Type1"))
                        && d.name("BaseFont").is_some_and(|n| {
                            n.is("Helvetica") || n.is("Times-Roman") || n.is("Courier")
                        })
                        && d.get("Encoding").is_none_or(|o| {
                            o.as_name().is_some_and(|n| {
                                n.is("WinAnsiEncoding") || n.is("StandardEncoding")
                            })
                        })
                        && args[1].as_f64().is_some_and(|n| n > 0.0 && n <= 1024.0),
                )?;
                font = true;
            }
            b"TL" if text && nums(1) => {}
            b"T*" if text && nums(0) => {}
            b"Td" if text && nums(2) => {}
            b"Tm"
                if text
                    && nums(6)
                    && args[..4].iter().map(Object::as_f64).eq([
                        Some(1.),
                        Some(0.),
                        Some(0.),
                        Some(1.),
                    ]) => {}
            b"Tj" if text && font && args.len() == 1 => {
                require(
                    args[0]
                        .as_str()
                        .is_some_and(|s| s.iter().all(|c| (32..=126).contains(c))),
                )?;
            }
            _ => return Err(UNSUPPORTED),
        }
        args.clear();
    }
    require(args.is_empty() && !text && !saved && !path)
}

// The reader accepts damaged or missing geometry and repairs page trees.
// Validate the declared tree and inherited values before using its page view.
fn validate_tree(
    doc: &Doc,
    node: &Object,
    depth: usize,
    mut boxes: [Option<Rect>; 2],
    seen: &mut std::collections::HashSet<folium_syntax::Ref>,
) -> Result<usize> {
    require(depth <= 32 && seen.len() < 256 && seen.insert(node.as_ref().ok_or(UNSUPPORTED)?))?;
    let obj = doc.syntax.resolve(node)?;
    let d = obj.as_dict().ok_or(UNSUPPORTED)?;
    for (slot, key) in boxes.iter_mut().zip(["MediaBox", "CropBox"]) {
        if let Some(value) = d.get(key) {
            let value = doc.syntax.resolve(value)?;
            let coords = value.as_array().ok_or(UNSUPPORTED)?;
            require(
                coords.len() == 4
                    && coords
                        .iter()
                        .all(|v| v.as_f64().is_some_and(|n| n.abs() < 32768.0)),
            )?;
            let at = |i: usize| coords[i].as_f64().unwrap();
            require(at(0) < at(2) && at(1) < at(3))?;
            *slot = Some(Rect::new(at(0), at(1), at(2), at(3)));
        }
    }
    if let Some(angle) = d.get("Rotate") {
        require(
            doc.syntax
                .resolve(angle)?
                .as_int()
                .is_some_and(|n| n % 90 == 0),
        )?;
    }
    if let Some(res) = d.get("Resources") {
        require(doc.syntax.resolve(res)?.as_dict().is_some())?;
    }
    if let Some(unit) = d.get("UserUnit") {
        require(unit.as_f64().is_some_and(|n| n > 0.0 && n <= 75000.0))?;
    }
    if d.type_is("Page") {
        let media = boxes[0].ok_or(UNSUPPORTED)?;
        let crop = boxes[1].unwrap_or(media);
        require(
            media.x0.max(crop.x0) < media.x1.min(crop.x1)
                && media.y0.max(crop.y0) < media.y1.min(crop.y1)
                && d.get("Kids").is_none(),
        )?;
        return Ok(1);
    }
    require(d.type_is("Pages"))?;
    let kids = doc.syntax.get_in(d, "Kids")?;
    let mut total = 0;
    for kid in kids.as_array().ok_or(UNSUPPORTED)? {
        total += validate_tree(doc, kid, depth + 1, boxes, seen)?;
    }
    require(d.int("Count") == Some(total as i64))?;
    Ok(total)
}

pub fn export(source: Box<dyn Source>) -> Result<Vec<u8>> {
    require(source.len() <= 16 << 20)?;
    let tail = source.read_vec(source.len().saturating_sub(1024), 1024)?;
    require(tail.trim_ascii_end().ends_with(b"%%EOF"))?;
    let doc = Doc::from_syntax(folium_syntax::Document::open_for_export(source)?)?;
    require(!doc.tree_damaged && !doc.syntax.repaired() && (1..=64).contains(&doc.page_count()))?;
    let catalog = doc.catalog();
    require(
        validate_tree(
            &doc,
            catalog.get("Pages").ok_or(UNSUPPORTED)?,
            0,
            [None, None],
            &mut Default::default(),
        )? == doc.page_count(),
    )?;
    require(
        [
            "AcroForm",
            "OCProperties",
            "OutputIntents",
            "NeedsRendering",
            "Perms",
        ]
        .iter()
        .all(|k| catalog.get(k).is_none()),
    )?;
    let mut pdf = Writer::new(Vec::new(), "1.4")?;
    let root = pdf.reserve();
    let tree = pdf.reserve();
    let mut kids = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    let expired = || Instant::now() >= deadline;
    let mut pixels = 0;
    for i in 0..doc.page_count() {
        let p = doc.page(i)?;
        require(
            !expired()
                && ["Annots", "Group", "PresSteps", "VP", "SeparationInfo"]
                    .iter()
                    .all(|k| p.dict.get(k).is_none()),
        )?;
        require(allowed(&p.resources, &["Font", "ProcSet", "XObject"]))?;
        let (w, h) = p.size();
        let (pw, ph) = ((w * SCALE).ceil() as u32, (h * SCALE).ceil() as u32);
        require((1..=8192).contains(&pw) && (1..=8192).contains(&ph))?;
        let area = u64::from(pw) * u64::from(ph);
        pixels += area;
        require(area <= 8_000_000 && pixels <= PIXELS)?;
        let streams = doc.contents(&p)?;
        let contents = doc.syntax.get_in(&p.dict, "Contents")?;
        let count = match contents {
            Object::Array(a) => a.len(),
            Object::Stream(_) => 1,
            Object::Null if p.dict.get("Contents").is_none() => 0,
            _ => return Err(UNSUPPORTED),
        };
        require(streams.len() == count && count <= 64)?;
        let mut data = Vec::new();
        for stream in &streams {
            require(
                allowed(&stream.dict, &["Length", "Filter"])
                    && stream
                        .dict
                        .get("Filter")
                        .is_none_or(|f| f.as_name().is_some_and(|n| n.is("FlateDecode"))),
            )?;
            let decoded = doc.syntax.decode(stream)?;
            require(decoded.rest.is_empty() && data.len() + decoded.data.len() <= 1 << 20)?;
            data.extend(decoded.data);
            data.push(b'\n');
        }
        check_paint(&doc, &p.resources, &data, deadline)?;
        let crop = Rect::new(p.crop_box.x0, p.crop_box.y0, p.crop_box.x1, p.crop_box.y1);
        let shared = Shared::new();
        let mut interpreter =
            Interp::new(&doc.syntax, &shared, Matrix::IDENTITY, Some(crop), &expired);
        interpreter.keep_fills_separate();
        interpreter.run(&data, &p.resources)?;
        let list = interpreter.finish();
        require(list.skipped.is_empty() && !expired())?;
        let view = View {
            crop,
            rotate: p.rotate,
            scale: SCALE * p.user_unit,
        };
        let (bitmap, stats) =
            Renderer::default().render(&doc.syntax, &list, &view, (0, 0, pw, ph), &expired);
        require(stats.images_failed == 0 && !expired())?;
        let rgb: Vec<u8> = bitmap
            .data()
            .chunks_exact(4)
            .flat_map(|px| px[..3].iter().copied())
            .collect();
        let img = pdf.reserve();
        pdf.stream(
            img,
            w::dict([
                ("Type", w::name("XObject")),
                ("Subtype", w::name("Image")),
                ("Width", w::int(pw.into())),
                ("Height", w::int(ph.into())),
                ("BitsPerComponent", w::int(8)),
                ("ColorSpace", w::name("DeviceRGB")),
            ]),
            &rgb,
            true,
        )?;
        let content = pdf.reserve();
        pdf.stream(
            content,
            Dict::new(),
            format!("q {} 0 0 {} 0 0 cm /PageImage Do Q\n", w::num(w), w::num(h)).as_bytes(),
            false,
        )?;
        let page = pdf.reserve();
        let resources = w::dict([(
            "XObject",
            Object::Dict(w::dict([("PageImage", w::reference(img))])),
        )]);
        pdf.obj(
            page,
            &Object::Dict(w::dict([
                ("Type", w::name("Page")),
                ("Parent", w::reference(tree)),
                ("MediaBox", w::nums(&[0., 0., w, h])),
                ("Resources", Object::Dict(resources)),
                ("Contents", w::reference(content)),
            ])),
        )?;
        kids.push(w::reference(page));
    }
    pdf.obj(
        tree,
        &Object::Dict(w::dict([
            ("Type", w::name("Pages")),
            ("Count", w::int(kids.len() as i64)),
            ("Kids", Object::Array(kids)),
        ])),
    )?;
    pdf.obj(
        root,
        &Object::Dict(w::dict([
            ("Type", w::name("Catalog")),
            ("Pages", w::reference(tree)),
        ])),
    )?;
    require(!expired())?;
    Ok(pdf.finish(root, None)?.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use folium_syntax::source::Bytes;

    fn fixture(paints: &[&[u8]]) -> Vec<u8> {
        let mut pdf = Writer::new(Vec::new(), "1.4").unwrap();
        let root = pdf.reserve();
        let tree = pdf.reserve();
        let mut pages = Vec::new();
        for paint in paints {
            let stream = pdf.reserve();
            pdf.stream(stream, Dict::new(), paint, false).unwrap();
            let page = pdf.reserve();
            pages.push(w::reference(page));
            pdf.obj(
                page,
                &Object::Dict(w::dict([
                    ("Type", w::name("Page")),
                    ("Parent", w::reference(tree)),
                    ("MediaBox", w::nums(&[0., 0., 10., 20.])),
                    ("Rotate", w::int(90)),
                    ("Contents", w::reference(stream)),
                ])),
            )
            .unwrap();
        }
        pdf.obj(
            tree,
            &Object::Dict(w::dict([
                ("Type", w::name("Pages")),
                ("Count", w::int(pages.len() as i64)),
                ("Kids", Object::Array(pages)),
            ])),
        )
        .unwrap();
        pdf.obj(
            root,
            &Object::Dict(w::dict([
                ("Type", w::name("Catalog")),
                ("Pages", w::reference(tree)),
                (
                    "OpenAction",
                    Object::Dict(w::dict([
                        ("S", w::name("JavaScript")),
                        ("JS", w::text_string("Private source")),
                    ])),
                ),
            ])),
        )
        .unwrap();
        pdf.finish(root, None).unwrap().0
    }

    #[test]
    fn paints_every_page_and_drops_the_source_catalog() {
        let bytes = export(Box::new(Bytes(
            fixture(&[b"1 0 0 rg 0 0 10 20 re f 10 0 -10 20 re f", b""]).into(),
        )))
        .unwrap();
        // Re-importing the image-only file exercises strict Flate RGB input.
        let again = export(Box::new(Bytes(bytes.clone().into()))).unwrap();
        assert_eq!(bytes, again);
        let doc = Doc::open(Box::new(Bytes(bytes.into())), b"").unwrap();
        assert_eq!(doc.page_count(), 2);
        assert!(doc.catalog().get("OpenAction").is_none());
        for (i, color) in [[255, 0, 0], [255, 255, 255]].iter().enumerate() {
            let page = doc.page(i).unwrap();
            assert_eq!(page.size(), (20., 10.));
            assert!(allowed(&page.resources, &["XObject"]));
            let img = resource(&doc, &page.resources, "XObject", &w::name("PageImage")).unwrap();
            let rgb = doc.syntax.decode(img.as_stream().unwrap()).unwrap().data;
            assert_eq!(rgb.len(), 40 * 20 * 3);
            assert!(rgb.chunks_exact(3).all(|p| p == color));
        }
    }

    #[test]
    fn refuses_the_whole_document_when_the_last_page_is_incomplete() {
        for last in [
            b"/Shade sh".as_slice(),
            b"0 0 10 re f",
            b"BT /Missing 12 Tf (missing font) Tj ET",
            b"q 10 0 0 10 0 0 cm /Missing Do Q",
            b"q",
        ] {
            assert!(export(Box::new(Bytes(fixture(&[b"", last]).into()))).is_err());
        }
    }
}
