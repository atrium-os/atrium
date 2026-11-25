//! The system's image decoder (app-composition.md C6, dev-tools.md §3.5).
//!
//! `org.atrium.image-decode`: portcullisd starts it for an app whose
//! manifest names `decoders = ["image"]` (`SystemWorker`), in a one-shot
//! jail with no capability, as `_imagedec`. It enters capability mode, then
//! answers [`Decode`] calls on its standard input: the picture is the file
//! that came with the call, read up to navigator-image's encoded ceiling,
//! decoded by its memory-safe decoders into the box asked for, and sent
//! back as RGBA. The app never parses the bytes; this jail holds nothing
//! but them.

use std::fs::File;
use std::io::Read;

use atrium_assets::{Decode, Picture};
use navigator_image::image::{decode_unaddressed, Ceilings, MAX_ENCODED_IMAGE_BYTES};

/// The largest natural frame held while one picture decodes: the jail's
/// memory limit is 256 MiB, and the answer is at most 64 MiB of it.
pub const FRAME: usize = 96 << 20;

/// One call: the picture in `file`, fitted into the box.
pub fn decode(req: Decode, file: Option<File>) -> Result<Picture, String> {
    let file = file.ok_or("no picture came with the call")?;
    let mut bytes = Vec::new();
    file.take(MAX_ENCODED_IMAGE_BYTES as u64 + 1).read_to_end(&mut bytes).map_err(|e| format!("reading the picture: {e}"))?;
    decode_bytes(&bytes, req.max_width, req.max_height)
}

/// `bytes` fitted into `max_w` × `max_h`.
pub fn decode_bytes(bytes: &[u8], max_w: u32, max_h: u32) -> Result<Picture, String> {
    let (max_w, max_h) = (max_w.clamp(1, atrium_assets::MAX_SIDE), max_h.clamp(1, atrium_assets::MAX_SIDE));
    // The answer is checked against the box below; the budget is the
    // largest answer there is, since a decoder may reduce in steps through
    // sizes above the box.
    let budget = atrium_assets::MAX_SIDE as usize * atrium_assets::MAX_SIDE as usize * 4;
    let c = Ceilings { budget, frame: FRAME.max(budget), target: Some(box_for(bytes, max_w, max_h)), ..Ceilings::default() };
    let px = decode_unaddressed(bytes, c).map_err(|e| e.to_string())?;
    if px.width == 0 || px.height == 0 || px.width > max_w || px.height > max_h {
        return Err(format!("decoded to {}x{}, outside the {max_w}x{max_h} asked for", px.width, px.height));
    }
    if px.rgba.len() != px.width as usize * px.height as usize * 4 {
        return Err("the decoder's pixels don't match its size".into());
    }
    Ok(Picture { width: px.width, height: px.height, rgba: px.rgba })
}

/// The size to decode into: the largest size of the picture's own shape
/// in the box (the decoders fill the target they are given, and SVG is
/// stretched to it). A raster picture is never enlarged.
fn box_for(bytes: &[u8], max_w: u32, max_h: u32) -> (u32, u32) {
    use navigator_image::image::{natural_size, sniff, Format};
    let (w, h, up) = if sniff(bytes) == Ok(Format::Svg) {
        // Refused here, it is refused again (with its reason) when drawn.
        let Ok((tree, _)) = navigator_svg::load(bytes) else { return (max_w, max_h) };
        let s = tree.size();
        (s.width() as f64, s.height() as f64, true)
    } else {
        let Ok((_, w, h)) = natural_size(bytes) else { return (max_w, max_h) };
        (w as f64, h as f64, false)
    };
    if !(w > 0.0 && h > 0.0 && w.is_finite() && h.is_finite()) {
        return (max_w, max_h);
    }
    let mut k = (max_w as f64 / w).min(max_h as f64 / h);
    if !up {
        k = k.min(1.0);
    }
    (((w * k).round() as u32).clamp(1, max_w), ((h * k).round() as u32).clamp(1, max_h))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SVG: &[u8] = br##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><rect x="2" y="2" width="20" height="20" fill="#c06030"/></svg>"##;

    #[test]
    fn an_icon_is_drawn_at_the_box() {
        let p = decode_bytes(SVG, 48, 48).unwrap();
        assert_eq!((p.width, p.height), (48, 48));
        // The middle is the rect's colour, opaque.
        let i = (24 * 48 + 24) * 4;
        assert_eq!(&p.rgba[i..i + 4], &[0xc0, 0x60, 0x30, 0xff]);
        // A corner is outside it: clear.
        assert_eq!(p.rgba[3], 0);
        atrium_assets::check(&p, 48, 48).unwrap();
    }

    #[test]
    fn what_it_refuses() {
        assert!(decode_bytes(b"not a picture", 16, 16).is_err());
        assert!(decode_bytes(b"", 16, 16).is_err());
        // Script and outside references are not in the strict profile.
        let bad = br#"<svg xmlns="http://www.w3.org/2000/svg"><script>x()</script></svg>"#;
        assert!(decode_bytes(bad, 16, 16).is_err());
        assert!(decode(Decode { max_width: 4, max_height: 4 }, None).is_err());
    }

    #[test]
    fn a_raster_picture_is_fitted_and_never_enlarged() {
        let png = |w: u32, h: u32| {
            let mut out = Vec::new();
            let mut e = png::Encoder::new(&mut out, w, h);
            e.set_color(png::ColorType::Rgba);
            let mut wr = e.write_header().unwrap();
            wr.write_image_data(&[0x30, 0x90, 0x40, 0xff].repeat((w * h) as usize)).unwrap();
            drop(wr);
            out
        };
        let p = decode_bytes(&png(200, 100), 64, 64).unwrap();
        assert_eq!((p.width, p.height), (64, 32));
        assert_eq!(&p.rgba[..4], &[0x30, 0x90, 0x40, 0xff]);
        let p = decode_bytes(&png(10, 5), 64, 64).unwrap();
        assert_eq!((p.width, p.height), (10, 5));
    }
}
