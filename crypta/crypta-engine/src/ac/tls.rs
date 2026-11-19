//! TLS to the gateway (vpn.md §9.2): rustls, as the rest of Atrium. The
//! gateway is checked against the system's roots and the profile's own
//! CAs, or, for a self-signed gateway, against the certificate the profile
//! pins (SHA-256 of it, as OpenConnect's `--servercert`). A client
//! certificate, when the profile has one, signs through Arca.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme};

pub const SYSTEM_ROOTS: &str = "/etc/ssl/cert.pem";

#[derive(Debug)]
struct Pin {
    sha256: [u8; 32],
    provider: Arc<CryptoProvider>,
}

impl ServerCertVerifier for Pin {
    fn verify_server_cert(&self, end: &CertificateDer<'_>, _: &[CertificateDer<'_>], _: &ServerName<'_>, _: &[u8], _: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        let d = ring::digest::digest(&ring::digest::SHA256, end.as_ref());
        if d.as_ref() == self.sha256 {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General("the gateway's certificate is not the one this profile pins".into()))
        }
    }

    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// A client key in Arca (vpn.md §9.3): the handshake's signature is made
/// by arcad through the secrets helper (`sign digest=`), P-256 only.
#[derive(Debug)]
pub struct ArcaKey {
    pub chain: Vec<CertificateDer<'static>>,
    pub secrets: Arc<std::sync::Mutex<crate::link::Secrets>>,
}

#[derive(Debug)]
struct ArcaSigner(Arc<std::sync::Mutex<crate::link::Secrets>>);

impl rustls::sign::Signer for ArcaSigner {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, rustls::Error> {
        let d = ring::digest::digest(&ring::digest::SHA256, message);
        let mut s = self.0.lock().map_err(|_| rustls::Error::General("secrets".into()))?;
        let a = s.call(&aura_ipc::Line::new("sign").with_hex("digest", d.as_ref())).map_err(|e| rustls::Error::General(format!("Arca: {e}")))?;
        match (a.verb.as_str(), a.hex("sig")) {
            ("ok", Some(sig)) => Ok(sig),
            _ => Err(rustls::Error::General(format!("Arca refused to sign ({})", a.get("status").unwrap_or("?")))),
        }
    }

    fn scheme(&self) -> SignatureScheme {
        SignatureScheme::ECDSA_NISTP256_SHA256
    }
}

#[derive(Debug)]
struct ArcaSigning(Arc<std::sync::Mutex<crate::link::Secrets>>);

impl rustls::sign::SigningKey for ArcaSigning {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn rustls::sign::Signer>> {
        offered.contains(&SignatureScheme::ECDSA_NISTP256_SHA256).then(|| Box::new(ArcaSigner(self.0.clone())) as Box<dyn rustls::sign::Signer>)
    }

    fn algorithm(&self) -> rustls::SignatureAlgorithm {
        rustls::SignatureAlgorithm::ECDSA
    }
}

impl rustls::client::ResolvesClientCert for ArcaKey {
    fn resolve(&self, _: &[&[u8]], schemes: &[SignatureScheme]) -> Option<Arc<rustls::sign::CertifiedKey>> {
        if !schemes.contains(&SignatureScheme::ECDSA_NISTP256_SHA256) {
            return None;
        }
        Some(Arc::new(rustls::sign::CertifiedKey::new(self.chain.clone(), Arc::new(ArcaSigning(self.secrets.clone())))))
    }

    fn has_certs(&self) -> bool {
        true
    }
}

/// The client configuration for one profile. `system` is the PEM read
/// before the sandbox (may be empty); `client` signs with Arca's key.
pub fn config(system: &[u8], cas: &[Vec<u8>], pin: Option<[u8; 32]>, client: Option<Arc<dyn rustls::client::ResolvesClientCert>>) -> Result<Arc<ClientConfig>, String> {
    use rustls_pki_types::pem::PemObject;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let b = ClientConfig::builder_with_provider(provider.clone()).with_safe_default_protocol_versions().map_err(|e| e.to_string())?;
    let b = match pin {
        Some(sha256) => b.dangerous().with_custom_certificate_verifier(Arc::new(Pin { sha256, provider })),
        None => {
            let mut roots = rustls::RootCertStore::empty();
            for c in CertificateDer::pem_slice_iter(system).flatten() {
                let _ = roots.add(c);
            }
            for c in cas {
                roots.add(CertificateDer::from(c.clone())).map_err(|e| format!("a CA in the profile: {e}"))?;
            }
            if roots.is_empty() {
                return Err("no CA to check the gateway with".into());
            }
            b.with_root_certificates(roots)
        }
    };
    let cfg = match client {
        Some(r) => b.with_client_cert_resolver(r),
        None => b.with_no_client_auth(),
    };
    Ok(Arc::new(cfg))
}

/// A TLS connection, blocking, with the socket's timeouts.
pub struct Tls {
    pub c: ClientConnection,
    pub s: TcpStream,
}

impl Tls {
    pub fn new(cfg: Arc<ClientConfig>, host: &str, s: TcpStream) -> Result<Tls, String> {
        let name = ServerName::try_from(host.to_string()).map_err(|_| format!("{host}: not a server name"))?;
        let c = ClientConnection::new(cfg, name).map_err(|e| e.to_string())?;
        let mut t = Tls { c, s };
        while t.c.is_handshaking() {
            t.c.complete_io(&mut t.s).map_err(|e| match e.get_ref().and_then(|i| i.downcast_ref::<rustls::Error>()) {
                Some(rustls::Error::General(w)) => w.clone(),
                Some(r) => format!("TLS: {r}"),
                None => format!("TLS: {e}"),
            })?;
        }
        Ok(t)
    }

    pub fn flush(&mut self) -> io::Result<()> {
        while self.c.wants_write() {
            self.c.write_tls(&mut self.s)?;
        }
        Ok(())
    }

    pub fn send(&mut self, b: &[u8]) -> io::Result<()> {
        self.c.writer().write_all(b)?;
        self.flush()
    }

    /// Plaintext already decrypted, without touching the socket.
    pub fn drain(&mut self, out: &mut Vec<u8>) -> io::Result<bool> {
        let mut b = [0u8; 16384];
        loop {
            match self.c.reader().read(&mut b) {
                Ok(0) => return Ok(false),
                Ok(n) => out.extend_from_slice(&b[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(true),
                Err(e) => return Err(e),
            }
        }
    }

    /// Read from the socket once (blocking up to its timeout), then what
    /// that decrypted. `Ok(false)`: the gateway closed.
    pub fn recv(&mut self, out: &mut Vec<u8>) -> io::Result<bool> {
        let n = self.c.read_tls(&mut self.s)?;
        if n == 0 {
            return Ok(false);
        }
        self.c.process_new_packets().map_err(io::Error::other)?;
        self.flush()?;
        self.drain(out)
    }
}
