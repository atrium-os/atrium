//! Pictures in formats the apps don't draw, made ones they do as a
//! document is read (in the converter, never in the app): TIFF becomes
//! PNG; SVG is kept (the pictures worker draws it at its painted size)
//! and only sized here.

use crate::docx::read::pixel_size;

/// Largest side of a TIFF decoded, in pixels, and most bytes its pixels
/// take while decoding.
pub const MAX_TIFF_SIDE: u32 = 16_384;
pub const MAX_TIFF_BYTES: usize = 256 << 20;

/// A picture's bytes and natural size as the document keeps them: as they
/// are, or (TIFF) re-encoded as PNG; `None` for what can't be shown.
pub fn normalize(bytes: Vec<u8>) -> Option<(Vec<u8>, u32, u32)> {
    if let Some((w, h)) = pixel_size(&bytes) {
        return Some((bytes, w, h));
    }
    if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") {
        return tiff_to_png(&bytes);
    }
    let (w, h) = svg_size(&bytes)?;
    Some((bytes, w, h))
}

/// Whether `b` is an SVG document (as the pictures worker sniffs it).
pub fn is_svg(b: &[u8]) -> bool {
    matches!(navigator_image::image::sniff(b), Ok(navigator_image::image::Format::Svg))
}

/// An SVG's size in pixels: its root's `width` and `height`, else its
/// `viewBox`, else 300 × 150 (the CSS default).
pub fn svg_size(b: &[u8]) -> Option<(u32, u32)> {
    if !is_svg(b) {
        return None;
    }
    let head = String::from_utf8_lossy(&b[..b.len().min(8192)]);
    let at = head.find("<svg")?;
    let tag = &head[at..head[at..].find('>').map_or(head.len(), |e| at + e)];
    let attr = |name: &str| -> Option<String> {
        let mut rest = tag;
        while let Some(i) = rest.find(name) {
            let before = rest[..i].chars().last();
            let after = &rest[i + name.len()..];
            if before.is_some_and(|c| c.is_whitespace()) {
                if let Some(v) = after.trim_start().strip_prefix('=') {
                    let v = v.trim_start();
                    let q = v.chars().next()?;
                    if q == '"' || q == '\'' {
                        return v[1..].find(q).map(|e| v[1..1 + e].to_string());
                    }
                }
            }
            rest = after;
        }
        None
    };
    let px = |v: &str| -> Option<f64> {
        let v = v.trim();
        let split = v.find(|c: char| c.is_ascii_alphabetic() || c == '%').unwrap_or(v.len());
        let n: f64 = v[..split].trim().parse().ok()?;
        let k = match &v[split..] {
            "" | "px" => 1.0,
            "pt" => 96.0 / 72.0,
            "pc" => 16.0,
            "in" => 96.0,
            "cm" => 96.0 / 2.54,
            "mm" => 96.0 / 25.4,
            _ => return None,
        };
        Some(n * k).filter(|x| x.is_finite() && *x > 0.0)
    };
    let view = attr("viewBox").and_then(|v| {
        let n: Vec<f64> = v.split(|c: char| c == ',' || c.is_whitespace()).filter(|s| !s.is_empty()).filter_map(|s| s.parse().ok()).collect();
        (n.len() == 4 && n[2] > 0.0 && n[3] > 0.0).then(|| (n[2], n[3]))
    });
    let (w, h) = match (attr("width").and_then(|v| px(&v)), attr("height").and_then(|v| px(&v))) {
        (Some(w), Some(h)) => (w, h),
        (Some(w), None) => (w, view.map_or(w / 2.0, |v| w * v.1 / v.0)),
        (None, Some(h)) => (view.map_or(h * 2.0, |v| h * v.0 / v.1), h),
        (None, None) => view.unwrap_or((300.0, 150.0)),
    };
    let side = |v: f64| (v.round() as u32).clamp(1, 65_536);
    Some((side(w), side(h)))
}

/// A TIFF's first image as PNG (8 bits a channel; grey, grey and alpha,
/// RGB or RGBA; 16-bit samples taken to 8), within the limits.
pub fn tiff_to_png(b: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    use tiff::decoder::{Decoder, DecodingResult, Limits};
    use tiff::ColorType;
    let mut limits = Limits::default();
    limits.decoding_buffer_size = MAX_TIFF_BYTES;
    limits.intermediate_buffer_size = MAX_TIFF_BYTES / 2;
    limits.ifd_value_size = 16 << 20;
    let mut d = Decoder::new(std::io::Cursor::new(b)).ok()?.with_limits(limits);
    let (w, h) = d.dimensions().ok()?;
    if w == 0 || h == 0 || w > MAX_TIFF_SIDE || h > MAX_TIFF_SIDE || (w as usize) * (h as usize) * 4 > MAX_TIFF_BYTES {
        return None;
    }
    let color = d.colortype().ok()?;
    let data: Vec<u8> = match d.read_image().ok()? {
        DecodingResult::U8(v) => v,
        DecodingResult::U16(v) => v.into_iter().map(|x| (x >> 8) as u8).collect(),
        _ => return None,
    };
    let n = w as usize * h as usize;
    let (ct, per) = match color {
        ColorType::Gray(8 | 16) => (png::ColorType::Grayscale, 1),
        ColorType::GrayA(8 | 16) => (png::ColorType::GrayscaleAlpha, 2),
        ColorType::RGB(8 | 16) => (png::ColorType::Rgb, 3),
        ColorType::RGBA(8 | 16) => (png::ColorType::Rgba, 4),
        _ => return None,
    };
    let data = data.get(..n * per)?;
    let mut out = Vec::new();
    {
        let mut e = png::Encoder::new(&mut out, w, h);
        e.set_color(ct);
        e.set_depth(png::BitDepth::Eight);
        let mut wr = e.write_header().ok()?;
        wr.write_image_data(data).ok()?;
    }
    Some((out, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An uncompressed 2 × 1 RGB TIFF (little-endian).
    pub fn tiny_tiff() -> Vec<u8> {
        let mut b = b"II*\0".to_vec();
        b.extend(8u32.to_le_bytes());
        let entries: [(u16, u16, u32, u32); 9] = [(256, 3, 1, 2), (257, 3, 1, 1), (258, 3, 1, 8), (259, 3, 1, 1), (262, 3, 1, 2), (273, 4, 1, 0), (277, 3, 1, 3), (278, 3, 1, 1), (279, 4, 1, 6)];
        let data_at = 8 + 2 + entries.len() as u32 * 12 + 4;
        b.extend((entries.len() as u16).to_le_bytes());
        for (tag, ty, n, v) in entries {
            b.extend(tag.to_le_bytes());
            b.extend(ty.to_le_bytes());
            b.extend(n.to_le_bytes());
            let v = if tag == 273 { data_at } else { v };
            if ty == 3 {
                b.extend((v as u16).to_le_bytes());
                b.extend([0, 0]);
            } else {
                b.extend(v.to_le_bytes());
            }
        }
        b.extend(0u32.to_le_bytes());
        // BitsPerSample is one value for all three samples here.
        b.extend([255, 0, 0, 0, 0, 255]);
        b
    }

    #[test]
    fn tiff_becomes_png_and_svg_is_sized() {
        let (png, w, h) = normalize(tiny_tiff()).expect("decoded");
        assert_eq!((w, h), (2, 1));
        assert_eq!(pixel_size(&png), Some((2, 1)));
        let svg = br#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg" width="2in" height="72pt"><rect width="10" height="10"/></svg>"#.to_vec();
        assert_eq!(normalize(svg.clone()).map(|x| (x.1, x.2)), Some((192, 96)));
        assert_eq!(svg_size(br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 40 20"></svg>"#), Some((40, 20)));
        assert_eq!(svg_size(br#"<svg xmlns="http://www.w3.org/2000/svg" stroke-width="3" width="50%"></svg>"#), Some((300, 150)));
        assert!(normalize(b"II*\0garbage".to_vec()).is_none());
        assert!(normalize(b"<html><svg></svg></html>".to_vec()).is_none());
    }
}
