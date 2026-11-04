//! fresco-compute — Fresco's 2D renderer as two compute kernels.
//!
//! Fresco draws four ops: rounded rects, path segments (rotated rects),
//! textured quads and glyphs from an R8 coverage atlas, blended src-over into
//! an sRGB target. This crate draws them with compute alone:
//!
//! 1. `bin.comp`: one invocation per op sets the op's bit in the mask of every
//!    16x16 tile its bounding box touches. Bit index = op index, so each
//!    tile's mask lists its ops in draw order without sorting.
//! 2. `raster.comp`: one invocation per pixel walks its tile's ops in order,
//!    evaluates coverage the way the Fresco fragment shaders do, blends in
//!    linear RGB and writes sRGB BGRA8 (alpha 255) into a linear buffer with
//!    a pitch: the scanout buffer itself.
//!
//! Only storage buffers are used (no images, no samplers, no shared memory,
//! no barriers), so the same SPIR-V runs on Tier-2 (CPU), the GIR
//! interpreter, and the RDNA4 backend.
//!
//! [`Frame::pack`] turns a [`Scene`] into the kernels' buffers;
//! [`reference::render`] is a Rust model of the two kernels, the oracle the
//! executors are tested against; [`tier2::Tier2`] runs the SPIR-V on the CPU;
//! `rdna4::Rdna4` (feature `rdna4`) records it for an RDNA4 GPU.
//!
//! Draw order matches `fresco_vulkan::HeadlessRenderer`: all rects, then all
//! paths, then texture batches, then glyph batches, each in list order.

#[cfg(feature = "rdna4")]
pub mod rdna4;
pub mod reference;
pub mod tier2;

/// SPIR-V of the bin kernel (local size 64).
pub const BIN_SPV: &[u8] = include_bytes!("../shaders/bin.comp.spv");
/// SPIR-V of the raster kernel (local size 8x8).
pub const RASTER_SPV: &[u8] = include_bytes!("../shaders/raster.comp.spv");

/// Bindings (set 0).
pub mod binding {
    pub const FRAME: u32 = 0;
    pub const OPS: u32 = 1;
    pub const TEX: u32 = 2;
    pub const TEXELS: u32 = 3;
    pub const MASK: u32 = 4;
    pub const TARGET: u32 = 5;
    pub const LUT: u32 = 6;
    pub const COUNT: usize = 7;
}

pub const TILE: u32 = 16;
pub const BIN_LOCAL: u32 = 64;
pub const RASTER_LOCAL: u32 = 8;

pub const KIND_RECT: u32 = 0;
pub const KIND_PATH: u32 = 1;
pub const KIND_TEXTURE: u32 = 2;
pub const KIND_GLYPH: u32 = 3;

/// The default clear colour, linear RGB (encodes to `#0A808C`), the same as
/// `fresco_vulkan::HeadlessRenderer`.
pub const CLEAR_COLOR: [f32; 4] = [0.00304, 0.2158, 0.2603, 1.0];

// ---------------------------------------------------------------------------
// Scene input (mirrors fresco-vulkan's node types)
// ---------------------------------------------------------------------------

/// A rounded rect: `fresco_vulkan::SceneNode`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Rect {
    pub position: [f32; 2],
    pub size: [f32; 2],
    /// Linear RGB, straight alpha.
    pub color: [f32; 4],
    /// Corner radius in pixels, 0 = square.
    pub radius: f32,
}

/// A path segment, a rect rotated about its centre: `fresco_vulkan::PathNode`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Path {
    /// cx, cy, length, width.
    pub model: [f32; 4],
    /// Radians.
    pub angle: f32,
    pub color: [f32; 4],
}

/// Texture formats Fresco uploads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TexFormat {
    /// `R8G8B8A8_SRGB`: filtered after decoding to linear.
    Rgba8Srgb,
    /// `R8_UNORM`: glyph coverage atlases.
    R8,
}

impl TexFormat {
    pub fn bytes_per_texel(self) -> usize {
        match self {
            TexFormat::Rgba8Srgb => 4,
            TexFormat::R8 => 1,
        }
    }
}

/// A texture slot's pixels.
#[derive(Clone, Debug)]
pub struct Texture {
    pub width: u32,
    pub height: u32,
    pub format: TexFormat,
    /// `width * height * bytes_per_texel`, rows packed.
    pub data: Vec<u8>,
}

/// Quads sampling one texture: `fresco_vulkan::TextureBatch`.
#[derive(Clone, Debug)]
pub struct TextureBatch {
    /// Index into [`Scene::textures`].
    pub texture: usize,
    /// x, y, w, h per quad.
    pub quads: Vec<[f32; 4]>,
}

/// One glyph of a run: `fresco_vulkan::GlyphInstance`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Glyph {
    pub d_offset: [f32; 2],
    /// u, v, w, h in atlas pixels.
    pub atlas_uv: [f32; 4],
    pub bearing: [f32; 2],
}

/// A shaped text run: `fresco_vulkan::GlyphRunNode` with its glyphs.
#[derive(Clone, Debug, Default)]
pub struct GlyphRun {
    pub origin: [f32; 2],
    pub color: [f32; 4],
    pub glyphs: Vec<Glyph>,
}

/// Runs sharing one atlas: `fresco_vulkan::GlyphRunBatch`.
#[derive(Clone, Debug)]
pub struct GlyphBatch {
    /// Index into [`Scene::textures`] (an [`TexFormat::R8`] atlas).
    pub atlas: usize,
    pub runs: Vec<GlyphRun>,
}

/// Everything one frame draws.
#[derive(Clone, Debug, Default)]
pub struct Scene {
    pub rects: Vec<Rect>,
    pub paths: Vec<Path>,
    pub textures: Vec<Texture>,
    pub texture_batches: Vec<TextureBatch>,
    pub glyph_batches: Vec<GlyphBatch>,
}

// ---------------------------------------------------------------------------
// Packed frame (the kernels' buffers)
// ---------------------------------------------------------------------------

/// One op as the kernels see it (80 bytes, std430).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Op {
    pub p0: [f32; 4],
    pub p1: [f32; 4],
    pub p2: [f32; 4],
    /// Bounding box x0, y0, x1, y1 in whole pixels (exclusive end).
    pub p3: [f32; 4],
    /// kind, texture index, 0, 0.
    pub meta: [u32; 4],
}

/// The frame header (binding 0).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct Header {
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub tiles_x: u32,
    pub tiles_y: u32,
    pub op_count: u32,
    pub wpt: u32,
    pub _pad: u32,
    pub clear: [f32; 4],
}

/// The packed inputs of one frame. The mask and target are the executor's.
#[derive(Clone, Debug)]
pub struct Frame {
    pub header: Header,
    pub ops: Vec<Op>,
    /// Per texture: first word in `texels`, width, height, format.
    pub tex: Vec<[u32; 4]>,
    pub texels: Vec<u32>,
    pub lut: Vec<f32>,
}

/// sRGB transfer, decode one channel.
pub fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

/// The LUT binding: 256 decode values, then 255 encode thresholds (+1 pad):
/// a linear value `v` encodes to the largest `b` with `v >= enc[b - 1]`,
/// which is exactly `round(encode(v) * 255)`.
pub fn lut() -> Vec<f32> {
    let mut l = Vec::with_capacity(512);
    for b in 0..256 {
        l.push(srgb_to_linear(b as f32 / 255.0));
    }
    for b in 0..255 {
        l.push(srgb_to_linear((b as f32 + 0.5) / 255.0));
    }
    l.push(f32::INFINITY);
    l
}

fn bbox(x0: f32, y0: f32, x1: f32, y1: f32) -> [f32; 4] {
    [x0.floor(), y0.floor(), x1.ceil(), y1.ceil()]
}

impl Frame {
    /// Pack `scene` for a `width` x `height` target with `pitch` pixels per
    /// line.
    pub fn pack(scene: &Scene, width: u32, height: u32, pitch: u32) -> Frame {
        assert!(pitch >= width);
        let mut ops = Vec::new();
        for r in &scene.rects {
            let [x, y] = r.position;
            let [w, h] = r.size;
            ops.push(Op {
                p0: [x, y, w, h],
                p1: r.color,
                p2: [r.radius, 0.0, 0.0, 0.0],
                p3: bbox(x, y, x + w, y + h),
                meta: [KIND_RECT, 0, 0, 0],
            });
        }
        for p in &scene.paths {
            let [cx, cy, len, wid] = p.model;
            let (s, c) = p.angle.sin_cos();
            let ex = (c * len).abs() * 0.5 + (s * wid).abs() * 0.5;
            let ey = (s * len).abs() * 0.5 + (c * wid).abs() * 0.5;
            ops.push(Op {
                p0: p.model,
                p1: p.color,
                p2: [c, s, 0.0, 0.0],
                p3: bbox(cx - ex, cy - ey, cx + ex, cy + ey),
                meta: [KIND_PATH, 0, 0, 0],
            });
        }
        let mut tex = Vec::with_capacity(scene.textures.len());
        let mut texels: Vec<u32> = Vec::new();
        for t in &scene.textures {
            let n = (t.width * t.height) as usize;
            assert_eq!(t.data.len(), n * t.format.bytes_per_texel(), "texture size");
            tex.push([texels.len() as u32, t.width, t.height, (t.format == TexFormat::R8) as u32]);
            match t.format {
                TexFormat::Rgba8Srgb => texels.extend(t.data.chunks_exact(4).map(|c| {
                    u32::from_le_bytes([c[0], c[1], c[2], c[3]])
                })),
                TexFormat::R8 => texels.extend(t.data.chunks(4).map(|c| {
                    let mut b = [0u8; 4];
                    b[..c.len()].copy_from_slice(c);
                    u32::from_le_bytes(b)
                })),
            }
        }
        if texels.is_empty() {
            texels.push(0); // a binding may not be empty
        }
        for b in &scene.texture_batches {
            for q in &b.quads {
                let [x, y, w, h] = *q;
                ops.push(Op {
                    p0: *q,
                    p3: bbox(x, y, x + w, y + h),
                    meta: [KIND_TEXTURE, b.texture as u32, 0, 0],
                    ..Default::default()
                });
            }
        }
        for b in &scene.glyph_batches {
            let t = &scene.textures[b.atlas];
            let (aw, ah) = (t.width as f32, t.height as f32);
            for run in &b.runs {
                for g in &run.glyphs {
                    // As op_glyph_run: bearing_y is baseline-to-top.
                    let x = run.origin[0] + g.d_offset[0] + g.bearing[0];
                    let y = run.origin[1] + g.d_offset[1] - g.bearing[1];
                    let [u, v, w, h] = g.atlas_uv;
                    ops.push(Op {
                        p0: [x, y, w, h],
                        p1: run.color,
                        p2: [u / aw, v / ah, (u + w) / aw, (v + h) / ah],
                        p3: bbox(x, y, x + w, y + h),
                        meta: [KIND_GLYPH, b.atlas as u32, 0, 0],
                    });
                }
            }
        }
        if tex.is_empty() {
            tex.push([0; 4]);
        }
        let tiles_x = width.div_ceil(TILE);
        let tiles_y = height.div_ceil(TILE);
        let header = Header {
            width,
            height,
            pitch,
            tiles_x,
            tiles_y,
            op_count: ops.len() as u32,
            wpt: (ops.len() as u32).div_ceil(32),
            _pad: 0,
            clear: CLEAR_COLOR,
        };
        if ops.is_empty() {
            ops.push(Op::default()); // a binding may not be empty
        }
        Frame { header, ops, tex, texels, lut: lut() }
    }

    /// Words in the tile-mask buffer (at least 1).
    pub fn mask_words(&self) -> usize {
        ((self.header.tiles_x * self.header.tiles_y * self.header.wpt) as usize).max(1)
    }

    /// Words in the target buffer.
    pub fn target_words(&self) -> usize {
        (self.header.pitch * self.header.height) as usize
    }

    /// Workgroups for the bin dispatch.
    pub fn bin_groups(&self) -> [u32; 3] {
        [self.header.op_count.div_ceil(BIN_LOCAL).max(1), 1, 1]
    }

    /// Workgroups for the raster dispatch.
    pub fn raster_groups(&self) -> [u32; 3] {
        [self.header.width.div_ceil(RASTER_LOCAL), self.header.height.div_ceil(RASTER_LOCAL), 1]
    }

    /// The buffers by binding as bytes, with a zeroed mask and target.
    pub fn buffers(&self) -> Vec<Vec<u8>> {
        let mut b = vec![Vec::new(); binding::COUNT];
        b[binding::FRAME as usize] = bytes_of(std::slice::from_ref(&self.header));
        b[binding::OPS as usize] = bytes_of(&self.ops);
        b[binding::TEX as usize] = bytes_of(&self.tex);
        b[binding::TEXELS as usize] = bytes_of(&self.texels);
        b[binding::MASK as usize] = vec![0; self.mask_words() * 4];
        b[binding::TARGET as usize] = vec![0; self.target_words() * 4];
        b[binding::LUT as usize] = bytes_of(&self.lut);
        b
    }
}

/// The bytes of a slice of plain-old-data values.
pub fn bytes_of<T: Copy>(v: &[T]) -> Vec<u8> {
    let n = std::mem::size_of_val(v);
    let mut out = vec![0u8; n];
    // SAFETY: T is Copy plain data (repr(C) structs of u32/f32 here); the
    // source is n readable bytes and the destination n writable bytes.
    unsafe { std::ptr::copy_nonoverlapping(v.as_ptr() as *const u8, out.as_mut_ptr(), n) };
    out
}

/// BGRA8 words to bytes (B, G, R, A in memory).
pub fn target_bytes(words: &[u32]) -> Vec<u8> {
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_matches_std430() {
        assert_eq!(std::mem::size_of::<Op>(), 80);
        assert_eq!(std::mem::size_of::<Header>(), 48);
    }

    #[test]
    fn lut_encodes_exactly() {
        let l = lut();
        for b in 0..256u32 {
            let v = l[b as usize];
            assert_eq!(reference::encode(&l, v), b, "byte {b}");
        }
    }
}
