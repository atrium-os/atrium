//! Colour spaces in v1 (folium.md §7): device spaces by simple formulas,
//! ICC by component count, Indexed by lookup, Separation and DeviceN by
//! their alternate where the tint transform is simple, else by ink
//! coverage. Patterns and shadings are P4: drawn as a light grey.

use std::sync::Arc;

use folium_syntax::{Document, Object};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Rgba {
    pub const BLACK: Rgba = Rgba { r: 0.0, g: 0.0, b: 0.0, a: 1.0 };
    pub fn gray(v: f32) -> Rgba { Rgba { r: v, g: v, b: v, a: 1.0 } }
}

#[derive(Debug, Clone)]
pub enum Space {
    Gray,
    Rgb,
    Cmyk,
    /// Index into a palette of RGB.
    Indexed { base: Box<Space>, hival: usize, lookup: Arc<[u8]> },
    /// One or more inks drawn as grey coverage (v1).
    Inks(usize),
    Pattern,
}

impl Space {
    pub fn components(&self) -> usize {
        match self {
            Space::Gray | Space::Indexed { .. } | Space::Pattern => 1,
            Space::Rgb => 3,
            Space::Cmyk => 4,
            Space::Inks(n) => *n,
        }
    }

    /// The initial colour (§8.6.5).
    pub fn initial(&self) -> Vec<f32> {
        match self {
            Space::Cmyk => vec![0.0, 0.0, 0.0, 1.0],
            Space::Inks(n) => vec![1.0; *n],
            s => vec![0.0; s.components()],
        }
    }

    pub fn to_rgb(&self, v: &[f32]) -> Rgba {
        let g = |i: usize| v.get(i).copied().unwrap_or(0.0).clamp(0.0, 1.0);
        match self {
            Space::Gray => Rgba::gray(g(0)),
            Space::Rgb => Rgba { r: g(0), g: g(1), b: g(2), a: 1.0 },
            Space::Cmyk => {
                let k = 1.0 - g(3);
                Rgba { r: (1.0 - g(0)) * k, g: (1.0 - g(1)) * k, b: (1.0 - g(2)) * k, a: 1.0 }
            }
            Space::Indexed { base, hival, lookup } => {
                let i = (v.first().copied().unwrap_or(0.0).max(0.0) as usize).min(*hival);
                let n = base.components();
                let vals: Vec<f32> = (0..n).map(|k| lookup.get(i * n + k).copied().unwrap_or(0) as f32 / 255.0).collect();
                base.to_rgb(&vals)
            }
            Space::Inks(n) => {
                let cover = (0..*n).map(g).fold(0.0f32, f32::max);
                Rgba::gray(1.0 - cover)
            }
            Space::Pattern => Rgba::gray(0.85),
        }
    }

    pub fn read(doc: &Document, o: &Object, depth: usize) -> Space {
        if depth > 4 {
            return Space::Rgb;
        }
        let o = doc.resolve(o).unwrap_or(Object::Null);
        let (name, arr): (Vec<u8>, Vec<Object>) = match &o {
            Object::Name(n) => (n.as_bytes().to_vec(), Vec::new()),
            Object::Array(a) => match a.first().and_then(|x| doc.resolve(x).ok()) {
                Some(Object::Name(n)) => (n.as_bytes().to_vec(), a.clone()),
                _ => return Space::Gray,
            },
            _ => return Space::Gray,
        };
        match &name[..] {
            b"DeviceGray" | b"G" | b"CalGray" => Space::Gray,
            b"DeviceRGB" | b"RGB" | b"CalRGB" | b"Lab" => Space::Rgb,
            b"DeviceCMYK" | b"CMYK" => Space::Cmyk,
            b"ICCBased" => {
                let n = arr
                    .get(1)
                    .and_then(|x| doc.resolve(x).ok())
                    .and_then(|s| s.as_dict().and_then(|d| d.int("N")))
                    .unwrap_or(3);
                match n {
                    1 => Space::Gray,
                    4 => Space::Cmyk,
                    _ => Space::Rgb,
                }
            }
            b"Indexed" | b"I" => {
                let base = arr.get(1).map(|b| Space::read(doc, b, depth + 1)).unwrap_or(Space::Rgb);
                let hival = arr.get(2).and_then(|x| doc.resolve(x).ok()).and_then(|x| x.as_int()).unwrap_or(0).clamp(0, 255) as usize;
                let lookup: Arc<[u8]> = match arr.get(3).and_then(|x| doc.resolve(x).ok()) {
                    Some(Object::String(s)) => s,
                    Some(Object::Stream(st)) => doc.decode(&st).map(|d| Arc::from(d.data)).unwrap_or_else(|_| Arc::from(Vec::new())),
                    _ => Arc::from(Vec::new()),
                };
                Space::Indexed { base: Box::new(base), hival, lookup }
            }
            b"Separation" => {
                // "All" and black inks are coverage; others too, in v1.
                Space::Inks(1)
            }
            b"DeviceN" => {
                let n = arr.get(1).and_then(|x| doc.resolve(x).ok()).and_then(|x| x.as_array().map(|a| a.len())).unwrap_or(1);
                Space::Inks(n.clamp(1, 32))
            }
            b"Pattern" => Space::Pattern,
            _ => Space::Gray,
        }
    }
}
