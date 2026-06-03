//! End-to-end live routing: a real `GpuClient` drives the full
//! `client → Session → RoutingBackend(Tier2Backend + MoltenVk)` path over a
//! socket — the same wire Carillon carries faithfully to the FreeBSD VM.
//! A sustained heavy frame should migrate the surface from Tier-2 (home) to
//! Tier-3, and the read-back pixels must be correct on whichever tier
//! rendered (seamless = correct pixels, no chatter).
//!
//! Gated on a MoltenVK loader + the atrium-spv-compile toolchain (Tier-2
//! shader compilation). Skips cleanly when either is absent.
//! Run with `DYLD_LIBRARY_PATH=/opt/homebrew/lib`.

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use aqueduct::Connection;
use aqueduct_gpu::{
    backends::BackendId, ids::ResourceId, opcodes::FrameOp,
    payloads::{BufferCreatePayload, ClientKind, ImageCreatePayload, MemoryUsage, PipelineKind, ShaderKind},
};
use aqueduct_gpu_client::GpuClient;
use aqueduct_gpu_host::{
    Backend, CpuProfile, DeviceProfile, GpuPowerModel, Listener, MoltenVkBackend, RouteMode,
    RoutingBackend, Tier2Backend, Tier2Registry,
};
use atrium_spv_loader::LoaderConfig;

fn locate_compile_binary() -> Option<PathBuf> {
    let mut p = std::env::current_exe().ok()?;
    for _ in 0..5 { p.pop(); }
    p.push("atrium-spv-compile");
    p.push("target");
    p.push("debug");
    p.push("atrium-spv-compile");
    p.exists().then_some(p)
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

#[test]
fn routing_migrates_a_heavy_surface_and_reads_back_correctly() {
    let mvk = match MoltenVkBackend::new() {
        Ok(b) => b,
        Err(_) => { eprintln!("SKIP routing_live: MoltenVK unavailable"); return; }
    };
    let Some(compile) = locate_compile_binary() else {
        eprintln!("SKIP routing_live: atrium-spv-compile unavailable"); return;
    };
    let cache = tempfile::TempDir::new().unwrap();
    let registry = Arc::new(Tier2Registry::new(LoaderConfig {
        cache_root: cache.path().to_path_buf(),
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: compile,
    }));

    // The routing backend over real Tier-2 + Tier-3. Keep the concrete Arc
    // to observe the per-surface assignment as frames flow through.
    let t2: Arc<dyn Backend> = Arc::new(Tier2Backend::new(registry.clone()));
    let t3: Arc<dyn Backend> = Arc::new(mvk);
    let rb = Arc::new(RoutingBackend::new(
        t2, t3, DeviceProfile::uma_apple_m4_max(), CpuProfile::apple_m4_max(),
        GpuPowerModel::apple_m4_max(), RouteMode::Perf).with_trusted_tiers());

    const W: u32 = 512;
    const H: u32 = 512;
    let sock = {
        let mut p = std::env::temp_dir();
        p.push(format!("aqueduct-routing-{}.sock", std::process::id()));
        p
    };
    let listener = Listener::bind(&sock, rb.clone() as Arc<dyn Backend>)
        .unwrap()
        .with_tier2_registry(registry.clone());
    let server = thread::spawn(move || { let _ = listener.accept_loop(); });
    thread::sleep(Duration::from_millis(50));

    let conn = Connection::connect(&sock).unwrap();
    let mut client = GpuClient::new(conn);
    let hs = client.handshake(ClientKind::VulkanIcd).unwrap().clone();
    let backend_id: BackendId = hs.backend;

    let img_mem = client.allocate_memory((W * H * 4) as u64, MemoryUsage::ImageBacking).unwrap();
    let image = client.create_image(ImageCreatePayload {
        image_id: ResourceId(0), backing_region: img_mem.region_id, region_offset: 0,
        format: 37, width: W, height: H, depth: 1, mip_levels: 1, array_layers: 1, usage: 0x07,
    }).unwrap();
    let buf_mem = client.allocate_memory((W * H * 4) as u64, MemoryUsage::Staging).unwrap();
    let buffer = client.create_buffer(BufferCreatePayload {
        buffer_id: ResourceId(0), backing_region: buf_mem.region_id, region_offset: 0,
        size: (W * H * 4) as u64, usage: 0x01,
    }).unwrap();

    let vs = build_fullscreen_tri_vs();
    let fs = build_heavy_grey_fs(800); // ~grey, heavy enough to favour Tier-3
    let vs_id = client.upload_shader(sha256(&vs), ShaderKind::SpirV, backend_id, vs).unwrap();
    let fs_id = client.upload_shader(sha256(&fs), ShaderKind::SpirV, backend_id, fs).unwrap();
    let pipe = client.create_pipeline(PipelineKind::Graphics, vec![vs_id, fs_id], Vec::new()).unwrap();
    thread::sleep(Duration::from_millis(50));

    let frame_bytes = |c: &mut GpuClient| {
        let fence = c.create_fence().unwrap();
        let mut fb = c.frame_builder();
        let mut brp = image.raw().to_le_bytes().to_vec();
        brp.extend_from_slice(&[10u8, 10, 10, 255]);
        brp.extend_from_slice(&0u32.to_le_bytes());
        fb.push(FrameOp::BeginRenderPass, &brp).unwrap();
        fb.push(FrameOp::BindPipeline, &pipe.raw().to_le_bytes()).unwrap();
        let mut draw = 3u32.to_le_bytes().to_vec();
        draw.extend_from_slice(&1u32.to_le_bytes());
        draw.extend_from_slice(&[0u8; 8]);
        fb.push(FrameOp::Draw, &draw).unwrap();
        fb.push(FrameOp::EndRenderPass, &[]).unwrap();
        let mut cib = image.raw().to_le_bytes().to_vec();
        cib.extend_from_slice(&buffer.raw().to_le_bytes());
        cib.extend_from_slice(&0u32.to_le_bytes());
        cib.extend_from_slice(&1u32.to_le_bytes());
        let mut region = vec![0u8; 56];
        region[44..48].copy_from_slice(&W.to_le_bytes());
        region[48..52].copy_from_slice(&H.to_le_bytes());
        region[52..56].copy_from_slice(&1u32.to_le_bytes());
        cib.extend_from_slice(&region);
        fb.push(FrameOp::CopyImgToBuf, &cib).unwrap();
        (fence, fb)
    };

    // Drive a sustained run of heavy frames; the surface should migrate to
    // Tier-3 over the residency timescale.
    let mut migrated_at = None;
    for t in 1..=16u64 {
        let (fence, fb) = frame_bytes(&mut client);
        client.submit_frame(fence, fb, t).unwrap();
        assert!(client.wait_fence(fence, 30_000_000_000).unwrap(), "frame {t} fence");
        let (_t2c, t3c) = rb.assignment_counts();
        if t3c > 0 && migrated_at.is_none() { migrated_at = Some(t); }
    }
    let (scored, skipped) = rb.decision_stats();
    let (a2, a3) = rb.assignment_counts();
    eprintln!("routing live: scored={scored} skipped={skipped} surfaces tier2={a2} tier3={a3} migrated_at={migrated_at:?}");

    // Read back the final frame's pixels — correct on whichever tier rendered.
    let px = client.read_buffer(buffer, 0, (W * H * 4) as u64).expect("readback");
    let i = ((H as usize / 2) * W as usize + W as usize / 2) * 4;
    // build_heavy_grey_fs(800): 0.1 + 0.001*800 = 0.9 → ~230 per channel.
    eprintln!("routing live: centre pixel = {:?}", &px[i..i + 4]);
    assert!(px[i] > 210 && px[i] < 245 && px[i + 3] == 255,
        "rendered grey ~230 regardless of tier, got {:?}", &px[i..i + 4]);

    // The surface was eligible (trusted) and the heavy workload migrated it.
    assert!(scored > 0, "eligible surface was scored");
    assert_eq!(a3, 1, "the heavy surface migrated to Tier-3");

    drop(client);
    let _ = server;
    let _ = std::fs::remove_file(&sock);
}

// ── SPIR-V builders ──────────────────────────────────────────────────

/// A heavy fragment shader: acc = 0.1, then `n` real FAdds of 0.001 (a true
/// dependent chain, not dead), output grey `(acc,acc,acc,1)`. The op count
/// makes the modeled cost favour Tier-3 over a large frame.
fn build_heavy_grey_fs(n: u32) -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode, ExecutionModel, FunctionControl,
        MemoryModel, StorageClass,
    };
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32t = b.type_float(32, None);
    let vec4 = b.type_vector(f32t, 4);
    let void_fn = b.type_function(void, vec![]);
    let ptr_out = b.type_pointer(None, StorageClass::Output, vec4);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location, vec![rspirv::dr::Operand::LiteralBit32(0)]);
    let start = b.constant_bit32(f32t, 0.1f32.to_bits());
    let delta = b.constant_bit32(f32t, 0.001f32.to_bits());
    let one = b.constant_bit32(f32t, 1.0f32.to_bits());
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let mut acc = start;
    for _ in 0..n {
        acc = b.f_add(f32t, None, acc, delta).unwrap();
    }
    let color = b.composite_construct(vec4, None, vec![acc, acc, acc, one]).unwrap();
    b.store(out, color, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = b.module().assemble();
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

fn build_fullscreen_tri_vs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::dr::Operand;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionModel, FunctionControl,
        MemoryModel, StorageClass,
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
    b.member_decorate(per_vertex, 0, Decoration::BuiltIn, vec![Operand::BuiltIn(BuiltIn::Position)]);
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
    let words: Vec<u32> = b.module().assemble();
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}
