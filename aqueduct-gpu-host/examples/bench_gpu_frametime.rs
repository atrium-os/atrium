//! bench_gpu_frametime — real host-GPU (M4 Max via MoltenVK) per-frame render
//! time, the GPU analog of the Tier-2 compositor's per-frame CPU cost.
//!
//! Measures how long the GPU takes to render+resolve a full-screen pass at
//! desktop resolutions, via the backend's built-in GPU timestamp queries
//! (`measured_gpu_time_s`, the silicon-calibration hook for the cost model).
//! A trivial fill-rate-bound fragment shader = the *floor*: this is the cheapest
//! a real frame can be, so if even this approaches the vblank budget the GPU is
//! the bottleneck; if it's far under (it will be), the GPU is not the desktop
//! frame-pacing lever — the CPU compositor is (bench_tier2_tiled).
//!
//! The HEAVY anchor is not synthesised here — Orbis already renders a real 3D
//! frame on this exact GPU at ~11 ms @ 720p (Tier-3 MoltenVK). That is the
//! regime where GPU render-timing bites; this bench establishes the floor.
//!
//! Run: DYLD_LIBRARY_PATH=/opt/homebrew/lib cargo run -p aqueduct-gpu-host \
//!        --release --example bench_gpu_frametime

use std::time::Instant;

use aqueduct_gpu::ids::{IdNamespace, ResourceId};
use aqueduct_gpu_host::{Backend, MoltenVkBackend};

fn main() {
    let be = match MoltenVkBackend::new() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("MoltenVK unavailable: {e:?} (need DYLD_LIBRARY_PATH=/opt/homebrew/lib)");
            std::process::exit(1);
        }
    };
    println!("{}", be.device_summary());

    let vs = build_fullscreen_tri_vs();
    let fs = build_push_constant_fs();
    let push: [u8; 16] = {
        let mut p = [0u8; 16];
        for (k, v) in [0.2f32, 0.4, 0.8, 1.0].iter().enumerate() {
            p[k * 4..k * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        p
    };

    // 60 Hz = 16.67 ms; 120 Hz = 8.33 ms.
    let budget60 = 16.667;
    let budget120 = 8.333;
    println!("\n  full-screen fill, GPU render+resolve time (trivial FS = fill-rate floor)");
    println!("  {:>12}  {:>10}  {:>11}  {:>10}", "resolution", "gpu-ms", "wall-ms", "vs 60Hz");

    for (w, h, label) in [
        (1280u32, 720u32, "720p"),
        (1920, 1080, "1080p"),
        (2560, 1440, "1440p"),
        (3840, 2160, "4K"),
    ] {
        let img = ResourceId::new(IdNamespace::IcdRuntime, 1);
        let buf = ResourceId::new(IdNamespace::IcdRuntime, 2);
        be.image_created(img, w, h);
        be.buffer_created(buf, (w as u64) * (h as u64) * 4);

        // warm up (pipeline/image/view creation, first-submit costs)
        for _ in 0..5 {
            let _ = be.draw_and_copy_full(img, buf, &vs, &fs, 3, [0, 0, 0, 255], &push, None, None, false);
        }
        let mut gpu_ms: Vec<f64> = Vec::new();
        let mut wall_ms: Vec<f64> = Vec::new();
        for _ in 0..40 {
            let t = Instant::now();
            be.draw_and_copy_full(img, buf, &vs, &fs, 3, [0, 0, 0, 255], &push, None, None, false)
                .expect("draw");
            wall_ms.push(t.elapsed().as_secs_f64() * 1e3);
            gpu_ms.push(be.measured_gpu_time_s() * 1e3);
        }
        let med = |mut v: Vec<f64>| {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v[v.len() / 2]
        };
        let g = med(gpu_ms);
        let wl = med(wall_ms);
        let frac = if g > 0.0 { format!("{:.1}%", 100.0 * g / budget60) } else { "n/a".into() };
        println!("  {label:>6} {w:>4}x{h:<4}  {g:>9.3}  {wl:>10.3}  {frac:>10}");
        let _ = budget120;
    }

    println!("\n  60Hz budget = {budget60:.2} ms   120Hz = {budget120:.2} ms");
    println!("  (gpu-ms = on-GPU render+resolve via timestamp queries; 0.000 = timestamps");
    println!("   unsupported, read wall-ms instead. Heavy anchor: Orbis ~11 ms @720p, real 3D.)");
}

// ── SPIR-V: a full-screen triangle from gl_VertexIndex (no vertex buffer). ──
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
    let words: Vec<u32> = b.module().assemble();
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

// ── SPIR-V: FS that outputs the push-constant vec4 (fill-rate only). ──
fn build_push_constant_fs() -> Vec<u8> {
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
    let i32t = b.type_int(32, 1);
    let vec4 = b.type_vector(f32t, 4);
    let void_fn = b.type_function(void, vec![]);
    let blk = b.type_struct(vec![vec4]);
    b.decorate(blk, Decoration::Block, vec![]);
    b.member_decorate(blk, 0, Decoration::Offset, vec![Operand::LiteralBit32(0)]);
    let ptr_pc_blk = b.type_pointer(None, StorageClass::PushConstant, blk);
    let pc = b.variable(ptr_pc_blk, None, StorageClass::PushConstant, None);
    let ptr_pc_v4 = b.type_pointer(None, StorageClass::PushConstant, vec4);
    let ptr_out = b.type_pointer(None, StorageClass::Output, vec4);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location, vec![Operand::LiteralBit32(0)]);
    let c0i = b.constant_bit32(i32t, 0);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let ac = b.access_chain(ptr_pc_v4, None, pc, vec![c0i]).unwrap();
    let val = b.load(vec4, None, ac, None, vec![]).unwrap();
    b.store(out, val, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    let words: Vec<u32> = b.module().assemble();
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}
