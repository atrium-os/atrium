//! Carillon "bridge" — the full aqueduct-gpu wire over the Carillon byte
//! FIFOs, reusing `Session` (host) and `GpuClient` (guest) verbatim.
//!
//! Proves resource-creation-over-rings + a real rendered frame: a real
//! `GpuClient` drives handshake → create image/buffer → upload VS+FS →
//! create graphics pipeline → submit a draw frame → read back, with every
//! byte flowing through the Carillon shared-memory byte streams (g2h /
//! h2g) + doorbells, bridged to a `Session` running a real
//! `MoltenVkBackend`. The green triangle comes back through the rings.
//!
//! This is the host-side (no-QEMU) proof of the transport carrying the
//! whole wire; the VM guest pump (cdev mmap + ring ioctls) reuses the same
//! `pump_*` halves. Gated on MoltenVK (macOS host) — skips elsewhere.

#![cfg(unix)]

use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use aqueduct::Connection;
use aqueduct_gpu::{
    backends::BackendId,
    ids::ResourceId,
    opcodes::FrameOp,
    payloads::{
        BufferCreatePayload, ClientKind, ImageCreatePayload, MemoryUsage,
        PipelineKind, ShaderKind,
    },
};
use aqueduct_gpu_client::GpuClient;
use aqueduct_gpu_host::carillon::{
    layout, pump_fd_to_stream, pump_stream_to_fd, Doorbell, Region,
};
use aqueduct_gpu_host::{Backend, MoltenVkBackend, MoltenVkError, Session};

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

#[test]
fn real_frame_through_carillon_bridge_to_metal() {
    let backend: Arc<dyn Backend> = match MoltenVkBackend::new() {
        Ok(b) => Arc::new(b),
        Err(MoltenVkError::LoaderUnavailable(e)) => {
            eprintln!("SKIP real_frame_through_carillon_bridge_to_metal: {e}");
            return;
        }
        Err(e) => panic!("MoltenVK init failed: {e}"),
    };

    const W: u32 = 64;
    const H: u32 = 64;

    // Shared region (host view) + a second mapping (guest view).
    let shm = std::env::temp_dir().join(format!("carillon-wire-{}.shm", std::process::id()));
    let host_region = Region::create(&shm, layout::TOTAL_SIZE).unwrap();
    let guest_region = Region::open(&shm, layout::TOTAL_SIZE).unwrap();
    guest_region.validate_header().unwrap();

    // Doorbells: g2h (guest pump rings, host pump waits) + h2g (host pump
    // rings, guest pump waits) + a shutdown self-pipe per FIFO→socket pump.
    let g2h = Doorbell::new().unwrap();
    let h2g = Doorbell::new().unwrap();
    let host_shutdown = Doorbell::new().unwrap();
    let guest_shutdown = Doorbell::new().unwrap();

    // socketpairs: Session ⇄ host bridge, GpuClient ⇄ guest bridge.
    let (hs_a, hs_b) = UnixStream::pair().unwrap();
    let (gs_a, gs_b) = UnixStream::pair().unwrap();
    let hs_a_fd = hs_a.as_raw_fd();
    let gs_a_fd = gs_a.as_raw_fd();

    thread::scope(|s| {
        // Host: a real Session on hs_b, running a MoltenVk backend.
        let be = backend.clone();
        s.spawn(move || {
            let conn = Connection::wrap(hs_b).unwrap();
            let _ = Session::new(conn, be).run();
        });
        // Host req-pump: g2h FIFO → Session socket.
        s.spawn(|| {
            let _ = pump_stream_to_fd(
                hs_a_fd, &host_region, layout::STREAM_G2H_OFFSET,
                layout::C_STREAM_G2H_HEAD, layout::C_STREAM_G2H_TAIL,
                &g2h, host_shutdown.read_fd(),
            );
        });
        // Host resp-pump: Session socket → h2g FIFO (ring guest).
        s.spawn(|| {
            pump_fd_to_stream(
                hs_a_fd, &host_region, layout::STREAM_H2G_OFFSET,
                layout::C_STREAM_H2G_HEAD, layout::C_STREAM_H2G_TAIL, &h2g,
            );
        });
        // Guest req-pump: GpuClient socket → g2h FIFO (ring host).
        s.spawn(|| {
            pump_fd_to_stream(
                gs_a_fd, &guest_region, layout::STREAM_G2H_OFFSET,
                layout::C_STREAM_G2H_HEAD, layout::C_STREAM_G2H_TAIL, &g2h,
            );
        });
        // Guest resp-pump: h2g FIFO → GpuClient socket.
        s.spawn(|| {
            let _ = pump_stream_to_fd(
                gs_a_fd, &guest_region, layout::STREAM_H2G_OFFSET,
                layout::C_STREAM_H2G_HEAD, layout::C_STREAM_H2G_TAIL,
                &h2g, guest_shutdown.read_fd(),
            );
        });

        // ── Guest: drive a real frame through GpuClient (over the bridge) ──
        let mut client = GpuClient::new(Connection::wrap(gs_b).unwrap());
        let hs = client.handshake(ClientKind::VulkanIcd).unwrap().clone();
        let backend_id: BackendId = hs.backend;

        let img_mem = client
            .allocate_memory((W * H * 4) as u64, MemoryUsage::ImageBacking)
            .unwrap();
        let image = client
            .create_image(ImageCreatePayload {
                image_id: ResourceId(0),
                backing_region: img_mem.region_id,
                region_offset: 0,
                format: 37, // RGBA8_UNORM
                width: W, height: H, depth: 1,
                mip_levels: 1, array_layers: 1, usage: 0x07,
            })
            .unwrap();
        let buf_mem = client
            .allocate_memory((W * H * 4) as u64, MemoryUsage::Staging)
            .unwrap();
        let buffer = client
            .create_buffer(BufferCreatePayload {
                buffer_id: ResourceId(0),
                backing_region: buf_mem.region_id,
                region_offset: 0,
                size: (W * H * 4) as u64,
                usage: 0x01,
            })
            .unwrap();

        let vs = build_fullscreen_tri_vs();
        let fs = build_const_fs([0.2, 0.85, 0.3, 1.0]); // green
        let vs_id = client
            .upload_shader(sha256(&vs), ShaderKind::SpirV, backend_id, vs)
            .unwrap();
        let fs_id = client
            .upload_shader(sha256(&fs), ShaderKind::SpirV, backend_id, fs)
            .unwrap();
        let pipe = client
            .create_pipeline(PipelineKind::Graphics, vec![vs_id, fs_id], Vec::new())
            .unwrap();

        thread::sleep(Duration::from_millis(30));

        let fence = client.create_fence().unwrap();
        let mut fb = client.frame_builder();
        let mut brp = Vec::new();
        brp.extend_from_slice(&image.raw().to_le_bytes());
        brp.extend_from_slice(&[10u8, 10, 10, 255]);
        brp.extend_from_slice(&0u32.to_le_bytes());
        fb.push(FrameOp::BeginRenderPass, &brp).unwrap();
        fb.push(FrameOp::BindPipeline, &pipe.raw().to_le_bytes()).unwrap();
        let mut draw = Vec::new();
        draw.extend_from_slice(&3u32.to_le_bytes());
        draw.extend_from_slice(&1u32.to_le_bytes());
        draw.extend_from_slice(&0u32.to_le_bytes());
        draw.extend_from_slice(&0u32.to_le_bytes());
        fb.push(FrameOp::Draw, &draw).unwrap();
        fb.push(FrameOp::EndRenderPass, &[]).unwrap();
        let mut cib = Vec::new();
        cib.extend_from_slice(&image.raw().to_le_bytes());
        cib.extend_from_slice(&buffer.raw().to_le_bytes());
        cib.extend_from_slice(&0u32.to_le_bytes());
        cib.extend_from_slice(&1u32.to_le_bytes());
        let mut region = vec![0u8; 56];
        region[44..48].copy_from_slice(&W.to_le_bytes());
        region[48..52].copy_from_slice(&H.to_le_bytes());
        region[52..56].copy_from_slice(&1u32.to_le_bytes());
        cib.extend_from_slice(&region);
        fb.push(FrameOp::CopyImgToBuf, &cib).unwrap();

        client.submit_frame(fence, fb, 1).unwrap();
        assert!(client.wait_fence(fence, 5_000_000_000).unwrap(),
            "fence signals after the bridged MoltenVk frame");

        let px = client.read_buffer(buffer, 0, (W * H * 4) as u64)
            .expect("readback over the Carillon bridge");
        assert_eq!(px.len(), (W * H * 4) as usize);
        let i = ((H as usize / 2) * W as usize + W as usize / 2) * 4;
        assert!(
            px[i] < 90 && px[i + 1] > 180 && px[i + 2] < 110 && px[i + 3] == 255,
            "centre = the drawn green triangle, delivered through Carillon: {:?}",
            &px[i..i + 4]
        );

        // ── Teardown: collapse the pumps + Session, then the scope joins ──
        drop(client); // closes gs_b → guest req-pump hits EOF
        host_shutdown.signal();  // host req-pump (FIFO→socket) exits
        guest_shutdown.signal(); // guest resp-pump (FIFO→socket) exits
        // Closing hs_a EOFs Session (hs_b) + the host resp-pump (reads hs_a).
        let _ = hs_a.shutdown(std::net::Shutdown::Both);
        let _ = gs_a.shutdown(std::net::Shutdown::Both);
    });

    let _ = std::fs::remove_file(&shm);
}

// ── SPIR-V builders (fullscreen tri VS + const-colour FS) ───────────────
// Self-contained copies of the proven builders (private in src/moltenvk.rs).

fn build_fullscreen_tri_vs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::dr::Operand;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32t = b.type_float(32, None);
    let i32t = b.type_int(32, 1);
    let v4 = b.type_vector(f32t, 4);
    let void_fn = b.type_function(void, vec![]);
    let per_vertex = b.type_struct(vec![v4]);
    b.member_decorate(per_vertex, 0, Decoration::BuiltIn,
        vec![Operand::BuiltIn(BuiltIn::Position)]);
    b.member_decorate(per_vertex, 0, Decoration::Offset, vec![Operand::LiteralBit32(0)]);
    b.decorate(per_vertex, Decoration::Block, vec![]);
    let ptr_pv = b.type_pointer(None, StorageClass::Output, per_vertex);
    let ptr_out_v4 = b.type_pointer(None, StorageClass::Output, v4);
    let ptr_in_i32 = b.type_pointer(None, StorageClass::Input, i32t);
    let in_idx = b.variable(ptr_in_i32, None, StorageClass::Input, None);
    b.decorate(in_idx, Decoration::BuiltIn, vec![Operand::BuiltIn(BuiltIn::VertexIndex)]);
    let pv_var = b.variable(ptr_pv, None, StorageClass::Output, None);
    let c0i = b.constant_bit32(i32t, 0);
    let c1i = b.constant_bit32(i32t, 1);
    let c2i = b.constant_bit32(i32t, 2);
    let c2f = b.constant_bit32(f32t, 2.0f32.to_bits());
    let c1f = b.constant_bit32(f32t, 1.0f32.to_bits());
    let c0f = b.constant_bit32(f32t, 0.0f32.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let idx = b.load(i32t, None, in_idx, None, vec![]).unwrap();
    let sh = b.shift_left_logical(i32t, None, idx, c1i).unwrap();
    let xb = b.bitwise_and(i32t, None, sh, c2i).unwrap();
    let yb = b.bitwise_and(i32t, None, idx, c2i).unwrap();
    let xf = b.convert_s_to_f(f32t, None, xb).unwrap();
    let yf = b.convert_s_to_f(f32t, None, yb).unwrap();
    let xm = b.f_mul(f32t, None, xf, c2f).unwrap();
    let x = b.f_sub(f32t, None, xm, c1f).unwrap();
    let ym = b.f_mul(f32t, None, yf, c2f).unwrap();
    let y = b.f_sub(f32t, None, ym, c1f).unwrap();
    let pos = b.composite_construct(v4, None, vec![x, y, c0f, c1f]).unwrap();
    let dst = b.access_chain(ptr_out_v4, None, pv_var, vec![c0i]).unwrap();
    b.store(dst, pos, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Vertex, main, "main", vec![in_idx, pv_var]);
    b.module().assemble().iter().flat_map(|w| w.to_le_bytes()).collect()
}

fn build_const_fs(rgba: [f32; 4]) -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::dr::Operand;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode, ExecutionModel,
        FunctionControl, MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32t = b.type_float(32, None);
    let v4 = b.type_vector(f32t, 4);
    let void_fn = b.type_function(void, vec![]);
    let ptr_out = b.type_pointer(None, StorageClass::Output, v4);
    let cs: Vec<_> = rgba.iter().map(|x| b.constant_bit32(f32t, x.to_bits())).collect();
    let color = b.constant_composite(v4, cs);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location, vec![Operand::LiteralBit32(0)]);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    b.store(out, color, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    b.module().assemble().iter().flat_map(|w| w.to_le_bytes()).collect()
}
