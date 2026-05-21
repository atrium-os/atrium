//! Frontend recognises OpAtomicIAdd and emits Op::AtomicIAdd.
//! Memory scope + semantics operands are parsed but ignored:
//! the Tier-2 serial dispatcher's per-invocation loop makes
//! true atomicity trivially satisfied by load+add+store.

use atrium_spv_frontend::translate;
use atrium_spv_ir::Op;

fn build_atomic_add_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
        MemorySemantics, Scope,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let uvec3  = b.type_vector(u32_ty, 3);
    let void_fn = b.type_function(void, vec![]);
    let ssbo_struct = b.type_struct(vec![u32_ty]);
    b.decorate(ssbo_struct, Decoration::Block, vec![]);
    b.member_decorate(ssbo_struct, 0, Decoration::Offset,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_ssbo_struct = b.type_pointer(None, StorageClass::StorageBuffer, ssbo_struct);
    let ptr_ssbo_u32    = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let ssbo_var = b.variable(ptr_ssbo_struct, None, StorageClass::StorageBuffer, None);
    b.decorate(ssbo_var, Decoration::DescriptorSet,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    b.decorate(ssbo_var, Decoration::Binding,
        vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let ptr_in_uvec3 = b.type_pointer(None, StorageClass::Input, uvec3);
    let gid_var = b.variable(ptr_in_uvec3, None, StorageClass::Input, None);
    b.decorate(gid_var, Decoration::BuiltIn,
        vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let c_zero = b.constant_bit32(u32_ty, 0);
    let c_scope = b.constant_bit32(u32_ty, Scope::Device as u32);
    let c_semantics = b.constant_bit32(u32_ty,
        MemorySemantics::ATOMIC_COUNTER_MEMORY.bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let gid = b.load(uvec3, None, gid_var, None, vec![]).unwrap();
    let gid_x = b.composite_extract(u32_ty, None, gid, vec![0]).unwrap();
    let dst = b.access_chain(ptr_ssbo_u32, None, ssbo_var, vec![c_zero]).unwrap();
    // OpAtomicIAdd <u32_ty> <result> <ptr> <scope> <semantics> <value>
    let _old = b.atomic_i_add(u32_ty, None, dst, c_scope, c_semantics, gid_x).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![gid_var, ssbo_var]);
    b.execution_mode(main, ExecutionMode::LocalSize, [4u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn frontend_emits_atomic_i_add() {
    let spv = build_atomic_add_cs();
    let module = translate(&spv).expect("frontend should accept OpAtomicIAdd");
    let func = &module.functions[0];
    let entry = func.blocks.get(&func.entry_block).expect("entry");
    let mut saw = false;
    for inst in &entry.insts {
        if matches!(&inst.op, Op::AtomicIAdd { .. }) {
            saw = true;
            assert!(inst.result.is_some(), "AtomicIAdd should have a result Value");
        }
    }
    assert!(saw, "expected Op::AtomicIAdd in the entry block");
}
