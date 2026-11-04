//! `compose.comp`: stack window surfaces into the screen from a layer list
//! (threat model §2.5.2, F4).
//!
//! Each window's renderer rasters its own scene into its own surface; the
//! broker then runs this one kernel over every surface with a layer list
//! the window manager decided. The layer list is trusted state, the
//! surfaces are pixels: compose never reads a hostile scene.
//!
//! All surfaces live in one heap binding (on the GPU, surface BOs mapped
//! back to back in the compose pass's VM), each found by its first word
//! and pitch.

use crate::{bytes_of, lut, CLEAR_COLOR, MAX_DIM};

/// Bindings of `compose.comp`.
pub mod binding {
    pub const HEADER: u32 = 0;
    pub const LAYERS: u32 = 1;
    pub const HEAP: u32 = 2;
    pub const TARGET: u32 = 3;
    pub const LUT: u32 = 4;
    pub const COUNT: usize = 5;
}

/// `compose.comp`'s SPIR-V.
pub const COMPOSE_SPV: &[u8] = include_bytes!("../shaders/compose.comp.spv");
/// Invocations per workgroup edge.
pub const COMPOSE_LOCAL: u32 = 8;
/// Most layers one compose may stack.
pub const MAX_LAYERS: u32 = 256;

/// The compose header (binding 0, 48 bytes std430).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Header {
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub layer_count: u32,
    pub heap_words: u32,
    pub _pad: [u32; 3],
    pub clear: [f32; 4],
}

/// One layer (binding 1, 64 bytes).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Layer {
    /// Screen rect it shows in, x0, y0, x1, y1 (exclusive end): the window
    /// rect, clipped by whatever the window manager clips it to.
    pub rect: [i32; 4],
    /// Screen position of surface pixel (0, 0); a scrolled surface sits
    /// up or left of its rect.
    pub origin: [i32; 2],
    /// The surface in the heap: first word, pitch, width, height (pixels).
    pub surface: [u32; 4],
    pub opacity: f32,
    pub _pad: [u32; 5],
}

/// A surface's place in the heap: first word, pitch, width, height.
pub type Surface = [u32; 4];

impl Layer {
    /// `surface` shown whole at `(x, y)`.
    pub fn at(surface: Surface, x: i32, y: i32, opacity: f32) -> Layer {
        let [_, _, w, h] = surface;
        Layer {
            rect: [x, y, x.saturating_add(w as i32), y.saturating_add(h as i32)],
            origin: [x, y],
            surface,
            opacity,
            _pad: [0; 5],
        }
    }
}

/// One compose pass's inputs (the heap and target are the executor's).
#[derive(Clone, Debug)]
pub struct Compose {
    pub header: Header,
    pub layers: Vec<Layer>,
    pub lut: Vec<f32>,
}

impl Compose {
    /// Stack `layers` (bottom first) into a `width` x `height` target with
    /// `pitch` pixels per line, over a heap of `heap_words`.
    pub fn new(layers: &[Layer], width: u32, height: u32, pitch: u32, heap_words: u32) -> Compose {
        let header = Header {
            width,
            height,
            pitch,
            layer_count: layers.len() as u32,
            heap_words,
            _pad: [0; 3],
            clear: CLEAR_COLOR,
        };
        let mut layers = layers.to_vec();
        if layers.is_empty() {
            layers.push(Layer::default()); // a binding may not be empty
        }
        Compose { header, layers, lut: lut() }
    }

    /// Check the pass against its buffers before a dispatch: the target
    /// holds `pitch * height`, every layer's surface lies within the heap
    /// (`heap_len` words), the work is bounded. The layer list is the
    /// broker's own, so a failure here is a broker bug, not an attack.
    pub fn validate(&self, target_words: usize, heap_len: usize) -> Result<(), String> {
        let h = &self.header;
        let bad = |m: String| Err(format!("compose: {m}"));
        if h.width == 0 || h.height == 0 || h.width > MAX_DIM || h.height > MAX_DIM {
            return bad("size out of range".into());
        }
        if h.pitch < h.width || (h.pitch as u64) * (h.height as u64) > target_words as u64 {
            return bad("pitch or target too small".into());
        }
        if h.layer_count > MAX_LAYERS || h.layer_count as usize > self.layers.len() {
            return bad("layer count".into());
        }
        if h.heap_words as usize > heap_len {
            return bad("heap_words past the heap".into());
        }
        for (i, l) in self.layers[..h.layer_count as usize].iter().enumerate() {
            let [off, pitch, w, hh] = l.surface;
            let end = off as u64 + (hh as u64).saturating_sub(1) * pitch as u64 + w as u64;
            if w > pitch || (w > 0 && hh > 0 && end > h.heap_words as u64) {
                return bad(format!("layer {i}: surface outside the heap"));
            }
        }
        if self.lut.len() != 512 {
            return bad("lut".into());
        }
        Ok(())
    }

    /// Words in the target.
    pub fn target_words(&self) -> usize {
        (self.header.pitch * self.header.height) as usize
    }

    /// Workgroups for the whole target.
    pub fn groups(&self) -> [u32; 3] {
        [self.header.width.div_ceil(COMPOSE_LOCAL), self.header.height.div_ceil(COMPOSE_LOCAL), 1]
    }

    /// The header, layer and lut bindings as bytes (heap and target are
    /// the caller's).
    pub fn buffers(&self) -> [Vec<u8>; 3] {
        [bytes_of(std::slice::from_ref(&self.header)), bytes_of(&self.layers), bytes_of(&self.lut)]
    }
}

const NONE: u32 = u32::MAX;

fn sample_at(c: &Compose, l: usize, x: i32, y: i32) -> u32 {
    let ly = &c.layers[l];
    if x < ly.rect[0] || y < ly.rect[1] || x >= ly.rect[2] || y >= ly.rect[3] {
        return NONE;
    }
    let sx = x.wrapping_sub(ly.origin[0]);
    let sy = y.wrapping_sub(ly.origin[1]);
    if sx < 0 || sy < 0 || sx as u32 >= ly.surface[2] || sy as u32 >= ly.surface[3] {
        return NONE;
    }
    let i = ly.surface[0].wrapping_add((sy as u32).wrapping_mul(ly.surface[1])).wrapping_add(sx as u32);
    if i >= c.header.heap_words {
        return NONE;
    }
    i
}

fn opacity(c: &Compose, l: usize) -> f32 {
    let o = c.layers[l].opacity;
    if o >= 0.0 { if o <= 1.0 { o } else { 1.0 } } else { 0.0 }
}

/// `compose.comp` for one pixel.
pub fn pixel(c: &Compose, heap: &[u32], x: u32, y: u32) -> u32 {
    let (ix, iy) = (x as i32, y as i32);
    let n = c.header.layer_count as usize;
    let mut base = 0;
    for top in (0..n).rev() {
        if opacity(c, top) >= 1.0 && sample_at(c, top, ix, iy) != NONE {
            base = top;
            break;
        }
    }
    let dec = &c.lut[..256];
    let h = &c.header;
    let mut dst = [h.clear[0], h.clear[1], h.clear[2]];
    for l in base..n {
        let i = sample_at(c, l, ix, iy);
        if i == NONE {
            continue;
        }
        let w = heap[i as usize];
        let s = [dec[((w >> 16) & 255) as usize], dec[((w >> 8) & 255) as usize], dec[(w & 255) as usize]];
        let o = opacity(c, l);
        for k in 0..3 {
            dst[k] += o * (s[k] - dst[k]);
        }
    }
    let lut = &c.lut;
    let (r, g, b) = (crate::reference::encode(lut, dst[0]), crate::reference::encode(lut, dst[1]), crate::reference::encode(lut, dst[2]));
    b | (g << 8) | (r << 16) | (255 << 24)
}

/// The reference compose: the whole target (padding 0).
pub fn render(c: &Compose, heap: &[u32]) -> Vec<u32> {
    let h = &c.header;
    let mut out = vec![0u32; c.target_words()];
    for y in 0..h.height {
        for x in 0..h.width {
            out[(y * h.pitch + x) as usize] = pixel(c, heap, x, y);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_matches_std430() {
        assert_eq!(std::mem::size_of::<Header>(), 48);
        assert_eq!(std::mem::size_of::<Layer>(), 64);
    }

    #[test]
    fn opaque_layers_pass_pixels_through() {
        // Two 4x4 surfaces, the second over the first at (2, 1).
        let heap: Vec<u32> = (0..32u32).map(|i| 0xff00_0000 | i * 0x010203).collect();
        let layers = [Layer::at([0, 4, 4, 4], 0, 0, 1.0), Layer::at([16, 4, 4, 4], 2, 1, 1.0)];
        let c = Compose::new(&layers, 6, 5, 6, heap.len() as u32);
        c.validate(c.target_words(), heap.len()).unwrap();
        let out = render(&c, &heap);
        assert_eq!(out[0], heap[0]);
        assert_eq!(out[(1 * 6 + 2) as usize], heap[16]);
        assert_eq!(out[(1 * 6 + 1) as usize], heap[4 + 1]);
        assert_eq!(out[(4 * 6) as usize], pixel(&Compose::new(&[], 1, 1, 1, 0), &[], 0, 0), "uncovered is the clear colour");
    }
}
