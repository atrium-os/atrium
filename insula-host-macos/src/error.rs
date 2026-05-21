//! Error type for host-adapter operations.

use thiserror::Error;

/// Host-adapter operation error.
#[derive(Debug, Error)]
pub enum Error {
    /// The manifest declares something the host adapter
    /// can't currently translate (e.g. a capability that
    /// has no macOS equivalent yet).
    #[error("unsupported manifest feature on macOS host: {0}")]
    UnsupportedFeature(String),

    /// Manifest is internally inconsistent (e.g.
    /// `bundle.form = "wasm"` with non-empty `arches`).
    #[error("manifest validation error: {0}")]
    ManifestInvalid(String),
}
