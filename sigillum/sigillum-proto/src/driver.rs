//! Drivers <-> `sigillumd` (sigillum.md §14.2), over the socketpair
//! portcullisd made when it started the driver's jail. Every
//! device-derived field is capped; `sigillumd` decodes, then calls
//! [`FromDriver::check_caps`] before looking at anything.

use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::client::CeremonyId;

pub const MAX_MESSAGE: usize = 8 * 1024;

pub mod cap {
    pub const NAME: usize = 64;
    pub const CRED_ID: usize = 1023;
    pub const PUBLIC_KEY: usize = 133;
    pub const AUTH_DATA: usize = 4096;
    pub const ATT_OBJ: usize = 4096;
    pub const SIGNATURE: usize = 512;
    pub const USER_HANDLE: usize = 64;
    pub const LIST: usize = 64;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceClass {
    Fido,
    Ccid,
    Fingerprint,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FidoCaps {
    pub ctap2: bool,
    pub resident_keys: bool,
    pub client_pin_set: bool,
    pub builtin_uv: bool,
    pub cred_protect: bool,
    pub hmac_secret: bool,
}

/// A PIN on its way from `sigillum-ui` to a driver. Zeroized on drop.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pin(pub Zeroizing<Vec<u8>>);

impl std::fmt::Debug for Pin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Pin(..)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToDriver {
    Register {
        ceremony: CeremonyId,
        rp_id: String,
        rp_name: String,
        client_data_hash: [u8; 32],
        user_id: Vec<u8>,
        user_name: String,
        user_display_name: String,
        exclude: Vec<Vec<u8>>,
        resident_key: bool,
        uv_required: bool,
        pin: Option<Pin>,
    },
    Assert {
        ceremony: CeremonyId,
        rp_id: String,
        client_data_hash: [u8; 32],
        allow: Vec<Vec<u8>>,
        uv_required: bool,
        pin: Option<Pin>,
    },
    Cancel {
        ceremony: CeremonyId,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DriverError {
    /// The user didn't touch, or refused on the key.
    Denied,
    Excluded,
    NoCredentials,
    PinInvalid { retries: u8 },
    PinBlocked,
    /// UV was required and the key has neither a PIN nor built-in UV.
    UvUnavailable,
    /// The device broke protocol or answered something we refused.
    Device,
    Gone,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FromDriver {
    Hello {
        class: DeviceClass,
        vendor: u16,
        product: u16,
        name: String,
        fido: Option<FidoCaps>,
    },
    Registered {
        ceremony: CeremonyId,
        cred_id: Vec<u8>,
        rp_id_hash: [u8; 32],
        flags: u8,
        sign_count: u32,
        cose_alg: i64,
        /// SEC1 uncompressed point for ES256.
        public_key: Vec<u8>,
        /// Opaque to sigillumd except its first 37 bytes.
        auth_data: Vec<u8>,
        /// Opaque to sigillumd.
        att_obj: Vec<u8>,
    },
    Asserted {
        ceremony: CeremonyId,
        cred_id: Vec<u8>,
        rp_id_hash: [u8; 32],
        flags: u8,
        sign_count: u32,
        auth_data: Vec<u8>,
        signature: Vec<u8>,
        user_handle: Option<Vec<u8>>,
    },
    NeedPin {
        ceremony: CeremonyId,
        retries: u8,
    },
    NeedTouch {
        ceremony: CeremonyId,
    },
    Error {
        ceremony: CeremonyId,
        error: DriverError,
    },
}

impl FromDriver {
    /// Enforce §14.2's caps. A record that fails is a device fault.
    pub fn check_caps(&self) -> Result<(), &'static str> {
        let within = |b: &[u8], max: usize, what| if b.len() > max { Err(what) } else { Ok(()) };
        match self {
            FromDriver::Hello { name, .. } => within(name.as_bytes(), cap::NAME, "name"),
            FromDriver::Registered { cred_id, public_key, auth_data, att_obj, .. } => {
                if cred_id.is_empty() {
                    return Err("empty credential id");
                }
                within(cred_id, cap::CRED_ID, "credential id")?;
                within(public_key, cap::PUBLIC_KEY, "public key")?;
                within(auth_data, cap::AUTH_DATA, "authenticator data")?;
                within(att_obj, cap::ATT_OBJ, "attestation object")
            }
            FromDriver::Asserted { cred_id, auth_data, signature, user_handle, .. } => {
                if cred_id.is_empty() {
                    return Err("empty credential id");
                }
                within(cred_id, cap::CRED_ID, "credential id")?;
                within(auth_data, cap::AUTH_DATA, "authenticator data")?;
                within(signature, cap::SIGNATURE, "signature")?;
                within(user_handle.as_deref().unwrap_or(&[]), cap::USER_HANDLE, "user handle")
            }
            _ => Ok(()),
        }
    }

    pub fn ceremony(&self) -> Option<CeremonyId> {
        match self {
            FromDriver::Hello { .. } => None,
            FromDriver::Registered { ceremony, .. }
            | FromDriver::Asserted { ceremony, .. }
            | FromDriver::NeedPin { ceremony, .. }
            | FromDriver::NeedTouch { ceremony }
            | FromDriver::Error { ceremony, .. } => Some(*ceremony),
        }
    }
}
