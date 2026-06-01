//! P1b.2 measurement: per-DRAW vs per-PASS rayon dispatch.
//!
//! This isolates the exact thing P1b.2 changed.  Both models do
//! the SAME pixel work (same triangles, same coverage + FS call +
//! blend, same band-parallel rasterizer); they differ ONLY in how
//! the rayon fan-out is structured:
//!
//!   * per-DRAW  (pre-P1b.2): one `par_chunks_mut` fan-out PER draw.
//!                A frame of N draws pays the chunk-split + task-Vec
//!                + rayon fan-out N times.
//!   * per-PASS  (P1b.2):     bin every draw's triangles into bands
//!                once, then ONE `par_chunks_mut` fan-out for the
//!                whole frame.
//!
//! Scene: `NDRAW` opaque quads tiling the 4K framebuffer (≈ full-
//! screen coverage regardless of N, so per-pixel work is constant
//! and the delta is purely dispatch overhead).  Swept over a range
//! of N to show per-draw cost growing with draw count while per-
//! pass stays flat — the win that lets a many-widget compositor
//! frame hit 4K@120.
//!
//!   cargo run -p aqueduct-gpu-host --example bench_tier2_passbatch --release

use std::path::PathBuf;
use std::time::Instant;
use rayon::prelude::*;

use aqueduct_gpu_host::Tier2Registry;
use atrium_spv_loader::LoaderConfig;

const W: usize = 3840;
const H: usize = 2160;
const BAND: usize = 64;
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

type FsMain = atrium_spv_loader::FsMain;

fn cpu_secs() -> f64 {
    unsafe {
        let mut ru: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut ru);
        ru.ru_utime.tv_sec as f64 + ru.ru_utime.tv_usec as f64 * 1e-6
            + ru.ru_stime.tv_sec as f64 + ru.ru_stime.tv_usec as f64 * 1e-6
    }
}

#[inline]
fn u8f(f: f32) -> u8 { (f.clamp(0.0, 1.0) * 255.0 + 0.5) as u8 }
#[inline]
fn edge(ax: f32, ay: f32, bx: f32, by: f32, px: f32, py: f32) -> f32 {
    (bx - ax) * (py - ay) - (by - ay) * (px - ax)
}

#[derive(Clone, Copy)]
struct Tri {
    v: [(f32, f32); 3],
    ymin: i32, ymax: i32, xmin: i32, xmax: i32,
}
fn tri(a: (f32,f32), b: (f32,f32), c: (f32,f32)) -> Tri {
    let xs = [a.0, b.0, c.0]; let ys = [a.1, b.1, c.1];
    Tri {
        v: [a, b, c],
        xmin: xs.iter().cloned().fold(f32::INFINITY, f32::min).floor() as i32,
        xmax: xs.iter().cloned().fold(f32::NEG_INFINITY, f32::max).ceil() as i32,
        ymin: ys.iter().cloned().fold(f32::INFINITY, f32::min).floor() as i32,
        ymax: ys.iter().cloned().fold(f32::NEG_INFINITY, f32::max).ceil() as i32,
    }
}

/// One "draw" = a quad (two triangles) covering a grid cell.
struct Draw { tris: [Tri; 2] }

/// Build `ndraw` quads tiling the framebuffer in a near-square grid
/// so their union ≈ the whole screen (constant per-pixel work).
fn build_draws(ndraw: usize) -> Vec<Draw> {
    let cols = (ndraw as f64).sqrt().ceil() as usize;
    let rows = (ndraw + cols - 1) / cols;
    let cw = W as f32 / cols as f32;
    let ch = H as f32 / rows as f32;
    let mut out = Vec::with_capacity(ndraw);
    for i in 0..ndraw {
        let gx = i % cols; let gy = i / cols;
        let x0 = gx as f32 * cw; let y0 = gy as f32 * ch;
        let x1 = x0 + cw;        let y1 = y0 + ch;
        out.push(Draw { tris: [
            tri((x0,y0),(x1,y0),(x1,y1)),
            tri((x0,y0),(x1,y1),(x0,y1)),
        ]});
    }
    out
}

/// Rasterize a band-local slice given the triangles that touch it.
fn raster_band(band: &mut [u8], band_top: usize, band_h: usize,
               tris: &[Tri], fs: FsMain) {
    let mut out = [0.0f32; 4];
    let mut od = 0.0f32;
    for tr in tris {
        let [a, b, c] = tr.v;
        let area = edge(a.0,a.1, b.0,b.1, c.0,c.1);
        if area == 0.0 { continue; }
        let y0 = tr.ymin.max(band_top as i32);
        let y1 = tr.ymax.min((band_top + band_h) as i32);
        let x0 = tr.xmin.max(0);
        let x1 = tr.xmax.min(W as i32);
        if y0 >= y1 || x0 >= x1 { continue; }
        for py in y0..y1 {
            let fy = py as f32 + 0.5;
            let row = (py as usize - band_top) * W * 4;
            for px in x0..x1 {
                let fx = px as f32 + 0.5;
                let e0 = edge(a.0,a.1, b.0,b.1, fx, fy);
                let e1 = edge(b.0,b.1, c.0,c.1, fx, fy);
                let e2 = edge(c.0,c.1, a.0,a.1, fx, fy);
                let inside = (e0>=0.0 && e1>=0.0 && e2>=0.0)
                          || (e0<=0.0 && e1<=0.0 && e2<=0.0);
                if !inside { continue; }
                unsafe {
                    fs(std::ptr::null(), std::ptr::null(), std::ptr::null(),
                       fx, fy, 0.0, 1.0, 0, out.as_mut_ptr(), &mut od, 1, 0);
                }
                let i = row + px as usize * 4;
                band[i]   = u8f(out[0]);
                band[i+1] = u8f(out[1]);
                band[i+2] = u8f(out[2]);
                band[i+3] = u8f(out[3]);
            }
        }
    }
}

/// per-PASS: bin all draws' tris into bands once, ONE fan-out.
fn render_perpass(fb: &mut [u8], draws: &[Draw], fs: FsMain, n_bands: usize) {
    let mut bins: Vec<Vec<Tri>> = vec![Vec::new(); n_bands];
    for d in draws {
        for tr in &d.tris {
            let b0 = (tr.ymin.max(0) as usize) / BAND;
            let b1 = ((tr.ymax.max(0) as usize) / BAND).min(n_bands - 1);
            for b in b0..=b1 { bins[b].push(*tr); }
        }
    }
    fb.par_chunks_mut(BAND * W * 4).enumerate().for_each(|(bi, band)| {
        let band_h = band.len() / (W * 4);
        raster_band(band, bi * BAND, band_h, &bins[bi], fs);
    });
}

/// per-DRAW: a separate fan-out PER draw (pre-P1b.2 daemon shape).
fn render_perdraw(fb: &mut [u8], draws: &[Draw], fs: FsMain) {
    for d in draws {
        let tris = d.tris;
        fb.par_chunks_mut(BAND * W * 4).enumerate().for_each(|(bi, band)| {
            let band_h = band.len() / (W * 4);
            raster_band(band, bi * BAND, band_h, &tris, fs);
        });
    }
}

fn time_ms<F: FnMut()>(mut f: F, iters: u32) -> f64 {
    f(); // warm
    let t = Instant::now();
    for _ in 0..iters { f(); }
    t.elapsed().as_secs_f64() * 1000.0 / iters as f64
}

fn main() {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    println!("4K {W}x{H} @120fps budget {BUDGET_MS:.2} ms   cores={cores}");
    println!("per-PASS = one rayon fan-out per frame (P1b.2);  \
              per-DRAW = one fan-out per draw (pre-P1b.2)\n");

    let cache = std::env::temp_dir().join(format!("tier2_passbatch_{}", std::process::id()));
    std::fs::create_dir_all(&cache).ok();
    let registry = Tier2Registry::new(LoaderConfig {
        cache_root: cache.clone(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    });
    let fs = registry.get(registry.register(
        &build_constant_color_fs([0.27,0.29,0.32,1.0])).unwrap())
        .unwrap().entry_points.fs_main.unwrap();

    let n_bands = (H + BAND - 1) / BAND;
    println!("{:>7}  {:>12}  {:>12}  {:>9}", "draws", "per-PASS ms", "per-DRAW ms", "speedup");
    for &ndraw in &[16usize, 64, 256, 1024, 4096] {
        let draws = build_draws(ndraw);
        let iters = if ndraw >= 1024 { 5 } else { 20 };

        let mut fb = vec![0u8; W * H * 4];
        let pass_ms = time_ms(|| render_perpass(&mut fb, &draws, fs, n_bands), iters);

        let c0 = cpu_secs();
        let mut fb2 = vec![0u8; W * H * 4];
        let draw_ms = time_ms(|| render_perdraw(&mut fb2, &draws, fs), iters);
        let _ = cpu_secs() - c0;

        let budget = if pass_ms <= BUDGET_MS { "✓<120fps" } else { "✗" };
        println!("{:>7}  {:>9.2} {}  {:>12.2}  {:>8.1}x",
                 ndraw, pass_ms, budget, draw_ms, draw_ms / pass_ms);
    }
    std::fs::remove_dir_all(&cache).ok();
}
