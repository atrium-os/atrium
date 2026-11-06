//! What Arca holds (arca.md §2, §3, §6.2).

use crate::codec::{Reader, Writer};
use crate::Error;

/// Caps on every variable field, so no record can be made to allocate much.
pub mod caps {
    pub const PERSONA: usize = 64;
    pub const USER: usize = 64;
    pub const HOST: usize = 253;
    pub const APP_ID: usize = 255;
    pub const RP_ID: usize = 253;
    pub const ACCOUNT: usize = 512;
    pub const SCOPE: usize = 4096;
    /// A cookie is at most 4 KiB on the web; passkeys and SSH keys are small.
    /// Larger secrets (a certificate chain) are not Arca's business.
    pub const VALUE: usize = 16 * 1024;
    /// Items per recipient group (cookie count limits per origin are tighter
    /// and enforced by the web profile, A2).
    pub const ITEMS_PER_GROUP: usize = 1024;
}

/// When an item is usable (arca.md §6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Class {
    /// From the first unlock after boot until shutdown, including while
    /// the screen is locked. Cookies, tokens, push keys.
    AfterFirstUnlock = 1,
    /// Only while the session is unlocked; the key is evicted at lock.
    /// Passwords, refresh tokens, API keys.
    WhileUnlocked = 2,
    /// Each use passes a Vestibulum check.
    VerifyEachUse = 3,
}

impl Class {
    pub const ALL: [Class; 3] = [Class::AfterFirstUnlock, Class::WhileUnlocked, Class::VerifyEachUse];
    pub fn from_u8(v: u8) -> Result<Class, Error> {
        match v {
            1 => Ok(Class::AfterFirstUnlock),
            2 => Ok(Class::WhileUnlocked),
            3 => Ok(Class::VerifyEachUse),
            _ => Err(Error::Corrupt("unknown class")),
        }
    }
}

/// Key or bearer (arca.md §2). The shape decides how an item may be handed
/// over: a key is never released, only used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Shape {
    Key = 1,
    Bearer = 2,
}

/// What an item is. The shape follows from the kind, so an item can't be a
/// "key" that the daemon then releases.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Kind {
    Password = 1,
    Cookie = 2,
    OAuthToken = 3,
    ApiKey = 4,
    HttpAuth = 5,
    Totp = 6,
    /// An ed25519 signing key (per-service keypairs, insula.md §13.3).
    Ed25519 = 0x100,
    /// A P-256 key (passkeys, ES256).
    P256 = 0x101,
    /// A symmetric key released only as an unwrap/derive operation
    /// (the Loculus master key).
    Symmetric = 0x102,
}

impl Kind {
    pub fn from_u16(v: u16) -> Result<Kind, Error> {
        use Kind::*;
        Ok(match v {
            1 => Password,
            2 => Cookie,
            3 => OAuthToken,
            4 => ApiKey,
            5 => HttpAuth,
            6 => Totp,
            0x100 => Ed25519,
            0x101 => P256,
            0x102 => Symmetric,
            _ => return Err(Error::Corrupt("unknown kind")),
        })
    }
    pub fn shape(self) -> Shape {
        if (self as u16) >= 0x100 { Shape::Key } else { Shape::Bearer }
    }
}

/// Who created an item (arca.md §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Provenance {
    UserTyped = 1,
    ServerSet = 2,
    AppMinted = 3,
    ArcaGenerated = 4,
    Migrated = 5,
}

impl Provenance {
    pub fn from_u8(v: u8) -> Result<Provenance, Error> {
        use Provenance::*;
        Ok(match v {
            1 => UserTyped,
            2 => ServerSet,
            3 => AppMinted,
            4 => ArcaGenerated,
            5 => Migrated,
            _ => return Err(Error::Corrupt("unknown provenance")),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Scheme {
    Https = 1,
    /// Allowed as a recipient only for items created from `http`; an item
    /// bound to `https` is never sent over `http` because the schemes are
    /// different recipients.
    Http = 2,
}

/// The one party that may receive an item or its effect (arca.md §3).
/// Arca matches recipients by exact equality of this value, and refuses
/// non-canonical spellings at construction so that there is exactly one
/// encoding per recipient.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Recipient {
    /// A network endpoint, verified by the courier's own TLS handshake.
    /// `partition` is the top-level site for web items (§8.2), or None.
    Endpoint { scheme: Scheme, host: String, port: u16, partition: Option<String> },
    /// An app, verified by its channel: publisher key + app id.
    App { publisher: [u8; 32], app_id: String },
    /// The user's own eyes, after a Vestibulum check.
    User,
    /// A WebAuthn relying party ID: the key is used, never released.
    RpId(String),
}

/// A lowercase ASCII DNS name (already IDNA-encoded by whoever verified
/// it), no trailing dot, no empty labels; or a bracketed IPv6 / dotted IPv4
/// literal is refused here too: Arca binds only to names TLS can verify.
fn valid_host(h: &str) -> bool {
    if h.is_empty() || h.len() > caps::HOST || h.ends_with('.') || h.starts_with('.') {
        return false;
    }
    if !h.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-' || c == b'.') {
        return false;
    }
    if h.split('.').any(|l| l.is_empty() || l.len() > 63 || l.starts_with('-') || l.ends_with('-')) {
        return false;
    }
    // An all-numeric last label is an IPv4 literal, not a name.
    !h.rsplit('.').next().unwrap().bytes().all(|c| c.is_ascii_digit())
}

fn valid_app_id(a: &str) -> bool {
    !a.is_empty()
        && a.len() <= caps::APP_ID
        && a.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'.' || c == b'-' || c == b'_')
}

impl Recipient {
    pub fn endpoint(scheme: Scheme, host: &str, port: u16, partition: Option<&str>) -> Result<Recipient, Error> {
        if !valid_host(host) || port == 0 {
            return Err(Error::BadRecipient);
        }
        if let Some(p) = partition {
            if !valid_host(p) {
                return Err(Error::BadRecipient);
            }
        }
        Ok(Recipient::Endpoint { scheme, host: host.into(), port, partition: partition.map(Into::into) })
    }

    pub fn app(publisher: [u8; 32], app_id: &str) -> Result<Recipient, Error> {
        if !valid_app_id(app_id) {
            return Err(Error::BadRecipient);
        }
        Ok(Recipient::App { publisher, app_id: app_id.into() })
    }

    pub fn rp_id(id: &str) -> Result<Recipient, Error> {
        if !valid_host(id) {
            return Err(Error::BadRecipient);
        }
        Ok(Recipient::RpId(id.into()))
    }

    /// The canonical bytes: the file encoding, and part of every AAD.
    pub fn encode(&self, w: &mut Writer) {
        match self {
            Recipient::Endpoint { scheme, host, port, partition } => {
                w.u8(1);
                w.u8(*scheme as u8);
                w.str(host);
                w.u16(*port);
                match partition {
                    None => w.u8(0),
                    Some(p) => {
                        w.u8(1);
                        w.str(p);
                    }
                }
            }
            Recipient::App { publisher, app_id } => {
                w.u8(2);
                w.raw(publisher);
                w.str(app_id);
            }
            Recipient::User => w.u8(3),
            Recipient::RpId(id) => {
                w.u8(4);
                w.str(id);
            }
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut w = Writer::new();
        self.encode(&mut w);
        w.buf
    }

    /// Decode and re-validate: a decoded recipient obeys the same rules as
    /// a constructed one.
    pub fn decode(r: &mut Reader<'_>) -> Result<Recipient, Error> {
        match r.u8()? {
            1 => {
                let scheme = match r.u8()? {
                    1 => Scheme::Https,
                    2 => Scheme::Http,
                    _ => return Err(Error::Corrupt("scheme")),
                };
                let host = r.str(caps::HOST)?.to_string();
                let port = r.u16()?;
                let partition = match r.u8()? {
                    0 => None,
                    1 => Some(r.str(caps::HOST)?.to_string()),
                    _ => return Err(Error::Corrupt("partition flag")),
                };
                Recipient::endpoint(scheme, &host, port, partition.as_deref())
                    .map_err(|_| Error::Corrupt("recipient"))
            }
            2 => {
                let publisher = r.array::<32>()?;
                let app_id = r.str(caps::APP_ID)?;
                Recipient::app(publisher, app_id).map_err(|_| Error::Corrupt("recipient"))
            }
            3 => Ok(Recipient::User),
            4 => Recipient::rp_id(r.str(caps::RP_ID)?).map_err(|_| Error::Corrupt("recipient")),
            _ => Err(Error::Corrupt("recipient tag")),
        }
    }
}

pub fn valid_persona(p: &str) -> bool {
    !p.is_empty() && p.len() <= caps::PERSONA && p.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}

pub fn valid_user(u: &str) -> bool {
    !u.is_empty() && u.len() <= caps::USER && u.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_' || c == b'.')
}

/// 16 random bytes. Ids carry no meaning and are never derived from content.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ItemId(pub [u8; 16]);

impl ItemId {
    pub fn random() -> ItemId {
        let mut b = [0u8; 16];
        getrandom::getrandom(&mut b).expect("getrandom");
        ItemId(b)
    }
    pub fn hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl std::fmt::Debug for ItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "item/{}", self.hex())
    }
}

/// Everything about an item except its value. Safe to log apart from
/// `account`, which may be an email address: logs use the id only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemMeta {
    pub id: ItemId,
    pub version: u64,
    pub persona: String,
    pub recipient: Recipient,
    pub class: Class,
    pub kind: Kind,
    pub provenance: Provenance,
    /// The username or label; for cookies, the cookie name.
    pub account: String,
    /// Optional narrowing (methods, paths, cookie attributes), opaque to
    /// the store; interpreted by the profile that wrote it.
    pub scope: Vec<u8>,
    /// Unix seconds.
    pub created: u64,
    pub updated: u64,
}

/// A new item as a client hands it in. The value is copied into locked
/// memory by the store; the caller zeroizes its own copy.
pub struct NewItem<'a> {
    pub persona: &'a str,
    pub recipient: Recipient,
    pub class: Class,
    pub kind: Kind,
    pub provenance: Provenance,
    pub account: &'a str,
    pub scope: &'a [u8],
    pub value: &'a [u8],
}
