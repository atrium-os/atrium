//! Run the two kernels on Tier-2: Atrium's AOT SPIR-V to native CPU code.
//!
//! The kernels are compiled once through `atrium-spv-loader` (which spawns
//! `atrium-spv-compile` on a cache miss and maps the result), then
//! dispatched here with the compute calling convention
//! `aqueduct-gpu-host::Tier2Backend::dispatch_compute` uses: X0 the
//! storage-image table (its barrier slot filled), X2 a table of one pointer
//! per binding, one call per invocation. Workgroups are spread over threads;
//! the kernels use no shared memory or barriers, so lanes run serially.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;

use atrium_spv_loader::{CsMain, LoadedShader, LoaderConfig, ShaderCache};

use crate::{binding, Frame, BIN_LOCAL, BIN_SPV, RASTER_LOCAL, RASTER_SPV};

/// The two compiled kernels.
pub struct Tier2 {
    _cache: ShaderCache,
    bin: (Arc<LoadedShader>, CsMain),
    raster: (Arc<LoadedShader>, CsMain),
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
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
        Ok(Self { _cache: cache, bin, raster, threads })
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
        let dir = cache_root.join(format!("v{}", atrium_spv_ir::TIER2_SHADER_ABI_VERSION));
        std::fs::create_dir_all(&dir)?;
        for (name, spv) in [("bin", BIN_SPV), ("raster", RASTER_SPV)] {
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

    /// Render `frame` into `target` (`pitch * height` BGRA8 words).
    pub fn render(&self, frame: &Frame, target: &mut [u32]) -> io::Result<()> {
        if target.len() < frame.target_words() {
            return Err(err(format!("target has {} words, need {}", target.len(), frame.target_words())));
        }
        let mut mask = vec![0u32; frame.mask_words()];
        let header = [frame.header];
        let mut table = [0u64; binding::COUNT];
        table[binding::FRAME as usize] = header.as_ptr() as u64;
        table[binding::OPS as usize] = frame.ops.as_ptr() as u64;
        table[binding::TEX as usize] = frame.tex.as_ptr() as u64;
        table[binding::TEXELS as usize] = frame.texels.as_ptr() as u64;
        table[binding::MASK as usize] = mask.as_mut_ptr() as u64;
        table[binding::TARGET as usize] = target.as_mut_ptr() as u64;
        table[binding::LUT as usize] = frame.lut.as_ptr() as u64;
        // SAFETY: every pointer in `table` stays valid until both dispatches
        // return; the kernels read only within their buffers' sizes, which
        // `Frame` sized from the same header the kernels read.
        unsafe {
            self.dispatch(self.bin.1, &table, frame.bin_groups(), [BIN_LOCAL, 1, 1]);
            self.dispatch(self.raster.1, &table, frame.raster_groups(), [RASTER_LOCAL, RASTER_LOCAL, 1]);
        }
        Ok(())
    }

    unsafe fn dispatch(&self, cs: CsMain, table: &[u64], groups: [u32; 3], local: [u32; 3]) {
        let total = groups[0] as u64 * groups[1] as u64 * groups[2] as u64;
        let n = (self.threads as u64).min(total).max(1);
        let chunk = total.div_ceil(n);
        let table_addr = table.as_ptr() as usize;
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
