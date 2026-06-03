//! Cross-tier equivalence probe: render the same flat colour on the Tier-2
//! software backend and the Tier-3 MoltenVK backend, then measure the
//! divergence with the certification comparator. This is the empirical
//! check behind the tier-equivalence precondition the energy router needs —
//! "do the two backends actually agree on a pixel?" — isolated to a flat
//! colour so it probes the BGRA/sRGB *convention*, not shading.
//!
//! Gated on both the Tier-2 compile toolchain (`atrium-spv-compile`) and a
//! working MoltenVK loader; skips cleanly when either is absent (Linux CI,
//! the VM). Run on Metal with `DYLD_LIBRARY_PATH=/opt/homebrew/lib`.

use std::path::PathBuf;
use std::sync::Arc;

use aqueduct_gpu::frame::FrameBuilder;
use aqueduct_gpu::ids::{IdNamespace, ResourceId};
use aqueduct_gpu::opcodes::FrameOp;
use aqueduct_gpu_host::{
    compare_framebuffers, Backend, Certification, MoltenVkBackend, Tier2Backend, Tier2Registry,
};
use atrium_spv_loader::LoaderConfig;
use tempfile::TempDir;

fn locate_compile_binary() -> Option<PathBuf> {
    let mut p = std::env::current_exe().ok()?;
    for _ in 0..5 { p.pop(); }
    p.push("atrium-spv-compile");
    p.push("target");
    p.push("debug");
    p.push("atrium-spv-compile");
    p.exists().then_some(p)
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
    let cs: Vec<_> = rgba.iter().map(|x| b.constant_bit32(f32_ty, x.to_bits())).collect();
    let color = b.constant_composite(vec4_f32, cs);
    let out = b.variable(ptr_out_vec4, None, StorageClass::Output, None);
    b.decorate(out, rspirv::spirv::Decoration::Location, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    b.store(out, color, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = b.module().assemble();
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

const W: u32 = 16;
const H: u32 = 16;
const COLOR: [u8; 4] = [179, 51, 128, 255];

/// Tier-2: a constant-colour fragment shader fills the image.
fn tier2_flat_color() -> Option<Vec<u8>> {
    let compile = locate_compile_binary()?;
    let cache = TempDir::new().ok()?;
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: compile,
    }));
    let be = Tier2Backend::new(registry.clone());
    let img = ResourceId::new(IdNamespace::IcdRuntime, 0x10);
    be.image_created(img, W, H);
    let spirv = build_constant_color_spirv(
        [COLOR[0] as f32 / 255.0, COLOR[1] as f32 / 255.0, COLOR[2] as f32 / 255.0, 1.0]);
    let sid = registry.register(&spirv).ok()?;
    let pid = ResourceId::new(IdNamespace::IcdRuntime, 0x11);
    be.bind_pipeline(pid, sid);
    let mut fb = FrameBuilder::new(1024);
    let mut begin = [0u8; 12];
    begin[..4].copy_from_slice(&img.raw().to_le_bytes());
    fb.push(FrameOp::BeginRenderPass, &begin).unwrap();
    fb.push(FrameOp::BindPipeline, &pid.raw().to_le_bytes()).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();
    be.submit_frame(ResourceId::new(IdNamespace::IcdRuntime, 0x12), 1, fb.as_bytes());
    be.read_image_pixels(img)
}

/// Tier-3: clear the image to the same colour on Metal + read it back.
fn moltenvk_flat_color() -> Option<Vec<u8>> {
    let be = MoltenVkBackend::new().ok()?;
    let img = ResourceId::new(IdNamespace::IcdRuntime, 0x20);
    let buf = ResourceId::new(IdNamespace::IcdRuntime, 0x21);
    be.image_created(img, W, H);
    be.set_image_format(img, 37); // VK_FORMAT_R8G8B8A8_UNORM
    be.buffer_created(buf, (W * H * 4) as u64);
    let mut fb = FrameBuilder::new(4096);
    let mut brp = img.raw().to_le_bytes().to_vec();
    brp.extend_from_slice(&COLOR);
    brp.extend_from_slice(&0u32.to_le_bytes());
    fb.push(FrameOp::BeginRenderPass, &brp).unwrap();
    let mut cib = img.raw().to_le_bytes().to_vec();
    cib.extend_from_slice(&buf.raw().to_le_bytes());
    cib.extend_from_slice(&0u32.to_le_bytes()); // src_layout
    cib.extend_from_slice(&1u32.to_le_bytes()); // region_count
    let mut region = vec![0u8; 56];
    region[44..48].copy_from_slice(&W.to_le_bytes());
    region[48..52].copy_from_slice(&H.to_le_bytes());
    region[52..56].copy_from_slice(&1u32.to_le_bytes());
    cib.extend_from_slice(&region);
    fb.push(FrameOp::CopyImgToBuf, &cib).unwrap();
    be.submit_frame(ResourceId::new(IdNamespace::IcdRuntime, 0x22), 1, fb.as_bytes());
    be.buffer_read_bytes(buf, 0, (W * H * 4) as u64).ok()
}

#[test]
fn tier2_and_moltenvk_agree_on_a_flat_color() {
    let (Some(px2), Some(px3)) = (tier2_flat_color(), moltenvk_flat_color()) else {
        eprintln!("cross-tier probe skipped (compile toolchain or MoltenVK unavailable)");
        return;
    };
    assert_eq!(px2.len(), px3.len(), "both render {}x{} RGBA8", W, H);
    // tolerance 1 LSB for any rounding between the CPU rasteriser and Metal.
    let result = compare_framebuffers(&px2, &px3, 1);
    eprintln!(
        "cross-tier flat-colour: tier2[0..4]={:?} moltenvk[0..4]={:?} → {:?}",
        &px2[..4], &px3[..4], result);
    assert_eq!(result, Certification::Certified,
        "Tier-2 and MoltenVK must agree on a flat colour (the tier-equivalence \
         convention precondition); got {result:?} — tier2={:?} mvk={:?}",
        &px2[..4], &px3[..4]);
}
