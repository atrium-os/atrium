//! bespoke-vs-Cranelift micro-benchmark driver.
//!
//! For one SPIR-V shader, measures and compares the two
//! tier-2 backends along both axes that matter:
//!
//!   * **compile time** — how long `backend::compile()`
//!     takes to turn the IR module into an object file
//!     (frontend + `cc` link excluded; this isolates the
//!     backend itself). Relevant to the cache-miss
//!     latency budget.
//!   * **runtime** — ns per `atrium_fs_main` call of the
//!     linked + dlopen'd shader. This is the number the
//!     bespoke backend exists to win: the per-draw hot
//!     path (spec §8.1 — "target the steady-state perf of
//!     hand-written ARM64 code").
//!
//! The same binary runs on the macOS host and, cross-built,
//! inside the FreeBSD/aarch64 VM — so `run-bench.sh` gets a
//! host vs on-target comparison from one source.
//!
//! Usage:
//!   bench_driver <spirv> [push-const] [int]
//!
//! The optional push-const mirrors `verify/harness.c`: a
//! bare value is an f32, a trailing `int` makes it an i32.
//!
//! stdout: one line per axis, plus a `BENCH` summary line
//! the harness script parses:
//!   BENCH <compile_bespoke_ns> <compile_cranelift_ns> \
//!         <run_bespoke_ns> <run_cranelift_ns>
//! A field is `-` when bespoke can't compile the shader
//! (Cranelift-only fallback shaders).

use std::path::Path;
use std::process::Command;
use std::time::Instant;

use atrium_spv_backend_bespoke::{
    compile as bespoke_compile, BackendError as BespokeError,
    Target as BespokeTarget,
};
use atrium_spv_backend_cranelift::{
    compile as cranelift_compile, Target as ClifTarget,
};
use atrium_spv_frontend::translate as frontend_translate;

/// Fragment-shader entry, AAPCS64 ABI (spec §4.1).
type FsMain = unsafe extern "C" fn(
    *const u8, *const u8, *const u8,
    f32, f32, f32, f32, u32,
    *mut f32, *mut f32,
);

const COMPILE_ITERS: u32 = 300;
const RUN_WARMUP: u32 = 50_000;
const RUN_ITERS: u64 = 4_000_000;

fn host_targets() -> (BespokeTarget, ClifTarget) {
    #[cfg(target_os = "freebsd")]
    { (BespokeTarget::Aarch64FreeBSD, ClifTarget::Aarch64FreeBSD) }
    #[cfg(not(target_os = "freebsd"))]
    { (BespokeTarget::Aarch64Darwin, ClifTarget::Aarch64Darwin) }
}

/// Link an object blob into a shared library, return its path.
fn link_so(object: &[u8], tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir();
    let obj = dir.join(format!("atrium_bench_{tag}.o"));
    let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
    let so = dir.join(format!("atrium_bench_{tag}.{ext}"));
    std::fs::write(&obj, object).expect("write object");
    let flag = if cfg!(target_os = "macos") { "-dynamiclib" } else { "-shared" };
    let status = Command::new("cc")
        .arg(flag).arg("-o").arg(&so).arg(&obj)
        .status().expect("spawn cc");
    assert!(status.success(), "cc failed linking {tag}");
    so
}

/// dlopen a shader `.so` and resolve `atrium_fs_main`.
fn load_fs_main(so: &Path) -> (libloading::Library, FsMain) {
    // SAFETY: dlopen of a backend-produced shader library;
    // the entry-point shape matches the AAPCS64 fragment ABI.
    unsafe {
        let lib = libloading::Library::new(so)
            .unwrap_or_else(|e| panic!("dlopen {}: {e}", so.display()));
        let sym = lib.get::<FsMain>(b"atrium_fs_main")
            .expect("no atrium_fs_main symbol");
        let f = *sym;
        (lib, f)
    }
}

/// Time `RUN_ITERS` calls of an fs_main, return ns/call.
fn bench_run(f: FsMain, pc_ptr: *const u8) -> f64 {
    let mut out = [0f32; 4];
    let mut depth = 0f32;
    // Warmup.
    for _ in 0..RUN_WARMUP {
        unsafe {
            f(std::ptr::null(), std::ptr::null(), pc_ptr,
              0.0, 0.0, 0.0, 0.0, 0,
              out.as_mut_ptr(), &mut depth);
        }
        std::hint::black_box(&out);
    }
    let t0 = Instant::now();
    for _ in 0..RUN_ITERS {
        unsafe {
            f(std::ptr::null(), std::ptr::null(),
              std::hint::black_box(pc_ptr),
              0.0, 0.0, 0.0, 0.0, 0,
              out.as_mut_ptr(), &mut depth);
        }
        std::hint::black_box(&out);
    }
    let elapsed = t0.elapsed();
    elapsed.as_nanos() as f64 / RUN_ITERS as f64
}

fn main() {
    let mut args = std::env::args().skip(1);
    let spirv_path = args.next().expect("arg1: spirv path");
    let pc_arg = args.next();
    let pc_is_int = args.next().as_deref() == Some("int");

    let spirv = std::fs::read(&spirv_path)
        .unwrap_or_else(|e| panic!("reading {spirv_path}: {e}"));
    let module = frontend_translate(&spirv).expect("frontend translate");
    let (bespoke_target, clif_target) = host_targets();

    // ---- compile-time bench --------------------------------
    // Probe bespoke once: it may not support this shader.
    let bespoke_ok = matches!(
        bespoke_compile(&module, bespoke_target), Ok(_),
    );
    let bespoke_unsupported = match bespoke_compile(&module, bespoke_target) {
        Err(BespokeError::Unsupported(_)) => true,
        _ => false,
    };

    let mut compile_bespoke_ns: Option<f64> = None;
    if bespoke_ok {
        let mut best = u128::MAX;
        for _ in 0..COMPILE_ITERS {
            let t0 = Instant::now();
            let o = bespoke_compile(&module, bespoke_target).unwrap();
            best = best.min(t0.elapsed().as_nanos());
            std::hint::black_box(&o.object);
        }
        compile_bespoke_ns = Some(best as f64);
    }

    let compile_cranelift_ns: f64 = {
        let mut best = u128::MAX;
        for _ in 0..COMPILE_ITERS {
            let t0 = Instant::now();
            let o = cranelift_compile(&module, clif_target).unwrap();
            best = best.min(t0.elapsed().as_nanos());
            std::hint::black_box(&o.object);
        }
        best as f64
    };

    // ---- runtime bench -------------------------------------
    // Build the optional 16-byte push-constant buffer.
    let mut pc = [0u8; 16];
    let pc_ptr: *const u8 = match &pc_arg {
        Some(v) => {
            if pc_is_int {
                let iv: i32 = v.parse().expect("push-const int");
                pc[..4].copy_from_slice(&iv.to_le_bytes());
            } else {
                let fv: f32 = v.parse().expect("push-const f32");
                pc[..4].copy_from_slice(&fv.to_le_bytes());
            }
            pc.as_ptr()
        }
        None => std::ptr::null(),
    };

    let clif_obj = cranelift_compile(&module, clif_target).unwrap().object;
    let clif_so = link_so(&clif_obj, "clif");
    let (_clif_lib, clif_fs) = load_fs_main(&clif_so);
    let run_cranelift_ns = bench_run(clif_fs, pc_ptr);

    let mut run_bespoke_ns: Option<f64> = None;
    if bespoke_ok {
        let besp_obj = bespoke_compile(&module, bespoke_target).unwrap().object;
        let besp_so = link_so(&besp_obj, "besp");
        let (_besp_lib, besp_fs) = load_fs_main(&besp_so);
        run_bespoke_ns = Some(bench_run(besp_fs, pc_ptr));
    }

    // ---- report --------------------------------------------
    let name = Path::new(&spirv_path).file_stem()
        .and_then(|s| s.to_str()).unwrap_or("shader");
    let fmt = |v: Option<f64>| v.map(|n| format!("{n:.1}"))
        .unwrap_or_else(|| "-".to_string());

    match compile_bespoke_ns {
        Some(b) => println!(
            "  {name:<12} compile:  bespoke {b:>9.1} ns   \
             cranelift {compile_cranelift_ns:>9.1} ns   \
             ({:.2}x)", compile_cranelift_ns / b,
        ),
        None => {
            let why = if bespoke_unsupported { "Unsupported" } else { "error" };
            println!(
                "  {name:<12} compile:  bespoke {why:>12}   \
                 cranelift {compile_cranelift_ns:>9.1} ns",
            );
        }
    }
    match run_bespoke_ns {
        Some(b) => println!(
            "  {name:<12} run:      bespoke {b:>9.2} ns   \
             cranelift {run_cranelift_ns:>9.2} ns   \
             ({:.2}x)", run_cranelift_ns / b,
        ),
        None => println!(
            "  {name:<12} run:      bespoke {:>12}   \
             cranelift {run_cranelift_ns:>9.2} ns", "(skipped)",
        ),
    }
    // Machine-parseable summary line.
    println!(
        "BENCH {} {} {} {} {}",
        name,
        fmt(compile_bespoke_ns),
        format!("{compile_cranelift_ns:.1}"),
        fmt(run_bespoke_ns),
        format!("{run_cranelift_ns:.2}"),
    );
}
