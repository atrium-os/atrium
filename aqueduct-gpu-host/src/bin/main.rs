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
//!
//! `--backend` defaults to `stub` (protocol-correct, no GPU work).
//! See `docs/spec/aqueduct-gpu.md` §6.5 for backend tier semantics.
//!
//! Logs honour `RUST_LOG`. Default level: `info`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Result};

use aqueduct_gpu_host::{
    Backend, Listener, MoltenVkBackend, MoltenVkError, SoftwareBackend, StubBackend,
};

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
}

fn parse_args() -> Result<Args> {
    let mut socket: Option<PathBuf> = None;
    let mut backend = BackendKind::Stub;
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
            "--help" | "-h" => {
                println!("usage: aqueduct-gpu-host [--socket PATH] [--backend stub|software|moltenvk]");
                std::process::exit(0);
            }
            other => return Err(anyhow!("unknown arg {other:?}; try --help")),
        }
    }
    Ok(Args {
        socket: socket.unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET)),
        backend,
    })
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
    log::info!("aqueduct-gpu-host starting (backend: {:?})", args.backend);
    let backend = make_backend(args.backend)?;
    let listener = Listener::bind(&args.socket, backend)?;
    listener.accept_loop()?;
    Ok(())
}
