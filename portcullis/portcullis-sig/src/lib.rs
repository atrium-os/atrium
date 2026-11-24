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
    /// A key held elsewhere (a hardware token) could not sign: why.
    External(String),
}

/// A signer for keys that are not a PEM: the text of a key file that names a
/// key held elsewhere (a hardware token, `portcullis-token`). `sign` answers
/// `None` when the text isn't one of its references; `Some` with a DER
/// signature or the reason it failed. Installed once by a tool that signs
/// (`opifex`); daemons never install one, so for them a key is a PEM only.
pub struct ExternalSigner {
    pub sign: fn(&[u8], &str) -> Option<Result<Vec<u8>, String>>,
    pub public_pem: fn(&str) -> Option<Result<String, String>>,
}

static EXTERNAL: std::sync::OnceLock<ExternalSigner> = std::sync::OnceLock::new();

/// Install the signer for non-PEM key files (once per process).
pub fn set_external_signer(s: ExternalSigner) { let _ = EXTERNAL.set(s); }

fn is_pem(key: &str) -> bool { key.trim_start().starts_with("-----BEGIN") }

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

/// Sign `manifest` with a PKCS#8 PEM private key: DER ECDSA over P-256 with
/// the SHA-256 prehash, what [`verify_one`] checks. Used for the machine's
/// own dev publisher (homes.md §5.3: what a workspace builds), never for a
/// third party's.
///
/// A key file that is not a PEM goes to the installed [`ExternalSigner`].
pub fn sign(manifest: &[u8], key_pem: &str) -> Result<Vec<u8>, SigError> {
    use p256::ecdsa::signature::Signer;
    if !is_pem(key_pem) {
        return match EXTERNAL.get().and_then(|x| (x.sign)(manifest, key_pem)) {
            Some(r) => r.map_err(SigError::External),
            None => Err(SigError::BadKey),
        };
    }
    let k = signing_key(key_pem)?;
    let s: Signature = k.sign(manifest);
    Ok(s.to_der().as_bytes().to_vec())
}

/// A PEM private key: PKCS#8 ("BEGIN PRIVATE KEY"), or SEC1 ("BEGIN EC
/// PRIVATE KEY", what `openssl ecparam -genkey` writes). Every path that
/// reads a private key goes through here, so both work everywhere.
fn signing_key(key_pem: &str) -> Result<p256::ecdsa::SigningKey, SigError> {
    use p256::pkcs8::DecodePrivateKey;
    p256::ecdsa::SigningKey::from_pkcs8_pem(key_pem)
        .or_else(|_| p256::SecretKey::from_sec1_pem(key_pem).map(p256::ecdsa::SigningKey::from))
        .map_err(|_| SigError::BadKey)
}

/// A key pair from 32 random bytes: (private PKCS#8 PEM, public SPKI PEM).
pub fn new_key(seed: [u8; 32]) -> Result<(String, String), SigError> {
    use p256::ecdsa::SigningKey;
    use p256::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
    let k = SigningKey::from_bytes(&seed.into()).map_err(|_| SigError::BadKey)?;
    let private = k.to_pkcs8_pem(LineEnding::LF).map_err(|_| SigError::BadKey)?.to_string();
    let public = k.verifying_key().to_public_key_pem(LineEnding::LF).map_err(|_| SigError::BadKey)?;
    Ok((private, public))
}

/// The public SPKI PEM of a PEM private key (PKCS#8 or SEC1).
pub fn public_pem(key_pem: &str) -> Result<String, SigError> {
    use p256::ecdsa::SigningKey;
    if !is_pem(key_pem) {
        return match EXTERNAL.get().and_then(|x| (x.public_pem)(key_pem)) {
            Some(r) => r.map_err(SigError::External),
            None => Err(SigError::BadKey),
        };
    }
    use p256::pkcs8::{EncodePublicKey, LineEnding};
    let k: SigningKey = signing_key(key_pem)?;
    k.verifying_key().to_public_key_pem(LineEnding::LF).map_err(|_| SigError::BadKey)
}

#[cfg(test)]
mod sign_tests {
    #[test]
    fn signs_with_a_sec1_key_too() {
        use p256::pkcs8::LineEnding;
        let (pkcs8, public) = super::new_key([7u8; 32]).unwrap();
        let sec1 = p256::SecretKey::from_bytes(&[7u8; 32].into()).unwrap().to_sec1_pem(LineEnding::LF).unwrap();
        assert!(sec1.contains("BEGIN EC PRIVATE KEY"));
        for k in [pkcs8.as_str(), sec1.as_str()] {
            assert_eq!(super::public_pem(k).unwrap(), super::public_pem(pkcs8.as_str()).unwrap(), "the public key of a SEC1 key");
            let sig = super::sign(b"m", k).unwrap();
            super::verify_one(b"m", &sig, &public).unwrap();
        }
    }

    #[test]
    fn a_signature_of_ours_verifies_and_only_for_its_text() {
        let (k, p) = super::new_key([7; 32]).unwrap();
        assert_eq!(super::public_pem(&k).unwrap(), p);
        let s = super::sign(b"[app]\nid = \"dev.scratch.hello\"\n", &k).unwrap();
        super::verify_one(b"[app]\nid = \"dev.scratch.hello\"\n", &s, &p).unwrap();
        assert!(super::verify_one(b"[app]\nid = \"dev.scratch.other\"\n", &s, &p).is_err());
    }
}
