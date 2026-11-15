//! Writes `scratch/floats.owrite` (and `.docx`, `.odt` beside it): an article
//! page with a picture the text wraps beside, one it goes above and below,
//! and a pale one behind the text, for looking at floating pictures in Write
//! and comparing with LibreOffice.
//!
//! cargo run --example floats_docs [DIR]

use officina_text::model::{CharAttrs, Float, HAlign, ImageData, ObjectRef, Rel, TextDoc, Wrap, OBJECT_CHAR};
use officina_text::native;

fn picture(w: u32, h: u32, pale: bool) -> Vec<u8> {
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let (r, g, b) = ((40 + x * 180 / w) as u8, (90 + y * 120 / h) as u8, (200 - x * 120 / w) as u8);
            if pale {
                px.extend_from_slice(&[255 - (255 - r) / 6, 255 - (255 - g) / 6, 255 - (255 - b) / 6, 255]);
            } else {
                px.extend_from_slice(&[r, g, b, 255]);
            }
        }
    }
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(&px).unwrap();
    out
}

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "../scratch".into());
    let mut d = TextDoc::new();
    d.blocks.remove(0);
    let title = d.styles.by_name("Title").unwrap();
    let h2 = d.styles.by_name("Heading 2").unwrap();
    let body = "The new building opens in spring. Every office moves in over two weekends, the library first, and the cafeteria keeps its old menu for a month while the kitchen settles in. ";
    let mut pic = |d: &mut TextDoc, w: u32, h: u32, pale: bool, f: Float, tw: u32, th: u32| {
        let img = d.images.add(ImageData { bytes: picture(w, h, pale).into(), width: w, height: h });
        d.attrs.intern(CharAttrs { object: Some(ObjectRef { image: img, width: tw, height: th, float: Some(f) }), ..Default::default() })
    };
    let n = OBJECT_CHAR.len_utf8() as u32;
    d.push_para("Moving Day", title, vec![]);
    // Behind the text, across the page under the title.
    let behind = pic(&mut d, 400, 300, true, Float { wrap: Wrap::Behind, h_rel: Rel::Margin, h_align: HAlign::Center, v_rel: Rel::Text, y: 0, ..Float::default() }, 6480, 4320);
    d.push_para(&format!("{OBJECT_CHAR}{}", body.repeat(3)), 0, vec![(0, behind), (n, 0)]);
    d.push_para("The building", h2, vec![]);
    let square = pic(&mut d, 300, 240, false, Float { wrap: Wrap::Square, h_align: HAlign::End, y: 0, ..Float::default() }, 2880, 2304);
    d.push_para(&format!("{OBJECT_CHAR}{}", body.repeat(2)), 0, vec![(0, square), (n, 0)]);
    d.push_para(&body.repeat(2), 0, vec![]);
    d.push_para("The library", h2, vec![]);
    let tb = pic(&mut d, 500, 160, false, Float { wrap: Wrap::TopBottom, h_align: HAlign::Center, y: 360, ..Float::default() }, 5040, 1612);
    d.push_para(&format!("{OBJECT_CHAR}{}", body.repeat(2)), 0, vec![(0, tb), (n, 0)]);
    d.push_para(&body.repeat(2), 0, vec![]);
    std::fs::write(format!("{dir}/floats.owrite"), native::save_to_vec(&d, &[])).unwrap();
    std::fs::write(format!("{dir}/floats.docx"), officina_convert::docx::write(&d, Vec::new()).unwrap()).unwrap();
    std::fs::write(format!("{dir}/floats.odt"), officina_convert::odt::write(&d, Vec::new()).unwrap()).unwrap();
    println!("wrote {dir}/floats.owrite, .docx, .odt");
}
