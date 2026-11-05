//! The three renderers frescod can draw a frame with.
//!
//! - `compute` (the default, `FRESCOD_RENDERER=compute`): `fresco_compute`'s compute
//!   kernels run on Tier-2, Atrium's own SPIR-V-to-CPU compiler. No Vulkan,
//!   no Mesa, no bundles. Kernels are compiled through `atrium-spv-compile`
//!   (`ATRIUM_SPV_COMPILE`) into `FRESCO_COMPUTE_CACHE`; built with the
//!   `cranelift` feature, frescod compiles them itself when the compiler
//!   binary isn't there.
//! - `gpu` (`FRESCOD_RENDERER=gpu`): the same kernels compiled for the GPU's
//!   architecture (`fresco_compute::gpu`: RDNA4 today, picked from the
//!   device or `FRESCOD_GPU_ARCH`) and submitted on `'A'`. The compose pass
//!   writes the VRAM scanout buffer directly: no read-back, no copy.
//! - `vulkan` (`FRESCOD_RENDERER=vulkan`): the old path,
//!   `fresco_vulkan::HeadlessRenderer` through whatever Vulkan ICD the loader
//!   finds (lavapipe in the VM), with the op bundles. It cannot show split
//!   mode's renderer frames, so with it split mode is off.
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
    if let Some(p) = &w.pixels {
        return (p.width, p.height);
    }
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

/// The GPU's architecture: `FRESCOD_GPU_ARCH` if set, else the device's
/// `CAP_ISA` family.
fn gpu_arch(vm: &Vm<'_>) -> Result<Box<dyn fresco_compute::gpu::Arch>> {
    use fresco_compute::gpu;
    let name = match std::env::var("FRESCOD_GPU_ARCH") {
        Ok(n) if !n.is_empty() => n,
        _ => {
            let c = vm.gpu().caps()?;
            gpu::for_isa(c.isa_family).ok_or_else(|| format!(
                "device {:?} reports ISA family {} ({:?}), which frescod does not know (have: {}); set FRESCOD_GPU_ARCH",
                c.vendor, c.isa_family, c.isa_name, gpu::names().join(", ")))?.to_string()
        }
    };
    gpu::by_name(&name).map_err(|e| format!("{name}: {e}").into())
}

pub enum Backend<'a> {
    Vulkan(Box<HeadlessRenderer>),
    Compute(Box<Compute>),
    Gpu(Box<GpuCompute<'a>>),
}

impl<'a> Backend<'a> {
    /// Pick the renderer from `FRESCOD_RENDERER` (`compute`, the default,
    /// `gpu` or `vulkan`). `load_bundles` is called only for Vulkan; `gpu` needs `vm`.
    pub fn from_env(
        width: u32,
        height: u32,
        vm: Option<&'a Vm<'a>>,
        load_bundles: impl FnOnce(&mut HeadlessRenderer) -> Result<()>,
    ) -> Result<Self> {
        match std::env::var("FRESCOD_RENDERER").as_deref() {
            Ok("compute") | Err(_) => Ok(Backend::Compute(Box::new(Compute::new(width, height)
                .map_err(|e| format!("compute renderer: {e} (FRESCOD_RENDERER=vulkan selects the old renderer)"))?))),
            Ok("gpu") => {
                let vm = vm.ok_or("FRESCOD_RENDERER=gpu needs the GPU (not headless)")?;
                let arch = gpu_arch(vm)?;
                eprintln!("frescod: gpu renderer, architecture {}", arch.name());
                Ok(Backend::Gpu(Box::new(GpuCompute::new(vm, arch, width, height)?)))
            }
            Ok("vulkan") => {
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
            Backend::Gpu(_) => "gpu (compute kernels, a VM per window)",
        }
    }

    /// Whether split mode's renderers should raster their windows themselves
    /// and send pixels: with the compute renderer, so a hostile frame is
    /// rastered in its own renderer's jail, not in frescod (the CPU's
    /// counterpart of the gpu renderer's VM per window). The gpu renderer
    /// takes frames and rasters each in its window's VM.
    /// `FRESCOD_SPLIT_PIXELS=0` sends frames to the compute renderer too.
    pub fn wants_pixels(&self) -> bool {
        matches!(self, Backend::Compute(_)) && std::env::var("FRESCOD_SPLIT_PIXELS").as_deref() != Ok("0")
    }

    /// Whether it can composite frames a split-mode renderer drew
    /// (`ScreenInput::frames`): the compute paths can, Vulkan cannot.
    pub fn takes_frames(&self) -> bool {
        !matches!(self, Backend::Vulkan(_))
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

/// Where frescod keeps its compiled Tier-2 kernels: `FRESCO_COMPUTE_CACHE`,
/// else the system shader cache if frescod may write it, else (frescod runs
/// as `_frescod`, which may not) a directory of its own in the temp dir,
/// created 0700 and refused if someone else made it first.
#[cfg(feature = "cranelift")]
fn kernel_dir() -> Result<std::path::PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    if let Some(d) = std::env::var_os("FRESCO_COMPUTE_CACHE") {
        return Ok(d.into());
    }
    let sys = atrium_spv_loader::LoaderConfig::production().cache_root;
    if std::fs::create_dir_all(&sys).is_ok() && tempfile_ok(&sys) {
        return Ok(sys);
    }
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let d = std::env::temp_dir().join(format!("frescod-kernels-{uid}"));
    match std::fs::DirBuilder::new().mode(0o700).create(&d) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e.into()),
    }
    let m = std::fs::symlink_metadata(&d)?;
    if !m.is_dir() || m.uid() != uid || m.mode() & 0o077 != 0 {
        return Err(format!("{} is not frescod's own private directory", d.display()).into());
    }
    Ok(d)
}

/// Whether frescod can create files in `d`.
#[cfg(feature = "cranelift")]
fn tempfile_ok(d: &std::path::Path) -> bool {
    let p = d.join(format!(".frescod-probe-{}", std::process::id()));
    let ok = std::fs::write(&p, b"").is_ok();
    let _ = std::fs::remove_file(&p);
    ok
}

impl Compute {
    pub fn new(width: u32, height: u32) -> Result<Self> {
        #[cfg(feature = "cranelift")]
        let t2 = {
            let dir = kernel_dir()?;
            fresco_compute::tier2::Tier2::seed_cache(&dir)?;
            let compiler = std::env::var_os("ATRIUM_SPV_COMPILE")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| atrium_spv_loader::LoaderConfig::production().compile_binary);
            fresco_compute::tier2::Tier2::new(dir, compiler)?
        };
        #[cfg(not(feature = "cranelift"))]
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
            if let Some(p) = &w.pixels {
                // Drawn by the window's renderer, in its jail: copied in as is
                // (the same drawing as last time means nothing changed).
                let key = p.gen;
                if self.held.get(&w.id).is_some_and(|h| h.as_slice() == [key]) {
                    continue;
                }
                let off = s[0] as usize;
                self.heap[off..off + p.px.len()].copy_from_slice(&p.px);
                self.held.insert(w.id, vec![key]);
                continue;
            }
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
/// A ring BO: the packets, then the end-of-pipe fence word.
const RING_BYTES: u64 = 64 * 1024;
const FENCE_OFFSET: u64 = RING_BYTES - 256;

/// The compute renderer on the GPU: the kernels compiled for the device's
/// architecture ([`fresco_compute::gpu::Arch`]; RDNA4 is the one built
/// today), every buffer in VRAM. Nothing here is specific to one GPU: the
/// architecture supplies the code and encodes the command stream.
///
/// F4e: every window has a GPU address space (VM) of its own. Its bin and
/// raster run there, and that VM maps only the kernels' code (read-only),
/// the window's own inputs and staging, its ring, and its surface. A
/// window's renderer supplies the data those kernels read, so whatever a
/// hostile frame could make them do, the GPU's page tables keep it inside
/// that window. Compose runs in frescod's own VM, which maps every surface
/// read-only, plus the scanout buffer.
///
/// Per frame, each window whose tiles changed is rastered into its surface
/// (only its dirty tiles), one submission per window in its VM; then
/// compose stacks the surfaces into the scanout buffer (only where a surface
/// changed since that buffer was last composed, or everywhere if the layer
/// list changed) and signals the caller's syncobj. The driver runs a queue's
/// work in order and lets a submission from another VM start only once the
/// queue is idle, so compose sees every window's raster.
pub struct GpuCompute<'a> {
    width: u32,
    height: u32,
    /// frescod's VM: code, compose, the scanout buffers.
    vm: &'a Vm<'a>,
    arch: Box<dyn fresco_compute::gpu::Arch>,
    scene: SceneState,
    /// The frame `prepare` packed, for `submit`.
    pending: Option<Pending>,
    /// The frames last rendered, for hit-testing.
    last: Vec<(u32, Arc<fresco_compute::Frame>)>,
    code: Bo<'a>,
    /// Compose's inputs, its staging, its ring (frescod's VM).
    data: Option<Bo<'a>>,
    staging: Bo<'a>,
    ring: Bo<'a>,
    /// Each window's VM and what it holds, by window id.
    wins: HashMap<u32, WinVm<'a>>,
    /// Bumped each time a window's surface is redrawn.
    version: HashMap<u32, u64>,
    /// Per scanout buffer (by GPU address): the windows, layers and
    /// surface versions it was last composed from.
    composed: HashMap<u64, (Vec<u32>, Vec<Layer>, HashMap<u32, u64>)>,
}

/// One window's address space.
struct WinVm<'a> {
    vm: Vm<'a>,
    /// The code, mapped read-only.
    code: Bo<'a>,
    /// Its surface: written here, mapped read-only into frescod's VM
    /// (`shown`).
    surface: Bo<'a>,
    shown: Bo<'a>,
    size: (u32, u32),
    /// Tile signatures of what the surface holds.
    held: Option<Vec<u64>>,
    data: Option<Bo<'a>>,
    staging: Bo<'a>,
    ring: Bo<'a>,
    sync: Syncobj<'a>,
    /// The last value `sync` will be signalled with.
    seq: u64,
}

impl<'a> WinVm<'a> {
    fn new(gpu: &'a atrium_gpu::amd::Gpu, main: &Vm<'a>, code: &Bo<'a>, w: u32, h: u32) -> std::io::Result<Self> {
        let vm = gpu.create_vm()?;
        let bytes = (w as u64 * h as u64 * 4).next_multiple_of(4096);
        let surface = vm.alloc(bytes, BO_VRAM)?;
        Ok(WinVm {
            code: vm.share(code, true)?,
            shown: main.share(&surface, true)?,
            surface,
            size: (w, h),
            held: None,
            data: None,
            // The CPU fills these; the GPU only reads them.
            staging: vm.alloc_readonly(1 << 20, 0)?,
            ring: vm.alloc(RING_BYTES, 0)?,
            sync: gpu.create_syncobj()?,
            seq: 0,
            vm,
        })
    }
}

struct Pending {
    /// Each window's id and packed frame.
    frames: Vec<(u32, Arc<fresco_compute::Frame>)>,
    /// Each window's screen position.
    pos: HashMap<u32, (i32, i32)>,
}

/// More dirty rectangles than this raster their bounding box instead.
const MAX_DAMAGE_RECTS: usize = 32;

fn grow<'a>(vm: &Vm<'a>, bo: Option<Bo<'a>>, need: u64, flags: u32) -> std::io::Result<Bo<'a>> {
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

/// Place `bytes` in `stage` at a 256-byte boundary; its offset.
fn place(stage: &mut Vec<u8>, bytes: &[u8]) -> u64 {
    stage.resize(stage.len().next_multiple_of(256), 0);
    let at = stage.len() as u64;
    stage.extend_from_slice(bytes);
    at
}

/// Write `ib` into `ring` and submit it in `vm`, signalling `signal`.
fn run<'a>(vm: &Vm<'a>, ring: &Bo<'a>, ib: &dyn fresco_compute::gpu::Stream, signal: (&Syncobj<'_>, u64), deadline_ns: u32) -> Result<()> {
    let bytes = ib.bytes();
    if bytes.len() as u64 > FENCE_OFFSET {
        return Err(format!("IB of {} bytes does not fit the ring", bytes.len()).into());
    }
    ring.write(0, &bytes)?;
    vm.submit_deadline(ring, ib.len_units(), ENGINE_GFX, Some(signal), deadline_ns)?;
    Ok(())
}

impl<'a> GpuCompute<'a> {
    pub fn new(vm: &'a Vm<'a>, arch: Box<dyn fresco_compute::gpu::Arch>, width: u32, height: u32) -> Result<Self> {
        // The code goes up once, here: every window's VM maps it read-only
        // from its first frame on.
        let code_bytes = arch.code();
        let code = vm.alloc((code_bytes.len() as u64).next_multiple_of(4096), BO_VRAM)?;
        let staging = vm.alloc_readonly((code_bytes.len() as u64).next_power_of_two().max(1 << 20), 0)?;
        let ring = vm.alloc(RING_BYTES, 0)?;
        staging.write(0, &code_bytes)?;
        let mut ib = arch.stream();
        ib.copy(staging.gpu_va(), code.gpu_va(), code_bytes.len() as u64);
        ib.end(ring.gpu_va() + FENCE_OFFSET);
        let done = vm.gpu().create_syncobj()?;
        run(vm, &ring, &*ib, (&done, 1), 0)?;
        drop(ib);
        done.wait(1, flip_wait_ms())?;
        Ok(Self {
            width,
            height,
            vm,
            arch,
            scene: SceneState::default(),
            pending: None,
            last: Vec::new(),
            code,
            data: None,
            staging,
            ring,
            wins: HashMap::new(),
            version: HashMap::new(),
            composed: HashMap::new(),
        })
    }

    /// Apply the frame's uploads, give each window a VM and a surface of its
    /// size, and pack each window's scene; `submit` records and runs it.
    fn prepare(&mut self, f: ScreenInput) -> Result<()> {
        self.scene.apply(f.uploads, f.clears);
        // Windows that are gone take their VMs with them.
        self.wins.retain(|id, _| f.windows.iter().any(|w| w.id == *id));
        self.version.retain(|id, _| f.windows.iter().any(|w| w.id == *id));
        let gpu = self.vm.gpu();
        let mut frames = Vec::with_capacity(f.windows.len());
        for w in &f.windows {
            let (sw, sh) = surface_size(w);
            if self.wins.get(&w.id).is_none_or(|v| v.size != (sw, sh)) {
                // New, or resized: a fresh VM and surface (its contents are gone).
                self.wins.remove(&w.id);
                self.wins.insert(w.id, WinVm::new(gpu, self.vm, &self.code, sw, sh)?);
                *self.version.entry(w.id).or_insert(0) += 1;
                self.composed.clear();
            }
            frames.push((w.id, frame_of(&self.scene, w, [0, sw, sw, sh])));
        }
        self.last = frames.clone();
        let pos = f.windows.iter().map(|w| (w.id, (w.pos.0.round() as i32, w.pos.1.round() as i32))).collect();
        self.pending = Some(Pending { frames, pos });
        Ok(())
    }

    /// The compose layers, and the heap they address: every surface as
    /// frescod's VM maps it, as word offsets from the lowest.
    fn layers(&self, windows: &[(u32, Arc<fresco_compute::Frame>)], pos: &HashMap<u32, (i32, i32)>) -> Result<(Vec<Layer>, u64, u32)> {
        let base = self.wins.values().map(|v| v.shown.gpu_va()).min().unwrap_or(0);
        let mut end = base;
        let mut layers = Vec::new();
        for (id, _) in windows {
            let v = &self.wins[id];
            let off = (v.shown.gpu_va() - base) / 4;
            end = end.max(v.shown.gpu_va() + v.size.0 as u64 * v.size.1 as u64 * 4);
            let off = u32::try_from(off).map_err(|_| "surfaces spread over more than 16 GiB of VA")?;
            let (x, y) = pos[id];
            layers.push(Layer::at([off, v.size.0, v.size.0, v.size.1], x, y, 1.0));
        }
        let words = u32::try_from((end - base) / 4).map_err(|_| "surfaces spread over more than 16 GiB of VA")?;
        Ok((layers, base, words.max(1)))
    }

    fn submit(&mut self, target: &Bo<'_>, deadline_ns: u32, signal: (&Syncobj<'_>, u64)) -> Result<()> {
        use fresco_compute::{binding, compose::binding as cb, damage};
        let p = self.pending.take().ok_or("present before render")?;
        if (self.width * self.height) as u64 * 4 > target.size() {
            return Err("scanout buffer smaller than the screen".into());
        }
        let name = self.arch.name();

        // FRESCOD_GPU_ISOLATION_PROBE=1 (a test): every window after the
        // first rasters to the address the first window's surface has in
        // frescod's VM. With one VM that overwrote the first window; with a
        // VM per window it must fault (the work is lost, EIO) or land in the
        // window's own memory, and the first window must stay as it was.
        let probe = std::env::var_os("FRESCOD_GPU_ISOLATION_PROBE").is_some()
            .then(|| p.frames.first().and_then(|(id, _)| self.wins.get(id)).map(|v| v.shown.gpu_va()))
            .flatten();
        // Each window whose tiles changed: its dirty rects, in its own VM.
        for (n, (id, f)) in p.frames.iter().enumerate() {
            let v = self.wins.get_mut(id).ok_or("window without a VM")?;
            let sig = damage::tile_signatures(f);
            let dirty = damage::dirty(&sig, v.held.as_deref());
            let rects = damage::rects(&dirty, f.header.tiles_x, f.header.tiles_y, MAX_DAMAGE_RECTS);
            v.held = Some(sig);
            if rects.is_empty() {
                continue;
            }
            *self.version.entry(*id).or_insert(0) += 1;
            let mut stage = Vec::new();
            let mut offs = [0u64; binding::COUNT];
            for (b, bytes) in f.buffers().into_iter().enumerate() {
                if b as u32 != binding::TARGET {
                    offs[b] = place(&mut stage, &bytes);
                }
            }
            v.data = Some(grow(&v.vm, v.data.take(), stage.len() as u64, BO_VRAM)?);
            if v.staging.size() < stage.len() as u64 {
                v.staging = v.vm.alloc_readonly((stage.len() as u64).next_power_of_two(), 0)?;
            }
            v.staging.write(0, &stage)?;
            let data_va = v.data.as_ref().unwrap().gpu_va();
            let mut va: [u64; binding::COUNT] = std::array::from_fn(|b| data_va + offs[b]);
            va[binding::TARGET as usize] = match probe {
                Some(other) if n > 0 => other,
                _ => v.surface.gpu_va(),
            };
            let mut ib = self.arch.stream();
            ib.copy(v.staging.gpu_va(), data_va, stage.len() as u64);
            ib.frame(v.code.gpu_va(), &va, f, &rects).map_err(|e| format!("{name} record: {e}"))?;
            ib.end(v.ring.gpu_va() + FENCE_OFFSET);
            v.seq += 1;
            run(&v.vm, &v.ring, &*ib, (&v.sync, v.seq), deadline_ns)?;
            if probe.is_some() && n > 0 {
                let r = v.sync.wait(v.seq, flip_wait_ms());
                eprintln!("frescod: isolation probe: window {id} raster to {:#x} (window {} in frescod's VM): {r:?}",
                    va[binding::TARGET as usize], p.frames[0].0);
            }
        }

        // What to compose: everywhere if the layers changed since this
        // scanout buffer was composed, else where a surface changed.
        let (layers, heap_va, heap_words) = self.layers(&p.frames, &p.pos)?;
        let versions: HashMap<u32, u64> = p.frames.iter().map(|(id, _)| (*id, self.version.get(id).copied().unwrap_or(0))).collect();
        let ids: Vec<u32> = p.frames.iter().map(|(id, _)| *id).collect();
        let screen_rects: Option<Vec<[u32; 4]>> = match self.composed.get(&target.gpu_va()) {
            Some((held_ids, old_layers, held)) if *held_ids == ids => {
                let (w, h) = (self.width, self.height);
                let mut r = Vec::new();
                for (((id, _), l), old) in p.frames.iter().zip(&layers).zip(old_layers) {
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
        let compose = Compose::new(&layers, self.width, self.height, self.width, heap_words);
        compose.validate(compose.target_words(), heap_words as usize)?;
        let mut ib = self.arch.stream();
        if screen_rects.as_ref().is_none_or(|r| !r.is_empty()) {
            let mut stage = Vec::new();
            let [chdr, clayers, clut] = compose.buffers();
            let offs = [place(&mut stage, &chdr), place(&mut stage, &clayers), place(&mut stage, &clut)];
            self.data = Some(grow(self.vm, self.data.take(), stage.len() as u64, BO_VRAM)?);
            if self.staging.size() < stage.len() as u64 {
                self.staging = self.vm.alloc_readonly((stage.len() as u64).next_power_of_two(), 0)?;
            }
            self.staging.write(0, &stage)?;
            let data_va = self.data.as_ref().unwrap().gpu_va();
            ib.copy(self.staging.gpu_va(), data_va, stage.len() as u64);
            let mut cva = [0u64; cb::COUNT];
            cva[cb::HEADER as usize] = data_va + offs[0];
            cva[cb::LAYERS as usize] = data_va + offs[1];
            cva[cb::LUT as usize] = data_va + offs[2];
            cva[cb::HEAP as usize] = heap_va;
            cva[cb::TARGET as usize] = target.gpu_va();
            let rects = screen_rects.as_deref().unwrap_or(&[]);
            ib.compose(self.code.gpu_va(), &cva, &compose, rects).map_err(|e| format!("{name} compose record: {e}"))?;
        }
        log::debug!("gpu: {} windows, compose {:?}", p.frames.len(), screen_rects.as_ref().map(|r| r.len()));
        self.composed.insert(target.gpu_va(), (ids, layers, versions));
        // End of pipe: the fence, then the interrupt that signals `signal`.
        ib.end(self.ring.gpu_va() + FENCE_OFFSET);
        if let Err(e) = run(self.vm, &self.ring, &*ib, signal, deadline_ns) {
            // EBUSY: a window's work kept the queue past the driver's wait.
            // Wait for every window's work, then try once more.
            let busy = e.downcast_ref::<std::io::Error>().and_then(|e| e.raw_os_error()) == Some(libc::EBUSY);
            if !busy {
                return Err(e);
            }
            for v in self.wins.values() {
                if v.seq > 0 {
                    v.sync.wait(v.seq, flip_wait_ms())?;
                }
            }
            run(self.vm, &self.ring, &*ib, signal, deadline_ns)?;
        }
        Ok(())
    }
}

/// How long frescod waits on GPU work it depends on: the flip timeout
/// (`FRESCOD_FLIP_TIMEOUT_MS`, raised on gpusim, where a frame takes
/// seconds).
fn flip_wait_ms() -> u32 {
    std::env::var("FRESCOD_FLIP_TIMEOUT_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(2000)
}
