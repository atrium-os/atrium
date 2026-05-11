//! Errors surfaced by the client crate.
//!
//! Wraps `aqueduct::io::Error` for transport failures and adds
//! protocol-level error categories so callers can react meaningfully
//! (e.g., reconnect on `DeviceLost`, retry on `ShaderResolveMissed`).

use std::io;

use thiserror::Error;

use aqueduct_gpu::ids::ResourceId;
use aqueduct_gpu::payloads::ShaderResolveStatus;

/// Result alias used throughout this crate.
pub type GpuClientResult<T> = Result<T, GpuClientError>;

/// All client-side error categories.
#[derive(Debug, Error)]
pub enum GpuClientError {
    /// Transport-level I/O failure (socket closed, EAGAIN with
    /// non-blocking enabled, etc.).
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// Postcard serialise/deserialise failure. Usually indicates a
    /// schema-version mismatch between client and host.
    #[error("postcard codec error: {0}")]
    Codec(#[from] postcard::Error),

    /// Frame-stream framing error during construction.
    #[error("frame codec error: {0}")]
    Frame(#[from] aqueduct_gpu::frame::FrameDecodeError),

    /// Received a message whose opcode_class is not `CLASS_GPU`. The
    /// caller is presumably sharing the aqueduct connection with
    /// another class and forgot to demultiplex.
    #[error("unexpected message class {0}; expected CLASS_GPU")]
    UnexpectedClass(u8),

    /// Received a GPU-class message whose op is not what we asked
    /// for (e.g., waiting for a handshake response, got a fence-signaled
    /// event mixed in). The unexpected message is dropped; callers
    /// who want event multiplexing should use [`recv_event`] explicitly.
    ///
    /// [`recv_event`]: super::GpuClient::recv_event
    #[error("unexpected GPU op {actual:#06x}; expected {expected:#06x}")]
    UnexpectedOp {
        /// The opcode we received.
        actual: u16,
        /// The opcode we wanted.
        expected: u16,
    },

    /// The connection's ID counter for the configured namespace was
    /// exhausted (2^28 = 268M IDs per namespace). In practice this is
    /// unreachable for a single connection's lifetime; surfaces only
    /// if there's a leak.
    #[error("ID namespace exhausted")]
    IdNamespaceExhausted,

    /// The host's handshake response declared a protocol version
    /// the client cannot speak.
    #[error("protocol version mismatch: client wants {client}, host offers {host}")]
    ProtocolMismatch {
        /// What we sent.
        client: u32,
        /// What the host sent back.
        host: u32,
    },

    /// `OP_GPU_SHADER_RESOLVE` returned `Miss`. Caller should follow up
    /// with `shader_upload(...)`. Carries the requested hash so the
    /// caller doesn't have to re-thread it.
    #[error("shader resolve miss for hash {hash:02x?}")]
    ShaderResolveMissed {
        /// The bytecode hash that missed.
        hash: [u8; 32],
    },

    /// `OP_GPU_SHADER_UPLOAD` failed sandbox validation or compile.
    /// The diagnostic explains why.
    #[error("shader upload rejected: {diagnostic}")]
    ShaderUploadRejected {
        /// Human-readable explanation from the host's validator.
        diagnostic: String,
    },

    /// Asynchronous validation error reported via
    /// `OP_GPU_VALIDATION_ERR`. Caller can choose to surface it as
    /// the closest Vulkan error code, or to abort the frame.
    #[error("validation error on op {opcode:#06x} (resource {resource_id:?}): {diagnostic}")]
    Validation {
        /// The op that failed validation.
        opcode: u16,
        /// Resource involved, if any.
        resource_id: Option<ResourceId>,
        /// Diagnostic.
        diagnostic: String,
    },

    /// The host reported `OP_GPU_DEVICE_LOST`. All resources on this
    /// connection are invalid; caller must reconnect.
    #[error("device lost: {diagnostic}")]
    DeviceLost {
        /// What the host said.
        diagnostic: String,
    },

    /// Bundle load failed because one or more declared resources did
    /// not validate. The aggregated diagnostic is in `messages`.
    #[error("bundle load failed: {} error(s)", messages.len())]
    BundleLoadFailed {
        /// Manifest hash.
        manifest_cas_hash: [u8; 32],
        /// Per-resource diagnostic messages collected from
        /// `OP_GPU_BUNDLE_LOAD_ERR` events.
        messages: Vec<String>,
    },
}

impl GpuClientError {
    /// Helper for translating a `ShaderResolveStatus::Miss` into the
    /// typed error, preserving the hash for the caller's follow-up.
    pub(crate) fn shader_miss(hash: [u8; 32], _status: ShaderResolveStatus) -> Self {
        GpuClientError::ShaderResolveMissed { hash }
    }
}
