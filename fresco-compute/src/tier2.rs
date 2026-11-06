//! Run the two kernels on Tier-2: Atrium's AOT SPIR-V to native CPU code.
//!
//! The kernels are compiled once through `atrium-spv-loader` (which spawns
//! `atrium-spv-compile` on a cache miss and maps the result), then
//! dispatched here with the compute calling convention
//! `aqueduct-gpu-host::Tier2Backend::dispatch_compute` uses: X0 the
//! storage-image table (its barrier slot filled), X2 the ABI v5 SSBO table
//! (`{ base, size }` per binding; the kernels bounds-check every access
//! against it), one call per invocation. Workgroups are spread over threads;
//! the kernels use no shared memory or barriers, so lanes run serially.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use atrium_spv_loader::{CsMain, LoadedShader, LoaderConfig, ShaderCache};
use atrium_spv_runtime::{SsboDesc, SsboTable};

/// The SSBO descriptor for any slice: its bytes.
fn desc<T>(s: &[T]) -> SsboDesc {
    SsboDesc { base: s.as_ptr() as u64, size: std::mem::size_of_val(s) as u64 }
}

use crate::{binding, Frame, BIN_LOCAL, BIN_SPV, RASTER_LOCAL, RASTER_SPV};

/// The two compiled kernels.
pub struct Tier2 {
    _cache: ShaderCache,
    bin: (Arc<LoadedShader>, CsMain),
    raster: (Arc<LoadedShader>, CsMain),
    compose: (Arc<LoadedShader>, CsMain),
    threads: usize,
}

fn err(m: String) -> io::Error {
    io::Error::other(m)
}

fn load(cache: &ShaderCache, spv: &[u8], name: &str) -> io::Result<(Arc<LoadedShader>, CsMain)> {
    let s = cache
        .load_or_compile(spv)
        .map_err(|e| err(format!("tier2 compile {name}: {e:?}")))?;
    let cs = s.entry_points.cs_main.ok_or_else(|| err(format!("{name}: no cs_main")))?;
    Ok((s, cs))
}

impl Tier2 {
    /// Compile (or load from the cache) both kernels. `cache_root` holds the
    /// compiled code; `compile_binary` is `atrium-spv-compile`.
    pub fn new(cache_root: PathBuf, compile_binary: PathBuf) -> io::Result<Self> {
        let cache = ShaderCache::new(LoaderConfig {
            cache_root,
            abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
            compile_binary,
        });
        let bin = load(&cache, BIN_SPV, "bin")?;
        let raster = load(&cache, RASTER_SPV, "raster")?;
        let compose = load(&cache, crate::compose::COMPOSE_SPV, "compose")?;
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        Ok(Self { _cache: cache, bin, raster, compose, threads })
    }

    /// Like [`Tier2::new`], from `FRESCO_COMPUTE_CACHE` (default
    /// `/var/atrium/shaders`) and `ATRIUM_SPV_COMPILE` (default the
    /// production path).
    pub fn from_env() -> io::Result<Self> {
        let prod = LoaderConfig::production();
        let cache = std::env::var_os("FRESCO_COMPUTE_CACHE").map(PathBuf::from).unwrap_or(prod.cache_root);
        let bin = std::env::var_os("ATRIUM_SPV_COMPILE").map(PathBuf::from).unwrap_or(prod.compile_binary);
        Self::new(cache, bin)
    }

    /// Compile both kernels in-process with the Cranelift backend into
    /// `cache_root`, where [`Tier2::new`] then finds them without spawning
    /// the compiler. Kernels already in the cache are left alone.
    #[cfg(feature = "cranelift")]
    pub fn seed_cache(cache_root: &std::path::Path) -> io::Result<()> {
        use atrium_spv_backend_cranelift::{compile_blob, Target};
        // Where the loader looks: `<root>/v{N}/<arch>/` (ShaderCache::version_dir).
        let dir = ShaderCache::new(LoaderConfig {
            cache_root: cache_root.to_path_buf(),
            abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
            compile_binary: PathBuf::new(),
        })
        .version_dir();
        std::fs::create_dir_all(&dir)?;
        for (name, spv) in [("bin", BIN_SPV), ("raster", RASTER_SPV), ("compose", crate::compose::COMPOSE_SPV)] {
            let hash = ShaderCache::hash(spv);
            let blob = dir.join(format!("{hash}.afblob"));
            if blob.exists() {
                continue;
            }
            let m = atrium_spv_frontend::translate(spv).map_err(|e| err(format!("{name}: frontend: {e:?}")))?;
            let o = compile_blob(&m, Target::host()).map_err(|e| err(format!("{name}: cranelift: {e:?}")))?;
            std::fs::write(dir.join(format!("{hash}.pcmap")), &o.pcmap)?;
            std::fs::write(&blob, &o.blob)?;
        }
        Ok(())
    }

    /// Cap the worker threads (default: all CPUs).
    pub fn set_threads(&mut self, n: usize) {
        self.threads = n.max(1);
    }

    /// Render `frame` into `target` (`pitch * height` BGRA8 words). The
    /// frame may come from an untrusted renderer: it is validated first.
    pub fn render(&self, frame: &Frame, target: &mut [u32]) -> io::Result<()> {
        frame.validate(target.len()).map_err(err)?;
        let mut mask = vec![0u32; frame.mask_words()];
        let header = [frame.header];
        let mut table = [SsboDesc::default(); binding::COUNT];
        table[binding::FRAME as usize] = desc(&header);
        table[binding::OPS as usize] = desc(&frame.ops);
        table[binding::TEX as usize] = desc(&frame.tex);
        table[binding::TEXELS as usize] = desc(&frame.texels);
        table[binding::MASK as usize] = desc(&mask);
        table[binding::TARGET as usize] = desc(target);
        table[binding::LUT as usize] = desc(&frame.lut);
        // SAFETY: every pointer in `table` stays valid until both dispatches
        // return, and `validate` checked the header against the buffers, so
        // the kernels stay within them.
        unsafe { self.render_table(&table, &frame.header) };
        Ok(())
    }

    /// Rect cap to pass [`crate::damage::rects`] for [`Tier2::render_tiles`]:
    /// none. Its cost is per tile rastered, not per rect, so scattered
    /// changes cost their share of the window; capped (as a GPU backend,
    /// one dispatch per rect, must be), they fall back to their bounding
    /// box, which for changes spread over the window is all of it.
    pub const MAX_TILE_RECTS: usize = usize::MAX;

    /// Like [`Tier2::render`], but raster only the tiles in `tiles` (tile
    /// rects `[tx0, ty0, tx1, ty1)`, as [`crate::damage::rects`] gives
    /// them); the rest of `target` keeps what it holds. Binning still
    /// covers the whole frame (it is per op, and cheap).
    pub fn render_tiles(&self, frame: &Frame, target: &mut [u32], tiles: &[[u32; 4]]) -> io::Result<()> {
        frame.validate(target.len()).map_err(err)?;
        if tiles.is_empty() {
            return Ok(());
        }
        // Every tile dirty (damage::rects gives disjoint rects): the plain
        // full dispatch, without listing each workgroup.
        let h = &frame.header;
        let area = |t: &[u32; 4]| u64::from(t[2].min(h.tiles_x).saturating_sub(t[0])) * u64::from(t[3].min(h.tiles_y).saturating_sub(t[1]));
        if tiles.iter().map(area).sum::<u64>() >= u64::from(h.tiles_x) * u64::from(h.tiles_y) {
            return self.render(frame, target);
        }
        let mut mask = vec![0u32; frame.mask_words()];
        let header = [frame.header];
        let mut table = [SsboDesc::default(); binding::COUNT];
        table[binding::FRAME as usize] = desc(&header);
        table[binding::OPS as usize] = desc(&frame.ops);
        table[binding::TEX as usize] = desc(&frame.tex);
        table[binding::TEXELS as usize] = desc(&frame.texels);
        table[binding::MASK as usize] = desc(&mask);
        table[binding::TARGET as usize] = desc(target);
        table[binding::LUT as usize] = desc(&frame.lut);
        let g = h.raster_groups();
        let per = crate::TILE / RASTER_LOCAL;
        let groups: Vec<[u32; 4]> = tiles
            .iter()
            .map(|t| [t[0] * per, t[1] * per, (t[2] * per).min(g[0]), (t[3] * per).min(g[1])])
            .filter(|r| r[2] > r[0] && r[3] > r[1])
            .collect();
        let t = SsboTable::new(&table);
        // SAFETY: as in `render`: the table's buffers outlive the
        // dispatches and `validate` checked them against the header; the
        // group rects are within the raster grid.
        unsafe {
            self.dispatch(self.bin.1, &t, h.bin_groups(), [BIN_LOCAL, 1, 1]);
            self.dispatch_rects(self.raster.1, &t, &groups, [RASTER_LOCAL, RASTER_LOCAL, 1]);
        }
        Ok(())
    }

    /// [`Tier2::render_tiles`], and also the pixels in `pixels` (target
    /// rectangles `[x0, y0, x1, y1)`), each drawn exactly as a full render
    /// would draw it: the raster kernel shares nothing between the lanes
    /// of a workgroup, so lanes outside a rectangle are simply not run.
    pub fn render_rects(&self, frame: &Frame, target: &mut [u32], tiles: &[[u32; 4]], pixels: &[[u32; 4]]) -> io::Result<()> {
        if pixels.is_empty() {
            return self.render_tiles(frame, target, tiles);
        }
        frame.validate(target.len()).map_err(err)?;
        let h = &frame.header;
        let mut mask = vec![0u32; frame.mask_words()];
        let header = [frame.header];
        let mut table = [SsboDesc::default(); binding::COUNT];
        table[binding::FRAME as usize] = desc(&header);
        table[binding::OPS as usize] = desc(&frame.ops);
        table[binding::TEX as usize] = desc(&frame.tex);
        table[binding::TEXELS as usize] = desc(&frame.texels);
        table[binding::MASK as usize] = desc(&mask);
        table[binding::TARGET as usize] = desc(target);
        table[binding::LUT as usize] = desc(&frame.lut);
        let per = crate::TILE / RASTER_LOCAL;
        let l = RASTER_LOCAL;
        // Workgroup rects of the tiles, then each pixel rectangle as the
        // workgroups it touches with the lanes inside it.
        let mut items: Vec<[u32; 6]> = Vec::new();
        for t in tiles {
            for gy in t[1] * per..(t[3] * per).min(h.height.div_ceil(l)) {
                for gx in t[0] * per..(t[2] * per).min(h.width.div_ceil(l)) {
                    items.push([gx, gy, 0, 0, l, l]);
                }
            }
        }
        for r in pixels {
            let r = [r[0], r[1], r[2].min(h.width), r[3].min(h.height)];
            if r[0] >= r[2] || r[1] >= r[3] {
                continue;
            }
            for gy in r[1] / l..r[3].div_ceil(l) {
                for gx in r[0] / l..r[2].div_ceil(l) {
                    let (x0, y0) = (gx * l, gy * l);
                    items.push([gx, gy, r[0].max(x0) - x0, r[1].max(y0) - y0, r[2].min(x0 + l) - x0, r[3].min(y0 + l) - y0]);
                }
            }
        }
        let t = SsboTable::new(&table);
        // SAFETY: as in `render_tiles`; every lane run is inside the
        // raster grid.
        unsafe {
            self.dispatch(self.bin.1, &t, h.bin_groups(), [BIN_LOCAL, 1, 1]);
            self.dispatch_lanes(self.raster.1, &t, &items);
        }
        Ok(())
    }

    /// Run `cs` for workgroups `[gx, gy]` with only lanes `lx0..lx1`,
    /// `ly0..ly1` of each (items `[gx, gy, lx0, ly0, lx1, ly1]`).
    unsafe fn dispatch_lanes(&self, cs: CsMain, table: &SsboTable, items: &[[u32; 6]]) {
        if items.is_empty() {
            return;
        }
        let n = self.threads.min(items.len()).max(1);
        let chunk = items.len().div_ceil(n);
        let table_addr = table.as_ptr() as usize;
        let _ = table;
        std::thread::scope(|s| {
            for part in items.chunks(chunk) {
                s.spawn(move || {
                    let mut img = atrium_spv_runtime::image_table_buffer(0);
                    atrium_spv_runtime::write_barrier_helper_ptr(&mut img, atrium_spv_runtime::atrium_barrier);
                    let out = table_addr as *mut u8;
                    for &[gx, gy, x0, y0, x1, y1] in part {
                        for ly in y0..y1 {
                            for lx in x0..x1 {
                                cs(img.as_ptr(), std::ptr::null(), out, gx, gy, 0, lx, ly, 0, std::ptr::null_mut());
                            }
                        }
                    }
                });
            }
        });
    }

    /// Both dispatches over a caller-built binding table (one `{ base,
    /// size }` per binding), e.g. buffers placed against guard pages by the
    /// fuzzer.
    ///
    /// # Safety
    /// Each pointer must address a live buffer the size [`Frame::validate`]
    /// accepted for `header`, which must be the header `table` points to;
    /// the mask (zeroed, [`Frame::mask_words`] words) and target writable.
    pub unsafe fn render_table(&self, table: &[SsboDesc; binding::COUNT], header: &crate::Header) {
        let t = SsboTable::new(table);
        self.dispatch(self.bin.1, &t, header.bin_groups(), [BIN_LOCAL, 1, 1]);
        self.dispatch(self.raster.1, &t, header.raster_groups(), [RASTER_LOCAL, RASTER_LOCAL, 1]);
    }

    /// Stack the surfaces in `heap` into `target` by `c`'s layer list.
    pub fn compose(&self, c: &crate::compose::Compose, heap: &[u32], target: &mut [u32]) -> io::Result<()> {
        use crate::compose::{binding as cb, COMPOSE_LOCAL};
        c.validate(target.len(), heap.len()).map_err(err)?;
        let header = [c.header];
        let mut table = [SsboDesc::default(); cb::COUNT];
        table[cb::HEADER as usize] = desc(&header);
        table[cb::LAYERS as usize] = desc(&c.layers);
        table[cb::HEAP as usize] = desc(heap);
        table[cb::TARGET as usize] = desc(target);
        table[cb::LUT as usize] = desc(&c.lut);
        let table = SsboTable::new(&table);
        // SAFETY: the pointers outlive the dispatch, and `validate` checked
        // the header and every layer's surface against these buffers.
        unsafe { self.dispatch(self.compose.1, &table, c.groups(), [COMPOSE_LOCAL, COMPOSE_LOCAL, 1]) };
        Ok(())
    }

    /// Like [`Tier2::compose`], but only over the screen rects `rects`
    /// (pixels, `[x0, y0, x1, y1)`); the rest of `target` keeps what it
    /// holds, so it must hold this compose's result there already.
    pub fn compose_rects(&self, c: &crate::compose::Compose, heap: &[u32], target: &mut [u32], rects: &[[u32; 4]]) -> io::Result<()> {
        use crate::compose::{binding as cb, COMPOSE_LOCAL};
        c.validate(target.len(), heap.len()).map_err(err)?;
        let g = c.groups();
        let groups: Vec<[u32; 4]> = rects
            .iter()
            .map(|r| [r[0] / COMPOSE_LOCAL, r[1] / COMPOSE_LOCAL, r[2].div_ceil(COMPOSE_LOCAL).min(g[0]), r[3].div_ceil(COMPOSE_LOCAL).min(g[1])])
            .filter(|r| r[2] > r[0] && r[3] > r[1])
            .collect();
        if groups.is_empty() {
            return Ok(());
        }
        if groups.iter().any(|r| r[0] == 0 && r[1] == 0 && r[2] >= g[0] && r[3] >= g[1]) {
            return self.compose(c, heap, target); // all of it
        }
        // Where the top layer is opaque the pixel is the surface's: copy
        // it here, and run the kernel only on workgroups with a blend.
        let (l, pitch) = (COMPOSE_LOCAL, c.header.pitch);
        let mut blend: Vec<[u32; 4]> = Vec::new();
        for r in &groups {
            for gy in r[1]..r[3] {
                for gx in r[0]..r[2] {
                    let mut all = true;
                    'px: for y in gy * l..((gy + 1) * l).min(c.header.height) {
                        for x in gx * l..((gx + 1) * l).min(c.header.width) {
                            match crate::compose::opaque_pixel(c, heap, x, y) {
                                Some(p) => target[(y * pitch + x) as usize] = p,
                                None => {
                                    all = false;
                                    break 'px;
                                }
                            }
                        }
                    }
                    if !all {
                        blend.push([gx, gy, gx + 1, gy + 1]);
                    }
                }
            }
        }
        let groups = blend;
        if groups.is_empty() {
            return Ok(());
        }
        let header = [c.header];
        let mut table = [SsboDesc::default(); cb::COUNT];
        table[cb::HEADER as usize] = desc(&header);
        table[cb::LAYERS as usize] = desc(&c.layers);
        table[cb::HEAP as usize] = desc(heap);
        table[cb::TARGET as usize] = desc(target);
        table[cb::LUT as usize] = desc(&c.lut);
        let table = SsboTable::new(&table);
        // SAFETY: as in `compose`; the group rects are within its grid.
        unsafe { self.dispatch_rects(self.compose.1, &table, &groups, [COMPOSE_LOCAL, COMPOSE_LOCAL, 1]) };
        Ok(())
    }

    /// Run the workgroups in `rects` (`[gx0, gy0, gx1, gy1)`, z 0), each
    /// once even where rects overlap, spread over the threads.
    unsafe fn dispatch_rects(&self, cs: CsMain, table: &SsboTable, rects: &[[u32; 4]], local: [u32; 3]) {
        let mut ids: Vec<(u32, u32)> = rects
            .iter()
            .flat_map(|r| (r[1]..r[3]).flat_map(move |y| (r[0]..r[2]).map(move |x| (x, y))))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() {
            return;
        }
        let n = self.threads.min(ids.len()).max(1);
        let chunk = ids.len().div_ceil(n);
        let table_addr = table.as_ptr() as usize;
        let _ = table;
        std::thread::scope(|s| {
            for part in ids.chunks(chunk) {
                s.spawn(move || {
                    let mut img = atrium_spv_runtime::image_table_buffer(0);
                    atrium_spv_runtime::write_barrier_helper_ptr(&mut img, atrium_spv_runtime::atrium_barrier);
                    let out = table_addr as *mut u8;
                    for &(gx, gy) in part {
                        for lz in 0..local[2] {
                            for ly in 0..local[1] {
                                for lx in 0..local[0] {
                                    cs(img.as_ptr(), std::ptr::null(), out, gx, gy, 0, lx, ly, lz, std::ptr::null_mut());
                                }
                            }
                        }
                    }
                });
            }
        });
    }

    unsafe fn dispatch(&self, cs: CsMain, table: &SsboTable, groups: [u32; 3], local: [u32; 3]) {
        let total = groups[0] as u64 * groups[1] as u64 * groups[2] as u64;
        let n = (self.threads as u64).min(total).max(1);
        let chunk = total.div_ceil(n);
        let table_addr = table.as_ptr() as usize;
        let _ = table; // borrowed for the whole scope below
        std::thread::scope(|s| {
            for t in 0..n {
                let (lo, hi) = (t * chunk, ((t + 1) * chunk).min(total));
                s.spawn(move || {
                    // X0: the storage-image table; the bespoke backend lowers
                    // a barrier through its slot, so it must not be null.
                    let mut img = atrium_spv_runtime::image_table_buffer(0);
                    atrium_spv_runtime::write_barrier_helper_ptr(&mut img, atrium_spv_runtime::atrium_barrier);
                    let out = table_addr as *mut u8;
                    for w in lo..hi {
                        let gx = (w % groups[0] as u64) as u32;
                        let gy = ((w / groups[0] as u64) % groups[1] as u64) as u32;
                        let gz = (w / (groups[0] as u64 * groups[1] as u64)) as u32;
                        for lz in 0..local[2] {
                            for ly in 0..local[1] {
                                for lx in 0..local[0] {
                                    cs(img.as_ptr(), std::ptr::null(), out, gx, gy, gz, lx, ly, lz, std::ptr::null_mut());
                                }
                            }
                        }
                    }
                });
            }
        });
    }
}
