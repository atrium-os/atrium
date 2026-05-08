//! portcullisd opcode dictionary for aqueduct.
//!
//! `opcode_class = CLASS_PORTCULLIS` (= 6 in `aqueduct::classes`)
//! identifies messages riding the portcullisd substrate. Within
//! that class:
//!
//! | op    | name           | direction       | purpose                          |
//! |-------|----------------|-----------------|----------------------------------|
//! | 0x01  | ATTACH_MOUNT   | client → server | request runtime AttachMount      |
//! | 0x02  | DETACH_MOUNT   | client → server | request runtime DetachMount      |
//! | 0x10  | MOUNT_REPLY    | server → client | result of attach/detach          |
//!
//! Payload encoding: serde_json (consistent with jaild + atrium-
//! volumes wire formats; portcullisd-the-process already pulls
//! serde_json in for those). The envelope-level length field
//! frames the payload, so no extra length-prefixing is needed
//! inside the payload.
//!
//! ## Capability mediation lives at the server
//!
//! A request landing on portcullisd is *authenticated* by the
//! caller's effective uid (recovered via `getpeereid`) and the
//! socket's mount-namespace location (only jails granted the
//! capability see the socket). portcullisd then runs the same
//! manifest-side capability check used by the operator CLI
//! `atrium-portcullisd-attach`. See `docs/spec/storage.md` §6.2.

use serde::{Deserialize, Serialize};

pub const OP_ATTACH_MOUNT: u16 = 0x01;
pub const OP_DETACH_MOUNT: u16 = 0x02;
pub const OP_MOUNT_REPLY:  u16 = 0x10;

/// Mount kinds. Mirror of `jaild::protocol::MountKind`. Kept as
/// a separate type so this crate doesn't pull in jaild.
#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MountKind {
    RoNullfs,
    RwNullfs,
    Tmpfs,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AttachMountReq {
    /// Jail name. Authoritative source-of-truth is the caller's
    /// peer credentials and the socket's mount-namespace; this
    /// field is informational + for routing on portcullisd's
    /// side. portcullisd may cross-check that the named jail
    /// matches the caller (V1).
    pub jail_name:  String,
    pub source:     String,
    /// In-jail dest path.
    pub dest:       String,
    pub mount_kind: MountKind,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DetachMountReq {
    pub jail_name: String,
    pub dest:      String,
    #[serde(default)]
    pub force:     bool,
}

/// Reply for both AttachMount and DetachMount. Discriminator
/// `kind` shape mirrors jaild's existing Response shape so
/// portcullisd can pass through jaild errors verbatim.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MountReply {
    Ok,
    /// portcullisd's manifest-side capability gate denied the
    /// request before it ever reached jaild.
    CapabilityDenied { rule: String, detail: String },
    /// jaild rejected the request (its policy file allow-list).
    JaildPolicyDenied { rule: String, detail: String },
    /// A syscall on jaild's side failed.
    JaildSyscallFailed { name: String, errno: i32, msg: String },
    /// Catch-all (RPC error, malformed request, etc.).
    Error { detail: String },
}
