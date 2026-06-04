//! Codegen-ceiling bench: how close are tier-2's **bespoke** and
//! **cranelift** backends to LLVM on a compute-heavy fragment shader?
//!
//! "llvm-pipe, for our purposes" = lowering the shader to native with the
//! system `cc -O3` (clang/LLVM) — the §D2 "LLVM-as-library" option, but as a
//! reference subprocess (no permanent LLVM dependency). All three produce a
//! function with the identical `atrium_fs_main` ABI; we call each over
//! millions of pixels through the same loop. This isolates SHADER CODEGEN
//! QUALITY — which the renderer-level bench can't show (it's rasterizer-
//! bound, and the const-colour glyph FS is too trivial to differentiate).
//!
//! The shader is an **input-seeded dependent recurrence with no loop
//! invariants**: cc can't const-fold (depends on frag_coord), can't hoist
//! (nothing invariant), can't vectorise (each step depends on the last). So
//! the gap is pure per-op codegen: instruction selection (fma), sqrt/min
//! lowering, register allocation — exactly the §D2 "bespoke ≈ LLVM" claim.
//!
//! Run with `DYLD_LIBRARY_PATH=/opt/homebrew/lib` (loader dep), release.

use std::time::Instant;
use atrium_spv_loader::{FsMain, LoaderConfig};
use aqueduct_gpu_host::Tier2Registry;

const ITERS: u32 = 64; // per-pixel recurrence steps (unrolled in SPIR-V)
const PIX: usize = 2_000_000; // ~1080p worth of FS invocations

// ── The heavy FS as SPIR-V (rspirv). MUST match HEAVY_FS_C op-for-op. ──
fn build_heavy_fs() -> Vec<u8> {
    use rspirv::binary::Assemble;
    use rspirv::dr::Operand;
    use rspirv::spirv::{
        AddressingModel, Capability, Decoration, ExecutionMode,
        ExecutionModel, FunctionControl, MemoryModel, StorageClass,
    };
    const FABS: u32 = 4;
    const FSQRT: u32 = 31;
    const FMIN: u32 = 37;
    let mut b = rspirv::dr::Builder::new();
    b.set_version(1, 0);
    b.capability(Capability::Shader);
    let glsl = b.ext_inst_import("GLSL.std.450");
    b.memory_model(AddressingModel::Logical, MemoryModel::GLSL450);
    let void = b.type_void();
    let f = b.type_float(32, None);
    let v4 = b.type_vector(f, 4);
    let void_fn = b.type_function(void, vec![]);
    // Per-pixel seed comes through a Location=0 input varying (bespoke
    // supports this; it does not yet support gl_FragCoord in fragment).
    let ptr_in = b.type_pointer(None, StorageClass::Input, v4);
    let fc = b.variable(ptr_in, None, StorageClass::Input, None);
    b.decorate(fc, Decoration::Location, vec![Operand::LiteralBit32(0)]);
    let ptr_out = b.type_pointer(None, StorageClass::Output, v4);
    let out = b.variable(ptr_out, None, StorageClass::Output, None);
    b.decorate(out, Decoration::Location, vec![Operand::LiteralBit32(0)]);
    let k = |b: &mut rspirv::dr::Builder, x: f32| b.constant_bit32(f, x.to_bits());
    let c_seed = k(&mut b, 0.0001);
    let c_097 = k(&mut b, 0.97);
    let c_013 = k(&mut b, 0.013);
    let c_0007 = k(&mut b, 0.0007);
    let c_001 = k(&mut b, 0.001);
    let c_4 = k(&mut b, 4.0);
    let c_half = k(&mut b, 0.5);
    let c_one = k(&mut b, 1.0);
    let main = b.begin_function(void, None, FunctionControl::NONE, void_fn).unwrap();
    b.begin_block(None).unwrap();
    let fcv = b.load(v4, None, fc, None, vec![]).unwrap();
    let fx = b.composite_extract(f, None, fcv, vec![0]).unwrap();
    let fy = b.composite_extract(f, None, fcv, vec![1]).unwrap();
    // seed: acc = fx*0.0001 + fy*0.0001
    let sx = b.f_mul(f, None, fx, c_seed).unwrap();
    let sy = b.f_mul(f, None, fy, c_seed).unwrap();
    let mut acc = b.f_add(f, None, sx, sy).unwrap();
    for _ in 0..ITERS {
        // acc = acc*0.97 + 0.013
        let m = b.f_mul(f, None, acc, c_097).unwrap();
        acc = b.f_add(f, None, m, c_013).unwrap();
        // acc = acc*acc + 0.0007
        let sq = b.f_mul(f, None, acc, acc).unwrap();
        acc = b.f_add(f, None, sq, c_0007).unwrap();
        // acc = sqrt(fabs(acc) + 0.001)
        let ab = b.ext_inst(f, None, glsl, FABS, vec![Operand::IdRef(acc)]).unwrap();
        let pa = b.f_add(f, None, ab, c_001).unwrap();
        acc = b.ext_inst(f, None, glsl, FSQRT, vec![Operand::IdRef(pa)]).unwrap();
        // acc = min(acc, 4.0)
        acc = b.ext_inst(f, None, glsl, FMIN,
            vec![Operand::IdRef(acc), Operand::IdRef(c_4)]).unwrap();
    }
    // out = (acc, acc*0.5, 1-acc, 1)
    let g = b.f_mul(f, None, acc, c_half).unwrap();
    let bb = b.f_sub(f, None, c_one, acc).unwrap();
    let color = b.composite_construct(v4, None, vec![acc, g, bb, c_one]).unwrap();
    b.store(out, color, None, vec![]).unwrap();
    b.ret().unwrap();
    b.end_function().unwrap();
    b.entry_point(ExecutionModel::Fragment, main, "main", vec![fc, out]);
    b.execution_mode(main, ExecutionMode::OriginUpperLeft, vec![]);
    b.module().assemble().iter().flat_map(|w| w.to_le_bytes()).collect()
}

// Identical computation in C — compiled by `cc -O3` (the LLVM ceiling).
const HEAVY_FS_C: &str = r#"
#include <math.h>
void atrium_fs_main(const unsigned char* iv, const unsigned char* un,
    const unsigned char* pc, float fcx, float fcy, float fcz, float fcw,
    unsigned int sm, float* out, float* od, unsigned int ff, unsigned int pid)
{
    const float* uv = (const float*)iv;
    float fx = uv[0], fy = uv[1];
    float acc = fx * 0.0001f + fy * 0.0001f;
    for (int k = 0; k < 64; k++) {
        acc = acc * 0.97f + 0.013f;
        acc = acc * acc + 0.0007f;
        acc = sqrtf(fabsf(acc) + 0.001f);
        acc = fminf(acc, 4.0f);
    }
    out[0] = acc; out[1] = acc * 0.5f; out[2] = 1.0f - acc; out[3] = 1.0f;
}
"#;

fn locate_compile_binary() -> std::path::PathBuf {
    let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.push("atrium-spv-compile");
    p.push("target");
    p.push("debug");
    p.push("atrium-spv-compile");
    assert!(p.exists(), "build ../atrium-spv-compile first ({})", p.display());
    p
}

/// Compile a tier-2 shader with a forced backend (fresh cache so the forced
/// backend actually runs) and return its `atrium_fs_main` pointer.
fn fs_via(backend: &str, spirv: &[u8]) -> FsMain {
    std::env::set_var("ATRIUM_SPV_FORCE_BACKEND", backend);
    let cache = std::env::temp_dir().join(format!("fscg_{backend}_{}", std::process::id()));
    std::fs::create_dir_all(&cache).ok();
    let reg = Tier2Registry::new(LoaderConfig {
        cache_root: cache,
        abi_version: atrium_spv_ir::TIER2_SHADER_ABI_VERSION,
        compile_binary: locate_compile_binary(),
    });
    let id = reg.register(spirv).unwrap_or_else(|e| panic!("{backend} compile: {e:?}"));
    let loaded = reg.get(id).expect("loaded");
    let fs = loaded.entry_points.fs_main.expect("fs_main");
    std::mem::forget(reg); // keep the mmap'd code alive for the program's life
    std::mem::forget(loaded);
    fs
}

/// `cc -O3` the C reference (with extra flags), dlopen it, return its
/// `atrium_fs_main`. `tag` keeps the temp paths distinct.
fn fs_via_cc(tag: &str, extra: &[&str]) -> FsMain {
    let dir = std::env::temp_dir().join(format!("fscg_cc{tag}_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfile = dir.join("fs.c");
    let so = dir.join("fs.dylib");
    std::fs::write(&cfile, HEAVY_FS_C).unwrap();
    let mut cmd = std::process::Command::new("cc");
    cmd.args(["-O3", "-shared", "-fPIC"]).args(extra).arg("-o")
        .arg(&so).arg(&cfile);
    let out = cmd.output().expect("spawn cc");
    assert!(out.status.success(), "cc failed: {}", String::from_utf8_lossy(&out.stderr));
    let cpath = std::ffi::CString::new(so.to_str().unwrap()).unwrap();
    let handle = unsafe { libc::dlopen(cpath.as_ptr(), libc::RTLD_NOW) };
    assert!(!handle.is_null(), "dlopen cc dylib failed");
    let sym = std::ffi::CString::new("atrium_fs_main").unwrap();
    let p = unsafe { libc::dlsym(handle, sym.as_ptr()) };
    assert!(!p.is_null(), "dlsym atrium_fs_main");
    unsafe { std::mem::transmute::<*mut libc::c_void, FsMain>(p) }
}

fn bench(fs: FsMain) -> (f64, [f32; 4]) {
    let mut out = [0f32; 4];
    let mut depth = 0f32;
    let mut sink = 0f64;
    let mut last = [0f32; 4];
    let mut seed = [0f32; 4]; // per-pixel in_varyings: (fx, fy, 0, 1)
    seed[3] = 1.0;
    let mut call = |i: usize, out: &mut [f32; 4]| {
        seed[0] = (i % 1920) as f32 + 0.5;
        seed[1] = (i / 1920) as f32 + 0.5;
        unsafe { fs(seed.as_ptr() as *const u8, std::ptr::null(), std::ptr::null(),
            seed[0], seed[1], 0.5, 1.0, 0, out.as_mut_ptr(), &mut depth, 1, 0); }
    };
    for i in 0..50_000usize { call(i, &mut out); sink += out[0] as f64; } // warmup
    let t = Instant::now();
    for i in 0..PIX {
        call(i, &mut out);
        sink += out[0] as f64;
        if i == PIX - 1 { last = out; }
    }
    let ns = t.elapsed().as_nanos() as f64 / PIX as f64;
    std::hint::black_box(sink);
    (ns, last)
}

fn main() {
    println!("FS codegen ceiling: {ITERS} recurrence steps/pixel × {PIX} pixels");
    println!("(input-seeded dependent recurrence — no const-fold / hoist / vectorise escape)\n");
    let spirv = build_heavy_fs();
    let cc = fs_via_cc("", &[]); // LLVM at its best (fma fusion on)
    let cc_nofma = fs_via_cc("nf", &["-ffp-contract=off"]); // no fma — apples-to-apples
    let bespoke = fs_via("bespoke", &spirv);
    let cranelift = fs_via("cranelift", &spirv);

    let (ns_cc, o_cc) = bench(cc);
    let (ns_ccn, _) = bench(cc_nofma);
    let (ns_be, o_be) = bench(bespoke);
    let (ns_cr, o_cr) = bench(cranelift);

    // Same math → outputs must agree (small fp tolerance for fma rounding).
    let close = |a: [f32; 4], b: [f32; 4]| a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-2);
    println!("  outputs  cc={o_cc:?}");
    println!("           bespoke={o_be:?}  cranelift={o_cr:?}");
    assert!(close(o_cc, o_be) && close(o_cc, o_cr),
        "backends disagree — the SPIR-V and C must compute the same thing");
    println!();
    let row = |name: &str, ns: f64| {
        println!("  {name:<24} {ns:7.3} ns/pixel   ({:5.1}% of cc=LLVM ceiling, \
                  {:.2}× cc)", ns_cc / ns * 100.0, ns / ns_cc);
    };
    row("cc -O3 (LLVM, fma)", ns_cc);
    row("cc -O3 (LLVM, no-fma)", ns_ccn);
    row("bespoke", ns_be);
    row("cranelift", ns_cr);
    println!();
    let lead = ns_be - ns_cc; // cc's total lead over bespoke
    if lead > 0.10 * ns_cc {
        // Meaningful gap remains → attribute it (pre-fma case).
        let fma_share = (ns_ccn - ns_cc).max(0.0);
        println!("  fma fusion explains ~{:.0}% of cc's lead over bespoke \
                  (no-fma cc = {:.1}% of cc); the rest is regalloc / isel.",
            fma_share / lead * 100.0, ns_cc / ns_ccn * 100.0);
    } else {
        println!("  bespoke is within noise of cc/LLVM here — fma fusion \
                  closed the gap (no-fma cc = {:.1}% of cc confirms it was fma).",
            ns_cc / ns_ccn * 100.0);
    }

    // ── Compile time — the OTHER half of the tradeoff (why LLVM was rejected
    //    in the first place). Wall-clock each compiler subprocess: spawn +
    //    compile + produce a *loadable* artifact (cc's `-shared` linker tax
    //    included — it's the real cost of making LLVM output runnable). ──
    println!("\n  ── compile time (median of {} runs, wall-clock per shader) ──", CRUNS);
    let dir = std::env::temp_dir().join(format!("fscg_ct_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let spv = dir.join("s.spv");
    std::fs::write(&spv, &spirv).unwrap();
    let cfile = dir.join("s.c");
    std::fs::write(&cfile, HEAVY_FS_C).unwrap();
    let compiler = locate_compile_binary();
    let spv_cmd = |be: &str| {
        let mut c = std::process::Command::new(&compiler);
        c.arg("--input").arg(&spv).arg("--output-dir").arg(&dir)
            .arg("--hash").arg("bench").arg("--force-backend").arg(be);
        c
    };
    let cc_cmd = |extra: &[&str]| {
        let mut c = std::process::Command::new("cc");
        c.args(["-O3", "-shared", "-fPIC"]).args(extra)
            .arg("-o").arg(dir.join("s.dylib")).arg(&cfile);
        c
    };
    let ct_be = median_ms(spv_cmd("bespoke"));
    let ct_cr = median_ms(spv_cmd("cranelift"));
    let ct_cc = median_ms(cc_cmd(&[]));
    let crow = |name: &str, ms: f64| {
        println!("  {name:<24} {ms:8.3} ms   ({:7.1}× cheaper than cc/LLVM)", ct_cc / ms);
    };
    crow("bespoke", ct_be);
    crow("cranelift", ct_cr);
    crow("cc -O3 -shared (LLVM)", ct_cc);
}

const CRUNS: usize = 15;

/// Median wall-clock (ms) of running `cmd` `CRUNS` times.
fn median_ms(mut cmd: std::process::Command) -> f64 {
    let mut ts = Vec::with_capacity(CRUNS);
    for _ in 0..CRUNS {
        let t = Instant::now();
        let out = cmd.output().expect("spawn compiler");
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        assert!(out.status.success(), "compile failed: {}",
            String::from_utf8_lossy(&out.stderr));
        ts.push(ms);
    }
    ts.sort_by(|a, b| a.partial_cmp(b).unwrap());
    ts[CRUNS / 2]
}
