//! Tier-3 blend capture: render the EXACT SrcOver our Tier-2 `apply_blend`
//! uses (color SrcAlpha / OneMinusSrcAlpha, alpha One / OneMinusSrcAlpha,
//! ADD) through Metal's fixed-function blend unit (MoltenVK), and diff the
//! resulting bytes against our float CPU blend.
//!
//! Purpose: the energy router certifies Tier-2 ≡ Tier-3 pixel-identical
//! before hot-swapping.  We've been guarding "Tier-2 matches its own FLOAT
//! path" — which is self-referential and proves nothing about the GPU.  This
//! tells us whether there is a real equivalence ERROR (float CPU blend vs
//! Metal's UNORM8 blend) and, if so, exactly how Metal rounds — so we can
//! make the CPU path match the GPU (and vectorize it as integer SIMD).
//!
//! Method: a 1×1 R8G8B8A8_UNORM image cleared to the destination D, then a
//! fullscreen triangle whose FS outputs the source S=(rgb,α) via push
//! constant, with the SrcOver blend pipeline enabled.  The read-back byte is
//! Metal's `SrcOver(S, D)`.  Compare to our `f32_to_u8(srcover_float(S, D))`.
//!
//! Run (host, MoltenVK):
//!   DYLD_LIBRARY_PATH=/opt/homebrew/lib \
//!   cargo run -p aqueduct-gpu-host --example blend_capture --release

use aqueduct_gpu_host::{Backend, MoltenVkBackend};
use aqueduct_gpu::ids::{IdNamespace, ResourceId};

// ── our reference: the exact CPU float SrcOver + quantise ──
fn f32_to_u8(v: f32) -> u8 { (v * 255.0 + 0.5) as u8 } // saturating, matches tier-2
fn cpu_srcover(s: [f32; 4], d: [f32; 4]) -> [u8; 4] {
    let sa = s[3];
    let oms = 1.0 - sa;
    [
        f32_to_u8(sa * s[0] + oms * d[0]),
        f32_to_u8(sa * s[1] + oms * d[1]),
        f32_to_u8(sa * s[2] + oms * d[2]),
        f32_to_u8(1.0 * sa  + oms * d[3]),
    ]
}

fn main() {
    let be = match MoltenVkBackend::new() {
        Ok(b) => b,
        Err(e) => { eprintln!("MoltenVK unavailable: {e:?} (need DYLD_LIBRARY_PATH=/opt/homebrew/lib)"); std::process::exit(1); }
    };
    println!("{}", be.device_summary());

    let vs = build_fullscreen_tri_vs();
    let fs = build_push_constant_fs();
    let img = ResourceId::new(IdNamespace::IcdRuntime, 1);
    let buf = ResourceId::new(IdNamespace::IcdRuntime, 2);
    be.image_created(img, 1, 1);
    be.buffer_created(buf, 4);

    // Source colours to sweep (glyph white + a couple of arbitrary colours),
    // destinations (opaque, da=255), alpha 0..=255.
    let srcs: [[u8; 3]; 3] = [[230, 230, 235], [64, 192, 128], [200, 100, 50]];
    let dsts: [[u8; 4]; 4] = [
        [0, 0, 0, 255], [255, 255, 255, 255],
        [128, 128, 128, 255], [37, 113, 200, 255],
    ];

    let mut total = 0u64;
    let mut mism = 0u64;
    let mut max_diff = 0i32;
    let mut examples: Vec<String> = Vec::new();
    // Histogram of signed per-channel diff (metal - ours), clamped to [-4,4].
    let mut hist = [0u64; 9];

    for sc in srcs {
        for d in dsts {
            for a in 0u32..=255 {
                let s_f = [sc[0] as f32 / 255.0, sc[1] as f32 / 255.0,
                           sc[2] as f32 / 255.0, a as f32 / 255.0];
                let d_f = [d[0] as f32 / 255.0, d[1] as f32 / 255.0,
                           d[2] as f32 / 255.0, d[3] as f32 / 255.0];
                // push: vec4 src as f32 LE
                let mut push = [0u8; 16];
                for (k, v) in s_f.iter().enumerate() {
                    push[k * 4..k * 4 + 4].copy_from_slice(&v.to_le_bytes());
                }
                if let Err(e) = be.draw_and_copy_full(
                    img, buf, &vs, &fs, 3, d, &push, None, None, /*blend_srcover=*/ true)
                {
                    eprintln!("draw failed: {e:?}"); std::process::exit(1);
                }
                let metal = be.buffer_read_bytes(buf, 0, 4).expect("readback");
                let ours = cpu_srcover(s_f, d_f);
                for ch in 0..4 {
                    total += 1;
                    let diff = metal[ch] as i32 - ours[ch] as i32;
                    if diff != 0 {
                        mism += 1;
                        max_diff = max_diff.max(diff.abs());
                        let bucket = (diff.clamp(-4, 4) + 4) as usize;
                        hist[bucket] += 1;
                        if examples.len() < 12 {
                            examples.push(format!(
                                "  S={sc:?} α={a} D={d:?} ch{ch}: metal={} ours={} (Δ{diff:+})",
                                metal[ch], ours[ch]));
                        }
                    }
                }
            }
        }
    }

    println!("\n── Tier-3 (Metal) SrcOver vs Tier-2 float apply_blend ──");
    println!("  channels compared : {total}");
    println!("  mismatches        : {mism}  ({:.2}%)", 100.0 * mism as f64 / total as f64);
    println!("  max |Δ| (bytes)   : {max_diff}");
    println!("  signed Δ histogram (metal-ours), Δ clamped to ±4:");
    for (i, c) in hist.iter().enumerate() {
        if *c > 0 { println!("    Δ={:+}: {c}", i as i32 - 4); }
    }
    if !examples.is_empty() {
        println!("  examples:");
        for e in &examples { println!("{e}"); }
    }
    if mism == 0 {
        println!("\n  ⇒ NO equivalence error: our float blend already byte-matches Metal.");
    } else {
        println!("\n  ⇒ EQUIVALENCE ERROR: {mism} channels differ (max {max_diff} byte). \
                  Our float blend is NOT what Metal produces — the certify path is \
                  comparing against the wrong oracle.");
    }
}

// ── SPIR-V: fullscreen triangle VS (gl_VertexIndex → clip pos). ──
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

// ── SPIR-V: FS that outputs the push-constant vec4 (the source S). ──
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
