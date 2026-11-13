//! Clients <-> `sigillumd` (sigillum.md §14.1), on
//! `/atrium/sockets/sigillum/sigillumd.sock`. Who the client is comes from
//! the peer uid, never from a message.

use serde::{Deserialize, Serialize};

pub const MAX_MESSAGE: usize = 16 * 1024;
pub const SOCKET: &str = "/atrium/sockets/sigillum/sigillumd.sock";

/// Chosen by the client, unique on its connection; used to cancel.
pub type CeremonyId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Uv {
    Required,
    Preferred,
    Discouraged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Attestation {
    /// `fmt: "none"`, AAGUID zeroed (the default, §9.3).
    None,
    /// The authenticator's own statement, only after the user's yes on the
    /// sheet. Not built in S1: answered as `None`.
    Direct,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Register {
    pub ceremony: CeremonyId,
    pub rp_id: String,
    pub rp_name: String,
    pub client_data_hash: [u8; 32],
    pub user_id: Vec<u8>,
    pub user_name: String,
    pub user_display_name: String,
    pub exclude: Vec<Vec<u8>>,
    pub resident_key: bool,
    pub uv: Uv,
    pub attestation: Attestation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Assert {
    pub ceremony: CeremonyId,
    pub rp_id: String,
    pub client_data_hash: [u8; 32],
    pub allow: Vec<Vec<u8>>,
    pub uv: Uv,
}

/// What a verify-each-use or step-up check is for. Only fixed purposes;
/// arcad adds the item's own description from its store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Purpose {
    /// arcad: use of a verify-each-use item, with the item's description.
    ArcaItem { description: String },
    /// ostiarius: unlock the lock screen.
    Unlock,
    /// A privileged system change.
    Admin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Strength {
    BiometricPlain = 1,
    BiometricSecure = 2,
    KeyUv = 3,
    Passcode = 4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Method {
    Passcode,
    SecurityKey,
    Fingerprint,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyUser {
    pub ceremony: CeremonyId,
    pub user: String,
    pub purpose: Purpose,
    pub min_strength: Strength,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    Register(Register),
    Assert(Assert),
    VerifyUser(VerifyUser),
    Cancel { ceremony: CeremonyId },
}

impl Request {
    pub fn ceremony(&self) -> CeremonyId {
        match self {
            Request::Register(r) => r.ceremony,
            Request::Assert(a) => a.ceremony,
            Request::VerifyUser(v) => v.ceremony,
            Request::Cancel { ceremony } => *ceremony,
        }
    }
}

/// Which kind of authenticator answered (for the RP's `authenticatorAttachment`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthenticatorClass {
    /// Arca's platform authenticator.
    Platform,
    /// A roaming key over USB.
    Usb,
    Ble,
    Nfc,
    Hybrid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Refusal {
    /// Not something this client may ask.
    Denied,
    /// The user cancelled, the ceremony timed out, or an authenticator
    /// refused. The page gets `NotAllowedError` (§9.4).
    NotAllowed,
    /// `create` with a credential the authenticator already holds.
    Excluded,
    UnknownPeer,
    Busy,
    Malformed,
    /// The user's session isn't unlocked.
    Locked,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reply {
    Registered {
        ceremony: CeremonyId,
        cred_id: Vec<u8>,
        auth_data: Vec<u8>,
        att_obj: Vec<u8>,
        authenticator: AuthenticatorClass,
    },
    Asserted {
        ceremony: CeremonyId,
        cred_id: Vec<u8>,
        auth_data: Vec<u8>,
        signature: Vec<u8>,
        user_handle: Option<Vec<u8>>,
        authenticator: AuthenticatorClass,
    },
    Verified {
        ceremony: CeremonyId,
        method: Method,
        strength: Strength,
    },
    Refused {
        ceremony: CeremonyId,
        reason: Refusal,
    },
}
