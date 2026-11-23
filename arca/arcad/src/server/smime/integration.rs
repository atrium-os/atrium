//! Opt-in lab integration for the persistent Arca store and a fixed
//! Portcullis worker. Regular arcad and Mail have no access to this route.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use arca_store::{Access, Class, Kind, NewItem, Provenance, Recipient, Store};
use mail_smime::legacy_wire as wire;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use super::{native, storage_persona};

pub use wire::FAILURE;
type Fingerprint = [u8; 32];

fn closed() -> io::Error {
    io::Error::other(FAILURE)
}

/// Owns the private channel and the binding of one immutable request.
/// This value can be consumed once, by the trusted approval UI.
pub struct Prepared {
    worker: UnixStream,
    fingerprint: Fingerprint,
    cert_digest: [u8; 32],
    message_digest: [u8; 32],
    key_bytes: usize,
    wrapped: Vec<u8>,
}

impl Prepared {
    pub fn start(mut worker: UnixStream, cert: &[u8], cms: &[u8]) -> io::Result<Self> {
        worker
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|_| closed())?;
        worker
            .set_write_timeout(Some(Duration::from_secs(5)))
            .map_err(|_| closed())?;
        let mut ready = [0u8; wire::WORKER_READY.len()];
        worker.read_exact(&mut ready).map_err(|_| closed())?;
        if ready != wire::WORKER_READY {
            return Err(closed());
        }
        wire::write(&mut worker, cert, wire::MAX_CERT).map_err(|_| closed())?;
        wire::write(&mut worker, cms, wire::MAX_CMS).map_err(|_| closed())?;
        let mut header = [0u8; 2];
        worker.read_exact(&mut header[..1]).map_err(|_| closed())?;
        if header[0] != 1 {
            return Err(closed());
        }
        worker.read_exact(&mut header[1..]).map_err(|_| closed())?;
        if !matches!(header[1], 16 | 32) {
            return Err(closed());
        }
        let mut fingerprint = [0u8; 32];
        worker.read_exact(&mut fingerprint).map_err(|_| closed())?;
        let wrapped = wire::read(&mut worker, 512).map_err(|_| closed())?;
        Ok(Self {
            worker,
            fingerprint,
            wrapped,
            key_bytes: header[1] as usize,
            cert_digest: Sha256::digest(cert).into(),
            message_digest: Sha256::digest(cms).into(),
        })
    }

    pub fn message_digest(&self) -> [u8; 32] {
        self.message_digest
    }

    /// The only endorsement enters from the trusted UI, which spends the
    /// press against its own Fresco window before returning the binding.
    /// The candidate travels on the private socket to the assigned worker.
    pub fn finish(
        mut self,
        store: &Store,
        persona: Option<&str>,
        publisher: [u8; 32],
        approve: impl FnOnce([u8; 32]) -> bool,
    ) -> io::Result<Zeroizing<String>> {
        if !approve(self.message_digest) {
            return Err(closed());
        }
        let persona = storage_persona(persona).ok_or_else(closed)?;
        let recipient = Recipient::SmimeLegacy { publisher };
        let access = Access {
            persona,
            recipient: &recipient,
        };
        let row = store
            .list(&access)
            .map_err(|_| closed())?
            .into_iter()
            .find(|row| {
                row.kind == Kind::SmimeLegacyRsaKey
                    && row.scope.get(..32) == Some(&self.fingerprint)
                    && row.scope.get(32..) == Some(&self.cert_digest)
            })
            .ok_or_else(closed)?;
        let (_, secret) = store
            .release(&row.id, &access, None)
            .map_err(|_| closed())?;
        let key = native::Key::parse(secret.as_ref()).map_err(|_| closed())?;
        drop(secret);
        let candidate = key
            .legacy_candidate(&self.wrapped, self.key_bytes)
            .map_err(|_| closed())?;
        self.worker
            .write_all(candidate.as_ref())
            .map_err(|_| closed())?;
        drop(candidate);
        let mut status = [0u8; 1];
        self.worker.read_exact(&mut status).map_err(|_| closed())?;
        if status != [1] {
            return Err(closed());
        }
        let bytes = wire::read(&mut self.worker, wire::MAX_TEXT).map_err(|_| closed())?;
        String::from_utf8(bytes).map(Zeroizing::new).map_err(|e| {
            drop(Zeroizing::new(e.into_bytes()));
            closed()
        })
    }
}

pub fn enroll(
    store: &mut Store,
    persona: Option<&str>,
    publisher: [u8; 32],
    cert: &[u8],
    der: &[u8],
) -> io::Result<Fingerprint> {
    let persona = storage_persona(persona).ok_or_else(closed)?;
    if publisher == [0; 32]
        || cert.len() > wire::MAX_CERT
        || der.len() > arca_store::model::caps::VALUE
    {
        return Err(closed());
    }
    let key = native::Key::parse(der).map_err(|_| closed())?;
    key.check_private().map_err(|_| closed())?;
    let fingerprint = key.fingerprint().map_err(|_| closed())?;
    if native::certificate_fingerprint(cert).map_err(|_| closed())? != fingerprint
        || store
            .has_smime_key(Kind::SmimeRsaKey, &fingerprint)
            .map_err(|_| closed())?
        || store
            .has_smime_key(Kind::SmimeLegacyRsaKey, &fingerprint)
            .map_err(|_| closed())?
    {
        return Err(closed());
    }
    let mut scope = [0u8; 64];
    scope[..32].copy_from_slice(&fingerprint);
    scope[32..].copy_from_slice(&Sha256::digest(cert));
    store
        .add(NewItem {
            persona,
            recipient: Recipient::SmimeLegacy { publisher },
            class: Class::WhileUnlocked,
            kind: Kind::SmimeLegacyRsaKey,
            provenance: Provenance::Migrated,
            account: "Legacy S/MIME receive only",
            scope: &scope,
            value: der,
        })
        .map_err(|_| closed())?;
    Ok(fingerprint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        label::Label,
        proto::{op, status},
        server::State,
    };

    const CERT: &[u8] = include_bytes!("../../../../../mail/mail-smime/tests/data/leaf.der");
    const KEY: &[u8] = include_bytes!("../../../../../mail/mail-smime/tests/data/leaf.p8");
    const PUB: [u8; 32] = [8; 32];
    const ROOT: [u8; 32] = [7; 32];

    fn unlocked(dir: &std::path::Path) -> State {
        let mut state = State::new(dir.into()).unwrap();
        state.mail_sign_ins.mail_publisher = Some(PUB);
        let request = body(|w| {
            w.str("alice");
            w.raw(&ROOT);
        });
        assert_eq!(
            state
                .handle(&Label::Vestibulum, op::V_CREATE, &request)
                .status,
            status::OK
        );
        state
    }

    fn body(write: impl FnOnce(&mut arca_store::codec::Writer)) -> Vec<u8> {
        let mut writer = arca_store::codec::Writer::new();
        write(&mut writer);
        writer.buf
    }

    fn oaep_enrollment(persona: &str) -> Vec<u8> {
        body(|w| {
            w.str("alice");
            w.str(persona);
            w.str("Modern identity");
            w.bytes(CERT);
            w.bytes(KEY);
        })
    }

    #[test]
    fn legacy_and_oaep_keys_cannot_be_shared_across_personas_or_store_reopen() {
        let first = tempfile::tempdir().unwrap();
        let mut state = unlocked(first.path());
        assert_eq!(
            state
                .handle(
                    &Label::TrustedUi,
                    op::U_SMIME_PROVISION,
                    &oaep_enrollment("")
                )
                .status,
            status::OK
        );
        assert!(enroll(
            state.stores.get_mut("alice").unwrap(),
            Some("work"),
            [9; 32],
            CERT,
            KEY
        )
        .is_err());

        let second = tempfile::tempdir().unwrap();
        let mut state = unlocked(second.path());
        let fingerprint = enroll(
            state.stores.get_mut("alice").unwrap(),
            Some("work"),
            PUB,
            CERT,
            KEY,
        )
        .unwrap();
        drop(state);
        let mut state = State::new(second.path().into()).unwrap();
        state.mail_sign_ins.mail_publisher = Some(PUB);
        let request = body(|w| {
            w.str("alice");
            w.raw(&ROOT);
        });
        assert_eq!(
            state
                .handle(&Label::Vestibulum, op::V_UNLOCK, &request)
                .status,
            status::OK
        );
        assert!(state.stores["alice"]
            .has_smime_key(Kind::SmimeLegacyRsaKey, &fingerprint)
            .unwrap());
        assert_eq!(
            state
                .handle(
                    &Label::TrustedUi,
                    op::U_SMIME_PROVISION,
                    &oaep_enrollment("")
                )
                .status,
            status::DENIED
        );
        let mail = Label::App {
            user: "alice".into(),
            publisher: PUB,
            app_id: "org.atrium.mail".into(),
            persona: Some("work".into()),
        };
        let reply = state.handle(&mail, op::MAIL_SMIME_CERTS, &[]);
        assert_eq!(reply.status, status::OK);
        assert_eq!(reply.body.bytes(), 0u32.to_le_bytes());
    }
}
