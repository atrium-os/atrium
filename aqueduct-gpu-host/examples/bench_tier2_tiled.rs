//! P1 prototype + measurement: a tile-binned, per-band-parallel,
//! damage-aware Tier-2-style rasterizer.
//!
//! This proves the P1 architecture BEFORE refactoring the daemon:
//! it rasterizes the UI scene as screen-space TRIANGLES (faithful
//! to real Vulkan content), binned into horizontal bands,
//! rayon-parallel across bands, calling the actual cranelift-
//! compiled `fs_main` per covered pixel (so the per-pixel-call +
//! coverage + blend costs are all included -- i.e. the true
//! P1-only number, pre P2/P3).
//!
//! Compares against:
//!   * the current per-primitive Tier-2 dispatch is ~152ms@720p
//!     (bench_tinyskia_vs_tier2) -- the thing this replaces.
//!   * tiled tiny-skia ~2.5ms@4K (bench_4k_tiled) -- the SIMD-
//!     blitter reference ceiling.
//!
//! Also measures a DAMAGE frame (one small dirty rect) to show
//! steady-state cost scales with damage area, not screen area.
//!
//!   cargo run -p aqueduct-gpu-host --example bench_tier2_tiled --release

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

#[inline]
fn u8f(f: f32) -> u8 { (f.clamp(0.0, 1.0) * 255.0 + 0.5) as u8 }
#[inline]
fn edge(ax: f32, ay: f32, bx: f32, by: f32, px: f32, py: f32) -> f32 {
    (bx - ax) * (py - ay) - (by - ay) * (px - ax)
}

#[derive(Clone, Copy)]
struct Tri {
    v: [(f32, f32); 3],
    opaque: bool, // false => SrcOver alpha glyph
    ymin: i32, ymax: i32, xmin: i32, xmax: i32,
}
fn tri(a: (f32,f32), b: (f32,f32), c: (f32,f32), opaque: bool) -> Tri {
    let xs = [a.0, b.0, c.0]; let ys = [a.1, b.1, c.1];
    Tri {
        v: [a, b, c], opaque,
        xmin: xs.iter().cloned().fold(f32::INFINITY, f32::min).floor() as i32,
        xmax: xs.iter().cloned().fold(f32::NEG_INFINITY, f32::max).ceil() as i32,
        ymin: ys.iter().cloned().fold(f32::INFINITY, f32::min).floor() as i32,
        ymax: ys.iter().cloned().fold(f32::NEG_INFINITY, f32::max).ceil() as i32,
    }
}
/// Tessellate a pixel rect into two screen-space triangles.
fn rect(x: f32, y: f32, w: f32, h: f32, opaque: bool, out: &mut Vec<Tri>) {
    let (x0,y0,x1,y1) = (x, y, x+w, y+h);
    out.push(tri((x0,y0),(x1,y0),(x1,y1), opaque));
    out.push(tri((x0,y0),(x1,y1),(x0,y1), opaque));
}

fn build_scene() -> Vec<Tri> {
    let mut t = Vec::new();
    rect(0.0, 0.0, W as f32, H as f32, true, &mut t); // bg
    for gy in 0..8 { for gx in 0..12 {
        rect(24.0+gx as f32*315.0, 24.0+gy as f32*265.0, 290.0, 240.0, true, &mut t);
    }}
    for r in 0..100 { for c in 0..160 {
        rect(36.0+c as f32*10.0, 36.0+r as f32*18.0, 9.0, 14.0, false, &mut t);
    }}
    t
}

/// Rasterize one band's triangle list into `band` (band_h rows of
/// W RGBA8), with band-local top row `band_top`.  `clip` optionally
/// restricts to a screen-space dirty rect (x0,y0,x1,y1).
fn raster_band(
    band: &mut [u8], band_top: usize, band_h: usize,
    tris: &[Tri], fs_solid: FsMain, fs_alpha: FsMain,
    clip: Option<(i32,i32,i32,i32)>,
) {
    let mut out = [0.0f32; 4];
    let mut od = 0.0f32;
    let (cx0, cy0, cx1, cy1) = clip.unwrap_or((0, 0, W as i32, H as i32));
    for tr in tris {
        let [a, b, c] = tr.v;
        let area = edge(a.0,a.1, b.0,b.1, c.0,c.1);
        if area == 0.0 { continue; }
        let y0 = tr.ymin.max(band_top as i32).max(cy0);
        let y1 = tr.ymax.min((band_top + band_h) as i32).min(cy1);
        let x0 = tr.xmin.max(0).max(cx0);
        let x1 = tr.xmax.min(W as i32).min(cx1);
        if y0 >= y1 || x0 >= x1 { continue; }
        let fs = if tr.opaque { fs_solid } else { fs_alpha };
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
                // SAFETY: dlopened C-ABI FS, valid out pointers.
                unsafe {
                    fs(std::ptr::null(), std::ptr::null(), std::ptr::null(),
                       fx, fy, 0.0, 1.0, 0, out.as_mut_ptr(), &mut od, 1, 0);
                }
                let i = row + px as usize * 4;
                if tr.opaque {
                    band[i]   = u8f(out[0]);
                    band[i+1] = u8f(out[1]);
                    band[i+2] = u8f(out[2]);
                    band[i+3] = u8f(out[3]);
                } else {
                    // SrcOver, straight alpha.
                    let sa = out[3];
                    let ia = 1.0 - sa;
                    let d = [band[i] as f32/255.0, band[i+1] as f32/255.0,
                             band[i+2] as f32/255.0, band[i+3] as f32/255.0];
                    band[i]   = u8f(out[0]*sa + d[0]*ia);
                    band[i+1] = u8f(out[1]*sa + d[1]*ia);
                    band[i+2] = u8f(out[2]*sa + d[2]*ia);
                    band[i+3] = u8f(sa + d[3]*ia);
                }
            }
        }
    }
}

fn main() {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let scene = build_scene();
    println!("4K {W}x{H} @120fps budget {BUDGET_MS:.2} ms   cores={cores}   {} tris", scene.len());

    let cache = std::env::temp_dir().join(format!("tier2_tiled_{}", std::process::id()));
    std::fs::create_dir_all(&cache).ok();
    let registry = Tier2Registry::new(LoaderConfig {
        cache_root: cache.clone(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    });
    let fs_solid = registry.get(registry.register(&build_constant_color_fs([0.27,0.29,0.32,1.0])).unwrap())
        .unwrap().entry_points.fs_main.unwrap();
    let fs_alpha = registry.get(registry.register(&build_constant_color_fs([0.90,0.90,0.92,0.85])).unwrap())
        .unwrap().entry_points.fs_main.unwrap();

    let n_bands = (H + BAND - 1) / BAND;
    // Bin triangles into bands by y-bbox (full-frame).
    let mut bins: Vec<Vec<Tri>> = vec![Vec::new(); n_bands];
    for tr in &scene {
        let b0 = (tr.ymin.max(0) as usize) / BAND;
        let b1 = ((tr.ymax.max(0) as usize) / BAND).min(n_bands - 1);
        for b in b0..=b1 { bins[b].push(*tr); }
    }

    let mut fb = vec![0u8; W * H * 4];
    let render = |fb: &mut [u8], clip: Option<(i32,i32,i32,i32)>, bins: &[Vec<Tri>]| {
        fb.par_chunks_mut(BAND * W * 4).enumerate().for_each(|(bi, band)| {
            let band_h = band.len() / (W * 4);
            raster_band(band, bi * BAND, band_h, &bins[bi], fs_solid, fs_alpha, clip);
        });
    };

    // ── Full-frame ──
    for _ in 0..3 { render(&mut fb, None, &bins); }
    let iters = 30u32;
    let t0 = Instant::now();
    for _ in 0..iters { render(&mut fb, None, &bins); }
    let full_ms = t0.elapsed().as_secs_f64() * 1000.0 / iters as f64;

    // ── Damage frame: one small dirty rect (a caret line). ──
    // Re-bin only the tris overlapping the dirty rect, into the
    // bands it spans (work scales with damage, not screen).
    let dirty = (1800, 1040, 2000, 1080); // 200x40 px
    let mut dbins: Vec<Vec<Tri>> = vec![Vec::new(); n_bands];
    for tr in &scene {
        if tr.xmax < dirty.0 || tr.xmin > dirty.2 || tr.ymax < dirty.1 || tr.ymin > dirty.3 { continue; }
        let b0 = (tr.ymin.max(0) as usize) / BAND;
        let b1 = ((tr.ymax.max(0) as usize) / BAND).min(n_bands - 1);
        for b in b0..=b1 {
            if (b * BAND) as i32 > dirty.3 || ((b+1)*BAND) as i32 <= dirty.1 { continue; }
            dbins[b].push(*tr);
        }
    }
    for _ in 0..3 { render(&mut fb, Some(dirty), &dbins); }
    let t1 = Instant::now();
    for _ in 0..(iters*4) { render(&mut fb, Some(dirty), &dbins); }
    let dmg_ms = t1.elapsed().as_secs_f64() * 1000.0 / (iters*4) as f64;

    std::fs::remove_dir_all(&cache).ok();

    let v = |ms: f64| if ms <= BUDGET_MS { "MEETS 4K@120" } else { "misses" };
    println!();
    println!("  full-frame (tiled+binned)     : {full_ms:7.2} ms ({:6.1} fps)  [{}]",
             1000.0/full_ms, v(full_ms));
    println!("  damage frame (200x40 dirty)   : {dmg_ms:7.3} ms ({:6.0} fps)  [{}]",
             1000.0/dmg_ms, v(dmg_ms));
    println!();
    println!("  vs per-primitive Tier-2 (~152ms@720p) and tiled tiny-skia (~2.5ms@4K).");
    println!("  damage frame ~ {:.0}x cheaper than full repaint (work scales with damage).",
             full_ms / dmg_ms);
}
