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
use arca_store::{flags, Access, KdfParams, Class, Error, ItemId, Kind, LockState, NewItem, Provenance, Recipient, RootKey, Scheme, Shape, Store};
use ed25519_dalek::{Signer, SigningKey};

use crate::courier::{app_persona, CourierLabel};
use crate::label::Label;
use crate::proto::{broker_op, courier_op, op, status, MAX_SIGN_MSG};

/// Personas arrive with the label once insula.md §13.6 lands; until then
/// every app item is in the user's main persona.
pub const PERSONA: &str = "main";

/// The machine store's user and persona (arca.md §6.4). It lives in
/// `<dir>/machine`, apart from `<dir>/users/*`, so no login can name it.
pub const MACHINE_USER: &str = "_machine";
pub const MACHINE_PERSONA: &str = "machine";

pub struct State {
    dir: PathBuf,
    /// The parameters new users get. Tests lower it; shipped builds use
    /// [`KdfParams::NO_TPM`] until TPM sealing lands (A4).
    pub kdf_default: KdfParams,
    stores: HashMap<String, Store>,
    audit: Option<File>,
    handles: crate::handles::Handles,
    tickets: crate::tickets::Tickets,
    /// The machine store (arca.md §6.4): open from boot, for services.
    machine: Option<Store>,
    /// Unrecorded trails' cookie jars: in memory only (`crate::jar`).
    jars: crate::jar::Jars,
    /// Unix seconds; tests move it to expire cookies.
    pub clock: fn() -> u64,
    /// Set only by [`State::handle_verified`], for the one request it runs:
    /// Sigillum verified the user for this use of a verify-each-use item.
    verified: bool,
}

fn system_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
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
        // Verify-each-use: each use passes Sigillum's VERIFY_USER first
        // (crate::verify, sigillum.md §8).
        Ok(c) => Ok(c),
        Err(_) => Err(malformed()),
    }
}

impl State {
    /// `dir` holds one store directory per user, plus `audit.log`.
    pub fn new(dir: PathBuf) -> std::io::Result<State> {
        std::fs::DirBuilder::new().recursive(true).create(&dir)?;
        let audit = OpenOptions::new().create(true).append(true).mode(0o600).open(dir.join("audit.log")).ok();
        Ok(State { dir, kdf_default: KdfParams::NO_TPM, stores: HashMap::new(), audit, handles: Default::default(), tickets: Default::default(), machine: None, jars: Default::default(), clock: system_now, verified: false })
    }

    /// Run a request that answered `NEEDS_VERIFICATION` again, after
    /// Sigillum's `VERIFY_USER` for it succeeded: the store gets one
    /// `UserVerified`, for this request only.
    pub fn handle_verified(&mut self, who: &Label, op: u16, body: &[u8]) -> Reply {
        self.verified = true;
        let r = self.handle(who, op, body);
        self.verified = false;
        r
    }

    /// What to ask Sigillum about a request that answered
    /// `NEEDS_VERIFICATION`: the user, and the item's description for the
    /// sheet. Only an app's own verify-each-use item, by the ops that use one.
    pub fn verify_prompt(&self, who: &Label, op: u16, body: &[u8]) -> Option<(String, String)> {
        if let Label::Courier(c) = who {
            let Recipient::Endpoint { host, .. } = &c.endpoint else { return None };
            let store = self.stores.get(&c.user)?;
            let who = Access { persona: &c.persona, recipient: &c.endpoint };
            let names: Vec<String> = self
                .fill_items(c, op, body)?
                .iter()
                .filter_map(|id| store.meta(id, &who).ok())
                .filter(|m| m.class == Class::VerifyEachUse)
                .map(|m| format!("\u{201c}{}\u{201d}", m.account))
                .collect();
            return (!names.is_empty()).then(|| (c.user.clone(), format!("{} for {host}", names.join(", "))));
        }
        let (Label::App { user, publisher, app_id } | Label::WebBroker { user, publisher, app_id }) = who else { return None };
        if !matches!(op, op::PUBKEY | op::SIGN | op::RELEASE) || body.len() < 16 {
            return None;
        }
        let me = Recipient::app(*publisher, app_id).ok()?;
        let id = ItemId(body[..16].try_into().ok()?);
        let m = self.stores.get(user)?.meta(&id, &Access { persona: PERSONA, recipient: &me }).ok()?;
        (m.class == Class::VerifyEachUse).then(|| (user.clone(), format!("\u{201c}{}\u{201d} for {app_id}", m.account)))
    }

    pub fn handle(&mut self, who: &Label, op: u16, body: &[u8]) -> Reply {
        let mut item: Option<ItemId> = None;
        let r = match who {
            Label::App { user, publisher, app_id } => match Recipient::app(*publisher, app_id) {
                Ok(me) => self.app_op(user, &me, op, body, &mut item),
                Err(_) => reply(status::UNKNOWN_PEER),
            },
            Label::WebBroker { user, .. } if op == broker_op::W_TICKET => self.broker_ticket(user, body),
            Label::WebBroker { user, .. } if op == broker_op::W_FORGET => self.broker_forget(user, body),
            Label::WebBroker { user, publisher, app_id } => match Recipient::app(*publisher, app_id) {
                Ok(me) => self.app_op(user, &me, op, body, &mut item),
                Err(_) => reply(status::UNKNOWN_PEER),
            },
            Label::Vestibulum => self.vestibulum_op(op, body),
            Label::TrustedUi => self.ui_op(op, body, &mut item),
            Label::Courier(c) => self.courier_op(c, op, body, &mut item),
            Label::Service(name) if name == crate::passkey::SIGILLUM && (0x90..=0x92).contains(&op) => {
                match crate::passkey::handle(&mut self.stores, PERSONA, op, body, &mut item) {
                    Ok(b) => ok(b),
                    Err(e) => map_err(e),
                }
            }
            Label::Service(name) => self.service_op(name, op, body, &mut item),
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
            Label::WebBroker { user, app_id, .. } => format!("broker:{app_id}@{user}"),
            Label::Vestibulum => "vestibulum".into(),
            Label::TrustedUi => "ui".into(),
            Label::Service(n) => format!("service:{n}"),
            Label::Courier(c) => match &c.endpoint {
                Recipient::Endpoint { host, port, .. } => format!("courier:{host}:{port}/{}@{}", c.persona, c.user),
                _ => "courier:?".into(),
            },
            Label::Unknown => "unknown".into(),
        };
        let item = item.map(|i| i.hex()).unwrap_or_else(|| "-".into());
        let _ = writeln!(f, "{t} {who} op={op:#06x} item={item} status={st}");
    }

    // ── apps ─────────────────────────────────────────────────────────────

    fn app_op(&mut self, user: &str, me: &Recipient, op: u16, body: &[u8], item: &mut Option<ItemId>) -> Reply {
        if !matches!(
            op,
            op::MINT | op::PUBKEY | op::SIGN | op::STORE | op::RELEASE | op::LIST | op::DELETE | op::STORE_FOR | op::LIST_FOR
        ) {
            return reply(status::DENIED);
        }
        let verified = std::mem::take(&mut self.verified);
        let uv = || verified.then(arca_store::UserVerified::vouched_by_vestibulum);
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
                let (m, seed) = tri!(store.release(&i, &who, uv()));
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
                if kind.shape() != Shape::Bearer || matches!(kind, Kind::BluetoothKey | Kind::WifiCredential) {
                    // Keys are minted inside Arca, never imported by an app;
                    // bond keys and Wi-Fi credentials are the machine
                    // store's (§6.4).
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
                let (_, v) = tri!(store.release(&i, &who, uv()));
                let mut out = tri!(LockedBuf::zeroed(4 + v.len()).map_err(Error::Io));
                out.as_mut()[..4].copy_from_slice(&(v.len() as u32).to_le_bytes());
                out.as_mut()[4..].copy_from_slice(v.as_ref());
                Reply { status: status::OK, body: Body::Secret(out) }
            }
            op::STORE_FOR => {
                let kind = field!(r.u16());
                let class = rep!(class_for_app(field!(r.u8())));
                let account = field!(r.str(caps::ACCOUNT));
                let endpoint = field!(crate::proto::read_endpoint(&mut r));
                let value = field!(r.bytes(caps::VALUE));
                field!(r.end());
                let Ok(kind) = Kind::from_u16(kind) else { return malformed() };
                // Cookies are the server's to set, through its courier.
                if kind.shape() != Shape::Bearer || kind == Kind::Cookie {
                    return reply(status::UNSUPPORTED);
                }
                let Recipient::App { publisher, app_id } = me else { return reply(status::DENIED) };
                let m = tri!(store.add(NewItem {
                    persona: &app_persona(publisher, app_id),
                    recipient: endpoint,
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
            op::LIST | op::LIST_FOR => {
                let (persona, recipient) = if op == op::LIST_FOR {
                    let ep = field!(crate::proto::read_endpoint(&mut r));
                    let Recipient::App { publisher, app_id } = me else { return reply(status::DENIED) };
                    (app_persona(publisher, app_id), ep)
                } else {
                    (PERSONA.to_string(), me.clone())
                };
                field!(r.end());
                let items = tri!(store.list(&Access { persona: &persona, recipient: &recipient }));
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

    // ── the machine store ────────────────────────────────────────────────

    /// Open the machine store (arca.md §6.4), creating it and its key on
    /// first boot. Until TPM sealing lands (A4) the root key is a machine
    /// secret in `<dir>/machine.key` (0600, arcad's own directory): it keeps
    /// the store's files useless without that file, not against root.
    pub fn open_machine(&mut self) -> Result<(), Error> {
        use std::io::Read;
        let dir = self.dir.join("machine");
        let key_path = self.dir.join("machine.key");
        let mut raw = LockedBuf::zeroed(32)?;
        match File::open(&key_path) {
            Ok(mut f) => {
                f.read_exact(raw.as_mut())?;
                if f.read(&mut [0u8; 1])? != 0 {
                    return Err(Error::Corrupt("machine.key length"));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                getrandom::getrandom(raw.as_mut()).map_err(std::io::Error::other)?;
                let mut f = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&key_path)?;
                f.write_all(raw.as_ref())?;
                f.sync_all()?;
                File::open(&self.dir)?.sync_all()?;
            }
            Err(e) => return Err(Error::Io(e)),
        }
        eprintln!(
            "arcad: WARNING the machine store's key is a file ({}), not TPM-sealed: root on this machine can read Bluetooth bonds",
            key_path.display()
        );
        let root = RootKey::from_bytes(raw.as_ref())?;
        drop(raw);
        let store = if dir.join("store").exists() || dir.exists() && std::fs::read_dir(&dir)?.next().is_some() {
            let mut s = Store::open(&dir, MACHINE_USER)?;
            s.unlock(&root)?;
            s
        } else {
            Store::create(&dir, MACHINE_USER, &root)?
        };
        self.machine = Some(store);
        Ok(())
    }

    /// A system service's own items in the machine store: the app ops'
    /// wire, class after boot only, values released to the service itself
    /// (it hands them to hardware: a controller asks for a link key within
    /// milliseconds).
    fn service_op(&mut self, name: &str, op: u16, body: &[u8], item: &mut Option<ItemId>) -> Reply {
        let Ok(me) = Recipient::service(name) else { return reply(status::UNKNOWN_PEER) };
        let Some(store) = self.machine.as_mut() else { return reply(status::LOCKED) };
        let who = Access { persona: MACHINE_PERSONA, recipient: &me };
        let mut r = Reader::new(body);
        macro_rules! field {
            ($e:expr) => {
                match $e {
                    Ok(v) => v,
                    Err(_) => return malformed(),
                }
            };
        }
        match op {
            op::STORE => {
                let kind = field!(r.u16());
                let class = field!(r.u8());
                let account = field!(r.str(caps::ACCOUNT));
                let value = field!(r.bytes(caps::VALUE));
                field!(r.end());
                let Ok(kind) = Kind::from_u16(kind) else { return malformed() };
                // The machine store has one class: after boot.
                if class != Class::AfterFirstUnlock as u8 {
                    return reply(status::UNSUPPORTED);
                }
                if kind.shape() != Shape::Bearer || kind == Kind::Cookie {
                    return reply(status::UNSUPPORTED);
                }
                let m = tri!(store.add(NewItem {
                    persona: MACHINE_PERSONA,
                    recipient: me.clone(),
                    class: Class::AfterFirstUnlock,
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
                let id = ItemId(field!(r.array::<16>()));
                field!(r.end());
                *item = Some(id);
                let (_, v) = tri!(store.release(&id, &who, None));
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
                let id = ItemId(field!(r.array::<16>()));
                field!(r.end());
                *item = Some(id);
                tri!(store.delete(&id, &who));
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
                self.handles.lock_user(&user);
                self.jars.lock(&user);
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
            op::U_ACCOUNTS | op::U_HOLD | op::U_HANDLE | op::U_GENERATE => self.field_op(op, body, item),
            op::U_DROP => {
                let Ok(h) = <[u8; 16]>::try_from(body) else { return malformed() };
                self.handles.drop_handle(&h);
                ok(Vec::new())
            }
            _ => reply(status::DENIED),
        }
    }

    /// The credential field's ops. The recipient is the field's parent app,
    /// which arca-ui resolved from the launch registry by the parent's uid.
    fn field_op(&mut self, op: u16, body: &[u8], item: &mut Option<ItemId>) -> Reply {
        use crate::handles::{Entry, Source};
        let mut r = Reader::new(body);
        let parsed = (|| {
            let user = r.str(caps::USER)?.to_string();
            let publisher = r.array::<32>()?;
            let app_id = r.str(caps::APP_ID)?.to_string();
            Ok::<_, Error>((user, publisher, app_id))
        })();
        let Ok((user, publisher, app_id)) = parsed else { return malformed() };
        let Ok(app) = Recipient::app(publisher, &app_id) else { return malformed() };
        let Some(store) = self.stores.get(&user) else { return reply(status::LOCKED) };
        let who = Access { persona: PERSONA, recipient: &app };
        let entry = |source, generated| Entry {
            user: user.clone(),
            persona: PERSONA.into(),
            recipient: app.clone(),
            source,
            expires: std::time::Instant::now() + crate::handles::TTL,
            generated,
        };
        let issued = |h: Option<[u8; 16]>| match h {
            Some(h) => ok(h.to_vec()),
            None => reply(status::FULL),
        };
        match op {
            op::U_ACCOUNTS => {
                if r.end().is_err() {
                    return malformed();
                }
                let items = tri!(store.list(&who));
                let mut w = Writer::new();
                let pw: Vec<_> = items.into_iter().filter(|m| m.kind == Kind::Password).collect();
                w.u32(pw.len() as u32);
                for m in pw {
                    w.raw(&m.id.0);
                    w.str(&m.account);
                }
                ok(w.buf)
            }
            op::U_HOLD => {
                let Ok(v) = r.bytes(caps::VALUE) else { return malformed() };
                if r.end().is_err() || v.is_empty() {
                    return malformed();
                }
                if store.state() != LockState::Unlocked {
                    return reply(status::LOCKED); // passwords are while-unlocked
                }
                let v = tri!(LockedBuf::from_slice(v).map_err(Error::Io));
                issued(self.handles.issue(entry(Source::Pending(v), false)))
            }
            op::U_HANDLE => {
                let Ok(id) = r.array::<16>() else { return malformed() };
                if r.end().is_err() {
                    return malformed();
                }
                let id = ItemId(id);
                *item = Some(id);
                let m = match store.meta(&id, &who) {
                    Ok(m) => m,
                    // While locked, a while-unlocked item's group isn't in
                    // memory, so it looks absent: tell Arca's own UI the
                    // real reason, so the chip says "locked", not "no".
                    Err(_) if store.state() != LockState::Unlocked => return reply(status::LOCKED),
                    Err(e) => return map_err(e),
                };
                if m.kind != Kind::Password {
                    return reply(status::DENIED);
                }
                if m.class != Class::AfterFirstUnlock && store.state() != LockState::Unlocked {
                    return reply(status::LOCKED);
                }
                issued(self.handles.issue(entry(Source::Item(id), false)))
            }
            op::U_GENERATE => {
                if r.end().is_err() {
                    return malformed();
                }
                if store.state() != LockState::Unlocked {
                    return reply(status::LOCKED);
                }
                let v = tri!(generate_password());
                issued(self.handles.issue(entry(Source::Pending(v), true)))
            }
            _ => reply(status::DENIED),
        }
    }

    // ── the web broker ───────────────────────────────────────────────────

    /// A ticket for one credentialed request (arca.md §8.2). Only for a
    /// user whose store is open: a ticket is useless otherwise.
    fn broker_ticket(&mut self, user: &str, body: &[u8]) -> Reply {
        use crate::tickets::{Reason, Ticket};
        let mut r = Reader::new(body);
        let parsed = (|| {
            let ep = crate::proto::read_endpoint(&mut r)?;
            let partition = r.str(caps::HOST)?;
            let method = r.u8()?;
            let path = r.str(PATH_MAX)?;
            let reason = r.u8()?;
            let initiator = r.str(caps::HOST)?;
            let gesture = r.u8()?;
            let query = if r.remaining() > 0 { r.str(QUERY_MAX)? } else { "" };
            r.end()?;
            Ok::<_, Error>((ep, partition, method, path, reason, initiator, gesture, query))
        })();
        let Ok((ep, partition, method, path, reason, initiator, gesture, query)) = parsed else { return malformed() };
        let (Recipient::Endpoint { host, port, .. }, Some(reason)) = (ep, Reason::from_u8(reason)) else { return malformed() };
        if Recipient::endpoint(Scheme::Https, &host, port, Some(partition)).is_err()
            || !(1..=6).contains(&method)
            || !path.starts_with('/')
            || !uri_text(path, true)
            || !uri_text(query, false)
            || gesture > 1
            || (!initiator.is_empty() && Recipient::endpoint(Scheme::Https, initiator, 443, None).is_err())
        {
            return malformed();
        }
        if !self.stores.contains_key(user) {
            return reply(status::LOCKED);
        }
        let t = Ticket {
            user: user.into(),
            partition: partition.into(),
            host,
            port,
            method,
            path: path.into(),
            query: query.into(),
            reason,
            initiator: initiator.to_ascii_lowercase(),
            gesture: gesture == 1,
        };
        match self.tickets.issue(t) {
            Ok(id) => ok(id.to_vec()),
            Err(_) => reply(status::FULL),
        }
    }

    /// Forget web cookie jars: one partition, or an Unrecorded trail's (or
    /// every Unrecorded trail's) by prefix (`broker_op::W_FORGET`).
    fn broker_forget(&mut self, user: &str, body: &[u8]) -> Reply {
        let mut r = Reader::new(body);
        let parsed = (|| {
            let p = r.str(caps::HOST)?;
            let prefix = r.u8()?;
            r.end()?;
            Ok::<_, Error>((p, prefix))
        })();
        let Ok((partition, prefix)) = parsed else { return malformed() };
        let ok_ask = match prefix {
            0 => Recipient::endpoint(Scheme::Https, partition, 443, Some(partition)).is_ok(),
            1 => broker_op::forget_prefix_ok(partition),
            _ => false,
        };
        if !ok_ask {
            return malformed();
        }
        // Unrecorded jars are in memory; Personal ones in the store.
        let mem = self.jars.forget(user, partition, prefix == 1);
        let Some(store) = self.stores.get_mut(user) else { return reply(status::LOCKED) };
        let jars: Vec<Recipient> = tri!(store.recipients())
            .into_iter()
            .filter(|(persona, rec)| {
                persona == broker_op::WEB_PERSONA
                    && matches!(rec, Recipient::Endpoint { partition: Some(p), .. }
                        if if prefix == 1 { p.starts_with(partition) } else { p == partition })
            })
            .map(|(_, rec)| rec)
            .collect();
        let mut n = (mem > 0) as u32;
        for rec in &jars {
            tri!(store.forget(&Access { persona: broker_op::WEB_PERSONA, recipient: rec }));
            n += 1;
        }
        ok(n.to_le_bytes().to_vec())
    }

    /// The ticket a web courier names for this request, checked against
    /// its own label.
    /// Redeeming names the whole request (`exact`: method and query) — a
    /// ticket is for that one request; a response's Set-Cookies name only
    /// the path of the request whose ticket was redeemed.
    fn courier_ticket(&mut self, c: &CourierLabel, path: &str, exact: Option<(u8, &str)>, id: &[u8; 16], redeem: bool) -> Option<crate::tickets::Ticket> {
        let Recipient::Endpoint { host, port, partition: Some(partition), .. } = &c.endpoint else { return None };
        let ok = |t: &crate::tickets::Ticket| {
            t.user == c.user && &t.partition == partition && &t.host == host && t.port == *port && t.path == path
                && exact.is_none_or(|(m, q)| t.method == m && t.query == q)
        };
        if redeem {
            self.tickets.redeem(id, ok)
        } else {
            self.tickets.response(id, ok)
        }
    }

    // ── couriers ─────────────────────────────────────────────────────────

    /// The stored items a `FILL` names, directly or through a handle
    /// (looked at, not redeemed). `None` if it isn't a well-formed `FILL`.
    fn fill_items(&self, c: &CourierLabel, op: u16, body: &[u8]) -> Option<Vec<ItemId>> {
        if op != courier_op::FILL {
            return None;
        }
        let mut r = Reader::new(body);
        let n = r.u8().ok()? as usize;
        if n == 0 || n > courier_op::MAX_SLOTS {
            return None;
        }
        let mut out = Vec::new();
        for _ in 0..n {
            let (src, x) = (r.u8().ok()?, r.array::<16>().ok()?);
            match src {
                courier_op::SOURCE_ITEM => out.push(ItemId(x)),
                courier_op::SOURCE_HANDLE => out.extend(self.handles.item_of(&x, &c.user, &c.persona, &c.endpoint)),
                _ => return None,
            }
        }
        r.end().ok()?;
        Some(out)
    }

    /// A courier asks only after its TLS handshake with its endpoint, and
    /// only for its own label's (persona, endpoint): the store's access
    /// check and the handle's binding both use the registry's label.
    fn courier_op(&mut self, c: &CourierLabel, op: u16, body: &[u8], item: &mut Option<ItemId>) -> Reply {
        let verified = std::mem::take(&mut self.verified);
        let mut r = Reader::new(body);
        let who = Access { persona: &c.persona, recipient: &c.endpoint };
        match op {
            courier_op::FILL => {
                let Ok(n) = r.u8() else { return malformed() };
                let n = n as usize;
                if n == 0 || n > courier_op::MAX_SLOTS {
                    return malformed();
                }
                let mut refs = Vec::with_capacity(n);
                for _ in 0..n {
                    match (r.u8(), r.array::<16>()) {
                        (Ok(s), Ok(x)) => refs.push((s, x)),
                        _ => return malformed(),
                    }
                }
                if r.end().is_err() {
                    return malformed();
                }
                let Some(store) = self.stores.get(&c.user) else { return reply(status::LOCKED) };
                // Verify-each-use (arca.md §6.2): one check covers the whole
                // fill. Looked at before any handle is redeemed, so a fill
                // that waits for the sheet keeps its handles.
                if !verified {
                    for id in self.fill_items(c, op, body).unwrap_or_default() {
                        if store.meta(&id, &who).is_ok_and(|m| m.class == Class::VerifyEachUse) {
                            *item = Some(id);
                            return reply(status::NEEDS_VERIFICATION);
                        }
                    }
                }
                let uv = || verified.then(arca_store::UserVerified::vouched_by_vestibulum);
                let mut vals = Vec::with_capacity(n);
                for (src, x) in refs {
                    let v = match src {
                        courier_op::SOURCE_ITEM => {
                            let id = ItemId(x);
                            *item = Some(id);
                            let m = tri!(store.meta(&id, &who));
                            // Cookies go by Arca's own matching (A2), never by id.
                            if m.kind.shape() != Shape::Bearer || m.kind == Kind::Cookie {
                                return reply(status::DENIED);
                            }
                            tri!(store.release(&id, &who, uv())).1
                        }
                        courier_op::SOURCE_HANDLE => {
                            let Some(e) = self.handles.take(&x, &c.user, &c.persona, &c.endpoint) else {
                                return reply(status::DENIED);
                            };
                            match e.source {
                                crate::handles::Source::Pending(v) => v,
                                crate::handles::Source::Item(id) => {
                                    *item = Some(id);
                                    tri!(store.release(&id, &who, uv())).1
                                }
                            }
                        }
                        _ => return malformed(),
                    };
                    vals.push(v);
                }
                let total: usize = vals.iter().map(|v| 4 + v.len()).sum();
                let mut out = tri!(LockedBuf::zeroed(total).map_err(Error::Io));
                let mut at = 0;
                for v in &vals {
                    out.as_mut()[at..at + 4].copy_from_slice(&(v.len() as u32).to_le_bytes());
                    out.as_mut()[at + 4..at + 4 + v.len()].copy_from_slice(v.as_ref());
                    at += 4 + v.len();
                }
                Reply { status: status::OK, body: Body::Secret(out) }
            }
            courier_op::SET_COOKIE => {
                let parsed = (|| {
                    let name = r.str(COOKIE_NAME_MAX)?;
                    let value = r.bytes(COOKIE_VALUE_MAX)?;
                    let attrs = r.bytes(COOKIE_ATTRS_MAX)?;
                    let path = r.str(PATH_MAX)?;
                    let ticket = if r.remaining() == 16 { Some(r.array::<16>()?) } else { None };
                    r.end()?;
                    Ok::<_, Error>((name, value, attrs, path, ticket))
                })();
                let Ok((name, value, attrs, path, ticket)) = parsed else { return malformed() };
                let Recipient::Endpoint { host, port, partition, .. } = &c.endpoint else { return reply(status::DENIED) };
                // A web courier's cookies come only from a response to a
                // request the broker ticketed; an app's courier has none.
                match (partition, ticket) {
                    (None, None) => {}
                    (Some(_), Some(t)) if self.courier_ticket(c, path, None, &t, false).is_some() => {}
                    _ => return reply(status::DENIED),
                }
                let now = (self.clock)();
                let sc = match crate::cookie::parse(name, value, attrs, host, path, now) {
                    Ok(sc) => sc,
                    // Ignored, as a browser ignores it; the courier drops the header either way.
                    Err(_) => return reply(status::DENIED),
                };
                let Ok(rec) = Recipient::endpoint(Scheme::Https, &sc.domain, *port, partition.as_deref()) else {
                    return reply(status::DENIED);
                };
                // An Unrecorded trail's jar is never written (crate::jar).
                if let Some(p) = partition.as_deref().filter(|p| crate::jar::in_memory(p)) {
                    if !self.stores.get(&c.user).is_some_and(|s| s.state() == arca_store::LockState::Unlocked) {
                        return reply(status::LOCKED);
                    }
                    tri!(self.jars.set(&c.user, p, sc, now).map_err(Error::Io));
                    return ok(Vec::new());
                }
                let who = Access { persona: &c.persona, recipient: &rec };
                let Some(store) = self.stores.get_mut(&c.user) else { return reply(status::LOCKED) };
                let mut sc = sc;
                let mut cookies: Vec<(ItemId, Option<crate::cookie::Attrs>, String)> = tri!(store.list(&who))
                    .into_iter()
                    .filter(|m| m.kind == Kind::Cookie)
                    .map(|m| (m.id, crate::cookie::Attrs::decode(&m.scope), m.account))
                    .collect();
                // One cookie per (name, domain, path, host-only): a new one
                // replaces it, keeping its creation time; an expired one
                // deletes it. An unreadable scope is dropped.
                let same = |a: &Option<crate::cookie::Attrs>, n: &str| {
                    a.as_ref().is_none_or(|a| n == sc.name && a.path == sc.attrs.path && a.host_only == sc.attrs.host_only)
                };
                for (id, a, n) in &cookies {
                    if same(a, n) {
                        if let Some(a) = a {
                            sc.attrs.created = a.created;
                        }
                        tri!(store.delete(id, &who));
                    }
                }
                cookies.retain(|(_, a, n)| !same(a, n));
                if sc.expired(now) {
                    return ok(Vec::new());
                }
                // Expired ones go; then, if the group is full, the oldest.
                let mut live: Vec<(ItemId, u64)> = Vec::new();
                for (id, a, _) in &cookies {
                    match a {
                        Some(a) if a.expires.is_none_or(|e| e > now) => live.push((*id, a.created)),
                        _ => tri!(store.delete(id, &who)),
                    }
                }
                live.sort_by_key(|c| c.1);
                let excess = (live.len() + 1).saturating_sub(crate::cookie::PER_GROUP);
                for (id, _) in live.iter().take(excess) {
                    tri!(store.delete(id, &who));
                }
                let scope = sc.attrs.encode();
                let m = tri!(store.add(NewItem {
                    persona: &c.persona,
                    recipient: rec.clone(),
                    class: Class::AfterFirstUnlock,
                    kind: Kind::Cookie,
                    provenance: Provenance::ServerSet,
                    account: &sc.name,
                    scope: &scope,
                    value: &sc.value,
                }));
                *item = Some(m.id);
                ok(Vec::new())
            }
            courier_op::COOKIES => {
                let Ok(path) = r.str(PATH_MAX) else { return malformed() };
                // A web courier's ticket comes with the request's method and
                // query: it is redeemed only for exactly that request.
                let ticket = if r.remaining() > 0 {
                    match (r.array::<16>(), r.u8(), r.str(QUERY_MAX)) {
                        (Ok(t), Ok(m), Ok(q)) => Some((t, m, q)),
                        _ => return malformed(),
                    }
                } else { None };
                if r.end().is_err() || !path.starts_with('/') {
                    return malformed();
                }
                let Recipient::Endpoint { host, port, partition, .. } = &c.endpoint else { return reply(status::DENIED) };
                // A web courier redeems the broker's ticket for exactly this
                // request, and SameSite goes by it. An app's courier has none.
                let ctx = match (partition, ticket) {
                    (None, None) => None,
                    (Some(_), Some((t, m, q))) => match self.courier_ticket(c, path, Some((m, q)), &t, true) {
                        Some(t) => Some(t),
                        None => return reply(status::DENIED),
                    },
                    _ => return reply(status::DENIED),
                };
                let now = (self.clock)();
                if let Some(p) = partition.as_deref().filter(|p| crate::jar::in_memory(p)) {
                    if !self.stores.get(&c.user).is_some_and(|s| s.state() == arca_store::LockState::Unlocked) {
                        return reply(status::LOCKED);
                    }
                    let found = tri!(self.jars.matching(&c.user, p, host, path, now, ctx.as_ref()).map_err(Error::Io));
                    if found.is_empty() {
                        return ok(Vec::new());
                    }
                    return Reply { status: status::OK, body: Body::Secret(tri!(crate::cookie::header(found).map_err(Error::Io))) };
                }
                let Some(store) = self.stores.get_mut(&c.user) else { return reply(status::LOCKED) };
                let mut found = Vec::new();
                for d in crate::cookie::candidate_domains(host) {
                    let Ok(rec) = Recipient::endpoint(Scheme::Https, &d, *port, partition.as_deref()) else { continue };
                    let who = Access { persona: &c.persona, recipient: &rec };
                    for m in tri!(store.list(&who)).into_iter().filter(|m| m.kind == Kind::Cookie) {
                        let Some(a) = crate::cookie::Attrs::decode(&m.scope) else { continue };
                        if a.expires.is_some_and(|e| e <= now) {
                            tri!(store.delete(&m.id, &who));
                            continue;
                        }
                        if crate::cookie::applies(&d, &a, host, path, now)
                            && ctx.as_ref().is_none_or(|t| crate::cookie::same_site_allows(&a, t))
                        {
                            let (_, v) = tri!(store.release(&m.id, &who, None));
                            found.push((m.account, v, a.path.len(), a.created));
                        }
                    }
                }
                if found.is_empty() {
                    return ok(Vec::new());
                }
                Reply { status: status::OK, body: Body::Secret(tri!(crate::cookie::header(found).map_err(Error::Io))) }
            }
            _ => reply(status::DENIED),
        }
    }

    /// For the courier (A1) and tests: redeem a handle for the verified
    /// recipient, once. The value is in locked memory.
    pub fn redeem(&mut self, handle: &[u8; 16], user: &str, recipient: &Recipient) -> Option<LockedBuf> {
        use crate::handles::Source;
        let e = self.handles.take(handle, user, PERSONA, recipient)?;
        match e.source {
            Source::Pending(v) => Some(v),
            Source::Item(id) => {
                let store = self.stores.get(user)?;
                let who = Access { persona: PERSONA, recipient };
                store.release(&id, &who, None).ok().map(|(_, v)| v)
            }
        }
    }
}

const COOKIE_NAME_MAX: usize = 256;
const COOKIE_VALUE_MAX: usize = 4096;
const COOKIE_ATTRS_MAX: usize = 4096;
/// A request path as the courier sent it.
const PATH_MAX: usize = 8192;
const QUERY_MAX: usize = 8192;

/// A request target's text as the courier's schema admits it: visible
/// ASCII, no `#`, and in a path no `?`.
fn uri_text(s: &str, path: bool) -> bool {
    s.bytes().all(|c| (0x21..=0x7e).contains(&c) && c != b'#' && !(path && c == b'?'))
}

/// A strong password: four groups of five from [a-zA-Z0-9] joined by '-'
/// (about 119 bits), typeable on any keyboard and accepted by nearly every
/// site's rules. Made in locked memory; never logged or shown.
fn generate_password() -> Result<LockedBuf, Error> {
    const ALPHA: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    let mut out = LockedBuf::zeroed(23)?;
    let mut rnd = LockedBuf::zeroed(64)?;
    let mut i = 0;
    let mut pos = 0;
    while pos < 23 {
        if pos == 5 || pos == 11 || pos == 17 {
            out.as_mut()[pos] = b'-';
            pos += 1;
            continue;
        }
        if i == rnd.len() {
            i = 0;
        }
        if i == 0 {
            getrandom::getrandom(rnd.as_mut()).map_err(std::io::Error::other)?;
        }
        let b = rnd.as_ref()[i];
        i += 1;
        // Rejection sampling: 248 = 4 × 62, so no bias.
        if b < 248 {
            out.as_mut()[pos] = ALPHA[(b % 62) as usize];
            pos += 1;
        }
    }
    Ok(out)
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
