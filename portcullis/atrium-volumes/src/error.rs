//! Typed errors for atrium-volumes.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum VolumesError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("policy file: {0}")]
    Policy(String),

    #[error("state file: {0}")]
    State(String),

    #[error("protocol: {0}")]
    Protocol(#[from] serde_json::Error),

    #[error("policy violation: {rule}: {detail}")]
    PolicyViolation { rule: &'static str, detail: String },

    #[error("backend {name} unavailable; configured: {configured:?}")]
    BackendUnavailable { name: String, configured: Vec<String> },

    #[error("backend {kind} does not support {feature}")]
    BackendDoesNotSupport { kind: String, feature: &'static str },
}
