//! End-to-end pixel-correctness: the bespoke ARM64 backend
//! compiles a real SPIR-V constant-colour fragment shader,
//! links into a `.dylib`/`.so`, dlopens, and the resulting
//! `atrium_fs_main` writes the expected RGBA into the
//! output pointer. First proof that the bespoke perf-path
//! produces runnable code.

use std::process::Command;

use atrium_spv_backend_bespoke::{compile, Target};
use atrium_spv_frontend::translate;
use tempfile::TempDir;

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

fn link_to_shared_library(
    obj_path: &std::path::Path,
    lib_path: &std::path::Path,
) -> std::io::Result<()> {
    let flag = if cfg!(target_os = "macos") { "-dynamiclib" } else { "-shared" };
    let status = Command::new("cc")
        .arg(flag)
        .arg("-o").arg(lib_path)
        .arg(obj_path)
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!(
            "cc {flag} failed: {status}")));
    }
    Ok(())
}

/// Build `vec4(a+b, a-b, a*b, a/b)` and store. Exercises
/// all four scalar f32 arith ops through the bespoke
/// pipeline.
fn build_arith_shader(a: f32, b: f32) -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, ExecutionMode, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass,
    };
    let mut bld = rspirv::dr::Builder::new();
    bld.set_version(1, 0);
    bld.capability(Capability::Shader);
    bld.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = bld.type_void();
    let f32_ty = bld.type_float(32, None);
    let vec4 = bld.type_vector(f32_ty, 4);
    let void_fn = bld.type_function(void, vec![]);
    let ptr_out = bld.type_pointer(None, StorageClass::Output, vec4);
    let ca = bld.constant_bit32(f32_ty, a.to_bits());
    let cb = bld.constant_bit32(f32_ty, b.to_bits());
    let out = bld.variable(ptr_out, None, StorageClass::Output, None);
    bld.decorate(out, rspirv::spirv::Decoration::Location,
                 vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = bld.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    bld.begin_block(None).unwrap();
    let sum  = bld.f_add(f32_ty, None, ca, cb).unwrap();
    let diff = bld.f_sub(f32_ty, None, ca, cb).unwrap();
    let prod = bld.f_mul(f32_ty, None, ca, cb).unwrap();
    let quot = bld.f_div(f32_ty, None, ca, cb).unwrap();
    let color = bld.composite_construct(vec4, None,
        vec![sum, diff, prod, quot]).unwrap();
    bld.store(out, color, None, vec![]).unwrap();
    bld.ret().unwrap();
    bld.end_function().unwrap();
    bld.entry_point(ExecutionModel::Fragment, main, "main", vec![out]);
    bld.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = bld.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

/// Build a chain of N FAdds: `acc = a; for _ in 0..N { acc = acc + b; }`.
/// Result is `a + N*b`. With a bump V-reg allocator this
/// would burn N+1 regs (panic past 16). Linear-scan
/// recycles each intermediate's V-reg right after the
/// next add consumes it, keeping live-count at 3.
fn build_long_add_chain_shader(a: f32, b: f32, n: usize) -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, ExecutionMode, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass,
    };
    let mut bld = rspirv::dr::Builder::new();
    bld.set_version(1, 0);
    bld.capability(Capability::Shader);
    bld.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = bld.type_void();
    let f32_ty = bld.type_float(32, None);
    let vec4 = bld.type_vector(f32_ty, 4);
    let void_fn = bld.type_function(void, vec![]);
    let ptr_out = bld.type_pointer(None, StorageClass::Output, vec4);
    let ca = bld.constant_bit32(f32_ty, a.to_bits());
    let cb = bld.constant_bit32(f32_ty, b.to_bits());
    let c1 = bld.constant_bit32(f32_ty, 1.0f32.to_bits());
    let out = bld.variable(ptr_out, None, StorageClass::Output, None);
    bld.decorate(out, rspirv::spirv::Decoration::Location,
                 vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = bld.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    bld.begin_block(None).unwrap();
    let mut acc = ca;
    for _ in 0..n {
        acc = bld.f_add(f32_ty, None, acc, cb).unwrap();
    }
    let color = bld.composite_construct(vec4, None,
        vec![acc, acc, acc, c1]).unwrap();
    bld.store(out, color, None, vec![]).unwrap();
    bld.ret().unwrap();
    bld.end_function().unwrap();
    bld.entry_point(ExecutionModel::Fragment, main, "main", vec![out]);
    bld.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = bld.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn bespoke_shader_runs_long_add_chain_through_linear_scan_ra() {
    // 20 chained adds — would blow a bump allocator past
    // 16 regs. Linear-scan recycles each intermediate.
    let (a, b, n) = (0.1f32, 0.05f32, 20usize);
    let spirv = build_long_add_chain_shader(a, b, n);
    let module = translate(&spirv).expect("translate");
    let out = compile(&module, Target::host()).expect("bespoke compile");
    let dir = TempDir::new().unwrap();
    let obj_path = dir.path().join("shader.o");
    std::fs::write(&obj_path, &out.object).unwrap();
    let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
    let lib_path = dir.path().join(format!("shader.{ext}"));
    link_to_shared_library(&obj_path, &lib_path).unwrap();
    let lib = unsafe { libloading::Library::new(&lib_path).unwrap() };
    type FsMain = unsafe extern "C" fn(
        *const u8, *const u8, *const u8,
        f32, f32, f32, f32, u32,
        *mut f32, *mut f32,
    );
    let fs_main: libloading::Symbol<FsMain> = unsafe {
        lib.get(b"atrium_fs_main").unwrap()
    };
    let mut out_color = [0.0f32; 4];
    let mut out_depth = 0.0f32;
    unsafe {
        fs_main(
            std::ptr::null(), std::ptr::null(), std::ptr::null(),
            0.0, 0.0, 0.0, 0.0, 0,
            out_color.as_mut_ptr(), &mut out_depth,
        );
    }
    let expected_acc = a + b * (n as f32);
    let expected = [expected_acc, expected_acc, expected_acc, 1.0];
    assert_eq!(out_color, expected,
        "long-add-chain got {out_color:?}, expected {expected:?}");
}

/// Build a shader that reads a single `float scale` from
/// a push-constant block at offset 0 and writes
/// `vec4(scale, scale * 0.5, 0.25, 1.0)`. Exercises
/// AccessChain + Load + FMul through the bespoke pipeline.
fn build_pushconst_scale_shader() -> Vec<u8> {
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
    let i32_ty = b.type_int(32, 1);
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let pc_struct = b.type_struct(vec![f32_ty]);
    b.member_decorate(pc_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(pc_struct, Decoration::Block, vec![]);
    let ptr_pc_struct = b.type_pointer(None, StorageClass::PushConstant, pc_struct);
    let ptr_pc_f32    = b.type_pointer(None, StorageClass::PushConstant, f32_ty);
    let ptr_out_vec4  = b.type_pointer(None, StorageClass::Output, vec4);
    let zero_i = b.constant_bit32(i32_ty, 0u32);
    let c025 = b.constant_bit32(f32_ty, 0.25f32.to_bits());
    let c05  = b.constant_bit32(f32_ty, 0.5f32.to_bits());
    let c1   = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let pc_var = b.variable(ptr_pc_struct, None, StorageClass::PushConstant, None);
    let out    = b.variable(ptr_out_vec4,  None, StorageClass::Output,       None);
    b.decorate(out, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let scale_ptr = b.access_chain(ptr_pc_f32, None, pc_var, vec![zero_i]).unwrap();
    let scale = b.load(f32_ty, None, scale_ptr, None, vec![]).unwrap();
    let half = b.f_mul(f32_ty, None, scale, c05).unwrap();
    let color = b.composite_construct(vec4, None,
        vec![scale, half, c025, c1]).unwrap();
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
fn bespoke_shader_reads_push_constant_and_does_arith() {
    let spirv = build_pushconst_scale_shader();
    let module = translate(&spirv).expect("translate");
    let out = compile(&module, Target::host()).expect("bespoke compile");
    let dir = TempDir::new().unwrap();
    let obj_path = dir.path().join("shader.o");
    std::fs::write(&obj_path, &out.object).unwrap();
    let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
    let lib_path = dir.path().join(format!("shader.{ext}"));
    link_to_shared_library(&obj_path, &lib_path).unwrap();
    let lib = unsafe { libloading::Library::new(&lib_path).unwrap() };
    type FsMain = unsafe extern "C" fn(
        *const u8, *const u8, *const u8,
        f32, f32, f32, f32, u32,
        *mut f32, *mut f32,
    );
    let fs_main: libloading::Symbol<FsMain> = unsafe {
        lib.get(b"atrium_fs_main").unwrap()
    };
    let scale_value = 0.6f32;
    let mut pc_buf = [0u8; 4];
    pc_buf.copy_from_slice(&scale_value.to_le_bytes());
    let mut out_color = [0.0f32; 4];
    let mut out_depth = 0.0f32;
    unsafe {
        fs_main(
            std::ptr::null(), std::ptr::null(), pc_buf.as_ptr(),
            0.0, 0.0, 0.0, 0.0, 0,
            out_color.as_mut_ptr(), &mut out_depth,
        );
    }
    let expected = [scale_value, scale_value * 0.5, 0.25, 1.0];
    assert_eq!(out_color, expected,
        "bespoke push-const + arith got {out_color:?}, expected {expected:?}");
}

/// Two-block straight-line CFG: entry → merge. Tests the
/// branch-relocation machinery and cross-block live-range
/// handling without needing conditionals or phis.
fn build_two_block_shader() -> Vec<u8> {
    // We can't easily get the SPIR-V builder to emit a
    // bare entry-block-with-OpBranch in a fragment shader
    // (every shader is one function), so we build the IR
    // module directly and skip the frontend.
    let _ = ();
    Vec::new()
}

#[test]
fn bespoke_shader_runs_two_block_branch() {
    use atrium_spv_ir::{
        Block, BlockId, BlockKind, EntryPoint, FloatKind, Function, Inst,
        Module, Op, ShaderStage, StorageClass, Type, Value, ValueId,
    };
    use std::collections::HashMap;
    let _ = build_two_block_shader();

    // Build IR by hand:
    //   block 0 (entry):
    //     v0 = ConstFloat 0.4
    //     v1 = ConstFloat 0.5
    //     v2 = ConstFloat 0.6
    //     v3 = ConstFloat 1.0
    //     Branch -> block 1
    //   block 1 (merge):
    //     v4 = ConstVec [v0, v1, v2, v3]
    //     Store(out_var, v4)
    //     Return
    let f32_ty = Type::F32;
    let vec4_ty = Type::Vec4(atrium_spv_ir::VecElement::F32);
    let out_ptr_ty = Type::Pointer(StorageClass::Output, Box::new(vec4_ty.clone()));

    let v0 = Value { id: ValueId(0), ty: f32_ty.clone() };
    let v1 = Value { id: ValueId(1), ty: f32_ty.clone() };
    let v2 = Value { id: ValueId(2), ty: f32_ty.clone() };
    let v3 = Value { id: ValueId(3), ty: f32_ty.clone() };
    let v4 = Value { id: ValueId(4), ty: vec4_ty.clone() };
    let out_v = Value { id: ValueId(5), ty: out_ptr_ty.clone() };
    let mk_inst = |op, result, off: u32| Inst { op, result, source_spirv_offset: off };

    let entry = Block {
        id: BlockId(0),
        kind: BlockKind::Linear,
        insts: vec![
            mk_inst(Op::ConstFloat { value: 0.4, kind: FloatKind::F32 }, Some(v0.clone()), 1),
            mk_inst(Op::ConstFloat { value: 0.5, kind: FloatKind::F32 }, Some(v1.clone()), 2),
            mk_inst(Op::ConstFloat { value: 0.6, kind: FloatKind::F32 }, Some(v2.clone()), 3),
            mk_inst(Op::ConstFloat { value: 1.0, kind: FloatKind::F32 }, Some(v3.clone()), 4),
            mk_inst(Op::Branch(BlockId(1)), None, 5),
        ],
    };
    let merge = Block {
        id: BlockId(1),
        kind: BlockKind::Linear,
        insts: vec![
            mk_inst(Op::ConstVec(vec![v0, v1, v2, v3]), Some(v4.clone()), 6),
            mk_inst(Op::Store { ptr: out_v, value: v4 }, None, 7),
            mk_inst(Op::Return, None, 8),
        ],
    };
    let mut blocks = HashMap::new();
    blocks.insert(entry.id, entry);
    blocks.insert(merge.id, merge);

    let module = Module {
        functions: vec![Function {
            name: "main".to_string(),
            stage: ShaderStage::Fragment,
            params: Vec::new(),
            return_type: Type::Void,
            entry_block: BlockId(0),
            blocks,
            local_size: None,
            ssbo_bindings: std::collections::HashMap::new(),
            workgroup_size: 0,
            workgroup_var_offset: std::collections::HashMap::new(),
        }],
        entry_points: vec![EntryPoint {
            stage: ShaderStage::Fragment,
            function_index: 0,
            name: "main".to_string(),
        }],
        uniforms: Vec::new(),
        push_constants_size: 0,
        vertex_inputs: Vec::new(),
        varyings: Vec::new(),
    };

    let out = compile(&module, Target::host()).expect("bespoke compile");
    let dir = TempDir::new().unwrap();
    let obj_path = dir.path().join("shader.o");
    std::fs::write(&obj_path, &out.object).unwrap();
    let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
    let lib_path = dir.path().join(format!("shader.{ext}"));
    link_to_shared_library(&obj_path, &lib_path).unwrap();
    let lib = unsafe { libloading::Library::new(&lib_path).unwrap() };
    type FsMain = unsafe extern "C" fn(
        *const u8, *const u8, *const u8,
        f32, f32, f32, f32, u32,
        *mut f32, *mut f32,
    );
    let fs_main: libloading::Symbol<FsMain> = unsafe {
        lib.get(b"atrium_fs_main").unwrap()
    };
    let mut out_color = [0.0f32; 4];
    let mut out_depth = 0.0f32;
    unsafe {
        fs_main(
            std::ptr::null(), std::ptr::null(), std::ptr::null(),
            0.0, 0.0, 0.0, 0.0, 0,
            out_color.as_mut_ptr(), &mut out_depth,
        );
    }
    assert_eq!(out_color, [0.4, 0.5, 0.6, 1.0],
        "two-block bespoke shader produced {out_color:?}");
}

/// Build a real if/else shader read by the frontend's
/// CFG path: `if (scale < 0.5) out = red; else out = blue;`
/// where `scale` comes from push-constants. Exercises
/// AccessChain + Load + FOrdLt + BranchCond + multi-
/// block CFG + branch relocations all together.
fn build_if_else_shader() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, SelectionControl,
        StorageClass,
    };

    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);

    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let bool_ty = b.type_bool();
    let i32_ty = b.type_int(32, 1);
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);

    let pc_struct = b.type_struct(vec![f32_ty]);
    b.member_decorate(pc_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(pc_struct, Decoration::Block, vec![]);

    let ptr_pc_struct = b.type_pointer(None, StorageClass::PushConstant, pc_struct);
    let ptr_pc_f32    = b.type_pointer(None, StorageClass::PushConstant, f32_ty);
    let ptr_out       = b.type_pointer(None, StorageClass::Output, vec4);

    let zero_i = b.constant_bit32(i32_ty, 0u32);
    let c0  = b.constant_bit32(f32_ty, 0.0f32.to_bits());
    let c05 = b.constant_bit32(f32_ty, 0.5f32.to_bits());
    let c1  = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let red   = b.constant_composite(vec4, vec![c1, c0, c0, c1]);
    let blue  = b.constant_composite(vec4, vec![c0, c0, c1, c1]);

    let pc_var = b.variable(ptr_pc_struct, None, StorageClass::PushConstant, None);
    let out    = b.variable(ptr_out,        None, StorageClass::Output,       None);
    b.decorate(out, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let p = b.access_chain(ptr_pc_f32, None, pc_var, vec![zero_i]).unwrap();
    let v = b.load(f32_ty, None, p, None, vec![]).unwrap();
    let cond = b.f_ord_less_than(bool_ty, None, v, c05).unwrap();
    let then_id = b.id();
    let else_id = b.id();
    let merge_id = b.id();
    b.selection_merge(merge_id, SelectionControl::NONE).unwrap();
    b.branch_conditional(cond, then_id, else_id, vec![]).unwrap();

    b.begin_block(Some(then_id)).unwrap();
    b.store(out, red, None, vec![]).unwrap();
    b.branch(merge_id).unwrap();

    b.begin_block(Some(else_id)).unwrap();
    b.store(out, blue, None, vec![]).unwrap();
    b.branch(merge_id).unwrap();

    b.begin_block(Some(merge_id)).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();

    b.entry_point(ExecutionModel::Fragment, main, "main", vec![out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);

    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

fn run_bespoke_if_else(scale: f32) -> [f32; 4] {
    let spirv = build_if_else_shader();
    let module = translate(&spirv).expect("translate");
    let out = compile(&module, Target::host()).expect("bespoke compile");
    let dir = TempDir::new().unwrap();
    let obj_path = dir.path().join("shader.o");
    std::fs::write(&obj_path, &out.object).unwrap();
    let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
    let lib_path = dir.path().join(format!("shader.{ext}"));
    link_to_shared_library(&obj_path, &lib_path).unwrap();
    let lib = unsafe { libloading::Library::new(&lib_path).unwrap() };
    type FsMain = unsafe extern "C" fn(
        *const u8, *const u8, *const u8,
        f32, f32, f32, f32, u32,
        *mut f32, *mut f32,
    );
    let fs_main: libloading::Symbol<FsMain> = unsafe {
        lib.get(b"atrium_fs_main").unwrap()
    };
    let mut pc_buf = [0u8; 4];
    pc_buf.copy_from_slice(&scale.to_le_bytes());
    let mut out_color = [0.0f32; 4];
    let mut out_depth = 0.0f32;
    unsafe {
        fs_main(
            std::ptr::null(), std::ptr::null(), pc_buf.as_ptr(),
            0.0, 0.0, 0.0, 0.0, 0,
            out_color.as_mut_ptr(), &mut out_depth,
        );
    }
    out_color
}

#[test]
fn bespoke_shader_runs_if_else_then_branch() {
    let out = run_bespoke_if_else(0.2);
    assert_eq!(out, [1.0, 0.0, 0.0, 1.0],
        "0.2 < 0.5 must take then branch (red), got {out:?}");
}

#[test]
fn bespoke_shader_runs_if_else_else_branch() {
    let out = run_bespoke_if_else(0.8);
    assert_eq!(out, [0.0, 0.0, 1.0, 1.0],
        "0.8 >= 0.5 must take else branch (blue), got {out:?}");
}

#[test]
fn bespoke_shader_runs_fp_arithmetic() {
    let (a, b) = (0.75f32, 0.25f32);
    let spirv = build_arith_shader(a, b);
    let module = translate(&spirv).expect("translate");
    let out = compile(&module, Target::host()).expect("bespoke compile");

    let dir = TempDir::new().unwrap();
    let obj_path = dir.path().join("shader.o");
    std::fs::write(&obj_path, &out.object).unwrap();
    let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
    let lib_path = dir.path().join(format!("shader.{ext}"));
    link_to_shared_library(&obj_path, &lib_path).unwrap();
    let lib = unsafe { libloading::Library::new(&lib_path).unwrap() };
    type FsMain = unsafe extern "C" fn(
        *const u8, *const u8, *const u8,
        f32, f32, f32, f32, u32,
        *mut f32, *mut f32,
    );
    let fs_main: libloading::Symbol<FsMain> = unsafe {
        lib.get(b"atrium_fs_main").unwrap()
    };
    let mut out_color = [0.0f32; 4];
    let mut out_depth = 0.0f32;
    unsafe {
        fs_main(
            std::ptr::null(), std::ptr::null(), std::ptr::null(),
            0.0, 0.0, 0.0, 0.0, 0,
            out_color.as_mut_ptr(), &mut out_depth,
        );
    }
    let expected = [a + b, a - b, a * b, a / b];
    assert_eq!(out_color, expected,
        "bespoke FP arith wrong: got {out_color:?}, expected {expected:?}");
}

#[test]
fn bespoke_shader_dlopens_and_writes_constant_color() {
    let rgba = [0.25f32, 0.5, 0.75, 1.0];
    let spirv = build_constant_color_spirv(rgba);

    let module = translate(&spirv).expect("frontend translate");
    let out = compile(&module, Target::host()).expect("bespoke compile");

    let dir = TempDir::new().unwrap();
    let obj_path = dir.path().join("shader.o");
    std::fs::write(&obj_path, &out.object).unwrap();
    let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
    let lib_path = dir.path().join(format!("shader.{ext}"));
    link_to_shared_library(&obj_path, &lib_path).unwrap();

    let lib = unsafe { libloading::Library::new(&lib_path).unwrap() };
    type FsMain = unsafe extern "C" fn(
        *const u8, *const u8, *const u8,
        f32, f32, f32, f32, u32,
        *mut f32, *mut f32,
    );
    let fs_main: libloading::Symbol<FsMain> = unsafe {
        lib.get(b"atrium_fs_main").unwrap()
    };
    let mut out_color = [0.0f32; 4];
    let mut out_depth = 0.0f32;
    unsafe {
        fs_main(
            std::ptr::null(), std::ptr::null(), std::ptr::null(),
            0.0, 0.0, 0.0, 0.0, 0,
            out_color.as_mut_ptr(), &mut out_depth,
        );
    }
    assert_eq!(out_color, rgba,
        "bespoke shader wrong: got {out_color:?}, expected {rgba:?}");
}

// ---------------------------------------------------------------------
// Matrix arc phase 4 — bespoke ARM64 lowering of OpMatrixTimesVector.
//
// Same SPIR-V shape phase 3 (Cranelift) tested, now driven through the
// bespoke backend's per-lane scalar fmul/fadd path.  v1 emits 4 fmul +
// 12 (fmul, fadd) tmp pairs = 16 scalar fmul + 12 fadd, all on S-regs.
// v2 (later) will extend the NEON pack classifier to recognise the
// MatrixTimesVector chain and emit `fmul.4s / fadd.4s` instead.

fn build_mvp_vertex_shader() -> Vec<u8> {
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
    let vec3 = b.type_vector(f32_ty, 3);
    let vec4 = b.type_vector(f32_ty, 4);
    let mat4 = b.type_matrix(vec4, 4);
    let void_fn = b.type_function(void, vec![]);

    let per_vertex_struct = b.type_struct(vec![vec4]);
    b.member_decorate(per_vertex_struct, 0, Decoration::BuiltIn,
                      vec![rspirv::dr::Operand::BuiltIn(BuiltIn::Position)]);
    b.member_decorate(per_vertex_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(per_vertex_struct, Decoration::Block, vec![]);

    let ub_struct = b.type_struct(vec![mat4]);
    b.member_decorate(ub_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.member_decorate(ub_struct, 0, Decoration::MatrixStride,
                      vec![rspirv::dr::Operand::LiteralBit32(16)]);
    b.member_decorate(ub_struct, 0, Decoration::ColMajor, vec![]);
    b.decorate(ub_struct, Decoration::Block, vec![]);

    let ptr_pv_struct = b.type_pointer(None, SpvStorageClass::Output, per_vertex_struct);
    let ptr_out_vec4  = b.type_pointer(None, SpvStorageClass::Output, vec4);
    let ptr_ub_struct = b.type_pointer(None, SpvStorageClass::Uniform, ub_struct);
    let ptr_ub_mat4   = b.type_pointer(None, SpvStorageClass::Uniform, mat4);
    let ptr_in_vec3   = b.type_pointer(None, SpvStorageClass::Input, vec3);

    let in_pos = b.variable(ptr_in_vec3, None, SpvStorageClass::Input, None);
    b.decorate(in_pos, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let pv_var = b.variable(ptr_pv_struct, None, SpvStorageClass::Output, None);
    let ub = b.variable(ptr_ub_struct, None, SpvStorageClass::Uniform, None);
    b.decorate(ub, Decoration::DescriptorSet,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ub, Decoration::Binding,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);

    let c_zero  = b.constant_bit32(i32_ty, 0u32);
    let c_one_f = b.constant_bit32(f32_ty, 1.0f32.to_bits());

    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let pos3 = b.load(vec3, None, in_pos, None, vec![]).unwrap();
    let x = b.composite_extract(f32_ty, None, pos3, vec![0]).unwrap();
    let y = b.composite_extract(f32_ty, None, pos3, vec![1]).unwrap();
    let z = b.composite_extract(f32_ty, None, pos3, vec![2]).unwrap();
    let pos4 = b.composite_construct(vec4, None, vec![x, y, z, c_one_f]).unwrap();
    let mvp_ptr = b.access_chain(ptr_ub_mat4, None, ub, vec![c_zero]).unwrap();
    let mvp = b.load(mat4, None, mvp_ptr, None, vec![]).unwrap();
    let transformed = b.matrix_times_vector(vec4, None, mvp, pos4).unwrap();
    let dst_ptr = b.access_chain(ptr_out_vec4, None, pv_var, vec![c_zero]).unwrap();
    b.store(dst_ptr, transformed, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Vertex, main, "main",
                  vec![in_pos, pv_var, ub]);

    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

fn pack_mat4(m: [[f32; 4]; 4]) -> Vec<u8> {
    let mut b = Vec::with_capacity(64);
    for col in m {
        for f in col { b.extend_from_slice(&f.to_le_bytes()); }
    }
    b
}

fn pack_vec3(v: [f32; 3]) -> Vec<u8> {
    let mut b = Vec::with_capacity(12);
    for f in v { b.extend_from_slice(&f.to_le_bytes()); }
    b
}

/// Drive the bespoke-compiled vertex shader through dlopen and assert
/// the gl_Position output matches the expected MVP-transformed value.
/// Mirrors `cranelift_shader_runs_matrix_times_vector` in
/// atrium-spv-backend-cranelift/tests/dlopen_and_run.rs.
///
/// Bespoke must run on an aarch64 host (the emitted machine code IS
/// AArch64).  Skipped silently on non-arm64.
#[test]
fn bespoke_shader_runs_matrix_times_vector() {
    if !cfg!(target_arch = "aarch64") {
        eprintln!("bespoke matrix test: host is not aarch64, skipping");
        return;
    }

    let (tx, ty, tz) = (10.0f32, 20.0, 30.0);
    let mvp = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [tx,  ty,  tz,  1.0],
    ];
    let pos = [0.5f32, 1.5, 2.5];

    let spirv = build_mvp_vertex_shader();
    let module = translate(&spirv).expect("frontend translate");

    let object_bytes = compile(&module, Target::host())
        .expect("backend compile").object;
    let dir = TempDir::new().unwrap();
    let obj_path = dir.path().join("bespoke_mvp.o");
    std::fs::write(&obj_path, &object_bytes).unwrap();
    let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
    let lib_path = dir.path().join(format!("bespoke_mvp.{ext}"));
    link_to_shared_library(&obj_path, &lib_path).unwrap();

    let lib = unsafe { libloading::Library::new(&lib_path).unwrap() };
    type VsMain = unsafe extern "C" fn(
        *const u8, *const u8, *const u8, *const u8,
        u32, u32,
        *mut f32, *mut u8, *mut f32,
    );
    let vs_main: libloading::Symbol<VsMain> = unsafe {
        lib.get(b"atrium_vs_main").unwrap()
    };

    let attr_bytes = pack_vec3(pos);
    let ubo_bytes  = pack_mat4(mvp);
    let mut out_position = [0.0f32; 4];
    let mut out_varyings = [0u8; 256];
    let mut out_clip_distance = [0.0f32; 8];
    unsafe {
        vs_main(
            attr_bytes.as_ptr(),
            std::ptr::null(),
            ubo_bytes.as_ptr(),
            std::ptr::null(),
            0, 0,
            out_position.as_mut_ptr(),
            out_varyings.as_mut_ptr(),
            out_clip_distance.as_mut_ptr(),
        );
    }

    let expected = [pos[0] + tx, pos[1] + ty, pos[2] + tz, 1.0f32];
    for k in 0..4 {
        assert!((out_position[k] - expected[k]).abs() < 1e-6,
            "bespoke MVP lane {k}: expected {} got {} (full {:?})",
            expected[k], out_position[k], out_position);
    }
}

#[test]
fn bespoke_shader_runs_matrix_times_vector_scale() {
    if !cfg!(target_arch = "aarch64") {
        eprintln!("bespoke matrix test: host is not aarch64, skipping");
        return;
    }
    let mvp = [
        [2.0, 0.0, 0.0, 0.0],
        [0.0, 3.0, 0.0, 0.0],
        [0.0, 0.0, 4.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];
    let pos = [0.25f32, 0.5, -0.75];

    let spirv = build_mvp_vertex_shader();
    let module = translate(&spirv).expect("frontend translate");
    let object_bytes = compile(&module, Target::host())
        .expect("backend compile").object;
    let dir = TempDir::new().unwrap();
    let obj_path = dir.path().join("bespoke_mvp_scale.o");
    std::fs::write(&obj_path, &object_bytes).unwrap();
    let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
    let lib_path = dir.path().join(format!("bespoke_mvp_scale.{ext}"));
    link_to_shared_library(&obj_path, &lib_path).unwrap();

    let lib = unsafe { libloading::Library::new(&lib_path).unwrap() };
    type VsMain = unsafe extern "C" fn(
        *const u8, *const u8, *const u8, *const u8,
        u32, u32,
        *mut f32, *mut u8, *mut f32,
    );
    let vs_main: libloading::Symbol<VsMain> = unsafe {
        lib.get(b"atrium_vs_main").unwrap()
    };

    let attr_bytes = pack_vec3(pos);
    let ubo_bytes  = pack_mat4(mvp);
    let mut out_position = [0.0f32; 4];
    let mut out_varyings = [0u8; 256];
    let mut out_clip_distance = [0.0f32; 8];
    unsafe {
        vs_main(
            attr_bytes.as_ptr(),
            std::ptr::null(),
            ubo_bytes.as_ptr(),
            std::ptr::null(),
            0, 0,
            out_position.as_mut_ptr(),
            out_varyings.as_mut_ptr(),
            out_clip_distance.as_mut_ptr(),
        );
    }
    let expected = [pos[0] * 2.0, pos[1] * 3.0, pos[2] * 4.0, 1.0f32];
    for k in 0..4 {
        assert!((out_position[k] - expected[k]).abs() < 1e-6,
            "bespoke scale lane {k}: expected {} got {}",
            expected[k], out_position[k]);
    }
}
