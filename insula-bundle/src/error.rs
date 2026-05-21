//! Error types for bundle reading.

use std::path::PathBuf;
use thiserror::Error;

/// Bundle-reading error.
#[derive(Debug, Error)]
pub enum Error {
    /// Path passed to [`crate::InsulaBundle::read`] is
    /// not a directory.
    #[error("bundle root is not a directory: {0}")]
    BundleRootNotADir(PathBuf),

    /// `manifest.toml` could not be read.
    #[error("manifest read error at {path}: {source}")]
    ManifestRead {
        /// The path that was attempted.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// `manifest.toml` parsed-failure.
    #[error("manifest parse error: {0}")]
    ManifestParse(#[from] insula_manifest::Error),

    /// The binary the manifest declares does not exist
    /// at the expected path inside the bundle.
    #[error("binary missing at {expected}: declared as entry = {entry:?}")]
    BinaryMissing {
        /// Where the file was expected.
        expected: PathBuf,
        /// The `bundle.entry` value from the manifest.
        entry: String,
    },
}
