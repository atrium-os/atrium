//! The host strip over each per-app window (fenestra.md §11.5): the
//! host's name as this viewer knows it (what the person connected to,
//! never anything the host says), light on the host's colour. The host
//! draws nothing here, so a remote window can't pose as one of this
//! screen's own.

use ab_glyph::{point, Font, FontRef, PxScale, ScaleFont};

static FONT: &[u8] = include_bytes!("../../fonts/ibm-plex/IBMPlexSans.ttf");

/// The strip's height, points.
pub const STRIP_PT: f64 = 18.0;
/// The name's size, points.
const NAME_PT: f64 = 11.0;
/// Space before the name, points.
const PAD_PT: f64 = 6.0;
/// The name's colour, 0RGB.
const TEXT: u32 = 0x00f2_f2f2;

/// Fills the first `h` rows of `buf` (`w` wide) with `colour` (0RGB)
/// and writes `name` on it at `scale`, from the left, cut at the right
/// edge.
pub fn draw(buf: &mut [u32], w: u32, h: u32, colour: u32, name: &str, scale: f64) {
    let n = (w as usize).saturating_mul(h as usize).min(buf.len());
    let Some(strip) = buf.get_mut(..n) else { return };
    strip.fill(colour);
    let Ok(font) = FontRef::try_from_slice(FONT) else { return };
    let px = PxScale::from((NAME_PT * scale) as f32);
    let sf = font.as_scaled(px);
    // Baseline where the text's ascent and descent sit centred.
    let base = (h as f32 + sf.ascent() + sf.descent()) / 2.0;
    let mut x = (PAD_PT * scale) as f32;
    let mut last = None;
    for c in name.chars() {
        if x >= w as f32 {
            break;
        }
        let id = sf.glyph_id(c);
        if let Some(p) = last {
            x += sf.kern(p, id);
        }
        last = Some(id);
        let g = id.with_scale_and_position(px, point(x, base));
        x += sf.h_advance(id);
        let Some(o) = font.outline_glyph(g) else { continue };
        let b = o.px_bounds();
        o.draw(|gx, gy, cov| {
            let (dx, dy) = (b.min.x as i64 + i64::from(gx), b.min.y as i64 + i64::from(gy));
            if dx < 0 || dy < 0 || dx >= i64::from(w) || dy >= i64::from(h) {
                return;
            }
            if let Some(d) = strip.get_mut((dy * i64::from(w) + dx) as usize) {
                *d = mix(*d, TEXT, cov);
            }
        });
    }
}

/// `a` toward `b` by `t` (0..=1), each channel of 0RGB.
fn mix(a: u32, b: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    [0, 8, 16].iter().fold(0, |o, &s| {
        let (ca, cb) = (((a >> s) & 0xff) as f32, ((b >> s) & 0xff) as f32);
        o | (((ca + (cb - ca) * t).round() as u32) & 0xff) << s
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_strip_shows_the_name_inside_it_on_the_hosts_colour() {
        let (w, h, scale) = (300, 18, 1.0);
        let colour = 0x0040_3050;
        // One row under the strip, to see nothing writes past it.
        let mut buf = vec![0u32; (w * (h + 1)) as usize];
        draw(&mut buf, w, h, colour, "shankar@atrium", scale);
        let strip = &buf[..(w * h) as usize];
        let light = strip.iter().filter(|&&p| p != colour).count();
        assert!(light > 100, "the name drew {light} px");
        assert!(buf[(w * h) as usize..].iter().all(|&p| p == 0), "drew under the strip");
        // A longer name draws more, and an empty one nothing.
        let mut more = vec![0u32; (w * h) as usize];
        draw(&mut more, w, h, colour, "shankar@atrium.example", scale);
        assert!(more.iter().filter(|&&p| p != colour).count() > light);
        let mut none = vec![0u32; (w * h) as usize];
        draw(&mut none, w, h, colour, "", scale);
        assert!(none.iter().all(|&p| p == colour));
        // A name past the window's width is cut, not drawn out of bounds;
        // a buffer shorter than the strip is filled as far as it goes.
        let mut narrow = vec![0u32; 20 * h as usize];
        draw(&mut narrow, 20, h, colour, "a very long host name indeed", 2.0);
        let mut short = vec![0u32; 10];
        draw(&mut short, w, h, colour, "x", scale);
        assert!(short.iter().all(|&p| p == colour));
    }
}
