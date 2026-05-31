//! Benchmark: tiny-skia (current scene-graph compositor SW
//! rasterizer) vs the Tier-2 SW rasterizer, on a representative
//! server-side UI frame.
//!
//! Use case under test: the scene-graph compositor that
//! rasterizes regular UI apps' 2D primitives (NOT the per-app
//! Vulkan path).  So the scene is what a UI compositor draws:
//!   * 1 opaque full-screen background fill
//!   * a grid of opaque panel rects (windows / surfaces)
//!   * thousands of small ALPHA-BLENDED quads (text glyphs)
//!
//! Both renderers do the same work at the same resolution into
//! an RGBA8 target; we time only the per-frame rasterization
//! (geometry + shader setup is hoisted out of the timed loop).
//! This isolates raster throughput -- exactly the axis the
//! "should Tier-2 replace tiny-skia for more FPS?" decision
//! turns on.
//!
//! NOTE on fairness:
//!   * No anti-aliasing on either side (straight coverage).
//!   * Glyphs here are solid alpha quads, not textured atlas
//!     samples -- a real glyph adds one texture fetch per pixel
//!     to BOTH renderers, roughly neutral to the comparison;
//!     the structural cost (many small blended quads: per-quad
//!     dispatch + per-pixel work) is preserved.
//!   * tiny-skia is single-threaded; the Tier-2 rasterizer uses
//!     rayon across stripes.  We report the machine's core count
//!     so the multi-core advantage is visible, not hidden.
//!
//! Run (after building atrium-spv-compile):
//!   cargo build -p atrium-spv-compile
//!   cargo run -p aqueduct-gpu-host --example bench_tinyskia_vs_tier2 --release

use std::path::PathBuf;
use std::time::Instant;

use aqueduct_gpu_host::{
    BlendState, BlendFactor, BlendFactorPair, BlendOp, DrawTriangle, Tier2Registry,
};
use aqueduct_gpu_host::tier2_registry::Viewport;
use atrium_spv_loader::LoaderConfig;

const W: u32 = 1280;
const H: u32 = 720;

// ── Locate the workspace-built atrium-spv-compile binary. ──
fn locate_compile_binary() -> PathBuf {
    // examples run from the crate dir; the compiler is a sibling
    // crate's debug artifact.
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.push("atrium-spv-compile");
    p.push("target");
    p.push("debug");
    p.push("atrium-spv-compile");
    if !p.exists() {
        panic!("atrium-spv-compile not found at {} -- build it: \
                (cd ../atrium-spv-compile && cargo build)", p.display());
    }
    p
}

// ── SPIR-V: passthrough VS (in vec3 pos -> gl_Position). ──
fn build_passthrough_vs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let i32_ty = b.type_int(32, 1);
    let vec3 = b.type_vector(f32_ty, 3);
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let per_vertex = b.type_struct(vec![vec4]);
    b.member_decorate(per_vertex, 0, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::Position)]);
    b.member_decorate(per_vertex, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(per_vertex, Decoration::Block, vec![]);
    let ptr_pv = b.type_pointer(None, StorageClass::Output, per_vertex);
    let ptr_out_vec4 = b.type_pointer(None, StorageClass::Output, vec4);
    let ptr_in_vec3 = b.type_pointer(None, StorageClass::Input, vec3);
    let in_pos = b.variable(ptr_in_vec3, None, StorageClass::Input, None);
    b.decorate(in_pos, Decoration::Location, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let pv_var = b.variable(ptr_pv, None, StorageClass::Output, None);
    let c_zero = b.constant_bit32(i32_ty, 0u32);
    let c_one_f = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let pos = b.load(vec3, None, in_pos, None, vec![]).unwrap();
    let x = b.composite_extract(f32_ty, None, pos, vec![0]).unwrap();
    let y = b.composite_extract(f32_ty, None, pos, vec![1]).unwrap();
    let z = b.composite_extract(f32_ty, None, pos, vec![2]).unwrap();
    let pos4 = b.composite_construct(vec4, None, vec![x, y, z, c_one_f]).unwrap();
    let dst = b.access_chain(ptr_out_vec4, None, pv_var, vec![c_zero]).unwrap();
    b.store(dst, pos4, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Vertex, main, "main", vec![in_pos, pv_var]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

// ── SPIR-V: constant-colour FS. ──
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

#[derive(Clone, Copy)]
struct RectPx { x: f32, y: f32, w: f32, h: f32 }

/// Build the representative UI scene: background, panels, glyphs.
fn build_scene() -> (RectPx, Vec<RectPx>, Vec<RectPx>) {
    let bg = RectPx { x: 0.0, y: 0.0, w: W as f32, h: H as f32 };

    // 6x4 grid of opaque panels.
    let mut panels = Vec::new();
    for gy in 0..4 {
        for gx in 0..6 {
            panels.push(RectPx {
                x: 20.0 + gx as f32 * 205.0,
                y: 20.0 + gy as f32 * 175.0,
                w: 185.0, h: 150.0,
            });
        }
    }

    // ~3200 small alpha "glyph" quads laid out as text rows
    // inside the panel band.
    let mut glyphs = Vec::new();
    let (gw, gh, adv, line) = (8.0f32, 12.0f32, 9.0f32, 16.0f32);
    let cols = 80;
    let rows = 40;
    for r in 0..rows {
        for c in 0..cols {
            glyphs.push(RectPx {
                x: 30.0 + c as f32 * adv,
                y: 30.0 + r as f32 * line,
                w: gw, h: gh,
            });
        }
    }
    (bg, panels, glyphs)
}

// ── tiny-skia renderer. ──
fn render_tinyskia(
    pm: &mut tiny_skia::Pixmap,
    bg: &RectPx, panels: &[RectPx], glyphs: &[RectPx],
) {
    use tiny_skia::{Paint, Rect, Transform, Color, Shader, BlendMode};
    let xf = Transform::identity();
    let mut opaque = Paint::default();
    opaque.anti_alias = false;
    opaque.blend_mode = BlendMode::Source; // opaque overwrite
    let mut glyph = Paint::default();
    glyph.anti_alias = false;
    glyph.blend_mode = BlendMode::SourceOver; // alpha blend

    let fill = |pm: &mut tiny_skia::Pixmap, p: &Paint, r: &RectPx| {
        if let Some(rect) = Rect::from_xywh(r.x, r.y, r.w, r.h) {
            pm.fill_rect(rect, p, xf, None);
        }
    };
    // background (opaque grey)
    opaque.shader = Shader::SolidColor(Color::from_rgba8(40, 42, 48, 255));
    fill(pm, &opaque, bg);
    // panels (opaque lighter grey)
    opaque.shader = Shader::SolidColor(Color::from_rgba8(70, 74, 82, 255));
    for r in panels { fill(pm, &opaque, r); }
    // glyphs (white, 85% alpha, SrcOver)
    glyph.shader = Shader::SolidColor(Color::from_rgba8(230, 230, 235, 217));
    for r in glyphs { fill(pm, &glyph, r); }
}

// ── Tier-2 renderer helpers. ──
fn ndc(px: f32, py: f32) -> (f32, f32) {
    (px / W as f32 * 2.0 - 1.0, py / H as f32 * 2.0 - 1.0)
}
/// Two triangles (6 verts) for a pixel rect, as vec3 NDC bytes.
fn rect_tris(r: &RectPx) -> [[u8; 36]; 2] {
    let (x0, y0) = ndc(r.x, r.y);
    let (x1, y1) = ndc(r.x + r.w, r.y + r.h);
    let v = |x: f32, y: f32| -> [f32; 3] { [x, y, 0.0] };
    let tri = |a: [f32;3], b: [f32;3], c: [f32;3]| -> [u8; 36] {
        let mut out = [0u8; 36];
        for (i, p) in [a, b, c].iter().enumerate() {
            for (j, f) in p.iter().enumerate() {
                out[i*12 + j*4 .. i*12 + j*4 + 4].copy_from_slice(&f.to_le_bytes());
            }
        }
        out
    };
    [
        tri(v(x0,y0), v(x1,y0), v(x1,y1)),
        tri(v(x0,y0), v(x1,y1), v(x0,y1)),
    ]
}

fn main() {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    println!("resolution {W}x{H}  cores={cores}  (tiny-skia single-threaded, \
              tier-2 rayon multi-stripe)");

    let (bg, panels, glyphs) = build_scene();
    let n_rects = 1 + panels.len() + glyphs.len();
    println!("scene: 1 bg + {} panels + {} glyph quads = {} rects",
             panels.len(), glyphs.len(), n_rects);

    // ── tiny-skia ──
    let mut pm = tiny_skia::Pixmap::new(W, H).expect("pixmap");
    // warmup
    for _ in 0..3 { pm.fill(tiny_skia::Color::TRANSPARENT); render_tinyskia(&mut pm, &bg, &panels, &glyphs); }
    let iters = 60u32;
    let t0 = Instant::now();
    for _ in 0..iters {
        pm.fill(tiny_skia::Color::TRANSPARENT);
        render_tinyskia(&mut pm, &bg, &panels, &glyphs);
    }
    let ts_ms = t0.elapsed().as_secs_f64() * 1000.0 / iters as f64;

    // ── Tier-2 setup (shaders compiled once, out of the loop) ──
    let cache = std::env::temp_dir().join(format!("tier2_bench_{}", std::process::id()));
    std::fs::create_dir_all(&cache).ok();
    let config = LoaderConfig {
        cache_root: cache.clone(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    };
    let registry = Tier2Registry::new(config);
    let vs = registry.register(&build_passthrough_vs()).expect("vs");
    let fs_bg = registry.register(&build_constant_color_fs([0.16, 0.16, 0.19, 1.0])).expect("fs bg");
    let fs_panel = registry.register(&build_constant_color_fs([0.27, 0.29, 0.32, 1.0])).expect("fs panel");
    let fs_glyph = registry.register(&build_constant_color_fs([0.90, 0.90, 0.92, 0.85])).expect("fs glyph");

    // SrcOver blend state for the glyph quads.
    let src_over = BlendState {
        enable: true,
        color: BlendFactorPair { src: BlendFactor::SrcAlpha, dst: BlendFactor::OneMinusSrcAlpha },
        alpha: BlendFactorPair { src: BlendFactor::One, dst: BlendFactor::OneMinusSrcAlpha },
        color_op: BlendOp::Add,
        alpha_op: BlendOp::Add,
        ..BlendState::default()
    };
    let opaque = BlendState::default();
    let vp = Viewport { x: 0.0, y: 0.0, width: W as f32, height: H as f32, min_depth: 0.0, max_depth: 1.0 };

    // Pre-tessellate all rects into triangle byte buffers.
    let bg_t = rect_tris(&bg);
    let panel_t: Vec<_> = panels.iter().map(rect_tris).collect();
    let glyph_t: Vec<_> = glyphs.iter().map(rect_tris).collect();

    let mut fb = vec![0u8; (W * H * 4) as usize];

    let draw = |fb: &mut [u8], tris: &[[u8;36]; 2], fs: aqueduct_gpu_host::Tier2ShaderId,
                blend: &BlendState| {
        for t in tris {
            let v0 = &t[0..12]; let v1 = &t[12..24]; let v2 = &t[24..36];
            let dt = DrawTriangle {
                vertex_attrs: [v0, v1, v2],
                varying_f32_count: 0,
                uniforms: &[],
                viewport: Some(vp),
                depth_write: false,
                blend_state: *blend,
                ..Default::default()
            };
            let _ = registry.fill_image_triangle(vs, fs, &dt, W, H, fb, None, None, &mut []);
        }
    };
    let render_tier2 = |fb: &mut [u8]| {
        for b in fb.iter_mut() { *b = 0; }
        draw(fb, &bg_t, fs_bg, &opaque);
        for t in &panel_t { draw(fb, t, fs_panel, &opaque); }
        for t in &glyph_t { draw(fb, t, fs_glyph, &src_over); }
    };
    // warmup
    for _ in 0..3 { render_tier2(&mut fb); }
    let t1 = Instant::now();
    for _ in 0..iters { render_tier2(&mut fb); }
    let t2_ms = t1.elapsed().as_secs_f64() * 1000.0 / iters as f64;

    std::fs::remove_dir_all(&cache).ok();

    // ── Report ──
    let mpix = (W as f64 * H as f64) / 1e6;
    println!();
    println!("  tiny-skia : {ts_ms:8.3} ms/frame   ({:6.1} fps,  {:7.1} Mpix/s)",
             1000.0 / ts_ms, mpix / (ts_ms / 1000.0));
    println!("  tier-2    : {t2_ms:8.3} ms/frame   ({:6.1} fps,  {:7.1} Mpix/s)",
             1000.0 / t2_ms, mpix / (t2_ms / 1000.0));
    println!();
    if t2_ms < ts_ms {
        println!("  => tier-2 is {:.2}x FASTER than tiny-skia on this frame", ts_ms / t2_ms);
    } else {
        println!("  => tier-2 is {:.2}x SLOWER than tiny-skia on this frame", t2_ms / ts_ms);
    }
}
