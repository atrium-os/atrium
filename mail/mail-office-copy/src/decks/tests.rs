use super::*;
use officina_convert::zip::ZipWriter;

const PPTX: &[u8] = include_bytes!("../../tests/fixtures/complete.pptx");
const ODP: &[u8] = include_bytes!("../../tests/fixtures/complete.odp");

#[test]
fn complete_decks_include_blank_and_last_slide() {
    for (odf, b) in [(false, PPTX), (true, ODP)] {
        let copy = convert(odf, b).unwrap();
        assert_eq!(copy.pages.len(), 3);
        let raw = |i: usize| {
            miniz_oxide::inflate::decompress_to_vec_zlib(&copy.pages[i].samples).unwrap()
        };
        assert!(raw(1).iter().all(|p| *p == 255));
        assert_ne!(raw(0), raw(2));
    }
}

fn edit(b: &[u8], part: &str, old: &str, new: &str) -> Vec<u8> {
    let mut entries = parts(b).unwrap();
    let xml = String::from_utf8(entries.remove(part).unwrap()).unwrap();
    assert!(xml.contains(old));
    entries.insert(part.into(), xml.replace(old, new).into_bytes());
    let mut z = ZipWriter::new(vec![]);
    for (n, b) in entries {
        z.add(&n, &b).unwrap();
    }
    z.finish().unwrap()
}
#[test]
fn refused_objects_on_final_slide() {
    for (odf, b, part, old, new) in [
        (
            false,
            PPTX,
            "ppt/slides/slide3.xml",
            "<p:sld ",
            "<p:sld show=\"false\" ",
        ),
        (
            false,
            PPTX,
            "ppt/slides/slide3.xml",
            "</p:cSld>",
            "<p:timing/></p:cSld>",
        ),
        (
            false,
            PPTX,
            "ppt/slides/slide3.xml",
            "</p:cSld>",
            "<a:audioFile/></p:cSld>",
        ),
        (
            false,
            PPTX,
            "ppt/slides/slide3.xml",
            "</p:cSld>",
            "<a:videoFile/></p:cSld>",
        ),
        (
            false,
            PPTX,
            "ppt/slides/slide3.xml",
            "</p:cSld>",
            "<p:graphicFrame/></p:cSld>",
        ),
        (
            true,
            ODP,
            "content.xml",
            "draw:name=\"page3\"",
            "draw:name=\"page3\" presentation:visibility=\"hidden\"",
        ),
        (
            true,
            ODP,
            "content.xml",
            "</office:presentation>",
            "<anim:seq/></office:presentation>",
        ),
        (
            true,
            ODP,
            "content.xml",
            "</office:presentation>",
            "<draw:plugin/></office:presentation>",
        ),
    ] {
        assert!(convert(odf, &edit(b, part, old, new)).is_err(), "{new}");
    }
}
#[test]
fn rejects_clipped_text_on_last_slide() {
    let bytes = edit(PPTX, "ppt/slides/slide3.xml", "cy=\"914400\"", "cy=\"635\"");
    assert!(convert(false, &bytes).is_err());
}

#[test]
fn speaker_notes_do_not_change_output() {
    for (odf, b, part) in [
        (false, PPTX, "ppt/notesSlides/notesSlide3.xml"),
        (true, ODP, "content.xml"),
    ] {
        let original = convert(odf, b).unwrap().into_pdf().unwrap();
        let changed = edit(b, part, "PRIVATE NOTES", "Confidential speaker cue");
        assert_eq!(
            original,
            convert(odf, &changed).unwrap().into_pdf().unwrap()
        );
    }
}
#[test]
fn static_master_and_local_picture() {
    for odf in [false, true] {
        let mut d = Deck::new();
        let id = d.new_id();
        d.masters[0].shapes.push(Shape::rect(
            id,
            Rect::new(0, 0, 914400, 914400),
            Fill::Solid(Color::rgb(0x112233)),
        ));
        let layout = d.layout_of_kind("blank").unwrap().id;
        let mut slide = new_slide(&mut d, layout);
        let mut image = tiny_skia::Pixmap::new(2, 1).unwrap();
        image
            .data_mut()
            .copy_from_slice(&[255, 0, 0, 255, 0, 0, 255, 255]);
        let media = d.add_media("image/png", &image.encode_png().unwrap());
        slide.shapes.push(Shape::new(
            d.new_id(),
            Rect::new(914400, 914400, 914400, 914400),
            ShapeKind::Picture {
                media,
                crop: if odf { [0; 4] } else { [50000, 0, 0, 0] },
                line: None,
            },
        ));
        d.push_slide(slide);
        let bytes = if odf {
            odp::write(&mut d, vec![])
        } else {
            pptx::write(&mut d, vec![])
        }
        .unwrap();
        let copy = convert(odf, &bytes).unwrap();
        let p = &copy.pages[0];
        let rgb = miniz_oxide::inflate::decompress_to_vec_zlib(&p.samples).unwrap();
        let pixel = |x: usize, y: usize| {
            let a = (y * p.width_twips.div_ceil(10) as usize + x) * 3;
            &rgb[a..a + 3]
        };
        assert_eq!(pixel(30, 30), [17, 34, 51]);
        assert_eq!(pixel(170, 170), if odf { [255, 0, 0] } else { [0, 0, 255] });
        assert_eq!(pixel(260, 170), [0, 0, 255]);
    }
}

#[test]
fn rich_decks() {
    for (odf, bytes) in [
        (
            false,
            include_bytes!("../../tests/fixtures/rich.pptx").as_slice(),
        ),
        (
            true,
            include_bytes!("../../tests/fixtures/rich.odp").as_slice(),
        ),
    ] {
        let pdf = convert(odf, bytes).unwrap_or_else(|e| panic!("odf={odf}: {e}"));
        assert_eq!(pdf.pages.len(), 3);
        for page in &pdf.pages {
            let pixels = miniz_oxide::inflate::decompress_to_vec_zlib(&page.samples).unwrap();
            assert!(pixels.chunks_exact(3).any(|p| p == [0xb0, 0x12, 0x28]));
            assert!(pixels.chunks_exact(3).any(|p| p == [0, 0, 0]));
        }
    }
}

#[test]
fn incomplete_rich_final_slide_refuses_the_whole_deck() {
    let pptx = include_bytes!("../../tests/fixtures/rich.pptx");
    let odp = include_bytes!("../../tests/fixtures/rich.odp");
    for (odf, bytes, part, label) in [
        (
            false,
            pptx.as_slice(),
            "ppt/slides/slide3.xml",
            "PPTX complete",
        ),
        (true, odp.as_slice(), "content.xml", "ODP complete"),
    ] {
        let overflow = "Unfinished words ".repeat(200);
        let error = convert(odf, &edit(bytes, part, label, overflow.trim_end()))
            .err()
            .expect("refuse overflowing final title");
        assert!(
            error.to_string().contains("text overflows")
                || error.to_string().contains("glyph outside frame"),
            "{error}"
        );
    }

    let invalid = edit(
        odp,
        "content.xml",
        "draw:master-page-name=\"Layout2\"",
        "draw:master-page-name=\"Absent\"",
    );
    assert!(convert(true, &invalid).is_err());
}
