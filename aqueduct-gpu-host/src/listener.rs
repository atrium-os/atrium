//! Unix-domain-socket listener for incoming guest connections.
//!
//! Each accepted connection becomes a [`Session`] running on its
//! own OS thread. The listener owns the backend Arc and clones it
//! per session — backends are designed to be `Send + Sync` so the
//! shared MoltenVK device (or stub state) is safely accessible
//! across sessions.

use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result};

use aqueduct::Connection;

use crate::backend::Backend;
use crate::session::Session;

/// The host endpoint's accept loop.
pub struct Listener {
    socket_path: PathBuf,
    listener: UnixListener,
    backend: Arc<dyn Backend>,
}

impl Listener {
    /// Bind a fresh listener at `socket_path`. Removes any existing
    /// socket file at that path (the daemon owns the path
    /// exclusively per its lifetime).
    pub fn bind(socket_path: impl AsRef<Path>, backend: Arc<dyn Backend>) -> Result<Self> {
        let socket_path = socket_path.as_ref().to_path_buf();
        let _ = std::fs::remove_file(&socket_path);
        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("bind {}", socket_path.display()))?;
        log::info!("listening on {}", socket_path.display());
        Ok(Self { socket_path, listener, backend })
    }

    /// The socket path the listener is bound to.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Accept connections until interrupted. Each accepted
    /// connection becomes a thread running a [`Session::run`] loop.
    /// Returns only on listener error (typically the socket being
    /// closed externally).
    pub fn accept_loop(self) -> Result<()> {
        for incoming in self.listener.incoming() {
            match incoming {
                Ok(stream) => {
                    let conn = Connection::wrap(stream)
                        .context("wrap accepted stream")?;
                    let backend = Arc::clone(&self.backend);
                    log::info!("new client connection");
                    thread::Builder::new()
                        .name("aqueduct-gpu-session".to_string())
                        .spawn(move || {
                            if let Err(e) = Session::new(conn, backend).run() {
                                log::warn!("session ended with error: {e}");
                            } else {
                                log::info!("session ended cleanly");
                            }
                        })
                        .context("spawn session thread")?;
                }
                Err(e) => {
                    log::warn!("accept error: {e}");
                    break;
                }
            }
        }
        Ok(())
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // Best-effort socket cleanup. We don't fail on missing path.
        let _ = std::fs::remove_file(&self.socket_path);
    }
}
