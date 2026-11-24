//! An EAP-TLS key that is not here: the TLS CertificateVerify is signed by
//! Arca, through aura-keys (wifi.md §7.4 E3). P-256 with SHA-256 only.

use std::sync::Arc;

use rustls::sign::{Signer, SigningKey};
use rustls::{SignatureAlgorithm, SignatureScheme};
use sha2::{Digest, Sha256};

/// Signs a SHA-256 digest; `None` when the key's owner refused.
pub type SignFn = dyn Fn(&[u8; 32]) -> Option<Vec<u8>> + Send + Sync;

#[derive(Clone)]
pub struct RemoteKey(pub Arc<SignFn>);

impl std::fmt::Debug for RemoteKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RemoteKey(P-256)")
    }
}

impl SigningKey for RemoteKey {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        offered.contains(&SignatureScheme::ECDSA_NISTP256_SHA256).then(|| Box::new(self.clone()) as Box<dyn Signer>)
    }

    fn algorithm(&self) -> SignatureAlgorithm {
        SignatureAlgorithm::ECDSA
    }
}

impl Signer for RemoteKey {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        let d: [u8; 32] = Sha256::digest(message).into();
        (self.0)(&d).filter(|s| !s.is_empty()).ok_or_else(|| rustls::Error::General("the key's owner did not sign".into()))
    }

    fn scheme(&self) -> SignatureScheme {
        SignatureScheme::ECDSA_NISTP256_SHA256
    }
}
