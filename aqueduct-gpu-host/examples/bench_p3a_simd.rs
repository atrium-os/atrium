//! P3a measurement: SIMD vs scalar rasterizer fixed-function path.
//!
//! Drives the REAL registry path (`fill_image_triangle` → VS + clip
//! + setup + `rasterize_pass` → `rasterize_stripe`), which routes
//! simple opaque draws to `rasterize_stripe_simd` (P3a) unless
//! `ATRIUM_TIER2_NOSIMD=1`.  A screen-covering opaque triangle with
//! a constant-colour FS isolates the coverage cost (n=0 varyings).
//! Run twice to compare:
//!
//!   cargo run -p aqueduct-gpu-host --example bench_p3a_simd --release
//!   ATRIUM_TIER2_NOSIMD=1 cargo run -p aqueduct-gpu-host --example bench_p3a_simd --release

use std::path::PathBuf;
use std::time::Instant;

use aqueduct_gpu_host::Tier2Registry;
use aqueduct_gpu_host::tier2_registry::{DrawTriangle, CompareOp};
use atrium_spv_loader::LoaderConfig;

const W: u32 = 3840;
const H: u32 = 2160;

fn locate_compile_binary() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); p.push("atrium-spv-compile"); p.push("target"); p.push("debug");
    p.push("atrium-spv-compile");
    assert!(p.exists(), "build atrium-spv-compile first ({})", p.display());
    p
}

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
    let mut out = Vec::new();
    for w in words { out.extend_from_slice(&w.to_le_bytes()); }
    out
}

fn build_const_fs(rgba: [f32; 4]) -> Vec<u8> {
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
    let mut out = Vec::new();
    for w in words { out.extend_from_slice(&w.to_le_bytes()); }
    out
}

fn main() {
    let simd = std::env::var("ATRIUM_TIER2_NOSIMD").map(|v| v == "1").unwrap_or(false);
    let label = if simd { "SCALAR (NOSIMD=1)" } else { "SIMD (default)" };
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);

    let cache = std::env::temp_dir().join(format!("p3a_{}", std::process::id()));
    std::fs::create_dir_all(&cache).ok();
    let reg = Tier2Registry::new(LoaderConfig {
        cache_root: cache.clone(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    });
    let vs = reg.register(&build_passthrough_vs()).unwrap();
    let fs = reg.register(&build_const_fs([0.27, 0.29, 0.32, 1.0])).unwrap();

    // One screen-covering triangle (clip-space NDC), opaque, depth
    // disabled (compare Always) so it hits the P3a SIMD gate.
    let verts: [[f32; 3]; 3] = [[-1.0, -1.0, 0.0], [3.0, -1.0, 0.0], [-1.0, 3.0, 0.0]];
    let mut vbytes = Vec::new();
    for v in &verts { for f in v { vbytes.extend_from_slice(&f.to_le_bytes()); } }
    let v0 = &vbytes[0..12];
    let v1 = &vbytes[12..24];
    let v2 = &vbytes[24..36];
    let draw = DrawTriangle {
        vertex_attrs: [v0, v1, v2],
        varying_f32_count: 0,
        sample_count: 1,
        depth_compare_op: CompareOp::Always,
        depth_write: false,
        ..Default::default()
    };

    let mut fb = vec![0u8; (W * H * 4) as usize];
    let mut run = || {
        reg.fill_image_triangle(vs, fs, &draw, W, H, &mut fb, None, None, &mut [])
            .unwrap();
    };
    run(); // warm
    let iters = 40u32;
    let t = Instant::now();
    for _ in 0..iters { run(); }
    let ms = t.elapsed().as_secs_f64() * 1000.0 / iters as f64;

    println!("P3a  4K {W}x{H}  cores={cores}  path={label}");
    println!("  screen-covering opaque triangle (const FS): {ms:.3} ms/frame");
    // sanity: interior pixel painted
    let i = ((H as usize / 2) * W as usize + W as usize / 2) * 4;
    println!("  center px = [{},{},{},{}]", fb[i], fb[i+1], fb[i+2], fb[i+3]);
    std::fs::remove_dir_all(&cache).ok();
}
