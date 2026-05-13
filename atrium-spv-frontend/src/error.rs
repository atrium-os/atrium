//! Frontend error type.

/// All ways frontend translation can fail.
///
/// `atrium-spv-compile` translates these to
/// `VK_ERROR_INVALID_SHADER_NV` for the app on entry-point
/// load failure; well-behaved apps gracefully degrade or
/// switch to a different shader variant.
#[derive(Debug, Clone)]
pub enum FrontendError {
    /// SPIR-V byte stream couldn't be parsed.
    ParseFailed(String),
    /// Shader uses a SPIR-V feature the frontend doesn't
    /// (yet) implement. Includes a human-readable
    /// description of what was unsupported.
    Unsupported(String),
    /// IR construction failed for a reason the frontend
    /// considers internal (shouldn't happen from valid
    /// SPIR-V; report as a bug).
    Internal(String),
    /// SPIR-V structurally violates the rules (forward
    /// reference, missing type, dangling id). May indicate
    /// a corrupt SPIR-V binary or a tool we should reject.
    Malformed(String),
}

impl std::fmt::Display for FrontendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrontendError::ParseFailed(s) => write!(f, "parse failed: {s}"),
            FrontendError::Unsupported(s) => write!(f, "unsupported: {s}"),
            FrontendError::Internal(s) => write!(f, "internal error: {s}"),
            FrontendError::Malformed(s) => write!(f, "malformed SPIR-V: {s}"),
        }
    }
}

impl std::error::Error for FrontendError {}
