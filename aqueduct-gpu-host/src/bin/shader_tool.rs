//! `aqueduct-shader-tool` — offline shader validator + cache populator.
//!
//! Wraps [`aqueduct_gpu_host::shader_validator`] and
//! [`aqueduct_gpu_host::shader_cache`] for use outside the daemon's
//! hot path:
//!
//! - **CI**: gate merges on validator success for atrium-shipped
//!   bundles.
//! - **atrium-pkg install hook (Phase 2.5)**: walk a freshly-installed
//!   package's SPIR-V directory, validate each module, write into the
//!   shared shader cache. Subsequent `OP_GPU_SHADER_RESOLVE` calls
//!   from the app hit the warm path.
//! - **Local dev**: quick check on a shader before submitting a PR.
//!
//! ## Subcommands
//!
//! ```text
//! aqueduct-shader-tool check <FILE>
//!     Validate a single SPIR-V module. Exit 0 on success, 1 on
//!     rejection. Diagnostic on stderr.
//!
//! aqueduct-shader-tool annotate --max-iters N <FILE>
//!     Patch every OpLoopMerge in <FILE> whose LoopControl does not
//!     already declare an iteration bound, setting MaxIterations | N.
//!     Required for slangc (2026.8) output: slangc accepts
//!     [MaxIters(N)] in source but silently drops it during SPIR-V
//!     emission, leaving us no in-band way to express bounded
//!     runtime loops. Authors keep the bound in the .slang source
//!     as a comment + duplicate it in the build script.
//!
//! aqueduct-shader-tool verify-bundle <DIR>
//!     Load DIR/manifest.json; for each op, resolve and validate the
//!     compute_entry and render_pipeline .spv files. Exit 0 if every
//!     referenced shader passes the validator. Used by atrium-pkg's
//!     install hook as the bundle-level gate.
//!
//! aqueduct-shader-tool precompile [--cache DIR] [--backend NAME]
//!                                 [--generation N] [--compiler-version N]
//!                                 [--dry-run] <DIR>
//!     Recursively walk DIR for .spv files. Validate each; on success
//!     insert into the cache keyed by (sha256(bytes), backend, gen,
//!     compiler_version, SpirV). Reports per-file outcomes.
//!     --dry-run validates without writing the cache.
//! ```
//!
//! Exit codes:
//! - `0` — all files passed
//! - `1` — at least one file failed validation
//! - `2` — usage / I/O error
//!
//! Cache default location: `$HOME/.cache/atrium/shaders/` (or
//! `$XDG_CACHE_HOME/atrium/shaders/` when set). Overridden by
//! `--cache`.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};

use aqueduct_gpu::backends::{BackendId, GpuVendor};
use aqueduct_gpu::payloads::ShaderKind;
use aqueduct_gpu_host::shader_annotate;
use aqueduct_gpu_host::shader_cache::{CacheKey, ShaderCache};
use aqueduct_gpu_host::shader_inspect;
use aqueduct_gpu_host::shader_validator;

#[derive(Debug)]
enum Cmd {
    Check { file: PathBuf },
    Inspect { file: PathBuf },
    Annotate { file: PathBuf, max_iters: u32 },
    VerifyBundle { dir: PathBuf },
    Precompile {
        dir: PathBuf,
        cache_dir: PathBuf,
        backend: BackendId,
        compiler_version: u32,
        dry_run: bool,
    },
    Help,
}

fn parse_args() -> Result<Cmd> {
    let mut args = std::env::args().skip(1);
    let sub = args.next().ok_or_else(|| anyhow!("missing subcommand; try --help"))?;
    match sub.as_str() {
        "check" => {
            let f = args.next()
                .ok_or_else(|| anyhow!("check: missing <FILE> argument"))?;
            Ok(Cmd::Check { file: PathBuf::from(f) })
        }
        "inspect" => {
            let f = args.next()
                .ok_or_else(|| anyhow!("inspect: missing <FILE> argument"))?;
            Ok(Cmd::Inspect { file: PathBuf::from(f) })
        }
        "verify-bundle" => {
            let d = args.next()
                .ok_or_else(|| anyhow!("verify-bundle: missing <DIR> argument"))?;
            Ok(Cmd::VerifyBundle { dir: PathBuf::from(d) })
        }
        "annotate" => {
            let mut max_iters: Option<u32> = None;
            let mut file: Option<PathBuf> = None;
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--max-iters" => max_iters = Some(args.next()
                        .ok_or_else(|| anyhow!("--max-iters needs an argument"))?
                        .parse()?),
                    other if other.starts_with("--") =>
                        return Err(anyhow!("unknown flag {other:?}")),
                    other => {
                        if file.is_some() {
                            return Err(anyhow!("annotate: only one FILE argument allowed"));
                        }
                        file = Some(PathBuf::from(other));
                    }
                }
            }
            let file = file.ok_or_else(|| anyhow!("annotate: missing FILE argument"))?;
            let max_iters = max_iters.ok_or_else(|| anyhow!("annotate: --max-iters is required"))?;
            Ok(Cmd::Annotate { file, max_iters })
        }
        "precompile" => {
            let mut dir: Option<PathBuf> = None;
            let mut cache_dir: Option<PathBuf> = None;
            let mut backend_name = "software".to_string();
            let mut generation: u16 = 0;
            let mut compiler_version: u32 = 0;
            let mut dry_run = false;
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--cache" => cache_dir = Some(PathBuf::from(
                        args.next().ok_or_else(|| anyhow!("--cache needs an argument"))?
                    )),
                    "--backend" => backend_name = args.next()
                        .ok_or_else(|| anyhow!("--backend needs an argument"))?,
                    "--generation" => generation = args.next()
                        .ok_or_else(|| anyhow!("--generation needs an argument"))?
                        .parse()?,
                    "--compiler-version" => compiler_version = args.next()
                        .ok_or_else(|| anyhow!("--compiler-version needs an argument"))?
                        .parse()?,
                    "--dry-run" => dry_run = true,
                    other if other.starts_with("--") =>
                        return Err(anyhow!("unknown flag {other:?}")),
                    other => {
                        if dir.is_some() {
                            return Err(anyhow!("precompile: only one DIR argument allowed"));
                        }
                        dir = Some(PathBuf::from(other));
                    }
                }
            }
            let dir = dir.ok_or_else(|| anyhow!("precompile: missing DIR argument"))?;
            let cache_dir = cache_dir.unwrap_or_else(default_cache_dir);
            let backend = BackendId::new(parse_vendor(&backend_name)?, generation);
            Ok(Cmd::Precompile { dir, cache_dir, backend, compiler_version, dry_run })
        }
        "--help" | "-h" | "help" => Ok(Cmd::Help),
        other => Err(anyhow!("unknown subcommand {other:?}; try --help")),
    }
}

fn parse_vendor(s: &str) -> Result<GpuVendor> {
    Ok(match s {
        "software" => GpuVendor::Software,
        "apple"    => GpuVendor::Apple,
        "amd"      => GpuVendor::Amd,
        "intel"    => GpuVendor::Intel,
        "nvidia"   => GpuVendor::Nvidia,
        "atrium-gpu" | "atrium" => GpuVendor::AtriumGpu,
        other => return Err(anyhow!(
            "unknown --backend {other:?}; valid: software|apple|amd|intel|nvidia|atrium-gpu"
        )),
    })
}

fn default_cache_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        return PathBuf::from(xdg).join("atrium").join("shaders");
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".cache").join("atrium").join("shaders");
    }
    PathBuf::from("/tmp/atrium-shaders")
}

fn print_help() {
    eprintln!(
        "aqueduct-shader-tool — offline SPIR-V validator + cache populator\n\
         \n\
         Usage:\n\
         \x20   aqueduct-shader-tool check <FILE>\n\
         \x20   aqueduct-shader-tool precompile [--cache DIR] [--backend NAME]\n\
         \x20                                   [--generation N] [--compiler-version N]\n\
         \x20                                   [--dry-run] <DIR>\n\
         \n\
         Subcommands:\n\
         \x20 check        Validate a single SPIR-V module. Exit 0 / 1.\n\
         \x20 inspect      Diagnostic dump (version, caps, loops, etc.). Exit 0 / 1 / 2.\n\
         \x20 annotate     Inject MaxIterations into bare OpLoopMerge instructions.\n\
         \x20 verify-bundle Validate manifest.json + every referenced shader.\n\
         \x20 precompile   Validate every .spv under DIR; populate cache.\n\
         \n\
         --backend values: software (default) | apple | amd | intel | nvidia | atrium-gpu\n\
         --cache default:  $XDG_CACHE_HOME/atrium/shaders/  or  $HOME/.cache/atrium/shaders/\n\
         \n\
         See aqueduct_gpu_host::shader_validator for the rejection rules."
    );
}

fn main() -> ExitCode {
    let cmd = match parse_args() {
        Ok(c) => c,
        Err(e) => { eprintln!("aqueduct-shader-tool: {e}"); print_help(); return ExitCode::from(2); }
    };
    match cmd {
        Cmd::Help => { print_help(); ExitCode::from(0) }
        Cmd::VerifyBundle { dir } => match run_verify_bundle(&dir) {
            Ok(stats) => {
                println!("aqueduct-shader-tool verify-bundle: {} ok / {} rejected / {} errors",
                         stats.ok, stats.rejected, stats.errored);
                if stats.rejected + stats.errored == 0 { ExitCode::from(0) }
                else if stats.rejected > 0 { ExitCode::from(1) }
                else { ExitCode::from(2) }
            }
            Err(e) => { eprintln!("verify-bundle: {e}"); ExitCode::from(2) }
        },
        Cmd::Inspect { file } => match fs::read(&file) {
            Ok(bytes) => {
                let report = shader_inspect::inspect(&bytes);
                println!("{}:", file.display());
                print!("{report}");
                if report.warnings.is_empty() && report.all_loops_bounded() {
                    ExitCode::from(0)
                } else if !report.warnings.is_empty() {
                    ExitCode::from(2)
                } else {
                    // Loops present but not all bounded; strict validator
                    // would reject. Surface as exit 1 for CI gating.
                    ExitCode::from(1)
                }
            }
            Err(e) => { eprintln!("{}: I/O error: {e}", file.display()); ExitCode::from(2) }
        },
        Cmd::Annotate { file, max_iters } => match run_annotate(&file, max_iters) {
            Ok(patched) => {
                eprintln!("{}: annotated {patched} OpLoopMerge instruction(s) with MaxIterations({max_iters})",
                          file.display());
                ExitCode::from(0)
            }
            Err(e) => {
                eprintln!("{}: annotate failed: {e}", file.display());
                ExitCode::from(2)
            }
        },
        Cmd::Check { file } => match run_check(&file) {
            Ok(()) => ExitCode::from(0),
            Err(CheckError::Validator(diag)) => {
                eprintln!("{}: REJECTED  {diag}", file.display());
                ExitCode::from(1)
            }
            Err(CheckError::Io(e)) => {
                eprintln!("{}: I/O error: {e}", file.display());
                ExitCode::from(2)
            }
        },
        Cmd::Precompile { dir, cache_dir, backend, compiler_version, dry_run } => {
            match run_precompile(&dir, &cache_dir, backend, compiler_version, dry_run) {
                Ok(s) => {
                    println!("aqueduct-shader-tool: {} ok / {} rejected / {} errors",
                             s.ok, s.rejected, s.errored);
                    if s.rejected + s.errored == 0 { ExitCode::from(0) }
                    else if s.rejected > 0 { ExitCode::from(1) }
                    else { ExitCode::from(2) }
                }
                Err(e) => { eprintln!("aqueduct-shader-tool: {e}"); ExitCode::from(2) }
            }
        }
    }
}

#[derive(Debug)]
enum CheckError {
    Validator(String),
    Io(io::Error),
}

fn run_check(path: &Path) -> Result<(), CheckError> {
    let bytes = fs::read(path).map_err(CheckError::Io)?;
    match shader_validator::validate_spirv(&bytes) {
        Ok(()) => {
            let _ = writeln!(io::stderr(), "{}: OK  ({} bytes)", path.display(), bytes.len());
            Ok(())
        }
        Err(e) => Err(CheckError::Validator(e.to_string())),
    }
}

#[derive(Default)]
struct PrecompileStats {
    ok: usize,
    rejected: usize,
    errored: usize,
}

fn run_precompile(
    dir: &Path,
    cache_dir: &Path,
    backend: BackendId,
    compiler_version: u32,
    dry_run: bool,
) -> Result<PrecompileStats> {
    let cache = if dry_run {
        None
    } else {
        Some(ShaderCache::open(cache_dir)
            .with_context(|| format!("open cache dir {}", cache_dir.display()))?)
    };

    let files = collect_spv(dir)
        .with_context(|| format!("walk {}", dir.display()))?;
    if files.is_empty() {
        eprintln!("aqueduct-shader-tool: no .spv files found under {}", dir.display());
    }

    let mut stats = PrecompileStats::default();
    for f in &files {
        match fs::read(f) {
            Ok(bytes) => match shader_validator::validate_spirv(&bytes) {
                Ok(()) => {
                    let hash = sha256_32(&bytes);
                    let key = CacheKey {
                        bytecode_hash: hash,
                        backend,
                        compiler_version,
                        kind: ShaderKind::SpirV,
                    };
                    if let Some(c) = &cache {
                        if let Err(e) = c.insert(&key, &bytes) {
                            eprintln!("{}: validated but cache insert failed: {e}", f.display());
                            stats.errored += 1;
                            continue;
                        }
                    }
                    println!("{}: ok  ({} bytes)", f.display(), bytes.len());
                    stats.ok += 1;
                }
                Err(e) => {
                    eprintln!("{}: REJECTED  {e}", f.display());
                    stats.rejected += 1;
                }
            },
            Err(e) => {
                eprintln!("{}: read error: {e}", f.display());
                stats.errored += 1;
            }
        }
    }
    Ok(stats)
}

/// Verify a bundle directory's manifest.json + every shader it
/// references.
///
/// Manifest schema (subset we care about):
/// ```json
/// {
///   "name":    "atrium-core",
///   "version": 1,
///   "ops": [
///     { "id":              4096,
///       "name":            "rect",
///       "compute_entry":   "compute/op_rectangle.comp.spv:main",
///       "render_pipeline": "pipelines/pipe_rectangle" }
///   ]
/// }
/// ```
///
/// For each op:
/// - `compute_entry` is `<path>:<entry_name>`. The path part is a
///   `.spv` file relative to the bundle directory.
/// - `render_pipeline` is a path stem; both `<stem>.vert.spv` and
///   `<stem>.frag.spv` must exist + validate.
///
/// Missing-but-optional fields are tolerated (a compute-only or
/// render-only op is fine). Missing manifest.json is a hard error.
fn run_verify_bundle(dir: &Path) -> Result<PrecompileStats> {
    let manifest_path = dir.join("manifest.json");
    let manifest_bytes = fs::read(&manifest_path)
        .with_context(|| format!("read {}", manifest_path.display()))?;
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes)
        .with_context(|| format!("parse JSON {}", manifest_path.display()))?;

    // Top-level schema check.
    let name = manifest.get("name").and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("manifest.json: missing string field 'name'"))?;
    let version = manifest.get("version").and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("manifest.json: missing integer field 'version'"))?;
    let ops = manifest.get("ops").and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("manifest.json: missing array field 'ops'"))?;
    eprintln!("bundle '{name}' v{version}: {} op(s)", ops.len());

    let mut stats = PrecompileStats::default();
    let mut shader_paths: Vec<PathBuf> = Vec::new();

    for (i, op) in ops.iter().enumerate() {
        let op_id = op.get("id").and_then(|v| v.as_u64());
        let op_name = op.get("name").and_then(|v| v.as_str()).unwrap_or("?");
        if op_id.is_none() {
            return Err(anyhow!("ops[{i}]: missing integer 'id'"));
        }

        // compute_entry: <path>:<entry>
        if let Some(ce) = op.get("compute_entry").and_then(|v| v.as_str()) {
            let path_part = ce.split(':').next()
                .ok_or_else(|| anyhow!(
                    "ops[{i}]={op_name}: compute_entry {ce:?} missing path"
                ))?;
            shader_paths.push(dir.join(path_part));
        }
        // render_pipeline: stem → stem.vert.spv + stem.frag.spv
        if let Some(rp) = op.get("render_pipeline").and_then(|v| v.as_str()) {
            shader_paths.push(dir.join(format!("{rp}.vert.spv")));
            shader_paths.push(dir.join(format!("{rp}.frag.spv")));
        }
    }
    shader_paths.sort();
    shader_paths.dedup();

    for p in &shader_paths {
        match fs::read(p) {
            Ok(bytes) => match shader_validator::validate_spirv(&bytes) {
                Ok(()) => {
                    println!("{}: ok  ({} bytes)", p.display(), bytes.len());
                    stats.ok += 1;
                }
                Err(e) => {
                    eprintln!("{}: REJECTED  {e}", p.display());
                    stats.rejected += 1;
                }
            },
            Err(e) => {
                eprintln!("{}: read error: {e}", p.display());
                stats.errored += 1;
            }
        }
    }
    Ok(stats)
}

/// Read `file`, run [`shader_annotate::annotate_loop_merges`], write
/// the result back atomically (temp + rename). Returns the number of
/// instructions patched.
fn run_annotate(path: &Path, max_iters: u32) -> Result<usize> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let report = shader_annotate::annotate_loop_merges(&bytes, max_iters)
        .map_err(|e| anyhow!("{e}"))?;
    let tmp = path.with_extension("spv.annotate-tmp");
    fs::write(&tmp, &report.bytes).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("rename {}", path.display()))?;
    if report.already_bounded > 0 {
        eprintln!("{}: skipped {} already-bounded OpLoopMerge instruction(s)",
                  path.display(), report.already_bounded);
    }
    Ok(report.patched)
}

/// Compute SHA-256 of `bytes` and return the 32-byte digest.
fn sha256_32(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// Recursively gather all .spv files under `dir`. Deterministic
/// (sorted) for reproducible CI output.
fn collect_spv(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk(dir, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let p = entry.path();
        if ft.is_dir() {
            walk(&p, out)?;
        } else if ft.is_file() {
            if p.extension().and_then(|s| s.to_str()) == Some("spv") {
                out.push(p);
            }
        }
    }
    Ok(())
}
