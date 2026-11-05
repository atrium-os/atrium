//! A Tier-2 client for checking an executor end to end (used for the
//! per-app jail check in the FreeBSD VM, docs/spec/tier2-robust-access.md
//! §4): connect, upload a compute shader, dispatch it over a 16-byte
//! buffer, read the buffer back.
//!
//!   tier2_probe <socket> [index]
//!
//! The shader stores 0xC0FFEE at word 0 and 0xBAD at word `index`
//! (default 0xFFFF_FFF0, far out of bounds). The bounds checks must
//! send the second store to the sink: the probe prints the buffer and
//! exits 0 when words 1..4 are untouched.

use aqueduct::Connection;
use aqueduct_gpu::frame::DispatchCmd;
use aqueduct_gpu::ids::{IdNamespace, ResourceId};
use aqueduct_gpu::opcodes::FrameOp;
use aqueduct_gpu::payloads::{BufferCreatePayload, ClientKind, PipelineKind, ShaderKind};
use aqueduct_gpu_client::GpuClient;
use sha2::{Digest, Sha256};

fn build_cs(index: u32) -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::dr::Operand::LiteralBit32 as L;
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
    let rt = b.type_runtime_array(u32_ty);
    b.decorate(rt, Decoration::ArrayStride, vec![L(4)]);
    let s = b.type_struct(vec![rt]);
    b.decorate(s, Decoration::Block, vec![]);
    b.member_decorate(s, 0, Decoration::Offset, vec![L(0)]);
    let ptr_s = b.type_pointer(None, StorageClass::StorageBuffer, s);
    let ptr_u = b.type_pointer(None, StorageClass::StorageBuffer, u32_ty);
    let data = b.variable(ptr_s, None, StorageClass::StorageBuffer, None);
    b.decorate(data, Decoration::DescriptorSet, vec![L(0)]);
    b.decorate(data, Decoration::Binding, vec![L(0)]);
    let ptr_in = b.type_pointer(None, StorageClass::Input, uvec3);
    let gid = b.variable(ptr_in, None, StorageClass::Input, None);
    b.decorate(gid, Decoration::BuiltIn, vec![rspirv::dr::Operand::BuiltIn(BuiltIn::GlobalInvocationId)]);
    let c0 = b.constant_bit32(u32_ty, 0);
    let ck = b.constant_bit32(u32_ty, index);
    let coffee = b.constant_bit32(u32_ty, 0xC0FFEE);
    let bad = b.constant_bit32(u32_ty, 0xBAD);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let g = b.load(uvec3, None, gid, None, vec![]).unwrap();
    let gx = b.composite_extract(u32_ty, None, g, vec![0]).unwrap();
    let i = b.i_add(u32_ty, None, gx, ck).unwrap(); // dynamic index
    let p0 = b.access_chain(ptr_u, None, data, vec![c0, c0]).unwrap();
    b.store(p0, coffee, None, vec![]).unwrap();
    let pi = b.access_chain(ptr_u, None, data, vec![c0, i]).unwrap();
    b.store(pi, bad, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::GLCompute, main, "main", vec![gid, data]);
    b.execution_mode(main, ExecutionMode::LocalSize, [1u32, 1, 1]);
    b.module().assemble().iter().flat_map(|w| w.to_le_bytes()).collect()
}

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let sock = args.next().ok_or_else(|| anyhow::anyhow!("usage: tier2_probe <socket> [index]"))?;
    let index: u32 = match args.next() {
        Some(s) => s.strip_prefix("0x").map(|h| u32::from_str_radix(h, 16))
            .unwrap_or_else(|| s.parse())?,
        None => 0xFFFF_FFF0,
    };
    let mut c = GpuClient::new(Connection::connect(&sock)?);
    let backend = c.handshake(ClientKind::FrescodRenderer)?.backend;

    let spv = build_cs(index);
    let hash: [u8; 32] = Sha256::digest(&spv).into();
    let shader = c.upload_shader(hash, ShaderKind::SpirV, backend, spv)?;
    let blob = aqueduct_gpu::Tier2ComputeStateBlob {
        local_size_x: 1, local_size_y: 1, local_size_z: 1,
        ssbo_binding_count: 1, workgroup_size: 0,
        spec_overrides: vec![], uses_barrier: false,
    };
    let pipeline = c.create_pipeline(PipelineKind::Compute, vec![shader], postcard::to_allocvec(&blob)?)?;
    let buf = c.create_buffer(BufferCreatePayload {
        buffer_id: ResourceId::new(IdNamespace::IcdRuntime, 0),
        backing_region: ResourceId::new(IdNamespace::IcdRuntime, 0),
        region_offset: 0, size: 16, usage: 0x20, // STORAGE_BUFFER
    })?;
    let prefill: Vec<u8> = [1u32, 2, 3, 4].iter().flat_map(|v| v.to_le_bytes()).collect();
    c.write_buffer(buf, 0, prefill)?;
    let fence = c.create_fence()?;
    let mut f = c.frame_builder();
    f.push(FrameOp::BindPipeline, &pipeline.raw().to_le_bytes())?;
    f.push_bind_storage_buffers(&[(0, buf.raw())])?;
    f.push_dispatch(DispatchCmd { group_count_x: 1, group_count_y: 1, group_count_z: 1 })?;
    c.submit_frame(fence, f, 1)?;
    let done = c.wait_fence(fence, 30_000_000_000)?;
    let out = c.read_buffer(buf, 0, 16)?;
    let w: Vec<u32> = out.chunks_exact(4).map(|x| u32::from_le_bytes(x.try_into().unwrap())).collect();
    println!("fence={done} buffer={w:x?}");
    let in_bounds = index < 4;
    let ok = w.first() == Some(&0xC0FFEE)
        && (1..4).all(|k| w[k] == if in_bounds && k as u32 == index { 0xBAD } else { k as u32 + 1 });
    println!("{}", if ok { "PROBE OK" } else { "PROBE FAILED" });
    std::process::exit(if ok { 0 } else { 1 });
}
