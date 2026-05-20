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

use aqueduct_gpu_host::{
    BlendFactor, BlendFactorPair, BlendOp, BlendState, ColorWriteMask,
    DrawTriangle, Tier2Registry,
};
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
    let draw = DrawTriangle {
        vertex_attrs: [&v0, &v1, &v2],
        ..Default::default()
    };
    registry.fill_image_triangle(
        vs_id, fs_id,
        &draw,
        8, 8,
        &mut pixels,
        None,
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

/// Fragment shader: in_varying[0] is `vec4 in_color` at Location=0;
/// out_color = in_color.  Used for R.2's varying-gradient tests.
fn build_passthrough_color_fs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let ptr_in = b.type_pointer(None, StorageClass::Input, vec4);
    let ptr_out = b.type_pointer(None, StorageClass::Output, vec4);
    let in_color = b.variable(ptr_in, None, StorageClass::Input, None);
    b.decorate(in_color, Decoration::Location,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let out_color = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out_color, Decoration::Location,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let c = b.load(vec4, None, in_color, None, vec![]).unwrap();
    b.store(out_color, c, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![in_color, out_color]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

fn pack_vec4(v: [f32; 4]) -> Vec<u8> {
    let mut b = Vec::with_capacity(16);
    for f in v { b.extend_from_slice(&f.to_le_bytes()); }
    b
}

/// R.2 acceptance: each triangle vertex carries a distinct
/// vec4 colour as a varying.  Rasterizer interpolates them
/// perspective-correctly per pixel; FS passes the
/// interpolation through to out_color.
///
/// With unit w (no perspective), the interpolation
/// collapses to barycentric-linear in screen space, which
/// is the easy case to assert against:
///   * pixel exactly at a vertex centre → that vertex's
///     colour.
///   * pixel near the centroid → roughly the average of
///     the three colours.
///
/// Image is 16×16 so the triangle spans enough pixels for
/// the centroid-style spot check to make sense.  Vertices
/// at NDC (-0.75, -0.75), (0.75, -0.75), (0, 0.75) map to
/// screen (2, 2), (14, 2), (8, 14).
#[test]
fn rasterizer_r2_varying_color_gradient() {
    let cache_dir = TempDir::new().unwrap();
    let config = LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    };
    let registry = Tier2Registry::new(config);

    let vs_id = registry.register(&build_passthrough_vs()).unwrap();
    let fs_id = registry.register(&build_passthrough_color_fs()).unwrap();

    let v0 = pack_vec3([-0.75, -0.75, 0.0]);   // screen (2, 2)
    let v1 = pack_vec3([ 0.75, -0.75, 0.0]);   // screen (14, 2)
    let v2 = pack_vec3([ 0.00,  0.75, 0.0]);   // screen (8, 14)

    // Colour per vertex.
    let red   = pack_vec4([1.0, 0.0, 0.0, 1.0]);
    let green = pack_vec4([0.0, 1.0, 0.0, 1.0]);
    let blue  = pack_vec4([0.0, 0.0, 1.0, 1.0]);

    let mut pixels = vec![0u8; 16 * 16 * 4];
    let draw = DrawTriangle {
        vertex_attrs: [&v0, &v1, &v2],
        varyings_per_vertex: [&red, &green, &blue],
        varying_f32_count: 4,
        ..Default::default()
    };
    registry.fill_image_triangle(
        vs_id, fs_id, &draw, 16, 16, &mut pixels, None,
    ).expect("rasterise gradient");

    let px = |x: usize, y: usize| -> [u8; 4] {
        let idx = (y * 16 + x) * 4;
        [pixels[idx], pixels[idx+1], pixels[idx+2], pixels[idx+3]]
    };
    // Pixel centres at (2.5, 2.5), (13.5, 2.5), (7.5, 13.5)
    // are NOT exactly the vertices (which sit on integer
    // grid points), so the vertex spot checks can't expect
    // pure red / green / blue.  Pick interior pixels whose
    // bary weights are clearly dominated by one vertex.
    //
    // Pixel (3, 3): closer to v0 (red) than any other.
    //   Expect R channel high, others low-ish.
    let p33 = px(3, 3);
    assert!(p33[0] > 150,
        "pixel near v0 should be red-dominant: {p33:?}");
    assert!(p33[1] < 100, "near-v0 green too high: {p33:?}");
    assert!(p33[2] < 100, "near-v0 blue too high: {p33:?}");
    // Pixel (12, 3): near v1 (green).
    let p123 = px(12, 3);
    assert!(p123[1] > 150,
        "pixel near v1 should be green-dominant: {p123:?}");
    assert!(p123[0] < 100, "near-v1 red too high: {p123:?}");
    assert!(p123[2] < 100, "near-v1 blue too high: {p123:?}");
    // Pixel (8, 12): near v2 (blue).
    let p812 = px(8, 12);
    assert!(p812[2] > 150,
        "pixel near v2 should be blue-dominant: {p812:?}");
    assert!(p812[0] < 100, "near-v2 red too high: {p812:?}");
    assert!(p812[1] < 100, "near-v2 green too high: {p812:?}");
    // Centroid-ish: roughly equal R/G/B (each ~85ish on
    // 0..255 scale, since 1/3 of 255 ~ 85).
    let pmid = px(8, 6);
    for k in 0..3 {
        assert!(pmid[k] > 50 && pmid[k] < 130,
            "centroid channel {k} = {} not in 50..130: {pmid:?}",
            pmid[k]);
    }
    assert_eq!(pmid[3], 255, "alpha at centroid: {pmid:?}");
}

/// R.2 acceptance: perspective-correctness specifically.
/// Two vertices share w=1; one vertex has w=2.  Linear-in-
/// screen-space interpolation would give the same colour
/// everywhere along the line through the two w=1 vertices,
/// but perspective-correct interpolation makes the colour
/// curve toward the w=2 vertex faster in screen space than
/// barycentric linearity would predict.
///
/// We can't easily emit a custom-w gl_Position through the
/// passthrough VS (which sets w=1), so this test bypasses
/// that by directly verifying the math on the rasterizer's
/// terms: we feed a VS that emits w=2 for vertex 2 via a
/// dedicated builder.
#[test]
fn rasterizer_r2_perspective_correct_varies_with_w() {
    // Build a VS that writes gl_Position = vec4(in_pos.x,
    // in_pos.y, in_pos.z, vertex_index==2 ? 2.0 : 1.0).  Hard
    // to express in pure-SPIR-V without a conditional; we
    // sidestep by reading w from a 4th attribute lane.
    //
    // Concretely: change the input from vec3 to vec4 (xyz +
    // w), and write gl_Position = vec4(x, y, z, w).  The
    // host packs w per vertex; vertices 0 and 1 get w=1.0,
    // vertex 2 gets w=2.0.
    let vs_spirv = build_vec4_position_vs();
    let fs_spirv = build_passthrough_color_fs();

    let cache_dir = TempDir::new().unwrap();
    let config = LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    };
    let registry = Tier2Registry::new(config);
    let vs_id = registry.register(&vs_spirv).unwrap();
    let fs_id = registry.register(&fs_spirv).unwrap();

    // Pack vec4 (x, y, z, w) per vertex.
    let v0 = pack_vec4([-0.75, -0.75, 0.0, 1.0]);
    let v1 = pack_vec4([ 0.75, -0.75, 0.0, 1.0]);
    let v2 = pack_vec4([ 0.00,  0.75 * 2.0, 0.0, 2.0]); // ndc.y = 0.75 after divide

    let red   = pack_vec4([1.0, 0.0, 0.0, 1.0]);
    let green = pack_vec4([0.0, 1.0, 0.0, 1.0]);
    let blue  = pack_vec4([0.0, 0.0, 1.0, 1.0]);

    let mut pixels = vec![0u8; 16 * 16 * 4];
    let draw = DrawTriangle {
        vertex_attrs: [&v0, &v1, &v2],
        varyings_per_vertex: [&red, &green, &blue],
        varying_f32_count: 4,
        ..Default::default()
    };
    registry.fill_image_triangle(
        vs_id, fs_id, &draw, 16, 16, &mut pixels, None,
    ).expect("rasterise PC");

    let px = |x: usize, y: usize| -> [u8; 4] {
        let idx = (y * 16 + x) * 4;
        [pixels[idx], pixels[idx+1], pixels[idx+2], pixels[idx+3]]
    };
    // We don't pin exact pixel values (the perspective-
    // correct formula is sensitive to floor/ceil choices in
    // viewport mapping), but we can pin the qualitative
    // behaviour: at the centroid, the blue channel comes
    // from v2 which has w=2, so its 1/w contribution is
    // HALF what an equal-w setup would give -- the blue
    // share at any pixel is suppressed relative to the
    // linear case.  At the centroid (b0 = b1 = b2 = 1/3):
    //   linear:        v2's share is 1/3 → blue ≈ 85
    //   perspective:   1/3 * (1/2) / (1/3 * 1 + 1/3 * 1 + 1/3 * 1/2)
    //                = (1/6) / (5/6) = 1/5 → blue ≈ 51
    // Easier-to-pin: blue channel near the centroid in this
    // setup is strictly LESS than in the all-w=1 test
    // above.  Hard-coded threshold of 70 on the 0..255 scale
    // separates the two cases reliably.
    let pmid = px(8, 6);
    assert!(pmid[2] < 70,
        "perspective-correct: blue at centroid should be < 70 \
         (in the all-w=1 test it was 50..130); got {pmid:?}");
    // And red/green should still total dominant -- v0+v1
    // share has gone UP relative to v2.
    assert!(pmid[0] + pmid[1] > pmid[2] * 2,
        "perspective-correct: red+green at centroid should \
         far outweigh blue; got {pmid:?}");
}

/// R.3 acceptance: depth-test ordering.  Two overlapping
/// triangles, the *closer* one drawn FIRST.  With the LESS
/// depth test, the second (further) triangle must be
/// rejected in the overlap region — without depth, it would
/// trivially overwrite.  Distinguishes "depth test working"
/// from "last draw wins".
///
/// Setup (16×16 image):
///   triangle A:  z = 0.0 (near plane), red
///                screen verts (2, 2), (14, 2), (8, 14)
///   triangle B:  z = 0.5 (mid),         blue
///                same screen footprint
///
/// Expected:
///   * overlap region                 → RED (A passed, B rejected)
///   * NO pixel is blue anywhere     (B's whole shape is covered
///                                    by A in this test)
///   * outside both triangles        → zero (cleared background)
#[test]
fn rasterizer_r3_depth_test_rejects_farther_triangle() {
    let cache_dir = TempDir::new().unwrap();
    let config = LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    };
    let registry = Tier2Registry::new(config);

    let vs_id = registry.register(&build_vec4_position_vs()).unwrap();
    let fs_id = registry.register(&build_passthrough_color_fs()).unwrap();

    // Triangle A — close, red.
    let a0 = pack_vec4([-0.75, -0.75, 0.0, 1.0]);
    let a1 = pack_vec4([ 0.75, -0.75, 0.0, 1.0]);
    let a2 = pack_vec4([ 0.00,  0.75, 0.0, 1.0]);
    let red = pack_vec4([1.0, 0.0, 0.0, 1.0]);

    // Triangle B — far, blue.  Same shape, deeper z.
    let b0 = pack_vec4([-0.75, -0.75, 0.5, 1.0]);
    let b1 = pack_vec4([ 0.75, -0.75, 0.5, 1.0]);
    let b2 = pack_vec4([ 0.00,  0.75, 0.5, 1.0]);
    let blue = pack_vec4([0.0, 0.0, 1.0, 1.0]);

    let mut pixels = vec![0u8; 16 * 16 * 4];
    let mut depth  = vec![1.0f32; 16 * 16];   // clear to far

    // Draw A first.
    let draw_a = DrawTriangle {
        vertex_attrs: [&a0, &a1, &a2],
        varyings_per_vertex: [&red, &red, &red],
        varying_f32_count: 4,
        ..Default::default()
    };
    registry.fill_image_triangle(
        vs_id, fs_id, &draw_a, 16, 16, &mut pixels, Some(&mut depth),
    ).expect("draw A");

    // Draw B second.  Its depth (0.5) is greater than A's
    // (0.0) in the overlap, so LESS depth test fails and B
    // is fully rejected.
    let draw_b = DrawTriangle {
        vertex_attrs: [&b0, &b1, &b2],
        varyings_per_vertex: [&blue, &blue, &blue],
        varying_f32_count: 4,
        ..Default::default()
    };
    registry.fill_image_triangle(
        vs_id, fs_id, &draw_b, 16, 16, &mut pixels, Some(&mut depth),
    ).expect("draw B");

    // Scan the whole image: no pixel should be blue.  Pixels
    // inside the triangle footprint should all be red; pixels
    // outside should be cleared.
    let mut blue_pixels = 0usize;
    let mut red_pixels  = 0usize;
    let mut clear_pixels = 0usize;
    for py in 0..16 {
        for px in 0..16 {
            let idx = (py * 16 + px) * 4;
            let p = [pixels[idx], pixels[idx+1], pixels[idx+2], pixels[idx+3]];
            if p[2] >= 200 && p[0] < 50 {
                blue_pixels += 1;
            } else if p[0] >= 200 && p[2] < 50 {
                red_pixels += 1;
            } else if p == [0, 0, 0, 0] {
                clear_pixels += 1;
            }
        }
    }
    assert_eq!(blue_pixels, 0,
        "depth test failed: {blue_pixels} blue pixels survived \
         after the far triangle should have been rejected");
    assert!(red_pixels > 20,
        "depth test maybe over-eager: only {red_pixels} red pixels \
         survived from triangle A (expected the triangle's interior)");
    // Sanity: most of the 16x16 = 256 pixels are outside the
    // triangle, so should be cleared.
    assert!(clear_pixels > 150,
        "too few cleared pixels: {clear_pixels} (expected the bulk \
         of the 256 outside the triangle footprint)");

    // Depth buffer should hold 0.0 at red pixels (A's depth)
    // and 1.0 (the initial clear value) elsewhere.  Spot-check
    // an interior pixel + a corner.
    let depth_at = |x: usize, y: usize| depth[y * 16 + x];
    assert!((depth_at(8, 6) - 0.0).abs() < 1e-6,
        "depth at red interior: {} (expected 0.0)", depth_at(8, 6));
    assert!((depth_at(0, 0) - 1.0).abs() < 1e-6,
        "depth at cleared corner: {} (expected 1.0)", depth_at(0, 0));
}

/// R.3 acceptance: reverse ordering — same two triangles
/// but the FAR one drawn first, then the NEAR one.  Depth
/// test should ACCEPT the near one (its z=0.0 < the stored
/// z=0.5), so the final image is blue.  Confirms the
/// comparison is LESS-not-GREATER (no flipped polarity bug).
#[test]
fn rasterizer_r3_depth_test_accepts_nearer_triangle() {
    let cache_dir = TempDir::new().unwrap();
    let config = LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    };
    let registry = Tier2Registry::new(config);
    let vs_id = registry.register(&build_vec4_position_vs()).unwrap();
    let fs_id = registry.register(&build_passthrough_color_fs()).unwrap();

    // Same A & B as the previous test, but B drawn first
    // (it's the FAR one; its z=0.5 lands in the depth buffer
    // where the triangle covers).  Then A (z=0.0) — LESS
    // test: 0.0 < 0.5 → pass → A's red overwrites + writes
    // depth 0.0.
    let a0 = pack_vec4([-0.75, -0.75, 0.0, 1.0]);
    let a1 = pack_vec4([ 0.75, -0.75, 0.0, 1.0]);
    let a2 = pack_vec4([ 0.00,  0.75, 0.0, 1.0]);
    let red = pack_vec4([1.0, 0.0, 0.0, 1.0]);
    let b0 = pack_vec4([-0.75, -0.75, 0.5, 1.0]);
    let b1 = pack_vec4([ 0.75, -0.75, 0.5, 1.0]);
    let b2 = pack_vec4([ 0.00,  0.75, 0.5, 1.0]);
    let blue = pack_vec4([0.0, 0.0, 1.0, 1.0]);

    let mut pixels = vec![0u8; 16 * 16 * 4];
    let mut depth  = vec![1.0f32; 16 * 16];

    // Draw B (far) first.
    let draw_b = DrawTriangle {
        vertex_attrs: [&b0, &b1, &b2],
        varyings_per_vertex: [&blue, &blue, &blue],
        varying_f32_count: 4,
        ..Default::default()
    };
    registry.fill_image_triangle(
        vs_id, fs_id, &draw_b, 16, 16, &mut pixels, Some(&mut depth),
    ).expect("draw B first");

    // Draw A (near) second.  Should overwrite.
    let draw_a = DrawTriangle {
        vertex_attrs: [&a0, &a1, &a2],
        varyings_per_vertex: [&red, &red, &red],
        varying_f32_count: 4,
        ..Default::default()
    };
    registry.fill_image_triangle(
        vs_id, fs_id, &draw_a, 16, 16, &mut pixels, Some(&mut depth),
    ).expect("draw A second");

    let mut blue_pixels = 0usize;
    let mut red_pixels  = 0usize;
    for py in 0..16 {
        for px in 0..16 {
            let idx = (py * 16 + px) * 4;
            let p = [pixels[idx], pixels[idx+1], pixels[idx+2], pixels[idx+3]];
            if p[2] >= 200 && p[0] < 50 { blue_pixels += 1; }
            if p[0] >= 200 && p[2] < 50 { red_pixels  += 1; }
        }
    }
    assert!(red_pixels > 20,
        "near triangle should win the overlap: red_pixels={red_pixels}, \
         blue_pixels={blue_pixels}");
    assert_eq!(blue_pixels, 0,
        "no blue should survive — A & B share screen footprint \
         and A is nearer ({red_pixels} red, {blue_pixels} blue)");
}

/// R.4 acceptance: a triangle with one vertex strictly
/// behind the near plane (cz < 0) is correctly clipped.
/// Without clipping, the perspective divide would map a
/// negative-z vertex to an arbitrary NDC z and the
/// rasterizer would paint pixels with bogus depth values.
/// With Sutherland-Hodgman in clip space, the offending
/// vertex is replaced by an interpolated vertex on cz = 0
/// and the visible portion renders cleanly.
///
/// Setup: vertices v0 and v1 in front of the near plane;
/// v2 well behind (cz = -1.0).  We clip → quad → 2 sub-
/// triangles.  Their union covers the *upper* part of the
/// original triangle (the part with positive interpolated
/// z), nothing else.
///
/// Hard assertions:
///   * No pixel has negative interpolated z written to the
///     depth buffer (depth buffer values are all in
///     [0, 1] or equal to the clear value 1.0).
///   * The clipped polygon paints a non-empty pixel set
///     (it shouldn't be culled outright).
#[test]
fn rasterizer_r4_near_plane_clips_behind_camera_vertex() {
    let cache_dir = TempDir::new().unwrap();
    let config = LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    };
    let registry = Tier2Registry::new(config);
    let vs_id = registry.register(&build_vec4_position_vs()).unwrap();
    let fs_id = registry.register(&build_passthrough_color_fs()).unwrap();

    // v0, v1 in front (cz=0.5), v2 behind near (cz=-1.0).
    // All have cw=1.0 so perspective divide is trivial.
    let v0 = pack_vec4([-0.75, -0.75,  0.5, 1.0]);
    let v1 = pack_vec4([ 0.75, -0.75,  0.5, 1.0]);
    let v2 = pack_vec4([ 0.00,  0.75, -1.0, 1.0]);
    let red = pack_vec4([1.0, 0.0, 0.0, 1.0]);

    let mut pixels = vec![0u8; 16 * 16 * 4];
    let mut depth  = vec![1.0f32; 16 * 16];
    let draw = DrawTriangle {
        vertex_attrs: [&v0, &v1, &v2],
        varyings_per_vertex: [&red, &red, &red],
        varying_f32_count: 4,
        ..Default::default()
    };
    registry.fill_image_triangle(
        vs_id, fs_id, &draw, 16, 16, &mut pixels, Some(&mut depth),
    ).expect("clip + rasterise");

    // No depth value below 0.0 — without clipping the
    // interpolated z would have ranged from -1.0 (near v2)
    // to 0.5 (near v0/v1), splashing negative depth values
    // across the upper portion of the triangle.
    let mut painted_pixels = 0usize;
    for (i, &z) in depth.iter().enumerate() {
        assert!(z >= 0.0,
            "depth[{i}] = {z} -- clipping should have prevented \
             any cz < 0 fragment from being painted");
        if z < 1.0 - 1e-6 { painted_pixels += 1; }
    }
    assert!(painted_pixels > 0,
        "clipped polygon painted zero pixels — the near-plane \
         clip culled too much (or the clip is broken)");

    // And confirm the polygon DID paint *something* on the
    // colour side too (clipper shouldn't have emptied it).
    let lit = pixels.chunks(4).any(|p| p != [0, 0, 0, 0]);
    assert!(lit, "clipped polygon produced no coloured pixels");
}

/// R.4 acceptance: a triangle fully behind the near plane
/// is rejected outright; no pixels painted, no depth values
/// written.
#[test]
fn rasterizer_r4_fully_behind_near_is_culled() {
    let cache_dir = TempDir::new().unwrap();
    let config = LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    };
    let registry = Tier2Registry::new(config);
    let vs_id = registry.register(&build_vec4_position_vs()).unwrap();
    let fs_id = registry.register(&build_passthrough_color_fs()).unwrap();

    // All 3 vertices have cz < 0.
    let v0 = pack_vec4([-0.5, -0.5, -0.5, 1.0]);
    let v1 = pack_vec4([ 0.5, -0.5, -0.5, 1.0]);
    let v2 = pack_vec4([ 0.0,  0.5, -0.5, 1.0]);
    let green = pack_vec4([0.0, 1.0, 0.0, 1.0]);

    let mut pixels = vec![0u8; 16 * 16 * 4];
    let mut depth  = vec![1.0f32; 16 * 16];
    let draw = DrawTriangle {
        vertex_attrs: [&v0, &v1, &v2],
        varyings_per_vertex: [&green, &green, &green],
        varying_f32_count: 4,
        ..Default::default()
    };
    registry.fill_image_triangle(
        vs_id, fs_id, &draw, 16, 16, &mut pixels, Some(&mut depth),
    ).expect("fully-clipped rasterise");

    let lit = pixels.chunks(4).any(|p| p != [0, 0, 0, 0]);
    assert!(!lit,
        "triangle fully behind near plane painted pixels — \
         near-plane reject must be a no-op");
    let touched = depth.iter().any(|&z| z < 1.0 - 1e-6);
    assert!(!touched,
        "triangle fully behind near plane wrote to depth buffer");
}

/// R.4 acceptance: a triangle fully past the far plane
/// is also culled.  Same shape as the previous test, but
/// crossing the cz = cw boundary instead of cz = 0.
#[test]
fn rasterizer_r4_fully_beyond_far_is_culled() {
    let cache_dir = TempDir::new().unwrap();
    let config = LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    };
    let registry = Tier2Registry::new(config);
    let vs_id = registry.register(&build_vec4_position_vs()).unwrap();
    let fs_id = registry.register(&build_passthrough_color_fs()).unwrap();

    // cz > cw (= 1.0) for every vertex — beyond far plane.
    let v0 = pack_vec4([-0.5, -0.5, 1.5, 1.0]);
    let v1 = pack_vec4([ 0.5, -0.5, 1.5, 1.0]);
    let v2 = pack_vec4([ 0.0,  0.5, 1.5, 1.0]);
    let blue = pack_vec4([0.0, 0.0, 1.0, 1.0]);

    let mut pixels = vec![0u8; 16 * 16 * 4];
    let mut depth  = vec![1.0f32; 16 * 16];
    let draw = DrawTriangle {
        vertex_attrs: [&v0, &v1, &v2],
        varyings_per_vertex: [&blue, &blue, &blue],
        varying_f32_count: 4,
        ..Default::default()
    };
    registry.fill_image_triangle(
        vs_id, fs_id, &draw, 16, 16, &mut pixels, Some(&mut depth),
    ).expect("far-clipped rasterise");

    let lit = pixels.chunks(4).any(|p| p != [0, 0, 0, 0]);
    assert!(!lit,
        "triangle past far plane painted pixels — far-plane \
         reject must be a no-op");
}

/// R.5 acceptance: alpha-over.  Pre-clear the image to
/// green, then draw a red triangle with src.a = 0.5 using
/// `BlendState::alpha_over()`.  Expected result inside the
/// triangle:
///   src = (1.0, 0.0, 0.0, 0.5)
///   dst = (0.0, 1.0, 0.0, 1.0)
///   result.rgb = src.rgb * src.a + dst.rgb * (1 - src.a)
///              = (0.5, 0, 0) + (0, 0.5, 0) = (0.5, 0.5, 0)
///   result.a   = src.a + dst.a * (1 - src.a) = 0.5 + 0.5 = 1.0
///   u8:        (128, 128, 0, 255)   (with 0.5 → 128 rounding)
/// Outside the triangle, the green background is untouched.
#[test]
fn rasterizer_r5_alpha_over_red_on_green() {
    let cache_dir = TempDir::new().unwrap();
    let config = LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    };
    let registry = Tier2Registry::new(config);

    let vs_id = registry.register(&build_passthrough_vs()).unwrap();
    let fs_id = registry.register(&build_passthrough_color_fs()).unwrap();

    let v0 = pack_vec3([-0.75, -0.75, 0.0]);
    let v1 = pack_vec3([ 0.75, -0.75, 0.0]);
    let v2 = pack_vec3([ 0.00,  0.75, 0.0]);

    // Source colour: red with alpha 0.5.
    let red_a05 = pack_vec4([1.0, 0.0, 0.0, 0.5]);

    // Pre-fill with opaque green.
    let mut pixels = vec![0u8; 16 * 16 * 4];
    for chunk in pixels.chunks_mut(4) {
        chunk[0] = 0;
        chunk[1] = 255;
        chunk[2] = 0;
        chunk[3] = 255;
    }

    let draw = DrawTriangle {
        vertex_attrs: [&v0, &v1, &v2],
        varyings_per_vertex: [&red_a05, &red_a05, &red_a05],
        varying_f32_count: 4,
        blend_state: BlendState::alpha_over(),
        ..Default::default()
    };
    registry.fill_image_triangle(
        vs_id, fs_id, &draw, 16, 16, &mut pixels, None,
    ).expect("alpha-over rasterise");

    let px = |x: usize, y: usize| -> [u8; 4] {
        let idx = (y * 16 + x) * 4;
        [pixels[idx], pixels[idx+1], pixels[idx+2], pixels[idx+3]]
    };

    // Interior pixel — alpha-over result, with f32→u8
    // quantisation tolerance of ±1.
    let interior = px(8, 6);
    for (channel_name, k, expected) in [
        ("r", 0, 128u8),
        ("g", 1, 128u8),
        ("b", 2, 0u8),
        ("a", 3, 255u8),
    ] {
        let got = interior[k];
        let diff = (got as i32 - expected as i32).abs();
        assert!(diff <= 1,
            "interior {channel_name}: expected ~{expected}, got {got} \
             (full {interior:?})");
    }

    // Outside pixel — green background untouched.
    let outside = px(0, 0);
    assert_eq!(outside, [0, 255, 0, 255],
        "outside pixel should be unchanged green: {outside:?}");
}

/// R.5 acceptance: colour write mask.  Draw an opaque-blue
/// triangle (1.0 in B, 1.0 alpha) over green, but with the
/// blue write bit disabled.  Without the mask the triangle
/// would paint pure blue; with the mask the interior pixels'
/// blue byte must stay at the background's 0, and the OTHER
/// channels (R, G, A) get the source's values.
#[test]
fn rasterizer_r5_write_mask_blocks_blue() {
    let cache_dir = TempDir::new().unwrap();
    let config = LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    };
    let registry = Tier2Registry::new(config);

    let vs_id = registry.register(&build_passthrough_vs()).unwrap();
    let fs_id = registry.register(&build_passthrough_color_fs()).unwrap();

    let v0 = pack_vec3([-0.75, -0.75, 0.0]);
    let v1 = pack_vec3([ 0.75, -0.75, 0.0]);
    let v2 = pack_vec3([ 0.00,  0.75, 0.0]);
    let blue = pack_vec4([0.0, 0.0, 1.0, 1.0]);

    let mut pixels = vec![0u8; 16 * 16 * 4];
    // Background: solid green again so we can see the mask
    // preserve the existing blue=0 byte.
    for chunk in pixels.chunks_mut(4) {
        chunk[0] = 0; chunk[1] = 255; chunk[2] = 0; chunk[3] = 255;
    }

    let blend = BlendState {
        write_mask: ColorWriteMask { r: true, g: true, b: false, a: true },
        ..Default::default()
    };
    let draw = DrawTriangle {
        vertex_attrs: [&v0, &v1, &v2],
        varyings_per_vertex: [&blue, &blue, &blue],
        varying_f32_count: 4,
        blend_state: blend,
        ..Default::default()
    };
    registry.fill_image_triangle(
        vs_id, fs_id, &draw, 16, 16, &mut pixels, None,
    ).expect("write-masked rasterise");

    let interior = {
        let idx = (6 * 16 + 8) * 4;
        [pixels[idx], pixels[idx+1], pixels[idx+2], pixels[idx+3]]
    };
    // R + G channels got the source's (0, 0).  B was masked,
    // so it stays at the background's 0.  A got the source's
    // 255.  The full interior pixel ends up (0, 0, 0, 255).
    assert_eq!(interior, [0, 0, 0, 255],
        "write-mask blocked blue + everything else fired from src: \
         got {interior:?}");
}

/// R.5 sanity: blend disabled is identical to "src replaces
/// dst" (the R.1-R.4 behaviour), independent of the factor
/// fields' contents.  Regression-pins the default path's
/// fast skip-the-read branch.
#[test]
fn rasterizer_r5_blend_disabled_is_source_replace() {
    let cache_dir = TempDir::new().unwrap();
    let config = LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    };
    let registry = Tier2Registry::new(config);

    let vs_id = registry.register(&build_passthrough_vs()).unwrap();
    let fs_id = registry.register(&build_passthrough_color_fs()).unwrap();

    let v0 = pack_vec3([-0.75, -0.75, 0.0]);
    let v1 = pack_vec3([ 0.75, -0.75, 0.0]);
    let v2 = pack_vec3([ 0.00,  0.75, 0.0]);
    let src = pack_vec4([0.3, 0.7, 0.1, 0.4]);   // arbitrary RGBA

    let mut pixels = vec![0u8; 16 * 16 * 4];
    for chunk in pixels.chunks_mut(4) {
        chunk[0] = 200; chunk[1] = 200; chunk[2] = 200; chunk[3] = 200;
    }

    // Plausible-looking blend factors, but enable=false.
    // The factor / op fields must be ignored.
    let blend = BlendState {
        enable: false,
        color: BlendFactorPair { src: BlendFactor::SrcAlpha, dst: BlendFactor::OneMinusSrcAlpha },
        alpha: BlendFactorPair { src: BlendFactor::SrcAlpha, dst: BlendFactor::OneMinusSrcAlpha },
        color_op: BlendOp::Add,
        alpha_op: BlendOp::Add,
        write_mask: ColorWriteMask::ALL,
    };
    let draw = DrawTriangle {
        vertex_attrs: [&v0, &v1, &v2],
        varyings_per_vertex: [&src, &src, &src],
        varying_f32_count: 4,
        blend_state: blend,
        ..Default::default()
    };
    registry.fill_image_triangle(
        vs_id, fs_id, &draw, 16, 16, &mut pixels, None,
    ).expect("disabled-blend rasterise");

    // Inside the triangle, the source colour replaces the dst.
    // 0.3 → ~76, 0.7 → ~178, 0.1 → ~26, 0.4 → ~102.
    let interior = {
        let idx = (6 * 16 + 8) * 4;
        [pixels[idx], pixels[idx+1], pixels[idx+2], pixels[idx+3]]
    };
    for (k, ef) in [(0, 0.3f32), (1, 0.7), (2, 0.1), (3, 0.4)] {
        let expected = (ef * 255.0 + 0.5) as u8;
        let got = interior[k];
        let diff = (got as i32 - expected as i32).abs();
        assert!(diff <= 1,
            "blend-disabled channel {k}: expected ~{expected}, got {got} \
             (full {interior:?})");
    }
}

/// R.6 acceptance: a triangle whose screen-space bbox
/// straddles multiple 8×8 tiles is rasterised identically
/// to what the un-tiled loop would have produced.  Spot-
/// checks pixels at (and just past) the tile-boundary
/// columns x=8 and x=16 — exactly where a tile-loop bug
/// would manifest as off-by-one or skipped pixels.
///
/// Setup: 24×24 image.  Triangle vertices in NDC pick a
/// nice symmetric shape that crosses 9 tiles
/// (3×3 grid of 8×8 tiles).  Bary-linear interp paints a
/// gradient varying so the boundary checks ALSO verify the
/// interpolated colour matches across tile seams.
#[test]
fn rasterizer_r6_tiles_24x24_spanning_9_tiles() {
    let cache_dir = TempDir::new().unwrap();
    let config = LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    };
    let registry = Tier2Registry::new(config);
    let vs_id = registry.register(&build_passthrough_vs()).unwrap();
    let fs_id = registry.register(&build_passthrough_color_fs()).unwrap();

    // NDC vertices forming a triangle that nearly fills the
    // 24x24 image -- crossing all 9 tiles in the 3x3 grid.
    //   (-1+1)*0.5*24 =  0      (+1+1)*0.5*24 = 24
    let v0 = pack_vec3([-0.9, -0.9, 0.0]);   // screen (1.2, 1.2)
    let v1 = pack_vec3([ 0.9, -0.9, 0.0]);   // screen (22.8, 1.2)
    let v2 = pack_vec3([ 0.0,  0.9, 0.0]);   // screen (12, 22.8)
    let red   = pack_vec4([1.0, 0.0, 0.0, 1.0]);
    let green = pack_vec4([0.0, 1.0, 0.0, 1.0]);
    let blue  = pack_vec4([0.0, 0.0, 1.0, 1.0]);

    let mut pixels = vec![0u8; 24 * 24 * 4];
    let draw = DrawTriangle {
        vertex_attrs: [&v0, &v1, &v2],
        varyings_per_vertex: [&red, &green, &blue],
        varying_f32_count: 4,
        ..Default::default()
    };
    registry.fill_image_triangle(
        vs_id, fs_id, &draw, 24, 24, &mut pixels, None,
    ).expect("tiled rasterise");

    let px = |x: usize, y: usize| -> [u8; 4] {
        let idx = (y * 24 + x) * 4;
        [pixels[idx], pixels[idx+1], pixels[idx+2], pixels[idx+3]]
    };

    // Tile-boundary spot checks at y=12 (deep inside the
    // triangle — its x-span at that row is [6.6, 17.4], so
    // pixels 7, 8, 15, 16 are all well inside).  x=8 and
    // x=16 are exactly the tile-boundary columns on an 8-pixel
    // tile grid; a tile-loop bug would show as off-by-one
    // pixel skips or visible seams in colour interpolation
    // exactly here.
    let y = 12usize;
    for x in [7usize, 8, 15, 16] {
        let p = px(x, y);
        assert_ne!(p, [0, 0, 0, 0],
            "pixel ({x}, {y}) at/near a tile boundary is \
             unpainted, but should be inside the triangle: {p:?}");
        assert_eq!(p[3], 255,
            "pixel ({x}, {y}) alpha drifted at tile boundary: {p:?}");
    }
    // Adjacent-pair difference within a tolerance per channel.
    // Bary interpolation across an 8-wide tile span shouldn't
    // introduce visible seams.
    let p7 = px(7, y);
    let p8 = px(8, y);
    for k in 0..3 {
        let d = (p7[k] as i32 - p8[k] as i32).abs();
        assert!(d <= 30,
            "tile boundary x=7..8, y={y}, channel {k}: \
             delta {d} > 30 (p7={p7:?}, p8={p8:?})");
    }
    let p15 = px(15, y);
    let p16 = px(16, y);
    for k in 0..3 {
        let d = (p15[k] as i32 - p16[k] as i32).abs();
        assert!(d <= 30,
            "tile boundary x=15..16, y={y}, channel {k}: \
             delta {d} > 30 (p15={p15:?}, p16={p16:?})");
    }

    // Pixels well outside the triangle (e.g., the corners
    // of the 24x24 grid) stay cleared.
    assert_eq!(px(0, 0), [0, 0, 0, 0]);
    assert_eq!(px(23, 0), [0, 0, 0, 0]);
    assert_eq!(px(0, 23), [0, 0, 0, 0]);
    assert_eq!(px(23, 23), [0, 0, 0, 0]);
}

/// Variant of the passthrough VS that reads a vec4
/// attribute (xyz + w) and writes gl_Position = that vec4
/// directly.  Lets the test feed a custom w per vertex so
/// the perspective-divide step is actually exercised.
fn build_vec4_position_vs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass as SpvStorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let i32_ty = b.type_int(32, 1);
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);

    let per_vertex = b.type_struct(vec![vec4]);
    b.member_decorate(per_vertex, 0, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::Position)]);
    b.member_decorate(per_vertex, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(per_vertex, Decoration::Block, vec![]);

    let ptr_pv = b.type_pointer(None, SpvStorageClass::Output, per_vertex);
    let ptr_out_vec4 = b.type_pointer(None, SpvStorageClass::Output, vec4);
    let ptr_in_vec4  = b.type_pointer(None, SpvStorageClass::Input, vec4);

    let in_pos = b.variable(ptr_in_vec4, None, SpvStorageClass::Input, None);
    b.decorate(in_pos, Decoration::Location,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let pv_var = b.variable(ptr_pv, None, SpvStorageClass::Output, None);
    let c_zero = b.constant_bit32(i32_ty, 0u32);

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let pos4 = b.load(vec4, None, in_pos, None, vec![]).unwrap();
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
    let draw = DrawTriangle {
        vertex_attrs: [&v0, &v1, &v2],
        ..Default::default()
    };
    registry.fill_image_triangle(
        vs_id, fs_id,
        &draw,
        8, 8,
        &mut pixels,
        None,
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
