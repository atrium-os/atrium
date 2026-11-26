//! frescod's trusted chrome as a viewer draws it (fenestra.md §11.5): the
//! strip, the focus ring and the rest of the layer frescod builds itself,
//! with the desktop's background colour under every window.
//!
//! The chrome is solid rectangles in a few colours (frescod's text is
//! coverage runs of its build-time glyphs), so it goes as a palette and
//! 10-byte rectangles in whole pixels:
//!
//! ```text
//! background  3 bytes, sRGB
//! width u16, height u16       the layer's size (from the screen's origin)
//! colours u8, then each       4 x f32 LE, linear RGB, straight alpha
//! rects u16, then each        x i16, y i16, w u16, h u16,
//!                             colour u8, radius u8 (half pixels)
//! ```
//!
//! The same body goes in the link's `Chrome` record and in the viewer's
//! control message. On the viewer it is host content like the windows:
//! drawn inside the remote area, above them, where the person sees what
//! the host's frescod says about the host's windows. It never takes the
//! viewer's own trusted lane; the viewer's host strip stays outside the
//! remote area (§11.5).

/// The most colours a chrome uses.
pub const MAX_COLOURS: usize = 64;
/// The most rectangles; with the palette this stays under one control
/// message (64 KiB).
pub const MAX_RECTS: usize = 6000;
/// The largest body.
pub const MAX_BODY: usize = 3 + 4 + 1 + MAX_COLOURS * 16 + 2 + MAX_RECTS * RECT;
const RECT: usize = 10;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: i16,
    pub y: i16,
    pub w: u16,
    pub h: u16,
    pub colour: u8,
    /// Corner radius in half pixels.
    pub radius_half: u8,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Chrome {
    /// What the screen shows where no window is, sRGB.
    pub background: [u8; 3],
    pub size: (u16, u16),
    /// Linear RGB, straight alpha, each in 0..=1.
    pub palette: Vec<[f32; 4]>,
    pub rects: Vec<Rect>,
}

impl Eq for Chrome {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused(pub &'static str);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "fenestra chrome: {}", self.0)
    }
}

impl std::error::Error for Refused {}

impl Chrome {
    /// Build from frescod's rectangles (`x, y, w, h, linear colour,
    /// radius`, in device pixels). Refused when one is not whole pixels
    /// on a 16-bit screen, or there are too many rects or colours: the
    /// viewer then keeps the chrome it had.
    pub fn from_rects(background: [u8; 3], size: (u32, u32), rects: impl IntoIterator<Item = ([f32; 4], [f32; 4], f32)>) -> Result<Chrome, Refused> {
        let side = |v: u32| u16::try_from(v).map_err(|_| Refused("layer too large"));
        let mut c = Chrome { background, size: (side(size.0)?, side(size.1)?), palette: Vec::new(), rects: Vec::new() };
        for ([x, y, w, h], colour, radius) in rects {
            if c.rects.len() == MAX_RECTS {
                return Err(Refused("too many rects"));
            }
            if w <= 0.0 || h <= 0.0 || colour[3] <= 0.0 {
                continue;
            }
            let whole = |v: f32| v.is_finite() && v.fract() == 0.0;
            if ![x, y, w, h].into_iter().all(whole) || !(radius * 2.0).is_finite() || (radius * 2.0).fract() != 0.0 {
                return Err(Refused("a rect not in whole pixels"));
            }
            if !colour.iter().all(|v| (0.0..=1.0).contains(v)) {
                return Err(Refused("a colour out of range"));
            }
            let i16_of = |v: f32| if (f32::from(i16::MIN)..=f32::from(i16::MAX)).contains(&v) { Ok(v as i16) } else { Err(Refused("a rect off the screen")) };
            let u16_of = |v: f32| if (1.0..=f32::from(u16::MAX)).contains(&v) { Ok(v as u16) } else { Err(Refused("a rect off the screen")) };
            let r2 = radius * 2.0;
            if !(0.0..=255.0).contains(&r2) {
                return Err(Refused("a radius too large"));
            }
            let colour = match c.palette.iter().position(|p| p.iter().zip(colour.iter()).all(|(a, b)| a.to_bits() == b.to_bits())) {
                Some(i) => i,
                None if c.palette.len() < MAX_COLOURS => {
                    c.palette.push(colour);
                    c.palette.len() - 1
                }
                None => return Err(Refused("too many colours")),
            };
            c.rects.push(Rect { x: i16_of(x)?, y: i16_of(y)?, w: u16_of(w)?, h: u16_of(h)?, colour: colour as u8, radius_half: r2 as u8 });
        }
        Ok(c)
    }

    pub fn encode(&self) -> Vec<u8> {
        let colours = self.palette.len().min(MAX_COLOURS);
        let rects = self.rects.len().min(MAX_RECTS);
        let mut b = Vec::with_capacity(10 + colours * 16 + rects * RECT);
        b.extend_from_slice(&self.background);
        b.extend_from_slice(&self.size.0.to_le_bytes());
        b.extend_from_slice(&self.size.1.to_le_bytes());
        b.push(colours as u8);
        for p in &self.palette[..colours] {
            for v in p {
                b.extend_from_slice(&v.to_le_bytes());
            }
        }
        b.extend_from_slice(&(rects as u16).to_le_bytes());
        for r in &self.rects[..rects] {
            b.extend_from_slice(&r.x.to_le_bytes());
            b.extend_from_slice(&r.y.to_le_bytes());
            b.extend_from_slice(&r.w.to_le_bytes());
            b.extend_from_slice(&r.h.to_le_bytes());
            b.push(r.colour);
            b.push(r.radius_half);
        }
        b
    }

    /// Parse a body; every byte is accounted for and every rect's colour
    /// is in the palette.
    pub fn decode(b: &[u8]) -> Result<Chrome, Refused> {
        if b.len() > MAX_BODY {
            return Err(Refused("too long"));
        }
        let short = || Refused("short");
        let u16_at = |i: usize| -> Result<u16, Refused> { b.get(i..i + 2).map(|s| u16::from_le_bytes([s[0], s[1]])).ok_or_else(short) };
        let background = [*b.first().ok_or_else(short)?, *b.get(1).ok_or_else(short)?, *b.get(2).ok_or_else(short)?];
        let size = (u16_at(3)?, u16_at(5)?);
        let colours = usize::from(*b.get(7).ok_or_else(short)?);
        if colours > MAX_COLOURS {
            return Err(Refused("too many colours"));
        }
        let mut at = 8;
        let mut palette = Vec::with_capacity(colours);
        for _ in 0..colours {
            let s = b.get(at..at + 16).ok_or_else(short)?;
            let mut p = [0f32; 4];
            for (k, v) in p.iter_mut().enumerate() {
                *v = f32::from_le_bytes([s[4 * k], s[4 * k + 1], s[4 * k + 2], s[4 * k + 3]]);
                if !(0.0..=1.0).contains(v) {
                    return Err(Refused("a colour out of range"));
                }
            }
            palette.push(p);
            at += 16;
        }
        let n = usize::from(u16_at(at)?);
        at += 2;
        if n > MAX_RECTS || b.len() != at + n * RECT {
            return Err(Refused("rects length"));
        }
        let mut rects = Vec::with_capacity(n);
        for c in b[at..].chunks_exact(RECT) {
            let r = Rect {
                x: i16::from_le_bytes([c[0], c[1]]),
                y: i16::from_le_bytes([c[2], c[3]]),
                w: u16::from_le_bytes([c[4], c[5]]),
                h: u16::from_le_bytes([c[6], c[7]]),
                colour: c[8],
                radius_half: c[9],
            };
            if r.w == 0 || r.h == 0 {
                return Err(Refused("an empty rect"));
            }
            if usize::from(r.colour) >= palette.len() {
                return Err(Refused("a colour not in the palette"));
            }
            rects.push(r);
        }
        Ok(Chrome { background, size, palette, rects })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_shares_colours() {
        let red = [1.0, 0.0, 0.0, 1.0];
        let c = Chrome::from_rects([10, 128, 140], (1280, 40), [([0.0, 0.0, 1280.0, 24.0], red, 0.0), ([4.0, 3.0, 50.0, 18.0], [0.5, 0.5, 0.5, 1.0], 9.0), ([-2.0, 30.0, 4.0, 4.0], red, 0.5)]).unwrap();
        assert_eq!(c.palette.len(), 2);
        assert_eq!(c.rects[1].radius_half, 18);
        assert_eq!(Chrome::decode(&c.encode()).unwrap(), c);
    }

    #[test]
    fn refuses_what_it_cannot_carry() {
        let red = [1.0, 0.0, 0.0, 1.0];
        assert!(Chrome::from_rects([0; 3], (10, 10), [([0.5, 0.0, 1.0, 1.0], red, 0.0)]).is_err());
        assert!(Chrome::from_rects([0; 3], (10, 10), [([0.0, 0.0, 1.0, 1.0], [2.0, 0.0, 0.0, 1.0], 0.0)]).is_err());
        assert!(Chrome::from_rects([0; 3], (70000, 10), []).is_err());
        let many = (0..MAX_COLOURS + 1).map(|i| ([0.0, 0.0, 1.0, 1.0], [i as f32 / 100.0, 0.0, 0.0, 1.0], 0.0));
        assert!(Chrome::from_rects([0; 3], (10, 10), many).is_err());
        let lots = (0..MAX_RECTS + 1).map(|_| ([0.0, 0.0, 1.0, 1.0], red, 0.0));
        assert!(Chrome::from_rects([0; 3], (10, 10), lots).is_err());
        // The largest chrome fits one control message.
        assert!(MAX_BODY < 64 * 1024);
    }

    #[test]
    fn decode_refuses_forged_bodies() {
        let c = Chrome::from_rects([1, 2, 3], (100, 30), [([0.0, 0.0, 100.0, 24.0], [0.1, 0.1, 0.1, 1.0], 0.0)]).unwrap();
        let good = c.encode();
        // Every truncation, and one byte too many.
        for n in 0..good.len() {
            assert!(Chrome::decode(&good[..n]).is_err(), "{n}");
        }
        let mut long = good.clone();
        long.push(0);
        assert!(Chrome::decode(&long).is_err());
        // A colour index past the palette.
        let mut bad = good.clone();
        let at = good.len() - 2;
        bad[at] = 1;
        assert!(Chrome::decode(&bad).is_err());
        // A NaN colour.
        let mut nan = good.clone();
        nan[8..12].copy_from_slice(&f32::NAN.to_le_bytes());
        assert!(Chrome::decode(&nan).is_err());
        // An empty rect.
        let mut empty = good;
        let w = empty.len() - 6;
        empty[w..w + 2].copy_from_slice(&0u16.to_le_bytes());
        assert!(Chrome::decode(&empty).is_err());
    }
}
