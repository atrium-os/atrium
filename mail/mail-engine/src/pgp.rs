//! OpenPGP on the engine's side (mail.md §10.2): keys per address, learned
//! from the `Autocrypt:` header the parse jail reads, or given by the
//! person; PGP/MIME encryption to them. Signatures are checked in the jail
//! with the keys sent along when a message is opened.
//!
//! Like S/MIME encryption in [`crate::smime`], encrypting reads keys here,
//! outside a jail: they are the person's own, or keys a jail already read
//! and found to be for the address they came with. Decrypting and signing
//! need the person's private key, which only Arca may hold: not yet.

use mail_schema::Summary;
use mail_store::pgp::PgpKey;
use mail_store::{AccountId, FolderId, Role};

use crate::{Engine, Error};

pub use mail_pgp::KeyInfo;

/// A key Mail has, for Settings (§10.2): the newest for an address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Known {
    pub addr: String,
    pub info: KeyInfo,
    /// Pasted by the person, not learned from their mail.
    pub given: bool,
    pub last_seen: i64,
}

impl Known {
    /// "Pasted by you · expires 2028-11-18", "From their mail · revoked".
    pub fn line(&self, now: i64) -> String {
        let from = if self.given { "Pasted by you" } else { "From their mail" };
        let state = if self.info.revoked {
            " · revoked by its owner".to_string()
        } else if let Some(e) = self.info.expires {
            if e <= now { format!(" · expired {}", crate::clock::day_iso(e)) } else { format!(" · expires {}", crate::clock::day_iso(e)) }
        } else {
            String::new()
        };
        format!("{} · {from}{state}", self.info.short())
    }
}

/// How a message is encrypted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scheme {
    Smime,
    OpenPgp,
}

impl Engine {
    /// The keys for `addr`, newest first (binary).
    pub fn pgp_keys_for(&self, addr: &str) -> Vec<Vec<u8>> {
        self.store.pgp_keys(addr).unwrap_or_default().into_iter().map(|k| k.key).collect()
    }

    /// The keys a signature on `s` is checked against: its From's.
    pub(crate) fn pgp_keys_of(&self, s: &Summary) -> Vec<Vec<u8>> {
        s.from.first().map(|a| self.pgp_keys_for(&a.addr)).unwrap_or_default()
    }

    /// The newest key for `addr` that can be encrypted to at `now`.
    pub fn usable_pgp_key(&self, addr: &str, now: i64) -> Option<Vec<u8>> {
        self.pgp_keys_for(addr).into_iter().find(|k| mail_pgp::key_info(k).is_ok_and(|i| i.usable(now)))
    }

    /// Every address Mail has a key for, with its newest key.
    pub fn known_pgp_keys(&self) -> Vec<Known> {
        self.store.all_pgp_keys().unwrap_or_default().into_iter().filter_map(|(addr, keys)| {
            let k = keys.into_iter().next()?;
            let info = mail_pgp::key_info(&k.key).ok()?;
            Some(Known { addr, info, given: k.given, last_seen: k.last_seen })
        }).collect()
    }

    /// What the newest key for `addr` is.
    pub fn pgp_key_info(&self, addr: &str) -> Option<KeyInfo> {
        self.pgp_keys_for(addr).first().and_then(|k| mail_pgp::key_info(k).ok())
    }

    /// Keys from `Autocrypt:` headers of new mail in `folder`: `(addr, key,
    /// when, uid)`. A key for one of the person's own addresses counts only
    /// from their Sent folder, where no one else can put mail.
    pub(crate) fn pgp_keys_seen(&self, account: AccountId, folder: FolderId, keys: &[(String, Vec<u8>, i64, u32)]) -> Result<(), Error> {
        if keys.is_empty() {
            return Ok(());
        }
        let sent = self.store.folder(folder).is_ok_and(|f| f.role == Role::Sent);
        let own: Vec<String> = self.identities(account).unwrap_or_default().into_iter().map(|i| i.addr.to_ascii_lowercase()).collect();
        let keep: Vec<_> = keys.iter().filter(|(a, ..)| sent || !own.contains(&a.to_ascii_lowercase())).collect();
        if keep.is_empty() {
            return Ok(());
        }
        self.store.write(|w| {
            for (addr, key, when, _) in &keep {
                w.saw_pgp_key(addr, PgpKey { key: key.clone(), first_seen: *when, last_seen: *when, given: false })?;
            }
            Ok(())
        })?;
        Ok(())
    }

    /// A public key the person gives (pasted, armored or binary): kept for
    /// each address on it, first.
    pub fn give_pgp_key(&self, bytes: &[u8], now: i64) -> Result<KeyInfo, Error> {
        let k = mail_pgp::read_key(bytes).map_err(|_| Error::Refused("that isn't an OpenPGP public key"))?;
        let info = mail_pgp::info(&k);
        if info.emails.is_empty() {
            return Err(Error::Refused("the key has no email address on it"));
        }
        if !info.can_encrypt {
            return Err(Error::Refused("the key can't be encrypted to"));
        }
        if info.revoked {
            return Err(Error::Refused("the key was revoked by its owner"));
        }
        if info.expires.is_some_and(|e| e <= now) {
            return Err(Error::Refused("the key has expired"));
        }
        let bin = mail_pgp::to_binary(&k).map_err(|_| Error::Refused("that isn't an OpenPGP public key"))?;
        if bin.len() > mail_schema::PGP_KEY_MAX {
            return Err(Error::Refused("the key is too large"));
        }
        self.store.write(|w| {
            for e in info.emails.iter().take(8) {
                w.saw_pgp_key(e, PgpKey { key: bin.clone(), first_seen: now, last_seen: now, given: true })?;
            }
            Ok(())
        })?;
        Ok(info)
    }

    /// Forget the keys for `addr`.
    pub fn forget_pgp_keys(&self, addr: &str) -> Result<(), Error> {
        self.store.write(|w| w.forget_pgp_keys(addr))?;
        Ok(())
    }

    /// The person's own key for `account`: the newest for any of their
    /// addresses.
    pub fn my_pgp_key(&self, account: AccountId) -> Option<KeyInfo> {
        self.identities(account).unwrap_or_default().iter().find_map(|i| self.pgp_key_info(&i.addr))
    }

    /// How `draft` can be encrypted: S/MIME when every recipient has a
    /// certificate, else OpenPGP when every one has a key; otherwise who
    /// stands in the way (for the scheme fewer are missing in).
    pub fn encryption_for(&self, draft: &mail_compose::Draft) -> Result<Scheme, Vec<String>> {
        let certs = self.missing_certs(draft);
        if certs.is_empty() {
            return Ok(Scheme::Smime);
        }
        let keys: Vec<String> = draft.to.iter().chain(&draft.cc).chain(&draft.bcc)
            .map(|m| m.addr.trim().to_ascii_lowercase())
            .filter(|a| self.usable_pgp_key(a, crate::now()).is_none())
            .collect();
        if keys.is_empty() {
            return Ok(Scheme::OpenPgp);
        }
        Err(if keys.len() < certs.len() { keys } else { certs })
    }

    /// `entity` (a MIME entity: its headers and body) as an armored
    /// OpenPGP message for `rcpts` and `from` (a copy they can read again).
    pub(crate) fn pgp_encrypt(&self, entity: &[u8], from: &str, rcpts: &[String], visible: &[String]) -> Result<Vec<u8>, Error> {
        let now = crate::now();
        // Expired and revoked keys are never encrypted to (§10.2).
        let recipients: Vec<_> = rcpts.iter().map(|a| (a.as_str(), self.usable_pgp_key(a, now))).collect();
        let missing: Vec<&str> = recipients.iter().filter_map(|(addr, key)| key.is_none().then_some(*addr)).collect();
        if !missing.is_empty() {
            let stale = missing.iter().any(|a| !self.pgp_keys_for(a).is_empty());
            let why = if stale { " (their key has expired or was revoked)" } else { "" };
            return Err(Error::Compose(format!("No certificate or OpenPGP key to encrypt to {}{why}", missing.join(", "))));
        }
        let own = self.usable_pgp_key(from, now).ok_or(Error::Compose("Mail has no OpenPGP key of yours to keep a copy you can read (or yours has expired); add yours in Settings".into()))?;
        let gossip = recipients.iter().filter_map(|(addr, key)| {
            let shared = !addr.eq_ignore_ascii_case(from) && visible.iter().any(|a| a.eq_ignore_ascii_case(addr));
            key.as_deref().filter(|_| shared).map(|key| (*addr, key))
        });
        let entity = mail_compose::smime::autocrypt_gossip(entity, gossip);
        let mut keys: Vec<Vec<u8>> = recipients.into_iter().filter_map(|(_, key)| key).collect();
        keys.push(own);
        keys.sort();
        keys.dedup();
        mail_pgp::encrypt(&entity, &keys).map_err(|e| Error::Compose(format!("OpenPGP: {e}")))
    }
}
