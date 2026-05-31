//! Pre-investment ceiling probe: can a reworked Tier-2 hit
//! 4K@120 BEFORE we build P1-P4?
//!
//! A fully-reworked Tier-2 (tile-binned + batched + SIMD) removes
//! the three measured bottlenecks.  This probe isolates the part
//! the rework can't conjure away -- raw fragment-shading +
//! framebuffer-write throughput of the actual cranelift-compiled
//! shader -- by calling `fs_main` in a tight loop over a 4K-sized
//! workload, single-threaded and across cores.
//!
//! What each number predicts:
//!   * 1-core scalar          -> today's shading floor.
//!   * N-core scalar          -> the P1-ONLY result (dispatch
//!                               fixed, still scalar per-pixel
//!                               call).  This is the key gate: if
//!                               N-core scalar already beats the
//!                               8.33ms budget, P2/P3 are pure
//!                               headroom; if not, the gap is how
//!                               much P2 (kill per-pixel call) +
//!                               P3 (SIMD) must deliver.
//!
//! Budget: 4K (3840x2160) @ 120fps = 8.33 ms/frame.  We shade
//! `OVERDRAW`x the framebuffer to model real UI layering.
//!
//!   cargo run -p aqueduct-gpu-host --example bench_tier2_ceiling --release

use std::path::PathBuf;
use std::time::Instant;
use rayon::prelude::*;

use aqueduct_gpu_host::Tier2Registry;
use atrium_spv_loader::LoaderConfig;

const W: usize = 3840;
const H: usize = 2160;
const OVERDRAW: usize = 2;
const BUDGET_MS: f64 = 1000.0 / 120.0;

fn locate_compile_binary() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); p.push("atrium-spv-compile"); p.push("target"); p.push("debug");
    p.push("atrium-spv-compile");
    assert!(p.exists(), "build atrium-spv-compile first ({})", p.display());
    p
}

fn build_constant_color_fs(rgba: [f32; 4]) -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let ptr_out = b.type_pointer(None, StorageClass::Output, vec4);
    let cs: Vec<_> = rgba.iter().map(|x| b.constant_bit32(f32_ty, x.to_bits())).collect();
    let color = b.constant_composite(vec4, cs);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    b.store(out, color, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[inline]
fn f32_to_u8(f: f32) -> u8 { (f.clamp(0.0, 1.0) * 255.0 + 0.5) as u8 }

fn main() {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let npix = W * H;
    let shaded = npix * OVERDRAW;
    let mpix = shaded as f64 / 1e6;
    println!("4K {W}x{H}, {OVERDRAW}x overdraw => {mpix:.1} Mpix shaded/frame");
    println!("budget {BUDGET_MS:.2} ms/frame  => need {:.2} Gpix/s aggregate   cores={cores}",
             mpix / 1e3 / (BUDGET_MS / 1000.0));

    let cache = std::env::temp_dir().join(format!("tier2_ceiling_{}", std::process::id()));
    std::fs::create_dir_all(&cache).ok();
    let registry = Tier2Registry::new(LoaderConfig {
        cache_root: cache.clone(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    });
    let fs = registry.register(&build_constant_color_fs([0.27, 0.55, 0.90, 1.0])).expect("fs");
    let loaded = registry.get(fs).expect("loaded");
    let fs_main = loaded.entry_points.fs_main.expect("fs_main");

    let mut fb = vec![0u8; npix * 4];

    // Per-pixel work identical to the rasterizer's inner step:
    // call the compiled FS, convert its f32 output to RGBA8, store.
    let shade_range = |fb: &mut [u8], lo: usize, hi: usize| {
        let mut out = [0.0f32; 4];
        let mut od = 0.0f32;
        for p in lo..hi {
            // SAFETY: dlopened C-ABI FS, valid out pointers.
            unsafe {
                fs_main(std::ptr::null(), std::ptr::null(), std::ptr::null(),
                        0.0, 0.0, 0.0, 1.0, 0,
                        out.as_mut_ptr(), &mut od, 1, 0);
            }
            let i = p * 4;
            fb[i]   = f32_to_u8(out[0]);
            fb[i+1] = f32_to_u8(out[1]);
            fb[i+2] = f32_to_u8(out[2]);
            fb[i+3] = f32_to_u8(out[3]);
        }
    };

    // ── 1-core scalar ──
    for _ in 0..2 { shade_range(&mut fb, 0, npix); }
    let iters = 20usize;
    let t0 = Instant::now();
    for _ in 0..iters * OVERDRAW { shade_range(&mut fb, 0, npix); }
    let one_core_ms = t0.elapsed().as_secs_f64() * 1000.0 / iters as f64;

    // ── N-core scalar (P1-equivalent: dispatch fixed, scalar FS) ──
    let chunk = (npix + cores - 1) / cores;
    let render_par = |fb: &mut [u8]| {
        fb.par_chunks_mut(chunk * 4).enumerate().for_each(|(ci, c)| {
            let base = ci * chunk;
            let n = c.len() / 4;
            let mut out = [0.0f32; 4];
            let mut od = 0.0f32;
            for k in 0..n {
                unsafe {
                    fs_main(std::ptr::null(), std::ptr::null(), std::ptr::null(),
                            0.0, 0.0, 0.0, 1.0, 0,
                            out.as_mut_ptr(), &mut od, 1, 0);
                }
                let _ = base; // (base would feed real frag coords)
                let i = k * 4;
                c[i]   = f32_to_u8(out[0]);
                c[i+1] = f32_to_u8(out[1]);
                c[i+2] = f32_to_u8(out[2]);
                c[i+3] = f32_to_u8(out[3]);
            }
        });
    };
    for _ in 0..2 { render_par(&mut fb); }
    let t1 = Instant::now();
    for _ in 0..iters * OVERDRAW { render_par(&mut fb); }
    let n_core_ms = t1.elapsed().as_secs_f64() * 1000.0 / iters as f64;

    std::fs::remove_dir_all(&cache).ok();

    let gpix = |ms: f64| mpix / 1e3 / (ms / 1000.0);
    println!();
    println!("  1-core  scalar FS : {one_core_ms:8.2} ms/frame  ({:5.2} Gpix/s)", gpix(one_core_ms));
    println!("  {cores}-core scalar FS : {n_core_ms:8.2} ms/frame  ({:5.2} Gpix/s)   <- P1-only prediction",
             gpix(n_core_ms));
    println!();
    if n_core_ms <= BUDGET_MS {
        println!("  => P1 ALONE clears 4K@120 ({:.1}x under budget). P2/P3 = headroom.",
                 BUDGET_MS / n_core_ms);
    } else {
        let need = n_core_ms / BUDGET_MS;
        println!("  => P1 alone misses by {need:.1}x. P2 (kill per-pixel call) + P3 (SIMD)");
        println!("     must deliver >= {need:.1}x.  A 4-8x SoA-SIMD gain on the shader");
        println!("     would clear it iff per-pixel-call overhead (P2) doesn't dominate.");
    }
    println!();
    println!("  note: solid-colour FS is the OPTIMISTIC case (no texture fetch).");
    println!("        glyph/textured shading is ~2-3x heavier per pixel.");
}
