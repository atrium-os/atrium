//! Disabled synthetic backend. The native tests use in-process replies.
//! The optional installed gate receives replies from one signed worker.

use super::{closed, wire};
use arca_store::codec::Writer;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use mail_proto::jmap;
use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use std::collections::HashSet;
use std::fmt;
use std::fs;
#[cfg(test)]
use std::fs::{File, OpenOptions};
use std::io;
#[cfg(test)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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
        if !fs::metadata(dir)?.is_dir() {
            return Err(closed());
        }
        Ok(Self(dir.to_owned()))
    }

    fn entry(&self, prefix: &str, id: Instance) -> PathBuf {
        let id: String = id.iter().map(|byte| format!("{byte:02x}")).collect();
        self.0.join(format!("{prefix}-{id}"))
    }

    fn require_active(&self, enrollment: &Enrollment) -> io::Result<()> {
        Self::open(&self.0)?;
        for (prefix, id) in [
            ("account", enrollment.instance),
            ("persona", enrollment.binding.persona),
        ] {
            match fs::symlink_metadata(self.entry(prefix, id)) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Ok(_) => return Err(closed()),
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    #[cfg(test)]
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

#[cfg(all(target_os = "freebsd", feature = "legacy-jmap-worker"))]
pub fn serve_jmap_proof(
    state: &std::sync::Mutex<super::State>,
    mut client: &std::os::unix::net::UnixStream,
    body: &[u8],
) -> io::Result<()> {
    use super::{launch_worker, op, proto, signed_persona, status, Label, SYNTHETIC_USER};
    use arca_store::codec::Reader;
    use portcullis_ipc::Response;

    let run = || -> io::Result<u8> {
        let [a, b, c, d, mode @ (0 | 1 | 2)] = *body else {
            return Err(closed());
        };
        let uid = u32::from_le_bytes([a, b, c, d]);
        let lock = || state.lock().unwrap_or_else(|p| p.into_inner());
        let (registry, publisher, ledger) = {
            let state = lock();
            if !state
                .stores
                .get(SYNTHETIC_USER)
                .is_some_and(|store| store.state() == arca_store::LockState::Unlocked)
            {
                return Err(closed());
            }
            (
                state.legacy_smime_registry.clone().ok_or_else(closed)?,
                state.mail_sign_ins.mail_publisher.ok_or_else(closed)?,
                state.dir.clone(),
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
        if mode == 2 {
            return if data == [2] { Ok(2) } else { Err(closed()) };
        }
        let mut reader = Reader::new(&data);
        let parsed = (|| {
            let nonce = reader.array::<32>()?;
            let origin = reader.str(128)?;
            let session = reader.bytes(4096)?;
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
        let fences = Fences::open(&ledger)?;
        let mut seed = zeroize::Zeroizing::new([0; 32]);
        getrandom::getrandom(seed.as_mut()).map_err(io::Error::other)?;
        let key = SigningKey::from_bytes(&seed);
        let issue = enrollment.issue(
            WorkerDownload {
                binding: &binding,
                requested_email: "message",
                metadata,
                effective_url: url,
                bytes,
            },
            &fences,
            &key,
        );
        let state = lock();
        if !live()
            || !state
                .stores
                .get(SYNTHETIC_USER)
                .is_some_and(|store| store.state() == arca_store::LockState::Unlocked)
        {
            return Err(closed());
        }
        if mode == 1 {
            let answers = jmap::answers(metadata).map_err(|_| closed())?;
            jmap::emails(&answers, "substituted-account", &["message".into()])
                .map_err(|_| closed())?;
            return if issue.is_err() { Ok(1) } else { Err(closed()) };
        }
        let receipt = issue?;
        let verifier = key.verifying_key();
        enrollment.authorize(&binding, "message", bytes, &receipt, &fences, &verifier)?;
        if enrollment
            .authorize(
                &binding,
                "message",
                &wire::unsupported_fixture_message(),
                &receipt,
                &fences,
                &verifier,
            )
            .is_ok()
        {
            return Err(closed());
        }
        // This public synthetic witness carries neither the receipt nor key.
        Ok(0)
    };
    let result = run();
    let (code, witness) = match result {
        Ok(witness) => (status::OK, vec![witness]),
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
