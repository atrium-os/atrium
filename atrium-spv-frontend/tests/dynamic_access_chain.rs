//! Frontend recognises an OpAccessChain whose last index is
//! non-constant + steps into a RuntimeArray member, and
//! emits an Op::PtrOffsetDynamic following the constant-
//! prefix Op::AccessChain.

use atrium_spv_frontend::translate;
use atrium_spv_ir::Op;

fn build_dynamic_ssbo_write_cs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 3);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void   = b.type_void();
    let u32_ty = b.type_int(32, 0);
    let uvec3  = b.type_vector(u32_ty, 3);
    let void_fn = b.type_function(void, vec![]);
    // RuntimeArray<u32> + struct { RuntimeArray<u32> data; }
    let rt_arr = b.type_runtime_array(u32_ty);
    b.decorate(rt_arr, Decoration::ArrayStride,
        vec![rspirv::dr::Operand::LiteralBit32(4)]);
    let ssbo_struct = b.type_struct(vec![rt_arr]);
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
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let gid = b.load(uvec3, None, gid_var, None, vec![]).unwrap();
    let gid_x = b.composite_extract(u32_ty, None, gid, vec![0]).unwrap();
    // ssbo.data[gid_x] = gid_x
    let dst = b.access_chain(ptr_ssbo_u32, None, ssbo_var,
        vec![c_zero, gid_x]).unwrap();
    b.store(dst, gid_x, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main",
        vec![gid_var, ssbo_var]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    let words: Vec<u32> = b.module().assemble();
    let mut bytes = Vec::with_capacity(words.len() * 4);
    for w in words { bytes.extend_from_slice(&w.to_le_bytes()); }
    bytes
}

#[test]
fn frontend_emits_ptr_offset_dynamic_for_runtime_array_index() {
    let spv = build_dynamic_ssbo_write_cs();
    let module = translate(&spv).expect("frontend should accept dynamic SSBO index");
    assert_eq!(module.functions.len(), 1);
    let func = &module.functions[0];
    let entry = func.blocks.get(&func.entry_block).expect("entry block");

    let mut saw_access_chain = false;
    let mut saw_ptr_offset_dynamic = false;
    let mut stride: u32 = 0;
    for inst in &entry.insts {
        match &inst.op {
            Op::AccessChain { byte_offset, .. } => {
                assert_eq!(*byte_offset, 0,
                    "constant prefix should be 0 (member 0 of the SSBO struct)");
                saw_access_chain = true;
            }
            Op::PtrOffsetDynamic { stride: s, .. } => {
                saw_ptr_offset_dynamic = true;
                stride = *s;
            }
            _ => {}
        }
    }
    assert!(saw_access_chain, "expected constant-prefix Op::AccessChain");
    assert!(saw_ptr_offset_dynamic, "expected Op::PtrOffsetDynamic for ssbo.data[gid_x]");
    assert_eq!(stride, 4, "u32 element stride should be 4");
}
