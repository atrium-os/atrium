//! Disabled synthetic backend. The native tests use in-process replies.
//! The optional installed gate receives replies from one signed worker.
//! An approved account and its receipt-signing key are kept as one sealed
//! row in the synthetic user's Arca store, next to a revocation ledger.

use super::{closed, wire};
use arca_store::codec::{Reader, Writer};
use arca_store::{Access, Class, ItemId, ItemMeta, Kind, NewItem, Provenance, Recipient, Store};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use mail_proto::jmap;
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fmt;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

type Instance = [u8; 16];
const ORIGIN: &str = "https://jmap.fixture.invalid";
#[cfg(test)]
const MAIL: &str = "urn:ietf:params:jmap:mail";

#[derive(Clone, Debug, PartialEq, Eq)]
struct Binding {
    owner: Instance,
    persona: Instance,
    publisher: [u8; 32],
    credential: Instance,
    generation: u64,
}

struct Challenge {
    nonce: [u8; 32],
    binding: Binding,
    deadline: Instant,
}

struct WorkerSession<'a> {
    nonce: [u8; 32],
    binding: &'a Binding,
    effective_origin: &'a str,
    json: &'a [u8],
}

struct Enrollment {
    instance: Instance,
    binding: Binding,
    principal: String,
    session: jmap::Session,
}

impl Challenge {
    fn new(binding: Binding) -> io::Result<Self> {
        let mut nonce = [0; 32];
        getrandom::getrandom(&mut nonce).map_err(io::Error::other)?;
        Ok(Self {
            nonce,
            binding,
            deadline: Instant::now() + Duration::from_secs(30),
        })
    }

    fn approve(self, worker: WorkerSession<'_>, current: &Binding) -> io::Result<Enrollment> {
        if self.binding != *current
            || worker.binding != current
            || worker.nonce != self.nonce
            || Instant::now() >= self.deadline
            || worker.effective_origin != ORIGIN
        {
            return Err(closed());
        }
        unique_json(worker.json)?;
        let session = jmap::session(worker.json).map_err(|_| closed())?;
        let body: serde_json::Value = serde_json::from_slice(worker.json).map_err(|_| closed())?;
        let principal = body["username"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 254 && s.bytes().all(|b| b.is_ascii_graphic()))
            .ok_or_else(closed)?;
        if body["accounts"][&session.account]["isPersonal"].as_bool() != Some(true)
            || !same_origin(&session.api_url)
            || !same_origin(&session.download_url)
        {
            return Err(closed());
        }
        let mut instance = [0; 16];
        getrandom::getrandom(&mut instance).map_err(io::Error::other)?;
        Ok(Enrollment {
            instance,
            binding: self.binding,
            principal: principal.into(),
            session,
        })
    }
}

fn same_origin(url: &str) -> bool {
    url.strip_prefix(ORIGIN)
        .is_some_and(|suffix| suffix.starts_with('/') && !suffix.contains('\\'))
}

// The normal Mail parser is shared with sync. Check duplicate JSON members
// before using any values as account or message authority.
fn unique_json(body: &[u8]) -> io::Result<()> {
    struct Unique;
    impl<'de> Deserialize<'de> for Unique {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            deserializer.deserialize_any(Unique)
        }
    }
    impl<'de> Visitor<'de> for Unique {
        type Value = Self;
        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("JSON with unique object members")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self, M::Error> {
            let mut keys = HashSet::new();
            while let Some(key) = map.next_key::<String>()? {
                if !keys.insert(key) {
                    return Err(de::Error::custom("duplicate JSON member"));
                }
                map.next_value::<Unique>()?;
            }
            Ok(self)
        }
        fn visit_seq<S: SeqAccess<'de>>(self, mut seq: S) -> Result<Self, S::Error> {
            while seq.next_element::<Unique>()?.is_some() {}
            Ok(self)
        }
        fn visit_str<E: de::Error>(self, _: &str) -> Result<Self, E> {
            Ok(self)
        }
        fn visit_i64<E: de::Error>(self, _: i64) -> Result<Self, E> {
            Ok(self)
        }
        fn visit_u64<E: de::Error>(self, _: u64) -> Result<Self, E> {
            Ok(self)
        }
        fn visit_f64<E: de::Error>(self, _: f64) -> Result<Self, E> {
            Ok(self)
        }
        fn visit_bool<E: de::Error>(self, _: bool) -> Result<Self, E> {
            Ok(self)
        }
        fn visit_unit<E: de::Error>(self) -> Result<Self, E> {
            Ok(self)
        }
    }
    if body.len() > 1 << 20 {
        return Err(closed());
    }
    serde_json::from_slice::<Unique>(body)
        .map(|_| ())
        .map_err(|_| closed())
}

struct WorkerDownload<'a> {
    binding: &'a Binding,
    requested_email: &'a str,
    metadata: &'a [u8],
    effective_url: &'a str,
    bytes: &'a [u8],
}

struct Receipt {
    signature: Signature,
}

impl Enrollment {
    fn payload(&self, email: &str, bytes: &[u8]) -> Vec<u8> {
        let mut w = Writer::new();
        w.str("Atrium synthetic JMAP received message v1");
        w.raw(&self.binding.owner);
        w.raw(&self.binding.persona);
        w.raw(&self.binding.publisher);
        w.raw(&self.binding.credential);
        w.raw(&self.binding.generation.to_le_bytes());
        w.raw(&self.instance);
        w.str(ORIGIN);
        w.str(&self.principal);
        w.str(&self.session.account);
        w.str(email);
        w.raw(&wire::digest(bytes));
        w.buf
    }

    fn issue(
        &self,
        worker: WorkerDownload<'_>,
        fences: &Fences,
        key: &SigningKey,
    ) -> io::Result<Receipt> {
        fences.require_active(self)?;
        if worker.binding != &self.binding
            || worker.bytes.is_empty()
            || worker.bytes.len() > wire::MAX_RECEIVED
        {
            return Err(closed());
        }
        unique_json(worker.metadata)?;
        let answers = jmap::answers(worker.metadata).map_err(|_| closed())?;
        let emails = jmap::emails(
            &answers,
            &self.session.account,
            &[worker.requested_email.into()],
        )
        .map_err(|_| closed())?;
        let [email] = emails.as_slice() else {
            return Err(closed());
        };
        if email.id != worker.requested_email
            || self.session.download(&email.blob) != worker.effective_url
            || email.size != worker.bytes.len() as u64
        {
            return Err(closed());
        }
        let signature = key.sign(&self.payload(worker.requested_email, worker.bytes));
        Ok(Receipt { signature })
    }

    fn authorize(
        &self,
        current: &Binding,
        email: &str,
        bytes: &[u8],
        receipt: &Receipt,
        fences: &Fences,
        key: &VerifyingKey,
    ) -> io::Result<()> {
        fences.require_active(self)?;
        if current != &self.binding
            || email.len() > 255
            || bytes.is_empty()
            || bytes.len() > wire::MAX_RECEIVED
        {
            return Err(closed());
        }
        key.verify_strict(&self.payload(email, bytes), &receipt.signature)
            .map_err(|_| closed())
    }
}

// The fixture directory is private. Files are never removed from this
// monotonic revocation ledger. Missing ledger storage is an error, distinct
// from an absent fence within a verified directory.
struct Fences(PathBuf);

impl Fences {
    fn open(dir: &Path) -> io::Result<Self> {
        if !fs::symlink_metadata(dir)?.is_dir() {
            return Err(closed());
        }
        Ok(Self(dir.to_owned()))
    }

    // Only a first enrollment creates the ledger. Once a sealed account
    // row exists, a missing ledger refuses key use (see `Accounts::fences`).
    fn create(dir: &Path) -> io::Result<Self> {
        match DirBuilder::new().mode(0o700).create(dir) {
            Ok(()) => File::open(dir.parent().ok_or_else(closed)?)?.sync_all()?,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        Self::open(dir)
    }

    fn entry(&self, prefix: &str, id: Instance) -> PathBuf {
        let id: String = id.iter().map(|byte| format!("{byte:02x}")).collect();
        self.0.join(format!("{prefix}-{id}"))
    }

    fn fenced(&self, instance: Instance, persona: Instance) -> io::Result<bool> {
        Self::open(&self.0)?;
        for (prefix, id) in [("account", instance), ("persona", persona)] {
            match fs::symlink_metadata(self.entry(prefix, id)) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Ok(_) => return Ok(true),
                Err(error) => return Err(error),
            }
        }
        Ok(false)
    }

    fn require_active(&self, enrollment: &Enrollment) -> io::Result<()> {
        match self.fenced(enrollment.instance, enrollment.binding.persona)? {
            false => Ok(()),
            true => Err(closed()),
        }
    }

    // The fence is durable before cleanup starts, so a failed or
    // interrupted cleanup can never make the instance usable again.
    fn revoke(
        &self,
        prefix: &str,
        id: Instance,
        cleanup: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<()> {
        let entry = self.entry(prefix, id);
        let file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&entry)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => File::open(entry)?,
            Err(error) => return Err(error),
        };
        file.sync_all()?;
        File::open(&self.0)?.sync_all()?;
        cleanup()
    }
}

const RECORD: &str = "Atrium synthetic JMAP account v1";
const SESSION_MAX: usize = 4096;

impl Enrollment {
    // Everything that names the account. The value holds the signing seed
    // and the verified Session, so a reload checks them again.
    fn scope(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.str(RECORD);
        w.raw(&self.binding.owner);
        w.raw(&self.binding.persona);
        w.raw(&self.binding.publisher);
        w.raw(&self.binding.credential);
        w.u64(self.binding.generation);
        w.raw(&self.instance);
        w.str(&self.principal);
        w.str(&self.session.account);
        w.buf
    }
}

struct Row {
    item: ItemId,
    binding: Binding,
    instance: Instance,
    principal: String,
    account: String,
}

impl Row {
    fn parse(meta: &ItemMeta) -> Option<Self> {
        let mut r = Reader::new(&meta.scope);
        if r.str(RECORD.len()).ok()? != RECORD {
            return None;
        }
        let binding = Binding {
            owner: r.array().ok()?,
            persona: r.array().ok()?,
            publisher: r.array().ok()?,
            credential: r.array().ok()?,
            generation: r.u64().ok()?,
        };
        let instance = r.array().ok()?;
        let principal = r.str(254).ok()?.into();
        let account = r.str(255).ok()?.into();
        r.end().ok()?;
        Some(Self {
            item: meta.id,
            binding,
            instance,
            principal,
            account,
        })
    }

    fn same_account(&self, binding: &Binding, principal: &str, account: &str) -> bool {
        self.binding.owner == binding.owner
            && self.binding.persona == binding.persona
            && self.binding.publisher == binding.publisher
            && self.principal == principal
            && self.account == account
    }
}

/// An enrolled account with the key that signs its receipts.
struct Authority {
    item: ItemId,
    enrollment: Enrollment,
    key: SigningKey,
}

impl Authority {
    fn issue(&self, worker: WorkerDownload<'_>, fences: &Fences) -> io::Result<Receipt> {
        self.enrollment.issue(worker, fences, &self.key)
    }

    fn authorize(
        &self,
        current: &Binding,
        email: &str,
        bytes: &[u8],
        receipt: &Receipt,
        fences: &Fences,
    ) -> io::Result<()> {
        let verifier = self.key.verifying_key();
        self.enrollment
            .authorize(current, email, bytes, receipt, fences, &verifier)
    }

    // A public label for the gate: equal only for the same instance and key.
    fn tag(&self) -> [u8; 8] {
        let mut hash = Sha256::new();
        hash.update(b"Atrium synthetic JMAP authority tag v1");
        hash.update(self.enrollment.instance);
        hash.update(self.key.verifying_key().as_bytes());
        hash.finalize()[..8].try_into().unwrap()
    }
}

/// The sealed account rows of one store persona and Mail publisher.
struct Accounts<'a> {
    persona: &'a str,
    recipient: Recipient,
}

impl<'a> Accounts<'a> {
    fn new(persona: &'a str, publisher: [u8; 32]) -> Self {
        Self {
            persona,
            recipient: Recipient::SmimeLegacy { publisher },
        }
    }

    fn access(&self) -> Access<'_> {
        Access {
            persona: self.persona,
            recipient: &self.recipient,
        }
    }

    // Only this module writes Ed25519 rows for the legacy recipient, so a
    // row that does not parse is corruption, not something to skip. A
    // locked store hides its WhileUnlocked groups, so listing one would
    // look empty and a revocation would seem to have nothing to clean up.
    fn rows(&self, store: &Store) -> io::Result<Vec<Row>> {
        if store.state() != arca_store::LockState::Unlocked {
            return Err(closed());
        }
        store
            .list(&self.access())
            .map_err(|_| closed())?
            .iter()
            .filter(|meta| meta.kind == Kind::Ed25519)
            .map(|meta| Row::parse(meta).ok_or_else(closed))
            .collect()
    }

    // The ledger of a store that has never enrolled may be absent. Once a
    // row exists, revocation state must be readable before any key use.
    fn fences(&self, store: &Store, dir: &Path) -> io::Result<Fences> {
        match Fences::open(dir) {
            Err(error)
                if error.kind() == io::ErrorKind::NotFound && self.rows(store)?.is_empty() =>
            {
                Fences::create(dir)
            }
            other => other,
        }
    }

    fn fenced(&self, fences: &Fences, row: &Row) -> io::Result<bool> {
        fences.fenced(row.instance, row.binding.persona)
    }

    /// Resume cleanup of every revoked row (a retry after a failed or
    /// interrupted cleanup, possibly in an earlier process).
    fn cleanup(&self, store: &mut Store, fences: &Fences) -> io::Result<usize> {
        let mut removed = 0;
        for row in self.rows(store)? {
            if self.fenced(fences, &row)? {
                store
                    .delete(&row.item, &self.access())
                    .map_err(|_| closed())?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// The active enrollment for this authenticated account, if any. The
    /// same account under another credential or generation must be revoked
    /// before it can be used or enrolled again.
    fn load(
        &self,
        store: &Store,
        fences: &Fences,
        binding: &Binding,
        principal: &str,
        account: &str,
    ) -> io::Result<Option<Authority>> {
        let mut found = None;
        for row in self.rows(store)? {
            if !row.same_account(binding, principal, account) || self.fenced(fences, &row)? {
                continue;
            }
            if row.binding != *binding || found.is_some() {
                return Err(closed());
            }
            found = Some(row);
        }
        let Some(row) = found else {
            return Ok(None);
        };
        let (_, value) = store
            .release(&row.item, &self.access(), None)
            .map_err(|_| closed())?;
        let mut r = Reader::new(value.as_ref());
        let seed = Zeroizing::new(r.array::<32>().map_err(|_| closed())?);
        let json = r.bytes(SESSION_MAX).map_err(|_| closed())?;
        r.end().map_err(|_| closed())?;
        let session = jmap::session(json).map_err(|_| closed())?;
        if session.account != row.account
            || !same_origin(&session.api_url)
            || !same_origin(&session.download_url)
        {
            return Err(closed());
        }
        Ok(Some(Authority {
            item: row.item,
            enrollment: Enrollment {
                instance: row.instance,
                binding: row.binding,
                principal: row.principal,
                session,
            },
            key: SigningKey::from_bytes(&seed),
        }))
    }

    /// Seal a newly approved account with a fresh receipt-signing key.
    /// `session` is the exact Session body the approval verified.
    fn enroll(
        &self,
        store: &mut Store,
        fences: &Fences,
        enrollment: Enrollment,
        session: &[u8],
    ) -> io::Result<Authority> {
        fences.require_active(&enrollment)?;
        if session.len() > SESSION_MAX
            || jmap::session(session).ok().as_ref() != Some(&enrollment.session)
            || self
                .load(
                    store,
                    fences,
                    &enrollment.binding,
                    &enrollment.principal,
                    &enrollment.session.account,
                )?
                .is_some()
        {
            return Err(closed());
        }
        let mut seed = Zeroizing::new([0; 32]);
        getrandom::getrandom(seed.as_mut()).map_err(io::Error::other)?;
        let mut value = Zeroizing::new(Vec::with_capacity(36 + session.len()));
        value.extend_from_slice(seed.as_ref());
        value.extend_from_slice(&(session.len() as u32).to_le_bytes());
        value.extend_from_slice(session);
        let saved = store
            .add(NewItem {
                persona: self.persona,
                recipient: self.recipient.clone(),
                class: Class::WhileUnlocked,
                kind: Kind::Ed25519,
                provenance: Provenance::ArcaGenerated,
                account: "Synthetic JMAP account",
                scope: &enrollment.scope(),
                value: &value,
            })
            .map_err(|_| closed())?;
        Ok(Authority {
            item: saved.id,
            enrollment,
            key: SigningKey::from_bytes(&seed),
        })
    }

    /// Fence one account instance, then delete its sealed row. Repeating
    /// it is harmless; a cleanup failure leaves the fence in place.
    fn revoke_account(&self, store: &mut Store, fences: &Fences, instance: Instance) -> io::Result<()> {
        fences.revoke("account", instance, || {
            for row in self.rows(store)? {
                if row.instance == instance {
                    store
                        .delete(&row.item, &self.access())
                        .map_err(|_| closed())?;
                }
            }
            Ok(())
        })
    }

    /// Fence a persona instance, then delete every account row it holds.
    /// `PERSONA_DESTROY` does not call this yet; the signed fixture has one
    /// fixed persona instance.
    #[cfg_attr(not(test), allow(dead_code))]
    fn revoke_persona(&self, store: &mut Store, fences: &Fences, persona: Instance) -> io::Result<()> {
        fences.revoke("persona", persona, || {
            for row in self.rows(store)? {
                if row.binding.persona == persona {
                    store
                        .delete(&row.item, &self.access())
                        .map_err(|_| closed())?;
                }
            }
            Ok(())
        })
    }

    /// True while the row is still sealed and not fenced.
    fn live(&self, store: &Store, fences: &Fences, authority: &Authority) -> io::Result<bool> {
        Ok(self.rows(store)?.iter().any(|row| row.item == authority.item)
            && !fences.fenced(authority.enrollment.instance, authority.enrollment.binding.persona)?)
    }
}

#[cfg(test)]
fn binding(persona: u8, credential: u8) -> Binding {
    Binding {
        owner: [1; 16],
        persona: [persona; 16],
        publisher: [7; 32],
        credential: [credential; 16],
        generation: 1,
    }
}

#[cfg(test)]
fn session(account: &str, personal: bool) -> String {
    serde_json::json!({
        "username": "login@fixture.invalid",
        "capabilities": {"urn:ietf:params:jmap:core": {}, MAIL: {}},
        "accounts": {(account): {"name": "display only", "isPersonal": personal,
                                  "accountCapabilities": {MAIL: {}}}},
        "primaryAccounts": {MAIL: account},
        "apiUrl": format!("{ORIGIN}/api"),
        "downloadUrl": format!("{ORIGIN}/download/{{accountId}}/{{blobId}}/{{name}}"),
        "uploadUrl": format!("{ORIGIN}/upload/{{accountId}}"),
        "state": "s"
    })
    .to_string()
}

#[cfg(test)]
fn proof<'a>(challenge: &Challenge, binding: &'a Binding, json: &'a str) -> WorkerSession<'a> {
    WorkerSession {
        nonce: challenge.nonce,
        binding,
        effective_origin: ORIGIN,
        json: json.as_bytes(),
    }
}

#[cfg(test)]
fn enrolled(binding: Binding, account: &str) -> Enrollment {
    let challenge = Challenge::new(binding.clone()).unwrap();
    let body = session(account, true);
    let reply = proof(&challenge, &binding, &body);
    challenge.approve(reply, &binding).unwrap()
}

#[cfg(test)]
fn email_reply(account: &str, email: &str, bytes: &[u8]) -> String {
    serde_json::json!({"methodResponses": [["Email/get", {
        "accountId": account, "state": "s", "notFound": [],
        "list": [{"id": email, "blobId": "blob", "mailboxIds": {"inbox": true},
                  "keywords": {}, "size": bytes.len()}]
    }, "g"]]})
    .to_string()
}

#[cfg(test)]
fn received(enrollment: &Enrollment, fences: &Fences, key: &SigningKey, bytes: &[u8]) -> Receipt {
    enrollment
        .issue(
            WorkerDownload {
                binding: &enrollment.binding,
                requested_email: "message",
                metadata: email_reply(&enrollment.session.account, "message", bytes).as_bytes(),
                effective_url: &enrollment.session.download("blob"),
                bytes,
            },
            fences,
            key,
        )
        .unwrap()
}

#[test]
fn jmap_legacy_proof_binds_account_challenge_and_credential() {
    let current = binding(1, 1);
    let account = enrolled(current.clone(), "a1");
    assert_eq!(account.principal, "login@fixture.invalid");
    assert_eq!(account.session.account, "a1");
    for (field, invalid) in [
        ("shared", session("a1", false)),
        (
            "ambiguous",
            session("a1", true).replacen("{", "{\"username\":\"other\",", 1),
        ),
        (
            "origin",
            session("a1", true).replace("/api", ".example/api"),
        ),
    ] {
        let challenge = Challenge::new(current.clone()).unwrap();
        let reply = proof(&challenge, &current, &invalid);
        assert!(challenge.approve(reply, &current).is_err(), "{field}");
    }
    let challenge = Challenge::new(current.clone()).unwrap();
    let body = session("a1", true);
    let mut reply = proof(&challenge, &current, &body);
    reply.nonce[0] ^= 1;
    assert!(challenge.approve(reply, &current).is_err());
    let mut challenge = Challenge::new(current.clone()).unwrap();
    challenge.deadline = Instant::now();
    let reply = proof(&challenge, &current, &body);
    assert!(challenge.approve(reply, &current).is_err());
    let challenge = Challenge::new(current.clone()).unwrap();
    let reply = proof(&challenge, &current, &body);
    let changed = Binding {
        generation: 2,
        ..current.clone()
    };
    assert!(challenge.approve(reply, &changed).is_err());
}

#[test]
fn jmap_legacy_receipt_binds_fetched_account_id_and_exact_bytes() {
    let root = tempfile::tempdir().unwrap();
    let fences = Fences::open(root.path()).unwrap();
    let key = SigningKey::from_bytes(&[5; 32]);
    let work = enrolled(binding(1, 1), "a1");
    let other = enrolled(binding(1, 2), "a2");
    let personal = enrolled(binding(2, 1), "a1");
    let bytes = wire::fixture_message();
    let receipt = received(&work, &fences, &key, &bytes);
    let verifier = key.verifying_key();
    let check = |enrollment: &Enrollment, email, bytes| {
        enrollment.authorize(
            &enrollment.binding,
            email,
            bytes,
            &receipt,
            &fences,
            &verifier,
        )
    };
    assert!(check(&work, "message", &bytes).is_ok());
    assert!(check(&other, "message", &bytes).is_err());
    assert!(check(&personal, "message", &bytes).is_err());
    assert!(check(&work, "other", &bytes).is_err());
    assert!(check(&work, "message", &wire::unsupported_fixture_message()).is_err());
    let fabricated = Receipt {
        signature: key.sign(b"locally imported"),
    };
    assert!(work
        .authorize(
            &work.binding,
            "message",
            &bytes,
            &fabricated,
            &fences,
            &verifier
        )
        .is_err());
    for (account, email, url) in [
        ("a2", "message", work.session.download("blob")),
        ("a1", "other", work.session.download("blob")),
        ("a1", "message", work.session.download("other")),
    ] {
        assert!(work
            .issue(
                WorkerDownload {
                    binding: &work.binding,
                    requested_email: "message",
                    metadata: email_reply(account, email, &bytes).as_bytes(),
                    effective_url: &url,
                    bytes: &bytes,
                },
                &fences,
                &key
            )
            .is_err());
    }
}

#[test]
fn jmap_legacy_account_revocation_survives_failed_cleanup_and_reopen() {
    let root = tempfile::tempdir().unwrap();
    let fences = Fences::open(root.path()).unwrap();
    let key = SigningKey::from_bytes(&[5; 32]);
    let work = enrolled(binding(1, 1), "a1");
    let other = enrolled(binding(1, 2), "a2");
    let bytes = wire::fixture_message();
    let receipt = received(&work, &fences, &key, &bytes);
    let check = |fences: &Fences| {
        work.authorize(
            &work.binding,
            "message",
            &bytes,
            &receipt,
            fences,
            &key.verifying_key(),
        )
    };
    assert!(check(&fences).is_ok());
    let key_material = root.path().join("synthetic-sealed-row");
    fs::write(&key_material, b"fixture").unwrap();
    assert!(fences
        .revoke("account", work.instance, || {
            assert!(check(&fences).is_err());
            Err(io::Error::other("interrupted cleanup"))
        })
        .is_err());
    assert!(key_material.is_file());
    drop(fences);
    let fences = Fences::open(root.path()).unwrap();
    assert!(check(&fences).is_err());
    let new_work = enrolled(work.binding.clone(), "a1");
    assert!(new_work
        .authorize(
            &new_work.binding,
            "message",
            &bytes,
            &receipt,
            &fences,
            &key.verifying_key()
        )
        .is_err());
    fences
        .revoke("account", work.instance, || fs::remove_file(&key_material))
        .unwrap();
    fences.revoke("account", work.instance, || Ok(())).unwrap();
    assert!(!key_material.exists());
    assert!(check(&fences).is_err());
    received(&new_work, &fences, &key, &bytes);
    received(&other, &fences, &key, &bytes);
}

#[test]
fn jmap_legacy_persona_revocation_isolated_across_reopen_and_recreation() {
    let root = tempfile::tempdir().unwrap();
    let fences = Fences::open(root.path()).unwrap();
    let key = SigningKey::from_bytes(&[5; 32]);
    let work = enrolled(binding(1, 1), "a1");
    let other_work = enrolled(binding(1, 2), "a2");
    let personal = enrolled(binding(2, 1), "a1");
    let bytes = wire::fixture_message();
    let receipt = received(&work, &fences, &key, &bytes);
    assert!(fences
        .revoke("persona", work.binding.persona, || Err(closed()))
        .is_err());
    drop(fences);
    let fences = Fences::open(root.path()).unwrap();
    assert!(fences.require_active(&work).is_err());
    assert!(fences.require_active(&other_work).is_err());
    received(&personal, &fences, &key, &bytes);
    let recreated = enrolled(binding(3, 1), "a1");
    received(&recreated, &fences, &key, &bytes);
    assert!(recreated
        .authorize(
            &recreated.binding,
            "message",
            &bytes,
            &receipt,
            &fences,
            &key.verifying_key()
        )
        .is_err());
}

#[cfg(test)]
fn approved(binding: Binding, account: &str) -> (Enrollment, String) {
    let challenge = Challenge::new(binding.clone()).unwrap();
    let body = session(account, true);
    let reply = proof(&challenge, &binding, &body);
    let enrollment = challenge.approve(reply, &binding).unwrap();
    (enrollment, body)
}

#[cfg(test)]
fn sealed_store(dir: &Path) -> Store {
    let root = arca_store::RootKey::from_bytes(&[4; 32]).unwrap();
    match Store::open(dir, "alice") {
        Ok(mut store) => {
            store.unlock(&root).unwrap();
            store
        }
        Err(_) => Store::create(dir, "alice", &root).unwrap(),
    }
}

#[cfg(test)]
fn download(authority: &Authority, fences: &Fences, bytes: &[u8]) -> io::Result<Receipt> {
    let enrollment = &authority.enrollment;
    authority.issue(
        WorkerDownload {
            binding: &enrollment.binding,
            requested_email: "message",
            metadata: email_reply(&enrollment.session.account, "message", bytes).as_bytes(),
            effective_url: &enrollment.session.download("blob"),
            bytes,
        },
        fences,
    )
}

#[test]
fn jmap_legacy_account_and_receipt_authority_survive_store_reopen() {
    let root = tempfile::tempdir().unwrap();
    let ledger = root.path().join("fences");
    let accounts = Accounts::new("work", [7; 32]);
    let bytes = wire::fixture_message();
    let mut store = sealed_store(&root.path().join("store"));
    let fences = accounts.fences(&store, &ledger).unwrap();
    let (enrollment, body) = approved(binding(1, 1), "a1");
    let authority = accounts.enroll(&mut store, &fences, enrollment, body.as_bytes()).unwrap();
    let receipt = download(&authority, &fences, &bytes).unwrap();
    let (tag, instance) = (authority.tag(), authority.enrollment.instance);
    // A second approval of the same account reuses, never duplicates.
    let (again, body) = approved(binding(1, 1), "a1");
    assert!(accounts.enroll(&mut store, &fences, again, body.as_bytes()).is_err());
    // The Session that is sealed must be the one that was verified.
    let (other, _) = approved(binding(1, 1), "a2");
    assert!(accounts
        .enroll(&mut store, &fences, other, session("a1", true).as_bytes())
        .is_err());
    drop((authority, fences, store));

    let mut store = sealed_store(&root.path().join("store"));
    let fences = accounts.fences(&store, &ledger).unwrap();
    let loaded = accounts
        .load(&store, &fences, &binding(1, 1), "login@fixture.invalid", "a1")
        .unwrap()
        .unwrap();
    assert_eq!((loaded.tag(), loaded.enrollment.instance), (tag, instance));
    assert_eq!(loaded.enrollment.session.account, "a1");
    assert!(accounts.live(&store, &fences, &loaded).unwrap());
    // A receipt issued before the restart still authorizes the exact bytes.
    loaded
        .authorize(&binding(1, 1), "message", &bytes, &receipt, &fences)
        .unwrap();
    assert!(loaded
        .authorize(&binding(1, 1), "message", &wire::unsupported_fixture_message(), &receipt, &fences)
        .is_err());
    assert_eq!(download(&loaded, &fences, &bytes).unwrap().signature, receipt.signature);
    // Another account in the same persona gets its own instance and key.
    let (second, body) = approved(binding(1, 2), "a2");
    let second = accounts.enroll(&mut store, &fences, second, body.as_bytes()).unwrap();
    assert_ne!(second.tag(), tag);
    assert!(second
        .authorize(&second.enrollment.binding, "message", &bytes, &receipt, &fences)
        .is_err());
    // Other personas and publishers see none of these rows.
    assert!(Accounts::new("school", [7; 32]).rows(&store).unwrap().is_empty());
    assert!(Accounts::new("work", [8; 32]).rows(&store).unwrap().is_empty());
    assert_eq!(accounts.rows(&store).unwrap().len(), 2);
}

#[test]
fn jmap_legacy_sealed_account_revocation_survives_restart_and_reenrollment_recovers() {
    let root = tempfile::tempdir().unwrap();
    let ledger = root.path().join("fences");
    let accounts = Accounts::new("work", [7; 32]);
    let bytes = wire::fixture_message();
    let mut store = sealed_store(&root.path().join("store"));
    let fences = accounts.fences(&store, &ledger).unwrap();
    let (enrollment, body) = approved(binding(1, 1), "a1");
    let old = accounts.enroll(&mut store, &fences, enrollment, body.as_bytes()).unwrap();
    let receipt = download(&old, &fences, &bytes).unwrap();
    // A locked store makes the cleanup fail after the fence is durable.
    store.lock();
    assert!(accounts
        .revoke_account(&mut store, &fences, old.enrollment.instance)
        .is_err());
    assert!(old.authorize(&binding(1, 1), "message", &bytes, &receipt, &fences).is_err());
    assert!(download(&old, &fences, &bytes).is_err());
    drop((fences, store));

    let mut store = sealed_store(&root.path().join("store"));
    let fences = accounts.fences(&store, &ledger).unwrap();
    // The sealed row outlived the failed cleanup but stays unusable.
    assert_eq!(accounts.rows(&store).unwrap().len(), 1);
    assert!(!accounts.live(&store, &fences, &old).unwrap());
    assert!(accounts
        .load(&store, &fences, &binding(1, 1), "login@fixture.invalid", "a1")
        .unwrap()
        .is_none());
    assert_eq!(accounts.cleanup(&mut store, &fences).unwrap(), 1);
    assert!(accounts.rows(&store).unwrap().is_empty());
    accounts
        .revoke_account(&mut store, &fences, old.enrollment.instance)
        .unwrap();

    // Re-adding the same mailbox is a new enrollment with a new key.
    let (enrollment, body) = approved(binding(1, 1), "a1");
    let new = accounts.enroll(&mut store, &fences, enrollment, body.as_bytes()).unwrap();
    assert_ne!(new.tag(), old.tag());
    assert!(new.authorize(&binding(1, 1), "message", &bytes, &receipt, &fences).is_err());
    let fresh = download(&new, &fences, &bytes).unwrap();
    drop((fences, store));
    let store = sealed_store(&root.path().join("store"));
    let fences = accounts.fences(&store, &ledger).unwrap();
    let loaded = accounts
        .load(&store, &fences, &binding(1, 1), "login@fixture.invalid", "a1")
        .unwrap()
        .unwrap();
    assert_eq!(loaded.tag(), new.tag());
    loaded.authorize(&binding(1, 1), "message", &bytes, &fresh, &fences).unwrap();
    assert!(old.authorize(&binding(1, 1), "message", &bytes, &receipt, &fences).is_err());
}

#[test]
fn jmap_legacy_sealed_persona_revocation_and_credential_change_need_revocation() {
    let root = tempfile::tempdir().unwrap();
    let ledger = root.path().join("fences");
    let accounts = Accounts::new("work", [7; 32]);
    let bytes = wire::fixture_message();
    let mut store = sealed_store(&root.path().join("store"));
    let fences = accounts.fences(&store, &ledger).unwrap();
    let (work, body) = approved(binding(1, 1), "a1");
    let work = accounts.enroll(&mut store, &fences, work, body.as_bytes()).unwrap();
    let (recreated, body) = approved(binding(3, 1), "a1");
    let recreated = accounts.enroll(&mut store, &fences, recreated, body.as_bytes()).unwrap();

    // A changed credential for the same account refuses until revoked.
    for changed in [binding(1, 2), Binding { generation: 2, ..binding(1, 1) }] {
        assert!(accounts
            .load(&store, &fences, &changed, "login@fixture.invalid", "a1")
            .is_err());
        let (approval, body) = approved(changed, "a1");
        assert!(accounts.enroll(&mut store, &fences, approval, body.as_bytes()).is_err());
    }

    accounts
        .revoke_persona(&mut store, &fences, work.enrollment.binding.persona)
        .unwrap();
    drop((fences, store));
    let mut store = sealed_store(&root.path().join("store"));
    let fences = accounts.fences(&store, &ledger).unwrap();
    assert!(!accounts.live(&store, &fences, &work).unwrap());
    let (approval, body) = approved(binding(1, 2), "a1");
    assert!(accounts.enroll(&mut store, &fences, approval, body.as_bytes()).is_err());
    // A new persona instance with the same name stays available.
    let kept = accounts
        .load(&store, &fences, &binding(3, 1), "login@fixture.invalid", "a1")
        .unwrap()
        .unwrap();
    assert_eq!(kept.tag(), recreated.tag());
    download(&kept, &fences, &bytes).unwrap();
    assert_eq!(accounts.rows(&store).unwrap().len(), 1);
}

#[test]
fn jmap_legacy_sealed_account_refuses_without_its_revocation_ledger() {
    let root = tempfile::tempdir().unwrap();
    let ledger = root.path().join("fences");
    let accounts = Accounts::new("work", [7; 32]);
    let mut store = sealed_store(&root.path().join("store"));
    let fences = accounts.fences(&store, &ledger).unwrap();
    assert!(ledger.is_dir());
    let (enrollment, body) = approved(binding(1, 1), "a1");
    accounts.enroll(&mut store, &fences, enrollment, body.as_bytes()).unwrap();
    fs::remove_dir(&ledger).unwrap();
    assert!(accounts.fences(&store, &ledger).is_err());
    assert!(accounts
        .load(&store, &fences, &binding(1, 1), "login@fixture.invalid", "a1")
        .is_err());
    // A ledger replaced by a link is not a ledger.
    std::os::unix::fs::symlink(root.path(), &ledger).unwrap();
    assert!(accounts.fences(&store, &ledger).is_err());
}

/// Witness codes for the public synthetic result.
#[cfg(all(target_os = "freebsd", feature = "legacy-jmap-worker"))]
mod witness {
    pub const NEW: u8 = 0;
    pub const WRONG_ACCOUNT_REFUSED: u8 = 1;
    pub const TLS_NAME_REFUSED: u8 = 2;
    pub const REVOKED: u8 = 3;
    pub const KEPT: u8 = 4;
}

#[cfg(all(target_os = "freebsd", feature = "legacy-jmap-worker"))]
pub fn serve_jmap_proof(
    state: &std::sync::Mutex<super::State>,
    mut client: &std::os::unix::net::UnixStream,
    body: &[u8],
) -> io::Result<()> {
    use super::{launch_worker, op, proto, signed_persona, status, Label, SYNTHETIC_USER};
    use portcullis_ipc::Response;

    const LEDGER: &str = "legacy-jmap-fences";
    let run = || -> io::Result<Vec<u8>> {
        let [a, b, c, d, mode @ (0..=3)] = *body else {
            return Err(closed());
        };
        let uid = u32::from_le_bytes([a, b, c, d]);
        let lock = || state.lock().unwrap_or_else(|p| p.into_inner());
        let unlocked = |state: &super::State| {
            state
                .stores
                .get(SYNTHETIC_USER)
                .is_some_and(|store| store.state() == arca_store::LockState::Unlocked)
        };
        let (registry, publisher, ledger) = {
            let state = lock();
            if !unlocked(&state) {
                return Err(closed());
            }
            (
                state.legacy_smime_registry.clone().ok_or_else(closed)?,
                state.mail_sign_ins.mail_publisher.ok_or_else(closed)?,
                state.dir.join(LEDGER),
            )
        };
        let live = || {
            signed_persona(&registry, uid, publisher)
                .as_ref()
                .and_then(|p| p.as_deref())
                == Some("work")
        };
        if !live() {
            return Err(closed());
        }

        // Every authority field is selected by Arca for this lab account.
        let binding = Binding {
            owner: [1; 16],
            persona: [2; 16],
            publisher,
            credential: [3; 16],
            generation: 1,
        };
        let accounts = Accounts::new("work", publisher);

        // Revocation never launches the worker or needs the provider.
        if mode == witness::REVOKED {
            let mut state = lock();
            if !live() || !unlocked(&state) {
                return Err(closed());
            }
            let store = state.stores.get_mut(SYNTHETIC_USER).ok_or_else(closed)?;
            let fences = accounts.fences(store, &ledger)?;
            let mut revoked = 0u8;
            for row in accounts.rows(store)? {
                if row.binding.owner == binding.owner
                    && row.binding.persona == binding.persona
                    && !accounts.fenced(&fences, &row)?
                {
                    accounts.revoke_account(store, &fences, row.instance)?;
                    revoked = revoked.saturating_add(1);
                }
            }
            accounts.cleanup(store, &fences)?;
            if !accounts.rows(store)?.is_empty() {
                return Err(closed());
            }
            return Ok(vec![witness::REVOKED, revoked]);
        }

        let challenge = Challenge::new(binding.clone())?;
        let (mut control, mut lifetime) = launch_worker("org.atrium.jmap-proof-fixture")?;
        let data = (|| {
            control.set_read_timeout(Some(Duration::from_secs(7)))?;
            control.set_write_timeout(Some(Duration::from_secs(2)))?;
            let mut request = challenge.nonce.to_vec();
            request.push(mode);
            wire::write(&mut control, &request, 33)?;
            wire::read(&mut control, 64 << 10)
        })();
        // Closing the private channel does not release the launch lifetime.
        drop(control);
        if !matches!(
            portcullis_ipc::read_response(&mut lifetime)?,
            Response::LaunchExit { code: Some(0) }
        ) || !live()
        {
            return Err(closed());
        }
        let data = data?;
        if mode == witness::TLS_NAME_REFUSED {
            return if data == [2] {
                Ok(vec![witness::TLS_NAME_REFUSED])
            } else {
                Err(closed())
            };
        }
        let mut reader = Reader::new(&data);
        let parsed = (|| {
            let nonce = reader.array::<32>()?;
            let origin = reader.str(128)?;
            let session = reader.bytes(SESSION_MAX)?;
            let metadata = reader.bytes(4096)?;
            let url = reader.str(512)?;
            let bytes = reader.bytes(wire::MAX_RECEIVED)?;
            reader.end()?;
            Ok::<_, arca_store::Error>((nonce, origin, session, metadata, url, bytes))
        })()
        .map_err(|_| closed())?;
        let (nonce, origin, session, metadata, url, bytes) = parsed;
        let enrollment = challenge.approve(
            WorkerSession {
                nonce,
                binding: &binding,
                effective_origin: origin,
                json: session,
            },
            &binding,
        )?;
        if enrollment.session.account != "a1" || bytes != wire::fixture_message() {
            return Err(closed());
        }

        // Resume any interrupted revocation, then use the sealed account
        // or seal this approval with a new receipt key.
        let (authority, code) = {
            let mut state = lock();
            if !live() || !unlocked(&state) {
                return Err(closed());
            }
            let store = state.stores.get_mut(SYNTHETIC_USER).ok_or_else(closed)?;
            let fences = accounts.fences(store, &ledger)?;
            accounts.cleanup(store, &fences)?;
            match accounts.load(
                store,
                &fences,
                &binding,
                &enrollment.principal,
                &enrollment.session.account,
            )? {
                Some(authority) => (authority, witness::KEPT),
                None => (
                    accounts.enroll(store, &fences, enrollment, session)?,
                    witness::NEW,
                ),
            }
        };
        let fences = Fences::open(&ledger)?;
        let issue = authority.issue(
            WorkerDownload {
                binding: &binding,
                requested_email: "message",
                metadata,
                effective_url: url,
                bytes,
            },
            &fences,
        );
        let state = lock();
        if !live() || !unlocked(&state) {
            return Err(closed());
        }
        let store = state.stores.get(SYNTHETIC_USER).ok_or_else(closed)?;
        if !accounts.live(store, &fences, &authority)? {
            return Err(closed());
        }
        if mode == witness::WRONG_ACCOUNT_REFUSED {
            let answers = jmap::answers(metadata).map_err(|_| closed())?;
            jmap::emails(&answers, "substituted-account", &["message".into()])
                .map_err(|_| closed())?;
            return if issue.is_err() {
                Ok(vec![witness::WRONG_ACCOUNT_REFUSED])
            } else {
                Err(closed())
            };
        }
        let receipt = issue?;
        authority.authorize(&binding, "message", bytes, &receipt, &fences)?;
        if authority
            .authorize(
                &binding,
                "message",
                &wire::unsupported_fixture_message(),
                &receipt,
                &fences,
            )
            .is_ok()
        {
            return Err(closed());
        }
        // This public synthetic witness carries neither the receipt nor key,
        // only whether the account was sealed now or kept, and a tag that
        // matches only the same instance and key.
        let mut reply = vec![code];
        reply.extend_from_slice(&authority.tag());
        Ok(reply)
    };
    let result = run();
    let (code, witness) = match result {
        Ok(witness) => (status::OK, witness),
        Err(_) => (status::DENIED, Vec::new()),
    };
    state.lock().unwrap_or_else(|p| p.into_inner()).audit(
        &Label::TrustedUi,
        op::U_SMIME_SYNTHETIC_JMAP,
        None,
        code,
    );
    proto::write_reply(&mut client, code, &witness)
}
