//! aqueduct-gpu-host daemon entry point.
//!
//! Phase 1.3a landed the daemon scaffolding + StubBackend.
//! Phase 1.3b lands MoltenVkBackend (binds ash::Entry::load,
//!   real Vulkan instance + device, frame stream → VkCommandBuffer).
//! Phase 1.3c lands SoftwareBackend (tiny-skia rasterisation of
//!   Atrium-native bundle ops; the stub is in place but submit_frame
//!   panics until the tiny-skia integration lands).
//!
//! Usage:
//!     aqueduct-gpu-host [--socket /tmp/aqueduct-gpu.sock]
//!                       [--backend stub|software|moltenvk]
//!                       [--tier2]
//!                       [--cache-root PATH]
//!                       [--compile-binary PATH]
//!
//! `--backend` defaults to `stub` (protocol-correct, no GPU work).
//! See `docs/spec/aqueduct-gpu.md` §6.5 for backend tier semantics.
//!
//! `--tier2` attaches a process-wide [`Tier2Registry`] so every
//! SPIR-V upload is routed through `atrium-spv-compile` into a
//! dlopened `.so`.  Off by default: tier-2 needs `atrium-spv-
//! compile` reachable on disk + a writable cache directory, and
//! most legacy callers (tier-1 demos, protocol-only fixtures)
//! don't want to pay for the compile.  See `docs/spec/tier2-
//! renderer.md` for the dispatch story.
//!
//! `--cache-root` / `--compile-binary` override
//! [`LoaderConfig::production`]'s defaults
//! (`/var/atrium/shaders` and `/usr/local/libexec/atrium-spv-
//! compile`).  Dev runs that lack root point both at workspace-
//! local paths, e.g.:
//!
//!     aqueduct-gpu-host --tier2 \
//!         --cache-root /tmp/atrium-shaders \
//!         --compile-binary ./target/debug/atrium-spv-compile
//!
//! Logs honour `RUST_LOG`. Default level: `info`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};

use aqueduct_gpu_host::{
    Backend, Listener, MoltenVkBackend, MoltenVkError, SoftwareBackend, StubBackend,
    Tier2Registry,
};
use atrium_spv_loader::LoaderConfig;

const DEFAULT_SOCKET: &str = "/tmp/aqueduct-gpu.sock";

/// Backend kind selected at daemon startup. Cannot change mid-run
/// in Phase 1; live policy switching deferred to Phase 2+.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendKind {
    /// `StubBackend` — protocol-correct, no GPU work. The Phase 1
    /// test target; useful for wire-path verification without a
    /// real GPU stack.
    Stub,
    /// `SoftwareBackend` — tier-1 tiny-skia rasterisation of
    /// Atrium-native bundle ops. (Stub until Phase 1.3c.)
    Software,
    /// `MoltenVkBackend` — real GPU via Apple's MoltenVK on macOS.
    /// Not yet implemented; reserved for Phase 1.3b.
    MoltenVk,
}

impl BackendKind {
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "stub"     => BackendKind::Stub,
            "software" => BackendKind::Software,
            "moltenvk" => BackendKind::MoltenVk,
            other => return Err(anyhow!(
                "unknown --backend {other:?}; valid: stub | software | moltenvk"
            )),
        })
    }
}

struct Args {
    socket: PathBuf,
    backend: BackendKind,
    /// `true` if `--tier2` was passed; gates the Tier2Registry
    /// attach in `main`.
    tier2: bool,
    /// Override for `LoaderConfig.cache_root`.  `None` means
    /// keep `LoaderConfig::production()`'s value.
    cache_root: Option<PathBuf>,
    /// Override for `LoaderConfig.compile_binary`.  `None` means
    /// keep `LoaderConfig::production()`'s value.
    compile_binary: Option<PathBuf>,
}

fn parse_args() -> Result<Args> {
    let mut socket: Option<PathBuf> = None;
    let mut backend = BackendKind::Stub;
    let mut tier2 = false;
    let mut cache_root: Option<PathBuf> = None;
    let mut compile_binary: Option<PathBuf> = None;
    let mut iter = std::env::args().skip(1);
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--socket" => {
                socket = Some(PathBuf::from(
                    iter.next().ok_or_else(|| anyhow!("--socket needs an argument"))?
                ));
            }
            "--backend" => {
                let v = iter.next().ok_or_else(|| anyhow!("--backend needs an argument"))?;
                backend = BackendKind::from_str(&v)?;
            }
            "--tier2" => {
                tier2 = true;
            }
            "--cache-root" => {
                cache_root = Some(PathBuf::from(
                    iter.next().ok_or_else(|| anyhow!("--cache-root needs an argument"))?
                ));
            }
            "--compile-binary" => {
                compile_binary = Some(PathBuf::from(
                    iter.next().ok_or_else(|| anyhow!("--compile-binary needs an argument"))?
                ));
            }
            "--help" | "-h" => {
                println!("usage: aqueduct-gpu-host [--socket PATH] \
                    [--backend stub|software|moltenvk] \
                    [--tier2] [--cache-root PATH] [--compile-binary PATH]");
                std::process::exit(0);
            }
            other => return Err(anyhow!("unknown arg {other:?}; try --help")),
        }
    }
    if !tier2 && (cache_root.is_some() || compile_binary.is_some()) {
        return Err(anyhow!(
            "--cache-root / --compile-binary require --tier2"
        ));
    }
    Ok(Args {
        socket: socket.unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET)),
        backend,
        tier2,
        cache_root,
        compile_binary,
    })
}

/// Build the [`Tier2Registry`] from a base [`LoaderConfig::production`]
/// with any CLI overrides applied.  Ensures the cache directory exists
/// (the loader requires it on first compile) and reports the chosen
/// paths through `log::info!` for operational visibility.
fn make_tier2_registry(
    cache_root: Option<PathBuf>,
    compile_binary: Option<PathBuf>,
) -> Result<Arc<Tier2Registry>> {
    let mut cfg = LoaderConfig::production();
    if let Some(p) = cache_root { cfg.cache_root = p; }
    if let Some(p) = compile_binary { cfg.compile_binary = p; }
    std::fs::create_dir_all(&cfg.cache_root)
        .with_context(|| format!(
            "tier-2 cache-root {} not creatable",
            cfg.cache_root.display(),
        ))?;
    if !cfg.compile_binary.exists() {
        // Not fatal at startup -- the binary might be installed
        // after the daemon launches, and tier-2 compile failure
        // is non-fatal at the wire level (the SPIR-V upload still
        // succeeds with tier2_id=None).  But warn loudly so the
        // operator sees the mis-configuration.
        log::warn!(
            "tier-2 compile-binary {} does not exist at startup; \
             every SPIR-V upload will fall through with \
             tier2_id=None until it appears",
            cfg.compile_binary.display(),
        );
    }
    log::info!(
        "tier-2 registry attached: cache-root={}, compile-binary={}",
        cfg.cache_root.display(), cfg.compile_binary.display(),
    );
    Ok(Arc::new(Tier2Registry::new(cfg)))
}

fn make_backend(kind: BackendKind) -> Result<Arc<dyn Backend>> {
    match kind {
        BackendKind::Stub     => Ok(Arc::new(StubBackend::new())),
        BackendKind::Software => Ok(Arc::new(SoftwareBackend::new())),
        BackendKind::MoltenVk => match MoltenVkBackend::new() {
            Ok(b) => {
                log::info!("MoltenVK backend: {}", b.device_summary());
                Ok(Arc::new(b))
            }
            Err(MoltenVkError::LoaderUnavailable(e)) => {
                log::warn!("MoltenVK loader unavailable: {e}; falling back to SoftwareBackend");
                Ok(Arc::new(SoftwareBackend::new()))
            }
            Err(e) => Err(anyhow!("MoltenVK init failed: {e}")),
        },
    }
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .init();

    let args = parse_args()?;
    log::info!(
        "aqueduct-gpu-host starting (backend: {:?}, tier2: {})",
        args.backend, args.tier2,
    );
    let backend = make_backend(args.backend)?;
    let mut listener = Listener::bind(&args.socket, backend)?;
    if args.tier2 {
        let reg = make_tier2_registry(args.cache_root, args.compile_binary)?;
        listener = listener.with_tier2_registry(reg);
    }
    listener.accept_loop()?;
    Ok(())
}
