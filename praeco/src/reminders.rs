//! Silent, transient reminder delivery. Only a waiting, registered shelf in
//! the poster's own persona may receive one. Ordinary history is independent.
use crate::proto::{self, op, status, Notification};
use aqueduct::{envelope, flag, Header};
use portcullis_peer::AppRegistry;
use std::collections::BTreeMap;
use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{mpsc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn fresh(due: u64) -> bool {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|now| u64::try_from(now.as_millis()).ok())
        .and_then(|now| now.checked_sub(due))
        .is_some_and(|age| age < 5000)
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Binding {
    pub owner: String,
    pub persona: Option<String>,
    pub app: String,
    signer: Option<[u8; 32]>,
}

fn binding(reg: &AppRegistry, uid: u32) -> Option<Binding> {
    let (owner, app) = reg.resolve(uid)?;
    if owner == proto::SYSTEM_USER || reg.is_session(uid) {
        return None;
    }
    Some(Binding {
        owner: owner.into(),
        app: app.into(),
        persona: reg.persona(uid)?.map(str::to_owned),
        signer: reg.publisher(uid)?,
    })
}

struct Waiter {
    binding: Binding,
    tx: mpsc::SyncSender<Notification>,
}

pub struct Mailbox {
    registry: String,
    waiters: Mutex<BTreeMap<u32, Waiter>>,
}

impl Mailbox {
    pub fn new(registry: String) -> Self {
        Self {
            registry,
            waiters: Mutex::new(BTreeMap::new()),
        }
    }

    pub(crate) fn binding(&self, uid: u32) -> Option<Binding> {
        binding(&AppRegistry::load(&self.registry).ok()?, uid)
    }

    pub(crate) fn wait(&self, uid: u32) -> Option<(Binding, mpsc::Receiver<Notification>)> {
        let who = self.binding(uid)?;
        let mut waiters = self.waiters.lock().ok()?;
        if waiters.contains_key(&uid) {
            return None;
        }
        let (tx, rx) = mpsc::sync_channel(1);
        waiters.insert(
            uid,
            Waiter {
                binding: who.clone(),
                tx,
            },
        );
        Some((who, rx))
    }

    pub(crate) fn finish(&self, uid: u32) {
        if let Ok(mut w) = self.waiters.lock() {
            w.remove(&uid);
        }
    }

    pub(crate) fn deliver(&self, uid: u32, id: u64, due: u64) -> u8 {
        let Ok(reg) = AppRegistry::load(&self.registry) else {
            return status::NOT_ALLOWED;
        };
        let Some(poster) = binding(&reg, uid) else {
            return status::NOT_ALLOWED;
        };
        let notice = Notification {
            id,
            uid,
            at_ms: due,
            app: poster.app.clone(),
            user: poster.owner.clone(),
            persona: poster.persona.clone(),
            urgency: proto::urgency::LOW,
            title: "Reminder".into(),
            body: String::new(),
            actions: vec![],
        };
        let Ok(waiters) = self.waiters.lock() else {
            return status::NOT_ALLOWED;
        };
        for (&receiver, w) in waiters.iter() {
            if w.binding.owner == poster.owner
                && w.binding.persona == poster.persona
                && binding(&reg, receiver).as_ref() == Some(&w.binding)
                && w.tx.try_send(notice.clone()).is_ok()
            {
                return status::OK;
            }
        }
        status::NOT_RUNNING
    }
}

fn request(sock: &Path, operation: u16, payload: &[u8]) -> io::Result<UnixStream> {
    let mut s = UnixStream::connect(sock)?;
    s.set_write_timeout(Some(Duration::from_secs(1)))?;
    s.set_read_timeout(Some(Duration::from_secs(2)))?;
    let len = u32::try_from(payload.len()).map_err(io::Error::other)?;
    envelope::write_message(
        &mut s,
        Header::new(proto::CLASS, operation, flag::RESPONSE_EXPECTED, len),
        payload,
    )?;
    let mut buffer = [0; 9];
    let (h, p) = envelope::read_message(&mut s, &mut buffer)?;
    if h.opcode_class != proto::CLASS
        || h.op != operation
        || h.flags != flag::IS_RESPONSE
        || p.len() != 9
        || p.first() != Some(&status::OK)
    {
        return Err(io::Error::other("Live reminder unavailable"));
    }
    Ok(s)
}

/// The original due time is the only caller-supplied field.
pub fn post(sock: &Path, due_ms: u64) -> io::Result<()> {
    request(sock, op::POST_REMINDER, &due_ms.to_le_bytes()).map(drop)
}

/// Register a single bounded wait. An empty result lets the shelf reconnect.
pub fn watch(sock: &Path, ready: impl FnOnce()) -> io::Result<Option<Notification>> {
    let mut s = request(sock, op::WAIT_REMINDER, &[])?;
    ready();
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut buffer = [0; 1024];
    let (h, p) = envelope::read_message(&mut s, &mut buffer)?;
    if h.opcode_class != proto::CLASS || h.op != op::REMINDER || h.flags != flag::ASYNC_EVENT {
        return Err(io::Error::other("Invalid private reminder event"));
    }
    if p.is_empty() {
        return Ok(None);
    }
    let n: Notification = postcard::from_bytes(p).map_err(io::Error::other)?;
    Ok(fresh(n.at_ms).then_some(n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::{Seek, Write};
    use std::os::unix::fs::OpenOptionsExt;

    #[test]
    fn a_waiter_rebound_to_another_persona_cannot_receive_the_original_offer() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("praeco-reminders-{}-{nonce}", std::process::id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap();
        let calendar = "501 alice org.atrium.calendar persona=work";
        writeln!(file, "{calendar}\n502 alice org.atrium.forum persona=work").unwrap();
        let mailbox = Mailbox::new(path.to_str().unwrap().to_owned());
        let (_, rx) = mailbox.wait(502).unwrap();
        assert!(mailbox.wait(503).is_none());
        assert!(mailbox.wait(502).is_none());

        file.rewind().unwrap();
        file.set_len(0).unwrap();
        writeln!(file, "{calendar}\n502 alice org.atrium.forum").unwrap();
        assert_eq!(mailbox.deliver(501, 1, 10), status::NOT_RUNNING);
        assert!(rx.try_recv().is_err());
        mailbox.finish(502);

        file.rewind().unwrap();
        file.set_len(0).unwrap();
        writeln!(file, "{calendar}\n502 alice org.atrium.forum persona=work").unwrap();
        let (_, rx) = mailbox.wait(502).unwrap();
        assert_eq!(mailbox.deliver(501, 2, 20), status::OK);
        let n = rx.try_recv().unwrap();
        assert_eq!(
            (n.id, n.at_ms, n.user.as_str(), n.persona.as_deref()),
            (2, 20, "alice", Some("work"))
        );
        assert!(n.body.is_empty() && n.actions.is_empty());
        std::fs::remove_file(path).unwrap();
    }
}
