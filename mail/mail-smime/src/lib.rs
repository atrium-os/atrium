//! S/MIME (RFC 8551) for Mail, mail.md §10.1. What is here holds no
//! private key: a signature is made by a callback (the token through
//! Sigillum, or a key Arca holds), and a content key is unwrapped the same
//! way. Signatures and chains are checked with ring against roots the
//! caller hands over (the system trust store and the person's added CAs).

pub mod ber;
pub mod cert;
pub mod envelope;
#[cfg(feature = "legacy-receive-prototype")]
pub mod legacy_wire;
pub mod sign;
pub mod verify;

pub use cert::CertInfo;

/// Object identifiers used here.
pub mod oid {
    use der::asn1::ObjectIdentifier as O;
    pub const DATA: O = O::new_unwrap("1.2.840.113549.1.7.1");
    pub const SIGNED_DATA: O = O::new_unwrap("1.2.840.113549.1.7.2");
    pub const ENVELOPED_DATA: O = O::new_unwrap("1.2.840.113549.1.7.3");
    pub const CONTENT_TYPE: O = O::new_unwrap("1.2.840.113549.1.9.3");
    pub const MESSAGE_DIGEST: O = O::new_unwrap("1.2.840.113549.1.9.4");
    pub const SIGNING_TIME: O = O::new_unwrap("1.2.840.113549.1.9.5");
    pub const EMAIL: O = O::new_unwrap("1.2.840.113549.1.9.1");
    pub const SHA1: O = O::new_unwrap("1.3.14.3.2.26");
    pub const SHA256: O = O::new_unwrap("2.16.840.1.101.3.4.2.1");
    pub const SHA384: O = O::new_unwrap("2.16.840.1.101.3.4.2.2");
    pub const SHA512: O = O::new_unwrap("2.16.840.1.101.3.4.2.3");
    pub const RSA: O = O::new_unwrap("1.2.840.113549.1.1.1");
    pub const RSA_OAEP: O = O::new_unwrap("1.2.840.113549.1.1.7");
    pub const MGF1: O = O::new_unwrap("1.2.840.113549.1.1.8");
    pub const PSPECIFIED: O = O::new_unwrap("1.2.840.113549.1.1.9");
    pub const RSA_PSS: O = O::new_unwrap("1.2.840.113549.1.1.10");
    pub const RSA_SHA256: O = O::new_unwrap("1.2.840.113549.1.1.11");
    pub const RSA_SHA384: O = O::new_unwrap("1.2.840.113549.1.1.12");
    pub const RSA_SHA512: O = O::new_unwrap("1.2.840.113549.1.1.13");
    pub const EC_PUBLIC_KEY: O = O::new_unwrap("1.2.840.10045.2.1");
    pub const ECDSA_SHA256: O = O::new_unwrap("1.2.840.10045.4.3.2");
    pub const ECDSA_SHA384: O = O::new_unwrap("1.2.840.10045.4.3.3");
    pub const ECDSA_SHA512: O = O::new_unwrap("1.2.840.10045.4.3.4");
    pub const P256: O = O::new_unwrap("1.2.840.10045.3.1.7");
    pub const P384: O = O::new_unwrap("1.3.132.0.34");
    pub const AES256_CBC: O = O::new_unwrap("2.16.840.1.101.3.4.1.42");
    pub const AES128_CBC: O = O::new_unwrap("2.16.840.1.101.3.4.1.2");
    pub const SAN: O = O::new_unwrap("2.5.29.17");
    pub const BASIC_CONSTRAINTS: O = O::new_unwrap("2.5.29.19");
    pub const CN: O = O::new_unwrap("2.5.4.3");
    pub const O_: O = O::new_unwrap("2.5.4.10");
}

/// Most bytes of a signature or envelope taken.
pub const MAX_CMS: usize = 4 << 20;
/// Most certificates looked at in one signature.
pub const MAX_CERTS: usize = 32;
