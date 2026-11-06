//! Request handling: one pure function from (label, op, body) to a reply,
//! so every rule is testable without sockets.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use arca_store::codec::{Reader, Writer};
use arca_store::model::{caps, valid_user};
use arca_store::secret::LockedBuf;
use arca_store::{flags, Access, KdfParams, Class, Error, ItemId, Kind, LockState, NewItem, Provenance, Recipient, RootKey, Shape, Store};
use ed25519_dalek::{Signer, SigningKey};

use crate::label::Label;
use crate::proto::{op, status, MAX_SIGN_MSG};

/// Personas arrive with the label once insula.md §13.6 lands; until then
/// every app item is in the user's main persona.
pub const PERSONA: &str = "main";

pub struct State {
    dir: PathBuf,
    /// The parameters new users get. Tests lower it; shipped builds use
    /// [`KdfParams::NO_TPM`] until TPM sealing lands (A4).
    pub kdf_default: KdfParams,
    stores: HashMap<String, Store>,
    audit: Option<File>,
}

/// A reply body. Secret bodies stay in locked memory up to the socket.
pub enum Body {
    Plain(Vec<u8>),
    Secret(LockedBuf),
}

impl Body {
    pub fn bytes(&self) -> &[u8] {
        match self {
            Body::Plain(v) => v,
            Body::Secret(b) => b.as_ref(),
        }
    }
}

pub struct Reply {
    pub status: u8,
    pub body: Body,
}

fn reply(status: u8) -> Reply {
    Reply { status, body: Body::Plain(Vec::new()) }
}

fn ok(body: Vec<u8>) -> Reply {
    Reply { status: status::OK, body: Body::Plain(body) }
}

fn map_err(e: Error) -> Reply {
    reply(match e {
        Error::Denied => status::DENIED,
        Error::Locked => status::LOCKED,
        Error::NeedsVerification => status::NEEDS_VERIFICATION,
        Error::Full => status::FULL,
        Error::BadInput(_) | Error::BadRecipient => status::MALFORMED,
        Error::WrongRootKey => status::WRONG_ROOT,
        Error::Exists => status::EXISTS,
        Error::Io(_) | Error::Corrupt(_) | Error::Unsealed => status::INTERNAL,
    })
}

/// Bail out of a handler with a reply.
macro_rules! tri {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return map_err(e),
        }
    };
}

fn malformed() -> Reply {
    reply(status::MALFORMED)
}

fn class_for_app(v: u8) -> Result<Class, Reply> {
    match Class::from_u8(v) {
        Ok(Class::VerifyEachUse) => Err(reply(status::UNSUPPORTED)), // needs Vestibulum's check (A0c)
        Ok(c) => Ok(c),
        Err(_) => Err(malformed()),
    }
}

impl State {
    /// `dir` holds one store directory per user, plus `audit.log`.
    pub fn new(dir: PathBuf) -> std::io::Result<State> {
        std::fs::DirBuilder::new().recursive(true).create(&dir)?;
        let audit = OpenOptions::new().create(true).append(true).mode(0o600).open(dir.join("audit.log")).ok();
        Ok(State { dir, kdf_default: KdfParams::NO_TPM, stores: HashMap::new(), audit })
    }

    pub fn handle(&mut self, who: &Label, op: u16, body: &[u8]) -> Reply {
        let mut item: Option<ItemId> = None;
        let r = match who {
            Label::App { user, publisher, app_id } => match Recipient::app(*publisher, app_id) {
                Ok(me) => self.app_op(user, &me, op, body, &mut item),
                Err(_) => reply(status::UNKNOWN_PEER),
            },
            Label::Vestibulum => self.vestibulum_op(op, body),
            Label::TrustedUi => self.ui_op(op, body, &mut item),
            Label::Unknown => reply(status::UNKNOWN_PEER),
        };
        self.audit(who, op, item.as_ref(), r.status);
        r
    }

    /// One line per request: who, what, which item, the outcome. Never a
    /// value, a key or an account name (arca.md §10).
    fn audit(&mut self, who: &Label, op: u16, item: Option<&ItemId>, st: u8) {
        let Some(f) = self.audit.as_mut() else { return };
        let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let who = match who {
            Label::App { user, publisher, app_id } => {
                let p: String = publisher[..6].iter().map(|b| format!("{b:02x}")).collect();
                format!("app:{app_id}/{p}@{user}")
            }
            Label::Vestibulum => "vestibulum".into(),
            Label::TrustedUi => "ui".into(),
            Label::Unknown => "unknown".into(),
        };
        let item = item.map(|i| i.hex()).unwrap_or_else(|| "-".into());
        let _ = writeln!(f, "{t} {who} op={op:#06x} item={item} status={st}");
    }

    // ── apps ─────────────────────────────────────────────────────────────

    fn app_op(&mut self, user: &str, me: &Recipient, op: u16, body: &[u8], item: &mut Option<ItemId>) -> Reply {
        let Some(store) = self.stores.get_mut(user) else { return reply(status::LOCKED) };
        let who = Access { persona: PERSONA, recipient: me };
        let mut r = Reader::new(body);
        let mut id = |r: &mut Reader<'_>| -> Result<ItemId, Reply> {
            let i = ItemId(r.array::<16>().map_err(|_| malformed())?);
            *item = Some(i);
            Ok(i)
        };
        macro_rules! field {
            ($e:expr) => {
                match $e {
                    Ok(v) => v,
                    Err(_) => return malformed(),
                }
            };
        }
        macro_rules! rep {
            ($e:expr) => {
                match $e {
                    Ok(v) => v,
                    Err(r) => return r,
                }
            };
        }
        match op {
            op::MINT => {
                let kind = field!(r.u16());
                let class = rep!(class_for_app(field!(r.u8())));
                let account = field!(r.str(caps::ACCOUNT));
                field!(r.end());
                if Kind::from_u16(kind).ok() != Some(Kind::Ed25519) {
                    return reply(status::UNSUPPORTED);
                }
                let seed = tri!(LockedBuf::random(32).map_err(Error::Io));
                let pk = signing_key(&seed).verifying_key().to_bytes();
                let m = tri!(store.add(NewItem {
                    persona: PERSONA,
                    recipient: me.clone(),
                    class,
                    kind: Kind::Ed25519,
                    provenance: Provenance::AppMinted,
                    account,
                    scope: b"",
                    value: seed.as_ref(),
                }));
                *item = Some(m.id);
                let mut w = Writer::new();
                w.raw(&m.id.0);
                w.raw(&pk);
                ok(w.buf)
            }
            op::PUBKEY | op::SIGN => {
                let i = rep!(id(&mut r));
                let msg = if op == op::SIGN { field!(r.bytes(MAX_SIGN_MSG)) } else { &[][..] };
                field!(r.end());
                let (m, seed) = tri!(store.release(&i, &who, None));
                if m.kind != Kind::Ed25519 {
                    return reply(status::UNSUPPORTED);
                }
                let sk = signing_key(&seed);
                drop(seed);
                if op == op::PUBKEY {
                    ok(sk.verifying_key().to_bytes().to_vec())
                } else {
                    ok(sk.sign(msg).to_bytes().to_vec())
                }
            }
            op::STORE => {
                let kind = field!(r.u16());
                let class = rep!(class_for_app(field!(r.u8())));
                let account = field!(r.str(caps::ACCOUNT));
                let value = field!(r.bytes(caps::VALUE));
                field!(r.end());
                let Ok(kind) = Kind::from_u16(kind) else { return malformed() };
                if kind.shape() != Shape::Bearer {
                    // Keys are minted inside Arca, never imported by an app.
                    return reply(status::UNSUPPORTED);
                }
                let m = tri!(store.add(NewItem {
                    persona: PERSONA,
                    recipient: me.clone(),
                    class,
                    kind,
                    provenance: Provenance::AppMinted,
                    account,
                    scope: b"",
                    value,
                }));
                *item = Some(m.id);
                ok(m.id.0.to_vec())
            }
            op::RELEASE => {
                let i = rep!(id(&mut r));
                field!(r.end());
                let m = tri!(store.meta(&i, &who));
                if m.kind.shape() == Shape::Key {
                    return reply(status::NOT_RELEASABLE);
                }
                if m.flags & flags::APP_HELD_GRANTED == 0 {
                    return reply(status::GRANT_REQUIRED);
                }
                let (_, v) = tri!(store.release(&i, &who, None));
                let mut out = tri!(LockedBuf::zeroed(4 + v.len()).map_err(Error::Io));
                out.as_mut()[..4].copy_from_slice(&(v.len() as u32).to_le_bytes());
                out.as_mut()[4..].copy_from_slice(v.as_ref());
                Reply { status: status::OK, body: Body::Secret(out) }
            }
            op::LIST => {
                field!(r.end());
                let items = tri!(store.list(&who));
                let mut w = Writer::new();
                w.u32(items.len() as u32);
                for m in items {
                    w.raw(&m.id.0);
                    w.u16(m.kind as u16);
                    w.u8(m.class as u8);
                    w.u32(m.flags);
                    w.str(&m.account);
                }
                ok(w.buf)
            }
            op::DELETE => {
                let i = rep!(id(&mut r));
                field!(r.end());
                tri!(store.delete(&i, &who));
                ok(Vec::new())
            }
            _ => reply(status::DENIED),
        }
    }

    // ── Vestibulum ───────────────────────────────────────────────────────

    fn user_dir(&self, user: &str) -> PathBuf {
        self.dir.join("users").join(user)
    }

    fn vestibulum_op(&mut self, op: u16, body: &[u8]) -> Reply {
        let mut r = Reader::new(body);
        let Ok(user) = r.str(caps::USER) else { return malformed() };
        if !valid_user(user) {
            return malformed();
        }
        let user = user.to_string();
        let root = |r: &mut Reader<'_>| -> Result<RootKey, Reply> {
            let b = r.raw(32).map_err(|_| malformed())?;
            RootKey::from_bytes(b).map_err(map_err)
        };
        match op {
            op::V_CREATE => {
                let k = match root(&mut r) {
                    Ok(k) => k,
                    Err(e) => return e,
                };
                if r.end().is_err() {
                    return malformed();
                }
                if self.stores.contains_key(&user) || self.user_dir(&user).join("header").exists() {
                    return reply(status::EXISTS);
                }
                let s = tri!(Store::create(&self.user_dir(&user), &user, &k));
                self.stores.insert(user, s);
                ok(Vec::new())
            }
            op::V_UNLOCK => {
                let k = match root(&mut r) {
                    Ok(k) => k,
                    Err(e) => return e,
                };
                if r.end().is_err() {
                    return malformed();
                }
                if !self.stores.contains_key(&user) {
                    if !self.user_dir(&user).join("header").exists() {
                        return reply(status::NO_STORE);
                    }
                    let s = tri!(Store::open(&self.user_dir(&user), &user));
                    self.stores.insert(user.clone(), s);
                }
                tri!(self.stores.get_mut(&user).unwrap().unlock(&k));
                ok(Vec::new())
            }
            op::V_LOCK => {
                if r.end().is_err() {
                    return malformed();
                }
                if let Some(s) = self.stores.get_mut(&user) {
                    s.lock();
                }
                ok(Vec::new())
            }
            op::V_REWRAP => {
                let (old, new) = match (root(&mut r), root(&mut r)) {
                    (Ok(a), Ok(b)) => (a, b),
                    _ => return malformed(),
                };
                if r.end().is_err() {
                    return malformed();
                }
                let Some(s) = self.stores.get_mut(&user) else { return reply(status::LOCKED) };
                tri!(s.rewrap_root(&old, &new));
                ok(Vec::new())
            }
            op::V_STATE => {
                if r.end().is_err() {
                    return malformed();
                }
                let st = match self.stores.get(&user).map(|s| s.state()) {
                    None => 0,
                    Some(LockState::Sealed) => 1,
                    Some(LockState::Locked) => 2,
                    Some(LockState::Unlocked) => 3,
                };
                ok(vec![st])
            }
            op::V_KDF => {
                if r.end().is_err() {
                    return malformed();
                }
                match self.kdf(&user) {
                    Ok((salt, k)) => {
                        let mut w = Writer::new();
                        w.raw(&salt);
                        w.u32(k.m_kib);
                        w.u32(k.t_cost);
                        w.u32(k.p_cost);
                        ok(w.buf)
                    }
                    Err(e) => map_err(e),
                }
            }
            _ => reply(status::DENIED),
        }
    }

    /// The user's root-key derivation parameters, made on first ask.
    fn kdf(&self, user: &str) -> Result<([u8; arca_store::root::SALT_LEN], KdfParams), Error> {
        let dir = self.user_dir(user);
        let path = dir.join("kdf");
        if let Ok(b) = std::fs::read(&path) {
            let mut r = Reader::new(&b);
            let salt = r.array::<{ arca_store::root::SALT_LEN }>()?;
            let k = KdfParams { m_kib: r.u32()?, t_cost: r.u32()?, p_cost: r.u32()? };
            r.end()?;
            // Editing this record gains an attacker nothing: other parameters
            // or another salt give another root key, which fails to unwrap
            // the master. Only sanity is checked.
            if k.t_cost == 0 || k.p_cost == 0 || k.m_kib < 8 * k.p_cost {
                return Err(Error::Corrupt("kdf parameters"));
            }
            return Ok((salt, k));
        }
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        let salt = arca_store::root::new_salt()?;
        let k = self.kdf_default;
        let mut w = Writer::new();
        w.raw(&salt);
        w.u32(k.m_kib);
        w.u32(k.t_cost);
        w.u32(k.p_cost);
        let tmp = dir.join("kdf.tmp");
        {
            let mut f = OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
            f.write_all(&w.buf)?;
            f.sync_all()?;
        }
        std::fs::rename(&tmp, &path)?;
        Ok((salt, k))
    }

    // ── trusted UI ───────────────────────────────────────────────────────

    fn ui_op(&mut self, op: u16, body: &[u8], item: &mut Option<ItemId>) -> Reply {
        match op {
            op::U_GRANT | op::U_REVOKE => {
                let mut r = Reader::new(body);
                let parsed = (|| {
                    let user = r.str(caps::USER)?.to_string();
                    let publisher = r.array::<32>()?;
                    let app_id = r.str(caps::APP_ID)?.to_string();
                    let id = ItemId(r.array::<16>()?);
                    r.end()?;
                    Ok::<_, Error>((user, publisher, app_id, id))
                })();
                let Ok((user, publisher, app_id, id)) = parsed else { return malformed() };
                *item = Some(id);
                let Ok(app) = Recipient::app(publisher, &app_id) else { return malformed() };
                let Some(s) = self.stores.get_mut(&user) else { return reply(status::LOCKED) };
                let who = Access { persona: PERSONA, recipient: &app };
                let m = tri!(s.meta(&id, &who));
                if m.kind.shape() == Shape::Key {
                    return reply(status::NOT_RELEASABLE);
                }
                let (set, clear) = if op == op::U_GRANT {
                    (flags::APP_HELD_GRANTED, 0)
                } else {
                    (0, flags::APP_HELD_GRANTED)
                };
                tri!(s.set_flags(&id, &who, set, clear));
                ok(Vec::new())
            }
            _ => reply(status::DENIED),
        }
    }
}

fn signing_key(seed: &LockedBuf) -> SigningKey {
    let mut b = [0u8; 32];
    b.copy_from_slice(seed.as_ref());
    let sk = SigningKey::from_bytes(&b);
    zeroize_array(&mut b);
    sk
}

fn zeroize_array(b: &mut [u8; 32]) {
    for x in b.iter_mut() {
        // SAFETY: plain volatile writes to our own stack array.
        unsafe { std::ptr::write_volatile(x, 0) };
    }
}
