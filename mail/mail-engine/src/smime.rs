//! S/MIME on the engine's side (mail.md §10.1): the trusted roots the parse
//! jails check signatures against, and the certificates seen on valid
//! signatures, kept per sender for encrypting to them.
//!
//! The engine parses no certificate: roots are PEM unwrapped to DER (a
//! base64 decode of a system file), and a jail's claim about a signature is
//! held to mail-schema's checks (a Valid signer's certificate is for From,
//! and its hash is the hash of the bytes sent).

use mail_schema as wire;
use mail_store::certs::SeenCert;
use mail_store::{MsgId, Store};

use crate::{Engine, Error};

/// Where the system's trusted roots are: the same file Nuntius and the
/// Arca couriers trust for TLS.
pub const SYSTEM_ROOTS: &str = "/etc/ssl/cert.pem";

fn b64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\r' | b'\n' | b' ' | b'\t' => continue,
            _ => return None,
        } as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// The certificates in a PEM file, DER, within the jail's limits.
pub fn pem_certs(pem: &str) -> Vec<Vec<u8>> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";
    let mut out = Vec::new();
    let mut total = 0;
    let mut rest = pem;
    while let Some(a) = rest.find(BEGIN) {
        let body = &rest[a + BEGIN.len()..];
        let Some(b) = body.find(END) else { break };
        if let Some(der) = b64(&body[..b]).filter(|d| !d.is_empty()) {
            if out.len() == wire::ROOTS_MAX || total + der.len() > wire::ROOTS_BYTES_MAX {
                break;
            }
            total += der.len();
            out.push(der);
        }
        rest = &body[b + END.len()..];
    }
    out
}

/// The system's roots; none when the file isn't there (signatures then
/// show as not trusted, never as valid).
pub fn system_roots() -> Vec<Vec<u8>> {
    std::fs::read_to_string(SYSTEM_ROOTS).map(|p| pem_certs(&p)).unwrap_or_default()
}

/// Where the CAs the person added are kept (PEM, settings of account 0).
const ADDED_CAS: &str = "added_cas";
/// At most this many added CAs.
pub const ADDED_CAS_MAX: usize = 20;

/// A CA the person added (§10.1), as Settings shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddedCa {
    pub name: String,
    pub not_after: i64,
    pub sha256: [u8; 32],
}

/// A certificate Mail has seen on a valid signature, as Settings shows it:
/// the newest for its address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KnownCert {
    pub addr: String,
    pub name: String,
    pub issuer: String,
    pub not_after: i64,
}

impl KnownCert {
    /// "Grace Hopper · from Example CA · until 2027-01-31" (or "expired").
    pub fn line(&self, now: i64) -> String {
        let until = if self.not_after <= now { format!("expired {}", crate::clock::day_iso(self.not_after)) } else { format!("until {}", crate::clock::day_iso(self.not_after)) };
        format!("{} · from {} · {until}", self.name, self.issuer)
    }
}

fn pem_of(der: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::from("-----BEGIN CERTIFICATE-----\n");
    for (i, c) in der.chunks(3).enumerate() {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for k in 0..4 {
            s.push(if k <= c.len() { T[(n >> (18 - 6 * k) & 63) as usize] as char } else { '=' });
        }
        if i % 16 == 15 {
            s.push('\n');
        }
    }
    s.push_str("\n-----END CERTIFICATE-----\n");
    s
}

impl Engine {
    /// The CAs the person added (DER).
    pub(crate) fn added_ca_ders(&self) -> Vec<Vec<u8>> {
        self.store.sync_state(0, ADDED_CAS).ok().flatten().map(|p| pem_certs(&p)).unwrap_or_default()
    }

    /// The roots signatures are checked against: the system's (or those
    /// set) and the CAs the person added.
    pub(crate) fn all_roots(&self) -> Vec<Vec<u8>> {
        let mut r = self.roots.clone();
        for d in self.added_ca_ders() {
            if !r.contains(&d) && r.len() < wire::ROOTS_MAX {
                r.push(d);
            }
        }
        r
    }

    /// The CAs the person added, for Settings.
    pub fn added_cas(&self) -> Vec<AddedCa> {
        self.added_ca_ders().iter().filter_map(|d| {
            let c = mail_smime::cert::parse(d).ok()?;
            let i = mail_smime::cert::info(&c);
            Some(AddedCa { name: if i.name.is_empty() { i.issuer } else { i.name }, not_after: i.not_after, sha256: i.sha256 })
        }).collect()
    }

    /// Trust the CA certificates in `pem` (pasted by the person) for
    /// signatures too; their names. A certificate that isn't a CA, has
    /// expired, or is already here is refused.
    pub fn add_ca(&self, pem: &str, now: i64) -> Result<Vec<String>, Error> {
        let ders = pem_certs(pem);
        if ders.is_empty() {
            return Err(Error::Refused("no certificate in it (it starts with -----BEGIN CERTIFICATE-----)"));
        }
        let mut have = self.added_ca_ders();
        let mut names = Vec::new();
        for d in ders {
            let c = mail_smime::cert::parse(&d).map_err(|_| Error::Refused("that isn't a certificate"))?;
            if !mail_smime::cert::is_ca(&c) {
                return Err(Error::Refused("that certificate isn't a certificate authority's"));
            }
            let i = mail_smime::cert::info(&c);
            if i.not_after <= now {
                return Err(Error::Refused("that certificate authority's certificate has expired"));
            }
            if !have.contains(&d) {
                if have.len() >= ADDED_CAS_MAX {
                    return Err(Error::Refused("Mail keeps at most 20 added certificate authorities"));
                }
                have.push(d);
                names.push(if i.name.is_empty() { i.issuer } else { i.name });
            }
        }
        let pem: String = have.iter().map(|d| pem_of(d)).collect();
        self.store.write(|w| w.set_sync_state(0, ADDED_CAS, Some(&pem)))?;
        Ok(names)
    }

    /// Stop trusting the added CA with this SHA-256.
    pub fn forget_ca(&self, sha256: &[u8; 32]) -> Result<(), Error> {
        let have: Vec<Vec<u8>> = self.added_ca_ders().into_iter().filter(|d| &mail_smime::cert::sha256(d) != sha256).collect();
        let pem: String = have.iter().map(|d| pem_of(d)).collect();
        self.store.write(|w| w.set_sync_state(0, ADDED_CAS, (!pem.is_empty()).then_some(pem.as_str())))?;
        Ok(())
    }

    /// Every address with a certificate seen on its signed mail.
    pub fn known_certs(&self) -> Vec<KnownCert> {
        self.store.all_certs().unwrap_or_default().into_iter().filter_map(|(addr, certs)| {
            let c = certs.into_iter().next()?;
            Some(KnownCert { addr, name: c.name, issuer: c.issuer, not_after: c.not_after })
        }).collect()
    }

    /// Forget the certificates seen for `addr` (encryption to them waits
    /// for their next signed mail).
    pub fn forget_certs(&self, addr: &str) -> Result<(), Error> {
        self.store.write(|w| w.forget_certs(addr))?;
        Ok(())
    }

    /// The roots signatures are checked against: the system's, and any the
    /// person added (DER).
    pub fn set_roots(&mut self, roots: Vec<Vec<u8>>) {
        self.roots = roots;
    }

    /// Keep the signer's certificate of each of `msgs` that has a valid
    /// signature, under its From address.
    pub(crate) fn certs_seen(&self, msgs: &[MsgId]) -> Result<(), Error> {
        let mut seen = Vec::new();
        for &id in msgs {
            let Ok(m) = self.store.message(id) else { continue };
            let Some(sm) = &m.summary.smime else { continue };
            let (wire::SmimeState::Valid, Some(c)) = (&sm.state, &sm.signer) else { continue };
            let Some(from) = m.summary.from.first() else { continue };
            let when = sm.signed_at.unwrap_or(m.date).min(crate::now());
            seen.push((from.addr.clone(), SeenCert { sha256: c.sha256, der: c.der.clone(), name: c.name.clone(), issuer: c.issuer.clone(), not_after: c.not_after, first_seen: when, last_seen: when }));
        }
        if !seen.is_empty() {
            self.store.write(|w| {
                for (a, c) in seen {
                    w.saw_cert(&a, c)?;
                }
                Ok(())
            })?;
        }
        Ok(())
    }

    /// The certificate to encrypt to `addr` with: the newest seen one that
    /// hasn't expired.
    pub fn cert_for(&self, addr: &str) -> Option<SeenCert> {
        let now = crate::now();
        self.store.certs(addr).ok()?.into_iter().find(|c| c.not_after > now)
    }
}

/// Whether the store has a certificate for every one of `addrs`.
pub fn can_encrypt_to(store: &Store, addrs: &[String]) -> bool {
    let now = crate::now();
    !addrs.is_empty() && addrs.iter().all(|a| store.certs(a).is_ok_and(|l| l.iter().any(|c| c.not_after > now)))
}

/// Who signs a message: the certificate (DER), its issuers to include, and
/// what makes the signature over the 32-byte digest (the token through
/// Sigillum, or Arca).
pub struct Signer {
    pub cert: Vec<u8>,
    pub chain: Vec<Vec<u8>>,
    pub sign: Box<dyn FnOnce(&[u8; 32]) -> Result<Vec<u8>, String>>,
}

/// How a message is sent: signed, encrypted, both, or neither.
#[derive(Default)]
pub struct Protect {
    pub sign: Option<Signer>,
    pub encrypt: bool,
}

impl Engine {
    /// The recipients of `draft` with no certificate to encrypt to.
    pub fn missing_certs(&self, draft: &mail_compose::Draft) -> Vec<String> {
        let now = crate::now();
        draft.to.iter().chain(&draft.cc).chain(&draft.bcc)
            .map(|m| m.addr.trim().to_ascii_lowercase())
            .filter(|a| !self.store.certs(a).is_ok_and(|l| l.iter().any(|c| c.not_after > now)))
            .collect()
    }

    /// The written message `w`, signed and/or encrypted.
    pub(crate) fn protect(&self, w: &mail_compose::Written, p: Protect, t: i64, random: &[u8; 16]) -> Result<Vec<u8>, Error> {
        use mail_compose::smime as shape;
        if p.sign.is_none() && !p.encrypt {
            return Ok(w.bytes.clone());
        }
        let (outer, entity) = shape::split(&w.bytes).ok_or(Error::Compose("the message can't be cut for S/MIME".into()))?;
        let mut bytes = w.bytes.clone();
        let mut own: Option<Vec<u8>> = None;
        let signed = p.sign.is_some();
        if let Some(s) = p.sign {
            let sig = mail_smime::sign::detached(entity, &s.cert, &s.chain, t, s.sign).map_err(Error::Compose)?;
            let boundary = format!("=_atrium_sig_{}", random[8..].iter().map(|b| format!("{b:02x}")).collect::<String>());
            bytes = shape::signed(outer, entity, &sig, &boundary);
            if mail_smime::sign::scheme_for(&s.cert) == Ok(mail_smime::sign::Scheme::RsaPkcs1Sha256) {
                own = Some(s.cert);
            }
        }
        if p.encrypt {
            let missing: Vec<String> = w.rcpts.iter().filter(|a| self.cert_for(a).is_none()).cloned().collect();
            // Not everyone has a certificate: OpenPGP, when everyone has a
            // key and nothing was signed with S/MIME (§10.2).
            if !missing.is_empty() && !signed && w.rcpts.iter().all(|a| !self.pgp_keys_for(a).is_empty()) {
                let entity = shape::entity_of(&bytes).ok_or(Error::Compose("the message can't be cut for OpenPGP".into()))?.to_vec();
                let armored = self.pgp_encrypt(&entity, &w.from, &w.rcpts)?;
                let outer = shape::split(&bytes).map(|(o, _)| o.to_vec()).unwrap_or_default();
                let boundary = format!("=_atrium_pgp_{}", random[8..].iter().map(|b| format!("{b:02x}")).collect::<String>());
                bytes = shape::pgp_encrypted(&outer, &armored, &boundary);
                if bytes.len() > mail_compose::write::MESSAGE_MAX {
                    return Err(Error::Compose("the message is too large".into()));
                }
                return Ok(bytes);
            }
            if !missing.is_empty() {
                return Err(Error::Compose(format!("No certificate or OpenPGP key to encrypt to {}: they need to send you signed mail, or mail with their key, first", missing.join(", "))));
            }
            // A copy the person can read again: their own certificate.
            let own = own.or_else(|| self.cert_for(&w.from).map(|c| c.der)).ok_or(Error::Compose("Mail has no certificate of yours to keep a copy you can read; sign a message first".into()))?;
            let mut certs: Vec<Vec<u8>> = w.rcpts.iter().filter_map(|a| self.cert_for(a)).map(|c| c.der).collect();
            certs.push(own);
            certs.dedup();
            let entity = shape::entity_of(&bytes).ok_or(Error::Compose("the message can't be cut for S/MIME".into()))?.to_vec();
            let env = mail_smime::envelope::encrypt(&entity, &certs).map_err(Error::Compose)?;
            let outer = shape::split(&bytes).map(|(o, _)| o.to_vec()).unwrap_or_default();
            bytes = shape::enveloped(&outer, &env);
        }
        if bytes.len() > mail_compose::write::MESSAGE_MAX {
            return Err(Error::Compose("the message is too large".into()));
        }
        Ok(bytes)
    }
}

/// Unwraps a content key: given the person's certificate (DER) the message
/// was encrypted to and the wrapped key, the key (RSA PKCS#1 v1.5
/// decryption by whoever holds the private key: the token or Arca).
pub type Unwrap<'a> = &'a dyn Fn(&[u8], &[u8]) -> Result<Vec<u8>, String>;

impl Engine {
    /// The person's certificates for `account`: those seen on their own
    /// signed mail (a Sent copy) for each of their addresses, plus `extra`
    /// (a token's, say).
    pub fn my_certs(&self, account: mail_store::AccountId, extra: &[Vec<u8>]) -> Vec<Vec<u8>> {
        let mut out: Vec<Vec<u8>> = extra.to_vec();
        for i in self.identities(account).unwrap_or_default() {
            for c in self.store.certs(&i.addr).unwrap_or_default() {
                if !out.contains(&c.der) {
                    out.push(c.der);
                }
            }
        }
        out.truncate(wire::MY_CERTS_MAX);
        out
    }

    /// Unlock encrypted message `msg` for this session: the jail finds the
    /// key wrapped for one of `certs`, `unwrap` unwraps it, and the key is
    /// kept in memory. False when the message isn't for any of them.
    pub fn unlock(&self, msg: MsgId, certs: &[Vec<u8>], unwrap: Unwrap) -> Result<bool, Error> {
        let m = self.store.message(msg).map_err(|_| Error::NotFound)?;
        let full = m.full.ok_or(Error::NotFound)?;
        let raw = self.store.blob(&full)?;
        let Some((i, wrapped)) = self.parser.key_for(&raw, certs)? else { return Ok(false) };
        let key = unwrap(&certs[i], &wrapped).map_err(Error::Compose)?;
        if !matches!(key.len(), 16 | 32) {
            return Err(Error::Compose("the unwrapped key has the wrong length".into()));
        }
        self.keys.borrow_mut().insert(msg, key);
        Ok(true)
    }

    /// Whether `msg` was unlocked this session.
    pub fn is_unlocked(&self, msg: MsgId) -> bool {
        self.keys.borrow().contains_key(&msg)
    }

    pub(crate) fn content_key(&self, msg: MsgId) -> Option<Vec<u8>> {
        self.keys.borrow().get(&msg).cloned()
    }

    /// Forget every unlocked key (the person locked the screen, say).
    pub fn forget_keys(&self) {
        self.keys.borrow_mut().clear();
    }
}
