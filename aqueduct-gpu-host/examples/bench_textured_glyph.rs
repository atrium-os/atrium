//! Benchmark: REAL (textured) glyph rendering — tier-2 vs tiny-skia.
//!
//! The sibling `bench_tinyskia_vs_tier2` uses SOLID alpha quads for
//! "glyphs", which qualify for tier-2's const-fill / LUT / rect fast
//! paths.  Real glyphs DON'T: each pixel samples a glyph atlas (a
//! varying UV), so they fall to the general per-pixel textured path
//! (UV interpolation + a per-pixel `fs_main` + `atrium_tex_sample_2d`
//! + SrcOver).  This bench measures that real text path head-to-head:
//!
//!   tier-2  : textured quads sampling an RGBA glyph atlas, SrcOver,
//!             through the production `submit_frame` pipeline.
//!   tiny-skia: `draw_pixmap` the same atlas sprite per glyph, SrcOver
//!             (the standard glyph-cache blit).
//!
//! Same 3200-glyph layout, same atlas, same blend — only the renderer
//! differs.  Reports wall-clock (latency) AND core-ms (energy).
//!
//! Run:
//!   cargo build -p atrium-spv-compile
//!   cargo run -p aqueduct-gpu-host --example bench_textured_glyph --release

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use aqueduct_gpu_host::{Backend, Tier2Backend, Tier2Registry};
use aqueduct_gpu::ids::{IdNamespace, ResourceId};
use aqueduct_gpu::frame::{BindVertexBufCmd, DrawCmd, FrameBuilder, SetViewportCmd};
use aqueduct_gpu::opcodes::FrameOp;
use aqueduct_gpu::{
    Tier2BlendFactor, Tier2BlendOp, Tier2BlendState, Tier2PrimitiveTopology,
    VertexAttributeDesc, VertexBindingDesc, VertexFormat, VertexInputState,
};
use atrium_spv_loader::LoaderConfig;

const W: u32 = 1280;
const H: u32 = 720;
const GW: u32 = 8;   // glyph / atlas width
const GH: u32 = 12;  // glyph / atlas height

fn cpu_ms() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru); }
    let tv = |t: &libc::timeval| t.tv_sec as f64 * 1000.0 + t.tv_usec as f64 / 1000.0;
    tv(&ru.ru_utime) + tv(&ru.ru_stime)
}

fn locate_compile_binary() -> PathBuf {
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

// ── SPIR-V: VS — in (vec3 pos @0, vec2 uv @1) -> gl_Position + uv varying @0.
fn build_uv_vs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::dr::Operand;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32t = b.type_float(32, None);
    let i32t = b.type_int(32, 1);
    let vec2 = b.type_vector(f32t, 2);
    let vec3 = b.type_vector(f32t, 3);
    let vec4 = b.type_vector(f32t, 4);
    let void_fn = b.type_function(void, vec![]);
    let per_vertex = b.type_struct(vec![vec4]);
    b.member_decorate(per_vertex, 0, Decoration::BuiltIn,
        vec![Operand::BuiltIn(BuiltIn::Position)]);
    b.member_decorate(per_vertex, 0, Decoration::Offset, vec![Operand::LiteralBit32(0)]);
    b.decorate(per_vertex, Decoration::Block, vec![]);
    let ptr_pv = b.type_pointer(None, StorageClass::Output, per_vertex);
    let ptr_out_v4 = b.type_pointer(None, StorageClass::Output, vec4);
    let ptr_out_v2 = b.type_pointer(None, StorageClass::Output, vec2);
    let ptr_in_v3 = b.type_pointer(None, StorageClass::Input, vec3);
    let ptr_in_v2 = b.type_pointer(None, StorageClass::Input, vec2);
    let in_pos = b.variable(ptr_in_v3, None, StorageClass::Input, None);
    b.decorate(in_pos, Decoration::Location, vec![Operand::LiteralBit32(0)]);
    let in_uv = b.variable(ptr_in_v2, None, StorageClass::Input, None);
    b.decorate(in_uv, Decoration::Location, vec![Operand::LiteralBit32(1)]);
    let pv_var = b.variable(ptr_pv, None, StorageClass::Output, None);
    let out_uv = b.variable(ptr_out_v2, None, StorageClass::Output, None);
    b.decorate(out_uv, Decoration::Location, vec![Operand::LiteralBit32(0)]);
    let c_zero = b.constant_bit32(i32t, 0u32);
    let c_one_f = b.constant_bit32(f32t, 1.0f32.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let pos = b.load(vec3, None, in_pos, None, vec![]).unwrap();
    let x = b.composite_extract(f32t, None, pos, vec![0]).unwrap();
    let y = b.composite_extract(f32t, None, pos, vec![1]).unwrap();
    let z = b.composite_extract(f32t, None, pos, vec![2]).unwrap();
    let pos4 = b.composite_construct(vec4, None, vec![x, y, z, c_one_f]).unwrap();
    let dst = b.access_chain(ptr_out_v4, None, pv_var, vec![c_zero]).unwrap();
    b.store(dst, pos4, None, vec![]).unwrap();
    let uv = b.load(vec2, None, in_uv, None, vec![]).unwrap();
    b.store(out_uv, uv, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Vertex, main, "main",
        vec![in_pos, in_uv, pv_var, out_uv]);
    let words: Vec<u32> = b.module().assemble();
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

// ── SPIR-V: FS — sample combined image sampler @(set 0, binding 0) at uv @0.
fn build_textured_fs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::dr::Operand;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, Dim, ExecutionMode, ExecutionModel,
        FunctionControl, ImageFormat, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32t = b.type_float(32, None);
    let vec2 = b.type_vector(f32t, 2);
    let vec4 = b.type_vector(f32t, 4);
    let void_fn = b.type_function(void, vec![]);
    let image = b.type_image(f32t, Dim::Dim2D, 0, 0, 0, 1, ImageFormat::Unknown, None);
    let sampled = b.type_sampled_image(image);
    let ptr_uc = b.type_pointer(None, StorageClass::UniformConstant, sampled);
    let ptr_in_v2 = b.type_pointer(None, StorageClass::Input, vec2);
    let ptr_out = b.type_pointer(None, StorageClass::Output, vec4);
    let tex = b.variable(ptr_uc, None, StorageClass::UniformConstant, None);
    b.decorate(tex, Decoration::DescriptorSet, vec![Operand::LiteralBit32(0)]);
    b.decorate(tex, Decoration::Binding, vec![Operand::LiteralBit32(0)]);
    let in_uv = b.variable(ptr_in_v2, None, StorageClass::Input, None);
    b.decorate(in_uv, Decoration::Location, vec![Operand::LiteralBit32(0)]);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location, vec![Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let uv = b.load(vec2, None, in_uv, None, vec![]).unwrap();
    let s = b.load(sampled, None, tex, None, vec![]).unwrap();
    let px = b.image_sample_implicit_lod(vec4, None, s, uv, None, vec![]).unwrap();
    b.store(out, px, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![tex, in_uv, out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = b.module().assemble();
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

#[derive(Clone, Copy)]
struct RectPx { x: f32, y: f32, w: f32, h: f32 }

fn glyph_layout() -> Vec<RectPx> {
    let (adv, line) = (9.0f32, 16.0f32);
    let (cols, rows) = (80, 40);
    let mut glyphs = Vec::new();
    for r in 0..rows {
        for c in 0..cols {
            glyphs.push(RectPx {
                x: 30.0 + c as f32 * adv,
                y: 30.0 + r as f32 * line,
                w: GW as f32, h: GH as f32,
            });
        }
    }
    glyphs
}

/// A GW×GH RGBA glyph sprite: text colour with a spatially-varying alpha
/// (so the sample is a real per-pixel fetch, not a constant).
fn build_atlas() -> Vec<u8> {
    let mut px = vec![0u8; (GW * GH * 4) as usize];
    for y in 0..GH {
        for x in 0..GW {
            let i = ((y * GW + x) * 4) as usize;
            // A simple diagonal coverage ramp + border — varies per texel.
            let cov = (((x * 31 + y * 17) % 256) as u32).min(255) as u8;
            px[i] = 230; px[i + 1] = 230; px[i + 2] = 235; px[i + 3] = cov;
        }
    }
    px
}

fn ndc(px: f32, py: f32) -> (f32, f32) {
    (px / W as f32 * 2.0 - 1.0, py / H as f32 * 2.0 - 1.0)
}

/// Two triangles for a glyph quad, each vertex = [x, y, z, u, v] (20 B).
fn glyph_tris(r: &RectPx) -> [[u8; 60]; 2] {
    let (x0, y0) = ndc(r.x, r.y);
    let (x1, y1) = ndc(r.x + r.w, r.y + r.h);
    // pos in NDC, uv 0..1 across the atlas.
    let v = |x: f32, y: f32, u: f32, w: f32| -> [f32; 5] { [x, y, 0.0, u, w] };
    let tri = |a: [f32; 5], b: [f32; 5], c: [f32; 5]| -> [u8; 60] {
        let mut out = [0u8; 60];
        for (i, p) in [a, b, c].iter().enumerate() {
            for (j, f) in p.iter().enumerate() {
                out[i * 20 + j * 4..i * 20 + j * 4 + 4].copy_from_slice(&f.to_le_bytes());
            }
        }
        out
    };
    [
        tri(v(x0, y0, 0.0, 0.0), v(x1, y0, 1.0, 0.0), v(x1, y1, 1.0, 1.0)),
        tri(v(x0, y0, 0.0, 0.0), v(x1, y1, 1.0, 1.0), v(x0, y1, 0.0, 1.0)),
    ]
}

fn main() {
    let glyphs = glyph_layout();
    let atlas = build_atlas();
    let iters = 200u32;
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    println!("resolution {W}x{H}  cores={cores}  glyphs={}  atlas={GW}x{GH}", glyphs.len());

    // ── tiny-skia: draw_pixmap the atlas sprite per glyph (SrcOver). ──
    let ts_atlas = {
        let mut pm = tiny_skia::Pixmap::new(GW, GH).unwrap();
        pm.data_mut().copy_from_slice(&atlas);
        pm
    };
    let mut ts = tiny_skia::Pixmap::new(W, H).unwrap();
    let paint = tiny_skia::PixmapPaint {
        opacity: 1.0,
        blend_mode: tiny_skia::BlendMode::SourceOver,
        quality: tiny_skia::FilterQuality::Nearest,
    };
    let render_ts = |ts: &mut tiny_skia::Pixmap| {
        ts.fill(tiny_skia::Color::TRANSPARENT);
        for g in &glyphs {
            ts.draw_pixmap(g.x as i32, g.y as i32, ts_atlas.as_ref(), &paint,
                tiny_skia::Transform::identity(), None);
        }
    };
    for _ in 0..3 { render_ts(&mut ts); }
    let (t0, c0) = (Instant::now(), cpu_ms());
    for _ in 0..iters { render_ts(&mut ts); }
    let ts_ms = t0.elapsed().as_secs_f64() * 1000.0 / iters as f64;
    let ts_cpu = (cpu_ms() - c0) / iters as f64;

    // ── tier-2: textured quads through submit_frame. ──
    let cache = std::env::temp_dir().join(format!("atrium-texbench-{}", std::process::id()));
    std::fs::create_dir_all(&cache).ok();
    let config = LoaderConfig {
        cache_root: cache.clone(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    };
    let registry = Arc::new(Tier2Registry::new(config));
    let vs = registry.register(&build_uv_vs()).expect("vs");
    let fs = registry.register(&build_textured_fs()).expect("fs");

    let backend = Tier2Backend::new(registry.clone());
    let img = ResourceId::new(IdNamespace::IcdRuntime, 0x1000);
    backend.image_created(img, W, H);
    // Atlas texture + sampler.
    let atlas_id = ResourceId::new(IdNamespace::IcdRuntime, 0x1100);
    backend.image_created(atlas_id, GW, GH);
    backend.image_write_pixels(atlas_id, 0, &atlas).expect("atlas upload");
    let samp_id = ResourceId::new(IdNamespace::IcdRuntime, 0x1200);
    // nearest filter, clamp-to-edge (address mode 2).
    backend.sampler_created(samp_id, 0, 0, 0, [2, 2, 2], 0.0, 0.0, 0.0, 0, 0);

    let layout = VertexInputState {
        bindings: vec![VertexBindingDesc { binding: 0, stride: 20, per_instance: false }],
        attributes: vec![
            VertexAttributeDesc { location: 0, binding: 0,
                format: VertexFormat::R32g32b32Sfloat, offset: 0 },
            VertexAttributeDesc { location: 1, binding: 0,
                format: VertexFormat::R32g32Sfloat, offset: 12 },
        ],
    };
    let srcover = Tier2BlendState {
        enable: true,
        color_src: Tier2BlendFactor::SrcAlpha, color_dst: Tier2BlendFactor::OneMinusSrcAlpha,
        alpha_src: Tier2BlendFactor::One, alpha_dst: Tier2BlendFactor::OneMinusSrcAlpha,
        color_op: Tier2BlendOp::Add, alpha_op: Tier2BlendOp::Add,
        write_mask_rgba: [true; 4],
    };
    let pid = ResourceId::new(IdNamespace::IcdRuntime, 0x2000);
    backend.bind_pipeline_vs(pid, vs);
    backend.bind_pipeline(pid, fs);
    backend.bind_layout(pid, layout);
    backend.bind_raster_state(pid, None, Some(srcover), &[], None,
        Tier2PrimitiveTopology::TriangleList, None, false);

    // Pack all glyph triangles into one vertex buffer.
    let mut vbuf: Vec<u8> = Vec::new();
    for g in &glyphs { for t in glyph_tris(g) { vbuf.extend_from_slice(&t); } }
    let vb = ResourceId::new(IdNamespace::IcdRuntime, 0x3000);
    backend.buffer_created(vb, vbuf.len() as u64);
    backend.buffer_write_bytes(vb, 0, &vbuf).unwrap();

    // BindDescriptors body: { set u32, count u32 } + 36-byte write
    // { binding, type, buffer_id, image_id, sampler_id, offset u64, range u64 }.
    let mut desc = Vec::new();
    desc.extend_from_slice(&0u32.to_le_bytes());          // set_index
    desc.extend_from_slice(&1u32.to_le_bytes());          // write_count
    desc.extend_from_slice(&0u32.to_le_bytes());          // binding 0
    desc.extend_from_slice(&1u32.to_le_bytes());          // COMBINED_IMAGE_SAMPLER
    desc.extend_from_slice(&0u32.to_le_bytes());          // buffer_id
    desc.extend_from_slice(&atlas_id.raw().to_le_bytes()); // image_id
    desc.extend_from_slice(&samp_id.raw().to_le_bytes());  // sampler_id
    desc.extend_from_slice(&0u64.to_le_bytes());          // offset
    desc.extend_from_slice(&0u64.to_le_bytes());          // range

    let mut frame = FrameBuilder::new(1 << 16);
    let mut begin = [0u8; 12];
    begin[..4].copy_from_slice(&img.raw().to_le_bytes());
    begin[4..8].copy_from_slice(&[0u8, 0, 0, 255]);
    frame.push(FrameOp::BeginRenderPass, &begin).unwrap();
    frame.push_set_viewport(SetViewportCmd {
        x: 0.0, y: 0.0, width: W as f32, height: H as f32, min_depth: 0.0, max_depth: 1.0,
    }).unwrap();
    frame.push(FrameOp::BindPipeline, &pid.raw().to_le_bytes()).unwrap();
    frame.push(FrameOp::BindDescriptors, &desc).unwrap();
    frame.push_bind_vertex_buf(BindVertexBufCmd { binding: 0, buffer_id: vb.raw(), offset: 0 }).unwrap();
    frame.push_draw(DrawCmd {
        vertex_count: (vbuf.len() / 20) as u32, instance_count: 1,
        first_vertex: 0, first_instance: 0,
    }).unwrap();
    frame.push(FrameOp::EndRenderPass, &[]).unwrap();
    let frame_bytes = frame.as_bytes().to_vec();
    let fence = ResourceId::new(IdNamespace::IcdRuntime, 0x4000);

    for _ in 0..3 { backend.submit_frame(fence, 1, &frame_bytes); }
    let drew = backend.read_image_pixels(img).map(|px| {
        // Sum over the glyph band to confirm pixels actually landed.
        let mut s = 0u64;
        for v in px.iter().step_by(997) { s += *v as u64; }
        s
    }).unwrap_or(0);
    let (t1, c1) = (Instant::now(), cpu_ms());
    for i in 0..iters { backend.submit_frame(fence, i as u64 + 2, &frame_bytes); }
    let t2_ms = t1.elapsed().as_secs_f64() * 1000.0 / iters as f64;
    let t2_cpu = (cpu_ms() - c1) / iters as f64;

    std::fs::remove_dir_all(&cache).ok();

    println!();
    println!("  tiny-skia (draw_pixmap) : {ts_ms:8.3} ms wall   {ts_cpu:7.2} core-ms");
    println!("  tier-2    (textured)    : {t2_ms:8.3} ms wall   {t2_cpu:7.2} core-ms   [sig={drew}]");
    println!();
    let cmp = |label: &str, a: f64, t: f64| {
        if t < a { println!("  => tier-2 {label} {:.2}x FASTER than tiny-skia", a / t); }
        else     { println!("  => tier-2 {label} {:.2}x slower than tiny-skia", t / a); }
    };
    cmp("WALL (latency)", ts_ms, t2_ms);
    cmp("ENERGY (core-ms)", ts_cpu, t2_cpu);
}
