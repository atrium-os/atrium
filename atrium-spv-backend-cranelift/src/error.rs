//! Cranelift backend error type.

/// Failures from the Cranelift backend.
#[derive(Debug, Clone)]
pub enum BackendError {
    /// IR shape that this phase of the backend doesn't
    /// (yet) translate. The harness reads this as "skip
    /// this runner"; the production driver reads it as
    /// "try bespoke or report the shader as unsupported".
    Unsupported(String),
    /// Cranelift / Object-IO error. Indicates a bug; the
    /// SPIR-V was structurally valid through the frontend.
    Internal(String),
}

impl std::fmt::Display for BackendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackendError::Unsupported(s) => write!(f, "unsupported: {s}"),
            BackendError::Internal(s) => write!(f, "internal: {s}"),
        }
    }
}

impl std::error::Error for BackendError {}
