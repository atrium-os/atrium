//! FreeBSD-VM guest pump for the Carillon transport.
//!
//! Mmaps `/dev/carillon0` (the shared BAR2 region), runs the same
//! `carillon-transport` byte-FIFO pumps the host uses — only the doorbell
//! signal/wait is the cdev `ioctl` (RING / WAIT) instead of pipes/kqueue —
//! and drives a real aqueduct-gpu frame through a verbatim `GpuClient`:
//! handshake → create image/buffer → upload VS+FS SPIR-V → create graphics
//! pipeline → submit a draw → read back the green triangle. Every byte
//! crosses the Carillon shared memory + the MSI-X doorbell to the host
//! daemon's `Session` → `MoltenVkBackend` → Metal.
//!
//! Run in the VM after the host daemon + `run-vm.sh --carillon`:
//!   carillon-guest

use std::os::fd::{IntoRawFd, RawFd};
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
use carillon_transport::{layout, pump_fd_to_stream_with, pump_stream_to_fd_with, Region};

mod scanout;

// cdev ioctls (must match carillon-kmod/carillon_abi.h).
//   CARILLON_RING = _IO('C', 1)
//   CARILLON_WAIT = _IOWR('C', 2, struct carillon_wait {u32 timeout_ms; u32 seq;})
const CARILLON_RING: libc::c_ulong = 0x2000_4301;
const CARILLON_WAIT: libc::c_ulong = 0xC008_4302;

#[repr(C)]
struct CarillonWait {
    timeout_ms: u32,
    seq: u32,
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    const W: u32 = 64;
    const H: u32 = 64;

    // Open + mmap the cdev (BAR2 shared region).
    let dev_fd: RawFd = unsafe {
        libc::open(c"/dev/carillon0".as_ptr(), libc::O_RDWR)
    };
    if dev_fd < 0 {
        eprintln!("open /dev/carillon0 failed (is carillon.ko loaded?)");
        return 1;
    }
    // Region::from_raw_fd takes ownership + mmaps the fd. We also need the
    // fd for ioctls, so dup it.
    let ioctl_fd = unsafe { libc::dup(dev_fd) };
    let region = match unsafe { Region::from_raw_fd(dev_fd, layout::TOTAL_SIZE) } {
        Ok(r) => Arc::new(r),
        Err(e) => { eprintln!("mmap /dev/carillon0: {e}"); return 1; }
    };
    if region.validate_header().is_err() {
        eprintln!("bad control header — host daemon not serving?");
        return 1;
    }
    region.set_guest_status(1);
    println!("carillon-guest: mapped BAR2, host_page_size={}",
             region.host_page_size_field());

    // GpuClient ⇄ guest-bridge socketpair.
    let (gs_a, gs_b) = match UnixStream::pair() {
        Ok(p) => p,
        Err(e) => { eprintln!("socketpair: {e}"); return 1; }
    };
    let gs_a_fd = gs_a.into_raw_fd();

    // Guest req-pump: GpuClient socket → g2h FIFO; ring host via ioctl RING.
    {
        let region = region.clone();
        thread::spawn(move || {
            pump_fd_to_stream_with(
                gs_a_fd, &region, layout::STREAM_G2H_OFFSET,
                layout::C_STREAM_G2H_HEAD, layout::C_STREAM_G2H_TAIL,
                || { unsafe { libc::ioctl(ioctl_fd, CARILLON_RING); } },
            );
        });
    }
    // Guest resp-pump: h2g FIFO → GpuClient socket; wait on host doorbell
    // via ioctl WAIT (MSI-X-backed). seq carried in/out so no lost wakeup.
    {
        let region = region.clone();
        thread::spawn(move || {
            let mut seq = 0u32;
            pump_stream_to_fd_with(
                gs_a_fd, &region, layout::STREAM_H2G_OFFSET,
                layout::C_STREAM_H2G_HEAD, layout::C_STREAM_H2G_TAIL,
                move || {
                    let mut cw = CarillonWait { timeout_ms: 1000, seq };
                    unsafe { libc::ioctl(ioctl_fd, CARILLON_WAIT, &mut cw); }
                    seq = cw.seq;
                    true
                },
            );
        });
    }

    // ── Drive a real frame through GpuClient (over the Carillon bridge) ──
    let mut client = GpuClient::new(match Connection::wrap(gs_b) {
        Ok(c) => c,
        Err(e) => { eprintln!("wrap: {e}"); return 1; }
    });
    let hs = client.handshake(ClientKind::VulkanIcd).unwrap().clone();
    let backend_id: BackendId = hs.backend;
    println!("carillon-guest: handshake ok, backend vendor={:?}", backend_id.vendor);

    // Routing demo: drive sustained heavy frames so the host RoutingBackend
    // migrates this surface Tier-2 → Tier-3 (watch the host daemon log).
    if std::env::var("CARILLON_ROUTING").is_ok() {
        return routing_demo(&mut client, backend_id);
    }

    // Scanout demo: render a shape on the host (through the router) and put
    // it on the VM's actual display via the D0 scanout path — the stack's
    // output made *visible* instead of read back headless.
    if std::env::var("CARILLON_SCANOUT").is_ok() {
        return scanout_demo(&mut client, backend_id);
    }

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
    let fs = build_const_fs([0.2, 0.85, 0.3, 1.0]); // green
    let vs_id = client.upload_shader(sha256(&vs), ShaderKind::SpirV, backend_id, vs).unwrap();
    let fs_id = client.upload_shader(sha256(&fs), ShaderKind::SpirV, backend_id, fs).unwrap();
    let pipe = client.create_pipeline(PipelineKind::Graphics, vec![vs_id, fs_id], Vec::new()).unwrap();

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
    for v in [3u32, 1, 0, 0] { draw.extend_from_slice(&v.to_le_bytes()); }
    fb.push(FrameOp::Draw, &draw).unwrap();
    fb.push(FrameOp::EndRenderPass, &[]).unwrap();
    let mut cib = Vec::new();
    cib.extend_from_slice(&image.raw().to_le_bytes());
    cib.extend_from_slice(&buffer.raw().to_le_bytes());
    cib.extend_from_slice(&0u32.to_le_bytes());
    cib.extend_from_slice(&1u32.to_le_bytes());
    let mut reg = vec![0u8; 56];
    reg[44..48].copy_from_slice(&W.to_le_bytes());
    reg[48..52].copy_from_slice(&H.to_le_bytes());
    reg[52..56].copy_from_slice(&1u32.to_le_bytes());
    cib.extend_from_slice(&reg);
    fb.push(FrameOp::CopyImgToBuf, &cib).unwrap();

    client.submit_frame(fence, fb, 1).unwrap();
    if !client.wait_fence(fence, 5_000_000_000).unwrap() {
        eprintln!("FAIL: fence never signalled");
        return 2;
    }
    let px = match client.read_buffer(buffer, 0, (W * H * 4) as u64) {
        Ok(p) => p,
        Err(e) => { eprintln!("FAIL: readback: {e}"); return 2; }
    };
    let i = ((H as usize / 2) * W as usize + W as usize / 2) * 4;
    println!("carillon-guest: centre pixel = {:?}", &px[i..i + 4]);
    if px[i] < 90 && px[i + 1] > 180 && px[i + 2] < 110 && px[i + 3] == 255 {
        println!("ROUND-TRIP OK: green triangle rendered on the host GPU, \
                  delivered through Carillon to the VM");
        0
    } else {
        eprintln!("FAIL: centre pixel is not the green triangle");
        2
    }
}

/// Drive a sustained run of heavy frames through one surface so the host
/// RoutingBackend migrates it Tier-2 → Tier-3. The migration is reported in
/// the host daemon log ("routing: frame N → … surfaces tier2=… tier3=…").
fn routing_demo(client: &mut GpuClient, backend_id: BackendId) -> i32 {
    const RW: u32 = 512;
    const RH: u32 = 512;
    let img_mem = client.allocate_memory((RW * RH * 4) as u64, MemoryUsage::ImageBacking).unwrap();
    let image = client.create_image(ImageCreatePayload {
        image_id: ResourceId(0), backing_region: img_mem.region_id, region_offset: 0,
        format: 37, width: RW, height: RH, depth: 1, mip_levels: 1, array_layers: 1, usage: 0x07,
    }).unwrap();
    let buf_mem = client.allocate_memory((RW * RH * 4) as u64, MemoryUsage::Staging).unwrap();
    let buffer = client.create_buffer(BufferCreatePayload {
        buffer_id: ResourceId(0), backing_region: buf_mem.region_id, region_offset: 0,
        size: (RW * RH * 4) as u64, usage: 0x01,
    }).unwrap();
    let vs = build_fullscreen_tri_vs();
    let fs = build_heavy_grey_fs(800);
    let vs_id = client.upload_shader(sha256(&vs), ShaderKind::SpirV, backend_id, vs).unwrap();
    let fs_id = client.upload_shader(sha256(&fs), ShaderKind::SpirV, backend_id, fs).unwrap();
    let pipe = client.create_pipeline(PipelineKind::Graphics, vec![vs_id, fs_id], Vec::new()).unwrap();
    thread::sleep(Duration::from_millis(50));
    println!("carillon-guest: ROUTING DEMO — 16 heavy 512x512 frames; \
              watch the host daemon log for the Tier-2→Tier-3 migration");

    for t in 1..=16u64 {
        let fence = client.create_fence().unwrap();
        let mut fb = client.frame_builder();
        let mut brp = image.raw().to_le_bytes().to_vec();
        brp.extend_from_slice(&[10u8, 10, 10, 255]);
        brp.extend_from_slice(&0u32.to_le_bytes());
        fb.push(FrameOp::BeginRenderPass, &brp).unwrap();
        fb.push(FrameOp::BindPipeline, &pipe.raw().to_le_bytes()).unwrap();
        let mut draw = Vec::new();
        for v in [3u32, 1, 0, 0] { draw.extend_from_slice(&v.to_le_bytes()); }
        fb.push(FrameOp::Draw, &draw).unwrap();
        fb.push(FrameOp::EndRenderPass, &[]).unwrap();
        let mut cib = image.raw().to_le_bytes().to_vec();
        cib.extend_from_slice(&buffer.raw().to_le_bytes());
        cib.extend_from_slice(&0u32.to_le_bytes());
        cib.extend_from_slice(&1u32.to_le_bytes());
        let mut reg = vec![0u8; 56];
        reg[44..48].copy_from_slice(&RW.to_le_bytes());
        reg[48..52].copy_from_slice(&RH.to_le_bytes());
        reg[52..56].copy_from_slice(&1u32.to_le_bytes());
        cib.extend_from_slice(&reg);
        fb.push(FrameOp::CopyImgToBuf, &cib).unwrap();
        client.submit_frame(fence, fb, t).unwrap();
        if !client.wait_fence(fence, 30_000_000_000).unwrap() {
            eprintln!("FAIL: frame {t} fence never signalled");
            return 2;
        }
        println!("carillon-guest: frame {t} done");
    }

    // Tiny centre readback (4 bytes) — verify the grey rendered on whatever
    // tier the surface migrated to.
    let off = ((RH as u64 / 2) * RW as u64 + RW as u64 / 2) * 4;
    let px = match client.read_buffer(buffer, off, 4) {
        Ok(p) => p,
        Err(e) => { eprintln!("FAIL: readback: {e}"); return 2; }
    };
    println!("carillon-guest: centre pixel = {:?}", px);
    if px[0] > 210 && px[0] < 245 && px[3] == 255 {
        println!("ROUTING DEMO OK: heavy grey rendered + delivered through Carillon \
                  (migration tier in the host daemon log)");
        0
    } else {
        eprintln!("FAIL: centre pixel not the expected grey (~230)");
        2
    }
}

/// Render a shape on the *host* (through the energy router) and put it on the
/// VM's real display via the D0 scanout path. Renders at the connector's
/// native resolution so the diamond fills the screen; reads the frame back
/// over Carillon and page-flips it onto the QEMU Cocoa window. Holds the
/// frame on screen (re-flipping) so it stays visible.
fn scanout_demo(client: &mut GpuClient, backend_id: BackendId) -> i32 {
    let sc = match scanout::Scanout::open() {
        Ok(s) => s,
        Err(e) => { eprintln!("FAIL: scanout open: {e}"); return 3; }
    };
    let (w, h) = (sc.width, sc.height);
    println!("carillon-guest: scanout {w}x{h} — rendering a shape on the host \
              through the router, presenting to the VM display");

    let bytes = (w * h * 4) as u64;
    let img_mem = client.allocate_memory(bytes, MemoryUsage::ImageBacking).unwrap();
    let image = client.create_image(ImageCreatePayload {
        image_id: ResourceId(0), backing_region: img_mem.region_id, region_offset: 0,
        format: 37, width: w, height: h, depth: 1, mip_levels: 1, array_layers: 1, usage: 0x07,
    }).unwrap();
    let buf_mem = client.allocate_memory(bytes, MemoryUsage::Staging).unwrap();
    let buffer = client.create_buffer(BufferCreatePayload {
        buffer_id: ResourceId(0), backing_region: buf_mem.region_id, region_offset: 0,
        size: bytes, usage: 0x01,
    }).unwrap();

    // Nested coloured squares, drawn with proven primitives only: the
    // vertex-less full-screen triangle + a constant-colour FS, one pipeline
    // per colour, each draw clipped to a scissor rect. (Varyings / FragCoord /
    // A procedural shape from a real fragment shader: the vertex-less
    // full-screen triangle + a gl_FragCoord diamond FS (orange diamond on a
    // blue/green gradient, GLSL.std.450 FAbs/FClamp/FMix). gl_FragCoord is now
    // supported on the compiled Tier-2 path (both per-pixel and the vectorized
    // span path), so the shape comes straight out of the shader.
    let vs = build_fullscreen_tri_vs();
    let fs = build_fragcoord_diamond_fs(w, h);
    let vs_id = client.upload_shader(sha256(&vs), ShaderKind::SpirV, backend_id, vs).unwrap();
    let fs_id = client.upload_shader(sha256(&fs), ShaderKind::SpirV, backend_id, fs).unwrap();
    let pipe = client.create_pipeline(PipelineKind::Graphics, vec![vs_id, fs_id], Vec::new()).unwrap();
    thread::sleep(Duration::from_millis(50));

    let render_one = |client: &mut GpuClient, t: u64| -> Result<Vec<u8>, String> {
        let fence = client.create_fence().map_err(|e| e.to_string())?;
        let mut fb = client.frame_builder();
        let mut brp = image.raw().to_le_bytes().to_vec();
        brp.extend_from_slice(&[10u8, 10, 14, 255]); // dark clear
        brp.extend_from_slice(&0u32.to_le_bytes());
        fb.push(FrameOp::BeginRenderPass, &brp).unwrap();
        fb.push(FrameOp::BindPipeline, &pipe.raw().to_le_bytes()).unwrap();
        let mut draw = Vec::new();
        for v in [3u32, 1, 0, 0] { draw.extend_from_slice(&v.to_le_bytes()); }
        fb.push(FrameOp::Draw, &draw).unwrap();
        fb.push(FrameOp::EndRenderPass, &[]).unwrap();
        let mut cib = image.raw().to_le_bytes().to_vec();
        cib.extend_from_slice(&buffer.raw().to_le_bytes());
        cib.extend_from_slice(&0u32.to_le_bytes());
        cib.extend_from_slice(&1u32.to_le_bytes());
        let mut reg = vec![0u8; 56];
        reg[44..48].copy_from_slice(&w.to_le_bytes());
        reg[48..52].copy_from_slice(&h.to_le_bytes());
        reg[52..56].copy_from_slice(&1u32.to_le_bytes());
        cib.extend_from_slice(&reg);
        fb.push(FrameOp::CopyImgToBuf, &cib).unwrap();
        client.submit_frame(fence, fb, t).map_err(|e| e.to_string())?;
        if !client.wait_fence(fence, 30_000_000_000).map_err(|e| e.to_string())? {
            return Err(format!("frame {t} fence never signalled"));
        }
        client.read_buffer(buffer, 0, bytes).map_err(|e| e.to_string())
    };

    // Render once on the host (through the router), read it back, present it.
    let px = match render_one(client, 1) {
        Ok(p) => p,
        Err(e) => { eprintln!("FAIL: render: {e}"); return 2; }
    };
    if let Err(e) = sc.present_rgba(&px) {
        eprintln!("FAIL: present: {e}");
        return 3;
    }
    println!("carillon-guest: SHAPE ON SCREEN — gl_FragCoord diamond, \
              rendered on the host through the router, shown on the VM display");

    // Hold it on screen (re-flip the same frame) so the window stays up.
    // Ctrl-C / kill to exit; ~60 s then return cleanly.
    for _ in 0..120 {
        let _ = sc.present_rgba(&px);
        thread::sleep(Duration::from_millis(500));
    }
    0
}

/// Heavy FS: acc = 0.1 then `n` real FAdds of 0.001 (a true dependent chain),
/// output grey (acc,acc,acc,1). The op count makes the modeled cost favour
/// Tier-3 over a 512x512 frame.
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
    for _ in 0..n { acc = b.f_add(f32t, None, acc, delta).unwrap(); }
    let color = b.composite_construct(vec4, None, vec![acc, acc, acc, one]).unwrap();
    b.store(out, color, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = b.module().assemble();
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

// ── SPIR-V builders (fullscreen tri VS + const-colour FS) ───────────────

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
    b.module().assemble().iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// A `gl_FragCoord` fragment shader: an orange diamond on a blue/green
/// gradient. `w`/`h` (render-target size) are baked in to normalise the
/// pixel coordinate. Uses GLSL.std.450 `FAbs` / `FClamp` / `FMix`. No vertex
/// buffer — geometry comes from the vertex-less full-screen triangle and the
/// shape from the fragment coordinate.
fn build_fragcoord_diamond_fs(w: u32, h: u32) -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::dr::Operand;
    use rspirv::spirv::{
        AddressingModel, BuiltIn, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    const FABS: u32 = 4;
    const FCLAMP: u32 = 43;
    const FMIX: u32 = 46;
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    let glsl = b.ext_inst_import("GLSL.std.450");
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f32t = b.type_float(32, None);
    let v3 = b.type_vector(f32t, 3);
    let v4 = b.type_vector(f32t, 4);
    let void_fn = b.type_function(void, vec![]);
    let ptr_in_v4 = b.type_pointer(None, StorageClass::Input, v4);
    let fragcoord = b.variable(ptr_in_v4, None, StorageClass::Input, None);
    b.decorate(fragcoord, Decoration::BuiltIn, vec![Operand::BuiltIn(BuiltIn::FragCoord)]);
    let ptr_out = b.type_pointer(None, StorageClass::Output, v4);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location, vec![Operand::LiteralBit32(0)]);
    let k = |b: &mut rspirv::dr::Builder, x: f32| b.constant_bit32(f32t, x.to_bits());
    let inv_w = k(&mut b, 1.0 / w as f32);
    let inv_h = k(&mut b, 1.0 / h as f32);
    let half = k(&mut b, 0.5);
    let edge = k(&mut b, 0.30);
    let sharp = k(&mut b, 12.0);
    let zero = k(&mut b, 0.0);
    let one = k(&mut b, 1.0);
    let bg_rx = k(&mut b, 0.35);
    let bg_gy = k(&mut b, 0.55);
    let bg_b = k(&mut b, 0.75);
    let fg_r = k(&mut b, 1.0);
    let fg_g = k(&mut b, 0.55);
    let fg_b = k(&mut b, 0.10);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let fc = b.load(v4, None, fragcoord, None, vec![]).unwrap();
    let fx = b.composite_extract(f32t, None, fc, vec![0]).unwrap();
    let fy = b.composite_extract(f32t, None, fc, vec![1]).unwrap();
    let u = b.f_mul(f32t, None, fx, inv_w).unwrap();
    let v = b.f_mul(f32t, None, fy, inv_h).unwrap();
    let du = b.f_sub(f32t, None, u, half).unwrap();
    let dv = b.f_sub(f32t, None, v, half).unwrap();
    let adx = b.ext_inst(f32t, None, glsl, FABS, vec![Operand::IdRef(du)]).unwrap();
    let ady = b.ext_inst(f32t, None, glsl, FABS, vec![Operand::IdRef(dv)]).unwrap();
    let diamond = b.f_add(f32t, None, adx, ady).unwrap();
    let em = b.f_sub(f32t, None, edge, diamond).unwrap();
    let scaled = b.f_mul(f32t, None, em, sharp).unwrap();
    let t = b.ext_inst(f32t, None, glsl, FCLAMP,
        vec![Operand::IdRef(scaled), Operand::IdRef(zero), Operand::IdRef(one)]).unwrap();
    let br = b.f_mul(f32t, None, u, bg_rx).unwrap();
    let bgc = b.f_mul(f32t, None, v, bg_gy).unwrap();
    let bg = b.composite_construct(v3, None, vec![br, bgc, bg_b]).unwrap();
    let fg = b.composite_construct(v3, None, vec![fg_r, fg_g, fg_b]).unwrap();
    let tv = b.composite_construct(v3, None, vec![t, t, t]).unwrap();
    let rgb = b.ext_inst(v3, None, glsl, FMIX,
        vec![Operand::IdRef(bg), Operand::IdRef(fg), Operand::IdRef(tv)]).unwrap();
    let rr = b.composite_extract(f32t, None, rgb, vec![0]).unwrap();
    let gg = b.composite_extract(f32t, None, rgb, vec![1]).unwrap();
    let bb = b.composite_extract(f32t, None, rgb, vec![2]).unwrap();
    let color = b.composite_construct(v4, None, vec![rr, gg, bb, one]).unwrap();
    b.store(out, color, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![fragcoord, out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
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
