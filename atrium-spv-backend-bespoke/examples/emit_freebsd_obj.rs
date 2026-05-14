//! Emit a FreeBSD/aarch64 ELF object for one of a few
//! verification shaders. Used by the in-VM verification
//! path: the object is scp'd into the FreeBSD VM, linked
//! with `cc -shared`, dlopen'd, and atrium_fs_main is
//! called + checked there — proving the bespoke backend's
//! ELF + AAPCS64 output runs on the actual production
//! target, not just the macOS host.
//!
//! Usage:
//!   emit_freebsd_obj <out.o>            -- constant-colour shader
//!   emit_freebsd_obj <out.o> const      -- (same)
//!   emit_freebsd_obj <out.o> ifelse <s> -- if(s<0.5) red else blue
//!   emit_freebsd_obj <out.o> loop <n>   -- counted-loop sum check
//!   emit_freebsd_obj <out.o> arith <n>  -- out = (n*2+1)*0.125
//!   emit_freebsd_obj <out.o> bitwise <n>-- nibble/xor/or extract
//!   emit_freebsd_obj <out.o> vecarith   -- (a+b)*(a-b) over vec4s
//!   emit_freebsd_obj <out.o> switch <n> -- switch(n) colour select
//!   emit_freebsd_obj <out.o> phi <s>    -- if/else joined by OpPhi
//!   emit_freebsd_obj <out.o> shuffle    -- OpVectorShuffle (bgra)
//!   emit_freebsd_obj <out.o> cextract   -- OpCompositeExtract
//!   emit_freebsd_obj <out.o> dot        -- OpDot + VectorTimesScalar
//!
//! Prefix any kind with `spirv` to write the raw SPIR-V
//! module instead of a compiled object (for the end-to-end
//! in-VM check that drives the real atrium-spv-compile):
//!   emit_freebsd_obj <out.spv> spirv const
//!   emit_freebsd_obj <out.spv> spirv loop 5
//!
//! stdout is always the expected RGBA for the chosen
//! shader+input, so the in-VM harness can diff against it
//! without re-deriving.

use atrium_spv_backend_bespoke::{compile, Target};
use atrium_spv_frontend::translate;

fn build_const_spirv(rgba: [f32; 4]) -> Vec<u8> {
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
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let ptr_out = b.type_pointer(None, StorageClass::Output, vec4);
    let cs: Vec<_> = rgba.iter().map(|x| b.constant_bit32(f32_ty, x.to_bits())).collect();
    let color = b.constant_composite(vec4, cs);
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

/// `if (push_const.scale < 0.5) out = red; else out = blue;`
/// Exercises AccessChain + Load + FOrdLt + BranchCond +
/// multi-block CFG + branch relocation — the full
/// AAPCS64 control-flow path.
fn build_ifelse_spirv() -> Vec<u8> {
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
    let red  = b.constant_composite(vec4, vec![c1, c0, c0, c1]);
    let blue = b.constant_composite(vec4, vec![c0, c0, c1, c1]);
    let pc_var = b.variable(ptr_pc_struct, None, StorageClass::PushConstant, None);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
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

/// Counted loop: `acc = 0; for (i = 0; i < n; i++) acc += i;`
/// then `out = (acc == n*(n-1)/2) ? white : black`.
/// `n` comes from an i32 push-constant. Exercises the
/// W-reg integer pool (IAdd, SLessThan, IEqual), the loop
/// header's two Phis (induction `i` + accumulator `acc`),
/// the back-edge Branch + its relocation, and OpSelect —
/// the full integer + loop path on the real target.
fn build_loop_spirv() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, LoopControl, MemoryModel,
        StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let i32_ty = b.type_int(32, 1);
    let bool_ty = b.type_bool();
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let pc_struct = b.type_struct(vec![i32_ty]);
    b.member_decorate(pc_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(pc_struct, Decoration::Block, vec![]);
    let ptr_pc_struct = b.type_pointer(None, StorageClass::PushConstant, pc_struct);
    let ptr_pc_i32    = b.type_pointer(None, StorageClass::PushConstant, i32_ty);
    let ptr_out       = b.type_pointer(None, StorageClass::Output, vec4);
    let zero_i = b.constant_bit32(i32_ty, 0u32);
    let one_i  = b.constant_bit32(i32_ty, 1u32);
    let c0  = b.constant_bit32(f32_ty, 0.0f32.to_bits());
    let c1  = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let pc_var = b.variable(ptr_pc_struct, None, StorageClass::PushConstant, None);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    let entry_id = b.id();
    let header_id = b.id();
    let body_id = b.id();
    let cont_id = b.id();
    let merge_id = b.id();
    let i_next = b.id();
    let acc_next = b.id();
    b.begin_block(Some(entry_id)).unwrap();
    let p = b.access_chain(ptr_pc_i32, None, pc_var, vec![zero_i]).unwrap();
    let n = b.load(i32_ty, None, p, None, vec![]).unwrap();
    b.branch(header_id).unwrap();
    b.begin_block(Some(header_id)).unwrap();
    let i_phi = b.phi(i32_ty, None,
        vec![(zero_i, entry_id), (i_next, cont_id)]).unwrap();
    let acc_phi = b.phi(i32_ty, None,
        vec![(zero_i, entry_id), (acc_next, cont_id)]).unwrap();
    let cond = b.s_less_than(bool_ty, None, i_phi, n).unwrap();
    b.loop_merge(merge_id, cont_id, LoopControl::NONE, vec![]).unwrap();
    b.branch_conditional(cond, body_id, merge_id, vec![]).unwrap();
    b.begin_block(Some(body_id)).unwrap();
    b.branch(cont_id).unwrap();
    b.begin_block(Some(cont_id)).unwrap();
    b.i_add(i32_ty, Some(i_next), i_phi, one_i).unwrap();
    b.i_add(i32_ty, Some(acc_next), acc_phi, i_phi).unwrap();
    b.branch(header_id).unwrap();
    b.begin_block(Some(merge_id)).unwrap();
    // expected sum for n=5 is 0+1+2+3+4 = 10.
    let expected = b.constant_bit32(i32_ty, 10u32);
    let ok = b.i_equal(bool_ty, None, acc_phi, expected).unwrap();
    let lum = b.select(f32_ty, None, ok, c1, c0).unwrap();
    let color = b.composite_construct(vec4, None,
        vec![lum, lum, lum, c1]).unwrap();
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

/// Integer arithmetic + int→float conversion:
/// `n` is an i32 push-constant; compute `r = n*2 + 1`
/// (IMul, IAdd), convert to f32 (ConvertSToF), scale by
/// 0.125 (FMul), and store `[r,r,r,1]`. The 0.125 scale
/// is a negative power of two so every result is exact in
/// f32 and prints identically under Rust's `{}` and the
/// harness's C `%g` — avoids a spurious format-precision
/// diff. Exercises the W-reg
/// integer pool and the scvtf path on the real target,
/// independent of any control flow — the on-target twin
/// of the host `three_way_int_arith_and_convert` test.
fn build_arith_spirv() -> Vec<u8> {
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
    let pc_struct = b.type_struct(vec![i32_ty]);
    b.member_decorate(pc_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(pc_struct, Decoration::Block, vec![]);
    let ptr_pc_struct = b.type_pointer(None, StorageClass::PushConstant, pc_struct);
    let ptr_pc_i32    = b.type_pointer(None, StorageClass::PushConstant, i32_ty);
    let ptr_out       = b.type_pointer(None, StorageClass::Output, vec4);
    let zero_i = b.constant_bit32(i32_ty, 0u32);
    let one_i  = b.constant_bit32(i32_ty, 1u32);
    let two_i  = b.constant_bit32(i32_ty, 2u32);
    let c01 = b.constant_bit32(f32_ty, 0.125f32.to_bits());
    let c1  = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let pc_var = b.variable(ptr_pc_struct, None, StorageClass::PushConstant, None);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let p = b.access_chain(ptr_pc_i32, None, pc_var, vec![zero_i]).unwrap();
    let n = b.load(i32_ty, None, p, None, vec![]).unwrap();
    let m = b.i_mul(i32_ty, None, n, two_i).unwrap();
    let r_i = b.i_add(i32_ty, None, m, one_i).unwrap();
    let r_f = b.convert_s_to_f(f32_ty, None, r_i).unwrap();
    let lum = b.f_mul(f32_ty, None, r_f, c01).unwrap();
    let color = b.composite_construct(vec4, None,
        vec![lum, lum, lum, c1]).unwrap();
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

/// Bitwise + shift: `n` is an i32 push-constant; compute
///   hi  = (n >> 4) & 0xF      (AShr, BitAnd)
///   lo  = n & 0xF             (BitAnd)
///   xor = n ^ 0xAA            (BitXor)
///   or_ = n | 0x10            (BitOr)
/// Output `vec4(hi/16, lo/16, xor/256, or_/256)` — all
/// divisors are powers of two so every quotient is exact
/// in f32 and prints identically under Rust `{}` and the
/// harness's C `%g`. The on-target twin of the host
/// `three_way_bitwise_and_shift` differential test.
fn build_bitwise_spirv() -> Vec<u8> {
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
    let pc_struct = b.type_struct(vec![i32_ty]);
    b.member_decorate(pc_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(pc_struct, Decoration::Block, vec![]);
    let ptr_pc_struct = b.type_pointer(None, StorageClass::PushConstant, pc_struct);
    let ptr_pc_i32    = b.type_pointer(None, StorageClass::PushConstant, i32_ty);
    let ptr_out       = b.type_pointer(None, StorageClass::Output, vec4);
    let zero_i = b.constant_bit32(i32_ty, 0u32);
    let four_i = b.constant_bit32(i32_ty, 4u32);
    let mask_i = b.constant_bit32(i32_ty, 0xFu32);
    let xor_i  = b.constant_bit32(i32_ty, 0xAAu32);
    let or_i   = b.constant_bit32(i32_ty, 0x10u32);
    let c16  = b.constant_bit32(f32_ty, 16.0f32.to_bits());
    let c256 = b.constant_bit32(f32_ty, 256.0f32.to_bits());
    let pc_var = b.variable(ptr_pc_struct, None, StorageClass::PushConstant, None);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let p = b.access_chain(ptr_pc_i32, None, pc_var, vec![zero_i]).unwrap();
    let n = b.load(i32_ty, None, p, None, vec![]).unwrap();
    let shifted = b.shift_right_arithmetic(i32_ty, None, n, four_i).unwrap();
    let hi = b.bitwise_and(i32_ty, None, shifted, mask_i).unwrap();
    let lo = b.bitwise_and(i32_ty, None, n, mask_i).unwrap();
    let xor = b.bitwise_xor(i32_ty, None, n, xor_i).unwrap();
    let or_ = b.bitwise_or(i32_ty, None, n, or_i).unwrap();
    let hi_f = b.convert_s_to_f(f32_ty, None, hi).unwrap();
    let lo_f = b.convert_s_to_f(f32_ty, None, lo).unwrap();
    let xor_f = b.convert_s_to_f(f32_ty, None, xor).unwrap();
    let or_f = b.convert_s_to_f(f32_ty, None, or_).unwrap();
    let hi_n = b.f_div(f32_ty, None, hi_f, c16).unwrap();
    let lo_n = b.f_div(f32_ty, None, lo_f, c16).unwrap();
    let xor_n = b.f_div(f32_ty, None, xor_f, c256).unwrap();
    let or_n = b.f_div(f32_ty, None, or_f, c256).unwrap();
    let color = b.composite_construct(vec4, None,
        vec![hi_n, lo_n, xor_n, or_n]).unwrap();
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

/// Vector arithmetic over constant vec4s:
///   a = [0.5, 1.0, 1.5, 2.0]   b = [0.25, 0.5, 0.5, 1.0]
///   out = (a + b) * (a - b)        (= a^2 - b^2)
/// No control flow, no push-const. Exercises per-lane
/// `FAdd` / `FSub` / `FMul` and the V-reg vector lane
/// allocator across three chained vec4 ops. All inputs
/// are halves/quarters so every lane result is exact in
/// f32. On-target twin of the host `three_way_vec_arithmetic`
/// differential test.
fn build_vecarith_spirv() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let a = [0.5f32, 1.0, 1.5, 2.0];
    let bvec = [0.25f32, 0.5, 0.5, 1.0];
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32_ty = b.type_float(32, None);
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let ptr_out = b.type_pointer(None, StorageClass::Output, vec4);
    let ca: Vec<_> = a.iter().map(|x| b.constant_bit32(f32_ty, x.to_bits())).collect();
    let cb: Vec<_> = bvec.iter().map(|x| b.constant_bit32(f32_ty, x.to_bits())).collect();
    let va = b.constant_composite(vec4, ca);
    let vb = b.constant_composite(vec4, cb);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let sum  = b.f_add(vec4, None, va, vb).unwrap();
    let diff = b.f_sub(vec4, None, va, vb).unwrap();
    let prod = b.f_mul(vec4, None, sum, diff).unwrap();
    b.store(out, prod, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

/// `switch (n) { 0: red; 1: green; 2: blue; default: white }`
/// `n` is an i32 push-constant. Exercises OpSwitch — the
/// multi-target jump codegen (one compare + branch per
/// case label, fall-through to default) — plus a 5-block
/// CFG with four branch relocations converging on a merge
/// block. On-target twin of the host `three_way_switch_*`
/// differential tests.
fn build_switch_spirv() -> Vec<u8> {
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
    let i32_ty = b.type_int(32, 1);
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let pc_struct = b.type_struct(vec![i32_ty]);
    b.member_decorate(pc_struct, 0, Decoration::Offset,
                      vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(pc_struct, Decoration::Block, vec![]);
    let ptr_pc_struct = b.type_pointer(None, StorageClass::PushConstant, pc_struct);
    let ptr_pc_i32    = b.type_pointer(None, StorageClass::PushConstant, i32_ty);
    let ptr_out       = b.type_pointer(None, StorageClass::Output, vec4);
    let zero_i = b.constant_bit32(i32_ty, 0u32);
    let c0 = b.constant_bit32(f32_ty, 0.0f32.to_bits());
    let c1 = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let red   = b.constant_composite(vec4, vec![c1, c0, c0, c1]);
    let green = b.constant_composite(vec4, vec![c0, c1, c0, c1]);
    let blue  = b.constant_composite(vec4, vec![c0, c0, c1, c1]);
    let white = b.constant_composite(vec4, vec![c1, c1, c1, c1]);
    let pc_var = b.variable(ptr_pc_struct, None, StorageClass::PushConstant, None);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let p = b.access_chain(ptr_pc_i32, None, pc_var, vec![zero_i]).unwrap();
    let n = b.load(i32_ty, None, p, None, vec![]).unwrap();
    let c0_id = b.id();
    let c1_id = b.id();
    let c2_id = b.id();
    let dflt_id = b.id();
    let merge_id = b.id();
    b.selection_merge(merge_id, SelectionControl::NONE).unwrap();
    b.switch(n, dflt_id, vec![
        (rspirv::dr::Operand::LiteralBit32(0), c0_id),
        (rspirv::dr::Operand::LiteralBit32(1), c1_id),
        (rspirv::dr::Operand::LiteralBit32(2), c2_id),
    ]).unwrap();
    b.begin_block(Some(c0_id)).unwrap();
    b.store(out, red, None, vec![]).unwrap();
    b.branch(merge_id).unwrap();
    b.begin_block(Some(c1_id)).unwrap();
    b.store(out, green, None, vec![]).unwrap();
    b.branch(merge_id).unwrap();
    b.begin_block(Some(c2_id)).unwrap();
    b.store(out, blue, None, vec![]).unwrap();
    b.branch(merge_id).unwrap();
    b.begin_block(Some(dflt_id)).unwrap();
    b.store(out, white, None, vec![]).unwrap();
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

/// `if (push_const.scale < 0.5) chosen = 1.0 else chosen
/// = 0.25;` where `chosen` is produced by an `OpPhi` at
/// the merge block (the then/else blocks are empty — they
/// only carry the branch). Output `vec4(chosen^3, 1)`.
/// Exercises Phi convergence in a *non-loop* CFG: the Phi
/// arms are constants defined in the entry block but the
/// Phi value materialises only at the merge, so the
/// backend must place each arm's value into the Phi's
/// register on the correct predecessor edge. On-target
/// twin of the host `three_way_phi_*` differential tests.
fn build_phi_spirv() -> Vec<u8> {
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
    let c025 = b.constant_bit32(f32_ty, 0.25f32.to_bits());
    let c05  = b.constant_bit32(f32_ty, 0.5f32.to_bits());
    let c1   = b.constant_bit32(f32_ty, 1.0f32.to_bits());
    let pc_var = b.variable(ptr_pc_struct, None, StorageClass::PushConstant, None);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
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
    b.branch(merge_id).unwrap();
    b.begin_block(Some(else_id)).unwrap();
    b.branch(merge_id).unwrap();
    b.begin_block(Some(merge_id)).unwrap();
    let chosen = b.phi(f32_ty, None,
        vec![(c1, then_id), (c025, else_id)]).unwrap();
    let color = b.composite_construct(vec4, None,
        vec![chosen, chosen, chosen, c1]).unwrap();
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

/// `OpVectorShuffle`: `va = [0.25, 0.5, 0.75, 1.0]`,
/// `out = va.bgra` (shuffle indices `[2, 1, 0, 3]`).
/// Exercises the ARM64 lane-shuffle codegen — distinct
/// from per-lane arithmetic, this moves lanes between
/// V-register positions. All lanes are exact in f32.
/// On-target twin of host `three_way_vector_shuffle_bgra`.
fn build_shuffle_spirv() -> Vec<u8> {
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
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let ptr_out = b.type_pointer(None, StorageClass::Output, vec4);
    let lanes = [0.25f32, 0.5, 0.75, 1.0];
    let cs: Vec<_> = lanes.iter()
        .map(|x| b.constant_bit32(f32_ty, x.to_bits())).collect();
    let va = b.constant_composite(vec4, cs);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let swizzled = b.vector_shuffle(vec4, None, va, va, vec![2, 1, 0, 3]).unwrap();
    b.store(out, swizzled, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

/// `OpCompositeExtract` + `OpCompositeConstruct`: pull
/// individual lanes from `va = [0.25, 0.5, 0.75, 1.0]` and
/// recombine in a different order — `out = vec4(va[3],
/// va[0], va[2], va[1])`. Exercises single-lane extraction
/// codegen. All lanes are exact in f32. On-target twin of
/// host `three_way_composite_extract`.
fn build_cextract_spirv() -> Vec<u8> {
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
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let ptr_out = b.type_pointer(None, StorageClass::Output, vec4);
    let lanes = [0.25f32, 0.5, 0.75, 1.0];
    let cs: Vec<_> = lanes.iter()
        .map(|x| b.constant_bit32(f32_ty, x.to_bits())).collect();
    let va = b.constant_composite(vec4, cs);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let e3 = b.composite_extract(f32_ty, None, va, vec![3]).unwrap();
    let e0 = b.composite_extract(f32_ty, None, va, vec![0]).unwrap();
    let e2 = b.composite_extract(f32_ty, None, va, vec![2]).unwrap();
    let e1 = b.composite_extract(f32_ty, None, va, vec![1]).unwrap();
    let color = b.composite_construct(vec4, None, vec![e3, e0, e2, e1]).unwrap();
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

/// `OpDot` + `OpVectorTimesScalar`: `d = dot(va, vb)`,
/// `scaled = vb * d` (exercised but unused), then
/// `out = vec4(va.x*vb.x, va.y*vb.y, va.z*vb.z, d)`.
/// `va = [0.5, 0.25, 0.125, 0.0]`, `vb = [0.5, 0.5, 0.5,
/// 1.0]` — every product and the dot result are exact in
/// f32. On-target twin of host `three_way_dot_and_composite`.
fn build_dot_spirv() -> Vec<u8> {
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
    let vec4 = b.type_vector(f32_ty, 4);
    let void_fn = b.type_function(void, vec![]);
    let ptr_out = b.type_pointer(None, StorageClass::Output, vec4);
    let a = [0.5f32, 0.25, 0.125, 0.0];
    let bvec = [0.5f32, 0.5, 0.5, 1.0];
    let ca: Vec<_> = a.iter().map(|x| b.constant_bit32(f32_ty, x.to_bits())).collect();
    let cb: Vec<_> = bvec.iter().map(|x| b.constant_bit32(f32_ty, x.to_bits())).collect();
    let va = b.constant_composite(vec4, ca.clone());
    let vb = b.constant_composite(vec4, cb.clone());
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location,
               vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let d = b.dot(f32_ty, None, va, vb).unwrap();
    // Exercise OpVectorTimesScalar; the result is unused.
    let _scaled = b.vector_times_scalar(vec4, None, vb, d).unwrap();
    let m0 = b.f_mul(f32_ty, None, ca[0], cb[0]).unwrap();
    let m1 = b.f_mul(f32_ty, None, ca[1], cb[1]).unwrap();
    let m2 = b.f_mul(f32_ty, None, ca[2], cb[2]).unwrap();
    let color = b.composite_construct(vec4, None, vec![m0, m1, m2, d]).unwrap();
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

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next()
        .unwrap_or_else(|| "/tmp/atrium_fs_freebsd.o".to_string());
    let mut kind = args.next().unwrap_or_else(|| "const".to_string());

    // `spirv` mode: write the raw SPIR-V module for the
    // *next* kind argument to <path> instead of a compiled
    // object. Used by the end-to-end in-VM check, which
    // feeds the .spv to the real atrium-spv-compile binary
    // on-target rather than emitting the object host-side.
    let emit_spirv_only = kind == "spirv";
    if emit_spirv_only {
        kind = args.next().unwrap_or_else(|| "const".to_string());
    }

    let (spirv, expected): (Vec<u8>, [f32; 4]) = match kind.as_str() {
        "const" => {
            let rgba = [0.125f32, 0.375, 0.625, 1.0];
            (build_const_spirv(rgba), rgba)
        }
        "ifelse" => {
            let scale: f32 = args.next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.2);
            let expected = if scale < 0.5 {
                [1.0, 0.0, 0.0, 1.0]   // red
            } else {
                [0.0, 0.0, 1.0, 1.0]   // blue
            };
            (build_ifelse_spirv(), expected)
        }
        "loop" => {
            // n comes from an i32 push-const supplied by
            // the harness. The shader's hardcoded
            // expected-sum is 10 (= sum 0..5), so the
            // loop produces white iff n == 5.
            let n: i32 = args.next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(5);
            let sum: i32 = (0..n).sum();
            let expected = if sum == 10 {
                [1.0, 1.0, 1.0, 1.0]   // white
            } else {
                [0.0, 0.0, 0.0, 1.0]   // black
            };
            (build_loop_spirv(), expected)
        }
        "arith" => {
            // n from an i32 push-const; out = (n*2+1)*0.125.
            let n: i32 = args.next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(3);
            let lum = (n * 2 + 1) as f32 * 0.125;
            (build_arith_spirv(), [lum, lum, lum, 1.0])
        }
        "bitwise" => {
            // n from an i32 push-const; vec4 of nibble /
            // xor / or extractions, all power-of-two
            // normalised.
            let n: i32 = args.next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0x53);
            let hi = ((n >> 4) & 0xF) as f32 / 16.0;
            let lo = (n & 0xF) as f32 / 16.0;
            let xor = (n ^ 0xAA) as f32 / 256.0;
            let or_ = (n | 0x10) as f32 / 256.0;
            (build_bitwise_spirv(), [hi, lo, xor, or_])
        }
        "vecarith" => {
            // (a+b)*(a-b) for the hardcoded constant vec4s.
            let a = [0.5f32, 1.0, 1.5, 2.0];
            let bv = [0.25f32, 0.5, 0.5, 1.0];
            let mut e = [0.0f32; 4];
            for i in 0..4 { e[i] = (a[i] + bv[i]) * (a[i] - bv[i]); }
            (build_vecarith_spirv(), e)
        }
        "switch" => {
            // n from an i32 push-const selects a colour.
            let n: i32 = args.next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1);
            let expected = match n {
                0 => [1.0, 0.0, 0.0, 1.0], // red
                1 => [0.0, 1.0, 0.0, 1.0], // green
                2 => [0.0, 0.0, 1.0, 1.0], // blue
                _ => [1.0, 1.0, 1.0, 1.0], // white (default)
            };
            (build_switch_spirv(), expected)
        }
        "phi" => {
            // scale<0.5 ? 1.0 : 0.25, joined by OpPhi.
            let scale: f32 = args.next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.2);
            let chosen = if scale < 0.5 { 1.0 } else { 0.25 };
            (build_phi_spirv(), [chosen, chosen, chosen, 1.0])
        }
        "shuffle" => {
            // va.bgra over [0.25,0.5,0.75,1.0].
            (build_shuffle_spirv(), [0.75, 0.5, 0.25, 1.0])
        }
        "cextract" => {
            // vec4(va[3],va[0],va[2],va[1]) over [0.25,0.5,0.75,1.0].
            (build_cextract_spirv(), [1.0, 0.25, 0.75, 0.5])
        }
        "dot" => {
            // d = dot(va,vb); out = (va.x*vb.x, va.y*vb.y,
            //                        va.z*vb.z, d).
            let a = [0.5f32, 0.25, 0.125, 0.0];
            let bv = [0.5f32, 0.5, 0.5, 1.0];
            let d: f32 = (0..4).map(|i| a[i] * bv[i]).sum();
            (build_dot_spirv(), [a[0]*bv[0], a[1]*bv[1], a[2]*bv[2], d])
        }
        other => {
            eprintln!("unknown shader kind: {other}");
            std::process::exit(2);
        }
    };

    if emit_spirv_only {
        std::fs::write(&path, &spirv).expect("write spirv");
        // stdout: the expected RGBA the in-VM harness diffs against.
        println!("{} {} {} {}", expected[0], expected[1], expected[2], expected[3]);
        eprintln!("wrote {} ({} bytes, SPIR-V, kind={})",
                  path, spirv.len(), kind);
        return;
    }

    let module = translate(&spirv).expect("frontend translate");
    let out = compile(&module, Target::Aarch64FreeBSD).expect("bespoke compile");
    std::fs::write(&path, &out.object).expect("write object");
    // stdout: the expected RGBA the in-VM harness diffs against.
    println!("{} {} {} {}", expected[0], expected[1], expected[2], expected[3]);
    eprintln!("wrote {} ({} bytes, ELF aarch64, kind={})",
              path, out.object.len(), kind);
}
