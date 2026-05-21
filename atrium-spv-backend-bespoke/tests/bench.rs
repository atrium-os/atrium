//! Microbenchmark: bespoke vs Cranelift compile time + code
//! size for the verified compute shader patterns.
//!
//! The bespoke backend's reason to exist is perf -- hand-tuned
//! ARM64 codegen to claw back what Cranelift's general-purpose
//! regalloc costs.  This test runs the same SPIR-V through
//! both backends, times each, and prints the comparison.
//!
//! Not assertive about which backend is faster (Cranelift has
//! O(n) constant overhead from JIT init that dominates on tiny
//! shaders, where bespoke's per-instruction emission is the
//! constant overhead -- the curves cross at some shader size).
//! The point is to make the numbers visible.
//!
//! Run with `cargo test --test bench -- --nocapture` to see
//! the printout.

use atrium_spv_backend_bespoke::{compile_blob as bespoke_compile, Target as BespokeTarget};
use atrium_spv_backend_cranelift::{compile_blob as cranelift_compile, Target as CraneliftTarget};
use atrium_spv_frontend::translate;
use std::time::Instant;

fn build_empty_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, ExecutionMode, ExecutionModel,
        FunctionControl, MemoryModel,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let void_fn = b.type_function(void, vec![]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![]);
    b.execution_mode(main, ExecutionMode::LocalSize, [4u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

fn build_ssbo_vec4_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let f32_ty = b.type_float(32, None);
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let ssbo_struct = b.type_struct(vec![vec4]);
    b.decorate(ssbo_struct, Decoration::Block, vec![]);
    b.member_decorate(ssbo_struct, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_ssbo_struct = b.type_pointer(None, StorageClass::StorageBuffer, ssbo_struct);
    let ptr_ssbo_vec4 = b.type_pointer(None, StorageClass::StorageBuffer, vec4);
    let ssbo_var = b.variable(ptr_ssbo_struct, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo_var, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo_var, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let cs: Vec<_> = [1.0f32, 2.0, 3.0, 4.0].iter()
        .map(|x| b.constant_bit32(f32_ty, x.to_bits())).collect();
    let c_vec = b.constant_composite(vec4, cs);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let dst = b.access_chain(ptr_ssbo_vec4, None, ssbo_var, vec![c_zero]).unwrap();
    b.store(dst, c_vec, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![ssbo_var]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

fn build_gid_cs_local_4() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let uvec3 = b.type_vector(u32_ty, 3);
    let void_fn = b.type_function(void, vec![]);
    let ptr_in_uvec3 = b.type_pointer(None, StorageClass::Input, uvec3);
    let gid_var = b.variable(ptr_in_uvec3, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let ssbo_struct = b.type_struct(vec![u32_ty]);
    b.decorate(ssbo_struct, Decoration::Block, vec![]);
    b.member_decorate(ssbo_struct, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_ssbo_struct = b.type_pointer(None, StorageClass::StorageBuffer, ssbo_struct);
    let ptr_ssbo_u32 = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo_var = b.variable(ptr_ssbo_struct, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo_var, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo_var, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let gid = b.load(uvec3, None, gid_var, None, vec![]).unwrap();
    let gid_x = b.composite_extract(u32_ty, None, gid, vec![0]).unwrap();
    let dst = b.access_chain(ptr_ssbo_u32, None, ssbo_var, vec![c_zero]).unwrap();
    b.store(dst, gid_x, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main",
        vec![gid_var, ssbo_var]);
    b.execution_mode(main, ExecutionMode::LocalSize, [4u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

struct BackendStats {
    compile_us: u128,
    blob_bytes: usize,
}

fn compile_both(spv: &[u8]) -> (BackendStats, BackendStats) {
    let module = translate(spv).expect("frontend translate");

    let bespoke_target = if cfg!(target_os = "macos") {
        BespokeTarget::Aarch64Darwin
    } else {
        BespokeTarget::Aarch64FreeBSD
    };
    let cranelift_target = if cfg!(target_os = "macos") {
        CraneliftTarget::Aarch64Darwin
    } else {
        CraneliftTarget::Aarch64FreeBSD
    };

    // Warm-up + median-of-3 for stability.  Both backends
    // are deterministic; variance comes from system noise.
    let mut bespoke_times = Vec::new();
    let mut bespoke_size = 0;
    for _ in 0..3 {
        let t0 = Instant::now();
        let out = bespoke_compile(&module, bespoke_target).expect("bespoke");
        bespoke_times.push(t0.elapsed().as_micros());
        bespoke_size = out.blob.len();
    }
    bespoke_times.sort();
    let bespoke_stats = BackendStats {
        compile_us: bespoke_times[1], // median
        blob_bytes: bespoke_size,
    };

    let mut cranelift_times = Vec::new();
    let mut cranelift_size = 0;
    for _ in 0..3 {
        let t0 = Instant::now();
        let out = cranelift_compile(&module, cranelift_target).expect("cranelift");
        cranelift_times.push(t0.elapsed().as_micros());
        cranelift_size = out.blob.len();
    }
    cranelift_times.sort();
    let cranelift_stats = BackendStats {
        compile_us: cranelift_times[1],
        blob_bytes: cranelift_size,
    };

    (bespoke_stats, cranelift_stats)
}

#[test]
fn compare_bespoke_vs_cranelift_compile() {
    if !cfg!(target_arch = "aarch64") {
        return;
    }
    let shaders: Vec<(&'static str, Vec<u8>)> = vec![
        ("empty",           build_empty_cs()),
        ("ssbo_vec4_write", build_ssbo_vec4_cs()),
        ("gid_ls4",         build_gid_cs_local_4()),
    ];

    println!("\n  shader            | bespoke compile | bespoke size | cranelift compile | cranelift size | bespoke speedup");
    println!(  "  ------------------|-----------------|--------------|-------------------|----------------|----------------");
    for (name, spv) in &shaders {
        let (b, c) = compile_both(spv);
        let speedup = c.compile_us as f64 / b.compile_us.max(1) as f64;
        println!("  {:18}| {:11} us | {:8} bytes | {:13} us | {:10} bytes | {:.2}x",
                 name, b.compile_us, b.blob_bytes,
                 c.compile_us, c.blob_bytes, speedup);

        // Both backends should produce non-empty output.
        assert!(b.blob_bytes > 0, "{name}: bespoke produced empty blob");
        assert!(c.blob_bytes > 0, "{name}: cranelift produced empty blob");
    }
    println!();
}
