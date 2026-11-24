//! TLS inside EAP (RFC 5216 §3, shared by PEAP and TTLS): the flags byte,
//! fragments both ways, and the server certificate check every method
//! here depends on.

use std::sync::{Arc, Mutex};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::{verify_server_cert_signed_by_trust_anchor, ResolvesClientCert};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::ParsedCertificate;
use rustls::sign::CertifiedKey;
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, RootCertStore, SignatureScheme};

pub const LENGTH: u8 = 0x80;
pub const MORE: u8 = 0x40;
pub const START: u8 = 0x20;

/// Bytes of TLS per fragment we send (well under a 1500-byte EAPOL frame).
pub const FRAGMENT: usize = 1000;
/// The largest TLS message we reassemble from a server (a long chain fits).
pub const REASSEMBLY_MAX: usize = 64 * 1024;

/// What the server must prove before any credential leaves the tunnel.
#[derive(Clone, Debug)]
pub enum Trust {
    /// Chains to one of these roots and names the domain (or a name under it).
    Roots { roots: Arc<RootCertStore>, domain: String },
    /// The top certificate the server sent at first join ("Trust this
    /// server"), and the name it carried then.
    Pinned { top: CertificateDer<'static>, domain: String },
    /// Nothing set yet: the join stops at the certificate and says what it saw.
    Unset,
}

/// A server certificate the user has not decided about.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    /// End entity first, as the server sent it.
    pub chain: Vec<CertificateDer<'static>>,
    /// The end entity's DNS names.
    pub names: Vec<String>,
}

impl Seen {
    /// The certificate to pin: the last one the server sent.
    pub fn top(&self) -> &CertificateDer<'static> {
        self.chain.last().expect("a chain is never empty")
    }

    /// The name to pin: the first DNS name.
    pub fn domain(&self) -> Option<&str> {
        self.names.first().map(String::as_str)
    }
}

/// Whether `name` is `domain` or a name under it (case-insensitive, no
/// wildcards: a server proves one name).
pub fn name_matches(name: &str, domain: &str) -> bool {
    let name = name.trim_end_matches('.');
    let domain = domain.trim_end_matches('.');
    if domain.is_empty() || name.starts_with("*.") {
        return false;
    }
    if name.eq_ignore_ascii_case(domain) {
        return true;
    }
    name.len() > domain.len() + 1
        && name.as_bytes()[name.len() - domain.len() - 1] == b'.'
        && name[name.len() - domain.len()..].eq_ignore_ascii_case(domain)
}

fn dns_names(cert: &CertificateDer<'_>) -> Vec<String> {
    webpki::EndEntityCert::try_from(cert)
        .map(|c| c.valid_dns_names().map(str::to_owned).collect())
        .unwrap_or_default()
}

#[derive(Debug)]
pub struct Verifier {
    trust: Trust,
    provider: Arc<rustls::crypto::CryptoProvider>,
    /// Filled when the trust is unset and a certificate arrived.
    pub seen: Mutex<Option<Seen>>,
}

fn other(s: &str) -> rustls::Error {
    rustls::Error::General(s.into())
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let names = dns_names(end_entity);
        let named = |domain: &str| names.iter().any(|n| name_matches(n, domain));
        let parsed = ParsedCertificate::try_from(end_entity)?;
        let algs = self.provider.signature_verification_algorithms.all;
        match &self.trust {
            Trust::Unset => {
                let mut chain = vec![end_entity.clone().into_owned()];
                chain.extend(intermediates.iter().map(|c| c.clone().into_owned()));
                *self.seen.lock().unwrap() = Some(Seen { chain, names });
                Err(other("server certificate not trusted yet"))
            }
            Trust::Roots { roots, domain } => {
                verify_server_cert_signed_by_trust_anchor(&parsed, roots, intermediates, now, algs)?;
                if !named(domain) {
                    return Err(other("server certificate does not name the domain"));
                }
                Ok(ServerCertVerified::assertion())
            }
            Trust::Pinned { top, domain } => {
                if !named(domain) {
                    return Err(other("server certificate does not name the domain"));
                }
                // A server that sends only its own certificate is pinned by
                // its bytes; otherwise the pinned top must anchor the chain.
                if end_entity.as_ref() == top.as_ref() {
                    return Ok(ServerCertVerified::assertion());
                }
                let anchor = webpki::anchor_from_trusted_cert(top)
                    .map_err(|_| other("pinned certificate unreadable"))?
                    .to_owned();
                let roots = RootCertStore { roots: vec![anchor] };
                let rest: Vec<_> = intermediates.iter().filter(|c| c.as_ref() != top.as_ref()).cloned().collect();
                verify_server_cert_signed_by_trust_anchor(&parsed, &roots, &rest, now, algs)?;
                Ok(ServerCertVerified::assertion())
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.provider.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

#[derive(Debug)]
struct OneCert(Arc<CertifiedKey>);

impl ResolvesClientCert for OneCert {
    fn resolve(&self, _hints: &[&[u8]], _schemes: &[SignatureScheme]) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }

    fn has_certs(&self) -> bool {
        true
    }
}

/// A TLS client for one EAP conversation: TLS 1.2 only (RFC 5216 keying;
/// TLS 1.3 in EAP is RFC 9190 and comes later), no SNI, no resumption.
pub fn client(trust: Trust, cert: Option<Arc<CertifiedKey>>) -> Result<(ClientConnection, Arc<Verifier>), rustls::Error> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = Arc::new(Verifier { trust, provider: provider.clone(), seen: Mutex::new(None) });
    let b = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS12])?
        .dangerous()
        .with_custom_certificate_verifier(verifier.clone());
    let mut cfg = match cert {
        Some(c) => b.with_client_cert_resolver(Arc::new(OneCert(c))),
        None => b.with_no_client_auth(),
    };
    cfg.enable_sni = false;
    cfg.resumption = rustls::client::Resumption::disabled();
    // The name is never sent (no SNI) and never checked against: the
    // verifier checks the configured domain instead.
    let name = ServerName::try_from("eap.invalid").unwrap();
    Ok((ClientConnection::new(Arc::new(cfg), name)?, verifier))
}

/// One method's TLS records over EAP: reassembles what arrives and cuts
/// up what leaves.
#[derive(Debug, Default)]
pub struct Frames {
    rx: Vec<u8>,
    rx_total: Option<usize>,
    tx: Vec<u8>,
    tx_off: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum In {
    /// A whole TLS message from the server.
    Message(Vec<u8>),
    /// A fragment; answer with an acknowledgement.
    Partial,
    /// The server acknowledged our fragment; send the next one.
    Ack,
    /// The method's start (S flag).
    Start,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Bad {
    Short,
    Oversized,
    Inconsistent,
}

impl Frames {
    /// Whether fragments of ours are still waiting to go.
    pub fn sending(&self) -> bool {
        self.tx_off < self.tx.len()
    }

    /// The method data of one request (flags first).
    pub fn take(&mut self, data: &[u8]) -> Result<In, Bad> {
        let (&flags, mut rest) = data.split_first().ok_or(Bad::Short)?;
        if flags & START != 0 {
            *self = Frames::default();
            return Ok(In::Start);
        }
        if flags & LENGTH != 0 {
            let l: [u8; 4] = rest.get(..4).ok_or(Bad::Short)?.try_into().unwrap();
            let l = u32::from_be_bytes(l) as usize;
            if l > REASSEMBLY_MAX {
                return Err(Bad::Oversized);
            }
            match self.rx_total {
                None if self.rx.is_empty() => self.rx_total = Some(l),
                Some(t) if t == l => {}
                _ => return Err(Bad::Inconsistent),
            }
            rest = &rest[4..];
        }
        if rest.is_empty() && flags & MORE == 0 && self.rx.is_empty() {
            return if self.sending() { Ok(In::Ack) } else { Err(Bad::Inconsistent) };
        }
        if self.sending() {
            return Err(Bad::Inconsistent);
        }
        self.rx.extend_from_slice(rest);
        if self.rx.len() > self.rx_total.unwrap_or(REASSEMBLY_MAX) {
            return Err(Bad::Oversized);
        }
        if flags & MORE != 0 {
            return Ok(In::Partial);
        }
        if self.rx_total.is_some_and(|t| t != self.rx.len()) {
            return Err(Bad::Inconsistent);
        }
        self.rx_total = None;
        Ok(In::Message(std::mem::take(&mut self.rx)))
    }

    /// Queues a TLS message to send.
    pub fn queue(&mut self, tls: Vec<u8>) {
        self.tx = tls;
        self.tx_off = 0;
    }

    /// The next fragment's method data, flags first (`version` in the low
    /// bits). An empty queue gives an acknowledgement.
    pub fn next(&mut self, version: u8) -> Vec<u8> {
        let left = self.tx.len() - self.tx_off;
        if left == 0 {
            return vec![version];
        }
        let n = left.min(FRAGMENT);
        let mut out = Vec::with_capacity(n + 5);
        let more = n < left;
        let first = self.tx_off == 0;
        let mut flags = version;
        if more {
            flags |= MORE;
        }
        if first && more {
            flags |= LENGTH;
        }
        out.push(flags);
        if first && more {
            out.extend_from_slice(&(self.tx.len() as u32).to_be_bytes());
        }
        out.extend_from_slice(&self.tx[self.tx_off..self.tx_off + n]);
        self.tx_off += n;
        if self.tx_off == self.tx.len() {
            self.tx.clear();
            self.tx_off = 0;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert!(name_matches("radius.corp.example", "corp.example"));
        assert!(name_matches("CORP.example.", "corp.example"));
        assert!(!name_matches("evilcorp.example", "corp.example"));
        assert!(!name_matches("corp.example.evil", "corp.example"));
        assert!(!name_matches("*.corp.example", "corp.example"));
        assert!(!name_matches("radius.corp.example", ""));
    }

    #[test]
    fn fragments_round_trip() {
        let msg: Vec<u8> = (0..2500u32).map(|i| i as u8).collect();
        let mut tx = Frames::default();
        tx.queue(msg.clone());
        let mut rx = Frames::default();
        let a = tx.next(0);
        assert_eq!(a[0], LENGTH | MORE);
        assert_eq!(rx.take(&a), Ok(In::Partial));
        let b = tx.next(0);
        assert_eq!(b[0], MORE);
        assert_eq!(rx.take(&b), Ok(In::Partial));
        let c = tx.next(0);
        assert_eq!(c[0], 0);
        assert!(!tx.sending());
        assert_eq!(rx.take(&c), Ok(In::Message(msg)));
        assert_eq!(tx.next(1), vec![1], "an empty queue acknowledges");
    }

    #[test]
    fn hostile_lengths() {
        let mut rx = Frames::default();
        assert_eq!(rx.take(&[LENGTH | MORE, 0, 0x10, 0, 0, 1]), Err(Bad::Oversized));
        let mut rx = Frames::default();
        assert_eq!(rx.take(&[LENGTH | MORE, 0, 0, 0, 4, 1, 2, 3]), Ok(In::Partial));
        assert_eq!(rx.take(&[0, 4, 5]), Err(Bad::Oversized), "more than declared");
        let mut rx = Frames::default();
        assert_eq!(rx.take(&[LENGTH, 0, 0, 0, 9, 1]), Err(Bad::Inconsistent), "less than declared");
        assert_eq!(Frames::default().take(&[0]), Err(Bad::Inconsistent), "an ack of nothing");
        assert_eq!(Frames::default().take(&[]), Err(Bad::Short));
        assert_eq!(Frames::default().take(&[START | 1]), Ok(In::Start));
    }
}
