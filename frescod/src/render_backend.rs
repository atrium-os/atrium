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
use std::sync::Arc;
use std::error::Error;

use atrium_gpu::amd::{Bo, Scanout, Syncobj, Vm, BO_VRAM, ENGINE_GFX};
use fresco_compute::compose::{Compose, Layer, Surface};
pub use crate::pack::{SceneState, ScreenInput, WindowFrame};
use fresco_vulkan::{
    GlyphRunBatch, HeadlessRenderer, PathNode, SceneNode, TextureBatch,
};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// Every window's nodes in screen coordinates, as one scene (Vulkan).
fn merge(windows: Vec<WindowFrame>) -> (Vec<SceneNode>, Vec<PathNode>, Vec<TextureBatch>, Vec<GlyphRunBatch>) {
    let (mut rects, mut paths) = (Vec::new(), Vec::new());
    let mut tex: std::collections::BTreeMap<u32, Vec<fresco_vulkan::TextureNode>> = Default::default();
    let mut glyphs: std::collections::BTreeMap<u32, GlyphRunBatch> = Default::default();
    for w in windows {
        let (ox, oy) = w.pos;
        rects.extend(w.rects.into_iter().map(|mut n| {
            n.position[0] += ox;
            n.position[1] += oy;
            n
        }));
        paths.extend(w.paths.into_iter().map(|mut n| {
            n.model[0] += ox;
            n.model[1] += oy;
            n
        }));
        for b in w.batches {
            tex.entry(b.slot_id).or_default().extend(b.nodes.into_iter().map(|mut n| {
                n.model[0] += ox;
                n.model[1] += oy;
                n
            }));
        }
        for b in w.glyph_batches {
            let e = glyphs.entry(b.atlas_slot_id).or_insert_with(|| GlyphRunBatch {
                atlas_slot_id: b.atlas_slot_id,
                nodes: Vec::new(),
                glyphs: Vec::new(),
            });
            let base = e.glyphs.len() as i32;
            e.glyphs.extend(b.glyphs);
            e.nodes.extend(b.nodes.into_iter().map(|mut n| {
                n.origin[0] += ox;
                n.origin[1] += oy;
                n.meta[1] += base;
                n
            }));
        }
    }
    let batches = tex.into_iter().map(|(slot_id, nodes)| TextureBatch { slot_id, nodes }).collect();
    (rects, paths, batches, glyphs.into_values().collect())
}

/// Where each window's surface sits in the surface heap.
#[derive(Default)]
struct Surfaces {
    map: HashMap<u32, Surface>,
    words: u32,
}

/// Surfaces start on 256-byte boundaries (GPU buffer alignment).
const SURFACE_ALIGN_WORDS: u32 = 64;

impl Surfaces {
    /// Make room for `wins` (id, width, height). If any is new or resized,
    /// every surface is laid out afresh and `true` says their contents are
    /// gone.
    fn layout(&mut self, wins: &[(u32, u32, u32)]) -> bool {
        if wins.iter().all(|&(id, w, h)| self.map.get(&id).is_some_and(|s| s[2] == w && s[3] == h)) {
            return false;
        }
        self.map.clear();
        let mut next = 0u32;
        for &(id, w, h) in wins {
            self.map.insert(id, [next, w, w, h]);
            next = (next + w * h).next_multiple_of(SURFACE_ALIGN_WORDS);
        }
        self.words = next.max(1);
        true
    }

    /// The compose layer list for `windows`, bottom to top.
    fn layers(&self, windows: &[WindowFrame]) -> Vec<Layer> {
        windows
            .iter()
            .filter_map(|w| {
                let s = *self.map.get(&w.id)?;
                Some(Layer::at(s, w.pos.0.round() as i32, w.pos.1.round() as i32, 1.0))
            })
            .collect()
    }
}

/// A window's surface size, kept within what the kernels accept.
pub fn surface_size(w: &WindowFrame) -> (u32, u32) {
    if let Some(f) = &w.frame {
        return (f.header.width, f.header.height);
    }
    let c = |v: u32| v.clamp(1, fresco_compute::MAX_DIM);
    (c(w.size.0), c(w.size.1))
}

/// A window's frame: the renderer's, or packed here from its nodes.
fn frame_of(scene: &SceneState, w: &WindowFrame, s: Surface) -> Arc<fresco_compute::Frame> {
    match &w.frame {
        Some(f) => f.clone(),
        None => Arc::new(scene.pack(w, s[2], s[3])),
    }
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

    pub fn render(&mut self, f: ScreenInput) -> Result<()> {
        match self {
            Backend::Vulkan(r) => {
                r.process_uploads(f.uploads, f.clears)?;
                let (rects, paths, batches, glyphs) = merge(f.windows);
                r.set_rect_nodes(rects);
                r.set_path_nodes(paths);
                r.set_texture_batches(batches);
                r.set_glyph_run_batches(glyphs);
                r.render_to_buffer()?;
                Ok(())
            }
            Backend::Compute(c) => c.render(f),
            Backend::Gpu(g) => g.prepare(f),
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
    /// The frames of the last render, by window, for hit-testing; empty
    /// for Vulkan, which merges the windows into one scene.
    pub fn frames(&self) -> &[(u32, Arc<fresco_compute::Frame>)] {
        match self {
            Backend::Vulkan(_) => &[],
            Backend::Compute(c) => &c.last,
            Backend::Gpu(g) => &g.last,
        }
    }

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

/// The compute renderer on Tier-2: each window rastered into its own
/// surface (skipped when its tiles' signatures are unchanged), then the
/// surfaces composed into the screen.
pub struct Compute {
    width: u32,
    height: u32,
    t2: fresco_compute::tier2::Tier2,
    scene: SceneState,
    surfaces: Surfaces,
    heap: Vec<u32>,
    /// Tile signatures of what each window's surface holds.
    held: HashMap<u32, Vec<u64>>,
    target: Vec<u32>,
    /// The frames last rendered, for hit-testing.
    last: Vec<(u32, Arc<fresco_compute::Frame>)>,
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
        Ok(Self {
            width,
            height,
            t2,
            scene: SceneState::default(),
            surfaces: Surfaces::default(),
            heap: Vec::new(),
            held: HashMap::new(),
            target: vec![0; (width * height) as usize],
            last: Vec::new(),
        })
    }

    fn render(&mut self, f: ScreenInput) -> Result<()> {
        self.scene.apply(f.uploads, f.clears);
        let sizes: Vec<(u32, u32, u32)> = f.windows.iter().map(|w| {
            let (sw, sh) = surface_size(w);
            (w.id, sw, sh)
        }).collect();
        if self.surfaces.layout(&sizes) {
            self.heap = vec![0; self.surfaces.words as usize];
            self.held.clear();
        }
        self.last.clear();
        for w in &f.windows {
            let s = self.surfaces.map[&w.id];
            let frame = frame_of(&self.scene, w, s);
            self.last.push((w.id, frame.clone()));
            let sig = fresco_compute::damage::tile_signatures(&frame);
            if self.held.get(&w.id) == Some(&sig) {
                continue;
            }
            let (off, len) = (s[0] as usize, (s[1] * s[3]) as usize);
            self.t2.render(&frame, &mut self.heap[off..off + len])?;
            self.held.insert(w.id, sig);
        }
        let layers = self.surfaces.layers(&f.windows);
        let c = Compose::new(&layers, self.width, self.height, self.width, self.heap.len() as u32);
        self.t2.compose(&c, &self.heap, &mut self.target)?;
        Ok(())
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
/// buffer in VRAM.
///
/// Per frame, each window whose tiles changed is rastered into its own
/// surface in one VRAM surface heap (only its dirty tiles), then
/// `compose` stacks the surfaces into the scanout buffer (only where a
/// surface changed since that buffer was last composed, or everywhere if
/// the layer list changed). The CPU writes the frame's buffers (and, the
/// first time, the code) into one System staging BO; the frame's IB copies
/// them into VRAM with CP DMA, runs the dispatches, and signals the
/// caller's syncobj from an end-of-pipe interrupt.
pub struct GpuCompute<'a> {
    width: u32,
    height: u32,
    vm: &'a Vm<'a>,
    k: fresco_compute::rdna4::Rdna4,
    kc: fresco_compute::rdna4::Compose,
    /// Offset of compose's code in `code`.
    compose_off: u64,
    scene: SceneState,
    /// The frame `prepare` packed, for `submit`.
    pending: Option<Pending>,
    /// The frames last rendered, for hit-testing.
    last: Vec<(u32, Arc<fresco_compute::Frame>)>,
    code: Bo<'a>,
    code_loaded: bool,
    staging: Bo<'a>,
    /// The frame's kernel inputs, sub-allocated per window.
    data: Option<Bo<'a>>,
    /// The surface heap.
    heap: Option<Bo<'a>>,
    surfaces: Surfaces,
    /// Tile signatures of what each window's surface holds.
    held: HashMap<u32, Vec<u64>>,
    /// Bumped each time a window's surface is redrawn.
    version: HashMap<u32, u64>,
    /// Per scanout buffer (by GPU address): the windows, layers and
    /// surface versions it was last composed from.
    composed: HashMap<u64, (Vec<u32>, Vec<Layer>, HashMap<u32, u64>)>,
    ring: Bo<'a>,
}

struct Pending {
    /// Each window's id and packed frame.
    frames: Vec<(u32, Arc<fresco_compute::Frame>)>,
    layers: Vec<Layer>,
}

/// More dirty rectangles than this raster their bounding box instead.
const MAX_DAMAGE_RECTS: usize = 32;

fn grow<'a>(vm: &'a Vm<'a>, bo: Option<Bo<'a>>, need: u64, flags: u32) -> std::io::Result<Bo<'a>> {
    match bo {
        Some(b) if b.size() >= need => Ok(b),
        _ => vm.alloc(need.next_power_of_two().max(64 * 1024), flags),
    }
}

/// The screen-space intersection of a layer's rect with the screen.
fn on_screen(l: &Layer, w: u32, h: u32) -> Option<[u32; 4]> {
    let c = |v: i32, hi: u32| v.clamp(0, hi as i32) as u32;
    let r = [c(l.rect[0], w), c(l.rect[1], h), c(l.rect[2], w), c(l.rect[3], h)];
    (r[2] > r[0] && r[3] > r[1]).then_some(r)
}

impl<'a> GpuCompute<'a> {
    pub fn new(vm: &'a Vm<'a>, width: u32, height: u32) -> Result<Self> {
        let k = fresco_compute::rdna4::Rdna4::new().map_err(|e| format!("rdna4 compile: {e}"))?;
        let kc = fresco_compute::rdna4::Compose::new().map_err(|e| format!("rdna4 compose compile: {e}"))?;
        let (_, code_end) = k.code_layout();
        let compose_off = code_end.next_multiple_of(atrium_rdna4::CODE_ALIGN);
        let code_bytes = compose_off + kc.code().len() as u64;
        Ok(Self {
            width,
            height,
            vm,
            code: vm.alloc(code_bytes.next_multiple_of(4096), BO_VRAM)?,
            code_loaded: false,
            staging: vm.alloc(1 << 20, 0)?,
            data: None,
            heap: None,
            ring: vm.alloc(RING_BYTES, 0)?,
            k,
            kc,
            compose_off,
            scene: SceneState::default(),
            pending: None,
            surfaces: Surfaces::default(),
            held: HashMap::new(),
            version: HashMap::new(),
            composed: HashMap::new(),
            last: Vec::new(),
        })
    }

    /// Apply the frame's uploads, lay out the surfaces and pack each
    /// window's scene; `submit` records and runs it.
    fn prepare(&mut self, f: ScreenInput) -> Result<()> {
        self.scene.apply(f.uploads, f.clears);
        let sizes: Vec<(u32, u32, u32)> = f.windows.iter().map(|w| {
            let (sw, sh) = surface_size(w);
            (w.id, sw, sh)
        }).collect();
        if self.surfaces.layout(&sizes) {
            self.forget_surfaces();
        }
        let frames: Vec<(u32, Arc<fresco_compute::Frame>)> = f.windows.iter().map(|w| {
            let s = self.surfaces.map[&w.id];
            (w.id, frame_of(&self.scene, w, s))
        }).collect();
        self.last = frames.clone();
        self.pending = Some(Pending { frames, layers: self.surfaces.layers(&f.windows) });
        Ok(())
    }

    /// The surfaces' contents are gone (new layout or heap).
    fn forget_surfaces(&mut self) {
        self.held.clear();
        self.composed.clear();
        for v in self.version.values_mut() {
            *v += 1;
        }
    }

    fn submit(&mut self, target: &Bo<'_>, deadline_ns: u32, signal: (&Syncobj<'_>, u64)) -> Result<()> {
        use fresco_compute::{binding, compose::binding as cb, damage};
        let p = self.pending.take().ok_or("present before render")?;
        if (self.width * self.height) as u64 * 4 > target.size() {
            return Err("scanout buffer smaller than the screen".into());
        }
        let heap_bytes = self.surfaces.words as u64 * 4;
        if self.heap.as_ref().is_none_or(|h| h.size() < heap_bytes) {
            self.heap = Some(grow(self.vm, self.heap.take(), heap_bytes, BO_VRAM)?);
            self.forget_surfaces();
        }
        let heap_va = self.heap.as_ref().unwrap().gpu_va();

        // What to draw: each window's dirty tile rects.
        let mut draws = Vec::new(); // (frame index, rects)
        for (i, (id, f)) in p.frames.iter().enumerate() {
            let sig = damage::tile_signatures(f);
            let dirty = damage::dirty(&sig, self.held.get(id).map(|v| v.as_slice()));
            let rects = damage::rects(&dirty, f.header.tiles_x, f.header.tiles_y, MAX_DAMAGE_RECTS);
            if !rects.is_empty() {
                draws.push((i, rects));
                *self.version.entry(*id).or_insert(0) += 1;
            }
            self.held.insert(*id, sig);
        }
        // What to compose: everywhere if the layers changed since this
        // scanout buffer was composed, else where a surface changed.
        let versions: HashMap<u32, u64> = p.frames.iter().map(|(id, _)| (*id, self.version.get(id).copied().unwrap_or(0))).collect();
        let ids: Vec<u32> = p.frames.iter().map(|(id, _)| *id).collect();
        let screen_rects: Option<Vec<[u32; 4]>> = match self.composed.get(&target.gpu_va()) {
            // Same windows in the same order: compose where a surface
            // changed, and where a layer moved (its old and new rects).
            Some((held_ids, layers, held)) if *held_ids == ids => {
                let (w, h) = (self.width, self.height);
                let mut r = Vec::new();
                for (((id, _), l), old) in p.frames.iter().zip(&p.layers).zip(layers) {
                    if l != old {
                        r.extend(on_screen(old, w, h));
                        r.extend(on_screen(l, w, h));
                    } else if held.get(id) != versions.get(id) {
                        r.extend(on_screen(l, w, h));
                    }
                }
                Some(r)
            }
            _ => None, // compose the whole screen
        };
        let compose = Compose::new(&p.layers, self.width, self.height, self.width, self.surfaces.words);
        compose.validate(compose.target_words(), self.surfaces.words as usize)?;

        // Stage: (code once), each drawn window's inputs, compose's inputs.
        let mut stage: Vec<u8> = Vec::new();
        let mut code_copy = None;
        if !self.code_loaded {
            let (raster_off, _) = self.k.code_layout();
            let [bin, raster] = self.k.code();
            let mut code = bin;
            code.resize(raster_off as usize, 0);
            code.extend(raster);
            code.resize(self.compose_off as usize, 0);
            code.extend(self.kc.code());
            code_copy = Some(code.len() as u64);
            stage.extend(code);
        }
        let data_start = stage.len().next_multiple_of(256);
        let place = |stage: &mut Vec<u8>, bytes: &[u8]| -> u64 {
            stage.resize(stage.len().next_multiple_of(256), 0);
            let at = (stage.len() - data_start) as u64;
            stage.extend_from_slice(bytes);
            at
        };
        let mut window_offs = Vec::new(); // per draw: binding offsets in `data`
        for (i, _) in &draws {
            let f = &p.frames[*i].1;
            let mut offs = [0u64; binding::COUNT];
            for (b, bytes) in f.buffers().into_iter().enumerate() {
                if b as u32 != binding::TARGET {
                    offs[b] = place(&mut stage, &bytes);
                }
            }
            window_offs.push(offs);
        }
        let compose_compose = screen_rects.as_ref().is_none_or(|r| !r.is_empty());
        let [chdr, clayers, clut] = compose.buffers();
        let compose_offs = [place(&mut stage, &chdr), place(&mut stage, &clayers), place(&mut stage, &clut)];
        let data_bytes = (stage.len() - data_start) as u64;
        self.data = Some(grow(self.vm, self.data.take(), data_bytes, BO_VRAM)?);
        let data_va = self.data.as_ref().unwrap().gpu_va();
        // Grow only when needed: the driver's per-VM VA is a bump allocator
        // that never reuses freed ranges, so a BO per frame runs a VM out of
        // VA (ENOSPC) after a few thousand frames.
        if self.staging.size() < stage.len() as u64 {
            self.staging = grow(self.vm, None, stage.len() as u64, 0)?;
        }
        self.staging.write(0, &stage)?;

        let mut ib = atrium_rdna4::Blob::compute();
        let src0 = self.staging.gpu_va();
        let copy = |ib: &mut atrium_rdna4::Blob, off: u64, dst: u64, len: u64| {
            let mut done = 0;
            while done < len {
                let n = (len - done).min(DMA_CHUNK);
                let (s, d) = (src0 + off + done, dst + done);
                // CP_SYNC (bit 31): later packets wait for the copy.
                ib.packet(IT_DMA_DATA, &[1 << 31, s as u32, (s >> 32) as u32, d as u32, (d >> 32) as u32, n as u32]);
                done += n;
            }
        };
        if let Some(len) = code_copy {
            copy(&mut ib, 0, self.code.gpu_va(), len);
        }
        copy(&mut ib, data_start as u64, data_va, data_bytes);

        let (raster_off, _) = self.k.code_layout();
        let code_va = self.code.gpu_va();
        for ((i, rects), offs) in draws.iter().zip(&window_offs) {
            let (id, f) = &p.frames[*i];
            let s = self.surfaces.map[id];
            let mut va: [u64; binding::COUNT] = std::array::from_fn(|b| data_va + offs[b]);
            va[binding::TARGET as usize] = heap_va + s[0] as u64 * 4;
            let addrs = fresco_compute::rdna4::Addrs { code: [code_va, code_va + raster_off], buffers: va };
            self.k.record_tiles(&mut ib, &addrs, f, rects).map_err(|e| format!("rdna4 record: {e}"))?;
        }
        if compose_compose {
            let mut cva = [0u64; cb::COUNT];
            cva[cb::HEADER as usize] = data_va + compose_offs[0];
            cva[cb::LAYERS as usize] = data_va + compose_offs[1];
            cva[cb::LUT as usize] = data_va + compose_offs[2];
            cva[cb::HEAP as usize] = heap_va;
            cva[cb::TARGET as usize] = target.gpu_va();
            let rects = screen_rects.as_deref().unwrap_or(&[]);
            self.kc.record(&mut ib, code_va + self.compose_off, &cva, &compose, rects)
                .map_err(|e| format!("rdna4 compose record: {e}"))?;
        }
        log::debug!("gpu: {} of {} windows redrawn, compose {:?}", draws.len(), p.frames.len(),
            screen_rects.as_ref().map(|r| r.len()));
        self.composed.insert(target.gpu_va(), (ids, p.layers, versions));
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
