//! Emit a FreeBSD/aarch64 ELF object for a constant-colour
//! fragment shader. Used by the in-VM verification path:
//! the object is scp'd into the FreeBSD VM, linked with
//! `cc -shared`, dlopen'd, and atrium_fs_main is called +
//! checked there — proving the bespoke backend's ELF +
//! AAPCS64 output runs on the actual production target,
//! not just the macOS host.
use atrium_spv_backend_bespoke::{compile, Target};
use atrium_spv_frontend::translate;

fn build_spirv(rgba: [f32; 4]) -> Vec<u8> {
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

fn main() {
    let rgba = [0.125f32, 0.375, 0.625, 1.0];
    let spirv = build_spirv(rgba);
    let module = translate(&spirv).expect("frontend translate");
    let out = compile(&module, Target::Aarch64FreeBSD).expect("bespoke compile");
    let path = std::env::args().nth(1)
        .unwrap_or_else(|| "/tmp/atrium_fs_freebsd.o".to_string());
    std::fs::write(&path, &out.object).expect("write object");
    // Print the expected RGBA so the in-VM harness can
    // assert against it without re-deriving.
    println!("{} {} {} {}", rgba[0], rgba[1], rgba[2], rgba[3]);
    eprintln!("wrote {} ({} bytes, ELF aarch64)", path, out.object.len());
}
