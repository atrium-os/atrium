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
//!                       [--backend stub|software|moltenvk|tier2]
//!                       [--tier2]
//!                       [--cache-root PATH]
//!                       [--compile-binary PATH]
//!
//! `--backend` defaults to `stub` (protocol-correct, no GPU work).
//! See `docs/spec/aqueduct-gpu.md` §6.5 for backend tier semantics.
//!
//! `--backend tier2` selects [`Tier2Backend`], which actually runs
//! the SPIR-V shaders compiled by atrium-spv-compile through
//! atrium-spv-runtime.  Requires `--tier2` to also be passed so
//! the matching [`Tier2Registry`] is attached to the listener;
//! without it, every SPIR-V upload would arrive with
//! `tier2_id=None` and every draw would be skipped.  The
//! end-to-end "real Vulkan app exercises atrium-spv-*" path:
//!
//!     aqueduct-gpu-host --backend tier2 --tier2 \
//!         --cache-root /tmp/atrium-shaders \
//!         --compile-binary ./target/debug/atrium-spv-compile
//!
//! `--tier2` without `--backend tier2` attaches the registry so
//! SPIR-V uploads get compiled + cached, but the dispatch backend
//! (stub / software / moltenvk) ignores the `tier2_id`.  Useful
//! for warming the compile cache from a CI run that doesn't
//! care about the dispatch output.
//!
//! `--cache-root` / `--compile-binary` override
//! [`LoaderConfig::production`]'s defaults
//! (`/var/atrium/shaders` and `/usr/local/libexec/atrium-spv-
//! compile`).  Dev runs that lack root point both at workspace-
//! local paths.
//!
//! Logs honour `RUST_LOG`. Default level: `info`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};

use aqueduct_gpu_host::{
    Backend, CostModelBackend, CpuProfile, DeviceProfile, GpuPowerModel, Listener,
    MoltenVkBackend, MoltenVkError, RouteMode, SoftwareBackend, StubBackend, Tier2Backend,
    Tier2Registry,
};
use atrium_spv_loader::LoaderConfig;

const DEFAULT_SOCKET: &str = "/tmp/aqueduct-gpu.sock";
const DEFAULT_CARILLON_SOCK: &str = "/tmp/carillon.sock";
const DEFAULT_CARILLON_SHM: &str = "/tmp/carillon.shm";

/// Where the daemon listens for client work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transport {
    /// Unix-socket aqueduct envelope listener (host dev / CI default).
    Socket,
    /// Carillon ivshmem-doorbell endpoint (the FreeBSD-VM path): stand up
    /// an `IvshmemServer`, wait for QEMU, and run `serve_ivshmem` against
    /// the selected backend. See docs/spec/carillon.md.
    Carillon,
}

/// Backend kind selected at daemon startup. Cannot change mid-run
/// in Phase 1; live policy switching deferred to Phase 2+.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BackendKind {
    /// `StubBackend` — protocol-correct, no GPU work. The Phase 1
    /// test target; useful for wire-path verification without a
    /// real GPU stack.
    Stub,
    /// `SoftwareBackend` — tier-1 tiny-skia rasterisation of
    /// Atrium-native bundle ops.  Rejects third-party SPIR-V
    /// (tier-2 territory).
    Software,
    /// `MoltenVkBackend` — real GPU via Apple's MoltenVK on macOS.
    MoltenVk,
    /// `Tier2Backend` — runs SPIR-V shaders compiled by atrium-spv-
    /// compile through atrium-spv-runtime.  Requires `--tier2` to
    /// also be passed so the matching `Tier2Registry` is attached
    /// to the listener; without it, every SPIR-V upload would
    /// arrive with `tier2_id=None` and every draw would be skipped.
    Tier2,
}

impl BackendKind {
    fn from_str(s: &str) -> Result<Self> {
        Ok(match s {
            "stub"     => BackendKind::Stub,
            "software" => BackendKind::Software,
            "moltenvk" => BackendKind::MoltenVk,
            "tier2"    => BackendKind::Tier2,
            other => return Err(anyhow!(
                "unknown --backend {other:?}; \
                 valid: stub | software | moltenvk | tier2"
            )),
        })
    }
}

struct Args {
    socket: PathBuf,
    transport: Transport,
    /// Carillon-mode socket QEMU connects to + shm backing file.
    carillon_sock: PathBuf,
    carillon_shm: PathBuf,
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
    /// Path to a `spirv-opt` binary.  When set, each shader
    /// upload runs through `spirv-opt --ssa-rewrite
    /// --eliminate-dead-code-aggressive` before validate, so
    /// slangc's `OpVariable Function` lands as proper SSA.
    /// Arc 144.
    spirv_opt_binary: Option<PathBuf>,
    /// `--device-profile NAME`: wrap the chosen backend in the GPU
    /// device-cost model (accounting mode). `None` = no model (today's
    /// zero-overhead path). See docs/spec/gpu-device-model.md.
    device_profile: Option<DeviceProfile>,
    /// `--route perf|balanced|battery`: enable live Tier-2↔Tier-3 routing
    /// observation on the device model (requires `--device-profile`). The
    /// verdict is tallied + logged, not acted on (accounting-safe).
    route: Option<RouteMode>,
}

fn parse_args() -> Result<Args> {
    let mut socket: Option<PathBuf> = None;
    let mut transport = Transport::Socket;
    let mut carillon_sock: Option<PathBuf> = None;
    let mut carillon_shm: Option<PathBuf> = None;
    let mut backend = BackendKind::Stub;
    let mut tier2 = false;
    let mut cache_root: Option<PathBuf> = None;
    let mut compile_binary: Option<PathBuf> = None;
    let mut spirv_opt_binary: Option<PathBuf> = None;
    let mut device_profile: Option<DeviceProfile> = None;
    let mut route: Option<RouteMode> = None;
    let mut iter = std::env::args().skip(1);
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--socket" => {
                socket = Some(PathBuf::from(
                    iter.next().ok_or_else(|| anyhow!("--socket needs an argument"))?
                ));
            }
            "--transport" => {
                let v = iter.next().ok_or_else(|| anyhow!("--transport needs an argument"))?;
                transport = match v.as_str() {
                    "socket"   => Transport::Socket,
                    "carillon" => Transport::Carillon,
                    other => return Err(anyhow!(
                        "unknown --transport {other:?}; valid: socket | carillon")),
                };
            }
            "--carillon-sock" => {
                carillon_sock = Some(PathBuf::from(
                    iter.next().ok_or_else(|| anyhow!("--carillon-sock needs an argument"))?
                ));
            }
            "--carillon-shm" => {
                carillon_shm = Some(PathBuf::from(
                    iter.next().ok_or_else(|| anyhow!("--carillon-shm needs an argument"))?
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
            "--spirv-opt-binary" => {
                spirv_opt_binary = Some(PathBuf::from(
                    iter.next().ok_or_else(|| anyhow!("--spirv-opt-binary needs an argument"))?
                ));
            }
            "--device-profile" => {
                let v = iter.next()
                    .ok_or_else(|| anyhow!("--device-profile needs an argument"))?;
                device_profile = Some(DeviceProfile::from_name(&v).ok_or_else(|| anyhow!(
                    "unknown --device-profile {v:?}; valid: passthrough | \
                     uma-apple-m4-max | discrete-rdna3-pcie4x16"))?);
            }
            "--route" => {
                let v = iter.next().ok_or_else(|| anyhow!("--route needs an argument"))?;
                route = Some(match v.as_str() {
                    "perf"     => RouteMode::Perf,
                    "balanced" => RouteMode::Balanced,
                    "battery"  => RouteMode::Battery,
                    other => return Err(anyhow!(
                        "unknown --route {other:?}; valid: perf | balanced | battery")),
                });
            }
            "--help" | "-h" => {
                println!("usage: aqueduct-gpu-host [--socket PATH] \
                    [--transport socket|carillon] \
                    [--carillon-sock PATH] [--carillon-shm PATH] \
                    [--backend stub|software|moltenvk|tier2] \
                    [--tier2] [--cache-root PATH] [--compile-binary PATH] \
                    [--spirv-opt-binary PATH] \
                    [--device-profile passthrough|uma-apple-m4-max|discrete-rdna3-pcie4x16] \
                    [--route perf|balanced|battery]");
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
    if route.is_some() && device_profile.is_none() {
        return Err(anyhow!(
            "--route requires --device-profile (routing scores against the \
             device model's Tier-3 cost)"
        ));
    }
    if backend == BackendKind::Tier2 && !tier2 {
        return Err(anyhow!(
            "--backend tier2 requires --tier2 (the matching \
             Tier2Registry must be attached to the listener so \
             SPIR-V uploads have somewhere to compile through)"
        ));
    }
    Ok(Args {
        socket: socket.unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET)),
        transport,
        carillon_sock: carillon_sock.unwrap_or_else(|| PathBuf::from(DEFAULT_CARILLON_SOCK)),
        carillon_shm: carillon_shm.unwrap_or_else(|| PathBuf::from(DEFAULT_CARILLON_SHM)),
        backend,
        tier2,
        cache_root,
        compile_binary,
        spirv_opt_binary,
        device_profile,
        route,
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

fn make_backend(
    kind: BackendKind,
    tier2_registry: Option<&Arc<Tier2Registry>>,
    device_profile: Option<DeviceProfile>,
    route: Option<RouteMode>,
) -> Result<Arc<dyn Backend>> {
    // Wrap a concrete backend in the device-cost model when --device-profile
    // is set, then erase to the trait object. CostModelBackend<B> is a
    // transparent forwarder, so the wrapped backend behaves identically
    // (accounting mode) while charging modeled cost per op. With --route it
    // also scores each frame Tier-2 vs Tier-3 (observational tally).
    fn finish<B: Backend + 'static>(
        b: B, profile: &Option<DeviceProfile>, route: Option<RouteMode>,
    ) -> Arc<dyn Backend> {
        match profile {
            Some(p) => {
                log::info!("device-model wrapping backend with profile '{}'", p.name);
                let cm = CostModelBackend::new(b, p.clone());
                let cm = match route {
                    Some(mode) => {
                        log::info!("live routing enabled (mode {mode:?})");
                        cm.with_routing(
                            CpuProfile::apple_m4_max(), GpuPowerModel::apple_m4_max(), mode)
                    }
                    None => cm,
                };
                Arc::new(cm)
            }
            None => Arc::new(b),
        }
    }
    match kind {
        BackendKind::Stub     => Ok(finish(StubBackend::new(), &device_profile, route)),
        BackendKind::Software => Ok(finish(SoftwareBackend::new(), &device_profile, route)),
        BackendKind::MoltenVk => match MoltenVkBackend::new() {
            Ok(b) => {
                log::info!("MoltenVK backend: {}", b.device_summary());
                Ok(finish(b, &device_profile, route))
            }
            Err(MoltenVkError::LoaderUnavailable(e)) => {
                log::warn!("MoltenVK loader unavailable: {e}; falling back to SoftwareBackend");
                Ok(finish(SoftwareBackend::new(), &device_profile, route))
            }
            Err(e) => Err(anyhow!("MoltenVK init failed: {e}")),
        },
        BackendKind::Tier2 => {
            // The CLI parser already enforced --tier2 when this
            // arm is reached, so the registry is present.
            let reg = tier2_registry
                .ok_or_else(|| anyhow!(
                    "internal: BackendKind::Tier2 reached make_backend \
                     without a registry -- parse_args should have rejected"
                ))?
                .clone();
            log::info!("tier-2 backend selected; draws against \
                tier2_id-bearing pipelines will dispatch through \
                atrium-spv-runtime");
            Ok(finish(Tier2Backend::new(reg), &device_profile, route))
        }
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

    // Build the registry up front (when --tier2): both the
    // listener and the Tier2Backend point at the same Arc, so
    // SPIR-V uploads land in the same compiled-shader pool the
    // dispatch path reads from.
    let registry: Option<Arc<Tier2Registry>> = if args.tier2 {
        Some(make_tier2_registry(args.cache_root, args.compile_binary)?)
    } else {
        None
    };

    let backend = make_backend(
        args.backend, registry.as_ref(), args.device_profile.clone(), args.route)?;

    // Carillon (FreeBSD-VM) transport: stand up the ivshmem-doorbell
    // endpoint instead of the Unix-socket listener.
    if args.transport == Transport::Carillon {
        #[cfg(unix)]
        {
            return run_carillon(
                &args.carillon_sock,
                &args.carillon_shm,
                args.backend,
                backend,
            );
        }
        #[cfg(not(unix))]
        {
            return Err(anyhow!("--transport carillon requires a unix host"));
        }
    }

    let mut listener = Listener::bind(&args.socket, backend)?;
    if let Some(reg) = registry {
        listener = listener.with_tier2_registry(reg);
    }
    if let Some(p) = args.spirv_opt_binary {
        if !p.exists() {
            log::warn!(
                "--spirv-opt-binary {} does not exist at startup; \
                 SSA-rewrite will be skipped (slangc shaders with \
                 OpVariable Function will fail compile until the \
                 binary appears)",
                p.display(),
            );
        } else {
            log::info!("spirv-opt SSA-rewrite enabled: {}", p.display());
        }
        listener = listener.with_spirv_opt_binary(p);
    }
    listener.accept_loop()?;
    Ok(())
}

/// Carillon transport (bridge mode): create the `IvshmemServer`, wait for
/// QEMU to connect (run-vm.sh --carillon), then bridge the shared-memory
/// byte FIFOs to a real `Session` over a socketpair — so the guest drives
/// the **full aqueduct-gpu wire** (handshake, resource creation, frame
/// submit, readback) against the selected backend, exactly as a socket
/// client would. The daemon runs until killed.
///
/// Verified host-side by `tests/carillon_wire.rs` (a real frame to Metal
/// through the same byte FIFOs + bridge). The VM session additionally
/// exercises the QEMU `send_init` handshake + MSI-X doorbell delivery.
#[cfg(unix)]
fn run_carillon(
    sock: &std::path::Path,
    shm: &std::path::Path,
    backend_kind: BackendKind,
    backend: Arc<dyn Backend>,
) -> Result<()> {
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::UnixStream;

    use aqueduct::Connection;
    use aqueduct_gpu_host::carillon::{
        layout, pump_fd_to_stream_with, pump_stream_to_fd_with, Doorbell, Waiter,
    };
    use aqueduct_gpu_host::{IvshmemServer, Session};

    let mut server = IvshmemServer::new(sock, shm, layout::TOTAL_SIZE)
        .with_context(|| format!("carillon: IvshmemServer on {}", sock.display()))?;
    log::info!(
        "carillon: IvshmemServer on {} (shm {}); launch scripts/run-vm.sh \
         --carillon and waiting for QEMU…",
        sock.display(),
        shm.display(),
    );

    while !server.try_accept()? {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    log::info!(
        "carillon: QEMU connected; bridging the full wire to a Session on \
         the {:?} backend",
        backend_kind
    );

    // A real Session on one socketpair end; the bridge pumps shuttle bytes
    // between the shared-memory FIFOs and the other end.
    let (sess_sock, bridge_sock) = UnixStream::pair()?;
    let bridge_fd = bridge_sock.as_raw_fd();
    let shutdown = Doorbell::new()?;

    std::thread::scope(|s| {
        // Session: reads requests from sess_sock, writes responses back.
        let be = backend.clone();
        s.spawn(move || {
            match Connection::wrap(sess_sock) {
                Ok(conn) => {
                    let _ = Session::new(conn, be).run();
                }
                Err(e) => log::error!("carillon: Connection::wrap: {e}"),
            }
        });
        // g2h FIFO (guest requests) → Session socket. Wakes on the
        // guest→host doorbell (server_read_fd); exits on shutdown.
        s.spawn(|| {
            let waiter = match Waiter::new(server.doorbell_read_fd(), shutdown.read_fd()) {
                Ok(w) => w,
                Err(e) => { log::error!("carillon: waiter: {e}"); return; }
            };
            pump_stream_to_fd_with(
                bridge_fd, server.region(), layout::STREAM_G2H_OFFSET,
                layout::C_STREAM_G2H_HEAD, layout::C_STREAM_G2H_TAIL,
                || match waiter.wait() {
                    Ok(w) => !w.shutdown,
                    Err(_) => false,
                },
            );
        });
        // Session socket → h2g FIFO (responses); ring the guest (MSI-X).
        s.spawn(|| {
            pump_fd_to_stream_with(
                bridge_fd, server.region(), layout::STREAM_H2G_OFFSET,
                layout::C_STREAM_H2G_HEAD, layout::C_STREAM_H2G_TAIL,
                || server.notify_peer(),
            );
        });
    });
    Ok(())
}
