//! The CTAP side of `sigillum-fido` (sigillum.md §6.1), with no I/O.
//!
//! - [`hid`]: CTAPHID framing (64-byte reports, channels, continuation
//!   packets, keepalive, errors).
//! - [`cmd`]: CTAP 2.1 requests a platform sends and the responses it
//!   parses: `GetInfo`, `MakeCredential`, `GetAssertion`, `ClientPIN`.
//! - [`authdata`] and [`cose`]: authenticator data and credential public
//!   keys, checked (the point is on the curve, lengths are in range).
//! - [`pin`]: PIN/UV auth protocol 2.
//! - [`u2f`]: CTAP1/U2F for keys without CTAP2, mapped onto WebAuthn.
//! - [`client`]: a blocking client over any [`hid::HidDevice`].
//!
//! Everything here parses bytes a device chose. It runs in the driver jail,
//! never in `sigillumd`.
#![forbid(unsafe_code)]

pub mod authdata;
pub mod client;
pub mod cmd;
pub mod cose;
pub mod hid;
pub mod pin;
pub mod u2f;

use std::fmt;

/// Every way a CTAP exchange fails, from the platform's side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The device sent something malformed or out of protocol.
    Protocol(&'static str),
    Cbor(sigillum_cbor::Error),
    /// The authenticator answered with a CTAP2 status other than success.
    Status(u8),
    /// A CTAPHID `ERROR` response.
    Hid(u8),
    Timeout,
    /// The transport failed (device gone, read error).
    Io(String),
    /// Our own request was invalid (too large, bad parameter).
    Request(&'static str),
    /// The broker cancelled the ceremony (`CTAPHID_CANCEL` was sent).
    Cancelled,
    /// The authenticator has no CTAP2 (`CBOR` capability); use [`u2f`].
    NoCtap2,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for Error {}

impl From<sigillum_cbor::Error> for Error {
    fn from(e: sigillum_cbor::Error) -> Error {
        Error::Cbor(e)
    }
}

/// CTAP2 status codes we act on (CTAP 2.1 §8.2).
pub mod status {
    pub const OK: u8 = 0x00;
    pub const INVALID_COMMAND: u8 = 0x01;
    pub const INVALID_PARAMETER: u8 = 0x02;
    pub const INVALID_LENGTH: u8 = 0x03;
    pub const CBOR_UNEXPECTED_TYPE: u8 = 0x11;
    pub const INVALID_CBOR: u8 = 0x12;
    pub const MISSING_PARAMETER: u8 = 0x14;
    pub const CREDENTIAL_EXCLUDED: u8 = 0x19;
    pub const UNSUPPORTED_ALGORITHM: u8 = 0x26;
    pub const OPERATION_DENIED: u8 = 0x27;
    pub const KEY_STORE_FULL: u8 = 0x28;
    pub const UNSUPPORTED_OPTION: u8 = 0x2B;
    pub const KEEPALIVE_CANCEL: u8 = 0x2D;
    pub const NO_CREDENTIALS: u8 = 0x2E;
    pub const USER_ACTION_TIMEOUT: u8 = 0x2F;
    pub const NOT_ALLOWED: u8 = 0x30;
    pub const PIN_INVALID: u8 = 0x31;
    pub const PIN_BLOCKED: u8 = 0x32;
    pub const PIN_AUTH_INVALID: u8 = 0x33;
    pub const PIN_AUTH_BLOCKED: u8 = 0x34;
    pub const PIN_NOT_SET: u8 = 0x35;
    pub const PUAT_REQUIRED: u8 = 0x36;
    pub const PIN_POLICY_VIOLATION: u8 = 0x37;
    pub const UP_REQUIRED: u8 = 0x3B;
}
