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
