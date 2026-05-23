//! Differential gate for `OpImageFetch` — closes the
//! image-op trio. Interpreter + Cranelift agree on a
//! `texelFetch(tex, ivec2(1, 0), 0)` against a 2×2 RGBW
//! checker = green. Bespoke skips for now (its ConstVec
//! emit doesn't yet handle integer-lane composites —
//! the `scalars` (V-reg) vs `ints` (W-reg) split needs a
//! small extension; tracked as a follow-on).

use atrium_spv_runtime::{
    FilterMode, SamplerDesc, TexDesc, TexFormat, WrapMode,
    DESC_SLOT_BYTES, UNIFORMS_DESC_BASE,
    descriptor_table_buffer, write_descriptor_slot, write_helper_pointers,
};
use atrium_spv_tests::harness::{
    assert_shader_agrees, InterpreterRunner, ShaderRunner,
};
use atrium_spv_tests::interpreter::{ShaderInputs, TextureBinding};
use atrium_spv_tests::pixels::ColorTolerance;

use atrium_spv_differential::{BespokeRunner, CraneliftRunner};

fn build_image_fetch_shader() -> Vec<u8> {
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
    let i32_ty = b.type_int(32, 1);
    let vec2_i32 = b.type_vector(i32_ty, 2);
    let vec4_f32 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);

    let image_ty = b.type_image(
        f32_ty, Dim::Dim2D, 0, 0, 0, 1, ImageFormat::Unknown, None);
    let ptr_uc_img = b.type_pointer(
        None, SpvStorageClass::UniformConstant, image_ty);
    let ptr_out_vec4 = b.type_pointer(
        None, SpvStorageClass::Output, vec4_f32);

    let tex = b.variable(ptr_uc_img, None, SpvStorageClass::UniformConstant, None);
    b.decorate(tex, Decoration::DescriptorSet,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(tex, Decoration::Binding,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let out = b.variable(ptr_out_vec4, None, SpvStorageClass::Output, None);
    b.decorate(out, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let c_one  = b.constant_bit32(i32_ty, 1u32);
    let c_zero = b.constant_bit32(i32_ty, 0u32);
    let coord = b.constant_composite(vec2_i32, vec![c_one, c_zero]);

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let loaded = b.load(image_ty, None, tex, None, vec![]).unwrap();
    let pixel = b.image_fetch(vec4_f32, None, loaded, coord, None, vec![]).unwrap();
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
fn image_fetch_at_one_zero() {
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
    let spirv = build_image_fetch_shader();
    let runners: [Box<dyn ShaderRunner>; 3] = [
        Box::new(InterpreterRunner),
        Box::new(CraneliftRunner::default()),
        Box::new(BespokeRunner::default()),
    ];
    let refs: Vec<&dyn ShaderRunner> = runners.iter().map(|b| b.as_ref()).collect();
    let tol = ColorTolerance::AbsEpsilon { eps: 1e-6 };
    assert_shader_agrees(&spirv, &inputs, tol, &refs);
}
