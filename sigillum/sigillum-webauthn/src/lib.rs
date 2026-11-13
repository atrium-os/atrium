//! The WebAuthn client for Navigator's broker (sigillum.md §9.1).
//!
//! A page calls `navigator.credentials.create()` or `.get()`. The page's
//! script runtime turns the options into their JSON form (WebAuthn L3
//! `PublicKeyCredentialCreationOptionsJSON`, binary fields as base64url)
//! and hands them to the broker, which calls [`create`] or [`get`] with the
//! **origin it knows** (from the courier's TLS endpoint; pages and workers
//! never state it). Everything a page could lie about is decided here or
//! in `sigillumd`:
//!
//! - the RP ID: the origin's host or a registrable suffix of it, never a
//!   public suffix, only on a secure origin ([`origin`]);
//! - `clientDataJSON`, built here from trusted values only, and its hash,
//!   which is all `sigillumd` and the authenticator see ([`client_data`]);
//! - the reply's authenticator data is checked again (RP ID hash, user
//!   presence, user verification when required) before anything is
//!   packaged for the page ([`client`]).
//!
//! The page sees only WebAuthn's own errors ([`DomError`], §9.4); the
//! detail is on the sheet and in `sigillumd`'s audit line.

pub mod client;
pub mod client_data;
pub mod options;
pub mod origin;

pub use client::{create, get, Sigillumd, Transport};
pub use options::{CreationOptions, RequestOptions};
pub use origin::Origin;

/// The `DOMException` names a WebAuthn call can reject with (§9.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomError {
    /// The user cancelled or did not finish in time, an authenticator or
    /// policy refused, or anything else the page may not learn more about.
    NotAllowed,
    /// `create` with a credential the authenticator already holds.
    InvalidState,
    /// The RP ID is not this origin's to use, or the origin is not secure.
    Security,
    /// None of `pubKeyCredParams` is an algorithm we support (ES256).
    NotSupported,
    /// The options are malformed (WebAuthn's `TypeError`).
    Type,
}

impl DomError {
    /// The name the page sees.
    pub fn name(self) -> &'static str {
        match self {
            DomError::NotAllowed => "NotAllowedError",
            DomError::InvalidState => "InvalidStateError",
            DomError::Security => "SecurityError",
            DomError::NotSupported => "NotSupportedError",
            DomError::Type => "TypeError",
        }
    }
}

/// base64url without padding, as WebAuthn's JSON forms use.
pub fn b64url(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// Decode base64url; padding is tolerated, nothing else.
pub fn unb64url(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')).ok()
}
