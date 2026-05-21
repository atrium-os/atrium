//! Error type for manifest parsing.

use thiserror::Error;

/// Manifest parse / validation error.
#[derive(Debug, Error)]
pub enum Error {
    /// Underlying TOML parse failure.
    #[error("TOML parse error: {0}")]
    TomlParse(#[from] toml::de::Error),

    /// Serialization failure (unusual; bugs in [`Manifest`]
    /// derive trip this).
    #[error("TOML serialize error: {0}")]
    TomlSerialize(#[from] toml::ser::Error),

    /// Strict-mode-only: the manifest contained top-level
    /// tables this parser does not recognize.
    ///
    /// Permissive parsing collects these into
    /// `Manifest::extra` instead of erroring.
    #[error("unknown top-level sections: {}", .0.join(", "))]
    UnknownSections(Vec<String>),
}
