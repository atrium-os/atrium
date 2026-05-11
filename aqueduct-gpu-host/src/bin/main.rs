//! aqueduct-gpu-host daemon entry point.
//!
//! Phase 1.3a: stub backend, single-listener, multi-thread accept
//! loop. Phase 1.3b will swap StubBackend for MoltenVkBackend.
//!
//! Usage:
//!     aqueduct-gpu-host [--socket /tmp/aqueduct-gpu.sock]
//!
//! Logs honour the `RUST_LOG` env var (env_logger). Default level
//! is `info`.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;

use aqueduct_gpu_host::{Listener, StubBackend};

const DEFAULT_SOCKET: &str = "/tmp/aqueduct-gpu.sock";

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .init();

    let socket = parse_socket_arg().unwrap_or_else(|| PathBuf::from(DEFAULT_SOCKET));

    log::info!("aqueduct-gpu-host starting (stub backend)");
    let backend = Arc::new(StubBackend::new());
    let listener = Listener::bind(&socket, backend)?;
    listener.accept_loop()?;
    Ok(())
}

fn parse_socket_arg() -> Option<PathBuf> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--socket" {
            return args.next().map(PathBuf::from);
        }
    }
    None
}
