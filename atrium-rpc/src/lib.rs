//! atrium-rpc — unified IPC substrate for Atrium services.
//!
//! See `docs/spec/atrium-rpc.md` for the full design. In short:
//!
//! - One versioned envelope across every service.
//! - Built-in CAS (content-addressed) layer so the same blob
//!   referenced from multiple channels is one allocation.
//! - Same SHA-256 hash space as Tessera; trusted services with the
//!   `tessera-cas-read` capability can answer hash lookups without
//!   going through the wire.
//! - fd-passed shm rendezvous for high-bandwidth payloads.
//! - Capability boundary is the filesystem (Portcullis nullfs of
//!   per-service sockets at `/atrium/sockets/`).
//!
//! Hashes are **advisory pointers**, not capability tokens — a
//! receiver only learns content X if a sender explicitly served it.
//! Verify-on-use is built in.

pub mod cas;
pub mod classes;
pub mod connection;
pub mod envelope;

pub use cas::{Hash, HASH_LEN};
pub use classes::*;
pub use connection::{Connection, Message, MessageKind, DEFAULT_CACHE_BYTES};
pub use envelope::{flag, Header, ENVELOPE_VERSION, HEADER_LEN, MAX_PAYLOAD};
