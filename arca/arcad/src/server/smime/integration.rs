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

use arca_store::{
    secret::LockedBuf, Access, Class, ItemId, Kind, NewItem, Provenance, Recipient, Store,
};
use mail_smime::legacy_wire as wire;
use portcullis_ipc::{Request, Response};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use super::{native, storage_persona};
use crate::label::Label;
use crate::proto::{self, op, status};
use crate::server::{reply, Body, Reply, State};

pub use wire::{received_selection, Lifecycle, Selection, FAILURE, FIXTURE_ACCOUNT};
type Fingerprint = [u8; 32];

fn closed() -> io::Error {
    io::Error::other(FAILURE)
}

const WORKER: &str = "org.atrium.smime-legacy-reader";
pub const SYNTHETIC_USER: &str = "maillegacyfixture";
pub const SYNTHETIC_PREVIEW: &str = "From OpenSSL.\r\n";
const CERT: &[u8] = include_bytes!("../../../../../mail/mail-smime/tests/data/leaf.der");
const KEY: &[u8] = include_bytes!("../../../../../mail/mail-smime/tests/data/leaf.p8");
#[cfg(test)]
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
    item: Option<ItemId>,
    selection: Selection,
    received: Vec<u8>,
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
}

impl Approval {
    fn valid(&self, client: &UnixStream) -> bool {
        binding_valid(
            client,
            &self.registry,
            self.uid,
            self.publisher,
            self.persona.as_deref(),
            &self.cancelled,
            self.deadline,
        )
    }
}

fn binding_valid(
    client: &UnixStream,
    registry: &Path,
    uid: u32,
    publisher: [u8; 32],
    persona: Option<&str>,
    cancelled: &AtomicBool,
    deadline: Instant,
) -> bool {
    if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
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
        && signed_persona(registry, uid, publisher)
            .as_ref()
            .map(|p| p.as_deref())
            == Some(persona)
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
    let (uid, selected) = body.split_at_checked(4)?;
    let uid = u32::from_le_bytes(uid.try_into().ok()?);
    let (selection, received) = received_selection(selected)?;
    let registry = state.legacy_smime_registry.clone()?;
    let publisher = state.mail_sign_ins.mail_publisher?;
    if state.legacy_smime_pending.is_some() {
        return None;
    }
    let persona = signed_persona(&registry, uid, publisher)?;
    let item = fixture_item(
        state.stores.get(SYNTHETIC_USER)?,
        persona.as_deref(),
        publisher,
    )
    .ok()?;
    let cancelled = Arc::new(AtomicBool::new(false));
    state.legacy_smime_pending = Some(cancelled.clone());
    Some(Approval {
        registry,
        uid,
        publisher,
        persona,
        item,
        selection,
        received: received.to_vec(),
        cancelled,
        deadline: Instant::now() + Duration::from_secs(32),
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
                let prepared = Prepared::start_received(control, CERT, &approval.received)?;
                if !valid()
                    || prepared.message_digest != approval.selection.digest
                    || prepared.cert_digest.as_slice() != Sha256::digest(CERT).as_slice()
                    || prepared.fingerprint
                        != native::certificate_fingerprint(CERT).map_err(|_| closed())?
                {
                    return Err(closed());
                }
                let persona = approval.persona.as_deref();
                let publisher = approval.publisher;
                let secret = {
                    let state = state.lock().unwrap_or_else(|p| p.into_inner());
                    let store = state.stores.get(SYNTHETIC_USER).ok_or_else(closed)?;
                    let item = approval.item.ok_or_else(closed)?;
                    if approval.cancelled.load(Ordering::Acquire)
                        || fixture_item(store, persona, publisher)? != Some(item)
                    {
                        return Err(closed());
                    }
                    let recipient = Recipient::SmimeLegacy { publisher };
                    store
                        .release(&item, &fixture_access(persona, &recipient)?, None)
                        .map_err(|_| closed())?
                        .1
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

pub fn serve_synthetic_open(
    state: &Mutex<State>,
    client: &UnixStream,
    body: &[u8],
) -> io::Result<()> {
    if let Some(action) = body.get(4..).and_then(Lifecycle::decode) {
        return serve_lifecycle(state, client, body, action);
    }
    serve_open(state, client, body, launch)
}

const FIXTURE_SCOPE: &[u8] = wire::FIXTURE_ACCOUNT.as_bytes();

fn fixture_access<'a>(
    persona: Option<&'a str>,
    recipient: &'a Recipient,
) -> io::Result<Access<'a>> {
    Ok(Access {
        persona: storage_persona(persona).ok_or_else(closed)?,
        recipient,
    })
}

fn fixture_item(
    store: &Store,
    persona: Option<&str>,
    publisher: [u8; 32],
) -> io::Result<Option<ItemId>> {
    let recipient = Recipient::SmimeLegacy { publisher };
    let digest = Sha256::digest(CERT);
    Ok(store
        .list(&fixture_access(persona, &recipient)?)
        .map_err(|_| closed())?
        .into_iter()
        .find(|row| {
            row.kind == Kind::SmimeLegacyRsaKey
                && row.scope.get(32..64) == Some(digest.as_slice())
                && row.scope.get(64..) == Some(FIXTURE_SCOPE)
        })
        .map(|row| row.id))
}

fn serve_lifecycle(
    state: &Mutex<State>,
    mut client: &UnixStream,
    body: &[u8],
    action: Lifecycle,
) -> io::Result<()> {
    let uid = u32::from_le_bytes(body[..4].try_into().map_err(|_| closed())?);
    let lock = || state.lock().unwrap_or_else(|p| p.into_inner());
    let pending = (|| {
        let mut state = lock();
        let registry = state.legacy_smime_registry.clone()?;
        let publisher = state.mail_sign_ins.mail_publisher?;
        if state.legacy_smime_management.is_some()
            || signed_persona(&registry, uid, publisher)?.as_deref() != Some("work")
        {
            return None;
        }
        let item = fixture_item(state.stores.get(SYNTHETIC_USER)?, Some("work"), publisher).ok()?;
        let cancelled = Arc::new(AtomicBool::new(false));
        state.legacy_smime_management = Some(cancelled.clone());
        Some((registry, publisher, item, cancelled))
    })();
    let Some((registry, publisher, item, cancelled)) = pending else {
        return proto::write_reply(&mut client, status::DENIED, &[]);
    };
    let result = (|| {
        client.set_read_timeout(Some(Duration::from_secs(32)))?;
        let deadline = Instant::now() + Duration::from_secs(32);
        proto::write_reply(&mut client, status::OK, &[])?;
        let frame = proto::read_frame(&mut client)?.ok_or_else(closed)?;
        let bytes = frame.as_ref();
        if bytes[..2] != op::U_SMIME_SYNTHETIC_MANAGE.to_le_bytes()
            || Lifecycle::decode(&bytes[2..]) != Some(action)
        {
            return Err(closed());
        }
        let mut state = lock();
        if !binding_valid(
            client,
            &registry,
            uid,
            publisher,
            Some("work"),
            &cancelled,
            deadline,
        ) {
            return Err(closed());
        }
        let store = state.stores.get_mut(SYNTHETIC_USER).ok_or_else(closed)?;
        if fixture_item(store, Some("work"), publisher)? != item {
            return Err(closed());
        }
        match (action, item) {
            (Lifecycle::Enroll, None) => {
                enroll_key(store, Some("work"), publisher, CERT, KEY, FIXTURE_SCOPE)?;
            }
            (Lifecycle::Forget, Some(item)) => {
                let recipient = Recipient::SmimeLegacy { publisher };
                store
                    .delete(&item, &fixture_access(Some("work"), &recipient)?)
                    .map_err(|_| closed())?;
                if let Some(preview) = &state.legacy_smime_pending {
                    preview.store(true, Ordering::Release);
                }
            }
            _ => {}
        }
        Ok(())
    })();
    let status = if result.is_ok() {
        status::OK
    } else {
        status::DENIED
    };
    {
        let mut state = lock();
        state.legacy_smime_management = None;
        state.audit(
            &Label::TrustedUi,
            op::U_SMIME_SYNTHETIC_MANAGE,
            None,
            status,
        );
    }
    proto::write_reply(&mut client, status, &[])
}

fn serve_open(
    state: &Mutex<State>,
    mut client: &UnixStream,
    body: &[u8],
    start: impl FnOnce() -> io::Result<(UnixStream, UnixStream)>,
) -> io::Result<()> {
    let lock = || state.lock().unwrap_or_else(|p| p.into_inner());
    let pending = {
        let mut state = lock();
        approval(&mut state, body)
    };
    let Some(approval) = pending else {
        lock().audit(
            &Label::TrustedUi,
            op::U_SMIME_SYNTHETIC_RESERVE,
            None,
            status::DENIED,
        );
        return proto::write_reply(&mut client, status::DENIED, &[]);
    };
    // The socket is the one-use reservation. Exactly one next frame may
    // redeem it. A mismatched frame, disconnect or timeout spends it too.
    let mut attempted = false;
    let result = (|| {
        client.set_read_timeout(Some(
            approval.deadline.saturating_duration_since(Instant::now()),
        ))?;
        proto::write_reply(&mut client, status::OK, &[])?;
        let frame = proto::read_frame(&mut client)?.ok_or_else(closed)?;
        let bytes = frame.as_ref();
        attempted = bytes[..2] == op::U_SMIME_SYNTHETIC_OPEN.to_le_bytes();
        if !attempted || Selection::decode(&bytes[2..]) != Some(approval.selection) {
            return Err(closed());
        }
        synthetic_open(state, client, &approval, start)
    })();
    let final_reply = {
        let mut state = lock();
        state.legacy_smime_pending = None;
        let result = match result {
            // Only the fixture UI sees the synthetic clean-exit marker.
            Ok(opened)
                if approval.valid(client)
                    && (!opened
                        || state.stores.get(SYNTHETIC_USER).and_then(|store| {
                            fixture_item(store, approval.persona.as_deref(), approval.publisher)
                                .ok()
                        }) == Some(approval.item)) =>
            {
                Reply {
                    status: if opened { status::OK } else { status::DENIED },
                    body: Body::Plain(vec![0]),
                }
            }
            _ => reply(status::DENIED),
        };
        let operation = if attempted {
            op::U_SMIME_SYNTHETIC_OPEN
        } else {
            op::U_SMIME_SYNTHETIC_RESERVE
        };
        state.audit(&Label::TrustedUi, operation, None, result.status);
        result
    };
    proto::write_reply(&mut client, final_reply.status, final_reply.body.bytes())
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
    pub fn start(worker: UnixStream, cert: &[u8], cms: &[u8]) -> io::Result<Self> {
        Self::start_input(worker, cert, cms, wire::INPUT_CMS, wire::MAX_CMS)
    }

    fn start_received(worker: UnixStream, cert: &[u8], received: &[u8]) -> io::Result<Self> {
        Self::start_input(
            worker,
            cert,
            received,
            wire::INPUT_RECEIVED,
            wire::MAX_RECEIVED,
        )
    }

    fn start_input(
        mut worker: UnixStream,
        cert: &[u8],
        input: &[u8],
        mode: u8,
        limit: usize,
    ) -> io::Result<Self> {
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
        worker.write_all(&[mode]).map_err(|_| closed())?;
        wire::write(&mut worker, cert, wire::MAX_CERT).map_err(|_| closed())?;
        wire::write(&mut worker, input, limit).map_err(|_| closed())?;
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
        let message_digest = wire::digest(input);
        if mode == wire::INPUT_RECEIVED {
            let mut consumed = [0; 32];
            worker.read_exact(&mut consumed).map_err(|_| closed())?;
            if consumed != message_digest {
                return Err(closed());
            }
        }
        Ok(Self {
            worker,
            fingerprint,
            wrapped,
            key_bytes: header[1] as usize,
            cert_digest: Sha256::digest(cert).into(),
            message_digest,
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
    enroll_key(store, persona, publisher, cert, der, &[]).map(|(fingerprint, _)| fingerprint)
}

fn enroll_key(
    store: &mut Store,
    persona: Option<&str>,
    publisher: [u8; 32],
    cert: &[u8],
    der: &[u8],
    account_binding: &[u8],
) -> io::Result<(Fingerprint, arca_store::ItemId)> {
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
    let mut scope = fingerprint.to_vec();
    scope.extend_from_slice(&Sha256::digest(cert));
    scope.extend_from_slice(account_binding);
    let saved = store
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
    Ok((fingerprint, saved.id))
}

#[cfg(test)]
mod account_gate;

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

    fn reservation(uid: u32, selection: Selection) -> Vec<u8> {
        [
            uid.to_le_bytes().as_slice(),
            selection.encode().as_slice(),
            &wire::fixture_message(),
        ]
        .concat()
    }

    fn read_reply(client: &mut UnixStream) -> (u8, Vec<u8>) {
        let (status, body) = proto::read_reply(client).unwrap();
        (status, body.as_ref().to_vec())
    }

    fn socket_pair() -> (UnixStream, UnixStream) {
        let (server, client) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        (server, client)
    }

    fn configured_fixture(dir: &Path) -> State {
        let mut state = unlocked(dir);
        let registry = dir.join("app-registry");
        let pub_hex = "08".repeat(32);
        std::fs::write(
            &registry,
            format!(
                "51000 {SYNTHETIC_USER} org.atrium.mail {pub_hex} persona=work\n\
             51001 {SYNTHETIC_USER} org.atrium.mail {pub_hex}\n"
            ),
        )
        .unwrap();
        state.legacy_smime_registry = Some(registry);
        let root = body(|w| {
            w.str(SYNTHETIC_USER);
            w.raw(&ROOT);
        });
        assert_eq!(
            state.handle(&Label::Vestibulum, op::V_CREATE, &root).status,
            status::OK
        );
        state
    }

    fn worker_header(worker: &mut UnixStream) -> usize {
        worker.write_all(wire::WORKER_READY).unwrap();
        let mut mode = [0];
        worker.read_exact(&mut mode).unwrap();
        assert_eq!(mode, [wire::INPUT_RECEIVED]);
        assert_eq!(wire::read(worker, wire::MAX_CERT).unwrap(), CERT);
        let raw = wire::read(worker, wire::MAX_RECEIVED).unwrap();
        assert_eq!(raw, wire::fixture_message());
        let env = mail_smime::envelope::enveloped_data(CMS).unwrap();
        let (fingerprint, size, wrapped) =
            mail_smime::envelope::legacy_recipient(&env, CERT).unwrap();
        worker.write_all(&[1, size as u8]).unwrap();
        worker.write_all(&fingerprint).unwrap();
        wire::write(worker, &wrapped, 512).unwrap();
        worker.write_all(&wire::digest(&raw)).unwrap();
        size
    }

    #[test]
    fn synthetic_dispatch_is_disabled_by_default_and_unavailable_to_mail() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = unlocked(dir.path());
        let request = reservation(51000, Selection::fixture());
        let (ui, mut client) = socket_pair();
        let state_mutex = Mutex::new(state);
        serve_open(&state_mutex, &ui, &request, || {
            panic!("disabled gate launched")
        })
        .unwrap();
        assert_eq!(read_reply(&mut client), (status::DENIED, vec![]));
        state = state_mutex.into_inner().unwrap();
        let mail = Label::App {
            user: SYNTHETIC_USER.into(),
            publisher: PUB,
            app_id: "org.atrium.mail".into(),
            persona: Some("work".into()),
        };
        state.legacy_smime_registry = Some(dir.path().join("app-registry"));
        for operation in [op::U_SMIME_SYNTHETIC_RESERVE, op::U_SMIME_SYNTHETIC_OPEN] {
            assert_ne!(state.handle(&mail, operation, &request).status, status::OK);
        }
        serve_open(&Mutex::new(state), &ui, &request[..3], || {
            panic!("malformed gate launched")
        })
        .unwrap();
        assert_eq!(read_reply(&mut client), (status::DENIED, vec![]));
    }

    #[test]
    fn legacy_approval_refuses_account_message_and_digest_changes() {
        let dir = tempfile::tempdir().unwrap();
        let state = Mutex::new(configured_fixture(dir.path()));
        let selection = Selection::fixture();
        for changed in [
            Selection {
                account: 2,
                ..selection
            },
            Selection {
                message: 2,
                ..selection
            },
            Selection::for_message(
                selection.account,
                selection.message,
                b"changed synthetic bytes",
            ),
        ] {
            let (ui, mut client) = socket_pair();
            std::thread::scope(|scope| {
                let serve = scope.spawn(|| {
                    serve_open(&state, &ui, &reservation(51000, selection), || {
                        panic!("changed selection launched")
                    })
                });
                assert_eq!(read_reply(&mut client), (status::OK, vec![]));
                proto::write_request(&mut client, op::U_SMIME_SYNTHETIC_OPEN, &changed.encode())
                    .unwrap();
                assert_eq!(read_reply(&mut client), (status::DENIED, vec![]));
                serve.join().unwrap().unwrap();
            });
            assert!(state.lock().unwrap().legacy_smime_pending.is_none());
        }
    }

    #[test]
    fn legacy_reservation_refuses_changed_received_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let state = Mutex::new(configured_fixture(dir.path()));
        let (ui, mut client) = socket_pair();
        let mut changed = wire::fixture_message();
        changed[0] = b'f';
        for selection in [Selection::fixture(), Selection::for_message(1, 1, &changed)] {
            let mut request = 51000u32.to_le_bytes().to_vec();
            request.extend_from_slice(&selection.encode());
            request.extend_from_slice(&changed);
            serve_open(&state, &ui, &request, || {
                panic!("changed bytes started worker")
            })
            .unwrap();
            assert_eq!(read_reply(&mut client), (status::DENIED, vec![]));
        }
        assert!(state.lock().unwrap().legacy_smime_pending.is_none());
    }

    #[test]
    fn legacy_approval_cannot_move_to_another_connection_or_be_replayed() {
        let dir = tempfile::tempdir().unwrap();
        let state = Mutex::new(configured_fixture(dir.path()));
        let selection = Selection::fixture().encode();
        let (first_server, mut first) = socket_pair();
        let (second_server, mut second) = socket_pair();
        std::thread::scope(|scope| {
            let original =
                scope.spawn(|| crate::serve_conn(first_server, &Label::TrustedUi, &state));
            proto::write_request(
                &mut first,
                op::U_SMIME_SYNTHETIC_RESERVE,
                &reservation(51000, Selection::fixture()),
            )
            .unwrap();
            assert_eq!(read_reply(&mut first), (status::OK, vec![]));
            let foreign =
                scope.spawn(|| crate::serve_conn(second_server, &Label::TrustedUi, &state));
            proto::write_request(&mut second, op::U_SMIME_SYNTHETIC_OPEN, &selection).unwrap();
            assert_eq!(read_reply(&mut second), (status::DENIED, vec![]));
            drop(second);
            foreign.join().unwrap().unwrap();

            // An invalid second frame spends the reservation.
            proto::write_request(&mut first, op::U_SMIME_SYNTHETIC_OPEN, &selection[..12]).unwrap();
            assert_eq!(read_reply(&mut first), (status::DENIED, vec![]));
            original.join().unwrap().unwrap();
            assert!(proto::read_frame(&mut first).unwrap().is_none());
        });
    }

    #[test]
    fn signed_fixture_enrollment_requires_matching_work_approval() {
        let dir = tempfile::tempdir().unwrap();
        let state = Mutex::new(configured_fixture(dir.path()));
        let enrolled = || {
            let state = state.lock().unwrap();
            fixture_item(&state.stores[SYNTHETIC_USER], Some("work"), [8; 32]).unwrap()
        };
        let exchange = |uid: u32, action: Lifecycle, next: Option<Lifecycle>| {
            let (ui, mut client) = socket_pair();
            std::thread::scope(|scope| {
                let serve = scope.spawn(|| crate::serve_conn(ui, &Label::TrustedUi, &state));
                let mut body = uid.to_le_bytes().to_vec();
                body.extend_from_slice(&action.encode());
                proto::write_request(&mut client, op::U_SMIME_SYNTHETIC_RESERVE, &body).unwrap();
                let ack = read_reply(&mut client);
                let reply = if ack.0 == status::OK {
                    if let Some(next) = next {
                        proto::write_request(
                            &mut client,
                            op::U_SMIME_SYNTHETIC_MANAGE,
                            &next.encode(),
                        )
                        .unwrap();
                        read_reply(&mut client).0
                    } else {
                        drop(client);
                        status::OK
                    }
                } else {
                    ack.0
                };
                serve.join().unwrap().unwrap_or_else(|e| {
                    assert!(next.is_none(), "{e}");
                });
                reply
            })
        };
        use Lifecycle::{Enroll, Forget};
        assert_eq!(exchange(51001, Enroll, Some(Enroll)), status::DENIED);
        assert_eq!(exchange(51000, Enroll, None), status::OK);
        assert_eq!(exchange(51000, Enroll, Some(Forget)), status::DENIED);
        assert!(enrolled().is_none());
        assert_eq!(exchange(51000, Enroll, Some(Enroll)), status::OK);
        let first = enrolled().unwrap();
        assert_eq!(exchange(51000, Forget, None), status::OK);
        assert_eq!(enrolled(), Some(first));
        assert_eq!(exchange(51000, Forget, Some(Forget)), status::OK);
        assert!(enrolled().is_none());
        assert_eq!(exchange(51000, Enroll, Some(Enroll)), status::OK);
        assert_ne!(enrolled(), Some(first));
    }

    #[test]
    fn synthetic_failure_timing_samples_include_admission_and_worker_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let state = Mutex::new(configured_fixture(dir.path()));
        let selection = Selection::fixture();
        let refused = Selection {
            account: 2,
            ..selection
        };
        let mut samples = [Vec::new(), Vec::new()];
        for _ in 0..5 {
            for (index, requested) in [refused, selection].into_iter().enumerate() {
                let (mut ui, mut client) = socket_pair();
                let (control, mut worker) = socket_pair();
                let (lifetime, mut launcher) = socket_pair();
                lifetime
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                std::thread::scope(|scope| {
                    let serve = scope.spawn(|| {
                        let frame = proto::read_frame(&mut ui).unwrap().unwrap();
                        assert_eq!(
                            frame.as_ref()[..2],
                            op::U_SMIME_SYNTHETIC_RESERVE.to_le_bytes()
                        );
                        serve_open(&state, &ui, &frame.as_ref()[2..], || {
                            assert_eq!(requested, selection);
                            Ok((control, lifetime))
                        })
                        .unwrap();
                    });
                    let simulated_worker = (requested == selection).then(|| {
                        scope.spawn(|| {
                            worker_header(&mut worker);
                            assert_eq!(worker.read(&mut [0]).unwrap(), 0);
                            portcullis_ipc::write_response(
                                &mut launcher,
                                &Response::LaunchExit { code: Some(0) },
                            )
                            .unwrap();
                        })
                    });

                    let started = Instant::now();
                    proto::write_request(
                        &mut client,
                        op::U_SMIME_SYNTHETIC_RESERVE,
                        &reservation(51001, requested),
                    )
                    .unwrap();
                    let first = read_reply(&mut client);
                    if let Some(worker) = simulated_worker {
                        assert_eq!(first, (status::OK, vec![]));
                        proto::write_request(
                            &mut client,
                            op::U_SMIME_SYNTHETIC_OPEN,
                            &selection.encode(),
                        )
                        .unwrap();
                        let refused = read_reply(&mut client);
                        samples[index].push(started.elapsed().as_micros());
                        assert_eq!(refused, (status::DENIED, vec![0]));
                        worker.join().unwrap();
                    } else {
                        samples[index].push(started.elapsed().as_micros());
                        assert_eq!(first, (status::DENIED, vec![]));
                    }
                    serve.join().unwrap();
                });
            }
        }
        for (kind, mut readings) in ["admission", "worker-refusal"].into_iter().zip(samples) {
            readings.sort_unstable();
            println!(
                "LEGACY_FAILURE_TIMING class={kind} samples=5 min_us={} median_us={} max_us={}",
                readings[0], readings[2], readings[4]
            );
        }
    }

    enum Revoke {
        Lock,
        Disconnect,
        Registry,
    }

    fn revoke_during_worker_read(revoke: Revoke) {
        let dir = tempfile::tempdir().unwrap();
        let mut fixture = configured_fixture(dir.path());
        enroll_key(
            fixture.stores.get_mut(SYNTHETIC_USER).unwrap(),
            Some("work"),
            [8; 32],
            CERT,
            KEY,
            FIXTURE_SCOPE,
        )
        .unwrap();
        let registry = fixture.legacy_smime_registry.clone().unwrap();
        let state = Mutex::new(fixture);
        let user = body(|w| w.str(SYNTHETIC_USER));
        let root = body(|w| {
            w.str(SYNTHETIC_USER);
            w.raw(&ROOT);
        });
        let (ui, mut client) = socket_pair();
        let (control, mut worker) = UnixStream::pair().unwrap();
        let (lifetime, mut launcher) = UnixStream::pair().unwrap();
        let bound = Some(Duration::from_secs(5));
        worker.set_read_timeout(bound).unwrap();
        launcher.set_read_timeout(bound).unwrap();
        lifetime.set_read_timeout(bound).unwrap();

        std::thread::scope(|scope| {
            let read = scope.spawn(|| {
                serve_open(
                    &state,
                    &ui,
                    &reservation(51000, Selection::fixture()),
                    || Ok((control, lifetime)),
                )
            });
            assert_eq!(read_reply(&mut client), (status::OK, vec![]));
            proto::write_request(
                &mut client,
                op::U_SMIME_SYNTHETIC_OPEN,
                &Selection::fixture().encode(),
            )
            .unwrap();
            let size = worker_header(&mut worker);
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
                Revoke::Disconnect => client.shutdown(Shutdown::Both).unwrap(),
                Revoke::Registry => std::fs::write(&registry, "").unwrap(),
            }
            assert_eq!(worker.read(&mut [0]).unwrap(), 0);
            assert_eq!(launcher.read(&mut [0]).unwrap(), 0);
            let result = read.join().unwrap();
            if !matches!(revoke, Revoke::Disconnect) {
                result.unwrap();
                assert_eq!(read_reply(&mut client), (status::DENIED, vec![]));
            }
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

#[cfg(test)]
mod jmap_account_gate;
