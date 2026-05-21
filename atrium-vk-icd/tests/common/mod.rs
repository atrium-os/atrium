//! Shared helpers for the tier-2 ICD integration tests.
//!
//! Each tier2_*.rs integration file is a separate test binary
//! (Rust's default for `tests/*.rs`), but `tests/common/mod.rs`
//! is treated as a *module* rather than a separate binary --
//! integration files include it with `mod common;`. This keeps
//! the helpers (EnvLock + SPIR-V builders + ResourceId probes)
//! in one place rather than copy-pasted across four files.

#![allow(dead_code)] // Each test file uses a subset.

use std::path::PathBuf;

pub type VkInstance       = *mut std::ffi::c_void;
pub type VkDevice         = *mut std::ffi::c_void;
pub type VkQueue          = *mut std::ffi::c_void;
pub type VkCommandBuffer  = *mut std::ffi::c_void;
pub type VkPhysicalDevice = *mut std::ffi::c_void;

/// Process-wide serialisation point for the ATRIUM_VK_ICD_SOCKET
/// env var.  Several tests need to set + read it; the global
/// lock prevents parallel cargo workers from clobbering each
/// other's setting.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub struct EnvLock {
    _g: std::sync::MutexGuard<'static, ()>,
    force_backend_was_set: bool,
}
impl EnvLock {
    pub fn set(sock: &std::path::Path) -> Self {
        let g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("ATRIUM_VK_ICD_SOCKET", sock);
        EnvLock { _g: g, force_backend_was_set: false }
    }

    /// Like `set`, but also pins atrium-spv-compile to a
    /// specific backend via ATRIUM_SPV_FORCE_BACKEND.  The
    /// var is cleared on Drop so the next test starts clean.
    pub fn set_with_force_backend(
        sock: &std::path::Path,
        backend: &str,
    ) -> Self {
        let g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        std::env::set_var("ATRIUM_VK_ICD_SOCKET", sock);
        std::env::set_var("ATRIUM_SPV_FORCE_BACKEND", backend);
        EnvLock { _g: g, force_backend_was_set: true }
    }
}
impl Drop for EnvLock {
    fn drop(&mut self) {
        if self.force_backend_was_set {
            std::env::remove_var("ATRIUM_SPV_FORCE_BACKEND");
        }
    }
}

pub fn tmp_socket(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("atrium-vk-icd-tier2-{}-{}.sock",
                   std::process::id(), name));
    p
}

/// Find the atrium-spv-compile binary that Tier2Registry
/// spawns to lower SPIR-V.  Located next to our own test
/// binary's target/debug/ tree.
pub fn locate_compile_binary() -> PathBuf {
    let here = std::env::current_exe().expect("current_exe");
    let mut p = here;
    p.pop(); p.pop(); p.pop(); p.pop(); p.pop();
    p.push("atrium-spv-compile");
    p.push("target");
    p.push("debug");
    p.push("atrium-spv-compile");
    assert!(p.exists(),
        "atrium-spv-compile not at {} -- run \
         `cd atrium-spv-compile && cargo build` first", p.display());
    p
}

/// Vertex-shader SPIR-V: reads a `vec3` position at
/// Location=0, emits `vec4(pos, 1.0)` as Position.
pub fn build_passthrough_vs() -> Vec<u8> {
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

/// Fragment-shader SPIR-V: writes `rgba` (constant) as
/// `out_color` at Location=0.
pub fn build_constant_color_fs(rgba: [f32; 4]) -> Vec<u8> {
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

/// Hand-encode a `VkShaderModuleCreateInfo` per the ICD's
/// vkCreateShaderModule docstring (sType@0, codeSize@24,
/// pCode@32) and call through to make a real shader module.
pub fn make_shader_module(device: VkDevice, spv: &[u8]) -> u64 {
    use atrium_vk_icd::vkCreateShaderModule;
    let mut info = [0u8; 40];
    info[ 0.. 4].copy_from_slice(&16u32.to_le_bytes());
    info[24..32].copy_from_slice(&(spv.len() as u64).to_le_bytes());
    info[32..40].copy_from_slice(&(spv.as_ptr() as u64).to_le_bytes());
    let mut sm: u64 = 0;
    unsafe {
        vkCreateShaderModule(device, info.as_ptr() as *const _,
                             std::ptr::null(), &mut sm);
    }
    assert!(sm != 0, "shader module create failed");
    sm
}
