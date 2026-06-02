//! Tier-3 level-2b-iii: the end-to-end ICD→daemon(MoltenVk)→Metal
//! graphics-draw path, driven over a real Unix socket.
//!
//! `tests/end_to_end.rs` proves the socket/client wire against the
//! Stub and Software backends; `src/moltenvk.rs` proves the
//! MoltenVkBackend renders a triangle *in-process*. This test closes
//! the gap: a real `GpuClient` (handshaking as `ClientKind::VulkanIcd`)
//! connects over the socket to a `Listener` bound to a real
//! `MoltenVkBackend`, uploads VS+FS SPIR-V, creates a graphics
//! pipeline, submits a BeginRenderPass/BindPipeline/Draw/EndRenderPass/
//! CopyImgToBuf frame, and reads back the rendered triangle — exercising
//! the exact wire path the daemon serves under `--backend moltenvk`.
//!
//! **Gated.** MoltenVK only exists on the macOS host. When the loader
//! is unavailable (Linux/FreeBSD CI, the VM) `MoltenVkBackend::new()`
//! returns `LoaderUnavailable`; the test prints a skip notice and
//! returns Ok so `cargo test` stays green everywhere.

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
use aqueduct_gpu_host::{Backend, Listener, MoltenVkBackend, MoltenVkError};

fn tmp_socket(name: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("aqueduct-gpu-mvk-{}-{}.sock", std::process::id(), name));
    p
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

#[test]
fn tier3_draw_triangle_icd_to_metal_over_socket() {
    // Gate on a real MoltenVK loader. Skip cleanly off-Metal.
    let backend: Arc<dyn Backend> = match MoltenVkBackend::new() {
        Ok(b) => {
            eprintln!("MoltenVK backend: {}", b.device_summary());
            Arc::new(b)
        }
        Err(MoltenVkError::LoaderUnavailable(e)) => {
            eprintln!("SKIP tier3_draw_triangle_icd_to_metal_over_socket: \
                       MoltenVK loader unavailable ({e})");
            return;
        }
        Err(e) => panic!("MoltenVK init failed: {e}"),
    };

    const W: u32 = 64;
    const H: u32 = 64;
    let sock = tmp_socket("draw");
    let listener = Listener::bind(&sock, backend).unwrap();
    let server = thread::spawn(move || {
        let _ = listener.accept_loop();
    });
    thread::sleep(Duration::from_millis(50));

    let conn = Connection::connect(&sock).unwrap();
    let mut client = GpuClient::new(conn);

    // Handshake as the Vulkan ICD — this is the production client that
    // drives the daemon's MoltenVk backend on the macOS host.
    let hs = client.handshake(ClientKind::VulkanIcd).unwrap().clone();
    let backend_id: BackendId = hs.backend;

    // Render target (device-local on MoltenVk; backing_region is
    // required by the wire payload but ignored by the GPU backend).
    let img_mem = client
        .allocate_memory((W * H * 4) as u64, MemoryUsage::ImageBacking)
        .unwrap();
    let image = client
        .create_image(ImageCreatePayload {
            image_id: ResourceId(0), // overwritten by the client's id pool
            backing_region: img_mem.region_id,
            region_offset: 0,
            format: 37, // VK_FORMAT_R8G8B8A8_UNORM
            width: W,
            height: H,
            depth: 1,
            mip_levels: 1,
            array_layers: 1,
            usage: 0x07,
        })
        .unwrap();

    // Readback buffer.
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

    // Upload VS + FS SPIR-V (cold path — fresh shaders never hit the
    // resolve cache). The session retains the SPIR-V on the
    // ShaderRecord; pipeline_create then forwards both stages to
    // MoltenVkBackend::pipeline_created.
    let vs = build_fullscreen_tri_vs();
    let fs = build_const_fs([0.2, 0.85, 0.3, 1.0]); // green
    let vs_id = client
        .upload_shader(sha256(&vs), ShaderKind::SpirV, backend_id, vs)
        .unwrap();
    let fs_id = client
        .upload_shader(sha256(&fs), ShaderKind::SpirV, backend_id, fs)
        .unwrap();

    // Graphics pipeline. The VkPipeline is materialised lazily on the
    // first draw, keyed by the render target's format.
    let pipe = client
        .create_pipeline(PipelineKind::Graphics, vec![vs_id, fs_id], Vec::new())
        .unwrap();

    // Give the session a beat to dispatch the fire-and-forget
    // image/buffer/pipeline creates into the backend.
    thread::sleep(Duration::from_millis(30));

    // Build the draw frame (byte layouts match the in-process
    // `frameop_draw_replay_through_metal` proof in src/moltenvk.rs).
    let fence = client.create_fence().unwrap();
    let mut fb = client.frame_builder();

    let mut brp = Vec::new();
    brp.extend_from_slice(&image.raw().to_le_bytes());
    brp.extend_from_slice(&[10u8, 10, 10, 255]); // dark clear
    brp.extend_from_slice(&0u32.to_le_bytes()); // flags (CLEAR)
    fb.push(FrameOp::BeginRenderPass, &brp).unwrap();

    fb.push(FrameOp::BindPipeline, &pipe.raw().to_le_bytes()).unwrap();

    let mut draw = Vec::new(); // vcount, icount, first_vertex, first_instance
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
    let signalled = client.wait_fence(fence, 5_000_000_000).unwrap();
    assert!(signalled, "fence should signal after the MoltenVk frame completes");

    // Read back the rendered pixels through the wire.
    let px = client
        .read_buffer(buffer, 0, (W * H * 4) as u64)
        .expect("readback over the socket");
    assert_eq!(px.len(), (W * H * 4) as usize);

    // Centre is covered by the full-screen triangle → the green FS
    // colour (~[51,217,77,255]), NOT the dark clear [10,10,10].
    let i = ((H as usize / 2) * W as usize + W as usize / 2) * 4;
    assert!(
        px[i] < 90 && px[i + 1] > 180 && px[i + 2] < 110 && px[i + 3] == 255,
        "centre should be the drawn (green) triangle, got {:?}",
        &px[i..i + 4]
    );

    drop(client);
    let _ = server;
    let _ = std::fs::remove_file(&sock);
}

// ── SPIR-V shader builders ──────────────────────────────────────────
//
// Self-contained copies of the proven builders in src/moltenvk.rs's
// test module (private there; integration tests can't reach them).
// A fullscreen triangle VS + a constant-colour FS — the minimal
// graphics pipeline that exercises VS→rasteriser→FS on Metal.

/// SPIR-V vertex shader emitting a full-screen triangle from
/// `gl_VertexIndex`: (-1,-1),(3,-1),(-1,3), covering the viewport.
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
    b.member_decorate(per_vertex, 0, Decoration::Offset,
        vec![Operand::LiteralBit32(0)]);
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
    let words: Vec<u32> = b.module().assemble();
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// SPIR-V fragment shader writing a constant colour to Output 0.
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
    let words: Vec<u32> = b.module().assemble();
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}
