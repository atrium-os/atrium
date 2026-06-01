//! P2.3 decision probe: measure the REAL emitted span entry
//! (`atrium_fs_main_span`, P2.2) vs the per-pixel `fs_main`, to
//! decide whether the rasterizer-side span gather/scatter is worth
//! building.
//!
//! Renders a full-screen 4K coverage (constant-colour FS — the
//! gated span subset) three ways, all doing the SAME per-pixel
//! coverage/interp/write work, differing only in how the fragment
//! shader is invoked:
//!   * per-pixel `fs_main` (today's path)
//!   * span `fs_main_span` over LANES-wide chunks (P2.2 codegen)
//!   * no FS call at all (write a constant) — the floor: the most
//!     the span could ever save is (per-pixel) − (floor).
//!
//!   cargo run -p aqueduct-gpu-host --example bench_fs_span --release

use std::path::PathBuf;
use std::time::Instant;
use rayon::prelude::*;

use aqueduct_gpu_host::Tier2Registry;
use atrium_spv_loader::LoaderConfig;

const W: usize = 3840;
const H: usize = 2160;
const BAND: usize = 64;
const LANES: usize = 8;

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

type FsMain = atrium_spv_loader::FsMain;
type FsSpanMain = atrium_spv_loader::FsSpanMain;

#[inline]
fn u8f(f: f32) -> u8 { (f.clamp(0.0, 1.0) * 255.0 + 0.5) as u8 }

/// Per-pixel: call fs_main once per pixel (today's path).
fn render_perpixel(fb: &mut [u8], fs: FsMain) {
    fb.par_chunks_mut(BAND * W * 4).enumerate().for_each(|(bi, band)| {
        let band_h = band.len() / (W * 4);
        let mut out = [0.0f32; 4];
        let mut od = 0.0f32;
        for ly in 0..band_h {
            let py = bi * BAND + ly;
            let cy = py as f32 + 0.5;
            for px in 0..W {
                let cx = px as f32 + 0.5;
                unsafe {
                    fs(std::ptr::null(), std::ptr::null(), std::ptr::null(),
                       cx, cy, 0.0, 1.0, 0, out.as_mut_ptr(), &mut od, 1, 0);
                }
                let i = (ly * W + px) * 4;
                band[i] = u8f(out[0]); band[i+1] = u8f(out[1]);
                band[i+2] = u8f(out[2]); band[i+3] = u8f(out[3]);
            }
        }
    });
}

/// Span: gather LANES pixels, one fs_span call, scatter.
fn render_span(fb: &mut [u8], fs: FsSpanMain) {
    fb.par_chunks_mut(BAND * W * 4).enumerate().for_each(|(bi, band)| {
        let band_h = band.len() / (W * 4);
        let mut fx = [0.0f32; LANES];
        let mut fy = [0.0f32; LANES];
        let fz = [0.0f32; LANES];
        let fw = [1.0f32; LANES];
        let mut out = [0.0f32; LANES * 4];
        let mut od = [0.0f32; LANES];
        for ly in 0..band_h {
            let py = bi * BAND + ly;
            let cy = py as f32 + 0.5;
            let mut px = 0;
            while px < W {
                let n = LANES.min(W - px);
                for l in 0..n {
                    fx[l] = (px + l) as f32 + 0.5;
                    fy[l] = cy;
                }
                let mask: u64 = if n == 64 { u64::MAX } else { (1u64 << n) - 1 };
                unsafe {
                    fs(std::ptr::null(), 0, std::ptr::null(), std::ptr::null(),
                       fx.as_ptr(), fy.as_ptr(), fz.as_ptr(), fw.as_ptr(),
                       mask, 0, out.as_mut_ptr(), od.as_mut_ptr(), 0, 0, n as u32);
                }
                for l in 0..n {
                    let i = (ly * W + px + l) * 4;
                    band[i] = u8f(out[l*4]); band[i+1] = u8f(out[l*4+1]);
                    band[i+2] = u8f(out[l*4+2]); band[i+3] = u8f(out[l*4+3]);
                }
                px += n;
            }
        }
    });
}

/// Floor: no FS call, write a constant.
fn render_floor(fb: &mut [u8]) {
    fb.par_chunks_mut(BAND * W * 4).for_each(|band| {
        let band_h = band.len() / (W * 4);
        for ly in 0..band_h {
            for px in 0..W {
                let i = (ly * W + px) * 4;
                band[i] = 69; band[i+1] = 74; band[i+2] = 82; band[i+3] = 255;
            }
        }
    });
}

fn time_ms<F: FnMut()>(mut f: F, iters: u32) -> f64 {
    f();
    let t = Instant::now();
    for _ in 0..iters { f(); }
    t.elapsed().as_secs_f64() * 1000.0 / iters as f64
}

fn main() {
    // `dumpspv <path>`: write the constant-colour FS SPIR-V and exit
    // (for manual `atrium-spv-compile --force-backend cranelift`).
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 3 && args[1] == "dumpspv" {
        std::fs::write(&args[2], build_constant_color_fs([0.27,0.29,0.32,1.0])).unwrap();
        eprintln!("wrote {}", args[2]);
        return;
    }
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    println!("4K {W}x{H}  cores={cores}  LANES={LANES}  (full-screen constant-colour FS)\n");

    let cache = std::env::temp_dir().join(format!("fs_span_{}", std::process::id()));
    std::fs::create_dir_all(&cache).ok();
    let registry = Tier2Registry::new(LoaderConfig {
        cache_root: cache.clone(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    });
    let id = registry.register(&build_constant_color_fs([0.27,0.29,0.32,1.0])).unwrap();
    let loaded = registry.get(id).unwrap();
    let fs_main = loaded.entry_points.fs_main.expect("fs_main");
    let fs_span = loaded.entry_points.fs_span_main;
    println!("fs_span_main emitted: {}\n", fs_span.is_some());

    let mut fb = vec![0u8; W * H * 4];
    let pp = time_ms(|| render_perpixel(&mut fb, fs_main), 20);
    let fl = time_ms(|| render_floor(&mut fb), 20);
    let sp = fs_span.map(|fs| time_ms(|| render_span(&mut fb, fs), 20));

    println!("{:>22}  {:>9}", "path", "ms/frame");
    println!("{:>22}  {:>9.2}", "per-pixel fs_main", pp);
    match sp {
        Some(s) => println!("{:>22}  {:>9.2}  ({:.2}x vs per-pixel)", "span fs_main_span", s, pp / s),
        None    => println!("{:>22}  {:>9}", "span fs_main_span", "n/a"),
    }
    println!("{:>22}  {:>9.2}  (no FS call)", "floor (const write)", fl);
    println!("\nFS-call overhead = per-pixel − floor = {:.2} ms ({:.0}% of per-pixel).",
             pp - fl, 100.0 * (pp - fl) / pp);
    if let Some(s) = sp {
        println!("Span captured {:.0}% of that headroom.",
                 100.0 * (pp - s) / (pp - fl).max(0.0001));
    }
    // NOTE: simple shaders compile to bespoke (.afblob), which does
    // not emit a span entry — so `fs_span_main` is None and the span
    // row reads n/a unless the shader falls back to cranelift.  The
    // per-pixel vs floor delta still measures the FS-call overhead
    // the span is designed to remove.
    std::fs::remove_dir_all(&cache).ok();
}
