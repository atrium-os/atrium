use super::{check, Error};
use mail_office_copy::{OfficeCopy, PagePixels};
use officina_slides::{
    draw::{draw_slide, DrawItem, Notes, Paint},
    model::*,
};
use officina_text::{layout::PinnedFonts, paint::Item, LU};
use std::collections::BTreeMap;
use tiny_skia::{Pixmap, Rect, Transform};
const PIXEL: f64 = 6350.;

fn picture(bytes: &[u8], crop: [i32; 4], dest: Rect, canvas: &mut Pixmap) -> Result<(), Error> {
    check(
        bytes.starts_with(b"\x89PNG\r\n\x1a\n") && bytes.len() > 32,
        "invalid PNG",
    )?;
    let uint = |p: &[u8]| u32::from_be_bytes(p.try_into().unwrap());
    let pixels = u64::from(uint(&bytes[16..20])) * u64::from(uint(&bytes[20..24]));
    check(
        (1..=8_000_000).contains(&pixels),
        "picture dimensions exceed limit",
    )?;
    let mut chunks = &bytes[8..];
    while !chunks.is_empty() {
        check(chunks.len() >= 12, "incomplete PNG")?;
        let end = 12 + uint(&chunks[..4]) as usize;
        check(
            end <= chunks.len()
                && matches!(
                    &chunks[4..8],
                    b"IHDR" | b"PLTE" | b"tRNS" | b"IDAT" | b"IEND"
                ),
            "unsupported PNG chunk",
        )?;
        check(
            officina_convert::zip::crc32(&chunks[4..end - 4]) == uint(&chunks[end - 4..end]),
            "damaged PNG",
        )?;
        if &chunks[4..8] == b"IEND" {
            check(end == chunks.len(), "trailing PNG data")?;
            break;
        }
        chunks = &chunks[end..];
    }
    check(chunks.get(4..8) == Some(b"IEND"), "incomplete PNG")?;
    let img = Pixmap::decode_png(bytes)?;
    let [l, t, r, b] = crop.map(|v| f64::from(v) / 100000.);
    check(
        crop.iter().all(|v| *v >= 0) && l + r < 1. && t + b < 1.,
        "invalid picture crop",
    )?;
    let scale_x = f64::from(dest.width()) / (f64::from(img.width()) * (1. - l - r));
    let scale_y = f64::from(dest.height()) / (f64::from(img.height()) * (1. - t - b));
    let xf = Transform::from_row(
        scale_x as f32,
        0.,
        0.,
        scale_y as f32,
        dest.x() - (l * f64::from(img.width()) * scale_x) as f32,
        dest.y() - (t * f64::from(img.height()) * scale_y) as f32,
    );
    let mut mask =
        tiny_skia::Mask::new(canvas.width(), canvas.height()).ok_or("invalid picture clip")?;
    mask.fill_path(
        &tiny_skia::PathBuilder::from_rect(dest),
        tiny_skia::FillRule::Winding,
        false,
        Transform::identity(),
    );
    canvas.draw_pixmap(0, 0, img.as_ref(), &Default::default(), xf, Some(&mask));
    Ok(())
}

pub(super) fn pages(deck: &mut Deck) -> Result<OfficeCopy, Error> {
    let twips = |v: i64| ((v + 317) / 635) as u32;
    let (w, h) = (twips(deck.size.0), twips(deck.size.1));
    let (pw, ph) = (w.div_ceil(10), h.div_ceil(10));
    let count = u64::from(pw) * u64::from(ph);
    check(
        count <= 8_000_000 && count * deck.slide_count() as u64 <= 48_000_000,
        "slide pixel budget",
    )?;
    let fonts = PinnedFonts::new()?;
    let mut pages = vec![];
    let mut encoded = 0;
    for i in 0..deck.slide_count() {
        let mut expected = BTreeMap::new();
        let slide = deck.slide_if_loaded(i).ok_or("missing slide")?;
        let layout = deck.layout(slide.layout).ok_or("missing layout")?;
        let master = deck.master(layout.master).ok_or("missing master")?;
        for shape in master
            .shapes
            .iter()
            .chain(&layout.shapes)
            .filter(|s| s.ph.is_none())
            .chain(&slide.shapes)
        {
            if let Some(text) = shape.text().map(TextBody::text).filter(|s| !s.is_empty()) {
                let f = shape.frame.ok_or("missing text frame")?;
                let frame = Rect::from_xywh(
                    (f.x as f64 / PIXEL) as f32,
                    (f.y as f64 / PIXEL) as f32,
                    (f.w as f64 / PIXEL) as f32,
                    (f.h as f64 / PIXEL) as f32,
                )
                .ok_or("invalid text frame")?;
                expected.insert(shape.id, (text, frame));
            }
        }
        let mut notes = Notes::default();
        let mut canvas = Pixmap::new(pw, ph).ok_or("invalid slide dimensions")?;
        canvas.fill(tiny_skia::Color::WHITE);
        for item in draw_slide(deck, i, &fonts, &mut notes).map_err(|e| e.0)? {
            match item {
                DrawItem::Text { xf, shape, items } => {
                    let k = officina_slides::EMU_PER_PT as f64 / LU as f64;
                    check(xf[..4] == [k, 0., 0., k], "unsupported text transform")?;
                    let (text, frame) = expected.remove(&shape).ok_or("unexpected slide text")?;
                    let mut painted = String::new();
                    for item in items {
                        let Item::Glyphs(mut run) = item else {
                            return Err("unsupported slide text paint".into());
                        };
                        run.x += (xf[4] / k).round() as i64;
                        run.y += (xf[5] / k).round() as i64;
                        crate::paint::glyphs_in(&mut canvas, &fonts, &run, frame)?;
                        painted.push_str(&run.text);
                    }
                    check(text == painted, "incomplete slide text")?;
                }
                other => {
                    // The reconstructed model contains only axis-aligned rectangles and pictures.
                    let (xf, w, h) = match &other {
                        DrawItem::Path { xf, w, h, .. } | DrawItem::Picture { xf, w, h, .. } => {
                            (xf, w, h)
                        }
                        _ => return Err("unsupported slide paint".into()),
                    };
                    check(xf[..4] == [1., 0., 0., 1.], "unsupported shape transform")?;
                    let dest = Rect::from_xywh(
                        (xf[4] / PIXEL) as f32,
                        (xf[5] / PIXEL) as f32,
                        (w / PIXEL) as f32,
                        (h / PIXEL) as f32,
                    )
                    .ok_or("invalid slide frame")?;
                    match other {
                        DrawItem::Path {
                            fill: Some(Paint::Solid(c)),
                            stroke: None,
                            ..
                        } => {
                            let mut p = tiny_skia::Paint::default();
                            let [r, g, b, a] = c.to_be_bytes();
                            p.set_color_rgba8(r, g, b, a);
                            canvas.fill_rect(dest, &p, Transform::identity(), None);
                        }
                        DrawItem::Path {
                            fill: None,
                            stroke: None,
                            ..
                        } => {}
                        DrawItem::Picture { media, crop, .. } => picture(
                            &deck.media_bytes(media).map_err(|e| e.0)?,
                            crop,
                            dest,
                            &mut canvas,
                        )?,
                        _ => return Err("unsupported slide paint".into()),
                    }
                }
            }
        }
        check(
            expected.is_empty() && notes.unknown_presets.is_empty(),
            "missing slide content",
        )?;
        let rgb: Vec<u8> = canvas
            .data()
            .chunks_exact(4)
            .flat_map(|p| p[..3].iter().copied())
            .collect();
        let samples = miniz_oxide::deflate::compress_to_vec_zlib(&rgb, 1);
        encoded += samples.len();
        check(encoded <= 32 << 20, "deck output limit")?;
        pages.push(PagePixels {
            width_twips: w,
            height_twips: h,
            samples,
        });
    }
    Ok(OfficeCopy { pages })
}
