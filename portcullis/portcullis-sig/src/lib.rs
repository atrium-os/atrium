//! Manifest signature verification — the **trust root** under Atrium's capability
//! chain (`docs/spec/portcullis.md`; Sigstore manifest trust).
//!
//! Keyed Sigstore (Option A): the manifest blob is signed `cosign sign-blob`
//! style — ECDSA over the NIST P-256 curve, SHA-256 prehash — and Portcullis
//! verifies it against **pinned trusted-publisher public keys**. So a manifest's
//! declared capabilities are honoured *only* if a trusted publisher signed them;
//! an unsigned or tampered manifest is refused before the app is ever launched.
//! That roots the rest of the chain: trusted manifest → Portcullis grants caps →
//! launches at a dedicated uid → uid→app registry → services enforce.
//!
//! Keyed (pinned-key) verification needs no network, no account, and works fully
//! offline — the right first step. Keyless (Fulcio identity + Rekor transparency
//! log) layers on later behind the same verify gate. RustCrypto deps only
//! (permissive, no C, cross-compiles with build-std).

use base64::Engine;
use p256::ecdsa::{signature::Verifier, Signature, VerifyingKey};
use p256::pkcs8::DecodePublicKey;

/// Why a manifest was rejected (for an auditable refusal).
#[derive(Debug, PartialEq)]
pub enum SigError {
    /// No trusted key produced a valid signature.
    Untrusted,
    /// The signature bytes weren't a valid ECDSA/DER signature.
    BadSignature,
    /// A trusted-key PEM didn't parse.
    BadKey,
    /// The base64 signature didn't decode.
    BadEncoding,
}

/// Verify `manifest` against one publisher key. `sig_der` is the raw DER ECDSA
/// signature (what `openssl dgst -sha256 -sign` and cosign produce); `pubkey_pem`
/// is the SPKI public key (`-----BEGIN PUBLIC KEY-----`). The P-256 verifier
/// applies the SHA-256 prehash, so this is `cosign sign-blob`-compatible.
pub fn verify_one(manifest: &[u8], sig_der: &[u8], pubkey_pem: &str) -> Result<(), SigError> {
    let vk = VerifyingKey::from_public_key_pem(pubkey_pem).map_err(|_| SigError::BadKey)?;
    let sig = Signature::from_der(sig_der).map_err(|_| SigError::BadSignature)?;
    vk.verify(manifest, &sig).map_err(|_| SigError::Untrusted)
}

/// Verify against the **trusted-publisher set** (Atrium policy): accept if *any*
/// pinned key validates the signature. This is the gate Portcullis calls.
pub fn verify_trusted(
    manifest: &[u8],
    sig_der: &[u8],
    trusted_keys_pem: &[String],
) -> Result<(), SigError> {
    let mut last = SigError::Untrusted;
    for pem in trusted_keys_pem {
        match verify_one(manifest, sig_der, pem) {
            Ok(()) => return Ok(()),
            Err(e) => last = e,
        }
    }
    Err(if trusted_keys_pem.is_empty() { SigError::Untrusted } else { last })
}

/// As [`verify_trusted`], and say WHICH publisher signed: the SHA-256 of the
/// matching key's SPKI DER. That 32-byte publisher id is half of an app's
/// identity (with its app id): services such as Arca bind secrets to it, so
/// an app id re-installed under another publisher's key is another app.
pub fn verify_trusted_publisher(
    manifest: &[u8],
    sig_der: &[u8],
    trusted_keys_pem: &[String],
) -> Result<[u8; 32], SigError> {
    let mut last = SigError::Untrusted;
    for pem in trusted_keys_pem {
        match verify_one(manifest, sig_der, pem) {
            Ok(()) => return publisher_id(pem),
            Err(e) => last = e,
        }
    }
    Err(if trusted_keys_pem.is_empty() { SigError::Untrusted } else { last })
}

/// The publisher id of a PEM public key: SHA-256 over its SPKI DER, so the
/// same key always has the same id however its PEM is wrapped.
pub fn publisher_id(pubkey_pem: &str) -> Result<[u8; 32], SigError> {
    use p256::pkcs8::EncodePublicKey;
    use sha2::Digest;
    let vk = VerifyingKey::from_public_key_pem(pubkey_pem).map_err(|_| SigError::BadKey)?;
    let der = vk.to_public_key_der().map_err(|_| SigError::BadKey)?;
    Ok(sha2::Sha256::digest(der.as_bytes()).into())
}

/// Decode a base64 signature (cosign's on-disk form) to DER bytes.
pub fn sig_from_base64(s: &str) -> Result<Vec<u8>, SigError> {
    base64::engine::general_purpose::STANDARD
        .decode(s.trim())
        .map_err(|_| SigError::BadEncoding)
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::{signature::Signer, SigningKey};
    use p256::pkcs8::EncodePublicKey;

    // a deterministic test keypair (fixed scalar — test only).
    fn keypair() -> (SigningKey, String) {
        let sk = SigningKey::from_bytes(&[7u8; 32].into()).unwrap();
        let pem = sk
            .verifying_key()
            .to_public_key_pem(Default::default())
            .unwrap();
        (sk, pem)
    }

    #[test]
    fn a_signed_manifest_verifies() {
        let (sk, pem) = keypair();
        let manifest = b"[app]\nid = \"org.atrium.recorder\"\n";
        let sig: Signature = sk.sign(manifest);
        assert_eq!(verify_trusted(manifest, sig.to_der().as_bytes(), &[pem]), Ok(()));
    }

    #[test]
    fn the_signing_publisher_is_named() {
        let (sk, pem) = keypair();
        let other = SigningKey::from_bytes(&[9u8; 32].into()).unwrap();
        let other_pem = other.verifying_key().to_public_key_pem(Default::default()).unwrap();
        let manifest = b"[app]\nid = \"org.atrium.mail\"\n";
        let sig: Signature = sk.sign(manifest);
        let id = verify_trusted_publisher(manifest, sig.to_der().as_bytes(), &[other_pem.clone(), pem.clone()]).unwrap();
        assert_eq!(id, publisher_id(&pem).unwrap());
        assert_ne!(id, publisher_id(&other_pem).unwrap());
        // The same signature under only the other key: untrusted, no id.
        assert!(verify_trusted_publisher(manifest, sig.to_der().as_bytes(), &[other_pem]).is_err());
    }

    #[test]
    fn a_tampered_manifest_is_refused() {
        let (sk, pem) = keypair();
        let sig: Signature = sk.sign(b"original manifest");
        // the file changed after signing (e.g. caps escalated).
        assert_eq!(
            verify_trusted(b"TAMPERED manifest", sig.to_der().as_bytes(), &[pem]),
            Err(SigError::Untrusted)
        );
    }

    #[test]
    fn an_untrusted_publisher_is_refused() {
        let (sk, _good) = keypair();
        // signed by `sk`, but we only trust a DIFFERENT key.
        let other = SigningKey::from_bytes(&[9u8; 32].into()).unwrap();
        let other_pem = other.verifying_key().to_public_key_pem(Default::default()).unwrap();
        let manifest = b"manifest";
        let sig: Signature = sk.sign(manifest);
        assert_eq!(
            verify_trusted(manifest, sig.to_der().as_bytes(), &[other_pem]),
            Err(SigError::Untrusted)
        );
    }

    #[test]
    fn no_trusted_keys_denies() {
        assert_eq!(verify_trusted(b"x", b"y", &[]), Err(SigError::Untrusted));
    }
}
