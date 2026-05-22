//! Mutual-auth TLS for the device↔relay link, via
//! self-signed certs + public-key pinning.
//!
//! Per the design sign-off:
//!   - `rustls` (pure-Rust, ring crypto provider) — no
//!     OpenSSL / system-TLS dependency, identical on
//!     macOS / FreeBSD / Linux.
//!   - Self-signed certs, no CA. Each side carries an
//!     `Identity` (a freshly-generated self-signed
//!     cert + its key) and *pins* the peer's exact
//!     cert DER. The custom verifiers below replace
//!     chain validation with a constant-time-ish DER
//!     equality check, then still verify the handshake
//!     signature so a pinned cert can't be replayed
//!     without its private key.
//!   - Opt-in: the relay + daemon negotiate TLS only
//!     when configured. The wire *inside* the TLS
//!     stream is unchanged — the same length-prefixed
//!     CBOR frames from `proto`.
//!
//! A full X.509 chain with a real Vestibulum-rooted CA
//! is the production follow-up; pinning is the v0
//! mechanism (`tabellarius.md` §3.2).

use rcgen::generate_simple_self_signed;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, Error, SignatureScheme};
use std::sync::Arc;

/// A self-signed TLS identity: the cert and its private
/// key, both DER-encoded. Generate one per relay and
/// one per device; pin the peer's `cert_der`.
#[derive(Debug, Clone)]
pub struct Identity {
    /// Self-signed certificate, DER.
    pub cert_der: Vec<u8>,
    /// PKCS#8 private key, DER.
    pub key_der: Vec<u8>,
}

impl Identity {
    /// Generate a fresh self-signed identity. The cert's
    /// SAN is `localhost` — irrelevant under pinning
    /// (the verifier ignores the name) but rcgen wants
    /// one.
    pub fn generate() -> Result<Identity, String> {
        let certified = generate_simple_self_signed(vec!["localhost".to_string()])
            .map_err(|e| format!("rcgen: {e}"))?;
        Ok(Identity {
            cert_der: certified.cert.der().to_vec(),
            key_der: certified.key_pair.serialize_der(),
        })
    }
}

/// Verifier that accepts exactly one pinned peer cert.
/// Used for both directions — the server pins the
/// device cert, the client pins the relay cert.
#[derive(Debug)]
struct Pinned {
    expected: Vec<u8>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl Pinned {
    fn new(expected: Vec<u8>) -> Self {
        Pinned {
            expected,
            provider: Arc::new(rustls::crypto::ring::default_provider()),
        }
    }

    fn check_pin(&self, end_entity: &CertificateDer<'_>) -> Result<(), Error> {
        if end_entity.as_ref() == self.expected.as_slice() {
            Ok(())
        } else {
            Err(Error::General(
                "peer certificate does not match the pinned key".into(),
            ))
        }
    }
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        self.check_pin(end_entity)?;
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls12_signature(
            message, cert, dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls13_signature(
            message, cert, dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

impl ClientCertVerifier for Pinned {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        self.check_pin(end_entity)?;
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls12_signature(
            message, cert, dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        rustls::crypto::verify_tls13_signature(
            message, cert, dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

fn identity_pair(me: &Identity)
    -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>), String>
{
    let cert = CertificateDer::from(me.cert_der.clone());
    let key = PrivateKeyDer::try_from(me.key_der.clone())
        .map_err(|e| format!("private key: {e}"))?;
    Ok((vec![cert], key))
}

/// Build a `ServerConfig` (relay side): present `me`'s
/// cert, require + pin the client (device) cert.
pub fn server_config(me: &Identity, pinned_peer_cert: &[u8])
    -> Result<Arc<rustls::ServerConfig>, String>
{
    let (chain, key) = identity_pair(me)?;
    let verifier = Arc::new(Pinned::new(pinned_peer_cert.to_vec()));
    let cfg = rustls::ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(chain, key)
        .map_err(|e| format!("server config: {e}"))?;
    Ok(Arc::new(cfg))
}

/// Build a `ClientConfig` (device side): present `me`'s
/// cert, pin the server (relay) cert.
pub fn client_config(me: &Identity, pinned_peer_cert: &[u8])
    -> Result<Arc<rustls::ClientConfig>, String>
{
    let (chain, key) = identity_pair(me)?;
    let verifier = Arc::new(Pinned::new(pinned_peer_cert.to_vec()));
    let cfg = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(chain, key)
        .map_err(|e| format!("client config: {e}"))?;
    Ok(Arc::new(cfg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    /// Drive a full mutual-TLS handshake over a
    /// localhost TCP pair + echo a byte.
    fn handshake(
        server_id: &Identity, server_pins: &[u8],
        client_id: &Identity, client_pins: &[u8],
    ) -> Result<u8, String> {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        let s_cfg = server_config(server_id, server_pins)?;
        let c_cfg = client_config(client_id, client_pins)?;

        let server = thread::spawn(move || -> Result<(), String> {
            let (tcp, _) = listener.accept().map_err(|e| e.to_string())?;
            let conn = rustls::ServerConnection::new(s_cfg)
                .map_err(|e| e.to_string())?;
            let mut tls = rustls::StreamOwned::new(conn, tcp);
            let mut buf = [0u8; 1];
            tls.read_exact(&mut buf).map_err(|e| e.to_string())?;
            tls.write_all(&[buf[0] + 1]).map_err(|e| e.to_string())?;
            tls.flush().map_err(|e| e.to_string())?;
            Ok(())
        });

        let tcp = TcpStream::connect(addr).map_err(|e| e.to_string())?;
        let name = ServerName::try_from("localhost").unwrap();
        let conn = rustls::ClientConnection::new(c_cfg, name)
            .map_err(|e| e.to_string())?;
        let mut tls = rustls::StreamOwned::new(conn, tcp);
        tls.write_all(&[41u8]).map_err(|e| e.to_string())?;
        tls.flush().map_err(|e| e.to_string())?;
        let mut got = [0u8; 1];
        tls.read_exact(&mut got).map_err(|e| e.to_string())?;

        server.join().unwrap()?;
        Ok(got[0])
    }

    #[test]
    fn mutual_tls_handshake_with_correct_pins_succeeds() {
        let relay = Identity::generate().unwrap();
        let device = Identity::generate().unwrap();
        // Relay pins the device cert; device pins the
        // relay cert.
        let echoed = handshake(
            &relay, &device.cert_der,
            &device, &relay.cert_der,
        ).expect("handshake should succeed with correct pins");
        assert_eq!(echoed, 42, "server should have echoed 41 + 1");
    }

    #[test]
    fn handshake_fails_when_client_pins_wrong_relay_cert() {
        let relay = Identity::generate().unwrap();
        let device = Identity::generate().unwrap();
        let imposter = Identity::generate().unwrap();
        // Device pins the imposter's cert, not the real
        // relay's — the relay it actually connects to
        // presents a cert that doesn't match.
        let r = handshake(
            &relay, &device.cert_der,
            &device, &imposter.cert_der,
        );
        assert!(r.is_err(), "wrong relay pin must fail the handshake");
    }

    #[test]
    fn handshake_fails_when_relay_pins_wrong_device_cert() {
        let relay = Identity::generate().unwrap();
        let device = Identity::generate().unwrap();
        let imposter = Identity::generate().unwrap();
        // Relay pins the imposter's cert; the real
        // device's client cert won't match.
        let r = handshake(
            &relay, &imposter.cert_der,
            &device, &relay.cert_der,
        );
        assert!(r.is_err(), "wrong device pin must fail the handshake");
    }

    #[test]
    fn generated_identities_are_distinct() {
        let a = Identity::generate().unwrap();
        let b = Identity::generate().unwrap();
        assert_ne!(a.cert_der, b.cert_der);
        assert_ne!(a.key_der, b.key_der);
    }
}
