//! `compose.comp`: stack window surfaces into the screen from a layer list
//! (threat model §2.5.2, F4).
//!
//! Each window's renderer rasters its own scene into its own surface; the
//! broker then runs this one kernel over every surface with a layer list
//! the window manager decided. The layer list is trusted state, the
//! surfaces are pixels: compose never reads a hostile scene.
//!
//! Surfaces hold premultiplied linear RGB (sRGB-encoded) and linear alpha
//! in the top byte, as `raster.comp` writes them over a transparent clear
//! ([`crate::TRANSPARENT`]); each layer blends src-over at its opacity.
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

/// [`pixel`] where the top layer at the pixel that draws anything there
/// is fully opaque (at opacity 1, a source alpha of 255), every layer
/// above it holding nothing at the pixel (a zero word: transparent, adds
/// nothing): then nothing is blended and the pixel is the source's
/// (decoding and encoding again is exact, see `lut`). `None` where a
/// blend is needed.
pub fn opaque_pixel(c: &Compose, heap: &[u32], x: u32, y: u32) -> Option<u32> {
    let (ix, iy) = (x as i32, y as i32);
    for top in (0..c.header.layer_count as usize).rev() {
        let i = sample_at(c, top, ix, iy);
        if i == NONE {
            continue;
        }
        let w = heap[i as usize];
        if w == 0 {
            continue;
        }
        return (opacity(c, top) >= 1.0 && w >> 24 == 255).then_some(w | 0xff00_0000);
    }
    None
}

/// `compose.comp` for one pixel.
pub fn pixel(c: &Compose, heap: &[u32], x: u32, y: u32) -> u32 {
    let (ix, iy) = (x as i32, y as i32);
    let n = c.header.layer_count as usize;
    let mut base = 0;
    for top in (0..n).rev() {
        let ti = sample_at(c, top, ix, iy);
        if opacity(c, top) >= 1.0 && ti != NONE && heap[ti as usize] >> 24 == 255 {
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
        let sa = (w >> 24) as f32 * (1.0 / 255.0);
        for k in 0..3 {
            dst[k] = s[k] * o + dst[k] * (1.0 - sa * o);
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

/// [`over`] with `src` scaled by `k` about its (0, 0): dst pixel (x, y)
/// takes src at ((x + 0.5 - at.0) / k - 0.5, (y + 0.5 - at.1) / k - 0.5),
/// bilinear (in the surfaces' encoding; a zoom transition's frames, not
/// exact), transparent past src's edges.
#[allow(clippy::too_many_arguments)]
pub fn over_scaled(dst: &mut [u32], dst_pitch: u32, src: &[u32], src_pitch: u32, src_size: (u32, u32), rect: [u32; 4], at: (f32, f32), k: f32, opacity: f32, threads: usize) {
    let o = if opacity >= 0.0 { opacity.min(1.0) } else { 0.0 };
    if o <= 0.0 || !(k > 1e-3) {
        return;
    }
    let (sw, sh) = src_size;
    // Where src lands, clipped.
    let x0 = (rect[0] as f32).max(at.0).max(0.0) as u32;
    let y0 = (rect[1] as f32).max(at.1).max(0.0) as u32;
    let x1 = (rect[2] as f32).min((at.0 + sw as f32 * k).ceil()).max(0.0) as u32;
    let y1 = (rect[3] as f32).min((at.1 + sh as f32 * k).ceil()).max(0.0) as u32;
    let dp = dst_pitch as usize;
    if x0 >= x1 || y0 >= y1 || x1 > dst_pitch || (y1 as usize - 1) * dp + x1 as usize > dst.len() || (src_pitch as usize) < sw as usize || src.len() < (sh as usize).saturating_sub(1) * src_pitch as usize + sw as usize {
        return;
    }
    let t = tables();
    let sp = src_pitch as usize;
    let fetch = |x: i64, y: i64| -> u32 {
        if x < 0 || y < 0 || x >= sw as i64 || y >= sh as i64 { 0 } else { src[y as usize * sp + x as usize] }
    };
    let lerp = |a: u32, b: u32, f: u32| -> u32 {
        // Per byte, f in 0..=256.
        let mut out = 0;
        for sh in [0, 8, 16, 24] {
            let (x, y) = ((a >> sh) & 255, (b >> sh) & 255);
            out |= ((x * (256 - f) + y * f + 128) >> 8) << sh;
        }
        out
    };
    let inv = 1.0 / k;
    let row = |y: u32, d: &mut [u32]| {
        let fy = (y as f32 + 0.5 - at.1) * inv - 0.5;
        let (iy, wy) = (fy.floor() as i64, ((fy - fy.floor()) * 256.0) as u32);
        let mut line = Vec::with_capacity((x1 - x0) as usize);
        for x in x0..x1 {
            let fx = (x as f32 + 0.5 - at.0) * inv - 0.5;
            let (ix, wx) = (fx.floor() as i64, ((fx - fx.floor()) * 256.0) as u32);
            let top = lerp(fetch(ix, iy), fetch(ix + 1, iy), wx);
            let bot = lerp(fetch(ix, iy + 1), fetch(ix + 1, iy + 1), wx);
            line.push(lerp(top, bot, wy));
        }
        over_row(&mut d[x0 as usize..x1 as usize], &line, o, t);
    };
    let rows = (y1 - y0) as usize;
    let dst = &mut dst[y0 as usize * dp..];
    let n = if rows * ((x1 - x0) as usize) < 1 << 15 { 1 } else { threads.clamp(1, rows) };
    let per = rows.div_ceil(n);
    std::thread::scope(|s| {
        for (k, band) in dst.chunks_mut(dp * per).enumerate() {
            let first = k * per;
            if first >= rows {
                break;
            }
            let row = &row;
            s.spawn(move || {
                for (i, d) in band.chunks_mut(dp).take(rows - first).enumerate() {
                    row(y0 + (first + i) as u32, d);
                }
            });
        }
    });
}

/// Decode and encode tables for [`over`]: the LUT's 256 decode values,
/// and a 65536-step encode (within one step of [`crate::reference::encode`]).
struct Tables {
    dec: Vec<f32>,
    enc: Vec<u8>,
}

fn tables() -> &'static Tables {
    static T: std::sync::OnceLock<Tables> = std::sync::OnceLock::new();
    T.get_or_init(|| {
        let l = lut();
        let enc = (0..65536u32).map(|i| crate::reference::encode(&l, i as f32 / 65535.0) as u8).collect();
        Tables { dec: l[..256].to_vec(), enc }
    })
}

/// Src-over one row: `s` onto `d` at opacity `o` (0 < o <= 1).
fn over_row(d: &mut [u32], s: &[u32], o: f32, t: &Tables) {
    let full = o >= 1.0;
    let enc = |v: f32| t.enc[(v.clamp(0.0, 1.0) * 65535.0 + 0.5) as usize] as u32;
    for (dp, &sp) in d.iter_mut().zip(s) {
        if sp == 0 {
            continue;
        }
        let sa = sp >> 24;
        if full && sa == 255 {
            *dp = sp;
            continue;
        }
        let k = 1.0 - sa as f32 * (1.0 / 255.0) * o;
        let dw = *dp;
        let ch = |sh: u32| enc(t.dec[((sp >> sh) & 255) as usize] * o + t.dec[((dw >> sh) & 255) as usize] * k);
        let a = (sa as f32 * o + (dw >> 24) as f32 * k + 0.5).min(255.0) as u32;
        *dp = ch(0) | (ch(8) << 8) | (ch(16) << 16) | (a << 24);
    }
}

/// Src-over `src` (`src_size` pixels, `src_pitch` words a row) onto `dst`
/// (`dst_pitch` words a row) inside `rect` (dst pixels `[x0, y0, x1, y1)`)
/// at `opacity`: dst pixel (x, y) takes src pixel (x - at.0, y - at.1);
/// what falls outside `src` is left alone. Both hold surface pixels
/// (premultiplied, sRGB-encoded, linear alpha on top). A wholly opaque
/// source pixel at opacity 1 is copied and a zero one skipped; others blend
/// in linear light as `compose.comp` does, encoded through a table (within
/// one step of the kernels' exact encode). Rows are spread over `threads`.
///
/// frescod draws an animating group this way (a sprite rastered once,
/// moved and faded per frame), so a transition costs a copy of what
/// moved, not a raster.
#[allow(clippy::too_many_arguments)]
pub fn over(dst: &mut [u32], dst_pitch: u32, src: &[u32], src_pitch: u32, src_size: (u32, u32), rect: [u32; 4], at: (i32, i32), opacity: f32, threads: usize) {
    let o = if opacity >= 0.0 { opacity.min(1.0) } else { 0.0 };
    if o <= 0.0 {
        return;
    }
    // The rect, clipped to where src has pixels.
    let lo = |a: u32, d: i32| (a as i64).max(d as i64).max(0) as u32;
    let hi = |a: u32, d: i32, n: u32| (a as i64).min(d as i64 + n as i64).max(0) as u32;
    let (x0, y0) = (lo(rect[0], at.0), lo(rect[1], at.1));
    let (x1, y1) = (hi(rect[2], at.0, src_size.0), hi(rect[3], at.1, src_size.1));
    let dp = dst_pitch as usize;
    if x0 >= x1 || y0 >= y1 || dp == 0 || (y1 as usize - 1) * dp + x1 as usize > dst.len() || x1 > dst_pitch {
        return;
    }
    let sp = src_pitch as usize;
    let (sx0, sy0) = ((x0 as i64 - at.0 as i64) as usize, (y0 as i64 - at.1 as i64) as usize);
    let w = (x1 - x0) as usize;
    if (sy0 + (y1 - y0) as usize - 1) * sp + sx0 + w > src.len() {
        return;
    }
    let t = tables();
    let rows = (y1 - y0) as usize;
    let row = |i: usize, d: &mut [u32]| {
        let s = &src[(sy0 + i) * sp + sx0..][..w];
        over_row(&mut d[x0 as usize..x0 as usize + w], s, o, t);
    };
    let dst = &mut dst[y0 as usize * dp..];
    let n = if rows * w < 1 << 16 { 1 } else { threads.clamp(1, rows) };
    if n == 1 {
        for (i, d) in dst.chunks_mut(dp).take(rows).enumerate() {
            row(i, d);
        }
        return;
    }
    let per = rows.div_ceil(n);
    std::thread::scope(|s| {
        for (k, band) in dst.chunks_mut(dp * per).enumerate() {
            let first = k * per;
            if first >= rows {
                break;
            }
            let row = &row;
            s.spawn(move || {
                for (i, d) in band.chunks_mut(dp).take(rows - first).enumerate() {
                    row(first + i, d);
                }
            });
        }
    });
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

    /// `over` matches compose.comp's blend of the same two surfaces: an
    /// opaque base (the destination), a source at an offset and opacity.
    #[test]
    fn over_matches_the_compose_kernels_blend() {
        let (w, h) = (40u32, 30u32);
        let mut seed = 7u32;
        let mut rnd = || { seed = seed.wrapping_mul(1664525).wrapping_add(1013904223); seed };
        let base: Vec<u32> = (0..w * h).map(|_| 0xff00_0000 | (rnd() & 0xff_ffff)).collect();
        // Premultiplied source: each channel at most its alpha, some zero
        // words, some opaque.
        let src: Vec<u32> = (0..w * h).map(|i| match i % 5 {
            0 => 0,
            1 => 0xff00_0000 | (rnd() & 0xff_ffff),
            _ => {
                let a = rnd() % 256;
                let c = |r: u32| if a == 0 { 0 } else { r % (a + 1) };
                (a << 24) | (c(rnd()) << 16) | (c(rnd()) << 8) | c(rnd())
            }
        }).collect();
        for (op, at) in [(1.0f32, (0, 0)), (0.5, (3, -2)), (0.2, (-5, 4))] {
            let mut heap = base.clone();
            heap.extend_from_slice(&src);
            let layers = [Layer::at([0, w, w, h], 0, 0, 1.0), Layer::at([w * h, w, w, h], at.0, at.1, op)];
            let c = Compose::new(&layers, w, h, w, heap.len() as u32);
            let want = render(&c, &heap);
            let mut got = base.clone();
            over(&mut got, w, &src, w, (w, h), [0, 0, w, h], at, op, 3);
            for (i, (g, e)) in got.iter().zip(&want).enumerate() {
                for sh in [0, 8, 16] {
                    let (a, b) = ((g >> sh) & 255, (e >> sh) & 255);
                    assert!(a.abs_diff(b) <= 1, "pixel {i} at opacity {op}: {g:08x} vs {e:08x}");
                }
                assert_eq!(g >> 24, 255, "an opaque base stays opaque");
            }
        }
    }

    /// At scale 1 and whole-pixel offsets, `over_scaled` is `over`.
    #[test]
    fn over_scaled_at_one_is_over() {
        let (w, h) = (37u32, 23u32);
        let src: Vec<u32> = (0..w * h).map(|i| if i % 3 == 0 { 0 } else { 0xff00_0000 | i.wrapping_mul(2654435761) & 0xff_ffff }).collect();
        let base: Vec<u32> = (0..w * h).map(|i| 0xff00_0000 | i * 77).collect();
        let (mut a, mut b) = (base.clone(), base.clone());
        over(&mut a, w, &src, w, (w, h), [0, 0, w, h], (4, 3), 0.75, 2);
        over_scaled(&mut b, w, &src, w, (w, h), [0, 0, w, h], (4.0, 3.0), 1.0, 0.75, 2);
        assert!(a == b);
    }
}
