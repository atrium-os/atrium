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
