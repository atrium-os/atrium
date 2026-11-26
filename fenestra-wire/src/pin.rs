//! TLS 1.3 configurations that accept exactly one pinned raw public key
//! (RFC 7250) from the peer, and present our own the same way.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls13_signature_with_raw_key, CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, SubjectPublicKeyInfoDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, Error, PeerIncompatible, SignatureScheme};

use crate::keys::SessionKey;

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn algs() -> WebPkiSupportedAlgorithms {
    rustls::crypto::ring::default_provider().signature_verification_algorithms
}

#[derive(Debug)]
struct Pinned {
    key: Vec<u8>,
    algs: WebPkiSupportedAlgorithms,
}

impl Pinned {
    fn check(&self, presented: &CertificateDer<'_>) -> Result<(), Error> {
        if presented.as_ref() == self.key.as_slice() {
            Ok(())
        } else {
            Err(Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure))
        }
    }

    fn tls13(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, Error> {
        // The signature is checked against the pinned key itself.
        self.check(cert)?;
        if dss.scheme != SignatureScheme::ED25519 {
            return Err(Error::PeerIncompatible(PeerIncompatible::NoSignatureSchemesInCommon));
        }
        verify_tls13_signature_with_raw_key(message, &SubjectPublicKeyInfoDer::from(self.key.as_slice()), dss, &self.algs)
    }
}

#[derive(Debug)]
struct PinnedServer(Pinned);

impl ServerCertVerifier for PinnedServer {
    fn verify_server_cert(&self, end_entity: &CertificateDer<'_>, intermediates: &[CertificateDer<'_>], _name: &ServerName<'_>, _ocsp: &[u8], _now: UnixTime) -> Result<ServerCertVerified, Error> {
        if !intermediates.is_empty() {
            return Err(Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure));
        }
        self.0.check(end_entity)?;
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, _m: &[u8], _c: &CertificateDer<'_>, _d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::PeerIncompatible(PeerIncompatible::Tls13RequiredForQuic))
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, Error> {
        self.0.tls13(m, c, d)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

#[derive(Debug)]
struct PinnedClient(Pinned);

impl ClientCertVerifier for PinnedClient {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    fn client_auth_mandatory(&self) -> bool {
        true
    }
    fn verify_client_cert(&self, end_entity: &CertificateDer<'_>, intermediates: &[CertificateDer<'_>], _now: UnixTime) -> Result<ClientCertVerified, Error> {
        if !intermediates.is_empty() {
            return Err(Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure));
        }
        self.0.check(end_entity)?;
        Ok(ClientCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, _m: &[u8], _c: &CertificateDer<'_>, _d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, Error> {
        Err(Error::PeerIncompatible(PeerIncompatible::Tls13RequiredForQuic))
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, Error> {
        self.0.tls13(m, c, d)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![SignatureScheme::ED25519]
    }
    fn requires_raw_public_keys(&self) -> bool {
        true
    }
}

/// The host's TLS: presents `key`, takes only a client presenting
/// `viewer_key`, no resumption, no early data.
pub fn server_config(key: &SessionKey, viewer_key: &[u8]) -> Result<rustls::ServerConfig, Error> {
    let verifier = Arc::new(PinnedClient(Pinned { key: viewer_key.to_vec(), algs: algs() }));
    let mut c = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_cert_resolver(Arc::new(rustls::server::AlwaysResolvesServerRawPublicKeys::new(key.certified()?)));
    c.alpn_protocols = vec![crate::ALPN.to_vec()];
    c.max_early_data_size = 0;
    c.send_tls13_tickets = 0;
    c.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    Ok(c)
}

/// The viewer's TLS: presents `key`, takes only a server presenting
/// `host_key`, no resumption, no early data.
pub fn client_config(key: &SessionKey, host_key: &[u8]) -> Result<rustls::ClientConfig, Error> {
    let verifier = Arc::new(PinnedServer(Pinned { key: host_key.to_vec(), algs: algs() }));
    let mut c = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_cert_resolver(Arc::new(rustls::client::AlwaysResolvesClientRawPublicKeys::new(key.certified()?)));
    c.alpn_protocols = vec![crate::ALPN.to_vec()];
    c.enable_early_data = false;
    c.resumption = rustls::client::Resumption::disabled();
    Ok(c)
}
