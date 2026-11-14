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

/// A window's frame: the renderer's, or packed here from its nodes (then
/// the fills it dropped go on `drops`, by window).
fn frame_of(scene: &SceneState, w: &WindowFrame, s: Surface, drops: &mut FillDrops) -> Arc<fresco_compute::Frame> {
    match &w.frame {
        Some(f) => f.clone(),
        None => {
            let (frame, d) = scene.pack_report(w, s[2], s[3]);
            drops.push((w.id, d));
            Arc::new(frame)
        }
    }
}

/// Per window packed here: the fills its last frame dropped (node id,
/// `fresco_protocol::fill_drop` reason).
pub type FillDrops = Vec<(u32, Vec<(u32, u8)>)>;

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

    /// After a device reset (GPU ABI v2.1 §10.3) the gpu renderer's GPU state
    /// may be gone: switch to Tier-2, which needs nothing from the GPU but the
    /// scanout copy, so the screen comes back at once. Returns whether it
    /// switched. Tier-2 takes frames as well, so split mode's renderers keep
    /// working unchanged.
    pub fn fall_back_to_cpu(&mut self, width: u32, height: u32) -> Result<bool> {
        if !matches!(self, Backend::Gpu(_)) {
            return Ok(false);
        }
        *self = Backend::Compute(Box::new(Compute::new(width, height)?));
        Ok(true)
    }

    /// Go back to the gpu renderer once the GPU has stayed up (see
    /// `fall_back_to_cpu`). A failure leaves Tier-2 in place.
    pub fn back_to_gpu(&mut self, vm: &'a Vm<'a>, width: u32, height: u32) -> Result<()> {
        let arch = gpu_arch(vm)?;
        *self = Backend::Gpu(Box::new(GpuCompute::new(vm, arch, width, height)?));
        Ok(())
    }

    /// Rebuild the renderer for a new screen size (a mode switch), keeping
    /// its kind. `vm` is needed for the gpu renderer, `load_bundles` for Vulkan.
    pub fn resize(
        &mut self,
        vm: &'a Vm<'a>,
        width: u32,
        height: u32,
        load_bundles: impl FnOnce(&mut HeadlessRenderer) -> Result<()>,
    ) -> Result<()> {
        match self {
            Backend::Compute(_) => *self = Backend::Compute(Box::new(Compute::new(width, height)?)),
            Backend::Gpu(_) => self.back_to_gpu(vm, width, height)?,
            Backend::Vulkan(_) => {
                let mut r = HeadlessRenderer::new(width, height)
                    .map_err(|e| format!("HeadlessRenderer::new: {e}"))?;
                load_bundles(&mut r)?;
                *self = Backend::Vulkan(Box::new(r));
            }
        }
        Ok(())
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
    /// Whether frames pass through the CPU on their way to scanout, so the
    /// HDR encode (hdr.rs) can run on them: not for the gpu renderer.
    pub fn can_hdr(&self) -> bool { !matches!(self, Backend::Gpu(_)) }

    /// The gpu renderer: it draws one screen straight into scanout.
    pub fn is_gpu(&self) -> bool { matches!(self, Backend::Gpu(_)) }

    /// Whether it composes surfaces given as pixels (video layers, split
    /// pixel mode): the compute renderer on the CPU.
    pub fn takes_pixels(&self) -> bool { matches!(self, Backend::Compute(_)) }

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

    /// The fills the last render dropped, for each window it packed.
    /// GPU renderer: a window's raster is still running (frescod looks
    /// again soon, to show it once it lands).
    pub fn rasters_pending(&self) -> bool {
        matches!(self, Backend::Gpu(g) if g.rasters_pending())
    }

    /// GPU renderer: submit the next slice of window rasters whose running
    /// slice is done (§F4j). Call it every pass of the loop.
    pub fn pump(&mut self) -> Result<()> {
        match self {
            Backend::Gpu(g) => g.pump(),
            _ => Ok(()),
        }
    }

    /// GPU renderer: a window's raster finished and is not on screen yet.
    pub fn raster_landed(&self) -> bool {
        matches!(self, Backend::Gpu(g) if g.raster_landed())
    }

    /// GPU renderer: windows whose frame was refused for its raster cost
    /// since the last call: (window, estimate, limit) in pixel-ops.
    pub fn take_raster_refusals(&mut self) -> Vec<(u32, u64, u64)> {
        match self {
            Backend::Gpu(g) => std::mem::take(&mut g.refusals),
            _ => Vec::new(),
        }
    }

    pub fn take_fill_drops(&mut self) -> FillDrops {
        match self {
            Backend::Compute(c) => std::mem::take(&mut c.fill_drops),
            Backend::Gpu(g) => std::mem::take(&mut g.fill_drops),
            Backend::Vulkan(_) => Vec::new(),
        }
    }

    /// Where the last render changed the screen: `Some([x0, y0, x1, y1))`
    /// (empty when nothing did), `None` when it may have changed anywhere.
    pub fn damage(&self) -> Option<[u32; 4]> {
        match self {
            Backend::Compute(c) => c.damage,
            _ => None,
        }
    }

    /// Put the last rendered frame into scanout buffer `i`, signalling
    /// `signal` when it is there. CPU renderers read their pixels into
    /// `staging` and copy them up (only the screen rect `rect`,
    /// `[x0, y0, x1, y1)`, when given: the buffer holds the rest already,
    /// and `staging` must hold the last frame everywhere else);
    /// the GPU renderer runs the frame's kernels with buffer `i` as the
    /// target.
    #[allow(clippy::too_many_arguments)]
    pub fn present(
        &mut self,
        scan: &Scanout<'_>,
        i: usize,
        staging: &mut [u8],
        rect: Option<[u32; 4]>,
        deadline_ns: u32,
        signal: (&Syncobj<'_>, u64),
        hdr: Option<&crate::hdr::Encoder>,
    ) -> Result<()> {
        match self {
            Backend::Gpu(g) => g.submit(scan.buffer(i), deadline_ns, signal),
            Backend::Compute(c) => {
                let pitch = c.width as usize * 4;
                let rows = (staging.len() / pitch).min(c.height as usize) as u32;
                let [x0, y0, x1, y1] = rect.map_or([0, 0, c.width, rows], |r| [r[0].min(c.width), r[1].min(rows), r[2].min(c.width), r[3].min(rows)]);
                // The flip still needs its copy: one pixel at least.
                let [x0, y0, x1, y1] = if x1 > x0 && y1 > y0 { [x0, y0, x1, y1] } else { [0, 0, 1, 1] };
                let (a, b) = (x0 as usize, x1 as usize);
                for y in y0 as usize..y1 as usize {
                    let line = &mut staging[y * pitch + a * 4..y * pitch + b * 4];
                    let src = &c.target[y * c.width as usize + a..y * c.width as usize + b];
                    for (d, w) in line.chunks_exact_mut(4).zip(src) {
                        d.copy_from_slice(&w.to_le_bytes());
                    }
                    if let Some(e) = hdr {
                        e.encode(line);
                    }
                }
                // `staging` is current everywhere (each frame converts at
                // least what changed), so one span from the rect's first
                // pixel to its last goes up as is: one copy, not one per
                // line, and the columns beside the rect are what they were.
                let range = y0 as usize * pitch + a * 4..(y1 as usize - 1) * pitch + b * 4;
                scan.upload_range(i, staging, range, deadline_ns, Some(signal))?;
                Ok(())
            }
            _ => {
                self.read_pixels(staging)?;
                if let Some(e) = hdr {
                    e.encode(staging);
                }
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
    /// The layers `target` was last composed from: while they stay the
    /// same, only where a surface changed is composed again.
    composed: Option<Vec<Layer>>,
    /// Where the last render changed `target` (see [`Backend::damage`]).
    damage: Option<[u32; 4]>,
    fill_drops: FillDrops,
}

/// Where the installer puts frescod's precompiled Tier-2 kernels
/// (`fresco-renderer --seed-kernels`, run by `scripts/fresco-renderer-bundle.sh`).
/// The jailed service mounts it read-only and writes nothing.
#[cfg(feature = "cranelift")]
const INSTALLED_KERNELS: &str = "/usr/local/share/atrium/fresco-kernels";

/// Where frescod finds (or compiles) its Tier-2 kernels, seeded: the first
/// of `FRESCO_COMPUTE_CACHE`; the installed kernels if they are complete
/// (seeding them then writes nothing); the system shader cache if frescod
/// may write it; else (frescod runs as `_frescod`, which may not) a
/// directory of its own in the temp dir, created 0700 and refused if
/// someone else made it first.
#[cfg(feature = "cranelift")]
fn kernel_dir() -> Result<std::path::PathBuf> {
    use fresco_compute::tier2::Tier2;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    if let Some(d) = std::env::var_os("FRESCO_COMPUTE_CACHE") {
        let d = std::path::PathBuf::from(d);
        Tier2::seed_cache(&d)?;
        return Ok(d);
    }
    let installed = std::path::PathBuf::from(INSTALLED_KERNELS);
    if installed.is_dir() && Tier2::seed_cache(&installed).is_ok() {
        return Ok(installed);
    }
    let sys = atrium_spv_loader::LoaderConfig::production().cache_root;
    if std::fs::create_dir_all(&sys).is_ok() && tempfile_ok(&sys) && Tier2::seed_cache(&sys).is_ok() {
        return Ok(sys);
    }
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let d = std::env::temp_dir().join(format!("frescod-kernels-{uid}"));
    match std::fs::DirBuilder::new().mode(0o700).create(&d) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(format!("{}: {e}", d.display()).into()),
    }
    let m = std::fs::symlink_metadata(&d)?;
    if !m.is_dir() || m.uid() != uid || m.mode() & 0o077 != 0 {
        return Err(format!("{} is not frescod's own private directory", d.display()).into());
    }
    Tier2::seed_cache(&d)?;
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
            composed: None,
            damage: None,
            fill_drops: Vec::new(),
        })
    }

    fn render(&mut self, f: ScreenInput) -> Result<()> {
        self.scene.apply(f.uploads, f.clears);
        self.fill_drops.clear();
        let sizes: Vec<(u32, u32, u32)> = f.windows.iter().map(|w| {
            let (sw, sh) = surface_size(w);
            (w.id, sw, sh)
        }).collect();
        if self.surfaces.layout(&sizes) {
            self.heap = vec![0; self.surfaces.words as usize];
            self.held.clear();
            self.composed = None;
        }
        self.last.clear();
        // Each surface's change this frame, in its own pixels.
        let mut changed: HashMap<u32, [u32; 4]> = HashMap::new();
        for w in &f.windows {
            let s = self.surfaces.map[&w.id];
            let whole = [0, 0, s[2], s[3]];
            if let Some(p) = &w.pixels {
                // Drawn by the window's renderer, in its jail: copied in as is,
                // only where it differs from the drawing held.
                let since = self.held.get(&w.id).and_then(|h| h.first()).and_then(|&g| p.changed_since(g));
                let off = s[0] as usize;
                match since {
                    Some(None) => continue,
                    Some(Some(r)) if p.px.len() == (s[2] * s[3]) as usize => {
                        for y in r[1]..r[3].min(s[3]) {
                            let (a, b) = ((y * s[2] + r[0]) as usize, (y * s[2] + r[2].min(s[2])) as usize);
                            self.heap[off + a..off + b].copy_from_slice(&p.px[a..b]);
                        }
                        changed.insert(w.id, r);
                    }
                    _ => {
                        self.heap[off..off + p.px.len()].copy_from_slice(&p.px);
                        changed.insert(w.id, whole);
                    }
                }
                self.held.insert(w.id, vec![p.gen]);
                continue;
            }
            let frame = frame_of(&self.scene, w, s, &mut self.fill_drops);
            self.last.push((w.id, frame.clone()));
            let sig = fresco_compute::damage::tile_signatures(&frame);
            let (off, len) = (s[0] as usize, (s[1] * s[3]) as usize);
            let surface = &mut self.heap[off..off + len];
            match self.held.get(&w.id) {
                Some(h) if *h == sig => continue,
                Some(h) if h.len() == sig.len() => {
                    use fresco_compute::{damage, TILE};
                    let h = &frame.header;
                    let dirty = damage::dirty(&sig, self.held.get(&w.id).map(Vec::as_slice));
                    let tiles = damage::rects(&dirty, h.tiles_x, h.tiles_y, fresco_compute::tier2::Tier2::MAX_TILE_RECTS);
                    self.t2.render_tiles(&frame, surface, &tiles)?;
                    let r = tiles.iter().map(|t| [t[0] * TILE, t[1] * TILE, (t[2] * TILE).min(s[2]), (t[3] * TILE).min(s[3])])
                        .reduce(|a, b| [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])]);
                    if let Some(r) = r {
                        changed.insert(w.id, r);
                    }
                }
                _ => {
                    self.t2.render(&frame, surface)?;
                    changed.insert(w.id, whole);
                }
            }
            self.held.insert(w.id, sig);
        }
        let layers = self.surfaces.layers(&f.windows);
        let c = Compose::new(&layers, self.width, self.height, self.width, self.heap.len() as u32);
        if self.composed.as_ref() == Some(&layers) {
            // The same stack: compose again only where a surface changed.
            let rects: Vec<[u32; 4]> = f.windows.iter().zip(&layers)
                .filter_map(|(w, l)| {
                    let r = changed.get(&w.id)?;
                    // The surface rect on screen, within the layer's rect.
                    let (ox, oy) = (l.origin[0], l.origin[1]);
                    let mut m = *l;
                    m.rect = [
                        (ox + r[0] as i32).max(l.rect[0]),
                        (oy + r[1] as i32).max(l.rect[1]),
                        (ox + r[2] as i32).min(l.rect[2]),
                        (oy + r[3] as i32).min(l.rect[3]),
                    ];
                    on_screen(&m, self.width, self.height)
                })
                .collect();
            log::trace!("compute: compose {} rects", rects.len());
            self.t2.compose_rects(&c, &self.heap, &mut self.target, &rects)?;
            self.damage = Some(rects.into_iter().reduce(union).unwrap_or([0; 4]));
        } else {
            self.t2.compose(&c, &self.heap, &mut self.target)?;
            self.composed = Some(layers);
            self.damage = None;
        }
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
    fill_drops: FillDrops,
    code: Bo<'a>,
    /// Compose's inputs, its staging, its ring (frescod's VM).
    data: Option<Bo<'a>>,
    staging: Bo<'a>,
    ring: Bo<'a>,
    /// Each window's VM and what it holds, by window id.
    wins: HashMap<u32, WinVm<'a>>,
    /// Bumped each time a window's surface is redrawn.
    version: HashMap<u32, u64>,
    /// Per window, the recent versions and the surface rect each changed
    /// (window pixels): a scanout buffer a few versions behind composes
    /// only their union. A version missing here means the whole surface.
    changes: HashMap<u32, Vec<(u64, [u32; 4])>>,
    /// Per scanout buffer (by GPU address): the windows, layers and
    /// surface versions it was last composed from.
    composed: HashMap<u64, Composed>,
    /// The scanout buffer composed last.
    last_target: Option<u64>,
    /// Windows whose latest frame is refused for its raster cost
    /// (`fresco_compute::cost::FAR_OVER`), and the refusals not yet
    /// reported: (window, estimate, limit).
    cost_refused: std::collections::HashSet<u32>,
    refusals: Vec<(u32, u64, u64)>,
    /// The raster cost ceiling (`FRESCOD_RASTER_LIMIT` pixel-ops, for
    /// measuring on hardware; default `cost::FAR_OVER`).
    raster_limit: u64,
    /// A submit found the GPU busy (EBUSY: on gpusim every VM's work shares
    /// one engine, so a long raster holds it): the frame goes again on the
    /// next pass of the loop instead of frescod waiting for the GPU.
    retry: bool,
    /// Pixel-ops one raster slice may take (`cost::split`), learnt from
    /// how long full slices take, to keep each near `slice_us`.
    slice_budget: u64,
    slice_us: u64,
    /// The one window whose later slice is on the GPU now: later slices
    /// (a costly frame's rest) go one at a time across all windows, round
    /// robin from `bg_last`, so a frame's own work (compose, each window's
    /// first slice) waits behind at most one of them however many costly
    /// windows there are.
    bg: Option<u32>,
    bg_last: u32,
    /// Submits of a frame's own work so far: a slice timed while none went
    /// ran alone, and its time is the GPU's.
    fg_submits: u64,
}

/// A window's raster on the GPU, not finished yet: compose keeps showing
/// the window's last finished surface meanwhile (fresco-f4-split.md §F4j).
struct Inflight {
    /// `sync` reaches this when the slice submitted last is done.
    seq: u64,
    /// The frame, its buffers' addresses in the window's VM, and the
    /// slices of its raster not submitted yet (`cost::split`): one slice
    /// goes at a time, so the window never holds the GPU for longer than
    /// one slice (§F4j). The drawing lands once the last is done.
    frame: Arc<fresco_compute::Frame>,
    va: [u64; fresco_compute::binding::COUNT],
    rest: std::collections::VecDeque<fresco_compute::cost::Slice>,
    /// The slice running: its estimate, when it went, and whether it was
    /// cut to the budget (only those teach the budget).
    slice_cost: u64,
    slice_at: std::time::Instant,
    slice_full: bool,
    /// `fg_submits` once this slice went.
    slice_fg: u64,
    /// The tile signatures and scroll viewports of the drawing it makes.
    sig: Vec<u64>,
    scrolls: Vec<(crate::pack::Scroll, u64)>,
    /// What it changes against the front surface (surface pixels).
    changed: [u32; 4],
    started: std::time::Instant,
    /// Logged as slow already.
    slow: bool,
}

impl Inflight {
    /// Its last slice is done: the drawing can land.
    fn done(&self, sync: &Syncobj<'_>) -> bool {
        self.rest.is_empty() && sync.query().is_ok_and(|d| d >= self.seq)
    }
}

/// What a scanout buffer was last composed from: the windows, their
/// layers, and each surface's version.
type Composed = (Vec<u32>, Vec<Layer>, HashMap<u32, u64>);

/// The scanout buffer to copy into `target` before composing, if any:
/// `target` does not hold the window list `ids` (a new buffer, or a list
/// that changed since it was composed) but the buffer composed last does.
fn composed_source(composed: &HashMap<u64, Composed>, target: u64, last: Option<u64>, ids: &[u32]) -> Option<u64> {
    if composed.get(&target).is_some_and(|c| c.0 == ids) {
        return None;
    }
    last.filter(|&l| l != target && composed.get(&l).is_some_and(|c| c.0 == ids))
}

/// One window's address space.
struct WinVm<'a> {
    vm: Vm<'a>,
    /// The code, mapped read-only.
    code: Bo<'a>,
    /// Its two surfaces: written here, mapped read-only into frescod's VM
    /// (`shown`). Compose reads `front`, the last finished drawing; a
    /// raster writes the other, and they swap once it is done. So compose
    /// never waits on, nor reads half of, a window's raster.
    surfaces: [Bo<'a>; 2],
    shown: [Bo<'a>; 2],
    front: usize,
    /// A drawing has finished at least once (before that the window is
    /// not composed: its surfaces hold nothing of it yet).
    landed: bool,
    inflight: Option<Inflight>,
    size: (u32, u32),
    /// Tile signatures of what the front surface holds.
    held: Option<Vec<u64>>,
    /// The scroll viewports of the front drawing, with their keys.
    scrolls: Vec<(crate::pack::Scroll, u64)>,
    /// Where a scroll's moves stage the pixels they copy.
    scratch: Option<Bo<'a>>,
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
        let surfaces = [vm.alloc(bytes, BO_VRAM)?, vm.alloc(bytes, BO_VRAM)?];
        Ok(WinVm {
            code: vm.share(code, true)?,
            shown: [main.share(&surfaces[0], true)?, main.share(&surfaces[1], true)?],
            surfaces,
            front: 0,
            landed: false,
            inflight: None,
            size: (w, h),
            held: None,
            scrolls: Vec::new(),
            scratch: None,
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
    /// Each window's scroll viewports, with their keys.
    scrolls: HashMap<u32, Vec<(crate::pack::Scroll, u64)>>,
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

/// How many versions of each window's changes are kept: a scanout buffer
/// further behind than this composes the whole window.
const CHANGE_LOG: usize = 8;

/// Where layer `l` changed on screen between surface versions `since` and
/// `now`: the union of the logged changes, or the whole layer if the log
/// does not reach back to `since`.
fn changed_on_screen(l: &Layer, log: Option<&Vec<(u64, [u32; 4])>>, since: u64, now: u64, w: u32, h: u32) -> Option<[u32; 4]> {
    let whole = on_screen(l, w, h)?;
    let log = log.map(Vec::as_slice).unwrap_or(&[]);
    let first = log.first().map_or(u64::MAX, |e| e.0);
    if since >= now || since + 1 < first || !log.iter().any(|e| e.0 == now) {
        return Some(whole);
    }
    let r = log.iter().filter(|e| e.0 > since && e.0 <= now).map(|e| e.1).fold([0; 4], union);
    if r[0] >= r[2] || r[1] >= r[3] {
        return None;
    }
    // Window pixels to screen: `ox, oy` is where surface pixel (0, 0) shows.
    let (ox, oy) = (l.origin[0] as i64, l.origin[1] as i64);
    let c = |v: i64, lo: u32, hi: u32| v.clamp(lo as i64, hi as i64) as u32;
    let s = [c(r[0] as i64 + ox, whole[0], whole[2]), c(r[1] as i64 + oy, whole[1], whole[3]),
             c(r[2] as i64 + ox, whole[0], whole[2]), c(r[3] as i64 + oy, whole[1], whole[3])];
    (s[2] > s[0] && s[3] > s[1]).then_some(s)
}

/// The smallest rect holding `a` and `b`; an empty rect (`x0 >= x1` or
/// `y0 >= y1`) holds nothing.
pub fn union(a: [u32; 4], b: [u32; 4]) -> [u32; 4] {
    let empty = |r: &[u32; 4]| r[0] >= r[2] || r[1] >= r[3];
    match (empty(&a), empty(&b)) {
        (true, _) => b,
        (_, true) => a,
        _ => [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])],
    }
}

/// Place `bytes` in `stage` at a 256-byte boundary; its offset.
fn place(stage: &mut Vec<u8>, bytes: &[u8]) -> u64 {
    stage.resize(stage.len().next_multiple_of(256), 0);
    let at = stage.len() as u64;
    stage.extend_from_slice(bytes);
    at
}

/// Write `ib` into `ring` and submit it in `vm`, signalling `signal`.
/// Whether `e` is the driver's EBUSY (the GPU did not take the work in
/// time).
pub fn is_busy(e: &(dyn std::error::Error + 'static)) -> bool {
    e.downcast_ref::<std::io::Error>().and_then(|e| e.raw_os_error()) == Some(libc::EBUSY)
}

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
            fill_drops: Vec::new(),
            code,
            data: None,
            staging,
            ring,
            wins: HashMap::new(),
            version: HashMap::new(),
            changes: HashMap::new(),
            composed: HashMap::new(),
            last_target: None,
            cost_refused: Default::default(),
            refusals: Vec::new(),
            raster_limit: std::env::var("FRESCOD_RASTER_LIMIT").ok().and_then(|v| v.parse().ok())
                .unwrap_or(fresco_compute::cost::FAR_OVER),
            retry: false,
            slice_budget: SLICE_START,
            slice_us: std::env::var("FRESCOD_RASTER_SLICE_US").ok().and_then(|v| v.parse().ok()).unwrap_or(SLICE_US),
            bg: None,
            bg_last: 0,
            fg_submits: 0,
        })
    }

    /// Whether a window's raster is still running on the GPU.
    pub fn rasters_pending(&self) -> bool {
        self.wins.values().any(|v| v.inflight.is_some())
    }

    /// Whether a window's raster finished since it was last looked at (a
    /// frame is due to show it).
    pub fn raster_landed(&self) -> bool {
        self.retry || self.wins.values().any(|v| v.inflight.as_ref().is_some_and(|f| f.done(&v.sync)))
    }

    /// Submit the next slice of every window raster whose running slice is
    /// done (§F4j), with no deadline: frescod's compose and other windows'
    /// first slices, which carry the frame's, go ahead of them. Cheap when
    /// nothing is due; frescod's loop calls it each pass.
    pub fn pump(&mut self) -> Result<()> {
        // The later slice on the GPU: wait for it, learn from it.
        if let Some(id) = self.bg {
            match self.wins.get(&id).and_then(|v| v.inflight.as_ref().map(|fl| (v, fl))) {
                Some((v, fl)) if v.sync.query()? < fl.seq => return Ok(()),
                Some((_, fl)) => learn(&mut self.slice_budget, self.slice_us, fl, fl.slice_fg == self.fg_submits),
                None => {}
            }
            self.bg = None;
        }
        // The next window, round robin, whose running slice is done and
        // that has slices left.
        let mut ids: Vec<u32> = self.wins.iter()
            .filter(|(_, v)| v.inflight.as_ref().is_some_and(|fl| !fl.rest.is_empty() && v.sync.query().is_ok_and(|d| d >= fl.seq)))
            .map(|(id, _)| *id)
            .collect();
        ids.sort_unstable();
        let Some(&id) = ids.iter().find(|&&i| i > self.bg_last).or(ids.first()) else { return Ok(()) };
        let v = self.wins.get_mut(&id).unwrap();
        let fl = v.inflight.as_mut().unwrap();
        {
            // The rest was cut with the budget of its frame's submit: if the
            // budget has since shrunk (a slow GPU learnt), cut it again, or
            // its slices would each hold the GPU far past `slice_us`.
            if fl.rest.front().is_some_and(|n| n.bin.is_none() && n.cost > 2 * self.slice_budget) {
                let tiles: Vec<[u32; 4]> = fl.rest.iter().flat_map(|s| s.tiles.iter().copied()).collect();
                if fl.rest.iter().all(|s| s.bin.is_none()) {
                    fl.rest = fresco_compute::cost::split_raster(&fl.frame, &tiles, self.slice_budget).into();
                    log::debug!("gpu: window {id}: rest cut again into {} slices of ~{} pixel-ops", fl.rest.len(), self.slice_budget);
                }
            }
            let Some(next) = fl.rest.pop_front() else { return Ok(()) };
            let mut ib = self.arch.stream();
            ib.frame_part(v.code.gpu_va(), &fl.va, &fl.frame, next.bin, &next.tiles)
                .map_err(|e| format!("{} record: {e}", self.arch.name()))?;
            ib.end(v.ring.gpu_va() + FENCE_OFFSET);
            if let Err(e) = run(&v.vm, &v.ring, &*ib, (&v.sync, v.seq + 1), 0) {
                if !is_busy(&*e) {
                    return Err(e);
                }
                log::debug!("gpu: window {id} raster slice: GPU busy, later");
                fl.rest.push_front(next);
                self.retry = true;
                return Ok(());
            }
            v.seq += 1;
            fl.seq = v.seq;
            fl.slice_full = next.cost * 2 >= self.slice_budget;
            fl.slice_cost = next.cost;
            fl.slice_at = std::time::Instant::now();
            fl.slice_fg = self.fg_submits;
            self.bg = Some(id);
            self.bg_last = id;
        }
        Ok(())
    }

    /// Apply the frame's uploads, give each window a VM and a surface of its
    /// size, and pack each window's scene; `submit` records and runs it.
    fn prepare(&mut self, f: ScreenInput) -> Result<()> {
        self.scene.apply(f.uploads, f.clears);
        self.fill_drops.clear();
        // Windows that are gone take their VMs with them.
        self.wins.retain(|id, _| f.windows.iter().any(|w| w.id == *id));
        self.version.retain(|id, _| f.windows.iter().any(|w| w.id == *id));
        self.changes.retain(|id, _| f.windows.iter().any(|w| w.id == *id));
        self.cost_refused.retain(|id| f.windows.iter().any(|w| w.id == *id));
        let gpu = self.vm.gpu();
        let mut frames = Vec::with_capacity(f.windows.len());
        let mut scrolls = HashMap::new();
        for w in &f.windows {
            let (sw, sh) = surface_size(w);
            if self.wins.get(&w.id).is_none_or(|v| v.size != (sw, sh)) {
                // New, or resized: a fresh VM and surface (its contents are gone).
                self.wins.remove(&w.id);
                self.wins.insert(w.id, WinVm::new(gpu, self.vm, &self.code, sw, sh)?);
                *self.version.entry(w.id).or_insert(0) += 1;
                self.changes.remove(&w.id);
                self.composed.clear();
            }
            frames.push((w.id, frame_of(&self.scene, w, [0, sw, sw, sh], &mut self.fill_drops)));
            scrolls.insert(w.id, crate::pack::keyed_scrolls(&self.scene, &w.scrolls));
        }
        self.last = frames.clone();
        let pos = f.windows.iter().map(|w| (w.id, (w.pos.0.round() as i32, w.pos.1.round() as i32))).collect();
        self.pending = Some(Pending { frames, scrolls, pos });
        Ok(())
    }

    /// The compose layers, and the heap they address: every surface as
    /// frescod's VM maps it, as word offsets from the lowest.
    fn layers(&self, windows: &[(u32, Arc<fresco_compute::Frame>)], pos: &HashMap<u32, (i32, i32)>) -> Result<(Vec<Layer>, u64, u32)> {
        let base = self.wins.values().flat_map(|v| v.shown.iter().map(|b| b.gpu_va())).min().unwrap_or(0);
        let mut end = base;
        let mut layers = Vec::new();
        for (id, _) in windows {
            let v = &self.wins[id];
            let shown = &v.shown[v.front];
            let off = (shown.gpu_va() - base) / 4;
            end = end.max(shown.gpu_va() + v.size.0 as u64 * v.size.1 as u64 * 4);
            let off = u32::try_from(off).map_err(|_| "surfaces spread over more than 16 GiB of VA")?;
            let (x, y) = pos[id];
            layers.push(Layer::at([off, v.size.0, v.size.0, v.size.1], x, y, 1.0));
        }
        let words = u32::try_from((end - base) / 4).map_err(|_| "surfaces spread over more than 16 GiB of VA")?;
        Ok((layers, base, words.max(1)))
    }

    fn submit(&mut self, target: &Bo<'_>, deadline_ns: u32, signal: (&Syncobj<'_>, u64)) -> Result<()> {
        use fresco_compute::{binding, compose::binding as cb, damage, TILE};
        let mut p = self.pending.take().ok_or("present before render")?;
        if (self.width * self.height) as u64 * 4 > target.size() {
            return Err("scanout buffer smaller than the screen".into());
        }
        let name = self.arch.name();
        self.retry = false;
        self.pump()?;

        // FRESCOD_GPU_ISOLATION_PROBE=1 (a test): every window after the
        // first rasters to the address the first window's surface has in
        // frescod's VM. With one VM that overwrote the first window; with a
        // VM per window it must fault (the work is lost, EIO) or land in the
        // window's own memory, and the first window must stay as it was.
        let probe = std::env::var_os("FRESCOD_GPU_ISOLATION_PROBE").is_some()
            .then(|| p.frames.first().and_then(|(id, _)| self.wins.get(id)).map(|v| v.shown[v.front].gpu_va()))
            .flatten();
        // Each window whose tiles changed: its dirty rects, in its own VM.
        // A scrolled viewport first moves what it shows on the GPU
        // (fresco-scene-groups.md §4); then only what that left to draw.
        let limit = self.raster_limit;
        for (n, (id, f)) in p.frames.iter().enumerate() {
            let v = self.wins.get_mut(id).ok_or("window without a VM")?;
            let now = p.scrolls.remove(id).unwrap_or_default();
            // A raster that finished: its surface is the front now.
            if let Some(fl) = v.inflight.as_mut() {
                if !fl.done(&v.sync) {
                    // Still running: compose shows the last finished
                    // drawing; this frame's changes wait for the next.
                    let took = fl.started.elapsed();
                    if !fl.slow && took > std::time::Duration::from_millis(SLOW_RASTER_MS) {
                        fl.slow = true;
                        log::warn!("gpu: window {id}: raster running {} ms; showing its last frame", took.as_millis());
                    }
                    continue;
                }
                learn(&mut self.slice_budget, self.slice_us, fl, fl.slice_fg == self.fg_submits);
                land(*id, v, &mut self.version, &mut self.changes);
            }
            let sig = damage::tile_signatures(f);
            let (fw, fh) = (f.header.width, f.header.height);
            // What this frame changes against the front drawing.
            let mut held = v.held.clone();
            let (moves, rects) = match held.as_mut() {
                Some(held) if held.len() == sig.len() && (fw, fh) == v.size => {
                    let sh = crate::pack::shifts(&v.scrolls, &now);
                    let (moves, out) = if sh.is_empty() { Default::default() } else { damage::plan(fw, fh, &sig, held, &sh) };
                    (moves, damage::tiles_after_plan(fw, fh, &sig, held, &out.pixels, MAX_DAMAGE_RECTS))
                }
                _ => {
                    let dirty = damage::dirty(&sig, None);
                    (Vec::new(), damage::rects(&dirty, f.header.tiles_x, f.header.tiles_y, MAX_DAMAGE_RECTS))
                }
            };
            if rects.is_empty() && moves.is_empty() {
                continue;
            }
            // H6: a frame far too costly to raster is not drawn; the window
            // keeps its last frame and its client is told (once, until a
            // frame of it fits again).
            let cost = fresco_compute::cost::estimate(f, &rects);
            if cost > limit {
                if self.cost_refused.insert(*id) {
                    log::warn!("gpu: window {id}: frame not drawn, raster estimate {cost} over {limit} pixel-ops");
                    self.refusals.push((*id, cost, limit));
                }
                continue;
            }
            self.cost_refused.remove(id);
            let changed = moves.iter().map(|m| m.dst)
                .chain(rects.iter().map(|t| [t[0] * TILE, t[1] * TILE, (t[2] * TILE).min(fw), (t[3] * TILE).min(fh)]))
                .fold([0; 4], union);
            let mut stage = Vec::new();
            let mut offs = [0u64; binding::COUNT];
            for (b, bytes) in f.buffers().into_iter().enumerate() {
                if b as u32 != binding::TARGET {
                    offs[b] = place(&mut stage, &bytes);
                }
            }
            // Each move goes surface -> scratch, then scratch -> surface, so
            // every move reads the drawing as it was.
            let surface_words = v.size.0 * v.size.1;
            let pitch = v.size.0;
            let mut scratch_words = 0u32;
            let mut hdrs = Vec::with_capacity(moves.len());
            for m in &moves {
                let [x0, y0, x1, y1] = m.dst;
                let s = m.src();
                let size = [x1 - x0, y1 - y0];
                hdrs.push((size, scratch_words, [s[1] * pitch + s[0], y0 * pitch + x0]));
                scratch_words += size[0] * size[1];
            }
            let mut move_offs = Vec::with_capacity(hdrs.len());
            for &(size, at, [src, dst]) in &hdrs {
                let out = damage::move_header(size, (src, pitch, surface_words), (at, size[0], scratch_words));
                let back = damage::move_header(size, (at, size[0], scratch_words), (dst, pitch, surface_words));
                move_offs.push((size, place(&mut stage, &fresco_compute::bytes_of(&out)), place(&mut stage, &fresco_compute::bytes_of(&back))));
            }
            if scratch_words > 0 {
                v.scratch = Some(grow(&v.vm, v.scratch.take(), scratch_words as u64 * 4, BO_VRAM)?);
            }
            v.data = Some(grow(&v.vm, v.data.take(), stage.len() as u64, BO_VRAM)?);
            if v.staging.size() < stage.len() as u64 {
                v.staging = v.vm.alloc_readonly((stage.len() as u64).next_power_of_two(), 0)?;
            }
            v.staging.write(0, &stage)?;
            let data_va = v.data.as_ref().unwrap().gpu_va();
            // The back surface: first a copy of the front (the drawing the
            // damage is against), then the moves and the dirty tiles.
            let back = v.surfaces[v.front ^ 1].gpu_va();
            let mut va: [u64; binding::COUNT] = std::array::from_fn(|b| data_va + offs[b]);
            va[binding::TARGET as usize] = match probe {
                Some(other) if n > 0 => other,
                _ => back,
            };
            let mut ib = self.arch.stream();
            ib.copy(v.staging.gpu_va(), data_va, stage.len() as u64);
            if v.held.is_some() {
                ib.copy(v.surfaces[v.front].gpu_va(), back, surface_words as u64 * 4);
                ib.barrier();
            }
            if let Some(scratch) = v.scratch.as_ref().filter(|_| !move_offs.is_empty()) {
                let (surf, scr) = (va[binding::TARGET as usize], scratch.gpu_va());
                for &(size, out, _) in &move_offs {
                    ib.shift(v.code.gpu_va(), data_va + out, surf, scr, size).map_err(|e| format!("{name} shift: {e}"))?;
                }
                ib.barrier();
                for &(size, _, back) in &move_offs {
                    ib.shift(v.code.gpu_va(), data_va + back, scr, surf, size).map_err(|e| format!("{name} shift: {e}"))?;
                }
                ib.barrier();
            }
            // The raster in slices of about `slice_budget` (a window's
            // whole frame when it is cheap): the first goes now, after the
            // copy and the moves; the rest one at a time (`pump`).
            let mut rest: std::collections::VecDeque<_> = if rects.is_empty() {
                Default::default()
            } else {
                fresco_compute::cost::split(f, &rects, self.slice_budget).into()
            };
            let first = rest.pop_front();
            if let Some(sl) = &first {
                ib.frame_part(v.code.gpu_va(), &va, f, sl.bin, &sl.tiles).map_err(|e| format!("{name} record: {e}"))?;
            }
            ib.end(v.ring.gpu_va() + FENCE_OFFSET);
            v.seq += 1;
            if let Err(e) = run(&v.vm, &v.ring, &*ib, (&v.sync, v.seq), deadline_ns) {
                // The GPU is busy with other work: this window keeps its last
                // drawing and rasters on a later pass; frescod goes on.
                v.seq -= 1;
                if !is_busy(&*e) {
                    return Err(e);
                }
                log::debug!("gpu: window {id} raster: GPU busy, later");
                self.retry = true;
                continue;
            }
            self.fg_submits += 1;
            if !rest.is_empty() {
                log::debug!("gpu: window {id}: raster in {} slices of ~{} pixel-ops", rest.len() + 1, self.slice_budget);
            }
            let slice_cost = first.as_ref().map_or(0, |s| s.cost);
            v.inflight = Some(Inflight {
                seq: v.seq, frame: f.clone(), va, rest,
                slice_cost, slice_at: std::time::Instant::now(), slice_full: slice_cost * 2 >= self.slice_budget,
                slice_fg: self.fg_submits,
                sig, scrolls: now, changed, started: std::time::Instant::now(), slow: false,
            });
            if probe.is_some() && n > 0 {
                let r = v.sync.wait(v.seq, flip_wait_ms());
                eprintln!("frescod: isolation probe: window {id} raster to {:#x} (window {} in frescod's VM): {r:?}",
                    va[binding::TARGET as usize], p.frames[0].0);
            }
        }

        // Rasters that are quick land in this frame: wait for them a little
        // (they run side by side, one queue per window VM), never longer.
        let t0 = std::time::Instant::now();
        let quick = std::time::Duration::from_micros(QUICK_RASTER_US);
        loop {
            let mut all = true;
            for v in self.wins.values() {
                if let Some(fl) = &v.inflight {
                    all &= fl.done(&v.sync);
                }
            }
            if all || t0.elapsed() >= quick {
                break;
            }
            std::thread::yield_now();
        }
        for (id, v) in self.wins.iter_mut() {
            if let Some(fl) = v.inflight.as_ref().filter(|fl| fl.done(&v.sync)) {
                learn(&mut self.slice_budget, self.slice_us, fl, fl.slice_fg == self.fg_submits);
                land(*id, v, &mut self.version, &mut self.changes);
            }
        }

        // Only windows with a finished drawing are composed.
        p.frames.retain(|(id, _)| self.wins.get(id).is_some_and(|v| v.landed));

        // What to compose: everywhere if the layers changed since this
        // scanout buffer was composed, else where a surface changed.
        let (layers, heap_va, heap_words) = self.layers(&p.frames, &p.pos)?;
        let versions: HashMap<u32, u64> = p.frames.iter().map(|(id, _)| (*id, self.version.get(id).copied().unwrap_or(0))).collect();
        let ids: Vec<u32> = p.frames.iter().map(|(id, _)| *id).collect();
        // A scanout buffer that holds another window list (or nothing yet,
        // its first frame) starts as a copy of the buffer composed last, if
        // that one holds this list: one DMA copy instead of composing the
        // whole screen; then only what changed since that buffer is composed.
        let copy_from = composed_source(&self.composed, target.gpu_va(), self.last_target, &ids);
        let held_state = match copy_from {
            Some(src) => self.composed.get(&src).cloned(),
            None => self.composed.get(&target.gpu_va()).cloned(),
        };
        let screen_rects: Option<Vec<[u32; 4]>> = match held_state.as_ref() {
            Some((held_ids, old_layers, held)) if *held_ids == ids => {
                let (w, h) = (self.width, self.height);
                let mut r = Vec::new();
                for (((id, _), l), old) in p.frames.iter().zip(&layers).zip(old_layers) {
                    // The surface's place in the heap moves when its two
                    // surfaces swap; what it shows changes by its version.
                    if !same_place(l, old) {
                        r.extend(on_screen(old, w, h));
                        r.extend(on_screen(l, w, h));
                    } else if held.get(id) != versions.get(id) {
                        let since = held.get(id).copied().unwrap_or(0);
                        let now = versions.get(id).copied().unwrap_or(0);
                        r.extend(changed_on_screen(l, self.changes.get(id), since, now, w, h));
                    }
                }
                Some(r)
            }
            _ => None, // compose the whole screen
        };
        let compose = Compose::new(&layers, self.width, self.height, self.width, heap_words);
        compose.validate(compose.target_words(), heap_words as usize)?;
        let mut ib = self.arch.stream();
        if let Some(src) = copy_from {
            // The source's compose may still be running on the queue.
            ib.barrier();
            ib.copy(src, target.gpu_va(), self.width as u64 * self.height as u64 * 4);
            ib.barrier();
        }
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
        self.last_target = Some(target.gpu_va());
        // End of pipe: the fence, then the interrupt that signals `signal`.
        ib.end(self.ring.gpu_va() + FENCE_OFFSET);
        self.fg_submits += 1;
        if let Err(e) = run(self.vm, &self.ring, &*ib, signal, deadline_ns) {
            // EBUSY: a window's work kept the GPU past the driver's wait.
            // Compose never waits on a window's raster (§F4j): this buffer
            // was not drawn, the screen keeps what it shows, and the frame
            // goes again on the loop's next pass. The caller skips the flip.
            self.composed.remove(&target.gpu_va());
            self.last_target = None;
            if is_busy(&*e) {
                self.retry = true;
            }
            return Err(e);
        }
        Ok(())
    }
}

/// Window `id`'s finished raster becomes its front surface: a new version,
/// changed where the raster drew.
fn land(id: u32, v: &mut WinVm<'_>, version: &mut HashMap<u32, u64>, changes: &mut HashMap<u32, Vec<(u64, [u32; 4])>>) {
    let Some(fl) = v.inflight.take() else { return };
    v.front ^= 1;
    v.landed = true;
    v.held = Some(fl.sig);
    v.scrolls = fl.scrolls;
    let ver = version.entry(id).or_insert(0);
    *ver += 1;
    let log = changes.entry(id).or_default();
    log.push((*ver, fl.changed));
    if log.len() > CHANGE_LOG {
        log.remove(0);
    }
}

/// A layer in the same place on screen, whichever surface it shows.
fn same_place(a: &Layer, b: &Layer) -> bool {
    a.rect == b.rect && a.origin == b.origin && a.surface[1..] == b.surface[1..] && a.opacity == b.opacity
}

/// What a raster slice aims to take on the GPU, µs (`FRESCOD_RASTER_SLICE_US`):
/// another window's frame or compose waits behind at most about this.
const SLICE_US: u64 = 4000;
/// The slice budget to start from (pixel-ops), and its bounds. It starts
/// low: a slow GPU (gpusim) is learnt from the first full slice, a fast one
/// grows it within a few frames.
const SLICE_START: u64 = 1 << 14;
const SLICE_MIN: u64 = 1 << 12;

/// Learn the slice budget from a slice of `fl` that finished: one cut to
/// the budget took `elapsed`, so the GPU does about cost / elapsed
/// pixel-ops a µs here. Slower is believed at once; faster halfway.
/// `elapsed` is seen late (frescod polls), so it errs short of the GPU's
/// speed: slices come out smaller, never larger, than `slice_us`.
fn learn(budget: &mut u64, slice_us: u64, fl: &Inflight, alone: bool) {
    if !fl.slice_full || fl.slice_cost == 0 {
        return;
    }
    let us = fl.slice_at.elapsed().as_micros().max(1) as u64;
    let want = (fl.slice_cost as u128 * slice_us as u128 / us as u128).min(u64::MAX as u128) as u64;
    // A slice that shared the GPU with a frame's own work took longer than
    // the GPU needs for it: it may raise the budget, not lower it.
    if !alone && want < *budget {
        return;
    }
    let next = if want < *budget { want } else { (*budget + want) / 2 };
    *budget = next.clamp(SLICE_MIN, fresco_compute::cost::FAR_OVER);
}

/// A window raster running longer than this is logged (once per raster).
const SLOW_RASTER_MS: u64 = 100;
/// How long a frame waits for a window's raster to land before composing
/// without it (its last finished drawing shows instead).
const QUICK_RASTER_US: u64 = 2000;

/// How long frescod waits on GPU work it depends on: the flip timeout
/// (`FRESCOD_FLIP_TIMEOUT_MS`, raised on gpusim, where a frame takes
/// seconds).
fn flip_wait_ms() -> u32 {
    std::env::var("FRESCOD_FLIP_TIMEOUT_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(2000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pack::Pixels;

    fn node(x: f32, y: f32, w: f32, h: f32, color: [f32; 4]) -> fresco_vulkan::SceneNode {
        fresco_vulkan::SceneNode { position: [x, y], size: [w, h], color, radius: 0.0, _pad: [0.0; 3] }
    }

    fn input(px: &Arc<Pixels>, color: [f32; 4], bpos: (f32, f32)) -> ScreenInput {
        let a = WindowFrame { id: 1, pos: (10.0, 10.0), size: (px.width, px.height), pixels: Some(px.clone()), ..Default::default() };
        let b = WindowFrame {
            id: 2,
            pos: bpos,
            size: (50, 40),
            rects: vec![node(0.0, 0.0, 50.0, 40.0, [0.1, 0.2, 0.3, 1.0]), node(20.0, 5.0, 6.0, 12.0, color)],
            ..Default::default()
        };
        ScreenInput { windows: vec![a, b], uploads: Vec::new(), clears: Vec::new() }
    }

    /// A scanout buffer without this window list starts as a copy of the
    /// buffer composed last, when that one has the list.
    #[test]
    fn a_new_scanout_buffer_copies_the_last_one() {
        let c = |ids: &[u32]| (ids.to_vec(), Vec::new(), HashMap::new());
        let mut m: HashMap<u64, Composed> = HashMap::new();
        assert_eq!(composed_source(&m, 0x2000, None, &[1]), None, "nothing composed yet");
        m.insert(0x1000, c(&[1]));
        assert_eq!(composed_source(&m, 0x2000, Some(0x1000), &[1]), Some(0x1000), "first frame of a buffer");
        assert_eq!(composed_source(&m, 0x1000, Some(0x1000), &[1]), None, "it holds the list");
        assert_eq!(composed_source(&m, 0x2000, Some(0x1000), &[1, 2]), None, "the last buffer has another list");
        m.insert(0x2000, c(&[1]));
        m.insert(0x1000, c(&[1, 2]));
        assert_eq!(composed_source(&m, 0x2000, Some(0x1000), &[1, 2]), Some(0x1000), "a window opened");
    }

    /// A scanout buffer a few versions behind composes the union of what
    /// changed since, on screen and clipped to the layer; past the log, or
    /// with a gap, the whole layer.
    #[test]
    fn compose_covers_what_changed_since() {
        let l = Layer::at([0, 100, 100, 50], 300, 200, 1.0);
        let log = vec![(5, [10, 10, 20, 20]), (6, [0, 40, 100, 50]), (7, [90, 0, 120, 8])];
        let (w, h) = (1920, 1080);
        assert_eq!(changed_on_screen(&l, Some(&log), 6, 7, w, h), Some([390, 200, 400, 208]), "clipped to the layer");
        assert_eq!(changed_on_screen(&l, Some(&log), 4, 7, w, h), Some([300, 200, 400, 250]));
        assert_eq!(changed_on_screen(&l, Some(&log), 5, 6, w, h), Some([300, 240, 400, 250]), "older entries only up to now");
        let whole = Some([300, 200, 400, 250]);
        assert_eq!(changed_on_screen(&l, Some(&log), 3, 7, w, h), whole, "log does not reach back");
        assert_eq!(changed_on_screen(&l, Some(&log), 6, 8, w, h), whole, "now is not logged");
        assert_eq!(changed_on_screen(&l, None, 6, 7, w, h), whole);
        let off = Layer::at([0, 100, 100, 50], 1900, -20, 1.0);
        assert_eq!(changed_on_screen(&off, Some(&log), 4, 5, w, h), None, "changed part is off screen");
    }

    /// Drawing only what changed gives the same screen as drawing it all.
    #[test]
    fn incremental_frames_match_full_ones() {
        let (w, h) = (128, 96);
        let mut inc = Compute::new(w, h).expect("kernels");
        let px1: Vec<u32> = (0..40 * 30).map(|i| 0xff00_0000 | i * 997).collect();
        let p1 = Arc::new(Pixels { gen: 1, width: 40, height: 30, px: px1.clone(), seq: 1, since: Vec::new() });
        inc.render(input(&p1, [1.0, 0.0, 0.0, 1.0], (60.0, 20.0))).unwrap();

        // A: a 5x4 patch changes; B: its small rect turns green.
        let mut px2 = px1.clone();
        for y in 7..11 {
            for x in 3..8 {
                px2[y * 40 + x] = 0xff12_3456;
            }
        }
        let p2 = Arc::new(Pixels { gen: 2, width: 40, height: 30, px: px2, seq: 2, since: vec![(1, [3, 7, 8, 11])] });
        let check = |inc: &mut Compute, f: &dyn Fn() -> ScreenInput| {
            inc.render(f()).unwrap();
            let mut full = Compute::new(w, h).unwrap();
            full.render(f()).unwrap();
            assert!(inc.target == full.target, "incremental frame differs from a full one");
        };
        check(&mut inc, &|| input(&p2, [0.0, 1.0, 0.0, 1.0], (60.0, 20.0)));
        // B moves: the stack changed, composed whole.
        check(&mut inc, &|| input(&p2, [0.0, 1.0, 0.0, 1.0], (50.0, 30.0)));
        // A drawing the consumer never held: copied whole.
        let p3 = Arc::new(Pixels { gen: 9, width: 40, height: 30, px: vec![0xff77_7777; 1200], seq: 7, since: vec![(8, [0, 0, 1, 1])] });
        check(&mut inc, &|| input(&p3, [0.0, 1.0, 0.0, 1.0], (50.0, 30.0)));
    }
}
