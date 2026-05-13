//! End-to-end test: Tier2Backend renders a registered
//! Tier-2 fragment shader into a backend-owned image and
//! the resulting pixels match the shader's expected output.

use std::path::PathBuf;
use std::sync::Arc;

use aqueduct_gpu::ids::{IdNamespace, ResourceId};
use aqueduct_gpu_host::{Backend, Tier2Backend, Tier2Registry};
use atrium_spv_loader::LoaderConfig;
use tempfile::TempDir;

fn locate_compile_binary() -> PathBuf {
    let here = std::env::current_exe().expect("current_exe");
    let mut p = here;
    p.pop(); p.pop(); p.pop(); p.pop(); p.pop();
    p.push("atrium-spv-compile");
    p.push("target");
    p.push("debug");
    p.push("atrium-spv-compile");
    assert!(p.exists(),
        "atrium-spv-compile binary not found at {}", p.display());
    p
}

fn build_constant_color_spirv(rgba: [f32; 4]) -> Vec<u8> {
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
    let ptr_out_vec4 = b.type_pointer(None, StorageClass::Output, vec4_f32);
    let cs: Vec<_> = rgba.iter()
        .map(|x| b.constant_bit32(f32_ty, x.to_bits())).collect();
    let color = b.constant_composite(vec4_f32, cs);
    let out = b.variable(ptr_out_vec4, None, StorageClass::Output, None);
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

#[test]
fn tier2_backend_runs_fragment_shader_into_image() {
    let cache_dir = TempDir::new().unwrap();
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    }));
    let backend = Tier2Backend::new(registry.clone());

    // Register a 64×32 image.
    let image_id = ResourceId::new(IdNamespace::IcdRuntime, 0x1000);
    backend.image_created(image_id, 64, 32);

    // Register a constant-colour fragment shader.
    let expected = [0.1f32, 0.8, 0.2, 1.0];
    let spirv = build_constant_color_spirv(expected);
    let shader_id = registry.register(&spirv).expect("register");

    // Run it into the image.
    backend.run_fragment_shader_into(image_id, shader_id, &[], &[])
        .expect("run_fragment_shader_into");

    // Read back the pixels.
    let pixels = backend.read_image_pixels(image_id)
        .expect("image must be registered");
    assert_eq!(pixels.len(), 64 * 32 * 4);
    let eq = |v: f32| (v * 255.0 + 0.5) as u8;
    let expected_u8 = [eq(expected[0]), eq(expected[1]),
                       eq(expected[2]), eq(expected[3])];
    for px in pixels.chunks_exact(4) {
        assert_eq!(px, &expected_u8[..],
            "pixel {px:?} != {expected_u8:?}");
    }
}

#[test]
fn tier2_backend_image_destroyed_drops_storage() {
    let cache_dir = TempDir::new().unwrap();
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    }));
    let backend = Tier2Backend::new(registry);
    let image_id = ResourceId::new(IdNamespace::IcdRuntime, 0x2000);
    backend.image_created(image_id, 16, 16);
    assert!(backend.read_image_pixels(image_id).is_some());
    backend.image_destroyed(image_id);
    assert!(backend.read_image_pixels(image_id).is_none());
}

#[test]
fn tier2_backend_submit_frame_counts() {
    let cache_dir = TempDir::new().unwrap();
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache_dir.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    }));
    let backend = Tier2Backend::new(registry);
    assert_eq!(backend.submission_count(), 0);
    let fence = ResourceId::new(IdNamespace::IcdRuntime, 0x3000);
    let signalled = backend.submit_frame(fence, 1, &[]);
    assert!(signalled);
    assert_eq!(backend.submission_count(), 1);
}
