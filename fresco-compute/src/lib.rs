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
//! paths, then filled paths ([`fill`]), then texture batches, then glyph
//! batches, each in list order.

pub mod compose;
pub mod damage;
pub mod fill;
pub mod fuzz;
pub mod gpu;
#[cfg(feature = "rdna4")]
pub mod rdna4;
pub mod reference;
pub mod tier2;

/// SPIR-V of the bin kernel (local size 64).
pub const BIN_SPV: &[u8] = include_bytes!("../shaders/bin.comp.spv");
/// SPIR-V of the raster kernel (local size 8x8).
pub const RASTER_SPV: &[u8] = include_bytes!("../shaders/raster.comp.spv");
/// SPIR-V of the shift kernel (local size 8x8): copies a rectangle of
/// words, for the GPU renderer's scroll ([`damage::plan`],
/// [`damage::move_header`]).
pub const SHIFT_SPV: &[u8] = include_bytes!("../shaders/shift.comp.spv");
/// The shift kernel's workgroup edge.
pub const SHIFT_LOCAL: u32 = 8;

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
/// Largest target edge [`Frame::validate`] accepts.
pub const MAX_DIM: u32 = 16384;
/// Most ops per frame [`Frame::validate`] accepts (bounds the work).
pub const MAX_OPS: u32 = 1 << 16;
/// The widest texture edge the kernels sample (common.glsl); wider
/// textures sample as zero.
pub const MAX_TEX_DIM: u32 = 65536;
/// Most entries of the texture table a frame may declare.
pub const MAX_TEXTURES: u32 = 1 << 16;
pub const BIN_LOCAL: u32 = 64;
pub const RASTER_LOCAL: u32 = 8;

pub const KIND_RECT: u32 = 0;
pub const KIND_PATH: u32 = 1;
pub const KIND_TEXTURE: u32 = 2;
pub const KIND_GLYPH: u32 = 3;
/// A filled path ([`fill`]).
pub const KIND_FILL: u32 = 4;

/// The default clear colour, linear RGB (encodes to `#0A808C`), the same as
/// `fresco_vulkan::HeadlessRenderer`.
pub const CLEAR_COLOR: [f32; 4] = [0.00304, 0.2158, 0.2603, 1.0];
/// The clear for a window surface: transparent, so compose shows what is
/// under the window wherever the window draws nothing.
pub const TRANSPARENT: [f32; 4] = [0.0; 4];

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

/// A node of a [`Scene`], as pack reports one it did not draw.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Node {
    /// Index into [`Scene::rects`].
    Rect(usize),
    /// Index into [`Scene::paths`].
    Path(usize),
    /// Index into [`Scene::fills`].
    Fill(usize),
    /// Batch and quad of [`Scene::texture_batches`].
    Quad(usize, usize),
    /// Batch and run of [`Scene::glyph_batches`].
    Run(usize, usize),
}

/// Everything one frame draws.
#[derive(Clone, Debug, Default)]
pub struct Scene {
    pub rects: Vec<Rect>,
    pub paths: Vec<Path>,
    /// Filled paths, drawn after `paths`.
    pub fills: Vec<fill::Fill>,
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
    /// Entries of the texture table the kernels may read.
    pub tex_count: u32,
    /// Words of `texels` the kernels may read.
    pub texel_words: u32,
    pub _pad: [u32; 3],
    pub clear: [f32; 4],
}

impl Header {
    /// Workgroups for the bin dispatch.
    pub fn bin_groups(&self) -> [u32; 3] {
        [self.op_count.div_ceil(BIN_LOCAL).max(1), 1, 1]
    }

    /// Workgroups for the raster dispatch.
    pub fn raster_groups(&self) -> [u32; 3] {
        [self.width.div_ceil(RASTER_LOCAL), self.height.div_ceil(RASTER_LOCAL), 1]
    }
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

/// Bring `ops` (packed from `scene`, `fill_nodes` naming the fills drawn)
/// within [`MAX_OPS`]: drop the ops off the target, then every node from
/// the one holding op `MAX_OPS` on, whole, reporting those as `OP_BUDGET`.
/// Frames within the cap never come here, so they pack as before.
fn budget(scene: &Scene, fill_nodes: &[usize], ops: &mut Vec<Op>, width: u32, height: u32, dropped: &mut Vec<(Node, u8)>) {
    // The node of each op, in pack order.
    let mut owner = Vec::with_capacity(ops.len());
    owner.extend((0..scene.rects.len()).map(Node::Rect));
    owner.extend((0..scene.paths.len()).map(Node::Path));
    owner.extend(fill_nodes.iter().map(|&k| Node::Fill(k)));
    for (b, t) in scene.texture_batches.iter().enumerate() {
        owner.extend((0..t.quads.len()).map(|q| Node::Quad(b, q)));
    }
    for (b, g) in scene.glyph_batches.iter().enumerate() {
        for (r, run) in g.runs.iter().enumerate() {
            owner.extend(std::iter::repeat_n(Node::Run(b, r), run.glyphs.len()));
        }
    }
    debug_assert_eq!(owner.len(), ops.len());
    let owned: Vec<(Op, Node)> = ops.iter().copied().zip(owner).collect();
    budget_owned(owned, ops, width, height, dropped);
}

/// [`budget`] for ops that already know their node, in paint order.
fn budget_owned(owned: Vec<(Op, Node)>, ops: &mut Vec<Op>, width: u32, height: u32, dropped: &mut Vec<(Node, u8)>) {
    // Culled: no pixel centre of the target inside its box (raster
    // clips every op to p3, so these draw nothing).
    let (w, h) = (width as f32, height as f32);
    let on = |o: &Op| {
        let [x0, y0, x1, y1] = o.p3;
        x0 < w && y0 < h && x1 > 0.0 && y1 > 0.0 && x1 > x0 && y1 > y0
    };
    let kept: Vec<(Op, Node)> = owned.into_iter().filter(|(o, _)| on(o)).collect();
    ops.clear();
    let cap = MAX_OPS as usize;
    if kept.len() <= cap {
        ops.extend(kept.into_iter().map(|(o, _)| o));
        return;
    }
    // The node that crosses the cap goes whole, with all after it.
    let first = kept[cap].1;
    let cut = kept[..cap].iter().position(|(_, n)| *n == first).unwrap_or(cap);
    ops.extend(kept[..cut].iter().map(|(o, _)| *o));
    let mut last = None;
    for (_, n) in &kept[cut..] {
        if last != Some(*n) {
            dropped.push((*n, fill::drop::OP_BUDGET));
            last = Some(*n);
        }
    }
}
/// An element in paint order, with the clip it is drawn under.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placed {
    pub item: Node,
    /// Target pixels `[x0, y0, x1, y1)`; `None`: unclipped.
    pub clip: Option<[f32; 4]>,
}

/// The raster kernel tests each pixel against its op's box (`p3`, f1b66f3),
/// so a clip narrows the box (fresco-scene-groups.md §3 step 4). Were it
/// false, a clip would only cull ops wholly outside it.
pub const CLIP_IN_RASTER: bool = true;

/// `op` under `clip`: `None` when wholly outside it.
fn clip_op(op: &Op, clip: Option<[f32; 4]>) -> Option<Op> {
    let Some(c) = clip else { return Some(*op) };
    let c = bbox(c[0], c[1], c[2], c[3]);
    let b = op.p3;
    let i = [b[0].max(c[0]), b[1].max(c[1]), b[2].min(c[2]), b[3].min(c[3])];
    if i[0] >= i[2] || i[1] >= i[3] {
        return None;
    }
    let mut o = *op;
    if CLIP_IN_RASTER {
        o.p3 = i;
    }
    Some(o)
}

/// A scene's ops, by kind and by element.
struct KindOps {
    rects: Vec<Op>,
    paths: Vec<Op>,
    /// `None`: culled or dropped.
    fills: Vec<Option<Op>>,
    /// Per texture batch, per quad.
    quads: Vec<Vec<Op>>,
    /// Per glyph batch, per run, per glyph.
    runs: Vec<Vec<Vec<Op>>>,
}

#[allow(clippy::type_complexity)]
fn kind_ops(
    scene: &Scene,
    width: u32,
    height: u32,
    mut cache: Option<&mut fill::FillCache>,
) -> (KindOps, Vec<[u32; 4]>, Vec<u32>, Vec<(Node, u8)>) {
    let mut dropped = Vec::new();
    let mut rects = Vec::with_capacity(scene.rects.len());
    for r in &scene.rects {
        let [x, y] = r.position;
        let [w, h] = r.size;
        rects.push(Op {
            p0: [x, y, w, h],
            p1: r.color,
            p2: [r.radius, 0.0, 0.0, 0.0],
            p3: bbox(x, y, x + w, y + h),
            meta: [KIND_RECT, 0, 0, 0],
        });
    }
    let mut paths = Vec::with_capacity(scene.paths.len());
    for p in &scene.paths {
        let [cx, cy, len, wid] = p.model;
        let (s, c) = p.angle.sin_cos();
        let ex = (c * len).abs() * 0.5 + (s * wid).abs() * 0.5;
        let ey = (s * len).abs() * 0.5 + (c * wid).abs() * 0.5;
        paths.push(Op {
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
    // Filled paths: each one's tiled block in `texels` (see `fill`).
    let mut fills = Vec::with_capacity(scene.fills.len());
    let (mut lines_total, mut indices_total) = (0usize, 0usize);
    if let Some(c) = cache.as_deref_mut() {
        c.begin();
    }
    for (k, fl) in scene.fills.iter().enumerate() {
        let r = match cache.as_deref_mut() {
            Some(c) => c.get(fl, width, height),
            None => std::sync::Arc::new(fill::prepare(fl, width, height)),
        };
        let p = match &*r {
            Ok(p) => p,
            Err(fill::CULLED) => {
                fills.push(None);
                continue;
            }
            Err(why) => {
                dropped.push((Node::Fill(k), *why));
                fills.push(None);
                continue;
            }
        };
        if lines_total + p.lines > fill::MAX_FRAME_SEGMENTS || indices_total + p.indices > fill::MAX_FRAME_INDICES {
            dropped.push((Node::Fill(k), fill::drop::FRAME_BUDGET));
            fills.push(None);
            continue;
        }
        lines_total += p.lines;
        indices_total += p.indices;
        let base = texels.len();
        fills.push(Some(Op {
            p0: p.clip,
            p1: fl.color,
            p2: [0.0; 4],
            p3: p.bbox,
            meta: [KIND_FILL, base as u32, 0, 0],
        }));
        texels.extend_from_slice(&p.block);
        fill::rebase(&mut texels[base..], base as u32);
    }
    if texels.is_empty() {
        texels.push(0); // a binding may not be empty
    }
    let quads = scene.texture_batches.iter().map(|b| {
        b.quads.iter().map(|q| {
            let [x, y, w, h] = *q;
            Op {
                p0: *q,
                p3: bbox(x, y, x + w, y + h),
                meta: [KIND_TEXTURE, b.texture as u32, 0, 0],
                ..Default::default()
            }
        }).collect()
    }).collect();
    let runs = scene.glyph_batches.iter().map(|b| {
        let t = &scene.textures[b.atlas];
        let (aw, ah) = (t.width as f32, t.height as f32);
        b.runs.iter().map(|run| {
            run.glyphs.iter().map(|g| {
                // As op_glyph_run: bearing_y is baseline-to-top.
                let x = run.origin[0] + g.d_offset[0] + g.bearing[0];
                let y = run.origin[1] + g.d_offset[1] - g.bearing[1];
                let [u, v, w, h] = g.atlas_uv;
                Op {
                    p0: [x, y, w, h],
                    p1: run.color,
                    p2: [u / aw, v / ah, (u + w) / aw, (v + h) / ah],
                    p3: bbox(x, y, x + w, y + h),
                    meta: [KIND_GLYPH, b.atlas as u32, 0, 0],
                }
            }).collect()
        }).collect()
    }).collect();
    (KindOps { rects, paths, fills, quads, runs }, tex, texels, dropped)
}

fn finish(mut ops: Vec<Op>, mut tex: Vec<[u32; 4]>, texels: Vec<u32>, tex_count: usize, width: u32, height: u32, pitch: u32) -> Frame {
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
        tex_count: tex_count as u32,
        texel_words: texels.len() as u32,
        _pad: [0; 3],
        clear: CLEAR_COLOR,
    };
    if ops.is_empty() {
        ops.push(Op::default()); // a binding may not be empty
    }
    Frame { header, ops, tex, texels, lut: lut() }
}

impl Frame {
    /// Pack `scene` for a `width` x `height` target with `pitch` pixels per
    /// line.
    pub fn pack(scene: &Scene, width: u32, height: u32, pitch: u32) -> Frame {
        Self::pack_report(scene, width, height, pitch).0
    }

    /// [`Frame::pack`], and the nodes it did not draw, with a `fill::drop`
    /// reason: the fills it could not tile, then, when the frame passes
    /// [`MAX_OPS`] ops after culling, every node in paint order from the
    /// one that crosses it on (`OP_BUDGET`). A node that is off the target
    /// (culled) is not listed.
    pub fn pack_report(scene: &Scene, width: u32, height: u32, pitch: u32) -> (Frame, Vec<(Node, u8)>) {
        Self::pack_cached(scene, width, height, pitch, None)
    }

    /// [`Frame::pack_report`], taking fills that did not change since an
    /// earlier pack from `cache` (the frame is the same either way).
    pub fn pack_cached(
        scene: &Scene,
        width: u32,
        height: u32,
        pitch: u32,
        cache: Option<&mut fill::FillCache>,
    ) -> (Frame, Vec<(Node, u8)>) {
        assert!(pitch >= width);
        let (k, tex, texels, mut dropped) = kind_ops(scene, width, height, cache);
        let fill_nodes: Vec<usize> = (0..k.fills.len()).filter(|&i| k.fills[i].is_some()).collect();
        // Draw order by kind: rects, paths, fills, textures, glyphs.
        let mut ops = k.rects;
        ops.extend(k.paths);
        ops.extend(k.fills.into_iter().flatten());
        ops.extend(k.quads.into_iter().flatten());
        ops.extend(k.runs.into_iter().flatten().flatten());
        if ops.len() > MAX_OPS as usize {
            budget(scene, &fill_nodes, &mut ops, width, height, &mut dropped);
        }
        (finish(ops, tex, texels, scene.textures.len(), width, height, pitch), dropped)
    }

    /// Pack `scene` drawing its elements in the order `order` names them,
    /// kinds interleaved (fresco-scene-groups.md §3). An element not named
    /// is not drawn. Each element may carry a clip (target pixels,
    /// `[x0, y0, x1, y1)`): its ops' boxes are narrowed to it, and an op
    /// wholly outside it is culled. A fill's own clip is
    /// `fill::Fill::clip`; set it there too.
    ///
    /// Returns the frame and the nodes not drawn, as [`Frame::pack_report`]
    /// does: fills that could not be tiled, and past [`MAX_OPS`] ops (after
    /// culling) every node in paint order from the one that crosses it on.
    pub fn pack_ordered(
        scene: &Scene,
        order: &[Placed],
        width: u32,
        height: u32,
        pitch: u32,
        cache: Option<&mut fill::FillCache>,
    ) -> (Frame, Vec<(Node, u8)>) {
        assert!(pitch >= width);
        let (k, tex, texels, fill_drops) = kind_ops(scene, width, height, cache);
        // Fills not drawn are reported in paint order, once each.
        let mut dropped = Vec::new();
        let mut owned: Vec<(Op, Node)> = Vec::new();
        for pl in order {
            let slice: &[Op] = match pl.item {
                Node::Rect(i) => k.rects.get(i..=i).unwrap_or(&[]),
                Node::Path(i) => k.paths.get(i..=i).unwrap_or(&[]),
                Node::Fill(i) => match k.fills.get(i) {
                    Some(Some(op)) => std::slice::from_ref(op),
                    _ => {
                        if let Some(d) = fill_drops.iter().find(|d| d.0 == Node::Fill(i)) {
                            dropped.push(*d);
                        }
                        &[]
                    }
                },
                Node::Quad(b, q) => k.quads.get(b).and_then(|v| v.get(q..=q)).unwrap_or(&[]),
                Node::Run(b, r) => k.runs.get(b).and_then(|v| v.get(r)).map(|v| v.as_slice()).unwrap_or(&[]),
            };
            owned.extend(slice.iter().filter_map(|op| clip_op(op, pl.clip)).map(|op| (op, pl.item)));
        }
        let mut ops: Vec<Op>;
        if owned.len() > MAX_OPS as usize {
            ops = Vec::new();
            budget_owned(owned, &mut ops, width, height, &mut dropped);
        } else {
            ops = owned.into_iter().map(|(o, _)| o).collect();
        }
        (finish(ops, tex, texels, scene.textures.len(), width, height, pitch), dropped)
    }

    /// Check the header against the buffers, as the broker must before it
    /// submits a frame it did not pack itself (threat model §2.5.1, F2/F3):
    /// derived fields consistent, every count within its buffer, the target
    /// within `target_words`, and the work bounded. The kernels then need
    /// only the per-element checks they make on data-derived indices.
    pub fn validate(&self, target_words: usize) -> Result<(), String> {
        let h = &self.header;
        let bad = |m: &str| Err(format!("frame header: {m}"));
        if h.width == 0 || h.height == 0 || h.width > MAX_DIM || h.height > MAX_DIM {
            return bad("size out of range");
        }
        if h.pitch < h.width || (h.pitch as u64) * (h.height as u64) > target_words as u64 {
            return bad("pitch or target too small");
        }
        if h.tiles_x != h.width.div_ceil(TILE) || h.tiles_y != h.height.div_ceil(TILE) {
            return bad("tile grid does not match the size");
        }
        if h.op_count > MAX_OPS || h.op_count as usize > self.ops.len() || h.wpt != h.op_count.div_ceil(32) {
            return bad("op count");
        }
        if h.tex_count > MAX_TEXTURES || h.tex_count as usize > self.tex.len() || h.texel_words as usize > self.texels.len() {
            return bad("texture table or texels past their buffers");
        }
        if self.lut.len() != 512 {
            return bad("lut");
        }
        Ok(())
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
        self.header.bin_groups()
    }

    /// Workgroups for the raster dispatch.
    pub fn raster_groups(&self) -> [u32; 3] {
        self.header.raster_groups()
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
        assert_eq!(std::mem::size_of::<Header>(), 64);
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
