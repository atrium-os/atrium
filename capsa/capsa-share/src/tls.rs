//! The WebDAV share server's TLS (S2, network-shares.md §5.1): which
//! certificate a server may show, and the connections themselves.
//!
//! A share is saved either as **trusted** (its certificate must validate
//! for the host name through the system trust store, as any HTTPS site's;
//! it may change at every renewal) or **pinned** (a self-signed NAS: the
//! SHA-256 of its public key, accepted by the user, whatever the name or
//! dates say; a different key is a hard stop). `Learn` validates and
//! records what it saw, for the user to decide, and the share server says
//! nothing over it.

use std::sync::{Arc, Mutex};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};

use crate::cert::{self, Summary};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Identity {
    /// The trust store vouches for the host name.
    Trusted,
    /// The SHA-256 of the server's public key.
    Pinned([u8; 32]),
    /// Record what is shown and accept it (`--learn`: nothing is sent).
    Learn,
}

impl Identity {
    /// What Arca pins for this share: the key's digest, or zeros for a
    /// share whose name the trust store vouches for.
    pub fn arca_key(&self) -> [u8; 32] {
        match self {
            Identity::Pinned(k) => *k,
            _ => [0; 32],
        }
    }
}

/// What the server showed.
#[derive(Debug, Clone, Default)]
pub struct Seen {
    pub summary: Summary,
    /// `None`: valid for the name through the trust store; else why not.
    pub untrusted: Option<String>,
}

#[derive(Debug)]
pub struct Verifier {
    id: Identity,
    webpki: Option<Arc<WebPkiServerVerifier>>,
    provider: Arc<CryptoProvider>,
    pub seen: Mutex<Option<Seen>>,
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(&self, ee: &CertificateDer<'_>, inter: &[CertificateDer<'_>], name: &ServerName<'_>, ocsp: &[u8], now: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        let summary = cert::summary(ee).ok_or_else(|| rustls::Error::General("the server's certificate can't be read".into()))?;
        let web = match &self.webpki {
            Some(v) => v.verify_server_cert(ee, inter, name, ocsp, now).map(|_| ()).map_err(|e| e.to_string()),
            None => Err("no trust store".into()),
        };
        let digest = summary.spki_sha256;
        *self.seen.lock().unwrap() = Some(Seen { summary, untrusted: web.clone().err() });
        match self.id {
            Identity::Learn => Ok(ServerCertVerified::assertion()),
            Identity::Trusted => web.map(|_| ServerCertVerified::assertion()).map_err(rustls::Error::General),
            Identity::Pinned(pin) if pin == digest => Ok(ServerCertVerified::assertion()),
            Identity::Pinned(_) => Err(rustls::Error::General(format!("KEY CHANGED: the server shows {}", cert::fingerprint(&digest)))),
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

/// The client config for `id`, with the trust store's roots (PEM; may be
/// empty for a pinned share or a learn).
pub fn config(id: Identity, roots_pem: &[u8]) -> Result<(Arc<rustls::ClientConfig>, Arc<Verifier>), String> {
    use rustls::pki_types::pem::PemObject;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    for c in CertificateDer::pem_slice_iter(roots_pem).flatten() {
        let _ = roots.add(c);
    }
    let webpki = if roots.is_empty() {
        None
    } else {
        Some(WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider.clone()).build().map_err(|e| e.to_string())?)
    };
    if id == Identity::Trusted && webpki.is_none() {
        return Err("the trust store holds no certificates".into());
    }
    let v = Arc::new(Verifier { id, webpki, provider: provider.clone(), seen: Mutex::new(None) });
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .dangerous()
        .with_custom_certificate_verifier(v.clone())
        .with_no_client_auth();
    Ok((Arc::new(cfg), v))
}

/// The name the certificate is checked against (a DNS name, or an IP
/// address for a server reached by one).
pub fn server_name(host: &str) -> Result<ServerName<'static>, String> {
    ServerName::try_from(host.to_string()).map_err(|_| format!("{host} is not a host name"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    struct Pki {
        ca_pem: String,
        cert: rcgen::Certificate,
        key: rcgen::KeyPair,
    }

    fn pki(signed: bool) -> Pki {
        let ca_key = rcgen::KeyPair::generate().unwrap();
        let mut ca = rcgen::CertificateParams::new(vec![]).unwrap();
        ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca.distinguished_name.push(rcgen::DnType::CommonName, "Test CA");
        let ca_cert = ca.self_signed(&ca_key).unwrap();
        let key = rcgen::KeyPair::generate().unwrap();
        let mut p = rcgen::CertificateParams::new(vec!["nas.lan".into()]).unwrap();
        p.distinguished_name.push(rcgen::DnType::CommonName, "nas.lan");
        let cert = if signed { p.signed_by(&key, &ca_cert, &ca_key).unwrap() } else { p.self_signed(&key).unwrap() };
        Pki { ca_pem: ca_cert.pem(), cert, key }
    }

    /// One TLS server that answers "ok" to whatever comes.
    fn serve(p: &Pki) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let chain = vec![p.cert.der().clone()];
        let key = rustls::pki_types::PrivateKeyDer::try_from(p.key.serialize_der()).unwrap();
        let cfg = Arc::new(
            rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(chain, key)
                .unwrap(),
        );
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let cfg = cfg.clone();
                std::thread::spawn(move || {
                    let c = rustls::ServerConnection::new(cfg).unwrap();
                    let mut t = rustls::StreamOwned::new(c, s);
                    let mut b = [0u8; 1];
                    if t.read(&mut b).is_ok() {
                        let _ = t.write_all(b"ok");
                        let _ = t.flush();
                    }
                });
            }
        });
        port
    }

    fn talk(id: Identity, roots: &str, port: u16) -> (Result<String, String>, Option<Seen>) {
        let (cfg, v) = config(id, roots.as_bytes()).unwrap();
        let c = rustls::ClientConnection::new(cfg, server_name("nas.lan").unwrap()).unwrap();
        let mut t = rustls::StreamOwned::new(c, TcpStream::connect(("127.0.0.1", port)).unwrap());
        let r = t.write_all(b"x").and_then(|_| t.flush()).and_then(|_| {
            let mut b = [0u8; 2];
            t.read_exact(&mut b).map(|_| String::from_utf8_lossy(&b).into_owned())
        });
        let seen = v.seen.lock().unwrap().clone();
        (r.map_err(|e| e.to_string()), seen)
    }

    #[test]
    fn a_trusted_share_needs_the_trust_store() {
        let p = pki(true);
        let port = serve(&p);
        assert_eq!(talk(Identity::Trusted, &p.ca_pem, port).0.as_deref(), Ok("ok"));
        let other = pki(true);
        assert!(talk(Identity::Trusted, &other.ca_pem, port).0.is_err(), "another CA's roots don't vouch for it");
        assert!(config(Identity::Trusted, b"").is_err());
    }

    #[test]
    fn a_pinned_share_is_its_key_alone() {
        let p = pki(false);
        let port = serve(&p);
        let (r, seen) = talk(Identity::Learn, "", port);
        assert_eq!(r.as_deref(), Ok("ok"));
        let seen = seen.unwrap();
        assert!(seen.untrusted.is_some(), "self-signed: the trust store doesn't vouch");
        let pin = seen.summary.spki_sha256;
        assert_eq!(talk(Identity::Pinned(pin), "", port).0.as_deref(), Ok("ok"));
        let (r, _) = talk(Identity::Pinned([7; 32]), "", port);
        assert!(r.unwrap_err().contains("KEY CHANGED"));
        assert!(talk(Identity::Trusted, &p.ca_pem, port).0.is_err(), "not valid through a trust store");
    }
}
