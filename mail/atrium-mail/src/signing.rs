//! Signing mail with a certificate on a token (mail.md §10.1): Mail asks
//! sigillumd, which shows its own sheet (which certificate, then the PIN),
//! has the token sign the 32-byte digest and checks the signature before
//! answering. Mail never sees the PIN or the key.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use mail_engine::smime::Signer;
use sigillum_proto::client::{self, CertPurpose, ChooseCertificate, Reply, Request, SignWithCertificate};
use sigillum_proto::driver::{HashAlg, SignScheme};
use sigillum_proto::{read_msg, write_msg};

/// Longer than any ceremony sigillumd runs (its own timeout ends it first).
const WAIT: Duration = Duration::from_secs(300);

struct Link {
    socket: PathBuf,
    next: AtomicU64,
}

impl Link {
    fn ask(&self, req: impl FnOnce(u64) -> Request) -> Result<Reply, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let mut s = UnixStream::connect(&self.socket).map_err(|e| format!("Sigillum isn't running ({e})"))?;
        s.set_read_timeout(Some(WAIT)).map_err(|e| e.to_string())?;
        write_msg(&mut s, &req(id), client::MAX_MESSAGE).map_err(|e| format!("Sigillum: {e}"))?;
        match read_msg::<Reply>(&mut s, client::MAX_MESSAGE).map_err(|e| format!("Sigillum: {e}"))? {
            Some(Reply::Refused { reason: client::Refusal::Denied, .. }) => Err("Mail isn't allowed to sign with a token on this system".into()),
            Some(Reply::Refused { reason, .. }) => Err(format!("Sigillum said no ({reason:?})")),
            Some(r) => Ok(r),
            None => Err("Sigillum closed the connection".into()),
        }
    }
}

/// The scheme for a certificate's key, by the algorithm in its SPKI.
fn scheme(cert: &[u8]) -> Result<SignScheme, String> {
    match mail_smime::sign::scheme_for(cert)? {
        mail_smime::sign::Scheme::RsaPkcs1Sha256 => Ok(SignScheme::RsaPkcs1(HashAlg::Sha256)),
        mail_smime::sign::Scheme::EcdsaSha256 => Ok(SignScheme::Ecdsa(HashAlg::Sha256)),
    }
}

/// A signer through sigillumd on `socket` ([`client::SOCKET`] on Atrium).
pub fn through_sigillum(socket: impl Into<PathBuf>) -> crate::compose::SignWith {
    let link = Arc::new(Link { socket: socket.into(), next: AtomicU64::new(1) });
    Rc::new(move |from: &str| {
        let purpose = CertPurpose::MailSign { from: from.to_string() };
        let schemes = vec![SignScheme::RsaPkcs1(HashAlg::Sha256), SignScheme::Ecdsa(HashAlg::Sha256)];
        let cert = match link.ask(|ceremony| Request::ChooseCertificate(ChooseCertificate { ceremony, timeout_ms: 0, purpose: purpose.clone(), schemes }))? {
            Reply::CertificateChosen { cert, .. } => cert,
            other => return Err(format!("unexpected answer from Sigillum: {other:?}")),
        };
        let scheme = scheme(&cert)?;
        let cert_sha256 = mail_smime::cert::sha256(&cert);
        let link = link.clone();
        Ok(Signer {
            cert,
            chain: Vec::new(),
            sign: Box::new(move |digest: &[u8; 32]| {
                match link.ask(|ceremony| Request::SignWithCertificate(SignWithCertificate { ceremony, timeout_ms: 0, purpose, cert_sha256, scheme, digest: digest.to_vec() }))? {
                    Reply::CertificateSigned { signature, .. } => Ok(signature),
                    other => Err(format!("unexpected answer from Sigillum: {other:?}")),
                }
            }),
        })
    })
}
