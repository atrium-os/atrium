//! `texture_sample_centre_rgbw` — first differential
//! test for the texture/sampler arc. Builds a `sampler2D`
//! shader that returns `texture(tex, vec2(0.5))`, drives
//! it through the interpreter and the Cranelift backend
//! (the bespoke backend currently skips as `Unsupported`
//! — it lands in phase 5), and asserts both runners agree
//! on the bilinear-centre average pixel.
//!
//! Exercises the v1 descriptor ABI (atrium-spv-runtime
//! `UNIFORMS_HELPERS_BASE` / `UNIFORMS_DESC_BASE`): the
//! uniforms buffer's first 16 bytes carry the runtime
//! helper function pointer, the next 16 bytes carry the
//! descriptor slot (tex_desc* + samp_desc*). The Cranelift
//! backend's emitted code reads those + builds a stack
//! slot for `out_rgba` + calls `atrium_tex_sample_2d` via
//! the loaded fn pointer.

use atrium_spv_runtime::{
    FilterMode, SamplerDesc, TexDesc, TexFormat, WrapMode,
    DESC_SLOT_BYTES, UNIFORMS_DESC_BASE,
    descriptor_table_buffer, write_descriptor_slot, write_helper_pointers,
};
use atrium_spv_tests::harness::{
    assert_shader_agrees, BackendError, InterpreterRunner, ShaderRunner,
};
use atrium_spv_tests::interpreter::{ShaderInputs, TextureBinding};

use atrium_spv_differential::{BespokeRunner, CraneliftRunner};

fn build_sample_centre_shader() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, Dim, ExecutionMode,
        ExecutionModel, FunctionControl, ImageFormat, MemoryModel,
        StorageClass as SpvStorageClass,
    };

    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let vec2_f32 = b.type_vector(f32_ty, 2);
    let vec4_f32 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);

    let image_ty = b.type_image(
        f32_ty, Dim::Dim2D, 0, 0, 0, 1, ImageFormat::Unknown, None);
    let sampled_image_ty = b.type_sampled_image(image_ty);

    let ptr_uc_si = b.type_pointer(
        None, SpvStorageClass::UniformConstant, sampled_image_ty);
    let ptr_out_vec4 = b.type_pointer(
        None, SpvStorageClass::Output, vec4_f32);

    let tex = b.variable(ptr_uc_si, None, SpvStorageClass::UniformConstant, None);
    b.decorate(tex, Decoration::DescriptorSet,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(tex, Decoration::Binding,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let out = b.variable(ptr_out_vec4, None, SpvStorageClass::Output, None);
    b.decorate(out, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let c_half = b.constant_bit32(f32_ty, 0.5f32.to_bits());
    let uv = b.constant_composite(vec2_f32, vec![c_half, c_half]);

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let sampled = b.load(sampled_image_ty, None, tex, None, vec![]).unwrap();
    let pixel = b.image_sample_implicit_lod(
        vec4_f32, None, sampled, uv, None, vec![]).unwrap();
    b.store(out, pixel, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();

    b.entry_point(ExecutionModel::Fragment, main, "main", vec![tex, out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);

    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn texture_sample_centre_rgbw() {
    // 2×2 RGBA8 checkerboard: red, green / blue, white.
    // Sampled at u=v=0.5 with Bilinear/Clamp the result
    // is the four-texel mean — (0.5, 0.5, 0.5, 1.0).
    let pixels: Vec<u8> = vec![
        255,   0,   0, 255,   // (0,0) red
          0, 255,   0, 255,   // (1,0) green
          0,   0, 255, 255,   // (0,1) blue
        255, 255, 255, 255,   // (1,1) white
    ];
    // Host-side descriptor structs. Their addresses go into
    // the uniforms buffer below; they must outlive the
    // shader invocations.
    let tex_desc = TexDesc {
        data: pixels.as_ptr(),
        width: 2, height: 2, stride_bytes: 8,
        format: TexFormat::Rgba8Unorm as u32,
        depth: 1, slice_bytes: 0,
            mip_count: 0, mip_descs: std::ptr::null(),
    };
    let samp_desc = SamplerDesc {
        mag_filter: FilterMode::Linear as u32,
        min_filter: FilterMode::Linear as u32,
        wrap_s: WrapMode::ClampToEdge as u32,
        wrap_t: WrapMode::ClampToEdge as u32,
    };

    // Build the uniforms buffer per the v1 ABI: helper
    // header at byte 0, descriptor table starting at
    // UNIFORMS_DESC_BASE.
    let mut uniforms = descriptor_table_buffer(/*count=*/1);
    assert_eq!(uniforms.len(), UNIFORMS_DESC_BASE + DESC_SLOT_BYTES);
    unsafe {
        write_helper_pointers(&mut uniforms,
            atrium_spv_runtime::atrium_tex_sample_2d,
            atrium_spv_runtime::atrium_tex_fetch_2d,
            atrium_spv_runtime::atrium_tex_sample_2d_lod,
            atrium_spv_runtime::atrium_tex_sample_2d_array,
            atrium_spv_runtime::atrium_tex_sample_cube,
            atrium_spv_runtime::atrium_tex_gather_2d,
            atrium_spv_runtime::atrium_tex_sample_2d_array_lod,
            atrium_spv_runtime::atrium_tex_sample_cube_lod);
        write_descriptor_slot(&mut uniforms, 0,
            &tex_desc as *const _, &samp_desc as *const _);
    }

    // For the interpreter, the same texture+sampler shows
    // up via a TextureBinding (host-owned data + dims +
    // format + sampler). The interpreter doesn't read
    // descriptors out of `uniforms`; it looks the binding
    // up by (set, binding).
    let texture = TextureBinding {
        set: 0, binding: 0,
        data: pixels.clone(),
        width: 2, height: 2, stride_bytes: 8,
        format: TexFormat::Rgba8Unorm as u32,
        sampler: samp_desc,
    };

    let inputs = ShaderInputs {
        textures: vec![texture],
        uniforms,
        ..ShaderInputs::default()
    };

    let spirv = build_sample_centre_shader();

    let runners: [Box<dyn ShaderRunner>; 3] = [
        Box::new(InterpreterRunner),
        Box::new(CraneliftRunner::default()),
        Box::new(BespokeRunner::default()),
    ];
    let refs: Vec<&dyn ShaderRunner> =
        runners.iter().map(|b| b.as_ref()).collect();

    // The bespoke runner isn't in the corpus here — it
    // currently returns Unsupported for Op::ImageHandle
    // (phase 5 wires it). assert_shader_agrees handles
    // that with skip semantics, but we don't need it in
    // the corpus at all for this test.
    let tol = atrium_spv_tests::pixels::ColorTolerance::AbsEpsilon { eps: 1e-6 };
    assert_shader_agrees(&spirv, &inputs, tol, &refs);

    // Sanity-check pin the absolute value too — guards
    // against both runners agreeing on garbage.
    use atrium_spv_tests::interpreter::Interpreter;
    let interp = Interpreter::new(&spirv).unwrap();
    let out = interp.run_fragment(&inputs).unwrap();
    let p = &out.pixels[0];
    for k in 0..3 { assert!((p[k] - 0.5).abs() < 1e-6,
        "lane {k}: expected 0.5, got {} (full {:?})", p[k], p); }
    assert!((p[3] - 1.0).abs() < 1e-6);

    // Catch the "unused" warning on BackendError without
    // forcing test-time usage.
    let _ = std::marker::PhantomData::<BackendError>;
}

/// Same texture, but the shader also computes a tint
/// vec4 *before* the sample and multiplies the sampled
/// pixel by it. The tint's V-regs are loop-carried across
/// the `blr` to `atrium_tex_sample_2d`, so the bespoke
/// backend's cross-call spill/reload of every owned V-reg
/// gates this test — without it the post-sample FMul
/// would read clobbered tint values and produce garbage.
fn build_tinted_sample_shader() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, Dim, ExecutionMode,
        ExecutionModel, FunctionControl, ImageFormat, MemoryModel,
        StorageClass as SpvStorageClass,
    };

    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let vec2_f32 = b.type_vector(f32_ty, 2);
    let vec4_f32 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);

    let image_ty = b.type_image(
        f32_ty, Dim::Dim2D, 0, 0, 0, 1, ImageFormat::Unknown, None);
    let sampled_image_ty = b.type_sampled_image(image_ty);

    let ptr_uc_si = b.type_pointer(
        None, SpvStorageClass::UniformConstant, sampled_image_ty);
    let ptr_out_vec4 = b.type_pointer(
        None, SpvStorageClass::Output, vec4_f32);

    let tex = b.variable(ptr_uc_si, None, SpvStorageClass::UniformConstant, None);
    b.decorate(tex, Decoration::DescriptorSet,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(tex, Decoration::Binding,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let out = b.variable(ptr_out_vec4, None, SpvStorageClass::Output, None);
    b.decorate(out, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);

    // uv = (0.25, 0.25) → centre of texel (0,0) under
    // Nearest/Clamp on the 2x2 RGBW checker → red.
    let c_quarter = b.constant_bit32(f32_ty, 0.25f32.to_bits());
    let uv = b.constant_composite(vec2_f32, vec![c_quarter, c_quarter]);
    // tint = (0.5, 0.25, 0.75, 1.0) — four distinct lanes
    // so a garbled cross-call reload would visibly diverge.
    let c_t0 = b.constant_bit32(f32_ty, 0.5f32.to_bits());
    let c_t1 = b.constant_bit32(f32_ty, 0.25f32.to_bits());
    let c_t2 = b.constant_bit32(f32_ty, 0.75f32.to_bits());
    let c_t3 = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let tint = b.constant_composite(vec4_f32, vec![c_t0, c_t1, c_t2, c_t3]);

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let sampled = b.load(sampled_image_ty, None, tex, None, vec![]).unwrap();
    let pixel = b.image_sample_implicit_lod(
        vec4_f32, None, sampled, uv, None, vec![]).unwrap();
    let result = b.f_mul(vec4_f32, None, pixel, tint).unwrap();
    b.store(out, result, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();

    b.entry_point(ExecutionModel::Fragment, main, "main", vec![tex, out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);

    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

/// `texture_sample_tinted` — exercises the cross-call
/// save/restore by carrying a tint vec4 across the
/// `ImageSample`. The tint lanes are live across `blr` to
/// `atrium_tex_sample_2d`; the bespoke spill/reload
/// preserves them. Also covers the `last_use` fix for
/// ImageSample's coord-lane propagation — without it
/// `c_quarter`'s V-reg would be recycled before the
/// sample reads it, clobbering the uv argument.
#[test]
fn texture_sample_tinted() {
    // Same 2x2 RGBW checker + Nearest/Clamp sampler. At
    // u=v=0.25 the Nearest sampler resolves to texel
    // (0,0) → red (1, 0, 0, 1). The shader multiplies by
    // tint=(0.5, 0.25, 0.75, 1.0), so we expect
    // (0.5, 0, 0, 1).
    let pixels: Vec<u8> = vec![
        255,   0,   0, 255,
          0, 255,   0, 255,
          0,   0, 255, 255,
        255, 255, 255, 255,
    ];
    let tex_desc = TexDesc {
        data: pixels.as_ptr(),
        width: 2, height: 2, stride_bytes: 8,
        format: TexFormat::Rgba8Unorm as u32,
        depth: 1, slice_bytes: 0,
            mip_count: 0, mip_descs: std::ptr::null(),
    };
    let samp_desc = SamplerDesc {
        mag_filter: FilterMode::Nearest as u32,
        min_filter: FilterMode::Nearest as u32,
        wrap_s: WrapMode::ClampToEdge as u32,
        wrap_t: WrapMode::ClampToEdge as u32,
    };
    let mut uniforms = descriptor_table_buffer(1);
    unsafe {
        write_helper_pointers(&mut uniforms,
            atrium_spv_runtime::atrium_tex_sample_2d,
            atrium_spv_runtime::atrium_tex_fetch_2d,
            atrium_spv_runtime::atrium_tex_sample_2d_lod,
            atrium_spv_runtime::atrium_tex_sample_2d_array,
            atrium_spv_runtime::atrium_tex_sample_cube,
            atrium_spv_runtime::atrium_tex_gather_2d,
            atrium_spv_runtime::atrium_tex_sample_2d_array_lod,
            atrium_spv_runtime::atrium_tex_sample_cube_lod);
        write_descriptor_slot(&mut uniforms, 0,
            &tex_desc as *const _, &samp_desc as *const _);
    }
    let texture = TextureBinding {
        set: 0, binding: 0,
        data: pixels.clone(),
        width: 2, height: 2, stride_bytes: 8,
        format: TexFormat::Rgba8Unorm as u32,
        sampler: samp_desc,
    };
    let inputs = ShaderInputs {
        textures: vec![texture],
        uniforms,
        ..ShaderInputs::default()
    };

    let spirv = build_tinted_sample_shader();
    let runners: [Box<dyn ShaderRunner>; 3] = [
        Box::new(InterpreterRunner),
        Box::new(CraneliftRunner::default()),
        Box::new(BespokeRunner::default()),
    ];
    let refs: Vec<&dyn ShaderRunner> =
        runners.iter().map(|b| b.as_ref()).collect();
    let tol = atrium_spv_tests::pixels::ColorTolerance::AbsEpsilon { eps: 1e-6 };
    assert_shader_agrees(&spirv, &inputs, tol, &refs);

    // Sanity pin.
    use atrium_spv_tests::interpreter::Interpreter;
    let interp = Interpreter::new(&spirv).unwrap();
    let out = interp.run_fragment(&inputs).unwrap();
    let p = &out.pixels[0];
    let expected = [0.5_f32, 0.0, 0.0, 1.0];
    for k in 0..4 {
        assert!((p[k] - expected[k]).abs() < 1e-6,
            "lane {k}: expected {}, got {} (full {:?})",
            expected[k], p[k], p);
    }
    let _ = DESC_SLOT_BYTES;
    let _ = UNIFORMS_DESC_BASE;
}

/// Arc 37 follow-up: projective texturing with an explicit
/// LOD operand.  `OpImageSampleProjExplicitLod` with coord
/// `(0.5, 0.5, 1.0)` and lod=0 — after the q=1 divide reduces
/// to a plain sample at `(0.5, 0.5)` with mip 0.  Exercises
/// the Proj-Explicit branch (extracts the LOD operand from
/// SPIR-V index 2, *before* the image-operand mask shift).
fn build_sample_proj_explicit_lod_shader() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, Dim, ExecutionMode,
        ExecutionModel, FunctionControl, ImageFormat, ImageOperands,
        MemoryModel, StorageClass as SpvStorageClass,
    };

    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let vec3_f32 = b.type_vector(f32_ty, 3);
    let vec4_f32 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);

    let image_ty = b.type_image(
        f32_ty, Dim::Dim2D, 0, 0, 0, 1, ImageFormat::Unknown, None);
    let sampled_image_ty = b.type_sampled_image(image_ty);

    let ptr_uc_si = b.type_pointer(
        None, SpvStorageClass::UniformConstant, sampled_image_ty);
    let ptr_out_vec4 = b.type_pointer(
        None, SpvStorageClass::Output, vec4_f32);

    let tex = b.variable(ptr_uc_si, None, SpvStorageClass::UniformConstant, None);
    b.decorate(tex, Decoration::DescriptorSet,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(tex, Decoration::Binding,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let out = b.variable(ptr_out_vec4, None, SpvStorageClass::Output, None);
    b.decorate(out, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let c_half = b.constant_bit32(f32_ty, 0.5f32.to_bits());
    let c_one  = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let c_zero = b.constant_bit32(f32_ty, 0.0f32.to_bits());
    let uvq = b.constant_composite(vec3_f32, vec![c_half, c_half, c_one]);

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let sampled = b.load(sampled_image_ty, None, tex, None, vec![]).unwrap();
    let pixel = b.image_sample_proj_explicit_lod(
        vec4_f32, None, sampled, uvq,
        ImageOperands::LOD,
        vec![rspirv::dr::Operand::IdRef(c_zero)]).unwrap();
    b.store(out, pixel, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();

    b.entry_point(ExecutionModel::Fragment, main, "main", vec![tex, out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);

    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn texture_sample_proj_explicit_lod() {
    let pixels: Vec<u8> = vec![
        255,   0,   0, 255,
          0, 255,   0, 255,
          0,   0, 255, 255,
        255, 255, 255, 255,
    ];
    let tex_desc = TexDesc {
        data: pixels.as_ptr(),
        width: 2, height: 2, stride_bytes: 8,
        format: TexFormat::Rgba8Unorm as u32,
        depth: 1, slice_bytes: 0,
        mip_count: 0, mip_descs: std::ptr::null(),
    };
    let samp_desc = SamplerDesc {
        mag_filter: FilterMode::Linear as u32,
        min_filter: FilterMode::Linear as u32,
        wrap_s: WrapMode::ClampToEdge as u32,
        wrap_t: WrapMode::ClampToEdge as u32,
    };
    let mut uniforms = descriptor_table_buffer(1);
    unsafe {
        write_helper_pointers(&mut uniforms,
            atrium_spv_runtime::atrium_tex_sample_2d,
            atrium_spv_runtime::atrium_tex_fetch_2d,
            atrium_spv_runtime::atrium_tex_sample_2d_lod,
            atrium_spv_runtime::atrium_tex_sample_2d_array,
            atrium_spv_runtime::atrium_tex_sample_cube,
            atrium_spv_runtime::atrium_tex_gather_2d,
            atrium_spv_runtime::atrium_tex_sample_2d_array_lod,
            atrium_spv_runtime::atrium_tex_sample_cube_lod);
        write_descriptor_slot(&mut uniforms, 0,
            &tex_desc as *const _, &samp_desc as *const _);
    }
    let texture = TextureBinding {
        set: 0, binding: 0,
        data: pixels.clone(),
        width: 2, height: 2, stride_bytes: 8,
        format: TexFormat::Rgba8Unorm as u32,
        sampler: samp_desc,
    };
    let inputs = ShaderInputs {
        textures: vec![texture],
        uniforms,
        ..ShaderInputs::default()
    };

    let spirv = build_sample_proj_explicit_lod_shader();
    let runners: [Box<dyn ShaderRunner>; 3] = [
        Box::new(InterpreterRunner),
        Box::new(CraneliftRunner::default()),
        Box::new(BespokeRunner::default()),
    ];
    let refs: Vec<&dyn ShaderRunner> =
        runners.iter().map(|b| b.as_ref()).collect();
    let tol = atrium_spv_tests::pixels::ColorTolerance::AbsEpsilon { eps: 1e-6 };
    assert_shader_agrees(&spirv, &inputs, tol, &refs);

    use atrium_spv_tests::interpreter::Interpreter;
    let interp = Interpreter::new(&spirv).unwrap();
    let out = interp.run_fragment(&inputs).unwrap();
    let p = &out.pixels[0];
    for k in 0..3 { assert!((p[k] - 0.5).abs() < 1e-6,
        "lane {k}: expected 0.5, got {} (full {:?})", p[k], p); }
    assert!((p[3] - 1.0).abs() < 1e-6);
}

/// Arc 38: `textureQueryLod(sampler, uv)` returns vec2(0, 0)
/// in Tier-2's derivative-free implicit-LOD world.  Shader
/// stores the lod into the red channel of the output.
fn build_query_lod_shader() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, Dim, ExecutionMode,
        ExecutionModel, FunctionControl, ImageFormat, MemoryModel,
        StorageClass as SpvStorageClass,
    };

    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.capability(Capability::ImageQuery);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let vec2_f32 = b.type_vector(f32_ty, 2);
    let vec4_f32 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);

    let image_ty = b.type_image(
        f32_ty, Dim::Dim2D, 0, 0, 0, 1, ImageFormat::Unknown, None);
    let sampled_image_ty = b.type_sampled_image(image_ty);

    let ptr_uc_si = b.type_pointer(
        None, SpvStorageClass::UniformConstant, sampled_image_ty);
    let ptr_out_vec4 = b.type_pointer(
        None, SpvStorageClass::Output, vec4_f32);

    let tex = b.variable(ptr_uc_si, None, SpvStorageClass::UniformConstant, None);
    b.decorate(tex, Decoration::DescriptorSet,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(tex, Decoration::Binding,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let out = b.variable(ptr_out_vec4, None, SpvStorageClass::Output, None);
    b.decorate(out, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let c_half = b.constant_bit32(f32_ty, 0.5f32.to_bits());
    let c_one  = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let uv = b.constant_composite(vec2_f32, vec![c_half, c_half]);

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let sampled = b.load(sampled_image_ty, None, tex, None, vec![]).unwrap();
    let lod = b.image_query_lod(vec2_f32, None, sampled, uv).unwrap();
    let lod0 = b.composite_extract(f32_ty, None, lod, vec![0]).unwrap();
    let lod1 = b.composite_extract(f32_ty, None, lod, vec![1]).unwrap();
    let pixel = b.composite_construct(vec4_f32, None,
        vec![lod0, lod1, c_half, c_one]).unwrap();
    b.store(out, pixel, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();

    b.entry_point(ExecutionModel::Fragment, main, "main", vec![tex, out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);

    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn texture_query_lod_returns_zero_vec2() {
    let pixels: Vec<u8> = vec![255, 0, 0, 255];
    let tex_desc = TexDesc {
        data: pixels.as_ptr(),
        width: 1, height: 1, stride_bytes: 4,
        format: TexFormat::Rgba8Unorm as u32,
        depth: 1, slice_bytes: 0,
        mip_count: 0, mip_descs: std::ptr::null(),
    };
    let samp_desc = SamplerDesc {
        mag_filter: FilterMode::Linear as u32,
        min_filter: FilterMode::Linear as u32,
        wrap_s: WrapMode::ClampToEdge as u32,
        wrap_t: WrapMode::ClampToEdge as u32,
    };
    let mut uniforms = descriptor_table_buffer(1);
    unsafe {
        write_helper_pointers(&mut uniforms,
            atrium_spv_runtime::atrium_tex_sample_2d,
            atrium_spv_runtime::atrium_tex_fetch_2d,
            atrium_spv_runtime::atrium_tex_sample_2d_lod,
            atrium_spv_runtime::atrium_tex_sample_2d_array,
            atrium_spv_runtime::atrium_tex_sample_cube,
            atrium_spv_runtime::atrium_tex_gather_2d,
            atrium_spv_runtime::atrium_tex_sample_2d_array_lod,
            atrium_spv_runtime::atrium_tex_sample_cube_lod);
        write_descriptor_slot(&mut uniforms, 0,
            &tex_desc as *const _, &samp_desc as *const _);
    }
    let texture = TextureBinding {
        set: 0, binding: 0,
        data: pixels.clone(),
        width: 1, height: 1, stride_bytes: 4,
        format: TexFormat::Rgba8Unorm as u32,
        sampler: samp_desc,
    };
    let inputs = ShaderInputs {
        textures: vec![texture],
        uniforms,
        ..ShaderInputs::default()
    };

    let spirv = build_query_lod_shader();
    let runners: [Box<dyn ShaderRunner>; 3] = [
        Box::new(InterpreterRunner),
        Box::new(CraneliftRunner::default()),
        Box::new(BespokeRunner::default()),
    ];
    let refs: Vec<&dyn ShaderRunner> =
        runners.iter().map(|b| b.as_ref()).collect();
    let tol = atrium_spv_tests::pixels::ColorTolerance::AbsEpsilon { eps: 1e-6 };
    assert_shader_agrees(&spirv, &inputs, tol, &refs);

    use atrium_spv_tests::interpreter::Interpreter;
    let interp = Interpreter::new(&spirv).unwrap();
    let out = interp.run_fragment(&inputs).unwrap();
    let p = &out.pixels[0];
    assert_eq!(p[0], 0.0, "lod lane 0 should be 0, got {}", p[0]);
    assert_eq!(p[1], 0.0, "lod lane 1 should be 0, got {}", p[1]);
}

/// Arc 37: projective texturing.  Builds a shader that uses
/// `OpImageSampleProjImplicitLod` with coord `(0.5, 0.5, 1.0)`.
/// After the q=1 divide this reduces to a plain sample at
/// `(0.5, 0.5)` — same expected output as the non-proj test.
/// Exercises the frontend's Proj→FDiv+ConstVec lowering.
fn build_sample_proj_centre_shader() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, Dim, ExecutionMode,
        ExecutionModel, FunctionControl, ImageFormat, MemoryModel,
        StorageClass as SpvStorageClass,
    };

    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let vec3_f32 = b.type_vector(f32_ty, 3);
    let vec4_f32 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);

    let image_ty = b.type_image(
        f32_ty, Dim::Dim2D, 0, 0, 0, 1, ImageFormat::Unknown, None);
    let sampled_image_ty = b.type_sampled_image(image_ty);

    let ptr_uc_si = b.type_pointer(
        None, SpvStorageClass::UniformConstant, sampled_image_ty);
    let ptr_out_vec4 = b.type_pointer(
        None, SpvStorageClass::Output, vec4_f32);

    let tex = b.variable(ptr_uc_si, None, SpvStorageClass::UniformConstant, None);
    b.decorate(tex, Decoration::DescriptorSet,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(tex, Decoration::Binding,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let out = b.variable(ptr_out_vec4, None, SpvStorageClass::Output, None);
    b.decorate(out, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let c_half = b.constant_bit32(f32_ty, 0.5f32.to_bits());
    let c_one  = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    // (0.5, 0.5, 1.0) → after /q gives (0.5, 0.5).
    let uvq = b.constant_composite(vec3_f32, vec![c_half, c_half, c_one]);

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let sampled = b.load(sampled_image_ty, None, tex, None, vec![]).unwrap();
    let pixel = b.image_sample_proj_implicit_lod(
        vec4_f32, None, sampled, uvq, None, vec![]).unwrap();
    b.store(out, pixel, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();

    b.entry_point(ExecutionModel::Fragment, main, "main", vec![tex, out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);

    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn texture_sample_proj_implicit_lod() {
    let pixels: Vec<u8> = vec![
        255,   0,   0, 255,
          0, 255,   0, 255,
          0,   0, 255, 255,
        255, 255, 255, 255,
    ];
    let tex_desc = TexDesc {
        data: pixels.as_ptr(),
        width: 2, height: 2, stride_bytes: 8,
        format: TexFormat::Rgba8Unorm as u32,
        depth: 1, slice_bytes: 0,
        mip_count: 0, mip_descs: std::ptr::null(),
    };
    let samp_desc = SamplerDesc {
        mag_filter: FilterMode::Linear as u32,
        min_filter: FilterMode::Linear as u32,
        wrap_s: WrapMode::ClampToEdge as u32,
        wrap_t: WrapMode::ClampToEdge as u32,
    };
    let mut uniforms = descriptor_table_buffer(1);
    unsafe {
        write_helper_pointers(&mut uniforms,
            atrium_spv_runtime::atrium_tex_sample_2d,
            atrium_spv_runtime::atrium_tex_fetch_2d,
            atrium_spv_runtime::atrium_tex_sample_2d_lod,
            atrium_spv_runtime::atrium_tex_sample_2d_array,
            atrium_spv_runtime::atrium_tex_sample_cube,
            atrium_spv_runtime::atrium_tex_gather_2d,
            atrium_spv_runtime::atrium_tex_sample_2d_array_lod,
            atrium_spv_runtime::atrium_tex_sample_cube_lod);
        write_descriptor_slot(&mut uniforms, 0,
            &tex_desc as *const _, &samp_desc as *const _);
    }
    let texture = TextureBinding {
        set: 0, binding: 0,
        data: pixels.clone(),
        width: 2, height: 2, stride_bytes: 8,
        format: TexFormat::Rgba8Unorm as u32,
        sampler: samp_desc,
    };
    let inputs = ShaderInputs {
        textures: vec![texture],
        uniforms,
        ..ShaderInputs::default()
    };

    let spirv = build_sample_proj_centre_shader();
    let runners: [Box<dyn ShaderRunner>; 3] = [
        Box::new(InterpreterRunner),
        Box::new(CraneliftRunner::default()),
        Box::new(BespokeRunner::default()),
    ];
    let refs: Vec<&dyn ShaderRunner> =
        runners.iter().map(|b| b.as_ref()).collect();
    let tol = atrium_spv_tests::pixels::ColorTolerance::AbsEpsilon { eps: 1e-6 };
    assert_shader_agrees(&spirv, &inputs, tol, &refs);

    use atrium_spv_tests::interpreter::Interpreter;
    let interp = Interpreter::new(&spirv).unwrap();
    let out = interp.run_fragment(&inputs).unwrap();
    let p = &out.pixels[0];
    for k in 0..3 { assert!((p[k] - 0.5).abs() < 1e-6,
        "lane {k}: expected 0.5, got {} (full {:?})", p[k], p); }
    assert!((p[3] - 1.0).abs() < 1e-6);
}

/// Arc 36: same centre-RGBW sample, but the shader uses
/// `OpImageSampleImplicitLod` with an `Image-Operands::Bias`
/// argument.  With a single-mip texture and our mip-0-only
/// implicit-LOD, Bias collapses to "select mip 0", so all
/// three runners must still agree on the bilinear-centre
/// average.  Exercises the frontend's new Bias→ExplicitLod
/// translation.
fn build_sample_centre_bias_shader() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, Dim, ExecutionMode,
        ExecutionModel, FunctionControl, ImageFormat, ImageOperands,
        MemoryModel, StorageClass as SpvStorageClass,
    };

    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let vec2_f32 = b.type_vector(f32_ty, 2);
    let vec4_f32 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);

    let image_ty = b.type_image(
        f32_ty, Dim::Dim2D, 0, 0, 0, 1, ImageFormat::Unknown, None);
    let sampled_image_ty = b.type_sampled_image(image_ty);

    let ptr_uc_si = b.type_pointer(
        None, SpvStorageClass::UniformConstant, sampled_image_ty);
    let ptr_out_vec4 = b.type_pointer(
        None, SpvStorageClass::Output, vec4_f32);

    let tex = b.variable(ptr_uc_si, None, SpvStorageClass::UniformConstant, None);
    b.decorate(tex, Decoration::DescriptorSet,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(tex, Decoration::Binding,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let out = b.variable(ptr_out_vec4, None, SpvStorageClass::Output, None);
    b.decorate(out, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let c_half = b.constant_bit32(f32_ty, 0.5f32.to_bits());
    let uv = b.constant_composite(vec2_f32, vec![c_half, c_half]);
    let c_zero = b.constant_bit32(f32_ty, 0.0f32.to_bits());

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let sampled = b.load(sampled_image_ty, None, tex, None, vec![]).unwrap();
    let pixel = b.image_sample_implicit_lod(
        vec4_f32, None, sampled, uv,
        Some(ImageOperands::BIAS),
        vec![rspirv::dr::Operand::IdRef(c_zero)]).unwrap();
    b.store(out, pixel, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();

    b.entry_point(ExecutionModel::Fragment, main, "main", vec![tex, out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);

    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn texture_sample_implicit_lod_with_bias() {
    let pixels: Vec<u8> = vec![
        255,   0,   0, 255,
          0, 255,   0, 255,
          0,   0, 255, 255,
        255, 255, 255, 255,
    ];
    let tex_desc = TexDesc {
        data: pixels.as_ptr(),
        width: 2, height: 2, stride_bytes: 8,
        format: TexFormat::Rgba8Unorm as u32,
        depth: 1, slice_bytes: 0,
        mip_count: 0, mip_descs: std::ptr::null(),
    };
    let samp_desc = SamplerDesc {
        mag_filter: FilterMode::Linear as u32,
        min_filter: FilterMode::Linear as u32,
        wrap_s: WrapMode::ClampToEdge as u32,
        wrap_t: WrapMode::ClampToEdge as u32,
    };
    let mut uniforms = descriptor_table_buffer(1);
    unsafe {
        write_helper_pointers(&mut uniforms,
            atrium_spv_runtime::atrium_tex_sample_2d,
            atrium_spv_runtime::atrium_tex_fetch_2d,
            atrium_spv_runtime::atrium_tex_sample_2d_lod,
            atrium_spv_runtime::atrium_tex_sample_2d_array,
            atrium_spv_runtime::atrium_tex_sample_cube,
            atrium_spv_runtime::atrium_tex_gather_2d,
            atrium_spv_runtime::atrium_tex_sample_2d_array_lod,
            atrium_spv_runtime::atrium_tex_sample_cube_lod);
        write_descriptor_slot(&mut uniforms, 0,
            &tex_desc as *const _, &samp_desc as *const _);
    }
    let texture = TextureBinding {
        set: 0, binding: 0,
        data: pixels.clone(),
        width: 2, height: 2, stride_bytes: 8,
        format: TexFormat::Rgba8Unorm as u32,
        sampler: samp_desc,
    };
    let inputs = ShaderInputs {
        textures: vec![texture],
        uniforms,
        ..ShaderInputs::default()
    };

    let spirv = build_sample_centre_bias_shader();
    let runners: [Box<dyn ShaderRunner>; 3] = [
        Box::new(InterpreterRunner),
        Box::new(CraneliftRunner::default()),
        Box::new(BespokeRunner::default()),
    ];
    let refs: Vec<&dyn ShaderRunner> =
        runners.iter().map(|b| b.as_ref()).collect();
    let tol = atrium_spv_tests::pixels::ColorTolerance::AbsEpsilon { eps: 1e-6 };
    assert_shader_agrees(&spirv, &inputs, tol, &refs);

    use atrium_spv_tests::interpreter::Interpreter;
    let interp = Interpreter::new(&spirv).unwrap();
    let out = interp.run_fragment(&inputs).unwrap();
    let p = &out.pixels[0];
    for k in 0..3 { assert!((p[k] - 0.5).abs() < 1e-6,
        "lane {k}: expected 0.5, got {} (full {:?})", p[k], p); }
    assert!((p[3] - 1.0).abs() < 1e-6);
}
