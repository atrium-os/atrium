//! Interpreter `OpImageFetch` handler — closes the
//! image-op trio (sample/fetch/sample-explicit-lod).
//!
//! Builds a fragment shader that declares a `texture2D`
//! (not `sampler2D`) at binding=0 and reads texel (1, 0)
//! via `texelFetch(tex, ivec2(1, 0), 0)`. On a 2×2 RGBW
//! checkerboard that's green. No sampler involved —
//! fetch is unfiltered integer-coord lookup.

use atrium_spv_runtime::{
    FilterMode, SamplerDesc, TexFormat, WrapMode,
};
use atrium_spv_tests::interpreter::{Interpreter, ShaderInputs, TextureBinding};

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

    // texture2D = image (no sampler).
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

    // ivec2 coord = (1, 0)
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
fn interpreter_image_fetch_at_one_zero() {
    let pixels: Vec<u8> = vec![
        255,   0,   0, 255,   // (0,0) red
          0, 255,   0, 255,   // (1,0) green
          0,   0, 255, 255,   // (0,1) blue
        255, 255, 255, 255,   // (1,1) white
    ];
    // Fetch doesn't use the sampler but TextureBinding
    // requires one; Nearest/Clamp is the canonical
    // default.
    let binding = TextureBinding {
        set: 0, binding: 0,
        data: pixels, width: 2, height: 2, stride_bytes: 8,
        format: TexFormat::Rgba8Unorm as u32,
        sampler: SamplerDesc {
            mag_filter: FilterMode::Nearest as u32,
            min_filter: FilterMode::Nearest as u32,
            wrap_s: WrapMode::ClampToEdge as u32,
            wrap_t: WrapMode::ClampToEdge as u32,
        },
    };
    let inputs = ShaderInputs {
        textures: vec![binding],
        ..ShaderInputs::default()
    };

    let spirv = build_image_fetch_shader();
    let interp = Interpreter::new(&spirv)
        .expect("interpreter must accept image-fetch shader");
    let out = interp.run_fragment(&inputs)
        .expect("ImageFetch must succeed");
    assert_eq!(out.pixels.len(), 1);
    let p = out.pixels[0];
    // Texel (1, 0) on the RGBW checker = green (0, 1, 0, 1).
    let expected = [0.0_f32, 1.0, 0.0, 1.0];
    for k in 0..4 {
        assert!((p[k] - expected[k]).abs() < 1e-6,
            "lane {k}: expected {} got {} (full {:?})",
            expected[k], p[k], p);
    }
}
