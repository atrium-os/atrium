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

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next()
        .unwrap_or_else(|| "/tmp/atrium_fs_freebsd.o".to_string());
    let kind = args.next().unwrap_or_else(|| "const".to_string());

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
        other => {
            eprintln!("unknown shader kind: {other}");
            std::process::exit(2);
        }
    };

    let module = translate(&spirv).expect("frontend translate");
    let out = compile(&module, Target::Aarch64FreeBSD).expect("bespoke compile");
    std::fs::write(&path, &out.object).expect("write object");
    // stdout: the expected RGBA the in-VM harness diffs against.
    println!("{} {} {} {}", expected[0], expected[1], expected[2], expected[3]);
    eprintln!("wrote {} ({} bytes, ELF aarch64, kind={})",
              path, out.object.len(), kind);
}
