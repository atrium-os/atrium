//! Interpreter ImageSample handler — phase-2 gate of the
//! texture/sampler arc.
//!
//! Builds a tiny fragment shader that samples a host-side
//! 2×2 texture at u=v=0.5 and stores the resulting vec4 to
//! `out_color`. Drives it through the interpreter and
//! checks the pixel against the texture's bilinear-centre
//! average (the four-texel mean — the same equation
//! `atrium-spv-runtime`'s own unit test verifies).
//!
//! This is the first end-to-end check that the runtime
//! crate, the interpreter's binding-decoration indexing,
//! the OpLoad-of-image short-circuit, and the
//! OpImageSampleImplicitLod handler agree.

use atrium_spv_runtime::{FilterMode, SamplerDesc, TexFormat, WrapMode};
use atrium_spv_tests::interpreter::{Interpreter, ShaderInputs, TextureBinding};

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
fn interpreter_bilinear_centre_of_rgbw_checker() {
    // 2×2 RGBA8 checkerboard: red, green / blue, white. The
    // four-texel meeting point at u=v=0.5 (after the
    // Vulkan/SPIR-V `u*w - 0.5` mapping) is exactly the
    // average — same equation the runtime's own
    // `bilinear_at_geometric_centre_averages_four` unit
    // test pins.
    let data: Vec<u8> = vec![
        255,   0,   0, 255,   // (0,0) red
          0, 255,   0, 255,   // (1,0) green
          0,   0, 255, 255,   // (0,1) blue
        255, 255, 255, 255,   // (1,1) white
    ];
    let binding = TextureBinding {
        set: 0, binding: 0,
        data, width: 2, height: 2, stride_bytes: 8,
        format: TexFormat::Rgba8Unorm as u32,
        sampler: SamplerDesc {
            mag_filter: FilterMode::Linear as u32,
            min_filter: FilterMode::Linear as u32,
            wrap_s: WrapMode::ClampToEdge as u32,
            wrap_t: WrapMode::ClampToEdge as u32,
        },
    };
    let inputs = ShaderInputs {
        textures: vec![binding],
        ..ShaderInputs::default()
    };

    let spirv = build_sample_centre_shader();
    let interp = Interpreter::new(&spirv).expect("interpreter must accept the shader");
    let out = interp.run_fragment(&inputs).expect("ImageSample must succeed");
    assert_eq!(out.pixels.len(), 1);
    let p = &out.pixels[0];
    for k in 0..3 {
        assert!((p[k] - 0.5).abs() < 1e-6,
            "lane {k}: expected 0.5, got {} (full pixel {:?})", p[k], p);
    }
    assert!((p[3] - 1.0).abs() < 1e-6);
}
