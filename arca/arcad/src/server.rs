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
use crate::proto::{broker_op, courier_op, op, share_op, ssh_op, status, volume_op, MAX_SIGN_MSG};
use crate::share::ShareLabel;

/// Storage partition for existing generic app operations. S/MIME uses
/// the attested launch persona carried by the app's label.
pub const PERSONA: &str = "main";

/// The machine store's user and persona (arca.md §6.4). It lives in
/// `<dir>/machine`, apart from `<dir>/users/*`, so no login can name it.
pub const MACHINE_USER: &str = "_machine";
pub const MACHINE_PERSONA: &str = "machine";

pub struct State {
    mail_leases: HashMap<[u8; 16], mail_refresh::Lease>,
    dir: PathBuf,
    /// The parameters new users get. Tests lower it; shipped builds use
    /// [`KdfParams::NO_TPM`] until TPM sealing lands (A4).
    pub kdf_default: KdfParams,
    stores: HashMap<String, Store>,
    audit: Option<File>,
    handles: crate::handles::Handles,
    tickets: crate::tickets::Tickets,
    pub mail_sign_ins: crate::oauth::Sessions,
    #[cfg(all(feature = "legacy-smime-integration", any(target_os = "freebsd", all(test, feature = "host-openssl-tests"))))]
    pub legacy_smime_registry: Option<PathBuf>,
    #[cfg(all(feature = "legacy-smime-integration", any(target_os = "freebsd", all(test, feature = "host-openssl-tests"))))]
    legacy_smime_pending: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    #[cfg(all(feature = "legacy-smime-integration", any(target_os = "freebsd", all(test, feature = "host-openssl-tests"))))]
    legacy_smime_management: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// The machine store (arca.md §6.4): open from boot, for services.
    machine: Option<Store>,
    /// Unrecorded trails' cookie jars: in memory only (`crate::jar`).
    jars: crate::jar::Jars,
    /// Unix seconds; tests move it to expire cookies.
    pub clock: fn() -> u64,
    /// Set only by [`State::handle_verified`], for the one request it runs:
    /// Sigillum verified the user for this use of a verify-each-use item.
    verified: bool,
    /// The kernel keyring (`/dev/tesserakeys`), opened at start; `None`:
    /// the volume ops answer `UNSUPPORTED`.
    pub keyring: Option<Box<dyn crate::volume::Keyring>>,
    /// Key-group keys (tessera-encryption.md §5.4, E4.6) loaded for each
    /// user's personas: dropped when the user's store locks at logout, so
    /// their folders lock with them. Sticks' keys aren't here: sticks are
    /// unmounted at the lock screen already (device-assignment.md §4.6).
    group_keys: HashMap<String, Vec<[u8; 16]>>,
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

/// [`tri`] in a function that answers `Result<_, Reply>`.
macro_rules! tri_r {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return Err(map_err(e)),
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

fn print_pin_binding<'a>(r: &mut Reader<'a>) -> Result<(&'a str, &'a str, [u8; 32], &'a str), Error> {
    let user = r.str(caps::USER)?;
    let persona = r.str(caps::PERSONA)?;
    let key = r.array::<32>()?;
    let uuid = r.str(36)?;
    if !valid_user(user) || !arca_store::model::valid_persona(persona) || uuid.len() != 36
        || !uuid.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        return Err(Error::BadRecipient);
    }
    Ok((user, persona, key, uuid))
}

mod mail_refresh;

impl State {
    /// `dir` holds one store directory per user, plus `audit.log`.
    pub fn new(dir: PathBuf) -> std::io::Result<State> {
        std::fs::DirBuilder::new().recursive(true).create(&dir)?;
        let audit = OpenOptions::new().create(true).append(true).mode(0o600).open(dir.join("audit.log")).ok();
        Ok(State {
            mail_leases: Default::default(),
            mail_sign_ins: Default::default(),
            #[cfg(all(feature = "legacy-smime-integration", any(target_os = "freebsd", all(test, feature = "host-openssl-tests"))))]
            legacy_smime_registry: None,
        #[cfg(all(feature = "legacy-smime-integration", any(target_os = "freebsd", all(test, feature = "host-openssl-tests"))))]
            legacy_smime_pending: None,
            #[cfg(all(feature = "legacy-smime-integration", any(target_os = "freebsd", all(test, feature = "host-openssl-tests"))))]
            legacy_smime_management: None,
            dir,
            kdf_default: KdfParams::NO_TPM,
            stores: HashMap::new(),
            audit,
            handles: Default::default(),
            tickets: Default::default(),
            machine: None,
            jars: Default::default(),
            clock: system_now,
            verified: false,
            keyring: None,
            group_keys: HashMap::new(),
        })
    }

    fn mail_begin(&mut self, user: &str, publisher: &[u8; 32], app: &str, body: &[u8], renewing: bool) -> Reply {
        let (ids, body) = if renewing {
            let Some((ids, address)) = body.split_first_chunk::<32>() else { return malformed() };
            (Some(*ids), address)
        } else { (None, body) };
        let Ok(address) = std::str::from_utf8(body) else { return malformed() };
        if !address.is_empty() && (address.len() > caps::ACCOUNT || !address.contains('@') || address.chars().any(|c| c.is_control() || c.is_whitespace())) { return malformed(); }
        if app != "org.atrium.mail" { return reply(status::DENIED); }
        if self.mail_sign_ins.registration.is_none() { return reply(status::UNSUPPORTED); }
        if self.mail_sign_ins.mail_publisher.as_ref() != Some(publisher) { return reply(status::DENIED); }
        if !self.stores.get(user).is_some_and(|s| s.state() == LockState::Unlocked) { return reply(status::LOCKED); }
        let start = if let Some(ids) = ids {
            let persona = app_persona(publisher, app);
            let (_, version) = tri!(mail_refresh::renewal_owner(&self.stores[user], &persona, address, &ids));
            self.mail_sign_ins.begin_for(user, address, Some((ids, version)))
        } else { self.mail_sign_ins.begin(user, address) };
        match start {
            Ok(handle) => ok(handle.to_vec()),
            Err(code) => reply(code),
        }
    }

    fn mail_claim(&mut self, user: &str, body: &[u8]) -> Reply {
        let Ok(handle) = body.try_into() else { return malformed(); };
        match self.mail_sign_ins.claim(user, handle) {
            Some(url) => ok(url.as_bytes().to_vec()),
            None => reply(status::DENIED),
        }
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
        if let Label::Service(name) = who {
            if name == crate::ssh::SERVICE && op == ssh_op::SA_SIGN {
                let mut r = Reader::new(body);
                let user = r.str(caps::USER).ok()?;
                let persona = r.str(caps::PERSONA).ok()?;
                let workspace = r.str(64).ok()?;
                let id = ItemId(r.array::<16>().ok()?);
                r.bytes(MAX_SIGN_MSG).ok()?;
                r.end().ok()?;
                let store = self.stores.get(user).filter(|s| s.state() == LockState::Unlocked)?;
                let recipient = Recipient::Service(name.clone());
                let meta = store.meta(&id, &Access { persona, recipient: &recipient }).ok()?;
                return (crate::ssh::is_key(meta.kind) && meta.class == Class::VerifyEachUse
                    && crate::ssh::granted(&meta.scope, workspace))
                    .then(|| (user.into(), format!("workspace {workspace}: {}", meta.account)));
            }
            if !matches!(op, volume_op::VK_FORGET | volume_op::VK_OLDER | volume_op::VK_CURRENT) || !crate::volume::SERVICES.contains(&name.as_str()) {
                return None;
            }
            let mut r = Reader::new(body);
            let user = r.str(caps::USER).ok()?.to_string();
            let persona = r.str(caps::PERSONA).ok()?.to_string();
            let key_id = r.array::<16>().ok()?;
            let store = if user == MACHINE_USER { self.machine.as_ref()? } else { self.stores.get(&user)? };
            let m = Self::volume_item(store, &persona, &key_id).ok()??;
            if op == volume_op::VK_OLDER {
                return Some((user, format!("Open an older copy of \u{201c}{}\u{201d} read-only", m.account)));
            }
            if op == volume_op::VK_CURRENT {
                return Some((user, format!("Use this older copy of \u{201c}{}\u{201d} as current", m.account)));
            }
            return Some((user, format!("Forget the key of \u{201c}{}\u{201d}", m.account)));
        }
        let (Label::App { user, publisher, app_id, .. } | Label::WebBroker { user, publisher, app_id } | Label::FileManager { user, publisher, app_id }) = who else { return None };
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
            Label::App { user, publisher, app_id, persona } if matches!(op, op::MAIL_SMIME_UNWRAP | op::MAIL_SMIME_CERTS) => {
                if app_id == smime::MAIL && *publisher != crate::label::UNSIGNED_PUBLISHER
                    && self.mail_sign_ins.mail_publisher.as_ref() == Some(publisher)
                {
                    if op == op::MAIL_SMIME_CERTS {
                        self.smime_certs(user, persona.as_deref(), body)
                    } else {
                        self.smime_unwrap(user, persona.as_deref(), body, &mut item)
                    }
                } else {
                    reply(status::DENIED)
                }
            }
            Label::App { user, publisher, app_id, .. } if matches!(op, op::MAIL_SIGN_IN | op::MAIL_REAUTH) => self.mail_begin(user, publisher, app_id, body, op == op::MAIL_REAUTH),
            Label::App { user, publisher, app_id, .. } if op == op::MAIL_SIGN_IN_CANCEL => {
                if app_id != "org.atrium.mail" || Some(publisher) != self.mail_sign_ins.mail_publisher.as_ref() { return reply(status::DENIED); }
                let Ok(ticket) = body.try_into() else { return malformed() };
                if self.mail_sign_ins.cancel(user, ticket) { ok(vec![]) } else { reply(status::DENIED) }
            }
            Label::App { user, publisher, app_id, .. } if op == op::MAIL_SIGN_IN_RESULT => {
                if app_id != "org.atrium.mail" || Some(publisher) != self.mail_sign_ins.mail_publisher.as_ref() { return reply(status::DENIED); }
                let mut r = Reader::new(body);
                let (Ok(ticket), Ok(address)) = (r.array::<16>(), r.str(caps::ACCOUNT)) else { return malformed() };
                if r.end().is_err() { return malformed(); }
                self.mail_sign_ins.result(user, &ticket, address).map_or_else(|| reply(status::DENIED), |ids| ok(ids.to_vec()))
            }
            Label::WebBroker { user, .. } if op == broker_op::W_MAIL_SIGN_IN => self.mail_claim(user, body),
            Label::App { user, publisher, app_id, .. } => match Recipient::app(*publisher, app_id) {
                Ok(me) => self.app_op(user, &me, op, body, &mut item),
                Err(_) => reply(status::UNKNOWN_PEER),
            },
            Label::WebBroker { user, .. } if op == broker_op::W_TICKET => self.broker_ticket(user, body),
            Label::WebBroker { user, .. } if op == broker_op::W_FORGET => self.broker_forget(user, body),
            Label::WebBroker { user, publisher, app_id } if op == broker_op::W_HANDLE_FOR => self.broker_handle_for(user, publisher, app_id, body),
            Label::WebBroker { user, .. } if op == broker_op::W_SIGN_OUT => self.broker_sign_out(user, body),
            Label::WebBroker { user, .. } if op == broker_op::W_ACCOUNTS => self.broker_accounts(user, body),
            Label::WebBroker { user, .. } if op == broker_op::W_SCRIPT_COOKIES => self.broker_script_cookies(user, body),
            Label::WebBroker { user, .. } if op == broker_op::W_SCRIPT_SET_COOKIE => self.broker_script_set_cookie(user, body),
            Label::WebBroker { user, publisher, app_id } => match Recipient::app(*publisher, app_id) {
                Ok(me) => self.app_op(user, &me, op, body, &mut item),
                Err(_) => reply(status::UNKNOWN_PEER),
            },
            Label::FileManager { user, publisher, app_id } => match Recipient::app(*publisher, app_id) {
                Ok(me) if (0xA0..=0xA3).contains(&op) => self.file_manager_op(user, &me, op, body, &mut item),
                Ok(me) => self.app_op(user, &me, op, body, &mut item),
                Err(_) => reply(status::UNKNOWN_PEER),
            },
            Label::Share(sl) => self.share_op(sl, op, body, &mut item),
            Label::Vestibulum => self.vestibulum_op(op, body),
            Label::TrustedUi => self.ui_op(op, body, &mut item),
            Label::Courier(c) => self.courier_op(c, op, body, &mut item),
            Label::Service(name) if name == crate::passkey::SIGILLUM && (0x90..=0x92).contains(&op) => {
                match crate::passkey::handle(&mut self.stores, PERSONA, op, body, &mut item) {
                    Ok(b) => ok(b),
                    Err(e) => map_err(e),
                }
            }
            Label::Service(name) if (0xB0..=0xBA).contains(&op) => self.volume_op(name, op, body, &mut item),
            Label::Service(name) if name == crate::ssh::SERVICE && (0xC0..=0xC9).contains(&op) => self.ssh_op(op, body, &mut item),
            Label::Service(name) => self.service_op(name, op, body, &mut item),
            Label::PrintWorker if op == op::TAKE_PRINT_PIN => self.take_print_pin(body),
            Label::PrintWorker if op == op::TAKE_PRINT_LOGIN => self.take_print_login(body),
            Label::PrintBroker if op == op::PRINT_LOGIN_GRANT => self.print_login_grant(body),
            Label::PrintWorker | Label::PrintBroker => reply(status::DENIED),
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
            Label::App { user, publisher, app_id, .. } => {
                let p: String = publisher[..6].iter().map(|b| format!("{b:02x}")).collect();
                format!("app:{app_id}/{p}@{user}")
            }
            Label::WebBroker { user, app_id, .. } => format!("broker:{app_id}@{user}"),
            Label::FileManager { user, app_id, .. } => format!("files:{app_id}@{user}"),
            Label::Share(sl) => match &sl.share {
                Recipient::Share { proto, host, port, .. } => format!("share:{}:{host}:{port}@{}", proto.name(), sl.user),
                _ => "share:?".into(),
            },
            Label::Vestibulum => "vestibulum".into(),
            Label::TrustedUi => "ui".into(),
            Label::Service(n) => format!("service:{n}"),
            Label::Courier(c) => match &c.endpoint {
                Recipient::Endpoint { scheme, host, port, .. } if scheme.is_mail() => format!("courier:{}://{host}:{port}/{}@{}", scheme.name(), c.persona, c.user),
                Recipient::Endpoint { host, port, .. } => format!("courier:{host}:{port}/{}@{}", c.persona, c.user),
                _ => "courier:?".into(),
            },
            Label::PrintWorker => "prelum-ipp".into(),
            Label::PrintBroker => "prelumd".into(),
            Label::Unknown => "unknown".into(),
        };
        let item = item.map(|i| i.hex()).unwrap_or_else(|| "-".into());
        let _ = writeln!(f, "{t} {who} op={op:#06x} item={item} status={st}");
    }

    // ── apps ─────────────────────────────────────────────────────────────

    fn app_op(&mut self, user: &str, me: &Recipient, op: u16, body: &[u8], item: &mut Option<ItemId>) -> Reply {
        if !matches!(
            op,
            op::MINT | op::PUBKEY | op::SIGN | op::STORE | op::RELEASE | op::LIST | op::DELETE | op::STORE_FOR | op::LIST_FOR | op::KEEP_FOR | op::DELETE_FOR | op::DH_X25519
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
                let kind = match Kind::from_u16(kind) {
                    Ok(k @ Kind::Ed25519) => k,
                    // A WireGuard key is used at every handshake, every two
                    // minutes: never "verify each use" (vpn.md §4.3).
                    Ok(k @ Kind::WireGuardKey) if class != Class::VerifyEachUse => k,
                    _ => return reply(status::UNSUPPORTED),
                };
                let seed = tri!(LockedBuf::random(32).map_err(Error::Io));
                let pk = public_of(kind, &seed);
                let m = tri!(store.add(NewItem {
                    persona: PERSONA,
                    recipient: me.clone(),
                    class,
                    kind,
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
                if op == op::PUBKEY && m.kind == Kind::WireGuardKey {
                    return ok(public_of(m.kind, &seed).to_vec());
                }
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
            op::DH_X25519 => {
                let i = rep!(id(&mut r));
                let point = field!(r.array::<32>());
                field!(r.end());
                let (m, secret) = tri!(store.release(&i, &who, uv()));
                if m.kind != Kind::WireGuardKey {
                    return reply(status::UNSUPPORTED);
                }
                match x25519(&secret, &point) {
                    Some(shared) => ok(shared.to_vec()),
                    None => malformed(),
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
                let endpoint = field!(crate::proto::read_endpoint_any(&mut r));
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
            op::KEEP_FOR => {
                let h = field!(r.array::<16>());
                let class = rep!(class_for_app(field!(r.u8())));
                let account = field!(r.str(caps::ACCOUNT)).to_string();
                let n = field!(r.u8()) as usize;
                if n == 0 || n > op::KEEP_FOR_MAX {
                    return malformed();
                }
                let mut endpoints = Vec::with_capacity(n);
                for _ in 0..n {
                    endpoints.push(field!(crate::proto::read_endpoint_any(&mut r)));
                }
                field!(r.end());
                let Recipient::App { publisher, app_id } = me else { return reply(status::DENIED) };
                // The field's handle, for exactly this app (a mismatch burns it).
                let Some(e) = self.handles.take(&h, user, PERSONA, me) else { return reply(status::DENIED) };
                let value = match e.source {
                    crate::handles::Source::Pending(v) => v,
                    crate::handles::Source::Item(src) => {
                        *item = Some(src);
                        tri!(store.release(&src, &who, uv())).1
                    }
                };
                let persona = app_persona(publisher, app_id);
                let mut out = Vec::with_capacity(16 * n);
                for ep in endpoints {
                    let m = tri!(store.add(NewItem {
                        persona: &persona,
                        recipient: ep,
                        class,
                        kind: Kind::Password,
                        provenance: Provenance::AppMinted,
                        account: &account,
                        scope: b"",
                        value: value.as_ref(),
                    }));
                    *item = Some(m.id);
                    out.extend_from_slice(&m.id.0);
                }
                ok(out)
            }
            op::LIST | op::LIST_FOR => {
                let (persona, recipient) = if op == op::LIST_FOR {
                    let ep = field!(crate::proto::read_endpoint_any(&mut r));
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
            op::DELETE_FOR => {
                let ep = field!(crate::proto::read_endpoint_any(&mut r));
                let i = rep!(id(&mut r));
                field!(r.end());
                let Recipient::App { publisher, app_id } = me else { return reply(status::DENIED) };
                let persona = app_persona(publisher, app_id);
                *item = Some(i);
                tri!(store.delete(&i, &Access { persona: &persona, recipient: &ep }));
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
    // ── Tessera volume keys (tessera-encryption.md §4.3) ─────────────────

    /// The owner's store and persona for a volume op: a user's store only
    /// while unlocked (`LOCKED` otherwise) unless `while_locked`; the
    /// machine store under its own persona.
    fn volume_store(&mut self, user: &str, persona: &str, while_locked: bool) -> Result<&mut Store, Reply> {
        if user == MACHINE_USER {
            if persona != MACHINE_PERSONA {
                return Err(reply(status::DENIED));
            }
            return self.machine.as_mut().ok_or_else(|| reply(status::LOCKED));
        }
        if !valid_user(user) || !arca_store::model::valid_persona(persona) {
            return Err(malformed());
        }
        let store = self.stores.get_mut(user).ok_or_else(|| reply(status::LOCKED))?;
        match store.state() {
            LockState::Unlocked => Ok(store),
            LockState::Locked if while_locked => Ok(store),
            _ => Err(reply(status::LOCKED)),
        }
    }

    /// The volume key item with this key id, if the owner holds one.
    fn volume_item(store: &Store, persona: &str, key_id: &[u8; 16]) -> Result<Option<arca_store::ItemMeta>, Error> {
        let me = Recipient::service(crate::volume::RECIPIENT)?;
        let who = Access { persona, recipient: &me };
        Ok(store.list(&who)?.into_iter().find(|m| {
            m.kind == Kind::VolumeKey && crate::volume::decode_scope(&m.scope).is_some_and(|(k, _, _)| &k == key_id)
        }))
    }

    fn note_group_key(&mut self, user: &str, key_id: [u8; 16]) {
        let ids = self.group_keys.entry(user.to_string()).or_default();
        if !ids.contains(&key_id) {
            ids.push(key_id);
        }
    }

    fn volume_op(&mut self, name: &str, op: u16, body: &[u8], item: &mut Option<ItemId>) -> Reply {
        use crate::volume::{decode_scope, encode_scope, owner_label, LoadInfo, RECIPIENT, SERVICES, VALUE_LEN};
        let verified = std::mem::take(&mut self.verified);
        if !SERVICES.contains(&name) {
            return reply(status::DENIED);
        }
        // A removed persona's items go whether or not there is a keyring.
        if self.keyring.is_none() && op != volume_op::PERSONA_DESTROY {
            return reply(status::UNSUPPORTED);
        }
        let mut r = Reader::new(body);
        macro_rules! field {
            ($e:expr) => {
                match $e {
                    Ok(v) => v,
                    Err(_) => return malformed(),
                }
            };
        }
        let user = field!(r.str(caps::USER)).to_string();
        if op == volume_op::VK_FIND {
            // A stick was plugged in: which of the user's personas, if any,
            // holds its key (§7.2). Not the machine store: sticks are a
            // person's.
            let key_id = field!(r.array::<16>());
            let uuid = field!(r.array::<16>());
            let group = field!(r.u64());
            field!(r.end());
            let me = tri!(Recipient::service(RECIPIENT));
            if user == MACHINE_USER || !valid_user(&user) {
                return reply(status::DENIED);
            }
            let personas = match self.stores.get(&user) {
                Some(s) if s.state() == LockState::Unlocked => s.personas_for(&me),
                Some(_) | None => return reply(status::LOCKED),
            };
            for persona in personas {
                let store = match self.volume_store(&user, &persona, false) {
                    Ok(s) => s,
                    Err(r) => return r,
                };
                let Some(m) = tri!(Self::volume_item(store, &persona, &key_id)) else { continue };
                // The scope the key was made for, exactly, or nothing.
                if decode_scope(&m.scope) != Some((key_id, uuid, group)) {
                    return reply(status::DENIED);
                }
                *item = Some(m.id);
                let (_, v) = tri!(store.release(&m.id, &Access { persona: &persona, recipient: &me }, None));
                if v.len() != VALUE_LEN {
                    return reply(status::INTERNAL);
                }
                let high_water = u64::from_le_bytes(v.as_ref()[32..].try_into().unwrap());
                let info = LoadInfo { key_id, uuid, group, high_water, owner_uid: 0, owner_label: owner_label(&user, &persona), older_read_only: false };
                let key: &[u8; 32] = v.as_ref()[..32].try_into().unwrap();
                // The persona, then the name the stick was given (the item's
                // account), for Scrinium and the bar to call it by.
                let mut answer = persona.into_bytes();
                answer.push(0);
                answer.extend_from_slice(m.account.as_bytes());
                return match self.keyring.as_mut().unwrap().load(&info, key) {
                    Ok(()) => ok(answer),
                    Err(e) if e == libc::EEXIST => ok(answer),
                    Err(_) => reply(status::INTERNAL),
                };
            }
            return reply(status::DENIED);
        }
        let persona = field!(r.str(caps::PERSONA)).to_string();
        let me = tri!(Recipient::service(RECIPIENT));
        let label = owner_label(&user, &persona);
        match op {
            volume_op::VK_MINT => {
                let uuid = field!(r.array::<16>());
                let group = field!(r.u64());
                let account = field!(r.str(volume_op::LABEL_MAX)).to_string();
                field!(r.end());
                let mut key_id = [0u8; 16];
                if getrandom::getrandom(&mut key_id).is_err() {
                    return reply(status::INTERNAL);
                }
                let mut value = tri!(LockedBuf::zeroed(VALUE_LEN).map_err(Error::Io));
                if getrandom::getrandom(&mut value.as_mut()[..32]).is_err() {
                    return reply(status::INTERNAL);
                }
                let store = match self.volume_store(&user, &persona, false) {
                    Ok(s) => s,
                    Err(r) => return r,
                };
                let m = tri!(store.add(NewItem {
                    persona: &persona,
                    recipient: me.clone(),
                    class: Class::AfterFirstUnlock,
                    kind: Kind::VolumeKey,
                    provenance: Provenance::ArcaGenerated,
                    account: &account,
                    scope: &encode_scope(&key_id, &uuid, group),
                    value: value.as_ref(),
                }));
                *item = Some(m.id);
                let info = LoadInfo { key_id, uuid, group, high_water: 0, owner_uid: 0, owner_label: label, older_read_only: false };
                let key: &[u8; 32] = value.as_ref()[..32].try_into().unwrap();
                if self.keyring.as_mut().unwrap().load(&info, key).is_err() {
                    // Not usable: don't keep a key the kernel never saw.
                    if let Ok(s) = self.volume_store(&user, &persona, false) {
                        let _ = s.delete(&m.id, &Access { persona: &persona, recipient: &me });
                    }
                    return reply(status::INTERNAL);
                }
                if group != 0 {
                    self.note_group_key(&user, key_id);
                }
                ok(key_id.to_vec())
            }
            volume_op::VK_LOAD => {
                let key_id = field!(r.array::<16>());
                let uuid = field!(r.array::<16>());
                let group = field!(r.u64());
                field!(r.end());
                let store = match self.volume_store(&user, &persona, false) {
                    Ok(s) => s,
                    Err(r) => return r,
                };
                let Some(m) = tri!(Self::volume_item(store, &persona, &key_id)) else { return reply(status::DENIED) };
                *item = Some(m.id);
                // The scope the key was made for, exactly: a key never
                // opens another volume or group.
                if decode_scope(&m.scope) != Some((key_id, uuid, group)) {
                    return reply(status::DENIED);
                }
                let (_, v) = tri!(store.release(&m.id, &Access { persona: &persona, recipient: &me }, None));
                if v.len() != VALUE_LEN {
                    return reply(status::INTERNAL);
                }
                let high_water = u64::from_le_bytes(v.as_ref()[32..].try_into().unwrap());
                let info = LoadInfo { key_id, uuid, group, high_water, owner_uid: 0, owner_label: label, older_read_only: false };
                let key: &[u8; 32] = v.as_ref()[..32].try_into().unwrap();
                match self.keyring.as_mut().unwrap().load(&info, key) {
                    Ok(()) => {}
                    Err(e) if e == libc::EEXIST => {}
                    Err(_) => return reply(status::INTERNAL),
                }
                if group != 0 {
                    self.note_group_key(&user, key_id);
                }
                ok(Vec::new())
            }
            volume_op::VK_RECORD => {
                let key_id = field!(r.array::<16>());
                field!(r.end());
                let gen = match self.keyring.as_mut().unwrap().generation(&key_id) {
                    Ok(g) => g,
                    Err(_) => return reply(status::DENIED),
                };
                let store = match self.volume_store(&user, &persona, true) {
                    Ok(s) => s,
                    Err(r) => return r,
                };
                let Some(m) = tri!(Self::volume_item(store, &persona, &key_id)) else { return reply(status::DENIED) };
                *item = Some(m.id);
                let who = Access { persona: &persona, recipient: &me };
                let (_, mut v) = tri!(store.release(&m.id, &who, None));
                if v.len() != VALUE_LEN {
                    return reply(status::INTERNAL);
                }
                let hw = u64::from_le_bytes(v.as_ref()[32..].try_into().unwrap());
                if gen > hw {
                    v.as_mut()[32..].copy_from_slice(&gen.to_le_bytes());
                    tri!(store.update(&m.id, &who, v.as_ref(), None));
                }
                ok(gen.max(hw).to_le_bytes().to_vec())
            }
            volume_op::VK_FORGET => {
                let key_id = field!(r.array::<16>());
                field!(r.end());
                let store = match self.volume_store(&user, &persona, false) {
                    Ok(s) => s,
                    Err(r) => return r,
                };
                let Some(m) = tri!(Self::volume_item(store, &persona, &key_id)) else { return reply(status::DENIED) };
                *item = Some(m.id);
                if !verified {
                    return reply(status::NEEDS_VERIFICATION);
                }
                tri!(store.delete(&m.id, &Access { persona: &persona, recipient: &me }));
                match self.keyring.as_mut().unwrap().drop_key(&key_id) {
                    Ok(()) => ok(Vec::new()),
                    Err(e) if e == libc::ENOENT => ok(Vec::new()),
                    Err(_) => reply(status::INTERNAL),
                }
            }
            volume_op::VK_RETIRE => {
                let key_id = field!(r.array::<16>());
                field!(r.end());
                if name != "atrium-volumes" {
                    return reply(status::DENIED);
                }
                let store = match self.volume_store(&user, &persona, false) {
                    Ok(s) => s,
                    Err(r) => return r,
                };
                let Some(m) = tri!(Self::volume_item(store, &persona, &key_id)) else { return reply(status::DENIED) };
                *item = Some(m.id);
                // A key group's only: a stick's key goes through VK_FORGET,
                // with the person verified.
                if !decode_scope(&m.scope).is_some_and(|(_, _, g)| g != 0) {
                    return reply(status::DENIED);
                }
                tri!(store.delete(&m.id, &Access { persona: &persona, recipient: &me }));
                if let Some(ids) = self.group_keys.get_mut(&user) {
                    ids.retain(|k| k != &key_id);
                }
                match self.keyring.as_mut().unwrap().drop_key(&key_id) {
                    Ok(()) => ok(Vec::new()),
                    Err(e) if e == libc::ENOENT => ok(Vec::new()),
                    Err(_) => reply(status::INTERNAL),
                }
            }
            volume_op::PERSONA_DESTROY => {
                field!(r.end());
                if name != "atrium-volumes" {
                    return reply(status::DENIED);
                }
                if persona == PERSONA || persona == "personal" || user == MACHINE_USER {
                    return reply(status::DENIED);
                }
                let store = match self.volume_store(&user, &persona, false) {
                    Ok(s) => s,
                    Err(r) => return r,
                };
                let who = Access { persona: &persona, recipient: &me };
                let keys: Vec<[u8; 16]> = tri!(store.list(&who))
                    .into_iter()
                    .filter(|m| m.kind == Kind::VolumeKey)
                    .filter_map(|m| decode_scope(&m.scope).map(|(k, _, _)| k))
                    .collect();
                if let Some(ring) = self.keyring.as_mut() {
                    for k in &keys {
                        match ring.drop_key(k) {
                            Ok(()) => {}
                            Err(e) if e == libc::ENOENT => {}
                            Err(_) => return reply(status::INTERNAL),
                        }
                    }
                }
                if let Some(ids) = self.group_keys.get_mut(&user) {
                    ids.retain(|k| !keys.contains(k));
                }
                // Synthetic JMAP accounts are fenced before their sealed
                // rows go with the persona's groups (arca.md, sealed
                // synthetic account).
                #[cfg(all(
                    feature = "legacy-smime-integration",
                    any(all(test, feature = "host-openssl-tests"), all(target_os = "freebsd", feature = "legacy-jmap-worker"))
                ))]
                if let Some(publisher) = self.mail_sign_ins.mail_publisher {
                    let dir = self.dir.clone();
                    let store = match self.volume_store(&user, &persona, false) {
                        Ok(s) => s,
                        Err(r) => return r,
                    };
                    if integration::fence_persona(store, &dir, &persona, publisher).is_err() {
                        return reply(status::INTERNAL);
                    }
                }
                let store = match self.volume_store(&user, &persona, false) {
                    Ok(s) => s,
                    Err(r) => return r,
                };
                let n = tri!(store.forget_persona(&persona));
                ok((n as u32).to_le_bytes().to_vec())
            }
            volume_op::VK_OLDER => {
                let key_id = field!(r.array::<16>());
                let uuid = field!(r.array::<16>());
                field!(r.end());
                let store = match self.volume_store(&user, &persona, false) {
                    Ok(s) => s,
                    Err(r) => return r,
                };
                let Some(m) = tri!(Self::volume_item(store, &persona, &key_id)) else { return reply(status::DENIED) };
                *item = Some(m.id);
                if decode_scope(&m.scope) != Some((key_id, uuid, 0)) {
                    return reply(status::DENIED);
                }
                if !verified {
                    return reply(status::NEEDS_VERIFICATION);
                }
                let (_, v) = tri!(store.release(&m.id, &Access { persona: &persona, recipient: &me }, None));
                if v.len() != VALUE_LEN {
                    return reply(status::INTERNAL);
                }
                let high_water = u64::from_le_bytes(v.as_ref()[32..].try_into().unwrap());
                let info = LoadInfo { key_id, uuid, group: 0, high_water, owner_uid: 0, owner_label: label, older_read_only: true };
                let key: &[u8; 32] = v.as_ref()[..32].try_into().unwrap();
                let ring = self.keyring.as_mut().unwrap();
                // Loaded as usual (the stick was found): reload it marked.
                match ring.drop_key(&key_id) {
                    Ok(()) => {}
                    Err(e) if e == libc::ENOENT => {}
                    Err(_) => return reply(status::INTERNAL),
                }
                match ring.load(&info, key) {
                    Ok(()) => ok(Vec::new()),
                    Err(_) => reply(status::INTERNAL),
                }
            }
            volume_op::VK_CURRENT => {
                let key_id = field!(r.array::<16>());
                let uuid = field!(r.array::<16>());
                field!(r.end());
                // What the kernel saw mounted: the older copy, opened
                // read-only under a key loaded for that (VK_OLDER).
                let gen = match self.keyring.as_mut().unwrap().generation(&key_id) {
                    Ok(g) if g > 0 => g,
                    _ => return reply(status::DENIED),
                };
                let store = match self.volume_store(&user, &persona, false) {
                    Ok(s) => s,
                    Err(r) => return r,
                };
                let Some(m) = tri!(Self::volume_item(store, &persona, &key_id)) else { return reply(status::DENIED) };
                *item = Some(m.id);
                if decode_scope(&m.scope) != Some((key_id, uuid, 0)) {
                    return reply(status::DENIED);
                }
                if !verified {
                    return reply(status::NEEDS_VERIFICATION);
                }
                let who = Access { persona: &persona, recipient: &me };
                let (_, mut v) = tri!(store.release(&m.id, &who, None));
                if v.len() != VALUE_LEN {
                    return reply(status::INTERNAL);
                }
                let hw = u64::from_le_bytes(v.as_ref()[32..].try_into().unwrap());
                // Only ever lowered here; a copy at or above the mark is
                // current already.
                if gen < hw {
                    v.as_mut()[32..].copy_from_slice(&gen.to_le_bytes());
                    tri!(store.update(&m.id, &who, v.as_ref(), None));
                }
                let mark = gen.min(hw);
                let info = LoadInfo { key_id, uuid, group: 0, high_water: mark, owner_uid: 0, owner_label: label, older_read_only: false };
                let key: &[u8; 32] = v.as_ref()[..32].try_into().unwrap();
                let ring = self.keyring.as_mut().unwrap();
                match ring.drop_key(&key_id) {
                    Ok(()) => {}
                    Err(e) if e == libc::ENOENT => {}
                    Err(_) => return reply(status::INTERNAL),
                }
                match ring.load(&info, key) {
                    Ok(()) => ok(mark.to_le_bytes().to_vec()),
                    Err(_) => reply(status::INTERNAL),
                }
            }
            volume_op::VK_RECOVERY => {
                let key_id = field!(r.array::<16>());
                let uuid = field!(r.array::<16>());
                field!(r.end());
                let kdf = self.kdf_default;
                let store = match self.volume_store(&user, &persona, false) {
                    Ok(s) => s,
                    Err(r) => return r,
                };
                let Some(m) = tri!(Self::volume_item(store, &persona, &key_id)) else { return reply(status::DENIED) };
                *item = Some(m.id);
                if decode_scope(&m.scope) != Some((key_id, uuid, 0)) {
                    return reply(status::DENIED);
                }
                let (_, v) = tri!(store.release(&m.id, &Access { persona: &persona, recipient: &me }, None));
                if v.len() != VALUE_LEN {
                    return reply(status::INTERNAL);
                }
                let Some((code, slot)) = crate::recovery::make(&v.as_ref()[..32], &uuid, &key_id, &m.account, kdf) else {
                    return reply(status::INTERNAL);
                };
                let mut out = tri!(LockedBuf::zeroed(slot.len() + code.len()).map_err(Error::Io));
                out.as_mut()[..slot.len()].copy_from_slice(&slot);
                out.as_mut()[slot.len()..].copy_from_slice(code.as_bytes());
                Reply { status: status::OK, body: Body::Secret(out) }
            }
            volume_op::VK_ADOPT => {
                let key_id = field!(r.array::<16>());
                let uuid = field!(r.array::<16>());
                let slot = field!(r.array::<{ volume_op::SLOT_LEN }>());
                let keep = field!(r.u8());
                let typed = field!(r.str(volume_op::CODE_MAX)).to_string();
                // No code: a handle for what was typed into Arca's own
                // field follows, so the app that asked never saw it.
                let held = if typed.is_empty() { Some(field!(r.array::<16>())) } else { None };
                field!(r.end());
                if keep > 1 {
                    return malformed();
                }
                let code = match held {
                    None => typed,
                    Some(h) => {
                        let Some(v) = self.handles.take_recovery(&h, &user) else { return reply(status::DENIED) };
                        match std::str::from_utf8(v.as_ref()) {
                            Ok(c) if c.len() <= volume_op::CODE_MAX => c.trim().to_string(),
                            _ => return malformed(),
                        }
                    }
                };
                // The person's own store must be open either way: the key
                // is loaded as theirs.
                if let Err(r) = self.volume_store(&user, &persona, false) {
                    return r;
                }
                let (vk, name) = match crate::recovery::open(&slot, &uuid, &key_id, &code) {
                    Ok(k) => k,
                    Err(crate::recovery::OpenError::BadCode) => return malformed(),
                    Err(_) => return reply(status::DENIED),
                };
                if keep == 1 {
                    let store = self.volume_store(&user, &persona, false).ok().unwrap();
                    match tri!(Self::volume_item(store, &persona, &key_id)) {
                        Some(m) => *item = Some(m.id),
                        None => {
                            let mut value = tri!(LockedBuf::zeroed(VALUE_LEN).map_err(Error::Io));
                            value.as_mut()[..32].copy_from_slice(vk.as_ref());
                            let m = tri!(store.add(NewItem {
                                persona: &persona,
                                recipient: me.clone(),
                                class: Class::AfterFirstUnlock,
                                kind: Kind::VolumeKey,
                                provenance: Provenance::UserTyped,
                                // The name it was given at format, from the slot.
                                account: &name,
                                scope: &encode_scope(&key_id, &uuid, 0),
                                value: value.as_ref(),
                            }));
                            *item = Some(m.id);
                        }
                    }
                }
                let info = LoadInfo { key_id, uuid, group: 0, high_water: 0, owner_uid: 0, owner_label: label, older_read_only: false };
                let key: &[u8; 32] = vk.as_ref().try_into().unwrap();
                match self.keyring.as_mut().unwrap().load(&info, key) {
                    Ok(()) => ok(name.into_bytes()),
                    Err(e) if e == libc::EEXIST => ok(name.into_bytes()),
                    Err(_) => reply(status::INTERNAL),
                }
            }
            _ => reply(status::DENIED),
        }
    }

    fn service_op(&mut self, name: &str, op: u16, body: &[u8], item: &mut Option<ItemId>) -> Reply {
        let Ok(me) = Recipient::service(name) else { return reply(status::UNKNOWN_PEER) };
        if op == op::TAKE_VPN {
            // What the person typed into a VPN sign-in field: only the
            // crypta service redeems it, once, for a tunnel's engine.
            if name != op::CRYPTA_SERVICE {
                return reply(status::DENIED);
            }
            let Ok(h) = <[u8; 16]>::try_from(body) else { return malformed() };
            let Some(v) = self.handles.take_vpn(&h) else { return reply(status::DENIED) };
            let mut out = tri!(LockedBuf::zeroed(4 + v.len()).map_err(Error::Io));
            out.as_mut()[..4].copy_from_slice(&(v.len() as u32).to_le_bytes());
            out.as_mut()[4..].copy_from_slice(v.as_ref());
            return Reply { status: status::OK, body: Body::Secret(out) };
        }
        if op == op::VPN_FORGET_JAR {
            if name != op::CRYPTA_SERVICE {
                return reply(status::DENIED);
            }
            let mut r = Reader::new(body);
            let Ok(profile) = r.str(caps::ACCOUNT) else { return malformed() };
            if r.end().is_err() || profile.is_empty() || profile.contains('.') {
                return malformed();
            }
            return self.vpn_forget_jar(&format!("{}{profile}.", crate::jar::VPN_JAR));
        }
        if op == op::TAKE_WIFI {
            // What the user typed into a Wi-Fi passphrase field: only the
            // Wi-Fi key service redeems it, once (curia.md §4).
            if name != op::WIFI_SERVICE {
                return reply(status::DENIED);
            }
            let Ok(h) = <[u8; 16]>::try_from(body) else { return malformed() };
            let Some(v) = self.handles.take_wifi(&h) else { return reply(status::DENIED) };
            let mut out = tri!(LockedBuf::zeroed(4 + v.len()).map_err(Error::Io));
            out.as_mut()[..4].copy_from_slice(&(v.len() as u32).to_le_bytes());
            out.as_mut()[4..].copy_from_slice(v.as_ref());
            return Reply { status: status::OK, body: Body::Secret(out) };
        }
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
        let crypta = name == op::CRYPTA_SERVICE;
        // The Wi-Fi key service mints, signs with and shows TLS client
        // keys only: 802.1X EAP-TLS certificates (wifi.md §7.4 E3).
        let tls_keys = crypta || name == op::WIFI_SERVICE;
        match op {
            // Crypta's WireGuard keys (vpn.md §4.3): minted here, or stored
            // once from an imported file; then only used.
            op::MINT if tls_keys => {
                let kind = field!(r.u16());
                if !crypta && kind != Kind::TlsClientKey as u16 {
                    return reply(status::DENIED);
                }
                let class = field!(r.u8());
                let account = field!(r.str(caps::ACCOUNT));
                field!(r.end());
                if class != Class::AfterFirstUnlock as u8 {
                    return reply(status::UNSUPPORTED);
                }
                if kind == Kind::TlsClientKey as u16 {
                    // A VPN gateway's client key (vpn.md §9.3): P-256.
                    let (seed, pk) = loop {
                        let s = tri!(LockedBuf::random(32).map_err(Error::Io));
                        if let Ok(k) = p256::ecdsa::SigningKey::from_slice(s.as_ref()) {
                            break (s, k.verifying_key().to_encoded_point(false).as_bytes().to_vec());
                        }
                    };
                    let m = tri!(store.add(NewItem {
                        persona: MACHINE_PERSONA,
                        recipient: me.clone(),
                        class: Class::AfterFirstUnlock,
                        kind: Kind::TlsClientKey,
                        provenance: Provenance::ArcaGenerated,
                        account,
                        scope: b"",
                        value: seed.as_ref(),
                    }));
                    *item = Some(m.id);
                    let mut w = Writer::new();
                    w.raw(&m.id.0);
                    w.raw(&pk);
                    return ok(w.buf);
                }
                if kind != Kind::WireGuardKey as u16 {
                    return reply(status::UNSUPPORTED);
                }
                let seed = tri!(LockedBuf::random(32).map_err(Error::Io));
                let pk = public_of(Kind::WireGuardKey, &seed);
                let m = tri!(store.add(NewItem {
                    persona: MACHINE_PERSONA,
                    recipient: me.clone(),
                    class: Class::AfterFirstUnlock,
                    kind: Kind::WireGuardKey,
                    provenance: Provenance::ArcaGenerated,
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
            op::SIGN_DIGEST if tls_keys => {
                let id = ItemId(field!(r.array::<16>()));
                let digest = field!(r.array::<32>());
                field!(r.end());
                *item = Some(id);
                let (m, secret) = tri!(store.release(&id, &who, None));
                if m.kind != Kind::TlsClientKey {
                    return reply(status::UNSUPPORTED);
                }
                let Ok(k) = p256::ecdsa::SigningKey::from_slice(secret.as_ref()) else { return reply(status::INTERNAL) };
                drop(secret);
                use p256::ecdsa::signature::hazmat::PrehashSigner;
                let Ok(sig) = PrehashSigner::<p256::ecdsa::Signature>::sign_prehash(&k, &digest) else { return reply(status::INTERNAL) };
                ok(sig.to_der().as_bytes().to_vec())
            }
            op::PUBKEY | op::DH_X25519 if tls_keys => {
                let id = ItemId(field!(r.array::<16>()));
                let point = if op == op::DH_X25519 { Some(field!(r.array::<32>())) } else { None };
                field!(r.end());
                *item = Some(id);
                let (m, secret) = tri!(store.release(&id, &who, None));
                if !crypta && (point.is_some() || m.kind != Kind::TlsClientKey) {
                    return reply(status::DENIED);
                }
                if m.kind == Kind::TlsClientKey && point.is_none() {
                    let Ok(k) = p256::ecdsa::SigningKey::from_slice(secret.as_ref()) else { return reply(status::INTERNAL) };
                    return ok(k.verifying_key().to_encoded_point(false).as_bytes().to_vec());
                }
                if m.kind != Kind::WireGuardKey {
                    return reply(status::UNSUPPORTED);
                }
                match point {
                    None => ok(public_of(m.kind, &secret).to_vec()),
                    Some(p) => match x25519(&secret, &p) {
                        Some(shared) => ok(shared.to_vec()),
                        None => malformed(),
                    },
                }
            }
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
                if crypta && kind == Kind::WireGuardKey {
                    // A key from an imported .conf: 32 bytes, a usable
                    // scalar (not all zero), answered with its public key.
                    let Ok(b) = <[u8; 32]>::try_from(value) else { return malformed() };
                    if b == [0; 32] {
                        return malformed();
                    }
                    let m = tri!(store.add(NewItem {
                        persona: MACHINE_PERSONA,
                        recipient: me.clone(),
                        class: Class::AfterFirstUnlock,
                        kind,
                        provenance: Provenance::UserTyped,
                        account,
                        scope: b"",
                        value,
                    }));
                    *item = Some(m.id);
                    let (_, secret) = tri!(store.release(&m.id, &who, None));
                    let mut w = Writer::new();
                    w.raw(&m.id.0);
                    w.raw(&public_of(kind, &secret));
                    return ok(w.buf);
                }
                // A gateway session cookie is the crypta service's only
                // cookie (vpn.md §9.3, re-authentication).
                if kind.shape() != Shape::Bearer || (kind == Kind::Cookie && !crypta) {
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
                let (m, v) = tri!(store.release(&id, &who, None));
                if m.kind.shape() == Shape::Key {
                    return reply(status::NOT_RELEASABLE);
                }
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
        #[cfg(all(feature = "legacy-smime-integration", any(target_os = "freebsd", all(test, feature = "host-openssl-tests"))))]
                if user == integration::SYNTHETIC_USER {
                    if let Some(management) = &self.legacy_smime_management {
                        management.store(true, std::sync::atomic::Ordering::Release);
                    }
                    if let Some(pending) = &self.legacy_smime_pending {
                        pending.store(true, std::sync::atomic::Ordering::Release);
                    }
                }
                if let Some(s) = self.stores.get_mut(&user) {
                    s.lock();
                }
                // The person logged out: their personas' folders lock. The
                // kernel writes back and forgets the group's plaintext
                // before each key goes (TKEY_DROP, E4.4).
                if let (Some(ids), Some(ring)) = (self.group_keys.remove(&user), self.keyring.as_mut()) {
                    for id in ids {
                        let _ = ring.drop_key(&id);
                    }
                }
                self.mail_sign_ins.lock(&user);
                self.mail_leases.retain(|_, l| !l.belongs_to(&user));
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
            op::U_HOLD_PRINT_PIN => self.hold_print_pin(body),
            op::U_STORE_PRINT_LOGIN => self.store_print_login(body, item),
            op::U_SAVE_PRINT_PIN => self.saved_print_pin(body, true, item),
            op::U_USE_SAVED_PRINT_PIN => self.saved_print_pin(body, false, item),
            op::U_SMIME_IMPORT | op::U_SMIME_PROVISION => self.smime_import(body, item, op == op::U_SMIME_PROVISION),
            op::U_SMIME_FORGET => self.smime_forget(body, item),
            op::U_DROP => {
                let Ok(h) = <[u8; 16]>::try_from(body) else { return malformed() };
                self.handles.drop_handle(&h);
                ok(Vec::new())
            }
            _ => reply(status::DENIED),
        }
    }

    fn store_print_login(&mut self, body: &[u8], item: &mut Option<ItemId>) -> Reply {
        let mut r = Reader::new(body);
        let parsed = (|| {
            let (user, persona, key, uuid) = print_pin_binding(&mut r)?;
            let recipient = Recipient::printer(key, uuid)?;
            let account = r.str(caps::ACCOUNT)?;
            let password = r.bytes(1024)?;
            r.end()?;
            Ok::<_, Error>((user, persona, recipient, account, password))
        })();
        let Ok((user, persona, recipient, account, password)) = parsed else { return malformed() };
        if account.is_empty() || account.contains(':') || account.chars().any(char::is_control)
            || password.is_empty() || password.iter().any(u8::is_ascii_control)
        {
            return malformed();
        }
        let Some(store) = self.stores.get_mut(user).filter(|s| s.state() == LockState::Unlocked) else {
            return reply(status::LOCKED);
        };
        let access = Access { persona, recipient: &recipient };
        let existing = tri!(store.list(&access)).into_iter().find(|m| m.kind == Kind::Password);
        let meta = match existing {
            Some(m) => tri!(store.update(&m.id, &access, password, Some(account))),
            None => tri!(store.add(NewItem {
                persona, recipient, class: Class::WhileUnlocked, kind: Kind::Password,
                provenance: Provenance::UserTyped, account, scope: b"", value: password,
            })),
        };
        *item = Some(meta.id);
        ok(Vec::new())
    }

    fn print_login_grant(&mut self, body: &[u8]) -> Reply {
        let mut r = Reader::new(body);
        let parsed = (|| {
            let (user, persona, key, uuid) = print_pin_binding(&mut r)?;
            r.end()?;
            Ok::<_, Error>((user, persona, Recipient::printer(key, uuid)?))
        })();
        let Ok((user, persona, recipient)) = parsed else { return malformed() };
        let Some(store) = self.stores.get(user).filter(|s| s.state() == LockState::Unlocked) else {
            // An ordinary job can proceed without credentials.
            return ok(Vec::new());
        };
        let access = Access { persona, recipient: &recipient };
        let Some(meta) = tri!(store.list(&access)).into_iter().find(|m| m.kind == Kind::Password) else {
            return ok(Vec::new());
        };
        let entry = crate::handles::Entry {
            user: user.into(), persona: persona.into(), recipient,
            source: crate::handles::Source::Item(meta.id),
            expires: std::time::Instant::now() + crate::handles::TTL,
            generated: false, hold: crate::handles::HOLD_PRINT_LOGIN, printer: None,
        };
        self.handles.issue(entry).map_or_else(|| reply(status::FULL), |h| ok(h.to_vec()))
    }

    fn take_print_login(&mut self, body: &[u8]) -> Reply {
        let mut r = Reader::new(body);
        let parsed = (|| {
            let h = r.array::<16>()?;
            let recipient = Recipient::printer(r.array::<32>()?, r.str(36)?)?;
            r.end()?;
            Ok::<_, Error>((h, recipient))
        })();
        let Ok((h, recipient)) = parsed else { return malformed() };
        let Some(entry) = self.handles.take_print_login(&h, &recipient) else { return reply(status::DENIED) };
        let crate::handles::Source::Item(id) = entry.source else { return reply(status::DENIED) };
        let Some(store) = self.stores.get(&entry.user).filter(|s| s.state() == LockState::Unlocked) else {
            return reply(status::LOCKED);
        };
        let (meta, password) = tri!(store.release(&id, &Access { persona: &entry.persona, recipient: &recipient }, None));
        if meta.kind != Kind::Password { return reply(status::DENIED); }
        let n = meta.account.len();
        let mut login = tri!(LockedBuf::zeroed(n + 1 + password.len()).map_err(Error::Io));
        login.as_mut()[..n].copy_from_slice(meta.account.as_bytes());
        login.as_mut()[n] = b':';
        login.as_mut()[n + 1..].copy_from_slice(password.as_ref());
        Reply { status: status::OK, body: Body::Secret(login) }
    }

    fn hold_print_pin(&mut self, body: &[u8]) -> Reply {
        let mut r = Reader::new(body);
        let parsed = (|| {
            let (user, persona, key, uuid) = print_pin_binding(&mut r)?;
            let pin = r.bytes(255)?;
            r.end()?;
            Ok::<_, Error>((user, persona, key, uuid, pin))
        })();
        let Ok((user, persona, key, uuid, pin)) = parsed else { return malformed() };
        if pin.is_empty() || !pin.iter().all(u8::is_ascii_digit) {
            return malformed();
        }
        if !self.print_user_unlocked(user) {
            return reply(status::LOCKED);
        }
        let value = tri!(LockedBuf::from_slice(pin).map_err(Error::Io));
        let entry = crate::handles::Entry {
            user: user.into(),
            persona: persona.into(),
            recipient: Recipient::Service("prelum-ipp".into()),
            source: crate::handles::Source::Pending(value),
            expires: std::time::Instant::now() + crate::handles::TTL,
            generated: false,
            hold: crate::handles::HOLD_PRINT_PIN,
            printer: Some((key, uuid.into())),
        };
        self.handles.issue(entry).map_or_else(|| reply(status::FULL), |h| ok(h.to_vec()))
    }

    fn saved_print_pin(&mut self, body: &[u8], save: bool, item: &mut Option<ItemId>) -> Reply {
        use crate::handles::{Entry, Source, HOLD_PRINT_PIN, TTL};
        let mut r = Reader::new(body);
        let parsed = (|| {
            let (user, persona, key, uuid) = print_pin_binding(&mut r)?;
            let recipient = Recipient::printer(key, uuid)?;
            let pin = if save { Some(r.bytes(255)?) } else { None };
            r.end()?;
            Ok::<_, Error>((user, persona, key, uuid, recipient, pin))
        })();
        let Ok((user, persona, key, uuid, recipient, pin)) = parsed else { return malformed() };
        if pin.is_some_and(|p| p.is_empty() || !p.iter().all(u8::is_ascii_digit)) {
            return malformed();
        }
        let Some(store) = self.stores.get_mut(user).filter(|s| s.state() == LockState::Unlocked) else {
            return reply(status::LOCKED);
        };
        let access = Access { persona, recipient: &recipient };
        let current = tri!(store.list(&access)).into_iter().find(|m| m.kind == Kind::PrintPin);
        let meta = match (pin, current) {
            (Some(pin), Some(m)) => tri!(store.update(&m.id, &access, pin, None)),
            (Some(pin), None) => tri!(store.add(NewItem {
                persona, recipient: recipient.clone(), class: Class::WhileUnlocked, kind: Kind::PrintPin,
                provenance: Provenance::UserTyped, account: "", scope: b"", value: pin,
            })),
            (None, Some(m)) => m,
            (None, None) => return reply(status::DENIED),
        };
        *item = Some(meta.id);
        let entry = Entry {
            user: user.into(), persona: persona.into(), recipient, source: Source::Item(meta.id),
            expires: std::time::Instant::now() + TTL, generated: false, hold: HOLD_PRINT_PIN,
            printer: Some((key, uuid.into())),
        };
        self.handles.issue(entry).map_or_else(|| reply(status::FULL), |h| ok(h.to_vec()))
    }

    fn print_user_unlocked(&self, user: &str) -> bool {
        self.stores.get(user).is_some_and(|store| store.state() == LockState::Unlocked)
    }

    fn take_print_pin(&mut self, body: &[u8]) -> Reply {
        let mut r = Reader::new(body);
        let parsed = (|| {
            let handle = r.array::<16>()?;
            let binding = print_pin_binding(&mut r)?;
            r.end()?;
            Ok::<_, Error>((handle, binding))
        })();
        let Ok((handle, (user, persona, key, uuid))) = parsed else { return malformed() };
        if !self.print_user_unlocked(user) {
            return reply(status::LOCKED);
        }
        use crate::handles::Source;
        let Some(entry) = self.handles.take_print_pin(&handle, user, persona, &key, uuid) else {
            return reply(status::DENIED);
        };
        let pin = match entry.source {
            Source::Pending(pin) => pin,
            Source::Item(id) => {
                let access = Access { persona, recipient: &entry.recipient };
                let (meta, pin) = tri!(self.stores[user].release(&id, &access, None));
                if meta.kind != Kind::PrintPin { return reply(status::DENIED); }
                pin
            }
        };
        Reply { status: status::OK, body: Body::Secret(pin) }
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
        let entry = |source, generated, hold| Entry {
            user: user.clone(),
            persona: PERSONA.into(),
            recipient: app.clone(),
            source,
            expires: std::time::Instant::now() + crate::handles::TTL,
            generated,
            hold,
            printer: None,
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
                let hold = match r.remaining() {
                    0 => 0,
                    _ => match r.u8() {
                        Ok(h @ (op::HOLD_FOR_WIFI | op::HOLD_FOR_RECOVERY | op::HOLD_FOR_VPN)) => h,
                        _ => return malformed(),
                    },
                };
                if r.end().is_err() || v.is_empty() {
                    return malformed();
                }
                if store.state() != LockState::Unlocked {
                    return reply(status::LOCKED); // passwords are while-unlocked
                }
                let v = tri!(LockedBuf::from_slice(v).map_err(Error::Io));
                issued(self.handles.issue(entry(Source::Pending(v), false, hold)))
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
                issued(self.handles.issue(entry(Source::Item(id), false, 0)))
            }
            op::U_GENERATE => {
                if r.end().is_err() {
                    return malformed();
                }
                if store.state() != LockState::Unlocked {
                    return reply(status::LOCKED);
                }
                let v = tri!(generate_password());
                issued(self.handles.issue(entry(Source::Pending(v), true, 0)))
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

    /// A typed password goes with the form the person pressed to send
    /// (`broker_op::W_HANDLE_FOR`).
    fn broker_handle_for(&mut self, user: &str, publisher: &[u8; 32], app_id: &str, body: &[u8]) -> Reply {
        let mut r = Reader::new(body);
        let parsed = (|| {
            let h = r.array::<16>()?;
            let ep = crate::proto::read_endpoint(&mut r)?;
            let partition = r.str(caps::HOST)?.to_string();
            r.end()?;
            Ok::<_, Error>((h, ep, partition))
        })();
        let Ok((h, ep, partition)) = parsed else { return malformed() };
        let Recipient::Endpoint { scheme, host, port, .. } = ep else { return malformed() };
        let Ok(to) = Recipient::endpoint(scheme, &host, port, Some(&partition)) else { return malformed() };
        let Ok(from) = Recipient::app(*publisher, app_id) else { return malformed() };
        match self.handles.rebind(&h, user, &from, broker_op::WEB_PERSONA, to) {
            Some(n) => ok(n.to_vec()),
            None => reply(status::DENIED),
        }
    }

    /// "Sign out here" (`broker_op::W_SIGN_OUT`): one partition's session
    /// cookies go, in memory or in the store.
    fn broker_sign_out(&mut self, user: &str, body: &[u8]) -> Reply {
        let mut r = Reader::new(body);
        let Ok(partition) = r.str(caps::HOST) else { return malformed() };
        if r.end().is_err() || Recipient::endpoint(Scheme::Https, partition, 443, Some(partition)).is_err() {
            return malformed();
        }
        let Some(store) = self.stores.get_mut(user) else { return reply(status::LOCKED) };
        if store.state() != arca_store::LockState::Unlocked {
            return reply(status::LOCKED);
        }
        if crate::jar::in_memory(partition) {
            let n = self.jars.sign_out(user, partition) as u32;
            return ok(n.to_le_bytes().to_vec());
        }
        let groups: Vec<Recipient> = tri!(store.recipients())
            .into_iter()
            .filter(|(persona, rec)| {
                persona == broker_op::WEB_PERSONA
                    && matches!(rec, Recipient::Endpoint { partition: Some(p), .. } if p == partition)
            })
            .map(|(_, rec)| rec)
            .collect();
        let mut n = 0u32;
        for rec in &groups {
            let who = Access { persona: broker_op::WEB_PERSONA, recipient: rec };
            for m in tri!(store.list(&who)).into_iter().filter(|m| m.kind == Kind::Cookie) {
                // A session cookie, or one whose scope can't be read.
                if crate::cookie::Attrs::decode(&m.scope).is_none_or(|a| a.expires.is_none()) {
                    tri!(store.delete(&m.id, &who));
                    n += 1;
                }
            }
        }
        ok(n.to_le_bytes().to_vec())
    }

    /// A cookied view's `document.cookie` (`broker_op::W_SCRIPT_COOKIES`):
    /// what a request there would carry, without the `HttpOnly` cookies.
    fn broker_script_cookies(&mut self, user: &str, body: &[u8]) -> Reply {
        let mut r = Reader::new(body);
        let parsed = (|| {
            let ep = crate::proto::read_endpoint(&mut r)?;
            let partition = r.str(caps::HOST)?;
            let path = r.str(1024)?;
            r.end()?;
            Ok::<_, Error>((ep, partition, path))
        })();
        let Ok((ep, partition, path)) = parsed else { return malformed() };
        let Recipient::Endpoint { host, port, .. } = ep else { return malformed() };
        if !path.starts_with('/') || Recipient::endpoint(Scheme::Https, &host, port, Some(partition)).is_err() {
            return malformed();
        }
        if !self.stores.get(user).is_some_and(|s| s.state() == LockState::Unlocked) {
            return reply(status::LOCKED);
        }
        let now = (self.clock)();
        let found = if crate::jar::in_memory(partition) {
            tri!(self.jars.script_matching(user, partition, &host, path, now).map_err(Error::Io))
        } else {
            let Some(store) = self.stores.get_mut(user) else { return reply(status::LOCKED) };
            let mut found = Vec::new();
            for d in crate::cookie::candidate_domains(&host) {
                let Ok(rec) = Recipient::endpoint(Scheme::Https, &d, port, Some(partition)) else { continue };
                let who = Access { persona: broker_op::WEB_PERSONA, recipient: &rec };
                for m in tri!(store.list(&who)).into_iter().filter(|m| m.kind == Kind::Cookie) {
                    let Some(a) = crate::cookie::Attrs::decode(&m.scope) else { continue };
                    if !a.http_only && crate::cookie::applies(&d, &a, &host, path, now) {
                        let (_, v) = tri!(store.release(&m.id, &who, None));
                        found.push((m.account, v, a.path.len(), a.created));
                    }
                }
            }
            found
        };
        if found.is_empty() {
            return ok(Vec::new());
        }
        Reply { status: status::OK, body: Body::Secret(tri!(crate::cookie::header(found).map_err(Error::Io))) }
    }

    /// A cookie a page's script wrote, kept because the reader took the
    /// transition that wrote it (`broker_op::W_SCRIPT_SET_COOKIE`).
    fn broker_script_set_cookie(&mut self, user: &str, body: &[u8]) -> Reply {
        let mut r = Reader::new(body);
        let parsed = (|| {
            let ep = crate::proto::read_endpoint(&mut r)?;
            let partition = r.str(caps::HOST)?;
            let path = r.str(PATH_MAX)?;
            let line = r.str(COOKIE_NAME_MAX + COOKIE_VALUE_MAX + COOKIE_ATTRS_MAX + 2)?;
            r.end()?;
            Ok::<_, Error>((ep, partition.to_string(), path.to_string(), line.to_string()))
        })();
        let Ok((ep, partition, path, line)) = parsed else { return malformed() };
        let Recipient::Endpoint { host, port, .. } = ep else { return malformed() };
        if !path.starts_with('/') || Recipient::endpoint(Scheme::Https, &host, port, Some(&partition)).is_err() {
            return malformed();
        }
        if !self.stores.get(user).is_some_and(|s| s.state() == LockState::Unlocked) {
            return reply(status::LOCKED);
        }
        // `document.cookie = "n=v; attrs"`: as a Set-Cookie header's value.
        let (pair, attrs) = line.split_once(';').unwrap_or((&line, ""));
        let Some((name, value)) = pair.split_once('=') else { return reply(status::DENIED) };
        let now = (self.clock)();
        let Ok(sc) = crate::cookie::parse(name.trim(), value.trim().as_bytes(), attrs.as_bytes(), &host, &path, now) else {
            return reply(status::DENIED);
        };
        match self.put_cookie(user, broker_op::WEB_PERSONA, port, Some(&partition), sc, now, true) {
            Ok(_) => ok(Vec::new()),
            Err(r) => r,
        }
    }

    /// The sign-on token a VPN gateway set in its sign-in window's jar:
    /// the crypta service's, as `<profile>/sso@<host>`, bound to the host
    /// that set it; the engine asks for its own gateway's only, once.
    fn vpn_token(&mut self, profile: &str, host: &str, value: &[u8], item: &mut Option<ItemId>) -> Reply {
        if value.is_empty() || value.len() > 8192 {
            return reply(status::DENIED);
        }
        let Ok(me) = Recipient::service(op::CRYPTA_SERVICE) else { return reply(status::UNKNOWN_PEER) };
        let Some(store) = self.machine.as_mut() else { return reply(status::LOCKED) };
        let who = Access { persona: MACHINE_PERSONA, recipient: &me };
        let account = format!("{profile}/sso@{host}");
        let old: Vec<ItemId> = tri!(store.list(&who)).into_iter().filter(|m| m.account == account).map(|m| m.id).collect();
        let m = tri!(store.add(NewItem {
            persona: MACHINE_PERSONA,
            recipient: me.clone(),
            class: Class::AfterFirstUnlock,
            kind: Kind::OAuthToken,
            provenance: Provenance::ServerSet,
            account: &account,
            scope: b"",
            value,
        }));
        for o in old {
            let _ = store.delete(&o, &who);
        }
        *item = Some(m.id);
        ok(Vec::new())
    }

    /// A VPN profile's sign-in jar, everywhere it can be reached: memory,
    /// and every unlocked person's store. A locked store keeps its copy
    /// until the profile's name is used again (cryptad asks at `create`
    /// too).
    fn vpn_forget_jar(&mut self, prefix: &str) -> Reply {
        let mut n = 0u32;
        let users: Vec<String> = self.stores.keys().cloned().collect();
        for user in &users {
            n += (self.jars.forget(user, prefix, true) > 0) as u32;
            let Some(store) = self.stores.get_mut(user) else { continue };
            let jars: Vec<Recipient> = tri!(store.recipients())
                .into_iter()
                .filter(|(persona, rec)| {
                    persona == broker_op::WEB_PERSONA && matches!(rec, Recipient::Endpoint { partition: Some(p), .. } if p.starts_with(prefix))
                })
                .map(|(_, rec)| rec)
                .collect();
            for rec in &jars {
                tri!(store.forget(&Access { persona: broker_op::WEB_PERSONA, recipient: rec }));
                n += 1;
            }
        }
        ok(n.to_le_bytes().to_vec())
    }

    /// Keep one cookie a web courier (or, `script`, a page's
    /// `document.cookie`) set, by Arca's rules: one per (name, domain,
    /// path, host-only), expired ones gone, the group's oldest out when
    /// full. An Unrecorded partition's go to memory. A script's never
    /// carries or replaces an `HttpOnly` one.
    #[allow(clippy::too_many_arguments)]
    fn put_cookie(&mut self, user: &str, persona: &str, port: u16, partition: Option<&str>,
                  sc: crate::cookie::SetCookie, now: u64, script: bool) -> Result<Option<ItemId>, Reply> {
        if script && sc.attrs.http_only {
            return Err(reply(status::DENIED));
        }
        let Ok(rec) = Recipient::endpoint(Scheme::Https, &sc.domain, port, partition) else {
            return Err(reply(status::DENIED));
        };
        // An Unrecorded trail's jar is never written (crate::jar).
        if let Some(p) = partition.filter(|p| crate::jar::in_memory(p)) {
            if !self.stores.get(user).is_some_and(|s| s.state() == arca_store::LockState::Unlocked) {
                return Err(reply(status::LOCKED));
            }
            if script && self.jars.http_only_named(user, p, &sc) {
                return Err(reply(status::DENIED));
            }
            tri_r!(self.jars.set(user, p, sc, now).map_err(Error::Io));
            return Ok(None);
        }
        let who = Access { persona, recipient: &rec };
        let Some(store) = self.stores.get_mut(user) else { return Err(reply(status::LOCKED)) };
        let mut sc = sc;
        let mut cookies: Vec<(ItemId, Option<crate::cookie::Attrs>, String)> = tri_r!(store.list(&who))
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
        // ★ A script never replaces an HttpOnly cookie (RFC 6265bis
        // §5.7 step 21).
        if script && cookies.iter().any(|(_, a, n)| a.as_ref().is_some_and(|a| a.http_only) && same(a, n)) {
            return Err(reply(status::DENIED));
        }
        for (id, a, n) in &cookies {
            if same(a, n) {
                if let Some(a) = a {
                    sc.attrs.created = a.created;
                }
                tri_r!(store.delete(id, &who));
            }
        }
        cookies.retain(|(_, a, n)| !same(a, n));
        if sc.expired(now) {
            return Ok(None);
        }
        // Expired ones go; then, if the group is full, the oldest.
        let mut live: Vec<(ItemId, u64)> = Vec::new();
        for (id, a, _) in &cookies {
            match a {
                Some(a) if a.expires.is_none_or(|e| e > now) => live.push((*id, a.created)),
                _ => tri_r!(store.delete(id, &who)),
            }
        }
        live.sort_by_key(|c| c.1);
        let excess = (live.len() + 1).saturating_sub(crate::cookie::PER_GROUP);
        for (id, _) in live.iter().take(excess) {
            tri_r!(store.delete(id, &who));
        }
        let scope = sc.attrs.encode();
        let m = tri_r!(store.add(NewItem {
            persona,
            recipient: rec.clone(),
            class: Class::AfterFirstUnlock,
            kind: Kind::Cookie,
            provenance: Provenance::ServerSet,
            account: &sc.name,
            scope: &scope,
            value: &sc.value,
        }));
        Ok(Some(m.id))
    }

    /// The accounts Arca holds for a site (`broker_op::W_ACCOUNTS`): names
    /// of passkeys and web passwords, never a value.
    fn broker_accounts(&mut self, user: &str, body: &[u8]) -> Reply {
        let mut r = Reader::new(body);
        let Ok(site) = r.str(caps::HOST) else { return malformed() };
        if r.end().is_err() || Recipient::rp_id(site).is_err() {
            return malformed();
        }
        let Some(store) = self.stores.get(user) else { return reply(status::LOCKED) };
        if store.state() != arca_store::LockState::Unlocked {
            return reply(status::LOCKED);
        }
        let under = |h: &str| h == site || h.strip_suffix(site).is_some_and(|p| p.ends_with('.'));
        let mut found: Vec<(u8, String)> = Vec::new();
        for (persona, rec) in tri!(store.recipients()) {
            let kind = match &rec {
                Recipient::RpId(rp) if persona == PERSONA && under(rp) => broker_op::ACCOUNT_PASSKEY,
                Recipient::Endpoint { scheme: Scheme::Https, host, .. } if persona == broker_op::WEB_PERSONA && under(host) => broker_op::ACCOUNT_PASSWORD,
                _ => continue,
            };
            let want = if kind == broker_op::ACCOUNT_PASSKEY { Kind::P256 } else { Kind::Password };
            for m in tri!(store.list(&Access { persona: &persona, recipient: &rec })) {
                if m.kind == want && !found.iter().any(|(k, a)| *k == kind && *a == m.account) {
                    found.push((kind, m.account));
                }
            }
        }
        found.sort();
        found.truncate(broker_op::ACCOUNTS_MAX);
        let mut w = Writer::new();
        w.u32(found.len() as u32);
        for (k, a) in &found {
            w.u8(*k);
            w.str(a);
        }
        ok(w.buf)
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
    fn mail_exchange(&mut self, c: &CourierLabel, op: u16, body: &[u8]) -> Reply {
        use crate::oauth::{mail_partition, MICROSOFT};
        let mut r = Reader::new(body);
        let Ok(ticket) = r.array::<16>() else { return malformed() };
        let expected = Recipient::endpoint(Scheme::Https, MICROSOFT.token_host, 443, Some(&mail_partition(&ticket)));
        if c.persona != "web" || expected.ok().as_ref() != Some(&c.endpoint) { return reply(status::DENIED); }
        let Some(store) = self.stores.get_mut(&c.user).filter(|s| s.state() == LockState::Unlocked) else { return reply(status::LOCKED) };
        if op == courier_op::MAIL_CODE {
            let Ok(url) = r.str(8192) else { return malformed() };
            if r.end().is_err() { return malformed(); }
            let Some(form) = self.mail_sign_ins.exchange(&c.user, &ticket, url) else { return reply(status::DENIED) };
            return match LockedBuf::from_slice(&form) {
                Ok(buf) => Reply { status: status::OK, body: Body::Secret(buf) },
                Err(_) => reply(status::INTERNAL),
            };
        }
        let Ok(token) = r.bytes(crate::oauth::MAX_TOKEN) else { return malformed() };
        if r.end().is_err() || token.is_empty() || !token.iter().all(u8::is_ascii_graphic) { return malformed(); }
        let Some(client) = self.mail_sign_ins.registration.clone() else { return reply(status::DENIED) };
        let Some(publisher) = self.mail_sign_ins.mail_publisher else { return reply(status::DENIED) };
        let Some((account, renewal)) = self.mail_sign_ins.exchanging_account(&c.user, &ticket) else { return reply(status::DENIED) };
        let persona = app_persona(&publisher, "org.atrium.mail");
        let head = format!("oauth1 microsoft {client} ");
        let mut grant = tri!(LockedBuf::zeroed(head.len() + token.len()).map_err(Error::Io));
        grant.as_mut()[..head.len()].copy_from_slice(head.as_bytes());
        grant.as_mut()[head.len()..].copy_from_slice(token);
        let imap = tri!(Recipient::endpoint(Scheme::Imaps, MICROSOFT.imap.0, MICROSOFT.imap.1, None));
        let smtp = tri!(Recipient::endpoint(Scheme::Submission, MICROSOFT.smtp.0, MICROSOFT.smtp.1, None));
        if let Some((ids, expected_version)) = renewal {
            let (owner, version) = tri!(mail_refresh::renewal_owner(store, &persona, account, &ids));
            if version != expected_version { return reply(status::DENIED); }
            tri!(store.update(&owner, &Access { persona: &persona, recipient: &imap }, grant.as_ref(), None));
            self.mail_leases.retain(|_, lease| !lease.uses(&c.user, owner));
            self.mail_sign_ins.complete(&ticket, ids);
            return ok(vec![]);
        }
        let mut add = |recipient, value: &[u8], scope: &[u8]| store.add(NewItem { persona: &persona, recipient, class: Class::AfterFirstUnlock,
            kind: Kind::OAuthToken, provenance: Provenance::AppMinted, account, scope, value });
        let incoming = tri!(add(imap.clone(), grant.as_ref(), mail_refresh::OWNER)).id;
        let outgoing = match add(smtp, &mail_refresh::link(incoming), b"") {
            Ok(meta) => meta.id,
            Err(err) => {
                tri!(store.delete(&incoming, &Access { persona: &persona, recipient: &imap }));
                return map_err(err);
            }
        };
        let mut ids = [0; 32];
        ids[..16].copy_from_slice(&incoming.0);
        ids[16..].copy_from_slice(&outgoing.0);
        self.mail_sign_ins.complete(&ticket, ids);
        ok(vec![])
    }

    fn courier_op(&mut self, c: &CourierLabel, op: u16, body: &[u8], item: &mut Option<ItemId>) -> Reply {
        if matches!(op, courier_op::MAIL_REFRESH_BEGIN | courier_op::MAIL_REFRESH_FINISH | courier_op::MAIL_REFRESH_CANCEL) { return self.refresh_mail(c, op, body); }
        if matches!(op, courier_op::MAIL_CODE | courier_op::MAIL_GRANT) { return self.mail_exchange(c, op, body); }
        let verified = std::mem::take(&mut self.verified);
        let mut r = Reader::new(body);
        let who = Access { persona: &c.persona, recipient: &c.endpoint };
        match op {
            courier_op::FILL => {
                if matches!(&c.endpoint, Recipient::Endpoint { scheme, .. } if scheme.is_mail()) {
                    return reply(status::DENIED);
                }
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
            courier_op::LOGIN => {
                let parsed = (|| {
                    let src = r.u8()?;
                    let x = r.array::<16>()?;
                    r.end()?;
                    Ok::<_, Error>((src, x))
                })();
                let Ok((src, x)) = parsed else { return malformed() };
                if src != courier_op::SOURCE_ITEM {
                    return malformed();
                }
                if let Some(login) = self.handles.take_mail_login(&x, &c.user, &c.persona, &c.endpoint) {
                    return Reply { status: status::OK, body: Body::Secret(login) };
                }
                // Only a mail courier logs in; an https courier fills slots.
                let Recipient::Endpoint { scheme, .. } = &c.endpoint else { return reply(status::DENIED) };
                if !scheme.is_mail() {
                    return reply(status::DENIED);
                }
                let Some(store) = self.stores.get(&c.user) else { return reply(status::LOCKED) };
                let id = ItemId(x);
                *item = Some(id);
                let m = tri!(store.meta(&id, &who));
                if !matches!(m.kind, Kind::Password | Kind::OAuthToken) {
                    return reply(status::DENIED);
                }
                if m.class == Class::VerifyEachUse && !verified {
                    return reply(status::NEEDS_VERIFICATION);
                }
                let uv = verified.then(arca_store::UserVerified::vouched_by_vestibulum);
                let (_, v) = tri!(store.release(&id, &who, uv));
                if m.kind == Kind::OAuthToken && (m.scope == mail_refresh::OWNER || v.as_ref().starts_with(mail_refresh::LINK)) { return reply(status::DENIED); }
                let acct = m.account.as_bytes();
                let total = 4 + acct.len() + 4 + v.len() + 1;
                let mut out = tri!(LockedBuf::zeroed(total).map_err(Error::Io));
                let b = out.as_mut();
                b[..4].copy_from_slice(&(acct.len() as u32).to_le_bytes());
                b[4..4 + acct.len()].copy_from_slice(acct);
                let at = 4 + acct.len();
                b[at..at + 4].copy_from_slice(&(v.len() as u32).to_le_bytes());
                b[at + 4..total - 1].copy_from_slice(v.as_ref());
                b[total - 1] = if m.kind == Kind::OAuthToken { courier_op::LOGIN_OAUTH } else { courier_op::LOGIN_PASSWORD };
                Reply { status: status::OK, body: Body::Secret(out) }
            }
            courier_op::REFRESHED => {
                let parsed = (|| {
                    let src = r.u8()?;
                    let x = r.array::<16>()?;
                    let v = r.bytes(arca_store::model::caps::VALUE)?;
                    r.end()?;
                    Ok::<_, Error>((src, x, v))
                })();
                let Ok((src, x, v)) = parsed else { return malformed() };
                if src != courier_op::SOURCE_ITEM {
                    return malformed();
                }
                let Recipient::Endpoint { scheme, .. } = &c.endpoint else { return reply(status::DENIED) };
                if !scheme.is_mail() {
                    return reply(status::DENIED);
                }
                // Only an OAuth item of this courier's endpoint (as LOGIN),
                // and only its refresh token changes: the same provider and
                // client id, so a courier can't point the item elsewhere.
                let id = ItemId(x);
                *item = Some(id);
                let Some(store) = self.stores.get_mut(&c.user) else { return reply(status::LOCKED) };
                let m = tri!(store.meta(&id, &who));
                if m.kind != Kind::OAuthToken {
                    return reply(status::DENIED);
                }
                let (_, old) = tri!(store.release(&id, &who, None));
                if m.scope == mail_refresh::OWNER || old.as_ref().starts_with(mail_refresh::LINK) { return reply(status::DENIED); }
                let head = |g: &[u8]| -> Option<usize> {
                    let g = g.strip_prefix(b"oauth1 ")?;
                    let last = g.iter().rposition(|&b| b == b' ')?;
                    let tok = &g[last + 1..];
                    (!tok.is_empty() && tok.iter().all(|b| b.is_ascii_graphic())).then_some(7 + last)
                };
                let same = match (head(old.as_ref()), head(v)) {
                    (Some(a), Some(b)) => a == b && old.as_ref()[..a] == v[..b],
                    _ => false,
                };
                drop(old);
                if !same {
                    return reply(status::DENIED);
                }
                tri!(store.update(&id, &who, v, None));
                reply(status::OK)
            }
            courier_op::ANOMALY => {
                let parsed = (|| {
                    let code = r.u8()?;
                    let detail = r.str(courier_op::ANOMALY_DETAIL_MAX)?;
                    r.end()?;
                    Ok::<_, Error>((code, detail.to_string()))
                })();
                let Ok((code, detail)) = parsed else { return malformed() };
                // A name, never a value: only a command-shaped token is kept.
                let detail: String = detail.split_ascii_whitespace().next().unwrap_or("")
                    .chars().filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-').collect();
                let what = match code {
                    courier_op::anomaly::REFUSED_COMMAND => "refused-command",
                    courier_op::anomaly::MALFORMED => "malformed",
                    _ => return malformed(),
                };
                if let (Some(f), Recipient::Endpoint { scheme, host, port, .. }) = (self.audit.as_mut(), &c.endpoint) {
                    let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                    let _ = writeln!(f, "{t} ANOMALY courier:{}://{host}:{port}/{}@{} {what} {detail}", scheme.name(), c.persona, c.user);
                }
                ok(Vec::new())
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
                // A VPN sign-in window's jar (vpn.md §9.3, SAML): the
                // gateway's sign-on token is Crypta's, never the page's.
                let vpn = partition.as_deref().and_then(crate::jar::vpn_profile);
                if let Some(profile) = vpn.as_ref() {
                    if name == crate::jar::VPN_SSO_COOKIE || name == crate::jar::VPN_GP_TOKEN {
                        let (host, value) = (host.clone(), value.to_vec());
                        return self.vpn_token(profile, &host, &value, item);
                    }
                }
                if name == crate::jar::VPN_GP_TOKEN {
                    return reply(status::DENIED);
                }
                let now = (self.clock)();
                let sc = match crate::cookie::parse(name, value, attrs, host, path, now) {
                    Ok(sc) => sc,
                    // Ignored, as a browser ignores it; the courier drops the header either way.
                    Err(_) => return reply(status::DENIED),
                };
                let (user, persona, port, partition) = (c.user.clone(), c.persona.clone(), *port, partition.clone());
                match self.put_cookie(&user, &persona, port, partition.as_deref(), sc, now, false) {
                    Ok(id) => {
                        *item = id;
                        ok(Vec::new())
                    }
                    Err(r) => r,
                }
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

    // ── network shares ───────────────────────────────────────────────────

    /// The file manager's share ops (network-shares.md §5.2). The value it
    /// saves comes from its own credential field, by handle: the file
    /// manager never sees it, and only it may bind a typed password to a
    /// server, so another app can't have the user type a password and then
    /// send it to a server of its choosing.
    fn file_manager_op(&mut self, user: &str, me: &Recipient, op: u16, body: &[u8], item: &mut Option<ItemId>) -> Reply {
        use crate::handles::Source;
        use crate::share::{read_share, write_share};
        let mut r = Reader::new(body);
        macro_rules! field {
            ($e:expr) => {
                match $e {
                    Ok(v) => v,
                    Err(_) => return malformed(),
                }
            };
        }
        // Shares' items are while-unlocked (a lock unmounts shares, §7).
        if self.stores.get(user).is_none_or(|s| s.state() != LockState::Unlocked) {
            return reply(status::LOCKED);
        }
        match op {
            share_op::SH_SAVE => {
                let h = field!(r.array::<16>());
                let share = field!(read_share(&mut r));
                let account = field!(r.str(caps::ACCOUNT));
                field!(r.end());
                if account.is_empty() {
                    return malformed();
                }
                let Some(e) = self.handles.take(&h, user, PERSONA, me) else { return reply(status::DENIED) };
                let store = self.stores.get_mut(user).expect("checked above");
                if store.state() != LockState::Unlocked {
                    return reply(status::LOCKED);
                }
                let (value, provenance) = match e.source {
                    Source::Pending(v) => (v, if e.generated { Provenance::ArcaGenerated } else { Provenance::UserTyped }),
                    Source::Item(id) => {
                        // A password the user had saved for the file manager itself.
                        let mine = Access { persona: PERSONA, recipient: me };
                        let m = tri!(store.meta(&id, &mine));
                        if m.kind != Kind::Password {
                            return reply(status::DENIED);
                        }
                        (tri!(store.release(&id, &mine, None)).1, Provenance::UserTyped)
                    }
                };
                let who = Access { persona: PERSONA, recipient: &share };
                for m in tri!(store.list(&who)) {
                    if m.kind == Kind::Password && m.account == account {
                        tri!(store.delete(&m.id, &who));
                    }
                }
                let m = tri!(store.add(NewItem {
                    persona: PERSONA,
                    recipient: share.clone(),
                    class: Class::WhileUnlocked,
                    kind: Kind::Password,
                    provenance,
                    account,
                    scope: b"",
                    value: value.as_ref(),
                }));
                *item = Some(m.id);
                ok(m.id.0.to_vec())
            }
            share_op::SH_MINT => {
                let share = field!(read_share(&mut r));
                let account = field!(r.str(caps::ACCOUNT));
                field!(r.end());
                if account.is_empty() {
                    return malformed();
                }
                let store = self.stores.get_mut(user).expect("checked above");
                let seed = tri!(LockedBuf::random(32).map_err(Error::Io));
                let pk = signing_key(&seed).verifying_key().to_bytes();
                let m = tri!(store.add(NewItem {
                    persona: PERSONA,
                    recipient: share,
                    class: Class::WhileUnlocked,
                    kind: Kind::Ed25519,
                    provenance: Provenance::ArcaGenerated,
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
            share_op::SH_LIST => {
                field!(r.end());
                let store = &self.stores[user];
                let mut n = 0u32;
                let mut w = Writer::new();
                for (persona, rec) in tri!(store.recipients()) {
                    if persona != PERSONA || !matches!(rec, Recipient::Share { .. }) {
                        continue;
                    }
                    for m in tri!(store.list(&Access { persona: PERSONA, recipient: &rec })) {
                        write_share(&mut w, &rec);
                        w.raw(&m.id.0);
                        w.u16(m.kind as u16);
                        w.str(&m.account);
                        n += 1;
                    }
                }
                let mut out = Writer::new();
                out.u32(n);
                out.raw(&w.buf);
                ok(out.buf)
            }
            share_op::SH_DELETE => {
                let share = field!(read_share(&mut r));
                let id = ItemId(field!(r.array::<16>()));
                field!(r.end());
                *item = Some(id);
                let store = self.stores.get_mut(user).expect("checked above");
                tri!(store.delete(&id, &Access { persona: PERSONA, recipient: &share }));
                ok(Vec::new())
            }
            _ => reply(status::DENIED),
        }
    }

    /// The SSH agent (arca.md §9.1): keys in the owner's persona for the
    /// agent's recipient; a workspace sees and signs with only those
    /// granted to it by name.
    fn ssh_op(&mut self, op: u16, body: &[u8], item: &mut Option<ItemId>) -> Reply {
        use crate::ssh::{granted, token_account, valid_host, valid_workspace, with_grant, SERVICE};
        let verified = std::mem::take(&mut self.verified);
        let mut r = Reader::new(body);
        macro_rules! field {
            ($e:expr) => {
                match $e {
                    Ok(v) => v,
                    Err(_) => return malformed(),
                }
            };
        }
        let user = field!(r.str(caps::USER)).to_string();
        let persona = field!(r.str(caps::PERSONA)).to_string();
        if !valid_user(&user) || !arca_store::model::valid_persona(&persona) {
            return malformed();
        }
        let me = Recipient::Service(SERVICE.into());
        let who = Access { persona: &persona, recipient: &me };
        let Some(store) = self.stores.get_mut(&user).filter(|s| s.state() == LockState::Unlocked) else {
            return reply(status::LOCKED);
        };
        let pubkey = |store: &Store, m: &arca_store::model::ItemMeta| -> Result<Vec<u8>, Error> {
            if m.class == Class::VerifyEachUse {
                return crate::ssh::scope_public_key(m.kind, &m.scope).ok_or(Error::BadInput("SSH public key metadata"));
            }
            let (_, seed) = store.release(&m.id, &who, None)?;
            agent_public(m.kind, &seed)
        };
        match op {
            ssh_op::SA_MINT => {
                let comment = field!(r.str(ssh_op::COMMENT_MAX));
                let class = if r.remaining() == 0 {
                    Class::WhileUnlocked
                } else {
                    match field!(r.u8()) {
                        c if c == Class::VerifyEachUse as u8 => Class::VerifyEachUse,
                        c if c == Class::WhileUnlocked as u8 => Class::WhileUnlocked,
                        _ => return malformed(),
                    }
                };
                let kind = if r.remaining() == 0 {
                    Kind::Ed25519
                } else {
                    match field!(r.u16()) {
                        k if k == Kind::Ed25519 as u16 => Kind::Ed25519,
                        k if k == Kind::P256 as u16 => Kind::P256,
                        _ => return malformed(),
                    }
                };
                field!(r.end());
                if comment.chars().any(|c| c.is_control()) {
                    return malformed();
                }
                // A P-256 scalar must be below the order: a draw that isn't
                // (about 2^-32) is drawn again.
                let mut seed = tri!(LockedBuf::random(32).map_err(Error::Io));
                for _ in 0..8 {
                    if agent_public(kind, &seed).is_ok() {
                        break;
                    }
                    seed = tri!(LockedBuf::random(32).map_err(Error::Io));
                }
                let pk = tri!(agent_public(kind, &seed));
                let scope = if class == Class::VerifyEachUse { crate::ssh::public_scope(kind, &pk) } else { Vec::new() };
                let m = tri!(store.add(NewItem {
                    persona: &persona,
                    recipient: me.clone(),
                    class,
                    kind,
                    provenance: Provenance::ArcaGenerated,
                    account: comment,
                    scope: &scope,
                    value: seed.as_ref(),
                }));
                *item = Some(m.id);
                let mut w = Writer::new();
                w.raw(&m.id.0);
                w.raw(&pk);
                ok(w.buf)
            }
            ssh_op::SA_LIST | ssh_op::SA_KEYS => {
                let ws = if op == ssh_op::SA_KEYS {
                    let ws = field!(r.str(64));
                    if !valid_workspace(ws) {
                        return malformed();
                    }
                    Some(ws)
                } else {
                    None
                };
                field!(r.end());
                let mut n = 0u32;
                let mut w = Writer::new();
                for m in tri!(store.list(&who)) {
                    let listed = match ws {
                        Some(ws) => crate::ssh::is_key(m.kind) && granted(&m.scope, ws),
                        None => crate::ssh::is_key(m.kind) || m.kind == Kind::ApiKey,
                    };
                    if !listed {
                        continue;
                    }
                    w.raw(&m.id.0);
                    w.u16(m.kind as u16);
                    if crate::ssh::is_key(m.kind) {
                        w.bytes(&tri!(pubkey(store, &m)));
                    } else {
                        w.bytes(&[]);
                    }
                    w.str(&m.account);
                    if ws.is_none() {
                        w.str(&crate::ssh::grants(&m.scope).join("\n"));
                    }
                    n += 1;
                }
                let mut out = Writer::new();
                out.u32(n);
                out.raw(&w.buf);
                ok(out.buf)
            }
            ssh_op::SA_GRANT | ssh_op::SA_REVOKE => {
                let id = ItemId(field!(r.array::<16>()));
                let ws = field!(r.str(64));
                field!(r.end());
                if !valid_workspace(ws) {
                    return malformed();
                }
                *item = Some(id);
                let m = tri!(store.meta(&id, &who));
                tri!(store.set_scope(&id, &who, &with_grant(&m.scope, ws, op == ssh_op::SA_GRANT)));
                ok(Vec::new())
            }
            ssh_op::SA_TOKEN_ADD => {
                let host = field!(r.str(253));
                let username = field!(r.str(128));
                let token = field!(r.bytes(ssh_op::TOKEN_MAX));
                field!(r.end());
                if !valid_host(host) || username.is_empty() || username.chars().any(|c| c.is_control() || c == ' ') || token.is_empty() {
                    return malformed();
                }
                let account = token_account(host, username);
                // Replaced, grants kept.
                let mut scope = Vec::new();
                for m in tri!(store.list(&who)) {
                    if m.kind == Kind::ApiKey && m.account == account {
                        scope = m.scope.clone();
                        tri!(store.delete(&m.id, &who));
                    }
                }
                let m = tri!(store.add(NewItem {
                    persona: &persona,
                    recipient: me.clone(),
                    class: Class::WhileUnlocked,
                    kind: Kind::ApiKey,
                    provenance: Provenance::UserTyped,
                    account: &account,
                    scope: &scope,
                    value: token,
                }));
                *item = Some(m.id);
                ok(m.id.0.to_vec())
            }
            ssh_op::SA_TOKEN_GET => {
                let ws = field!(r.str(64));
                let host = field!(r.str(253));
                field!(r.end());
                if !valid_workspace(ws) || !valid_host(host) {
                    return malformed();
                }
                let prefix = format!("{host} ");
                let Some(m) = tri!(store.list(&who)).into_iter()
                    .find(|m| m.kind == Kind::ApiKey && m.account.starts_with(&prefix) && granted(&m.scope, ws)) else {
                    return reply(status::DENIED);
                };
                *item = Some(m.id);
                let (_, v) = tri!(store.release(&m.id, &who, None));
                let user = &m.account[prefix.len()..];
                let mut out = tri!(LockedBuf::zeroed(4 + user.len() + 4 + v.len()).map_err(Error::Io));
                let b = out.as_mut();
                b[..4].copy_from_slice(&(user.len() as u32).to_le_bytes());
                b[4..4 + user.len()].copy_from_slice(user.as_bytes());
                let o = 4 + user.len();
                b[o..o + 4].copy_from_slice(&(v.len() as u32).to_le_bytes());
                b[o + 4..].copy_from_slice(v.as_ref());
                Reply { status: status::OK, body: Body::Secret(out) }
            }
            ssh_op::SA_FORGET => {
                let ws = field!(r.str(64));
                field!(r.end());
                if !valid_workspace(ws) {
                    return malformed();
                }
                let mut n = 0u32;
                for m in tri!(store.list(&who)) {
                    if granted(&m.scope, ws) {
                        tri!(store.set_scope(&m.id, &who, &with_grant(&m.scope, ws, false)));
                        n += 1;
                    }
                }
                ok(n.to_le_bytes().to_vec())
            }
            ssh_op::SA_DELETE => {
                let id = ItemId(field!(r.array::<16>()));
                field!(r.end());
                *item = Some(id);
                tri!(store.delete(&id, &who));
                ok(Vec::new())
            }
            ssh_op::SA_SIGN => {
                let ws = field!(r.str(64));
                let id = ItemId(field!(r.array::<16>()));
                let msg = field!(r.bytes(MAX_SIGN_MSG));
                field!(r.end());
                *item = Some(id);
                let m = tri!(store.meta(&id, &who));
                // Not granted reads as not there: DENIED either way.
                if !crate::ssh::is_key(m.kind) || !granted(&m.scope, ws) {
                    return reply(status::DENIED);
                }
                let verified = verified.then(arca_store::UserVerified::vouched_by_vestibulum);
                let (_, seed) = tri!(store.release(&id, &who, verified));
                if m.kind == Kind::P256 {
                    // ECDSA with the SHA-256 prehash: what ecdsa-sha2-nistp256
                    // and a bundle's atrium.toml.sig both are.
                    use p256::ecdsa::signature::Signer as _;
                    let Ok(sk) = p256::ecdsa::SigningKey::from_slice(seed.as_ref()) else { return reply(status::INTERNAL) };
                    drop(seed);
                    let sig: p256::ecdsa::Signature = sk.sign(msg);
                    return ok(sig.to_bytes().to_vec());
                }
                let sk = signing_key(&seed);
                drop(seed);
                ok(sk.sign(msg).to_bytes().to_vec())
            }
            _ => reply(status::DENIED),
        }
    }

    /// A share server asks after its handshake, naming the identity the
    /// server proved; Arca answers only if that is the identity pinned in
    /// the share server's own label, and only with that share's items.
    fn share_op(&mut self, sl: &ShareLabel, op: u16, body: &[u8], item: &mut Option<ItemId>) -> Reply {
        let mut r = Reader::new(body);
        let Ok(key) = r.array::<32>() else { return malformed() };
        if !matches!(op, share_op::SH_ITEMS | share_op::SH_FILL | share_op::SH_PUBKEY | share_op::SH_SIGN) {
            return reply(status::DENIED);
        }
        if &key != sl.key() {
            return reply(status::DENIED);
        }
        let Some(store) = self.stores.get(&sl.user).filter(|s| s.state() == LockState::Unlocked) else {
            return reply(status::LOCKED);
        };
        let who = Access { persona: PERSONA, recipient: &sl.share };
        if op == share_op::SH_ITEMS {
            if r.end().is_err() {
                return malformed();
            }
            let items = tri!(store.list(&who));
            let mut w = Writer::new();
            w.u32(items.len() as u32);
            for m in items {
                w.raw(&m.id.0);
                w.u16(m.kind as u16);
                w.str(&m.account);
            }
            return ok(w.buf);
        }
        let Ok(id) = r.array::<16>() else { return malformed() };
        let id = ItemId(id);
        *item = Some(id);
        let msg = if op == share_op::SH_SIGN {
            match r.bytes(MAX_SIGN_MSG) {
                Ok(m) => m,
                Err(_) => return malformed(),
            }
        } else {
            &[][..]
        };
        if r.end().is_err() {
            return malformed();
        }
        let m = tri!(store.meta(&id, &who));
        match (op, m.kind) {
            (share_op::SH_FILL, Kind::Password) => {
                let (_, v) = tri!(store.release(&id, &who, None));
                let mut out = tri!(LockedBuf::zeroed(4 + v.len()).map_err(Error::Io));
                out.as_mut()[..4].copy_from_slice(&(v.len() as u32).to_le_bytes());
                out.as_mut()[4..].copy_from_slice(v.as_ref());
                Reply { status: status::OK, body: Body::Secret(out) }
            }
            (share_op::SH_PUBKEY | share_op::SH_SIGN, Kind::Ed25519) => {
                let (_, seed) = tri!(store.release(&id, &who, None));
                let sk = signing_key(&seed);
                drop(seed);
                if op == share_op::SH_PUBKEY {
                    ok(sk.verifying_key().to_bytes().to_vec())
                } else {
                    ok(sk.sign(msg).to_bytes().to_vec())
                }
            }
            // A key's seed is never filled; a password signs nothing.
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

/// The public half of a minted key item.
fn public_of(kind: Kind, seed: &LockedBuf) -> [u8; 32] {
    match kind {
        Kind::WireGuardKey => {
            let mut b = [0u8; 32];
            b.copy_from_slice(seed.as_ref());
            let s = x25519_dalek::StaticSecret::from(b);
            zeroize_array(&mut b);
            x25519_dalek::PublicKey::from(&s).to_bytes()
        }
        _ => signing_key(seed).verifying_key().to_bytes(),
    }
}

/// X25519 with a stored private key; `None` for a low-order point (an
/// all-zero result), which WireGuard refuses too.
fn x25519(secret: &LockedBuf, point: &[u8; 32]) -> Option<zeroize::Zeroizing<[u8; 32]>> {
    let mut b = [0u8; 32];
    b.copy_from_slice(secret.as_ref());
    let s = x25519_dalek::StaticSecret::from(b);
    zeroize_array(&mut b);
    let shared = s.diffie_hellman(&x25519_dalek::PublicKey::from(*point));
    shared.was_contributory().then(|| zeroize::Zeroizing::new(shared.to_bytes()))
}

/// The public half of an agent key: Ed25519's 32 bytes, or P-256's
/// uncompressed SEC1 point (65 bytes). An error for a P-256 scalar that is
/// zero or not below the order.
fn agent_public(kind: Kind, seed: &LockedBuf) -> Result<Vec<u8>, Error> {
    if kind == Kind::P256 {
        let sk = p256::ecdsa::SigningKey::from_slice(seed.as_ref()).map_err(|_| Error::BadInput("P-256 scalar"))?;
        return Ok(sk.verifying_key().to_encoded_point(false).as_bytes().to_vec());
    }
    Ok(signing_key(seed).verifying_key().to_bytes().to_vec())
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

#[cfg(all(feature = "legacy-smime-integration", any(target_os = "freebsd", all(test, feature = "host-openssl-tests"))))]
pub use smime::integration;
mod smime;
