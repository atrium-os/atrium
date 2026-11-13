//! Drivers <-> `sigillumd` (sigillum.md §14.2), over the socketpair
//! portcullisd made when it started the driver's jail. Every
//! device-derived field is capped; `sigillumd` decodes, then calls
//! [`FromDriver::check_caps`] before looking at anything.

#[allow(unused_imports)]
use alloc::{boxed::Box, string::String, vec::Vec};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::client::CeremonyId;

/// 64 KiB: a token's certificates (S5) come in one record.
pub const MAX_MESSAGE: usize = 64 * 1024;
/// Where drivers connect to `sigillumd`; only `_sigillum-dev` is served.
pub const SOCKET: &str = "/atrium/sockets/sigillum-dev/drivers.sock";

pub mod cap {
    pub const NAME: usize = 64;
    pub const CRED_ID: usize = 1023;
    pub const PUBLIC_KEY: usize = 133;
    pub const AUTH_DATA: usize = 4096;
    pub const ATT_OBJ: usize = 4096;
    pub const SIGNATURE: usize = 512;
    pub const USER_HANDLE: usize = 64;
    pub const LIST: usize = 64;
    /// A token certificate (DER), its key id and label, and how many one record carries.
    pub const CERT: usize = 4096;
    pub const KEY_ID: usize = 64;
    pub const LABEL: usize = 64;
    pub const CERTS: usize = 8;
    /// A digest to sign: SHA-512 at most.
    pub const DIGEST: usize = 64;
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

impl core::fmt::Debug for Pin {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Pin(..)")
    }
}

/// The digest's algorithm, for an RSA signature's `DigestInfo` or PSS parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HashAlg {
    Sha256,
    Sha384,
    Sha512,
}

impl HashAlg {
    pub fn len(self) -> usize {
        match self {
            HashAlg::Sha256 => 32,
            HashAlg::Sha384 => 48,
            HashAlg::Sha512 => 64,
        }
    }
}

/// How a token signs a digest Sigillum computed (sigillum.md §16.5): the only mechanisms
/// a token driver may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignScheme {
    /// RSASSA-PKCS1-v1_5 (`CKM_RSA_PKCS` over the `DigestInfo`).
    RsaPkcs1(HashAlg),
    /// RSASSA-PSS, salt as long as the digest (`CKM_RSA_PKCS_PSS`).
    RsaPss(HashAlg),
    /// ECDSA over the digest (`CKM_ECDSA`); the signature is `r || s`.
    Ecdsa(HashAlg),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KeyKind {
    Rsa { bits: u16 },
    Ec { bits: u16 },
    Other,
}

/// One certificate on a token, with the private key that goes with it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCert {
    /// `CKA_ID`, shared by the certificate and its key.
    pub key_id: Vec<u8>,
    pub label: String,
    /// The certificate, DER. sigillumd parses it, never the driver's word about it.
    pub cert: Vec<u8>,
    pub key: KeyKind,
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
    /// Token drivers (S5): the certificates and the keys that sign with them. No PIN: the
    /// public objects are read without logging in.
    ListCertificates {
        ceremony: CeremonyId,
    },
    /// Token drivers (S5): sign `digest` with the key `key_id` names. `pin` is required by
    /// most tokens; without it the driver answers `NeedPin`.
    SignDigest {
        ceremony: CeremonyId,
        key_id: Vec<u8>,
        scheme: SignScheme,
        digest: Vec<u8>,
        pin: Option<Pin>,
    },
}

impl ToDriver {
    pub fn ceremony(&self) -> CeremonyId {
        match self {
            ToDriver::Register { ceremony, .. }
            | ToDriver::Assert { ceremony, .. }
            | ToDriver::Cancel { ceremony }
            | ToDriver::ListCertificates { ceremony }
            | ToDriver::SignDigest { ceremony, .. } => *ceremony,
        }
    }
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
    /// The request is not one this driver serves (a FIDO key asked for certificates).
    Unsupported,
    /// The token has no key with that id, or it can't use the scheme asked for.
    NoKey,
}

/// `PinInvalid::retries` when the token doesn't say how many tries are left.
pub const RETRIES_UNKNOWN: u8 = u8::MAX;

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
    /// Token drivers (S5): the answer to `ListCertificates`.
    Certificates {
        ceremony: CeremonyId,
        token_label: String,
        certs: Vec<TokenCert>,
    },
    /// Token drivers (S5): the answer to `SignDigest`.
    Signed {
        ceremony: CeremonyId,
        signature: Vec<u8>,
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
            FromDriver::Certificates { token_label, certs, .. } => {
                within(token_label.as_bytes(), cap::LABEL, "token label")?;
                if certs.len() > cap::CERTS {
                    return Err("too many certificates");
                }
                for c in certs {
                    if c.key_id.is_empty() || c.cert.is_empty() {
                        return Err("empty key id or certificate");
                    }
                    within(&c.key_id, cap::KEY_ID, "key id")?;
                    within(c.label.as_bytes(), cap::LABEL, "label")?;
                    within(&c.cert, cap::CERT, "certificate")?;
                }
                Ok(())
            }
            FromDriver::Signed { signature, .. } => {
                if signature.is_empty() {
                    return Err("empty signature");
                }
                within(signature, cap::SIGNATURE, "signature")
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
            | FromDriver::Error { ceremony, .. }
            | FromDriver::Certificates { ceremony, .. }
            | FromDriver::Signed { ceremony, .. } => Some(*ceremony),
        }
    }
}
