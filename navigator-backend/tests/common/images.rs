//! Image fixtures, ENCODED BY THE TEST from known pixels — never corpus
//! content, and every one small enough to read. A fixture must be able to
//! show what it certifies: each pattern below has a different value in every
//! corner, so a flipped, transposed or channel-swapped decode is visible.

#![allow(dead_code)]

/// A `w`×`h` RGBA pattern: red ramps across, green ramps down, blue is the
/// checkerboard parity, alpha is 255 except one half-transparent pixel at
/// (1, 0) — so straight alpha can be told from premultiplied.
pub fn pattern(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            let a = if (x, y) == (1, 0) { 128 } else { 255 };
            v.extend_from_slice(&[(x * 255 / w.max(2).saturating_sub(1).max(1)).min(255) as u8,
                                  (y * 255 / h.max(2).saturating_sub(1).max(1)).min(255) as u8,
                                  if (x + y) % 2 == 0 { 200 } else { 40 }, a]);
        }
    }
    v
}

pub fn png_rgba(w: u32, h: u32, px: &[u8]) -> Vec<u8> {
    let mut out = vec![];
    {
        let mut e = png::Encoder::new(&mut out, w, h);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        e.write_header().unwrap().write_image_data(px).unwrap();
    }
    out
}

/// A PNG in another colour type, for the conversion to RGBA.
pub fn png_typed(w: u32, h: u32, ct: png::ColorType, depth: png::BitDepth, data: &[u8]) -> Vec<u8> {
    let mut out = vec![];
    {
        let mut e = png::Encoder::new(&mut out, w, h);
        e.set_color(ct);
        e.set_depth(depth);
        if ct == png::ColorType::Indexed { e.set_palette(vec![255, 0, 0, 0, 0, 255]); }
        e.write_header().unwrap().write_image_data(data).unwrap();
    }
    out
}

pub fn rgb_of(px: &[u8]) -> Vec<u8> { px.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect() }

pub fn jpeg(w: u16, h: u16, px: &[u8], progressive: bool) -> Vec<u8> {
    let mut out = vec![];
    let mut e = jpeg_encoder::Encoder::new(&mut out, 95);
    e.set_progressive(progressive);
    e.encode(&rgb_of(px), w, h, jpeg_encoder::ColorType::Rgb).unwrap();
    out
}

pub fn webp_lossless(w: u32, h: u32, px: &[u8]) -> Vec<u8> {
    let mut out = vec![];
    image_webp::WebPEncoder::new(&mut out).encode(px, w, h, image_webp::ColorType::Rgba8).unwrap();
    out
}

/// A GIF whose logical screen is `sw`×`sh` and whose one frame is `fw`×`fh`
/// at (`left`, `top`), coloured red (index 0) and blue (index 1) by parity,
/// with index 2 transparent at the frame's first pixel.
pub fn gif(sw: u16, sh: u16, left: u16, top: u16, fw: u16, fh: u16) -> Vec<u8> {
    let mut out = vec![];
    {
        let mut e = gif::Encoder::new(&mut out, sw, sh, &[]).unwrap();
        let mut idx: Vec<u8> = (0..fw as u32 * fh as u32).map(|i| ((i % fw as u32 + i / fw as u32) % 2) as u8).collect();
        idx[0] = 2;
        let mut f = gif::Frame::from_palette_pixels(fw, fh, idx, vec![255, 0, 0, 0, 0, 255, 0, 0, 0], Some(2));
        f.left = left;
        f.top = top;
        e.write_frame(&f).unwrap();
    }
    out
}

pub fn address(bytes: &[u8]) -> String { format!("blake3:{}", blake3::hash(bytes).to_hex()) }

/// A recording whose document shows one image per address.
pub fn recording_with_images(addresses: &[String]) -> Vec<u8> {
    let imgs: String = addresses.iter().enumerate()
        .map(|(i, _)| format!("<img src=\\\"/i{i}.png\\\">")).collect();
    let table: Vec<String> = addresses.iter().enumerate()
        .map(|(i, a)| format!(r#"{{"src":"https://a.test/i{i}.png","width":2,"height":2,"address":"{a}"}}"#)).collect();
    format!(r##"{{"format":"atrium-navigator-recording/3","url":"https://a.test/","tier":1,
        "tier_reason":"","measurements":{{}},"transitions":[],"stylesheets":[],
        "images":[{}],
        "document":"<html><body>{imgs}</body></html>"}}"##, table.join(",")).into_bytes()
}
