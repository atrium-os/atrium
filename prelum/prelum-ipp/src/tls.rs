//! The printer's TLS (printing.md §7.3). Printers have self-signed
//! certificates, so a printer is pinned by the SHA-256 of its public key
//! (its SPKI), whatever names and dates the certificate shows: that
//! survives a renewed certificate with the same key. Adding a printer
//! learns the key; after that a different key is a hard stop, before any
//! request byte.

use std::sync::{Arc, Mutex};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};

#[derive(Debug)]
pub struct Verifier {
    /// `None`: learn (accept what is shown and record it).
    pin: Option<[u8; 32]>,
    provider: Arc<CryptoProvider>,
    /// The key the printer showed on the last handshake.
    pub seen: Mutex<Option<[u8; 32]>>,
}

impl Verifier {
    pub fn pinned(&self) -> Option<[u8; 32]> {
        self.pin
    }
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(&self, ee: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        let key = spki_sha256(ee).ok_or_else(|| rustls::Error::General("the printer's certificate can't be read".into()))?;
        *self.seen.lock().unwrap() = Some(key);
        match self.pin {
            None => Ok(ServerCertVerified::assertion()),
            Some(p) if p == key => Ok(ServerCertVerified::assertion()),
            Some(_) => Err(rustls::Error::General("the printer's key changed".into())),
        }
    }

    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// A client config that accepts exactly `pin` (or anything, to learn).
pub fn config(pin: Option<[u8; 32]>) -> Result<(Arc<rustls::ClientConfig>, Arc<Verifier>), String> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let v = Arc::new(Verifier { pin, provider: provider.clone(), seen: Mutex::new(None) });
    // TLS 1.2 too: many printers have nothing newer.
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(v.clone())
        .with_no_client_auth();
    Ok((Arc::new(cfg), v))
}

/// `SHA256:<base64>` of a key digest, as the sheet and Curia show it.
pub fn fingerprint(d: &[u8; 32]) -> String {
    use base64::Engine;
    format!("SHA256:{}", base64::engine::general_purpose::STANDARD_NO_PAD.encode(d))
}

/// One DER element: (tag, whole element, contents, rest).
fn tlv(b: &[u8]) -> Option<(u8, &[u8], &[u8], &[u8])> {
    let tag = *b.first()?;
    let l0 = *b.get(1)?;
    let (len, hdr) = if l0 < 0x80 {
        (l0 as usize, 2)
    } else {
        let n = (l0 & 0x7f) as usize;
        if n == 0 || n > 4 {
            return None;
        }
        let mut len = 0usize;
        for i in 0..n {
            len = (len << 8) | *b.get(2 + i)? as usize;
        }
        (len, 2 + n)
    };
    let end = hdr.checked_add(len)?;
    Some((tag, b.get(..end)?, b.get(hdr..end)?, b.get(end..)?))
}

/// SHA-256 of the certificate's SubjectPublicKeyInfo (the whole DER
/// element), the form `openssl pkey -pubout | sha256` gives.
pub fn spki_sha256(cert: &[u8]) -> Option<[u8; 32]> {
    let (0x30, _, c, _) = tlv(cert)? else { return None };
    let (0x30, _, mut tbs, _) = tlv(c)? else { return None };
    // version [0] (optional), serial, signature, issuer, validity, subject, spki
    if tbs.first() == Some(&0xa0) {
        tbs = tlv(tbs)?.3;
    }
    for _ in 0..5 {
        tbs = tlv(tbs)?.3;
    }
    let (0x30, whole, _, _) = tlv(tbs)? else { return None };
    Some(Sha256::digest(whole).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spki_matches_rcgen() {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec!["printer.local".into()]).unwrap().self_signed(&key).unwrap();
        let want: [u8; 32] = Sha256::digest(key.public_key_der()).into();
        assert_eq!(spki_sha256(cert.der()), Some(want));
        assert!(fingerprint(&want).starts_with("SHA256:"));
        assert_eq!(spki_sha256(&cert.der()[..40]), None);
    }
}
