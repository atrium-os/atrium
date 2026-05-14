//! atrium-spv-compile — the jailed AOT compile binary for
//! tier-2 software Vulkan shaders.
//!
//! Reads SPIR-V from a file, runs it through the frontend
//! + backend pipeline, links the produced object file
//! into a `.so` via the system `cc`, writes `.so` and
//! `.pcmap` files to an output directory keyed by content
//! hash.
//!
//! # Usage
//!
//! ```text
//! atrium-spv-compile --input <SPIR-V file> \
//!                    --output-dir <cache dir> \
//!                    [--target <triple>] \
//!                    [--abi-version <N>]
//! ```
//!
//! On success, exits 0 and writes:
//!   `<output-dir>/<sha256>.so`
//!   `<output-dir>/<sha256>.pcmap`
//!
//! On stderr the binary prints one structured JSON line
//! per compile (constraint G7) describing the result:
//!
//! ```json
//! {"shader_hash":"abc...","backend":"cranelift","ops":42,"compile_ms":123,"size_bytes":9216}
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
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::Instant;

use atrium_spv_backend_bespoke::{
    compile_blob as bespoke_compile_blob, BackendError as BespokeError,
    Target as BespokeTarget,
};
use atrium_spv_backend_cranelift::{compile as cranelift_compile, Target};
use atrium_spv_frontend::translate as frontend_translate;
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
                  \"compile_ms\":{},\"frontend_us\":{},\
                  \"backend_us\":{},\"link_us\":{},\
                  \"size_bytes\":{}}}",
                report.shader_hash, report.backend,
                report.compile_ms, report.frontend_us,
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
}

fn parse_args() -> Result<Args, String> {
    let mut input: Option<PathBuf> = None;
    let mut output_dir: Option<PathBuf> = None;
    let mut target: Option<Target> = None;
    let mut hash_override: Option<String> = None;

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
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(Args {
        input: input.ok_or("--input is required")?,
        output_dir: output_dir.ok_or("--output-dir is required")?,
        target: target.unwrap_or_else(Target::host),
        hash_override,
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
           [--hash <override sha256 hex>]"
    );
}

#[derive(Debug)]
struct CompileReport {
    shader_hash: String,
    backend: &'static str,
    compile_ms: u128,
    /// Per-phase breakdown of the wall clock, microseconds:
    /// frontend (SPIR-V → IR), backend (IR → object), link
    /// (`cc` object → `.so`). Reading + hashing + file I/O
    /// is the small remainder.
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

    // 2. Hash for cache key.
    let hash = args.hash_override.clone().unwrap_or_else(|| {
        let mut hasher = Sha256::new();
        hasher.update(&spirv);
        format!("{:x}", hasher.finalize())
    });

    // 3. Frontend: SPIR-V → atrium-spv-ir.
    let t_frontend = Instant::now();
    let module = frontend_translate(&spirv).map_err(|e| match e {
        atrium_spv_frontend::FrontendError::Unsupported(m) =>
            CompileError::Unsupported(m),
        other => CompileError::Internal(format!("frontend: {other}")),
    })?;
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

    // The two artifact shapes a backend can produce:
    //   * Blob   — a flat `atrium-spv-blob`, written
    //              straight to `<hash>.afblob`. No linker.
    //              The bespoke JIT-emit path.
    //   * Object — an ELF/Mach-O object, linked by `cc`
    //              into `<hash>.so`. The Cranelift path
    //              (until JIT-emit phase 4).
    enum Artifact { Blob(Vec<u8>), Object(Vec<u8>) }

    // Cranelift fallback, shared by the no-bespoke-target
    // case and the bespoke-returned-Unsupported case.
    let cranelift_path = || -> Result<(Artifact, Vec<u8>, &'static str), CompileError> {
        match cranelift_compile(&module, args.target) {
            Ok(o) => Ok((Artifact::Object(o.object), o.pcmap, "cranelift")),
            Err(atrium_spv_backend_cranelift::BackendError::Unsupported(m)) =>
                Err(CompileError::Unsupported(m)),
            Err(other) =>
                Err(CompileError::Internal(format!("cranelift: {other}"))),
        }
    };

    let t_backend = Instant::now();
    let (artifact, pcmap, backend_name): (Artifact, Vec<u8>, &'static str) =
        match bespoke_target {
            Some(bt) => match bespoke_compile_blob(&module, bt) {
                Ok(o) => (Artifact::Blob(o.blob), o.pcmap, "bespoke"),
                Err(BespokeError::Unsupported(_)) => cranelift_path()?,
                Err(BespokeError::Internal(m)) =>
                    return Err(CompileError::Internal(
                        format!("bespoke: {m}"))),
            },
            None => cranelift_path()?,
        };
    // Includes the bespoke probe-then-fallback when it
    // happens — that's the real production cost, so time
    // the whole selection, not just the winning backend.
    let backend_us = t_backend.elapsed().as_micros();

    // 5. Write the artifact into the cache directory.
    std::fs::create_dir_all(&args.output_dir).map_err(|e|
        CompileError::Internal(format!(
            "creating output dir {}: {e}", args.output_dir.display(),
        )))?;

    let (link_us, size_bytes) = match artifact {
        Artifact::Blob(blob) => {
            // The flat blob is the final artifact — the
            // loader `mmap`s it directly. No linker, so
            // `link_us` is zero (the ~99.5%-of-compile `cc`
            // step is gone for the bespoke path).
            let blob_path = args.output_dir.join(format!("{hash}.afblob"));
            std::fs::write(&blob_path, &blob).map_err(|e|
                CompileError::Internal(format!(
                    "writing {}: {e}", blob_path.display())))?;
            (0u128, blob.len())
        }
        Artifact::Object(object) => {
            // Object → temp `.o` → `cc -shared` → `<hash>.so`.
            let obj_path = args.output_dir.join(format!("{hash}.o"));
            std::fs::write(&obj_path, &object).map_err(|e|
                CompileError::Internal(format!(
                    "writing {}: {e}", obj_path.display())))?;
            let ext = if cfg!(target_os = "macos") { "dylib" } else { "so" };
            let so_path = args.output_dir.join(format!("{hash}.{ext}"));
            let t_link = Instant::now();
            link_to_shared_lib(&obj_path, &so_path)?;
            let link_us = t_link.elapsed().as_micros();
            // Cache only needs the `.so` + `.pcmap`.
            let _ = std::fs::remove_file(&obj_path);
            let size = std::fs::metadata(&so_path)
                .map(|m| m.len() as usize).unwrap_or(0);
            (link_us, size)
        }
    };

    // 6. Write pcmap sidecar (identical for both paths).
    let pcmap_path = args.output_dir.join(format!("{hash}.pcmap"));
    std::fs::write(&pcmap_path, &pcmap).map_err(|e|
        CompileError::Internal(format!(
            "writing {}: {e}", pcmap_path.display(),
        )))?;

    let so_size = size_bytes;
    let elapsed_ms = t0.elapsed().as_millis();

    Ok(CompileReport {
        shader_hash: hash,
        backend: backend_name,
        compile_ms: elapsed_ms,
        frontend_us,
        backend_us,
        link_us,
        size_bytes: so_size,
    })
}

fn link_to_shared_lib(obj: &Path, out: &Path) -> Result<(), CompileError> {
    let flag = if cfg!(target_os = "macos") { "-dynamiclib" } else { "-shared" };
    let output = Command::new("cc")
        .arg(flag).arg("-o").arg(out).arg(obj)
        .output()
        .map_err(|e| CompileError::Internal(format!("spawn cc: {e}")))?;
    if !output.status.success() {
        return Err(CompileError::Internal(format!(
            "cc failed: status={}\nstderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
        )));
    }
    Ok(())
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
