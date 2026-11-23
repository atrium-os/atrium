//! Synthetic receive-only prototype. The verifier, worker channel and
//! enrollment registry are in-process test adapters. There is no socket
//! operation, persistent enrollment, or production constructor.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use mail_smime::envelope::{self, EnvelopedData};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use super::{native, storage_persona, LockedBuf};

const MAX_PLAIN_TEXT: usize = 64 << 10;
const APPROVAL_LIFETIME: Duration = Duration::from_secs(5);
const FAILURE: &str = "Couldn’t open this legacy S/MIME message.";
type Fingerprint = [u8; 32];

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct Scope {
    user: String,
    persona: String,
    publisher: [u8; 32],
}

impl Scope {
    fn new(user: &str, persona: Option<&str>, publisher: [u8; 32]) -> Option<Self> {
        let persona = storage_persona(persona)?;
        if publisher == [0; 32] || !arca_store::model::valid_user(user) {
            return None;
        }
        Some(Self {
            user: user.into(),
            persona: persona.into(),
            publisher,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct OpenBinding {
    scope: Scope,
    launch_uid: u32,
    window: [u8; 16],
    worker: [u8; 16],
    fingerprint: Fingerprint,
    certificate_digest: [u8; 32],
    message_digest: [u8; 32],
    key_bytes: usize,
}

struct ReadWorker {
    binding: OpenBinding,
    cms: EnvelopedData,
    wrapped: Vec<u8>,
}

impl ReadWorker {
    fn stage(
        cms: &[u8],
        cert: &[u8],
        scope: Scope,
        launch_uid: u32,
        window: [u8; 16],
        worker: [u8; 16],
    ) -> Option<Self> {
        let parsed = envelope::enveloped_data(cms).ok()?;
        let (fingerprint, key_bytes, wrapped) = envelope::legacy_recipient(&parsed, cert)?;
        Some(Self {
            binding: OpenBinding {
                scope,
                launch_uid,
                window,
                worker,
                fingerprint,
                certificate_digest: Sha256::digest(cert).into(),
                key_bytes,
                message_digest: Sha256::digest(cms).into(),
            },
            cms: parsed,
            wrapped,
        })
    }

    fn finish(self, candidate: LockedBuf) -> Option<String> {
        let plain = Zeroizing::new(envelope::decrypt(&self.cms, candidate.as_ref()).ok()?);
        let body = plain.strip_prefix(b"Content-Type: text/plain\r\n\r\n")?;
        if body.is_empty() || body.len() > MAX_PLAIN_TEXT {
            return None;
        }
        let text = std::str::from_utf8(body).ok()?;
        text.chars()
            .all(|ch| !ch.is_control() || matches!(ch, '\n' | '\r' | '\t'))
            .then(|| text.to_owned())
    }
}

/// The trusted approval adapter owns and consumes this value. None means
/// cancellation, missing approval or a previously consumed press.
struct VerifiedApproval {
    binding: OpenBinding,
    at: Instant,
}

#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Opened(String),
    Closed(&'static str),
}

struct LegacyKey {
    certificate_digest: [u8; 32],
    der: LockedBuf,
}

#[derive(Default)]
struct Broker {
    enabled: bool,
    publisher: [u8; 32],
    legacy: HashMap<(Scope, Fingerprint), LegacyKey>,
    modern: HashSet<(String, Fingerprint)>,
    private_attempts: usize,
}

impl Broker {
    fn reserve_modern_key(&mut self, user: &str, fingerprint: Fingerprint) -> bool {
        if self
            .legacy
            .keys()
            .any(|(scope, key)| scope.user == user && *key == fingerprint)
        {
            return false;
        }
        self.modern.insert((user.into(), fingerprint));
        true
    }

    fn enroll(&mut self, scope: Scope, cert: &[u8], der: LockedBuf) -> Option<Fingerprint> {
        if !self.enabled || self.publisher != scope.publisher {
            return None;
        }
        let native_key = native::Key::parse(der.as_ref()).ok()?;
        native_key.check_private().ok()?;
        let fingerprint = native_key.fingerprint().ok()?;
        if native::certificate_fingerprint(cert).ok()? != fingerprint
            || self.modern.contains(&(scope.user.clone(), fingerprint))
        {
            return None;
        }
        let slot = self.legacy.entry((scope, fingerprint));
        if let std::collections::hash_map::Entry::Vacant(slot) = slot {
            slot.insert(LegacyKey {
                certificate_digest: Sha256::digest(cert).into(),
                der,
            });
            Some(fingerprint)
        } else {
            None
        }
    }

    fn open(
        &mut self,
        worker: ReadWorker,
        current: &OpenBinding,
        unlocked: bool,
        approval: Option<VerifiedApproval>,
        now: Instant,
    ) -> Outcome {
        self.open_inner(worker, current, unlocked, approval, now)
            .map_or(Outcome::Closed(FAILURE), Outcome::Opened)
    }

    fn open_inner(
        &mut self,
        worker: ReadWorker,
        current: &OpenBinding,
        unlocked: bool,
        approval: Option<VerifiedApproval>,
        now: Instant,
    ) -> Option<String> {
        let approval = approval?;
        if !self.enabled
            || !unlocked
            || worker.binding != *current
            || approval.binding != worker.binding
            || now.checked_duration_since(approval.at)? >= APPROVAL_LIFETIME
            || current.scope.publisher != self.publisher
        {
            return None;
        }
        let secret = self
            .legacy
            .get(&(current.scope.clone(), current.fingerprint))?;
        if current.certificate_digest != secret.certificate_digest {
            return None;
        }
        self.private_attempts += 1;
        let key = native::Key::parse(secret.der.as_ref()).ok()?;
        let candidate = key
            .legacy_candidate(&worker.wrapped, current.key_bytes)
            .ok()?;
        worker.finish(candidate)
    }
}

mod tests {
    use super::*;

    const PUBLISHER: [u8; 32] = [7; 32];
    const CERT: &[u8] = include_bytes!("../../../../../mail/mail-smime/tests/data/leaf.der");
    const KEY: &[u8] = include_bytes!("../../../../../mail/mail-smime/tests/data/leaf.p8");
    const LEGACY: &[u8] = include_bytes!("../../../../../mail/mail-smime/tests/data/env.der");

    fn scope(persona: Option<&str>) -> Scope {
        Scope::new("alice", persona, PUBLISHER).unwrap()
    }

    fn broker() -> Broker {
        Broker {
            enabled: true,
            publisher: PUBLISHER,
            ..Broker::default()
        }
    }

    fn staged(bytes: &[u8], scope: Scope) -> ReadWorker {
        ReadWorker::stage(bytes, CERT, scope, 50000, [1; 16], [2; 16]).unwrap()
    }

    fn consent(worker: &ReadWorker, now: Instant) -> VerifiedApproval {
        VerifiedApproval {
            binding: worker.binding.clone(),
            at: now,
        }
    }

    fn enroll(broker: &mut Broker) -> Fingerprint {
        broker
            .enroll(
                scope(Some("work")),
                CERT,
                LockedBuf::from_slice(KEY).unwrap(),
            )
            .unwrap()
    }

    #[test]
    fn legacy_requires_explicit_approval_and_uses_the_native_provider() {
        let now = Instant::now();
        let mut broker = broker();
        enroll(&mut broker);
        let work = scope(Some("work"));
        let worker = staged(LEGACY, work.clone());
        let current = worker.binding.clone();
        assert_eq!(
            broker.open(worker, &current, true, None, now),
            Outcome::Closed(FAILURE)
        );
        assert_eq!(broker.private_attempts, 0);

        let worker = staged(LEGACY, work.clone());
        let mut approved = Some(consent(&worker, now));
        assert_eq!(
            broker.open(worker, &current, true, approved.take(), now),
            Outcome::Opened("From OpenSSL.\r\n".into()),
        );
        let worker = staged(LEGACY, work);
        assert_eq!(
            broker.open(worker, &current, true, approved.take(), now),
            Outcome::Closed(FAILURE)
        );
        assert_eq!(broker.private_attempts, 1);
    }

    #[test]
    fn legacy_default_disabled_and_work_key_is_unavailable_in_personal() {
        let now = Instant::now();
        let work = scope(Some("work"));
        let mut disabled = Broker {
            publisher: PUBLISHER,
            ..Broker::default()
        };
        assert!(disabled
            .enroll(work.clone(), CERT, LockedBuf::from_slice(KEY).unwrap())
            .is_none());
        let worker = staged(LEGACY, work);
        let current = worker.binding.clone();
        let approval = consent(&worker, now);
        assert_eq!(
            disabled.open(worker, &current, true, Some(approval), now),
            Outcome::Closed(FAILURE)
        );
        assert_eq!(disabled.private_attempts, 0);

        let mut broker = broker();
        enroll(&mut broker);
        let personal = staged(LEGACY, scope(None));
        let current = personal.binding.clone();
        let approval = consent(&personal, now);
        assert_eq!(
            broker.open(personal, &current, true, Some(approval), now),
            Outcome::Closed(FAILURE)
        );
        assert_eq!(broker.private_attempts, 0);
    }

    #[test]
    fn legacy_enrollment_rejects_cross_persona_modern_key_reuse_in_both_directions() {
        let personal = scope(None);
        let mut broker = broker();
        let (fingerprint, _) = envelope::oaep_recipient(CERT).unwrap();
        assert!(broker.reserve_modern_key(&personal.user, fingerprint));
        assert!(broker
            .enroll(
                scope(Some("work")),
                CERT,
                LockedBuf::from_slice(KEY).unwrap()
            )
            .is_none());
        broker.modern.clear();
        assert_eq!(enroll(&mut broker), fingerprint);
        assert!(!broker.reserve_modern_key(&personal.user, fingerprint));
    }

    #[test]
    fn legacy_scope_and_content_failures_have_one_outcome() {
        let now = Instant::now();
        let mut broker = broker();
        enroll(&mut broker);
        let work = scope(Some("work"));
        for change in 0..7 {
            let worker = staged(LEGACY, work.clone());
            let mut current = worker.binding.clone();
            let approval_at = if change == 0 {
                now - APPROVAL_LIFETIME
            } else {
                now
            };
            let approval = consent(&worker, approval_at);
            match change {
                1 => current.message_digest = [0; 32],
                2 => current.window = [0; 16],
                3 => current.worker = [0; 16],
                4 => current.launch_uid += 1,
                5 => current.scope.publisher = [8; 32],
                _ => {}
            }
            assert_eq!(
                broker.open(worker, &current, change != 6, Some(approval), now),
                Outcome::Closed(FAILURE)
            );
        }
        assert_eq!(broker.private_attempts, 0);

        let unrenderable = envelope::encrypt(
            b"Content-Type: application/octet-stream\r\n\r\nsynthetic",
            &[CERT.to_vec()],
        )
        .unwrap();
        let worker = staged(&unrenderable, work);
        let current = worker.binding.clone();
        let approval = consent(&worker, now);
        assert_eq!(
            broker.open(worker, &current, true, Some(approval), now),
            Outcome::Closed(FAILURE)
        );
        assert_eq!(broker.private_attempts, 1);
    }
}
