//! Clients <-> `sigillumd` (sigillum.md §14.1), on
//! `/atrium/sockets/sigillum/sigillumd.sock`. Who the client is comes from
//! the peer uid, never from a message.

#[allow(unused_imports)]
use alloc::{boxed::Box, string::String, vec::Vec};
use serde::{Deserialize, Serialize};

pub use crate::driver::{HashAlg, SignScheme};

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
    /// The site's `timeout` (WebAuthn `options.timeout`), in ms; 0 when the
    /// site gave none. sigillumd keeps it within 30 s to 10 min.
    pub timeout_ms: u32,
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
    /// As [`Register::timeout_ms`].
    pub timeout_ms: u32,
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

/// What a certificate on a token is used for (sigillum.md §10): shown on
/// the sheet. TLS client authentication is the one use so far; document
/// signing for apps (§10.2) follows with `identity.certificate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CertPurpose {
    /// Client authentication to this server (§10.1).
    TlsClient { host: String, port: u16 },
}

/// §10.1 step 3: the user picks a certificate from the attached tokens, for
/// a server that asked for one. Only certificates whose key can sign with
/// one of `schemes` are offered. Nothing is signed and no PIN is asked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChooseCertificate {
    pub ceremony: CeremonyId,
    /// As [`Register::timeout_ms`].
    pub timeout_ms: u32,
    pub purpose: CertPurpose,
    pub schemes: Vec<SignScheme>,
}

/// §10.1 step 4: sign `digest` (the TLS `CertificateVerify` content's
/// digest) with the key of the certificate whose SHA-256 is `cert_sha256`,
/// after the user confirms on the sheet and types the token's PIN.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignWithCertificate {
    pub ceremony: CeremonyId,
    pub timeout_ms: u32,
    pub purpose: CertPurpose,
    pub cert_sha256: [u8; 32],
    pub scheme: SignScheme,
    pub digest: Vec<u8>,
}

/// Most schemes a [`ChooseCertificate`] names.
pub const MAX_SCHEMES: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    Register(Register),
    Assert(Assert),
    VerifyUser(VerifyUser),
    ChooseCertificate(ChooseCertificate),
    SignWithCertificate(SignWithCertificate),
    Cancel { ceremony: CeremonyId },
}

impl Request {
    pub fn ceremony(&self) -> CeremonyId {
        match self {
            Request::Register(r) => r.ceremony,
            Request::Assert(a) => a.ceremony,
            Request::VerifyUser(v) => v.ceremony,
            Request::ChooseCertificate(c) => c.ceremony,
            Request::SignWithCertificate(s) => s.ceremony,
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
    /// The certificate the user picked (DER), as the token holds it.
    CertificateChosen {
        ceremony: CeremonyId,
        cert: Vec<u8>,
    },
    /// The token's signature, checked by sigillumd against the
    /// certificate's key. ECDSA is `r || s`.
    CertificateSigned {
        ceremony: CeremonyId,
        signature: Vec<u8>,
    },
    Refused {
        ceremony: CeremonyId,
        reason: Refusal,
    },
}
