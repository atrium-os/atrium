//! Image XObjects and inline images to RGBA at (about) the painted size
//! (folium.md §7.1). Raw samples (Flate, LZW, RunLength, none) at 1, 2, 4,
//! 8 or 16 bits through the colour space and `/Decode`; DCT through
//! zune-jpeg; soft masks as alpha; stencil masks as coverage. JPEG 2000,
//! JBIG2 and CCITT come in P4 and draw a placeholder.

use folium_content::color::Space;
use folium_syntax::{Document, Object, Stream};

/// The largest image frame decoded, in pixels (the natural-frame ceiling
/// of §6 for formats that can't decode small).
pub const FRAME_MAX: u64 = 100 << 20;

pub struct Rgba {
    pub w: u32,
    pub h: u32,
    /// Premultiplied RGBA, rows top to bottom.
    pub px: Vec<u8>,
}

fn dim(st: &Stream, k: &str, k2: &str) -> Option<u32> {
    st.dict.get(k).or_else(|| st.dict.get(k2)).and_then(Object::as_int).filter(|v| *v > 0 && *v <= 1 << 16).map(|v| v as u32)
}

/// Samples of one row, unpacked to 0..=max per component.
fn unpack(data: &[u8], bpc: u32, count: usize, out: &mut Vec<u16>) {
    out.clear();
    match bpc {
        8 => out.extend(data.iter().take(count).map(|&b| b as u16)),
        16 => out.extend(data.chunks_exact(2).take(count).map(|c| u16::from_be_bytes([c[0], c[1]]))),
        1 | 2 | 4 => {
            let per = 8 / bpc as usize;
            let mask = (1u16 << bpc) - 1;
            for i in 0..count {
                let b = data.get(i / per).copied().unwrap_or(0) as u16;
                let sh = (per - 1 - i % per) as u32 * bpc;
                out.push((b >> sh) & mask);
            }
        }
        _ => {}
    }
    out.resize(count, 0);
}

/// Decode a stencil (`/ImageMask true`) to coverage 0..255, 255 = paint.
pub fn decode_mask(doc: &Document, st: &Stream) -> Option<(u32, u32, Vec<u8>)> {
    let w = dim(st, "Width", "W")?;
    let h = dim(st, "Height", "H")?;
    if w as u64 * h as u64 > FRAME_MAX {
        return None;
    }
    let d = doc.decode(st).ok()?;
    if !d.rest.is_empty() {
        return None;
    }
    // /Decode [1 0] inverts: then 1 bits paint.
    let invert = st.dict.get("Decode").or_else(|| st.dict.get("D")).and_then(Object::as_array).and_then(|a| a.first()).and_then(Object::as_f64) == Some(1.0);
    let stride = (w as usize).div_ceil(8);
    let mut out = Vec::with_capacity((w * h) as usize);
    let mut row = Vec::new();
    for y in 0..h as usize {
        let r = d.data.get(y * stride..((y + 1) * stride).min(d.data.len())).unwrap_or(&[]);
        unpack(r, 1, w as usize, &mut row);
        out.extend(row.iter().map(|&b| if (b == 0) != invert { 255 } else { 0 }));
    }
    Some((w, h, out))
}

/// Decode an image to premultiplied RGBA, box-filtered down to at most
/// `max_w` × `max_h` (the painted size, rounded up by the caller).
pub fn decode(doc: &Document, st: &Stream, max_w: u32, max_h: u32) -> Option<Rgba> { decode_d(doc, st, max_w, max_h, 0) }

fn decode_d(doc: &Document, st: &Stream, max_w: u32, max_h: u32, depth: u32) -> Option<Rgba> {
    let w = dim(st, "Width", "W")?;
    let h = dim(st, "Height", "H")?;
    if w as u64 * h as u64 > FRAME_MAX {
        return None;
    }
    let d = doc.decode(st).ok()?;
    let mut img = if let Some(codec) = d.rest.first() {
        if codec.name.is("DCTDecode") || codec.name.is("DCT") {
            jpeg(&d.data, w, h, st, doc)?
        } else {
            return None;
        }
    } else {
        raw(doc, st, &d.data, w, h)?
    };
    // Soft mask: its grey is alpha.
    if let (0, Ok(Object::Stream(sm))) = (depth, doc.get_in(&st.dict, "SMask")) {
        if let Some(a) = decode_d(doc, &sm, img.w, img.h, 1) {
            // /Matte: the colours were already blended with the matte by
            // alpha (§11.6.5.3), so they are premultiplied once undone.
            let matte: Option<Vec<f32>> = sm.dict.get("Matte").and_then(Object::as_array).map(|m| m.iter().map(|v| v.as_f64().unwrap_or(0.0) as f32).collect());
            let matte = matte.map(|m| {
                let c = space_of(doc, st).to_rgb(&m);
                [c.r, c.g, c.b]
            });
            apply_alpha(&mut img, &a, matte);
        }
    }
    Some(downsample(img, max_w.max(1), max_h.max(1)))
}

fn space_of(doc: &Document, st: &Stream) -> Space {
    match st.dict.get("ColorSpace").or_else(|| st.dict.get("CS")) {
        Some(o) => Space::read(doc, o, 0),
        None => Space::Gray,
    }
}

fn raw(doc: &Document, st: &Stream, data: &[u8], w: u32, h: u32) -> Option<Rgba> {
    let bpc = st.dict.get("BitsPerComponent").or_else(|| st.dict.get("BPC")).and_then(Object::as_int).unwrap_or(8) as u32;
    if !matches!(bpc, 1 | 2 | 4 | 8 | 16) {
        return None;
    }
    let space = space_of(doc, st);
    let n = space.components();
    let max = ((1u32 << bpc) - 1) as f32;
    let indexed = matches!(space, Space::Indexed { .. });
    let decode: Vec<f32> = match st.dict.get("Decode").or_else(|| st.dict.get("D")).and_then(Object::as_array) {
        Some(a) if a.len() >= 2 * n => a.iter().map(|o| o.as_f64().unwrap_or(0.0) as f32).collect(),
        _ => (0..n).flat_map(|_| if indexed { [0.0, max] } else { [0.0, 1.0] }).collect(),
    };
    let stride = (w as usize * n * bpc as usize).div_ceil(8);
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    let mut row = Vec::new();
    let mut comps = vec![0f32; n];
    // Fast paths for the common 8-bit device spaces.
    for y in 0..h as usize {
        let r = data.get(y * stride..((y + 1) * stride).min(data.len())).unwrap_or(&[]);
        unpack(r, bpc, w as usize * n, &mut row);
        for x in 0..w as usize {
            for k in 0..n {
                let v = row[x * n + k] as f32 / max;
                comps[k] = decode[2 * k] + v * (decode[2 * k + 1] - decode[2 * k]);
            }
            let c = space.to_rgb(&comps);
            px.extend_from_slice(&[(c.r * 255.0 + 0.5) as u8, (c.g * 255.0 + 0.5) as u8, (c.b * 255.0 + 0.5) as u8, 255]);
        }
    }
    Some(Rgba { w, h, px })
}

fn jpeg(data: &[u8], w: u32, h: u32, st: &Stream, doc: &Document) -> Option<Rgba> {
    use zune_core::{bytestream::ZCursor, colorspace::ColorSpace, options::DecoderOptions};
    let opts = DecoderOptions::default()
        .set_max_width(1 << 16)
        .set_max_height(1 << 16)
        .set_use_unsafe(false)
        .jpeg_set_out_colorspace(ColorSpace::RGBA);
    let mut dec = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(data), opts);
    dec.decode_headers().ok()?;
    let info = dec.info()?;
    let (jw, jh) = (info.width as u32, info.height as u32);
    if jw as u64 * jh as u64 > FRAME_MAX || jw == 0 || jh == 0 {
        return None;
    }
    let cmyk = info.components == 4;
    let px = if cmyk {
        // zune's RGBA from CMYK assumes non-inverted ink; Adobe writes
        // inverted CMYK, which PDF's /Decode [1 0 …] or the APP14 marker
        // says. Decode raw CMYK and convert ourselves.
        let opts = DecoderOptions::default().set_use_unsafe(false).jpeg_set_out_colorspace(ColorSpace::CMYK);
        let mut dec = zune_jpeg::JpegDecoder::new_with_options(ZCursor::new(data), opts);
        let raw = dec.decode().ok()?;
        let adobe = data.windows(5).take(4096).any(|w| w == b"Adobe");
        let inv_decode = st.dict.get("Decode").and_then(Object::as_array).and_then(|a| a.first()).and_then(Object::as_f64) == Some(1.0);
        let inverted = adobe != inv_decode;
        let _ = doc;
        raw.chunks_exact(4)
            .flat_map(|p| {
                let f = |v: u8| if inverted { 255 - v } else { v } as f32 / 255.0;
                let (c, m, y, k) = (f(p[0]), f(p[1]), f(p[2]), f(p[3]));
                let kk = 1.0 - k;
                [((1.0 - c) * kk * 255.0) as u8, ((1.0 - m) * kk * 255.0) as u8, ((1.0 - y) * kk * 255.0) as u8, 255]
            })
            .collect()
    } else {
        dec.decode().ok()?
    };
    if px.len() != (jw * jh * 4) as usize {
        return None;
    }
    let _ = (w, h);
    Some(Rgba { w: jw, h: jh, px })
}

fn apply_alpha(img: &mut Rgba, a: &Rgba, matte: Option<[f32; 3]>) {
    for y in 0..img.h {
        let ay = (y as u64 * a.h as u64 / img.h as u64) as u32;
        for x in 0..img.w {
            let ax = (x as u64 * a.w as u64 / img.w as u64) as u32;
            let al = a.px[((ay * a.w + ax) * 4) as usize] as u32; // grey in R
            let i = ((y * img.w + x) * 4) as usize;
            match matte {
                // stored = m + a(c - m), so c·a = stored - m(1 - a).
                Some(m) => {
                    let af = al as f32 / 255.0;
                    for k in 0..3 {
                        let v = img.px[i + k] as f32 / 255.0 - m[k] * (1.0 - af);
                        img.px[i + k] = (v.clamp(0.0, af) * 255.0 + 0.5) as u8;
                    }
                    img.px[i + 3] = al as u8;
                }
                None => {
                    for k in 0..4 {
                        img.px[i + k] = ((img.px[i + k] as u32 * al + 127) / 255) as u8;
                    }
                }
            }
        }
    }
}

/// Box-filter down so neither side exceeds the target (never up).
fn downsample(img: Rgba, max_w: u32, max_h: u32) -> Rgba {
    let fx = img.w.div_ceil(max_w).max(1);
    let fy = img.h.div_ceil(max_h).max(1);
    if fx == 1 && fy == 1 {
        return img;
    }
    let (nw, nh) = (img.w.div_ceil(fx), img.h.div_ceil(fy));
    let mut px = vec![0u8; (nw * nh * 4) as usize];
    for y in 0..nh {
        for x in 0..nw {
            let mut acc = [0u32; 4];
            let mut n = 0;
            for sy in y * fy..((y + 1) * fy).min(img.h) {
                let row = (sy * img.w) as usize * 4;
                for sx in x * fx..((x + 1) * fx).min(img.w) {
                    let i = row + sx as usize * 4;
                    for k in 0..4 {
                        acc[k] += img.px[i + k] as u32;
                    }
                    n += 1;
                }
            }
            let o = ((y * nw + x) * 4) as usize;
            for k in 0..4 {
                px[o + k] = (acc[k] / n.max(1)) as u8;
            }
        }
    }
    Rgba { w: nw, h: nh, px }
}
