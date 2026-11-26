//! A session's Ed25519 key, made fresh and held in memory only (§11.2).

use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};

pub struct SessionKey {
    pkcs8: Vec<u8>,
    spki: Vec<u8>,
}

impl SessionKey {
    pub fn generate() -> Result<SessionKey, rustls::Error> {
        let rng = ring::rand::SystemRandom::new();
        let doc = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng)
            .map_err(|_| rustls::Error::General("no randomness".into()))?;
        let pkcs8 = doc.as_ref().to_vec();
        let signer = rustls::crypto::ring::sign::any_eddsa_type(&PrivatePkcs8KeyDer::from(pkcs8.as_slice()))?;
        let spki = signer
            .public_key()
            .ok_or_else(|| rustls::Error::General("no public key".into()))?
            .as_ref()
            .to_vec();
        Ok(SessionKey { pkcs8, spki })
    }

    /// The public half, as the DER SubjectPublicKeyInfo TLS presents and
    /// the other side pins.
    pub fn public(&self) -> &[u8] {
        &self.spki
    }

    pub(crate) fn certified(&self) -> Result<std::sync::Arc<rustls::sign::CertifiedKey>, rustls::Error> {
        let der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.pkcs8.clone()));
        let signer = rustls::crypto::ring::sign::any_eddsa_type(match &der {
            PrivateKeyDer::Pkcs8(p) => p,
            _ => return Err(rustls::Error::General("key".into())),
        })?;
        let cert = vec![rustls::pki_types::CertificateDer::from(self.spki.clone())];
        Ok(std::sync::Arc::new(rustls::sign::CertifiedKey::new(cert, signer)))
    }
}

impl Drop for SessionKey {
    fn drop(&mut self) {
        // Best effort: the private key does not outlive the session in
        // this process's memory.
        for b in self.pkcs8.iter_mut() {
            // SAFETY: `b` is a valid, aligned, exclusive reference into
            // our own Vec; a volatile write keeps the zeroing from being
            // optimised away.
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}
