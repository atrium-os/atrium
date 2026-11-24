//! Opt-in lab integration for the persistent Arca store and a fixed
//! Portcullis worker. The daemon exposes only a fixed synthetic operation
//! when built and configured for the private verification gate.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use arca_store::{secret::LockedBuf, Access, Class, Kind, NewItem, Provenance, Recipient, Store};
use mail_smime::legacy_wire as wire;
use portcullis_ipc::{Request, Response};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use super::{native, storage_persona};
use crate::label::Label;
use crate::proto::{op, status};
use crate::server::{reply, Body, Reply, State};

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

struct Approval {
    registry: std::path::PathBuf,
    uid: u32,
    publisher: [u8; 32],
    persona: Option<String>,
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
}

impl Approval {
    fn valid(&self, client: &UnixStream) -> bool {
        if self.cancelled.load(Ordering::Acquire) || Instant::now() >= self.deadline {
            return false;
        }
        // This trusted UI connection carries one request. Readability, EOF
        // or an error while the reply is pending ends its approval.
        let mut poll = libc::pollfd {
            fd: client.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: poll receives one initialized descriptor for this call.
        let quiet = unsafe { libc::poll(&mut poll, 1, 0) } == 0;
        quiet
            && signed_persona(&self.registry, self.uid, self.publisher)
                == Some(self.persona.clone())
    }
}

fn watch_worker<T>(
    control: UnixStream,
    lifetime: UnixStream,
    valid: impl Fn() -> bool + Sync,
    operation: impl FnOnce(UnixStream, UnixStream, &UnixStream) -> io::Result<T>,
) -> io::Result<T> {
    let watched_control = control.try_clone()?;
    let watched_lifetime = lifetime.try_clone()?;
    let finished = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let watcher = std::thread::Builder::new().spawn_scoped(scope, || {
            while !finished.load(Ordering::Acquire) {
                if !valid() {
                    let _ = watched_control.shutdown(Shutdown::Both);
                    let _ = watched_lifetime.shutdown(Shutdown::Both);
                    break;
                }
                std::thread::park_timeout(Duration::from_millis(20));
            }
        })?;
        let result = operation(control, lifetime, &watched_control);
        finished.store(true, Ordering::Release);
        watcher.thread().unpark();
        result
    })
}

fn approval(state: &mut State, body: &[u8]) -> Option<Approval> {
    let uid = u32::from_le_bytes(body.try_into().ok()?);
    let registry = state.legacy_smime_registry.clone()?;
    let publisher = state.mail_sign_ins.mail_publisher?;
    if state.legacy_smime_pending.is_some() {
        return None;
    }
    let persona = signed_persona(&registry, uid, publisher)?;
    let cancelled = Arc::new(AtomicBool::new(false));
    state.legacy_smime_pending = Some(cancelled.clone());
    Some(Approval {
        registry,
        uid,
        publisher,
        persona,
        cancelled,
        deadline: Instant::now() + Duration::from_secs(24),
    })
}

fn synthetic_open(
    state: &Mutex<State>,
    client: &UnixStream,
    approval: &Approval,
    start: impl FnOnce() -> io::Result<(UnixStream, UnixStream)>,
) -> io::Result<bool> {
    let valid = || approval.valid(client);
    if !valid() {
        return Err(closed());
    }
    let (control, lifetime) = start().map_err(|_| closed())?;
    watch_worker(
        control,
        lifetime,
        valid,
        |control, mut lifetime, close_control| {
            let result = (|| {
                let prepared = Prepared::start(control, CERT, CMS)?;
                if !valid() || prepared.message_digest != <[u8; 32]>::from(Sha256::digest(CMS)) {
                    return Err(closed());
                }
                let persona = approval.persona.as_deref();
                let publisher = approval.publisher;
                let secret = {
                    let mut state = state.lock().unwrap_or_else(|p| p.into_inner());
                    if approval.cancelled.load(Ordering::Acquire) {
                        return Err(closed());
                    }
                    let store = state.stores.get_mut(SYNTHETIC_USER).ok_or_else(closed)?;
                    if persona == Some("work")
                        && prepared.key_row(store, persona, publisher)?.is_none()
                    {
                        enroll(store, persona, publisher, CERT, KEY)?;
                    }
                    prepared.store_key(store, persona, publisher)?
                };
                if !valid() {
                    return Err(closed());
                }
                prepared.finish_with_key(secret, valid)
            })();
            let _ = close_control.shutdown(Shutdown::Both);
            let exited = portcullis_ipc::read_response(&mut lifetime);
            if !matches!(exited, Ok(Response::LaunchExit { code: Some(0) })) || !valid() {
                return Err(closed());
            }
            Ok(result.is_ok_and(|text| text.as_str() == SYNTHETIC_PREVIEW))
        },
    )
}

pub fn serve_synthetic_open(state: &Mutex<State>, client: &UnixStream, body: &[u8]) -> Reply {
    serve_open(state, client, body, launch)
}

fn serve_open(
    state: &Mutex<State>,
    client: &UnixStream,
    body: &[u8],
    start: impl FnOnce() -> io::Result<(UnixStream, UnixStream)>,
) -> Reply {
    let lock = || state.lock().unwrap_or_else(|p| p.into_inner());
    let pending = {
        let mut state = lock();
        approval(&mut state, body)
    };
    let Some(approval) = pending else {
        lock().audit(
            &Label::TrustedUi,
            op::U_SMIME_SYNTHETIC_OPEN,
            None,
            status::DENIED,
        );
        return reply(status::DENIED);
    };
    let result = synthetic_open(state, client, &approval, start);
    let mut state = lock();
    state.legacy_smime_pending = None;
    let result = match result {
        // The test UI can attest the worker exited cleanly. Limen forwards
        // only the generic sheet outcome to Mail.
        Ok(opened) if approval.valid(client) => Reply {
            status: if opened { status::OK } else { status::DENIED },
            body: Body::Plain(vec![0]),
        },
        _ => reply(status::DENIED),
    };
    state.audit(
        &Label::TrustedUi,
        op::U_SMIME_SYNTHETIC_OPEN,
        None,
        result.status,
    );
    result
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
        self,
        store: &Store,
        persona: Option<&str>,
        publisher: [u8; 32],
        approve: impl FnOnce([u8; 32]) -> bool,
    ) -> io::Result<Zeroizing<String>> {
        if !approve(self.message_digest) {
            return Err(closed());
        }
        let secret = self.store_key(store, persona, publisher)?;
        self.finish_with_key(secret, || true)
    }

    fn key_row(
        &self,
        store: &Store,
        persona: Option<&str>,
        publisher: [u8; 32],
    ) -> io::Result<Option<arca_store::ItemId>> {
        let persona = storage_persona(persona).ok_or_else(closed)?;
        let recipient = Recipient::SmimeLegacy { publisher };
        let access = Access {
            persona,
            recipient: &recipient,
        };
        Ok(store
            .list(&access)
            .map_err(|_| closed())?
            .into_iter()
            .find(|row| {
                row.kind == Kind::SmimeLegacyRsaKey
                    && row.scope.get(..32) == Some(&self.fingerprint)
                    && row.scope.get(32..) == Some(&self.cert_digest)
            })
            .map(|row| row.id))
    }

    fn store_key(
        &self,
        store: &Store,
        persona: Option<&str>,
        publisher: [u8; 32],
    ) -> io::Result<LockedBuf> {
        let row = self
            .key_row(store, persona, publisher)?
            .ok_or_else(closed)?;
        let persona = storage_persona(persona).ok_or_else(closed)?;
        let recipient = Recipient::SmimeLegacy { publisher };
        let access = Access {
            persona,
            recipient: &recipient,
        };
        store
            .release(&row, &access, None)
            .map(|(_, secret)| secret)
            .map_err(|_| closed())
    }

    fn finish_with_key(
        mut self,
        secret: LockedBuf,
        valid: impl Fn() -> bool,
    ) -> io::Result<Zeroizing<String>> {
        let key = native::Key::parse(secret.as_ref()).map_err(|_| closed())?;
        drop(secret);
        if !valid() {
            return Err(closed());
        }
        let candidate = key
            .legacy_candidate(&self.wrapped, self.key_bytes)
            .map_err(|_| closed())?;
        if !valid() {
            return Err(closed());
        }
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
        let (ui, _client) = UnixStream::pair().unwrap();
        let state_mutex = Mutex::new(state);
        assert_eq!(
            serve_open(&state_mutex, &ui, &uid, || panic!(
                "disabled gate launched a worker"
            ))
            .status,
            status::DENIED
        );
        state = state_mutex.into_inner().unwrap();
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
            serve_open(&Mutex::new(state), &ui, &uid[..3], || panic!(
                "malformed request launched a worker"
            ))
            .status,
            status::DENIED
        );
    }

    enum Revoke {
        Lock,
        Disconnect,
        Registry,
    }

    fn revoke_during_worker_read(revoke: Revoke) {
        let dir = tempfile::tempdir().unwrap();
        let mut state = unlocked(dir.path());
        let registry = dir.path().join("app-registry");
        std::fs::write(
            &registry,
            format!(
                "51000 {SYNTHETIC_USER} org.atrium.mail {} persona=work\n",
                "08".repeat(32)
            ),
        )
        .unwrap();
        state.legacy_smime_registry = Some(registry.clone());
        let user = body(|w| w.str(SYNTHETIC_USER));
        let root = body(|w| {
            w.str(SYNTHETIC_USER);
            w.raw(&ROOT);
        });
        assert_eq!(
            state.handle(&Label::Vestibulum, op::V_CREATE, &root).status,
            status::OK
        );
        let state = Mutex::new(state);

        let (ui, client) = UnixStream::pair().unwrap();
        let (control, mut worker) = UnixStream::pair().unwrap();
        let (lifetime, mut launcher) = UnixStream::pair().unwrap();
        let bound = Some(Duration::from_secs(5));
        worker.set_read_timeout(bound).unwrap();
        launcher.set_read_timeout(bound).unwrap();
        lifetime.set_read_timeout(bound).unwrap();

        std::thread::scope(|scope| {
            let read = scope.spawn(|| {
                serve_open(&state, &ui, &51000u32.to_le_bytes(), || {
                    Ok((control, lifetime))
                })
            });
            worker.write_all(wire::WORKER_READY).unwrap();
            assert_eq!(wire::read(&mut worker, wire::MAX_CERT).unwrap(), CERT);
            assert_eq!(wire::read(&mut worker, wire::MAX_CMS).unwrap(), CMS);
            let env = mail_smime::envelope::enveloped_data(CMS).unwrap();
            let (fingerprint, size, wrapped) =
                mail_smime::envelope::legacy_recipient(&env, CERT).unwrap();
            worker.write_all(&[1, size as u8]).unwrap();
            worker.write_all(&fingerprint).unwrap();
            wire::write(&mut worker, &wrapped, 512).unwrap();
            let mut candidate = Zeroizing::new(vec![0; size]);
            worker.read_exact(&mut candidate).unwrap();
            assert!(
                !read.is_finished(),
                "legacy result arrived before the worker completed"
            );
            // The worker is paused before its content result. Arca must
            // grant another request access to State during this wait.
            match revoke {
                Revoke::Lock => {
                    let mut state = state
                        .try_lock()
                        .expect("worker I/O holds the shared state lock");
                    assert_eq!(
                        state.handle(&Label::Vestibulum, op::V_LOCK, &user).status,
                        status::OK
                    );
                    assert_eq!(
                        state.handle(&Label::Vestibulum, op::V_UNLOCK, &root).status,
                        status::OK
                    );
                }
                Revoke::Disconnect => drop(client),
                Revoke::Registry => std::fs::write(&registry, "").unwrap(),
            }
            assert_eq!(worker.read(&mut [0]).unwrap(), 0);
            assert_eq!(launcher.read(&mut [0]).unwrap(), 0);
            let reply = read.join().unwrap();
            assert_eq!(reply.status, status::DENIED);
            assert!(reply.body.bytes().is_empty());
            assert!(state.lock().unwrap().legacy_smime_pending.is_none());
        });
    }

    #[test]
    fn legacy_lock_revokes_a_pending_worker_read_even_after_unlock() {
        revoke_during_worker_read(Revoke::Lock);
    }

    #[test]
    fn legacy_sheet_disconnect_revokes_a_pending_worker_read() {
        revoke_during_worker_read(Revoke::Disconnect);
    }

    #[test]
    fn legacy_registry_change_revokes_a_pending_worker_read() {
        revoke_during_worker_read(Revoke::Registry);
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
