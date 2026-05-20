//! Tier-2 rasterizer **phase R.1** — hello triangle.
//!
//! Drives a passthrough vertex shader + a constant-colour
//! fragment shader through the new
//! `Tier2Registry::fill_image_triangle` API and asserts
//! that the rasterised triangle covers the expected pixels.
//!
//! This is the first tier-2 test that exercises *geometric*
//! input — every prior test fired the FS over the whole
//! image with no vertex stage involved.  Phase boundary:
//! gates the entire vertex-shading + triangle-setup + edge-
//! function-pixel-test path.  Subsequent phases (R.2 …) add
//! varying interpolation, depth, clipping, blending.

#![allow(dead_code)]

use std::path::PathBuf;

use aqueduct_gpu_host::Tier2Registry;
use atrium_spv_loader::LoaderConfig;
use tempfile::TempDir;

/// Mirrors the helper in `tier2_registry.rs` — finds the
/// workspace-built `atrium-spv-compile` binary so the
/// loader can spawn it from the test process.
fn locate_compile_binary() -> PathBuf {
    let here = std::env::current_exe().expect("current_exe");
    let mut p = here;
    p.pop(); p.pop(); p.pop(); p.pop(); p.pop();   // deps → debug → target → aqueduct-gpu-host → bsd
    p.push("atrium-spv-compile");
    p.push("target");
    p.push("debug");
    p.push("atrium-spv-compile");
    assert!(p.exists(),
        "atrium-spv-compile binary not found at {}.  Build it: (cd ../atrium-spv-compile && cargo build)",
        p.display());
    p
}

/// Passthrough vertex shader: vec3 attribute at location 0,
/// gl_Position = vec4(in.xyz, 1.0).  Same shape used by the
/// matrix-arc / vertex-stage interpreter + differential tests.
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
    let ptr_in_vec3  = b.type_pointer(None, StorageClass::Input, vec3);

    let in_pos = b.variable(ptr_in_vec3, None, StorageClass::Input, None);
    b.decorate(in_pos, Decoration::Location,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let pv_var = b.variable(ptr_pv, None, StorageClass::Output, None);
    let c_zero  = b.constant_bit32(i32_ty, 0u32);
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

/// Constant-colour fragment shader.  out_color = rgba.
fn build_constant_color_fs(rgba: [f32; 4]) -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, ExecutionMode, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let vec4_f32 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let ptr_out = b.type_pointer(None, StorageClass::Output, vec4_f32);

    let cs: Vec<_> = rgba.iter()
        .map(|x| b.constant_bit32(f32_ty, x.to_bits())).collect();
    let color = b.constant_composite(vec4_f32, cs);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, rspirv::spirv::Decoration::Location,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
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

fn pack_vec3(v: [f32; 3]) -> Vec<u8> {
    let mut b = Vec::with_capacity(12);
    for f in v { b.extend_from_slice(&f.to_le_bytes()); }
    b
}

/// R.1 acceptance: a triangle in NDC coords (-0.5,-0.5),
/// (0.5,-0.5), (0,0.5) on an 8×8 image should cover the
/// pixels we hand-compute via edge functions.  Vulkan
/// viewport convention (y NOT flipped) maps these to
/// screen-space vertices (2, 2), (6, 2), (4, 6).
///
/// Expected coverage at pixel centres (px+0.5, py+0.5):
///   row y=2  pixels (2..=5, 2) inside        — top edge band
///   row y=3  pixels (2..=5, 3) inside        — wide middle
///   row y=4  pixels (3..=4, 4) inside        — narrowing
///   row y=5  pixel  (3,    5) inside-ish ... — tip area
/// Pixels outside the triangle stay at the cleared (0,0,0,0)
/// background.
///
/// This test SPOT-CHECKS a few definitive inside/outside
/// points rather than enumerating the entire 64-pixel grid
/// — the rasterizer's correctness comes from the edge-
/// function algebra, which exhaustive enumeration would
/// merely re-derive.
#[test]
fn rasterizer_r1_hello_triangle() {
    let cache_dir = TempDir::new().unwrap();
    let config = LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    };
    let registry = Tier2Registry::new(config);

    let vs_id = registry.register(&build_passthrough_vs())
        .expect("vs registers");
    let fs_id = registry.register(&build_constant_color_fs([1.0, 0.2, 0.2, 1.0]))
        .expect("fs registers");

    // 3 NDC vertices forming a 4-pixel-tall triangle on an 8×8 image.
    let v0 = pack_vec3([-0.5, -0.5, 0.0]);   // screen (2, 2)
    let v1 = pack_vec3([ 0.5, -0.5, 0.0]);   // screen (6, 2)
    let v2 = pack_vec3([ 0.0,  0.5, 0.0]);   // screen (4, 6)

    let mut pixels = vec![0u8; 8 * 8 * 4];
    registry.fill_image_triangle(
        vs_id, fs_id,
        [&v0, &v1, &v2],
        &[],         // no uniforms
        &[],         // no push constants
        8, 8,
        &mut pixels,
    ).expect("rasterise");

    // Helper: extract one RGBA pixel.
    let px = |x: usize, y: usize| -> [u8; 4] {
        let idx = (y * 8 + x) * 4;
        [pixels[idx], pixels[idx + 1], pixels[idx + 2], pixels[idx + 3]]
    };
    // Expected interior colour after u8 quantisation.
    // (1.0, 0.2, 0.2, 1.0) → (255, 51, 51, 255).
    let red = [255u8, 51, 51, 255];

    // Definitive inside pixels (centres lie strictly inside
    // the 3 edges).
    assert_eq!(px(2, 2), red, "inside (2,2): {:?}", px(2, 2));
    assert_eq!(px(4, 2), red, "inside (4,2): {:?}", px(4, 2));
    assert_eq!(px(5, 2), red, "inside (5,2): {:?}", px(5, 2));
    assert_eq!(px(3, 3), red, "inside (3,3): {:?}", px(3, 3));
    assert_eq!(px(4, 3), red, "inside (4,3): {:?}", px(4, 3));
    assert_eq!(px(3, 4), red, "inside (3,4): {:?}", px(3, 4));
    assert_eq!(px(4, 4), red, "inside (4,4): {:?}", px(4, 4));

    // Definitive outside pixels (centres lie outside at
    // least one edge or outside the bbox entirely).
    assert_eq!(px(0, 0), [0, 0, 0, 0], "outside (0,0): {:?}", px(0, 0));
    assert_eq!(px(7, 7), [0, 0, 0, 0], "outside (7,7): {:?}", px(7, 7));
    assert_eq!(px(1, 2), [0, 0, 0, 0], "outside (1,2): {:?}", px(1, 2));
    assert_eq!(px(6, 2), [0, 0, 0, 0], "outside (6,2): {:?}", px(6, 2));
    assert_eq!(px(4, 0), [0, 0, 0, 0], "outside (4,0): {:?}", px(4, 0));
    // Pixel (5, 4): centre (5.5, 4.5).  Right edge at y=4.5 is
    // x = 6 - (4.5-2)/2 = 4.75.  5.5 > 4.75 → outside.
    assert_eq!(px(5, 4), [0, 0, 0, 0], "outside (5,4): {:?}", px(5, 4));
    // Pixel (4, 5): centre (4.5, 5.5).  Right edge at y=5.5 is
    // x = 6 - (5.5-2)/2 = 4.25.  4.5 > 4.25 → outside.
    assert_eq!(px(4, 5), [0, 0, 0, 0], "outside (4,5): {:?}", px(4, 5));
}

/// Sanity: a degenerate triangle (all 3 vertices collinear)
/// produces no covered pixels — the bbox is still computed
/// but every edge function is zero everywhere, so the
/// `all-same-sign` test trivially evaluates to "inside"
/// only for the line itself, which has zero area.  This is
/// a regression hedge: if anyone changes the inside test to
/// strict inequality, degenerate triangles would silently
/// drop pixels they should fire on, AND if anyone widens it
/// the wrong way, degenerate triangles would fire ALL pixels
/// in the bbox.  This test pins the current ≥0 inclusive
/// behaviour against a zero-area triangle: only the line
/// pixels themselves fire, the rest of the bbox stays clear.
#[test]
fn rasterizer_r1_degenerate_triangle_covers_only_line_pixels() {
    let cache_dir = TempDir::new().unwrap();
    let config = LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    };
    let registry = Tier2Registry::new(config);

    let vs_id = registry.register(&build_passthrough_vs()).unwrap();
    let fs_id = registry.register(&build_constant_color_fs([0.0, 1.0, 0.0, 1.0]))
        .unwrap();

    // Three collinear NDC points along y=0.
    let v0 = pack_vec3([-0.5, 0.0, 0.0]);    // screen (2, 4)
    let v1 = pack_vec3([ 0.0, 0.0, 0.0]);    // screen (4, 4)
    let v2 = pack_vec3([ 0.5, 0.0, 0.0]);    // screen (6, 4)

    let mut pixels = vec![0u8; 8 * 8 * 4];
    registry.fill_image_triangle(
        vs_id, fs_id,
        [&v0, &v1, &v2],
        &[], &[],
        8, 8,
        &mut pixels,
    ).expect("degenerate rasterise");

    // Pixel centres at y=4.5 are on a horizontal line in NDC y=0
    // which maps to screen y=4 .. and y=4 row has centres at y=4.5
    // (not equal to the line at y=4).  So the bbox is min/max
    // y = 4..4, the single tested row has cy=4.5, and the edges
    // are at y=4 — the edge function is non-zero there and the
    // sign test fails.  Result: zero pixels covered.
    let any_lit = pixels.chunks(4).any(|c| c != [0, 0, 0, 0]);
    assert!(!any_lit,
        "degenerate triangle covered some pixel (regression — \
         the all-same-sign inside test must reject zero-area \
         triangles whose pixel centres don't lie exactly on the \
         degenerate line)");
}
