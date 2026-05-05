//! CAS primitives shared across the scene-server.
//!
//! 32-byte SHA-256 hashes identify every CAS-stored blob (textures,
//! glyph bitmaps, fonts, etc.). The scene graph, slot table, and CAS
//! store all key on `Hash256`.
//!
//! This module previously held the legacy 128-byte `Command` /
//! `Completion` structs and their opcode constants — the wire protocol
//! that `CommandFrontend` dispatched. Both were excised at the M2.7e
//! cutover; the new envelope-based wire is defined in `fresco-protocol`
//! (CLASS_DISPLAY) and dispatched by `EnvelopeFrontend`.

pub type Hash256 = [u8; 32];
pub const NULL_HASH: Hash256 = [0u8; 32];
