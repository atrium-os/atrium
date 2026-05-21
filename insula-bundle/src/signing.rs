//! Bundle signing + verification.
//!
//! v0 wire format ("INSL" v1) — a single `signature`
//! file at the bundle root, layout:
//!
//! ```text
//!   bytes [0..4)    magic "INSL"
//!   byte  [4]       version = 1
//!   byte  [5]       key_id_len  (max 255)
//!   bytes [6..6+L)  key_id UTF-8
//!   bytes [..+32)   ed25519 pubkey (raw)
//!   bytes [..+64)   ed25519 signature (raw)
//! ```
//!
//! The signature input is
//! `SHA256(manifest.toml) || SHA256(<entry binary>)`.
//! Both files are covered — tampering with either
//! invalidates.
//!
//! Per-publisher trust: the verifier checks that the
//! signature's pubkey matches a pubkey the caller has
//! independently designated as trusted for the
//! signature's `key_id`. v0 doesn't include a key-
//! rotation chain; that's future work (cf.
//! `nomenclator.md` §6 for the rotation model the
//! eventual Insula publisher-manifest layer adopts).

use crate::InsulaBundle;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use sha2::{Digest, Sha256};
use std::path::Path;
use thiserror::Error;

/// Magic prefix in the on-disk signature file.
pub const MAGIC: &[u8; 4] = b"INSL";

/// Current signature-file version.
pub const VERSION: u8 = 1;

/// ed25519 public-key length.
pub const PUBKEY_LEN: usize = 32;

/// ed25519 signature length.
pub const SIG_LEN: usize = 64;

/// Parsed contents of a bundle's `signature` file.
#[derive(Debug, Clone)]
pub struct BundleSignature {
    /// Publisher key identifier. Maps the signature to
    /// a trusted-publisher entry on the verifier side.
    pub key_id: String,
    /// The pubkey the signer claims to have signed
    /// with. The verifier MUST cross-check this
    /// against its trusted-publisher store (a signer
    /// can put any pubkey here; the security comes
    /// from the verifier's trust decision).
    pub pubkey: [u8; PUBKEY_LEN],
    /// The signature bytes.
    pub signature: [u8; SIG_LEN],
}

/// Errors from signature parsing / verification.
#[derive(Debug, Error)]
pub enum SignatureError {
    #[error("signature file is missing or unreadable: {0}")]
    SignatureFileIo(#[from] std::io::Error),

    #[error("signature file is too short (got {got} bytes, need at least {need})")]
    TooShort { got: usize, need: usize },

    #[error("bad magic header (expected b\"INSL\")")]
    BadMagic,

    #[error("unsupported signature version: {0}")]
    UnsupportedVersion(u8),

    #[error("signature file length mismatch (expected {expected}, got {got})")]
    LengthMismatch { expected: usize, got: usize },

    #[error("key_id is not valid UTF-8")]
    BadKeyId,

    #[error("signature pubkey {sig_pubkey} does not match trusted pubkey {trusted_pubkey} for key_id {key_id}")]
    UntrustedPublisher {
        key_id: String,
        sig_pubkey: String,
        trusted_pubkey: String,
    },

    #[error("signature does not verify against bundle digest")]
    SignatureMismatch,

    #[error("trusted-publisher store has no entry for key_id {0}")]
    NoTrustedPublisherForKey(String),
}

/// Read + parse a bundle's `signature` file.
pub fn read_signature(bundle_root: &Path) -> Result<BundleSignature, SignatureError> {
    let bytes = std::fs::read(bundle_root.join("signature"))?;
    parse_signature_bytes(&bytes)
}

pub(crate) fn parse_signature_bytes(
    bytes: &[u8],
) -> Result<BundleSignature, SignatureError> {
    if bytes.len() < 6 {
        return Err(SignatureError::TooShort {
            got: bytes.len(),
            need: 6,
        });
    }
    if &bytes[..4] != MAGIC {
        return Err(SignatureError::BadMagic);
    }
    if bytes[4] != VERSION {
        return Err(SignatureError::UnsupportedVersion(bytes[4]));
    }
    let key_id_len = bytes[5] as usize;
    let expected_len = 6 + key_id_len + PUBKEY_LEN + SIG_LEN;
    if bytes.len() != expected_len {
        return Err(SignatureError::LengthMismatch {
            expected: expected_len,
            got: bytes.len(),
        });
    }
    let key_id = std::str::from_utf8(&bytes[6..6 + key_id_len])
        .map_err(|_| SignatureError::BadKeyId)?
        .to_string();
    let mut pubkey = [0u8; PUBKEY_LEN];
    pubkey.copy_from_slice(&bytes[6 + key_id_len..6 + key_id_len + PUBKEY_LEN]);
    let mut signature = [0u8; SIG_LEN];
    signature.copy_from_slice(
        &bytes[6 + key_id_len + PUBKEY_LEN..6 + key_id_len + PUBKEY_LEN + SIG_LEN],
    );
    Ok(BundleSignature {
        key_id,
        pubkey,
        signature,
    })
}

/// Serialize a [`BundleSignature`] to the v1 wire
/// format.
pub fn encode_signature(sig: &BundleSignature) -> Vec<u8> {
    assert!(
        sig.key_id.len() <= 255,
        "key_id too long for v1 (max 255 bytes UTF-8)"
    );
    let mut out =
        Vec::with_capacity(6 + sig.key_id.len() + PUBKEY_LEN + SIG_LEN);
    out.extend_from_slice(MAGIC);
    out.push(VERSION);
    out.push(sig.key_id.len() as u8);
    out.extend_from_slice(sig.key_id.as_bytes());
    out.extend_from_slice(&sig.pubkey);
    out.extend_from_slice(&sig.signature);
    out
}

/// Compute the digest over which the signature applies.
/// `SHA256(manifest.toml) || SHA256(entry-binary)`.
pub fn compute_digest(bundle: &InsulaBundle) -> Result<[u8; 64], SignatureError> {
    let manifest_bytes = std::fs::read(bundle.root.join("manifest.toml"))?;
    let mut h = Sha256::new();
    h.update(&manifest_bytes);
    let manifest_hash: [u8; 32] = h.finalize().into();

    let binary_bytes = std::fs::read(bundle.binary_path())?;
    let mut h = Sha256::new();
    h.update(&binary_bytes);
    let binary_hash: [u8; 32] = h.finalize().into();

    let mut digest = [0u8; 64];
    digest[..32].copy_from_slice(&manifest_hash);
    digest[32..].copy_from_slice(&binary_hash);
    Ok(digest)
}

/// Sign a bundle's digest with the given signing key.
pub fn sign_bundle(
    bundle: &InsulaBundle,
    key_id: &str,
    sk: &SigningKey,
) -> Result<BundleSignature, SignatureError> {
    let digest = compute_digest(bundle)?;
    let signature: Signature = sk.sign(&digest);
    Ok(BundleSignature {
        key_id: key_id.to_string(),
        pubkey: sk.verifying_key().to_bytes(),
        signature: signature.to_bytes(),
    })
}

/// Write the signature to `<bundle_root>/signature`.
pub fn write_signature_to_bundle(
    bundle_root: &Path,
    sig: &BundleSignature,
) -> Result<(), SignatureError> {
    let bytes = encode_signature(sig);
    std::fs::write(bundle_root.join("signature"), &bytes)?;
    Ok(())
}

/// Verify the signature inside a bundle against a
/// trusted publisher key. Returns Ok if (a) the
/// signature is well-formed, (b) the signature's
/// embedded pubkey matches the trusted pubkey for the
/// claimed key_id, and (c) the signature verifies
/// over the computed digest of the bundle.
pub fn verify_bundle(
    bundle: &InsulaBundle,
    trusted_pubkey: &VerifyingKey,
) -> Result<(), SignatureError> {
    let sig = read_signature(&bundle.root)?;
    let trusted_bytes = trusted_pubkey.to_bytes();
    if sig.pubkey != trusted_bytes {
        return Err(SignatureError::UntrustedPublisher {
            key_id: sig.key_id.clone(),
            sig_pubkey: hex(&sig.pubkey),
            trusted_pubkey: hex(&trusted_bytes),
        });
    }
    let digest = compute_digest(bundle)?;
    let signature = Signature::from_bytes(&sig.signature);
    trusted_pubkey
        .verify(&digest, &signature)
        .map_err(|_| SignatureError::SignatureMismatch)
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = std::fmt::Write::write_fmt(&mut s, format_args!("{:02x}", b));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;
    use rand::rngs::OsRng;
    use std::fs;

    fn make_bundle(root: &Path) -> InsulaBundle {
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::write(
            root.join("manifest.toml"),
            r#"
[app]
name = "com.example.x"
version = "0.1.0"
sdk-version = "1.x"

[bundle]
form = "native"
arches = ["aarch64-darwin"]
entry = "bin/x"
"#,
        )
        .unwrap();
        fs::write(root.join("bin/x"), b"the binary bytes").unwrap();
        InsulaBundle::read(root).unwrap()
    }

    #[test]
    fn sign_and_verify_roundtrip() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = make_bundle(tmp.path());
        let sk = SigningKey::generate(&mut OsRng);
        let pk = sk.verifying_key();

        let sig = sign_bundle(&bundle, "publisher-1", &sk).unwrap();
        write_signature_to_bundle(tmp.path(), &sig).unwrap();

        verify_bundle(&bundle, &pk).expect("signed bundle should verify");
    }

    #[test]
    fn verify_rejects_tampered_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = make_bundle(tmp.path());
        let sk = SigningKey::generate(&mut OsRng);
        let pk = sk.verifying_key();

        let sig = sign_bundle(&bundle, "p1", &sk).unwrap();
        write_signature_to_bundle(tmp.path(), &sig).unwrap();

        // Tamper with the binary.
        fs::write(tmp.path().join("bin/x"), b"different binary").unwrap();

        let r = verify_bundle(&bundle, &pk);
        assert!(matches!(r, Err(SignatureError::SignatureMismatch)),
                "tampered binary should fail signature verify, got {:?}", r);
    }

    #[test]
    fn verify_rejects_tampered_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = make_bundle(tmp.path());
        let sk = SigningKey::generate(&mut OsRng);
        let pk = sk.verifying_key();

        let sig = sign_bundle(&bundle, "p1", &sk).unwrap();
        write_signature_to_bundle(tmp.path(), &sig).unwrap();

        // Tamper with the manifest.
        let mut manifest = fs::read_to_string(tmp.path().join("manifest.toml")).unwrap();
        manifest.push_str("\n# malicious comment\n");
        fs::write(tmp.path().join("manifest.toml"), manifest).unwrap();

        let r = verify_bundle(&bundle, &pk);
        assert!(matches!(r, Err(SignatureError::SignatureMismatch)));
    }

    #[test]
    fn verify_rejects_untrusted_publisher() {
        let tmp = tempfile::tempdir().unwrap();
        let bundle = make_bundle(tmp.path());
        let sk_legit = SigningKey::generate(&mut OsRng);
        let sk_attacker = SigningKey::generate(&mut OsRng);

        // Attacker signs with a key the verifier doesn't trust.
        let sig = sign_bundle(&bundle, "publisher-1", &sk_attacker).unwrap();
        write_signature_to_bundle(tmp.path(), &sig).unwrap();

        // Verifier trusts a DIFFERENT pubkey for that key_id.
        let r = verify_bundle(&bundle, &sk_legit.verifying_key());
        assert!(matches!(r, Err(SignatureError::UntrustedPublisher { .. })));
    }

    #[test]
    fn parse_rejects_bad_magic() {
        let bad = b"NOPE\x01\x00..............................................................";
        let r = parse_signature_bytes(bad);
        assert!(matches!(r, Err(SignatureError::BadMagic)));
    }

    #[test]
    fn parse_rejects_unsupported_version() {
        let mut bytes = vec![];
        bytes.extend_from_slice(MAGIC);
        bytes.push(99); // version
        bytes.push(0); // key_id_len
        bytes.extend_from_slice(&[0u8; PUBKEY_LEN]);
        bytes.extend_from_slice(&[0u8; SIG_LEN]);
        let r = parse_signature_bytes(&bytes);
        assert!(matches!(r, Err(SignatureError::UnsupportedVersion(99))));
    }

    #[test]
    fn encode_decode_roundtrip() {
        let sig = BundleSignature {
            key_id: "publisher-with-a-long-name".to_string(),
            pubkey: [7u8; PUBKEY_LEN],
            signature: [9u8; SIG_LEN],
        };
        let bytes = encode_signature(&sig);
        let decoded = parse_signature_bytes(&bytes).unwrap();
        assert_eq!(decoded.key_id, sig.key_id);
        assert_eq!(decoded.pubkey, sig.pubkey);
        assert_eq!(decoded.signature, sig.signature);
    }
}
