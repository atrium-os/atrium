//! The two renderers frescod can draw a frame with.
//!
//! - `vulkan` (default): `fresco_vulkan::HeadlessRenderer` through whatever
//!   Vulkan ICD the loader finds (lavapipe in the VM today), with the op
//!   bundles.
//! - `compute` (`FRESCOD_RENDERER=compute`): `fresco_compute`'s two compute
//!   kernels run on Tier-2, Atrium's own SPIR-V-to-CPU compiler. No Vulkan,
//!   no Mesa, no bundles. Kernels are compiled through `atrium-spv-compile`
//!   (`ATRIUM_SPV_COMPILE`) into `FRESCO_COMPUTE_CACHE`; built with the
//!   `cranelift` feature, frescod compiles them itself when the compiler
//!   binary isn't there.
//! - `gpu` (`FRESCOD_RENDERER=gpu`): the same two kernels compiled for RDNA4
//!   by `atrium-rdna4` and submitted on `'A'`. The raster kernel writes the
//!   VRAM scanout buffer directly: no read-back, no copy.
//!
//! All take the same per-frame input (the node types and texture uploads
//! frescod already assembles) and produce BGRA8 sRGB pixels; [`Backend::present`]
//! puts them in a scanout buffer.

use std::collections::HashMap;
use std::error::Error;

use atrium_gpu::amd::{Bo, Scanout, Syncobj, Vm, BO_VRAM, ENGINE_GFX};
use fresco_vulkan::{
    GlyphRunBatch, HeadlessRenderer, PathNode, SceneNode, TextureBatch, UploadRequest,
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// One frame's input, as frescod's `render_one_frame` assembles it.
pub struct FrameInput {
    pub rects: Vec<SceneNode>,
    pub paths: Vec<PathNode>,
    pub batches: Vec<TextureBatch>,
    pub glyph_batches: Vec<GlyphRunBatch>,
    pub uploads: Vec<UploadRequest>,
    pub clears: Vec<u32>,
}

pub enum Backend<'a> {
    Vulkan(Box<HeadlessRenderer>),
    Compute(Box<Compute>),
    Gpu(Box<GpuCompute<'a>>),
}

impl<'a> Backend<'a> {
    /// Pick the renderer from `FRESCOD_RENDERER` (`vulkan`, `compute` or
    /// `gpu`). `load_bundles` is called only for Vulkan; `gpu` needs `vm`.
    pub fn from_env(
        width: u32,
        height: u32,
        vm: Option<&'a Vm<'a>>,
        load_bundles: impl FnOnce(&mut HeadlessRenderer) -> Result<()>,
    ) -> Result<Self> {
        match std::env::var("FRESCOD_RENDERER").as_deref() {
            Ok("compute") => Ok(Backend::Compute(Box::new(Compute::new(width, height)?))),
            Ok("gpu") => {
                let vm = vm.ok_or("FRESCOD_RENDERER=gpu needs the GPU (not headless)")?;
                Ok(Backend::Gpu(Box::new(GpuCompute::new(vm, width, height)?)))
            }
            Ok("vulkan") | Err(_) => {
                let mut r = HeadlessRenderer::new(width, height)
                    .map_err(|e| format!("HeadlessRenderer::new: {e}"))?;
                load_bundles(&mut r)?;
                Ok(Backend::Vulkan(Box::new(r)))
            }
            Ok(other) => Err(format!("FRESCOD_RENDERER={other}: want vulkan, compute or gpu").into()),
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Backend::Vulkan(_) => "vulkan",
            Backend::Compute(_) => "compute (Tier-2)",
            Backend::Gpu(_) => "gpu (RDNA4 compute)",
        }
    }

    pub fn render(&mut self, f: FrameInput) -> Result<()> {
        match self {
            Backend::Vulkan(r) => {
                r.process_uploads(f.uploads, f.clears)?;
                r.set_rect_nodes(f.rects);
                r.set_path_nodes(f.paths);
                r.set_texture_batches(f.batches);
                r.set_glyph_run_batches(f.glyph_batches);
                r.render_to_buffer()?;
                Ok(())
            }
            Backend::Compute(c) => c.render(f),
            Backend::Gpu(g) => {
                g.frame = Some(g.scene.frame(f, g.width, g.height));
                Ok(())
            }
        }
    }

    /// Put the last rendered frame into scanout buffer `i`, signalling
    /// `signal` when it is there. CPU renderers read their pixels into
    /// `staging` and copy them up; the GPU renderer runs the frame's
    /// kernels with buffer `i` as the target.
    pub fn present(
        &mut self,
        scan: &Scanout<'_>,
        i: usize,
        staging: &mut [u8],
        deadline_ns: u32,
        signal: (&Syncobj<'_>, u64),
    ) -> Result<()> {
        match self {
            Backend::Gpu(g) => g.submit(scan.buffer(i), deadline_ns, signal),
            _ => {
                self.read_pixels(staging)?;
                scan.upload(i, staging, deadline_ns, Some(signal))?;
                Ok(())
            }
        }
    }

    /// The last frame, BGRA8, `width * height * 4` bytes.
    pub fn read_pixels(&self, dst: &mut [u8]) -> Result<()> {
        match self {
            Backend::Vulkan(r) => Ok(r.read_pixels(dst)?),
            Backend::Compute(c) => {
                let n = dst.len().min(c.target.len() * 4);
                for (d, w) in dst[..n].chunks_exact_mut(4).zip(&c.target) {
                    d.copy_from_slice(&w.to_le_bytes());
                }
                Ok(())
            }
            Backend::Gpu(_) => Err("the gpu renderer draws into scanout; no read-back".into()),
        }
    }
}

// vk::Format raw values for the two formats Fresco uploads.
const VK_FORMAT_R8_UNORM: i32 = 9;
const VK_FORMAT_R8G8B8A8_SRGB: i32 = 43;

/// The kernels' scene input across frames: the texture slot table.
#[derive(Default)]
pub struct SceneState {
    slots: HashMap<u32, fresco_compute::Texture>,
}

/// The compute renderer on Tier-2.
pub struct Compute {
    width: u32,
    height: u32,
    t2: fresco_compute::tier2::Tier2,
    scene: SceneState,
    target: Vec<u32>,
}

impl Compute {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        #[cfg(feature = "cranelift")]
        {
            let have_compiler = std::env::var_os("ATRIUM_SPV_COMPILE")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| atrium_spv_loader::LoaderConfig::production().compile_binary)
                .exists();
            if !have_compiler {
                let cache = std::env::var_os("FRESCO_COMPUTE_CACHE")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| atrium_spv_loader::LoaderConfig::production().cache_root);
                fresco_compute::tier2::Tier2::seed_cache(&cache)?;
            }
        }
        let t2 = fresco_compute::tier2::Tier2::from_env()?;
        Ok(Self { width, height, t2, scene: SceneState::default(), target: vec![0; (width * height) as usize] })
    }

    fn render(&mut self, f: FrameInput) -> Result<()> {
        let frame = self.scene.frame(f, self.width, self.height);
        self.t2.render(&frame, &mut self.target)?;
        Ok(())
    }
}

impl SceneState {
    fn upload(&mut self, req: UploadRequest) {
        match req {
            UploadRequest::Texture { slot_id, bytes, width, height, format } => {
                let format = match format.as_raw() {
                    VK_FORMAT_R8_UNORM => fresco_compute::TexFormat::R8,
                    VK_FORMAT_R8G8B8A8_SRGB => fresco_compute::TexFormat::Rgba8Srgb,
                    other => {
                        log::warn!("compute: slot {slot_id}: unsupported format {other}; dropped");
                        return;
                    }
                };
                if bytes.len() != (width * height) as usize * format.bytes_per_texel() {
                    log::warn!("compute: slot {slot_id}: {} bytes for {width}x{height}; dropped", bytes.len());
                    return;
                }
                self.slots.insert(slot_id, fresco_compute::Texture { width, height, format, data: bytes });
            }
            UploadRequest::TextureRegion { slot_id, bytes, dst_x, dst_y, width, height } => {
                let Some(t) = self.slots.get_mut(&slot_id) else {
                    log::warn!("compute: TextureRegion for empty slot {slot_id}; dropped");
                    return;
                };
                let bpp = t.format.bytes_per_texel();
                if dst_x + width > t.width || dst_y + height > t.height
                    || bytes.len() != (width * height) as usize * bpp
                {
                    log::warn!("compute: slot {slot_id}: region out of bounds; dropped");
                    return;
                }
                let row = width as usize * bpp;
                for y in 0..height as usize {
                    let d = ((dst_y as usize + y) * t.width as usize + dst_x as usize) * bpp;
                    t.data[d..d + row].copy_from_slice(&bytes[y * row..(y + 1) * row]);
                }
            }
        }
    }

    /// Apply the frame's uploads and pack its scene for the kernels.
    fn frame(&mut self, f: FrameInput, width: u32, height: u32) -> fresco_compute::Frame {
        for s in f.clears {
            self.slots.remove(&s);
        }
        for u in f.uploads {
            self.upload(u);
        }
        use fresco_compute as fc;
        let mut scene = fc::Scene {
            rects: f.rects.iter().map(|n| fc::Rect {
                position: n.position, size: n.size, color: n.color, radius: n.radius,
            }).collect(),
            paths: f.paths.iter().map(|n| fc::Path {
                model: n.model, angle: n.extra[0], color: n.color,
            }).collect(),
            ..Default::default()
        };
        let mut index: HashMap<u32, usize> = HashMap::new();
        let mut tex_index = |slot: u32, scene: &mut fc::Scene| -> Option<usize> {
            if let Some(&i) = index.get(&slot) {
                return Some(i);
            }
            let t = self.slots.get(&slot)?.clone();
            scene.textures.push(t);
            index.insert(slot, scene.textures.len() - 1);
            Some(scene.textures.len() - 1)
        };
        for b in &f.batches {
            let Some(texture) = tex_index(b.slot_id, &mut scene) else { continue };
            scene.texture_batches.push(fc::TextureBatch {
                texture,
                quads: b.nodes.iter().map(|n| n.model).collect(),
            });
        }
        for b in &f.glyph_batches {
            let Some(atlas) = tex_index(b.atlas_slot_id, &mut scene) else { continue };
            let runs = b.nodes.iter().map(|n| {
                let (count, off) = (n.meta[0].max(0) as usize, n.meta[1].max(0) as usize);
                let glyphs = b.glyphs.get(off..off + count).unwrap_or(&[]);
                fc::GlyphRun {
                    origin: [n.origin[0], n.origin[1]],
                    color: n.color,
                    glyphs: glyphs.iter().map(|g| fc::Glyph {
                        d_offset: g.d_offset, atlas_uv: g.atlas_uv, bearing: g.bearing,
                    }).collect(),
                }
            }).collect();
            scene.glyph_batches.push(fc::GlyphBatch { atlas, runs });
        }
        fc::Frame::pack(&scene, width, height, width)
    }
}

/// Bytes per CP DMA copy: well under GFX12 DMA_DATA's 26-bit byte count.
const DMA_CHUNK: u64 = 1 << 20;
/// PM4 DMA_DATA (CP copy, memory to memory) and RELEASE_MEM.
const IT_DMA_DATA: u32 = 0x50;
const IT_RELEASE_MEM: u32 = 0x49;
/// The ring BO: the frame's packets, then the end-of-pipe fence word.
const RING_BYTES: u64 = 64 * 1024;
const FENCE_OFFSET: u64 = RING_BYTES - 256;

/// The compute renderer on the GPU: the kernels compiled for RDNA4, every
/// buffer in VRAM, the target the scanout buffer itself.
///
/// Per frame, the CPU writes the frame's buffers (and, the first time, the
/// code) into one System staging BO; the frame's IB copies them into VRAM
/// with CP DMA, runs bin then raster, and signals the caller's syncobj from
/// an end-of-pipe interrupt.
pub struct GpuCompute<'a> {
    width: u32,
    height: u32,
    vm: &'a Vm<'a>,
    k: fresco_compute::rdna4::Rdna4,
    scene: SceneState,
    frame: Option<fresco_compute::Frame>,
    code: Bo<'a>,
    code_loaded: bool,
    staging: Bo<'a>,
    /// VRAM BO per binding (none for the target, which is the scanout).
    bufs: Vec<Option<Bo<'a>>>,
    ring: Bo<'a>,
    /// Tile signatures of the frame each scanout buffer (by GPU address)
    /// holds: the next frame into that buffer rasters only tiles that differ.
    held: HashMap<u64, Vec<u64>>,
}

/// More dirty rectangles than this raster their bounding box instead.
const MAX_DAMAGE_RECTS: usize = 32;

fn grow<'a>(vm: &'a Vm<'a>, bo: Option<Bo<'a>>, need: u64, flags: u32) -> std::io::Result<Bo<'a>> {
    match bo {
        Some(b) if b.size() >= need => Ok(b),
        _ => vm.alloc(need.next_power_of_two().max(64 * 1024), flags),
    }
}

impl<'a> GpuCompute<'a> {
    pub fn new(vm: &'a Vm<'a>, width: u32, height: u32) -> Result<Self> {
        let k = fresco_compute::rdna4::Rdna4::new().map_err(|e| format!("rdna4 compile: {e}"))?;
        let (_, code_end) = k.code_layout();
        Ok(Self {
            width,
            height,
            vm,
            code: vm.alloc(code_end.next_multiple_of(4096), BO_VRAM)?,
            code_loaded: false,
            staging: vm.alloc(1 << 20, 0)?,
            bufs: (0..fresco_compute::binding::COUNT).map(|_| None).collect(),
            ring: vm.alloc(RING_BYTES, 0)?,
            k,
            scene: SceneState::default(),
            frame: None,
            held: HashMap::new(),
        })
    }

    fn submit(&mut self, target: &Bo<'_>, deadline_ns: u32, signal: (&Syncobj<'_>, u64)) -> Result<()> {
        use fresco_compute::binding;
        let f = self.frame.take().ok_or("present before render")?;
        if f.target_words() as u64 * 4 > target.size() {
            return Err("scanout buffer smaller than the frame".into());
        }
        // Stage: (code once), then every binding but the target, 256-aligned.
        let mut stage: Vec<u8> = Vec::new();
        let mut copies: Vec<(u64, u64, u64)> = Vec::new(); // (staging offset, dst va, bytes)
        if !self.code_loaded {
            let (raster_off, _) = self.k.code_layout();
            let [bin, raster] = self.k.code();
            let mut code = bin;
            code.resize(raster_off as usize, 0);
            code.extend(raster);
            copies.push((0, self.code.gpu_va(), code.len() as u64));
            stage.extend(code);
        }
        let mut va = [0u64; binding::COUNT];
        for (b, bytes) in f.buffers().into_iter().enumerate() {
            if b as u32 == binding::TARGET {
                va[b] = target.gpu_va();
                continue;
            }
            let bo = grow(self.vm, self.bufs[b].take(), bytes.len() as u64, BO_VRAM)?;
            va[b] = bo.gpu_va();
            self.bufs[b] = Some(bo);
            stage.resize(stage.len().next_multiple_of(256), 0);
            copies.push((stage.len() as u64, va[b], bytes.len() as u64));
            stage.extend(bytes);
        }
        self.staging = grow(self.vm, Some(std::mem::replace(&mut self.staging, self.vm.alloc(4096, 0)?)), stage.len() as u64, 0)?;
        self.staging.write(0, &stage)?;

        let mut ib = atrium_rdna4::Blob::compute();
        let src0 = self.staging.gpu_va();
        for (off, dst, len) in copies {
            let mut done = 0;
            while done < len {
                let n = (len - done).min(DMA_CHUNK);
                let (s, d) = (src0 + off + done, dst + done);
                // CP_SYNC (bit 31): later packets wait for the copy.
                ib.packet(IT_DMA_DATA, &[1 << 31, s as u32, (s >> 32) as u32, d as u32, (d >> 32) as u32, n as u32]);
                done += n;
            }
        }
        let (raster_off, _) = self.k.code_layout();
        let addrs = fresco_compute::rdna4::Addrs {
            code: [self.code.gpu_va(), self.code.gpu_va() + raster_off],
            buffers: va,
        };
        // Damage: raster only the tiles that differ from what this buffer holds.
        let sig = fresco_compute::damage::tile_signatures(&f);
        let dirty = fresco_compute::damage::dirty(&sig, self.held.get(&target.gpu_va()).map(|v| v.as_slice()));
        let (tx, ty) = (f.header.tiles_x, f.header.tiles_y);
        let rects = fresco_compute::damage::rects(&dirty, tx, ty, MAX_DAMAGE_RECTS);
        log::debug!("gpu: {} of {} tiles dirty, {} raster dispatches", dirty.iter().filter(|&&d| d).count(), dirty.len(), rects.len());
        self.k.record_tiles(&mut ib, &addrs, &f, &rects).map_err(|e| format!("rdna4 record: {e}"))?;
        self.held.insert(target.gpu_va(), sig);
        // End of pipe: write the fence, then interrupt (signals `signal`).
        let fence = self.ring.gpu_va() + FENCE_OFFSET;
        ib.packet(IT_RELEASE_MEM, &[5 << 8, (2 << 29) | (2 << 24), fence as u32, (fence >> 32) as u32, 1, 0]);
        let bytes = ib.bytes();
        if bytes.len() as u64 > FENCE_OFFSET {
            return Err(format!("frame IB of {} bytes does not fit the ring", bytes.len()).into());
        }
        self.ring.write(0, &bytes)?;
        self.code_loaded = true;
        self.vm.submit_deadline(&self.ring, ib.dw.len() as u32, ENGINE_GFX, Some(signal), deadline_ns)?;
        Ok(())
    }
}
