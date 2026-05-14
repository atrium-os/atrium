//! bespoke-backend micro-benchmark driver.
//!
//! For one SPIR-V shader, measures and compares the tier-2
//! backends along the two axes that matter:
//!
//!   * **compile time** — how long `backend::compile()`
//!     takes to turn the IR module into an object
//!     (frontend + link excluded; isolates the backend
//!     itself). bespoke vs Cranelift — same IR input, so
//!     it's the fair compile comparison.
//!   * **runtime** — ns per `atrium_fs_main` call of the
//!     linked + dlopen'd shader. This is the number the
//!     bespoke backend exists to win.
//!
//! ## The perf bar: `clang -O2`, not Cranelift
//!
//! Cranelift did its job — it dragged the bespoke backend
//! up to fast-tier-JIT quality (the optimisation arc got
//! `heavy` to 1.00× Cranelift). But Cranelift *itself*
//! trades codegen quality for compile speed; it is not a
//! native-speed bar. So the runtime oracle is now a
//! hand-written C reference compiled with `clang -O2`,
//! passed via `--native <c-file>`. Two builds:
//!   * `-ffp-contract=off` — same arithmetic as the
//!     backends; isolates scheduling / regalloc quality.
//!   * default (`fast` contraction) — the true native
//!     ceiling, FMA and all (results not bit-identical).
//! Cranelift stays as a reference column, but the headline
//! ratio is now **bespoke ÷ native** — the gap to close.
//!
//! The same binary runs on the macOS host and, cross-built,
//! inside the FreeBSD/aarch64 VM.
//!
//! Usage:
//!   bench_driver <spirv> [push-const] [int] [--native <c-file>]
//!
//! The optional push-const mirrors `verify/harness.c`: a
//! bare value is an f32, a trailing `int` makes it an i32.
//!
//! stdout: one line per axis, plus a `BENCH` summary line
//! the harness script parses:
//!   BENCH <name> <c_besp> <c_clif> <r_besp> <r_clif> \
//!         <r_native_strict> <r_native_fast>
//! A field is `-` when not measured (bespoke can't compile
//! the shader, or no `--native` reference was given).

use std::path::{Path, PathBuf};
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

fn shared_lib_flag() -> &'static str {
    if cfg!(target_os = "macos") { "-dynamiclib" } else { "-shared" }
}

fn shared_lib_ext() -> &'static str {
    if cfg!(target_os = "macos") { "dylib" } else { "so" }
}

/// Link a backend-produced object blob into a shared
/// library, return its path.
fn link_so(object: &[u8], tag: &str) -> PathBuf {
    let dir = std::env::temp_dir();
    let obj = dir.join(format!("atrium_bench_{tag}.o"));
    let so = dir.join(format!("atrium_bench_{tag}.{}", shared_lib_ext()));
    std::fs::write(&obj, object).expect("write object");
    let status = Command::new("cc")
        .arg(shared_lib_flag()).arg("-o").arg(&so).arg(&obj)
        .status().expect("spawn cc");
    assert!(status.success(), "cc failed linking {tag}");
    so
}

/// Compile a native C reference with `clang -O2` into a
/// shared library. `contract_off` selects
/// `-ffp-contract=off` (same arithmetic as the backends)
/// vs the default `fast` contraction (true native, FMA).
fn compile_native(c_path: &str, contract_off: bool, tag: &str) -> PathBuf {
    let dir = std::env::temp_dir();
    let so = dir.join(format!("atrium_bench_{tag}.{}", shared_lib_ext()));
    let mut cmd = Command::new("cc");
    cmd.arg("-O2");
    if contract_off {
        cmd.arg("-ffp-contract=off");
    }
    cmd.arg(shared_lib_flag()).arg("-o").arg(&so).arg(c_path);
    let status = cmd.status().expect("spawn cc for native reference");
    assert!(status.success(), "cc -O2 failed compiling {c_path}");
    so
}

/// dlopen a shader `.so` and resolve `atrium_fs_main`.
fn load_fs_main(so: &Path) -> (libloading::Library, FsMain) {
    // SAFETY: dlopen of a shader library whose entry-point
    // shape matches the AAPCS64 fragment ABI.
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
    t0.elapsed().as_nanos() as f64 / RUN_ITERS as f64
}

/// Compile a native C file and time its `atrium_fs_main`.
fn bench_native(c_path: &str, contract_off: bool, tag: &str,
                pc_ptr: *const u8) -> f64 {
    let so = compile_native(c_path, contract_off, tag);
    let (_lib, f) = load_fs_main(&so);
    bench_run(f, pc_ptr)
}

fn main() {
    // Pull `--native <path>` out of argv; the rest is the
    // positional `<spirv> [push-const] [int]`.
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    let mut native_c: Option<String> = None;
    if let Some(i) = argv.iter().position(|a| a == "--native") {
        argv.remove(i);
        native_c = Some(if i < argv.len() {
            argv.remove(i)
        } else {
            panic!("--native needs a path argument");
        });
    }
    let mut args = argv.into_iter();
    let spirv_path = args.next().expect("arg1: spirv path");
    let pc_arg = args.next();
    let pc_is_int = args.next().as_deref() == Some("int");

    let spirv = std::fs::read(&spirv_path)
        .unwrap_or_else(|e| panic!("reading {spirv_path}: {e}"));
    let module = frontend_translate(&spirv).expect("frontend translate");
    let (bespoke_target, clif_target) = host_targets();

    // ---- compile-time bench (bespoke vs Cranelift) ---------
    let bespoke_ok = bespoke_compile(&module, bespoke_target).is_ok();
    let bespoke_unsupported = matches!(
        bespoke_compile(&module, bespoke_target),
        Err(BespokeError::Unsupported(_)),
    );

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

    // The perf bar: `clang -O2` of a hand-written C
    // reference, two FP-contraction modes.
    let (run_native_strict, run_native_fast): (Option<f64>, Option<f64>) =
        match &native_c {
            Some(c) => (
                Some(bench_native(c, true,  "native_strict", pc_ptr)),
                Some(bench_native(c, false, "native_fast",   pc_ptr)),
            ),
            None => (None, None),
        };

    // ---- report --------------------------------------------
    let name = Path::new(&spirv_path).file_stem()
        .and_then(|s| s.to_str()).unwrap_or("shader");
    let fmt = |v: Option<f64>| v.map(|n| format!("{n:.2}"))
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

    // Runtime line. The `Nx` ratios are `native ÷ bespoke`
    // — same convention as the compile column: > 1 means
    // bespoke is *ahead* of that native build, < 1 means
    // behind. `O2` is `clang -O2 -ffp-contract=off` (same
    // arithmetic), `fma` is plain `clang -O2` (the true
    // native ceiling, FMA-contracted).
    let run_line = {
        let besp = run_bespoke_ns
            .map(|n| format!("{n:>9.2}"))
            .unwrap_or_else(|| format!("{:>9}", "(skipped)"));
        let mut s = format!(
            "  {name:<12} run:      bespoke {besp} ns   \
             cranelift {run_cranelift_ns:>9.2} ns");
        if let (Some(ns), Some(nf)) = (run_native_strict, run_native_fast) {
            s.push_str(&format!(
                "   native/O2 {ns:>9.2} ns   native/fma {nf:>9.2} ns"));
            if let Some(b) = run_bespoke_ns {
                s.push_str(&format!(
                    "   (vs native: O2 {:.2}x, fma {:.2}x)",
                    ns / b, nf / b));
            }
        }
        s
    };
    println!("{run_line}");

    println!(
        "BENCH {} {} {} {} {} {} {}",
        name,
        fmt(compile_bespoke_ns),
        format!("{compile_cranelift_ns:.1}"),
        fmt(run_bespoke_ns),
        format!("{run_cranelift_ns:.2}"),
        fmt(run_native_strict),
        fmt(run_native_fast),
    );
}
