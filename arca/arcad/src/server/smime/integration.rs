//! Opt-in lab integration for the persistent Arca store and a fixed
//! Portcullis worker. The daemon exposes only a fixed synthetic operation
//! when built and configured for the private verification gate.

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use arca_store::{Access, Class, Kind, NewItem, Provenance, Recipient, Store};
use mail_smime::legacy_wire as wire;
use portcullis_ipc::{Request, Response};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use super::{native, storage_persona};

pub use wire::FAILURE;
type Fingerprint = [u8; 32];

fn closed() -> io::Error {
    io::Error::other(FAILURE)
}

const WORKER: &str = "org.atrium.smime-legacy-reader";
pub const SYNTHETIC_USER: &str = "maillegacyfixture";
pub const SYNTHETIC_PREVIEW: &str = "From OpenSSL.\r\n";
const CERT: &[u8] = include_bytes!("../../../../../mail/mail-smime/tests/data/leaf.der");
const KEY: &[u8] = include_bytes!("../../../../../mail/mail-smime/tests/data/leaf.p8");
const CMS: &[u8] = include_bytes!("../../../../../mail/mail-smime/tests/data/env.der");

fn launch() -> io::Result<(UnixStream, UnixStream)> {
    let mut launch =
        UnixStream::connect(portcullis_ipc::resolve_socket_path().ok_or_else(closed)?)?;
    let timeout = Some(Duration::from_secs(8));
    launch.set_read_timeout(timeout)?;
    launch.set_write_timeout(timeout)?;
    match portcullis_ipc::round_trip(
        &mut launch,
        &Request::Hello {
            version: portcullis_ipc::PROTO_VERSION,
        },
    )? {
        Response::Hello { .. } => {}
        _ => return Err(closed()),
    }
    portcullis_ipc::write_request(
        &mut launch,
        &Request::ExecInstance {
            app_id: WORKER.into(),
            instance: None,
            tmpfs_mb: 16,
            memory_mb: None,
            cpu_pct: None,
            require_memory_limit: true,
            serves: None,
            persona: None,
        },
    )?;
    if !matches!(
        portcullis_ipc::read_response(&mut launch)?,
        Response::ReadyForFds
    ) {
        return Err(closed());
    }
    let (control, child) = UnixStream::pair()?;
    let null = std::fs::OpenOptions::new().write(true).open("/dev/null")?;
    portcullis_ipc::send_fds(
        &launch,
        &[child.as_raw_fd(), null.as_raw_fd(), null.as_raw_fd()],
    )?;
    Ok((control, launch))
}

/// The launch socket owns the jail. Keep it alive after the data channel
/// closes, including refusal paths, until Portcullis reports worker exit.
pub fn with_worker<T>(
    operation: impl FnOnce(UnixStream) -> io::Result<T>,
) -> io::Result<io::Result<T>> {
    let (control, mut lifetime) = launch().map_err(|_| closed())?;
    let result = operation(control);
    match portcullis_ipc::read_response(&mut lifetime) {
        Ok(Response::LaunchExit { code: Some(0) }) => Ok(result),
        _ => Err(closed()),
    }
}

/// Resolve the subject of the trusted UI call from Portcullis's live
/// registry. The caller supplies no persona, key, certificate or message.
fn signed_persona(registry: &Path, uid: u32, publisher: [u8; 32]) -> Option<Option<String>> {
    let registry = portcullis_peer::AppRegistry::load(registry.to_str()?).ok()?;
    if registry.resolve(uid) != Some((SYNTHETIC_USER, "org.atrium.mail"))
        || publisher == [0; 32]
        || registry.publisher(uid) != Some(Some(publisher))
    {
        return None;
    }
    match registry.persona(uid)? {
        None => Some(None),
        Some("work") => Some(Some("work".into())),
        Some(_) => None,
    }
}

pub(in crate::server) fn synthetic_open(
    store: &mut Store,
    registry: &Path,
    uid: u32,
    publisher: [u8; 32],
) -> io::Result<bool> {
    let persona = signed_persona(registry, uid, publisher).ok_or_else(closed)?;
    with_worker(|control| {
        let prepared = Prepared::start(control, CERT, CMS)?;
        let digest: [u8; 32] = Sha256::digest(CMS).into();
        if persona.as_deref() == Some("work") {
            let access = Access {
                persona: "work",
                recipient: &Recipient::SmimeLegacy { publisher },
            };
            let already_enrolled = store
                .list(&access)
                .map_err(|_| closed())?
                .iter()
                .any(|row| {
                    row.kind == Kind::SmimeLegacyRsaKey
                        && row.scope.get(..32) == Some(&prepared.fingerprint)
                        && row.scope.get(32..) == Some(&prepared.cert_digest)
                });
            if !already_enrolled {
                enroll(store, Some("work"), publisher, CERT, KEY)?;
            }
        }
        let text = prepared.finish(store, persona.as_deref(), publisher, |approved| {
            approved == digest && signed_persona(registry, uid, publisher) == Some(persona.clone())
        })?;
        if text.as_str() == SYNTHETIC_PREVIEW {
            Ok(())
        } else {
            Err(closed())
        }
    })
    .map(|operation| operation.is_ok())
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
    fn synthetic_parent_binding_requires_the_registered_mail_publisher_and_persona() {
        let dir = tempfile::tempdir().unwrap();
        let registry = dir.path().join("app-registry");
        let publisher = "08".repeat(32);
        std::fs::write(
            &registry,
            format!(
                "51000 {SYNTHETIC_USER} org.atrium.mail {publisher} persona=work\n\
                 51001 {SYNTHETIC_USER} org.atrium.mail {publisher}\n\
                 51002 {SYNTHETIC_USER} org.atrium.mail\n\
                 51003 {SYNTHETIC_USER} org.atrium.mail {publisher} persona=school\n\
                 51004 alice org.atrium.mail {publisher} persona=work\n\
                 51005 {SYNTHETIC_USER} org.atrium.other {publisher} persona=work\n"
            ),
        )
        .unwrap();
        assert_eq!(
            signed_persona(&registry, 51000, PUB),
            Some(Some("work".into()))
        );
        assert_eq!(signed_persona(&registry, 51001, PUB), Some(None));
        assert_eq!(signed_persona(&registry, 51000, [9; 32]), None);
        for uid in 51002..=51006 {
            assert_eq!(signed_persona(&registry, uid, PUB), None);
        }
        std::fs::write(&registry, "").unwrap();
        assert_eq!(signed_persona(&registry, 51000, PUB), None);
    }

    #[test]
    fn synthetic_dispatch_is_disabled_by_default_and_unavailable_to_mail() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = unlocked(dir.path());
        let uid = 51000u32.to_le_bytes();
        assert_eq!(
            state
                .handle(&Label::TrustedUi, op::U_SMIME_SYNTHETIC_OPEN, &uid)
                .status,
            status::DENIED
        );
        let mail = Label::App {
            user: SYNTHETIC_USER.into(),
            publisher: PUB,
            app_id: "org.atrium.mail".into(),
            persona: Some("work".into()),
        };
        state.legacy_smime_registry = Some(dir.path().join("app-registry"));
        assert_ne!(
            state.handle(&mail, op::U_SMIME_SYNTHETIC_OPEN, &uid).status,
            status::OK
        );
        assert_eq!(
            state
                .handle(&Label::TrustedUi, op::U_SMIME_SYNTHETIC_OPEN, &uid[..3])
                .status,
            status::DENIED
        );
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
