//! Frontend lowering of the image / sampler ops:
//! `OpSampledImage` + `OpImageSampleImplicitLod` + `OpImageFetch`.
//!
//! Phase 1 of the texture/sampler arc gates here — the
//! frontend must produce the right IR Op variants
//! (`CombineSampledImage`, `ImageSampleImplicitLod`,
//! `ImageFetch`). Execution comes later (interpreter +
//! backends are subsequent phases); here we only verify
//! the lowering shape.

use atrium_spv_ir::Op;
use atrium_spv_frontend::translate;

/// Hand-built SPIR-V fragment shader that:
///   * declares `uniform sampler2D tex` (image + sampler
///     + sampled-image, the GLSL combined form);
///   * samples it at uv = (0.5, 0.5);
///   * stores the sampled vec4 to `out_color`.
///
/// Mirrors what `glslang` would emit for the GLSL:
///
/// ```glsl
/// #version 450
/// layout(set = 0, binding = 0) uniform sampler2D tex;
/// layout(location = 0) out vec4 out_color;
/// void main() {
///   out_color = texture(tex, vec2(0.5));
/// }
/// ```
fn build_sample_shader() -> Vec<u8> {
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

    // sampler2D = sampled-image of a 2D image of f32.
    // OpTypeImage: SampledType, Dim, Depth, Arrayed, MS,
    // Sampled (1 = with sampler), Format (Unknown is fine).
    let image_ty = b.type_image(
        f32_ty,
        Dim::Dim2D,
        /*depth=*/0,
        /*arrayed=*/0,
        /*ms=*/0,
        /*sampled=*/1,
        ImageFormat::Unknown,
        None, // access qualifier
    );
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

    // The GLSL `sampler2D` lowers to a single combined
    // OpVariable of OpTypeSampledImage. OpLoad pulls the
    // sampled-image value; OpImageSampleImplicitLod uses
    // it directly. (Some compilers emit a separate
    // OpTypeImage + OpTypeSampler + OpSampledImage to
    // combine them at use sites — we cover that shape
    // too in a second test below.)
    let sampled = b.load(sampled_image_ty, None, tex, None, vec![]).unwrap();
    let sample = b.image_sample_implicit_lod(
        vec4_f32, None, sampled, uv, None, vec![]).unwrap();
    b.store(out, sample, None, vec![]).unwrap();
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
fn frontend_lowers_combined_sampler2d() {
    let spirv = build_sample_shader();
    let module = translate(&spirv).expect("frontend must accept image-sample shader");

    // Walk the single function's blocks and confirm an
    // ImageSampleImplicitLod op is present.
    assert_eq!(module.functions.len(), 1);
    let func = &module.functions[0];
    let mut saw_sample = false;
    for block in func.blocks.values() {
        for inst in &block.insts {
            if matches!(&inst.op, Op::ImageSampleImplicitLod { .. }) {
                saw_sample = true;
            }
        }
    }
    assert!(saw_sample, "expected ImageSampleImplicitLod in the IR");
}

/// Second shape: separate `sampler` + `texture2D` (the
/// Vulkan-native split) joined via OpSampledImage at the
/// use site. This is the form `slang` and modern HLSL→SPV
/// translators emit.
fn build_split_sampler_shader() -> Vec<u8> {
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
    let sampler_ty = b.type_sampler();
    let sampled_image_ty = b.type_sampled_image(image_ty);

    let ptr_uc_img = b.type_pointer(None, SpvStorageClass::UniformConstant, image_ty);
    let ptr_uc_samp = b.type_pointer(None, SpvStorageClass::UniformConstant, sampler_ty);
    let ptr_out_vec4 = b.type_pointer(None, SpvStorageClass::Output, vec4_f32);

    let img_var = b.variable(ptr_uc_img, None, SpvStorageClass::UniformConstant, None);
    b.decorate(img_var, Decoration::DescriptorSet, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(img_var, Decoration::Binding, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let samp_var = b.variable(ptr_uc_samp, None, SpvStorageClass::UniformConstant, None);
    b.decorate(samp_var, Decoration::DescriptorSet, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(samp_var, Decoration::Binding, vec![rspirv::dr::Operand::LiteralBit32(1)]);
    let out = b.variable(ptr_out_vec4, None, SpvStorageClass::Output, None);
    b.decorate(out, Decoration::Location, vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let c_half = b.constant_bit32(f32_ty, 0.5f32.to_bits());
    let uv = b.constant_composite(vec2_f32, vec![c_half, c_half]);

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let loaded_img = b.load(image_ty, None, img_var, None, vec![]).unwrap();
    let loaded_samp = b.load(sampler_ty, None, samp_var, None, vec![]).unwrap();
    let combined = b.sampled_image(sampled_image_ty, None, loaded_img, loaded_samp).unwrap();
    let sample = b.image_sample_implicit_lod(
        vec4_f32, None, combined, uv, None, vec![]).unwrap();
    b.store(out, sample, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![img_var, samp_var, out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);

    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn frontend_lowers_split_image_and_sampler() {
    let spirv = build_split_sampler_shader();
    let module = translate(&spirv).expect("frontend must accept split image+sampler shader");

    let func = &module.functions[0];
    let mut saw_combine = false;
    let mut saw_sample = false;
    for block in func.blocks.values() {
        for inst in &block.insts {
            match &inst.op {
                Op::CombineSampledImage { .. } => saw_combine = true,
                Op::ImageSampleImplicitLod { .. } => saw_sample = true,
                _ => {}
            }
        }
    }
    assert!(saw_combine, "expected CombineSampledImage in the IR");
    assert!(saw_sample, "expected ImageSampleImplicitLod in the IR");
}
