//! atrium-spv-compile — the jailed AOT compile binary for
//! tier-2 software Vulkan shaders.
//!
//! Reads SPIR-V from a file, runs it through the frontend
//! + backend pipeline, and writes a flat executable
//! `atrium-spv-blob` (`.afblob`) plus its `.pcmap` sidecar
//! to an output directory keyed by content hash. Both
//! backends emit the blob directly — there is no `cc`
//! link step (JIT-emit phase 4); the daemon's loader
//! `mmap`s the blob `PROT_EXEC`.
//!
//! # Usage
//!
//! ```text
//! atrium-spv-compile --input <SPIR-V file> \
//!                    --output-dir <cache dir> \
//!                    [--target <triple>] \
//!                    [--hash <override>]
//! ```
//!
//! On success, exits 0 and writes:
//!   `<output-dir>/<sha256>.afblob`
//!   `<output-dir>/<sha256>.pcmap`
//!
//! On stderr the binary prints one structured JSON line
//! per compile (constraint G7) describing the result —
//! `link_us` is always 0 now that `cc` is gone, kept in
//! the schema for continuity:
//!
//! ```json
//! {"shader_hash":"abc...","backend":"bespoke","compile_us":870,"frontend_us":600,"backend_us":240,"link_us":0,"size_bytes":292}
//! ```
//!
//! # Exit codes
//!
//! - `0` — success
//! - `1` — unsupported shader (frontend or backend
//!   rejected it). The daemon's Tier2Backend reads this
//!   as "this shader can't be compiled by tier-2"; the
//!   app sees `VK_ERROR_INVALID_SHADER_NV`.
//! - `2` — internal / setup error (argument parse, file
//!   I/O, linker failure). Indicates a bug or
//!   environment problem; not the shader's fault.
//!
//! # Spec references
//!
//! - [`docs/spec/tier2-renderer.md`] §5 (the compile
//!   pipeline) + §6 (the crate's role) + decision D3
//!   (jailed sub-process)
//! - [`docs/spec/tier2-shader-codegen-constraints.md`]
//!   §G3 (`can_handle` discipline) + §G7 (structured
//!   metrics output)

use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use atrium_spv_backend_bespoke::{
    compile_blob as bespoke_compile_blob, BackendError as BespokeError,
    Target as BespokeTarget,
};
use atrium_spv_backend_cranelift::{
    compile_blob as cranelift_compile_blob, Target,
};
use atrium_spv_frontend::{
    translate_with_spec_overrides as frontend_translate_with_overrides,
    SpecOverrides,
};
use sha2::{Digest, Sha256};

const EXIT_OK: u8 = 0;
const EXIT_UNSUPPORTED: u8 = 1;
const EXIT_INTERNAL: u8 = 2;

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("atrium-spv-compile: {e}");
            return ExitCode::from(EXIT_INTERNAL);
        }
    };
    match run(&args) {
        Ok(report) => {
            // G7: structured metrics line on stderr. The
            // `*_us` fields break the wall-clock down into
            // the three pipeline phases — frontend (SPIR-V
            // → IR), backend (IR → object), link (`cc`
            // → .so) — so the cost of each is visible
            // (informs the in-memory / JIT-emit question:
            // if `link_us` dominates, dropping `cc` is the
            // real lever, not the backend speed).
            let _ = writeln!(
                std::io::stderr(),
                "{{\"shader_hash\":\"{}\",\"backend\":\"{}\",\
                  \"compile_us\":{},\"frontend_us\":{},\
                  \"backend_us\":{},\"link_us\":{},\
                  \"size_bytes\":{}}}",
                report.shader_hash, report.backend,
                report.compile_us, report.frontend_us,
                report.backend_us, report.link_us,
                report.size_bytes,
            );
            ExitCode::from(EXIT_OK)
        }
        Err(CompileError::Unsupported(msg)) => {
            eprintln!("atrium-spv-compile: unsupported: {msg}");
            ExitCode::from(EXIT_UNSUPPORTED)
        }
        Err(CompileError::Internal(msg)) => {
            eprintln!("atrium-spv-compile: internal: {msg}");
            ExitCode::from(EXIT_INTERNAL)
        }
    }
}

#[derive(Debug)]
struct Args {
    input: PathBuf,
    output_dir: PathBuf,
    target: Target,
    /// Override the hash used for output filenames. When
    /// absent, sha256 of the SPIR-V bytes is used (the
    /// daemon's caching policy per §2 of the renderer
    /// spec).
    hash_override: Option<String>,
    /// Override the production "try bespoke, fall back to
    /// Cranelift on Unsupported" selection.  None = default
    /// selection.  Some(Bespoke) = bespoke only, error if
    /// it can't handle.  Some(Cranelift) = skip bespoke,
    /// go straight to Cranelift.  Used for debugging and
    /// tests that want to exercise a specific backend
    /// without the selection logic getting in the way.
    force_backend: Option<ForceBackend>,
    /// VkSpecializationInfo-style overrides, parsed from
    /// repeated `--spec-const SPECID=VALUE` flags.  Empty
    /// map means "use SPIR-V-declared defaults".
    /// VALUE is parsed as either a u32 decimal/hex literal
    /// (`0x...`) for int / bool spec constants, or as an f32
    /// when prefixed with `f:` (e.g. `--spec-const 2=f:3.14`).
    /// Boolean spec constants accept 0 / 1.
    spec_overrides: SpecOverrides,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ForceBackend { Bespoke, Cranelift }

fn parse_args() -> Result<Args, String> {
    let mut input: Option<PathBuf> = None;
    let mut output_dir: Option<PathBuf> = None;
    let mut target: Option<Target> = None;
    let mut hash_override: Option<String> = None;
    let mut force_backend: Option<ForceBackend> = None;
    let mut spec_overrides: SpecOverrides = SpecOverrides::new();

    let parse_spec_const = |raw: &str,
                             out: &mut SpecOverrides| -> Result<(), String> {
        let (id_str, val_str) = raw.split_once('=').ok_or_else(||
            format!("--spec-const expects SPECID=VALUE, got `{raw}`"))?;
        let spec_id: u32 = id_str.parse().map_err(|e|
            format!("--spec-const SPECID `{id_str}` not a u32: {e}"))?;
        // `f:` prefix: parse VALUE as f32, store its bit pattern.
        let value: u32 = if let Some(rest) = val_str.strip_prefix("f:") {
            let f: f32 = rest.parse().map_err(|e|
                format!("--spec-const f:value `{rest}` not an f32: {e}"))?;
            f.to_bits()
        } else if let Some(hex) = val_str.strip_prefix("0x")
            .or_else(|| val_str.strip_prefix("0X")) {
            u32::from_str_radix(hex, 16).map_err(|e|
                format!("--spec-const hex `{val_str}` not a u32: {e}"))?
        } else if let Some(neg) = val_str.strip_prefix('-') {
            // Signed-int literal: parse as i32, store bit pattern.
            let n: i32 = format!("-{neg}").parse().map_err(|e|
                format!("--spec-const signed `{val_str}` not an i32: {e}"))?;
            n as u32
        } else {
            val_str.parse::<u32>().map_err(|e|
                format!("--spec-const dec `{val_str}` not a u32: {e}"))?
        };
        out.insert(spec_id, value);
        Ok(())
    };

    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--input" => input = Some(PathBuf::from(
                it.next().ok_or("--input needs a path")?,
            )),
            "--output-dir" => output_dir = Some(PathBuf::from(
                it.next().ok_or("--output-dir needs a path")?,
            )),
            "--target" => {
                let t = it.next().ok_or("--target needs a triple")?;
                target = Some(parse_target(&t)?);
            }
            "--hash" => hash_override = Some(
                it.next().ok_or("--hash needs a value")?,
            ),
            "--spec-const" => {
                let raw = it.next().ok_or(
                    "--spec-const needs SPECID=VALUE")?;
                parse_spec_const(&raw, &mut spec_overrides)?;
            }
            "--force-backend" => {
                let v = it.next().ok_or(
                    "--force-backend needs bespoke|cranelift")?;
                force_backend = Some(match v.as_str() {
                    "bespoke"   => ForceBackend::Bespoke,
                    "cranelift" => ForceBackend::Cranelift,
                    other => return Err(format!(
                        "unknown --force-backend value: {other}")),
                });
            }
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    // Env var fallback: tests + daemons can force a backend
    // without rewriting their argv.  CLI --force-backend
    // takes precedence over the env var.
    if force_backend.is_none() {
        if let Ok(v) = std::env::var("ATRIUM_SPV_FORCE_BACKEND") {
            force_backend = Some(match v.as_str() {
                "bespoke"   => ForceBackend::Bespoke,
                "cranelift" => ForceBackend::Cranelift,
                other => return Err(format!(
                    "ATRIUM_SPV_FORCE_BACKEND={other} (expected bespoke|cranelift)")),
            });
        }
    }
    Ok(Args {
        input: input.ok_or("--input is required")?,
        output_dir: output_dir.ok_or("--output-dir is required")?,
        target: target.unwrap_or_else(Target::host),
        hash_override,
        force_backend,
        spec_overrides,
    })
}

fn parse_target(s: &str) -> Result<Target, String> {
    match s {
        "aarch64-unknown-freebsd" => Ok(Target::Aarch64FreeBSD),
        "aarch64-apple-darwin"    => Ok(Target::Aarch64Darwin),
        "x86_64-unknown-freebsd"  => Ok(Target::X86_64FreeBSD),
        other => Err(format!("unknown --target {other}")),
    }
}

fn print_usage() {
    eprintln!(
        "Usage: atrium-spv-compile \\\n  \
           --input <SPIR-V file> \\\n  \
           --output-dir <cache dir> \\\n  \
           [--target <aarch64-unknown-freebsd|aarch64-apple-darwin|x86_64-unknown-freebsd>] \\\n  \
           [--hash <override sha256 hex>] \\\n  \
           [--spec-const SPECID=VALUE]... \\\n  \
           [--force-backend bespoke|cranelift]\n\
         \n  \
         --spec-const overrides the SPIR-V OpSpecConstant\n  \
         with the matching SpecId decoration.  VALUE may be:\n    \
           NNNN          (decimal u32)\n    \
           -NNNN         (decimal i32, stored as bit pattern)\n    \
           0xNNNN        (hex u32)\n    \
           f:N.N         (f32, stored as bit pattern)\n    \
           0 / 1         (bool)"
    );
}

#[derive(Debug)]
struct CompileReport {
    shader_hash: String,
    backend: &'static str,
    /// Total compile wall clock, **microseconds**. Was
    /// `compile_ms` until `cc` was removed (JIT-emit phase
    /// 4) dropped the whole pipeline into sub-millisecond
    /// territory — millisecond resolution truncated both
    /// backends to "1" and hid the real ~2–4× backend
    /// difference. Microseconds keeps it visible.
    compile_us: u128,
    /// Per-phase breakdown of the wall clock, microseconds:
    /// frontend (SPIR-V → IR), backend (IR → object/blob),
    /// link (`cc`; always 0 now). Reading + hashing + file
    /// I/O is the small remainder.
    frontend_us: u128,
    backend_us: u128,
    link_us: u128,
    size_bytes: usize,
}

#[derive(Debug)]
enum CompileError {
    Unsupported(String),
    Internal(String),
}

fn run(args: &Args) -> Result<CompileReport, CompileError> {
    let t0 = Instant::now();

    // 1. Read SPIR-V.
    let spirv = std::fs::read(&args.input)
        .map_err(|e| CompileError::Internal(format!(
            "reading {}: {e}", args.input.display(),
        )))?;

    // 2. Hash for cache key.  When no host-supplied hash is
    // given, hash the SPIR-V bytes.  If spec-constant
    // overrides are present, mix them into the hash too: two
    // builds of the same SPIR-V with different overrides
    // must map to different cache outputs.  The mix is only
    // applied when `--spec-const` was supplied, so the no-
    // override case keeps the historical pure-SPIR-V hash
    // (preserves existing callers' cache keys).
    let hash = args.hash_override.clone().unwrap_or_else(|| {
        let mut hasher = Sha256::new();
        hasher.update(&spirv);
        if !args.spec_overrides.is_empty() {
            // Sort by SpecId for stability across HashMap
            // iteration order.
            let mut entries: Vec<(u32, u32)> =
                args.spec_overrides.iter().map(|(k, v)| (*k, *v)).collect();
            entries.sort_by_key(|(k, _)| *k);
            hasher.update(b"\x00spec\x00");
            for (k, v) in entries {
                hasher.update(k.to_le_bytes());
                hasher.update(v.to_le_bytes());
            }
        }
        format!("{:x}", hasher.finalize())
    });

    // 3. Frontend: SPIR-V → atrium-spv-ir, applying spec
    // overrides (no-op when --spec-const wasn't passed).
    let t_frontend = Instant::now();
    let mut module = frontend_translate_with_overrides(
        &spirv, &args.spec_overrides,
    ).map_err(|e| match e {
        atrium_spv_frontend::FrontendError::Unsupported(m) =>
            CompileError::Unsupported(m),
        other => CompileError::Internal(format!("frontend: {other}")),
    })?;
    // IR-level optimisation passes (backend-agnostic). FMA fusion folds
    // single-use FMul→FAdd into one FMADD — the measured ~1.3× gap to LLVM
    // on compute-heavy shaders (bench_fs_codegen).
    atrium_spv_ir::fuse_fma(&mut module);
    let frontend_us = t_frontend.elapsed().as_micros();

    // 4. Backend.
    //
    // Production order per spec §2: try the bespoke ARM64
    // backend first (fast path, hand-tuned codegen), fall
    // back to Cranelift when bespoke returns `Unsupported`
    // for an opcode/shape it doesn't handle yet. A bespoke
    // `Internal` error is a real bug, not a fallback
    // signal — it surfaces as a compile failure.
    //
    // The bespoke backend is ARM64-only by charter; for an
    // x86_64 target there is no bespoke target to map to,
    // so we go straight to Cranelift.
    let bespoke_target: Option<BespokeTarget> = match args.target {
        Target::Aarch64FreeBSD => Some(BespokeTarget::Aarch64FreeBSD),
        Target::Aarch64Darwin  => Some(BespokeTarget::Aarch64Darwin),
        Target::X86_64FreeBSD  => None,
    };

    // Both backends now emit a flat `atrium-spv-blob`
    // (`compile_blob`) — there is no object/`cc`/`.so`
    // path anymore. The Cranelift backend re-parses its
    // own object internally to lift out the flat `.text`;
    // if it ever hits a relocation it fails loudly rather
    // than silently falling back (no `cc` safety net —
    // that's the whole point of JIT-emit phase 4).
    //
    // Cranelift path, shared by the no-bespoke-target case
    // and the bespoke-returned-Unsupported case.
    let cranelift_path = || -> Result<(Vec<u8>, Vec<u8>, &'static str), CompileError> {
        match cranelift_compile_blob(&module, args.target) {
            Ok(o) => Ok((o.blob, o.pcmap, "cranelift")),
            Err(atrium_spv_backend_cranelift::BackendError::Unsupported(m)) =>
                Err(CompileError::Unsupported(m)),
            Err(other) =>
                Err(CompileError::Internal(format!("cranelift: {other}"))),
        }
    };

    let t_backend = Instant::now();
    let (blob, pcmap, backend_name): (Vec<u8>, Vec<u8>, &'static str) =
        match (args.force_backend, bespoke_target) {
            // --force-backend=cranelift: skip bespoke probe.
            (Some(ForceBackend::Cranelift), _) => cranelift_path()?,
            // --force-backend=bespoke: bespoke or bust (no
            // Cranelift fallback).  Errors out if the target
            // doesn't have a bespoke port or bespoke can't
            // handle the shader.
            (Some(ForceBackend::Bespoke), Some(bt)) =>
                match bespoke_compile_blob(&module, bt) {
                    Ok(o) => (o.blob, o.pcmap, "bespoke"),
                    Err(BespokeError::Unsupported(m)) =>
                        return Err(CompileError::Unsupported(
                            format!("bespoke (forced): {m}"))),
                    Err(BespokeError::Internal(m)) =>
                        return Err(CompileError::Internal(
                            format!("bespoke: {m}"))),
                },
            (Some(ForceBackend::Bespoke), None) =>
                return Err(CompileError::Unsupported(
                    "--force-backend=bespoke but target has no bespoke port".into())),
            // Default selection: try bespoke, fall back to
            // Cranelift on Unsupported.
            (None, Some(bt)) => match bespoke_compile_blob(&module, bt) {
                Ok(o) => (o.blob, o.pcmap, "bespoke"),
                Err(BespokeError::Unsupported(_)) => cranelift_path()?,
                Err(BespokeError::Internal(m)) =>
                    return Err(CompileError::Internal(
                        format!("bespoke: {m}"))),
            },
            (None, None) => cranelift_path()?,
        };
    // Includes the bespoke probe-then-fallback when it
    // happens — that's the real production cost, so time
    // the whole selection, not just the winning backend.
    let backend_us = t_backend.elapsed().as_micros();

    // 5. Write the flat blob into the cache directory. The
    // loader `mmap`s it `PROT_EXEC` directly — no linker,
    // so the ~99.5%-of-compile `cc` step is simply gone.
    std::fs::create_dir_all(&args.output_dir).map_err(|e|
        CompileError::Internal(format!(
            "creating output dir {}: {e}", args.output_dir.display(),
        )))?;

    let blob_path = args.output_dir.join(format!("{hash}.afblob"));
    std::fs::write(&blob_path, &blob).map_err(|e|
        CompileError::Internal(format!(
            "writing {}: {e}", blob_path.display())))?;
    let size_bytes = blob.len();
    // No link step on any path now.
    let link_us: u128 = 0;

    // 6. Write pcmap sidecar.
    let pcmap_path = args.output_dir.join(format!("{hash}.pcmap"));
    std::fs::write(&pcmap_path, &pcmap).map_err(|e|
        CompileError::Internal(format!(
            "writing {}: {e}", pcmap_path.display(),
        )))?;

    let so_size = size_bytes;
    let elapsed_us = t0.elapsed().as_micros();

    Ok(CompileReport {
        shader_hash: hash,
        backend: backend_name,
        compile_us: elapsed_us,
        frontend_us,
        backend_us,
        link_us,
        size_bytes: so_size,
    })
}

/// Compute the standard cache filename for a SPIR-V blob.
///
/// Exposed for tests + (future) the daemon's Tier2Backend
/// to keep the hash logic in one place.
#[cfg(test)]
pub(crate) fn hash_spirv(spirv: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(spirv);
    format!("{:x}", h.finalize())
}
