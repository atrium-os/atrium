//! Arca's wire (arca.md §4, §9): a fixed, length-prefixed binary schema.
//!
//! A frame is `u32 LE length | u16 LE op | body`, at most [`MAX_FRAME`]
//! bytes, read straight into locked memory (requests can carry a root key
//! or a value). A reply is `u32 LE length | u8 status | body`.
//!
//! Arca does not use Aqueduct's envelope: it has no use for content-
//! addressed uploads or a 16 MiB payload, and its receive buffers must be
//! locked. The grammar is flat: every field is a fixed-size integer, a
//! fixed array or a capped byte string, decoded with
//! [`arca_store::codec::Reader`].

use std::io::{self, Read, Write};

use arca_store::secret::LockedBuf;

/// The largest request or reply. A value is at most 16 KiB
/// (`arca_store::model::caps::VALUE`); a SIGN message at most 16 KiB.
pub const MAX_FRAME: usize = 64 * 1024;
/// The largest message SIGN accepts.
pub const MAX_SIGN_MSG: usize = 16 * 1024;

/// Opcodes. Apps use 0x01..; Vestibulum 0x40..; Arca's trusted UI 0x60..
pub mod op {
    /// Mint a key item for the calling app: `u16 kind | u8 class | str account`
    /// → `id[16] | pubkey[32]`.
    pub const MINT: u16 = 0x01;
    /// `id[16]` → `pubkey[32]`.
    pub const PUBKEY: u16 = 0x02;
    /// `id[16] | bytes msg` → `sig[64]`.
    pub const SIGN: u16 = 0x03;
    /// Store a bearer secret the app was given, bound to the app itself
    /// (app-held): `u16 kind | u8 class | str account | bytes value` → `id[16]`.
    pub const STORE: u16 = 0x04;
    /// `id[16]` → `bytes value`. Only bearer items, only with a user grant.
    pub const RELEASE: u16 = 0x05;
    /// → `u32 n | n × (id[16] | u16 kind | u8 class | u32 flags | str account)`.
    pub const LIST: u16 = 0x06;
    /// `id[16]` → nothing.
    pub const DELETE: u16 = 0x07;
    /// Store a bearer secret for one endpoint, courier-attached (arca.md
    /// §4.3 class 1): `u16 kind | u8 class | str account | endpoint |
    /// bytes value` → `id[16]`. Only this app's couriers to that endpoint
    /// can use it; the app never gets it back.
    pub const STORE_FOR: u16 = 0x08;
    /// `endpoint` → as LIST, for this app's items bound to that endpoint.
    pub const LIST_FOR: u16 = 0x09;

    /// `str user | root[32]` → nothing. First login: make the user's store.
    pub const V_CREATE: u16 = 0x40;
    /// `str user | root[32]` → nothing.
    pub const V_UNLOCK: u16 = 0x41;
    /// `str user` → nothing.
    pub const V_LOCK: u16 = 0x42;
    /// `str user | old[32] | new[32]` → nothing.
    pub const V_REWRAP: u16 = 0x43;
    /// `str user` → `u8 state` (0 none, 1 sealed, 2 locked, 3 unlocked).
    pub const V_STATE: u16 = 0x44;
    /// `str user` → `salt[16] | u32 m_kib | u32 t_cost | u32 p_cost`: how
    /// Vestibulum derives this user's root key without a TPM. Made on first
    /// ask. Not secret, but only Vestibulum may ask (it tells who has a store).
    pub const V_KDF: u16 = 0x45;

    /// The user, in Arca's trusted UI, grants an app-held item to its app:
    /// `str user | publisher[32] | str app_id | id[16]` → nothing.
    pub const U_GRANT: u16 = 0x60;
    /// Revoke that grant, same body.
    pub const U_REVOKE: u16 = 0x61;

    // The credential field (arca-autofill.md). Every body starts with the
    // field's parent as arca-ui resolved it from the launch registry:
    // `str user | publisher[32] | str app_id`, the item recipient.

    /// → `u32 n | n × (id[16] | str account)`: the passwords the chip offers.
    pub const U_ACCOUNTS: u16 = 0x62;
    /// `parent | bytes value` → `handle[16]`: hold what the user typed.
    pub const U_HOLD: u16 = 0x63;
    /// `parent | id[16]` → `handle[16]`: the item the user picked on the chip.
    pub const U_HANDLE: u16 = 0x64;
    /// `parent` → `handle[16]`: a strong password made inside Arca, held
    /// pending; nobody sees it unless the user reveals it.
    pub const U_GENERATE: u16 = 0x65;
    /// `handle[16]` → nothing: the field was cleared or detached.
    pub const U_DROP: u16 = 0x66;
}

/// Web broker ops (0x70..): only the configured broker (arca.md §8.2).
pub mod broker_op {
    /// `endpoint | str partition | u8 method (the courier schema's) | str
    /// path | u8 reason (1 navigation, 2 subresource, 3 form) | str
    /// initiator host ("" = the reader's own) | u8 gesture [| str query]` →
    /// `ticket[16]`. The ticket is for exactly that method, path and query
    /// (none if omitted).
    pub const W_TICKET: u16 = 0x70;
    /// `str partition | u8 prefix` → `u32 groups`. Forget web cookie jars
    /// (arca.md §8.6, navigator-ui.md §2.4): with `prefix` 0, the jar of
    /// exactly that partition ("Forget this site" in a persona); with 1,
    /// every partition beginning with it, where it must be an Unrecorded
    /// jar's [`UNRECORDED`] prefix (`u-` and 16 hex digits, then `.`) or
    /// [`UNRECORDED`] alone (every Unrecorded jar: the broker's start, so
    /// a crash leaves none behind). Each jar's groups go with their data
    /// keys. Needs the store unlocked.
    pub const W_FORGET: u16 = 0x71;
    /// The web couriers' persona (arca-courierd's `WEB_PERSONA`).
    pub const WEB_PERSONA: &str = "web";
    /// What an Unrecorded trail's partitions begin with (navigator-ui.md
    /// §2.4): `u-<16 hex>.<site>`, one jar per trail.
    pub const UNRECORDED: &str = "u-";

    /// Whether `p` may be a `W_FORGET` prefix.
    pub fn forget_prefix_ok(p: &str) -> bool {
        p == UNRECORDED
            || (p.len() == UNRECORDED.len() + 17
                && p.starts_with(UNRECORDED)
                && p.ends_with('.')
                && p[UNRECORDED.len()..p.len() - 1].bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
    }
}

/// Sigillum's ops (0x90..): only `sigillumd` (the `ARCA_SERVICES` entry
/// named [`crate::passkey::SIGILLUM`]). Arca is the platform authenticator
/// (arca.md §8.4, sigillum.md §9.2): a passkey is a P-256 key item whose
/// recipient is the RP ID, in the user's main persona, class
/// while-unlocked. Its credential id is its item id. Arca builds the
/// authenticator data itself (RP ID hash from the item's recipient, flags,
/// counter 0, AAGUID zero), so `sigillumd` can't have it sign for another
/// RP. `sigillumd` runs the sheet and the press first; `u8 uv` is 1 only
/// after its `VERIFY_USER` succeeded.
pub mod sigillum_op {
    /// `str user | str rp_id | bytes user_handle (1..=64) | str name | str
    /// display_name | u8 uv` → `id[16] | bytes auth_data` (with the
    /// attested credential data). A passkey for the same RP and user
    /// handle is replaced, as platform authenticators do.
    pub const P_CREATE: u16 = 0x90;
    /// `str user | str rp_id | id[16] | u8 uv | client_data_hash[32]` →
    /// `bytes auth_data | bytes signature (DER) | bytes user_handle`.
    pub const P_ASSERT: u16 = 0x91;
    /// `str user | str rp_id` → `u32 n | n × (id[16] | bytes user_handle |
    /// str name | str display_name)`, for the sheet and for
    /// `excludeCredentials`.
    pub const P_LIST: u16 = 0x92;
}

/// Network shares' ops (0xA0..), network-shares.md §5. A share is
/// `crate::share::read_share`'s wire. Items for a share live in the user's
/// main persona with the share as recipient, class while-unlocked (a lock
/// unmounts shares, §7).
pub mod share_op {
    /// File manager: save what the user typed (or picked) in the file
    /// manager's own credential field as the password for a share:
    /// `handle[16] | share | str account` → `id[16]`. The handle is redeemed
    /// here, for the file manager itself; an item for the same share and
    /// account is replaced.
    pub const SH_SAVE: u16 = 0xA0;
    /// File manager: mint an Ed25519 key for a share: `share | str account`
    /// → `id[16] | pubkey[32]`. The key never leaves Arca; the share server
    /// has Arca sign with it.
    pub const SH_MINT: u16 = 0xA1;
    /// File manager: → `u32 n | n × (share | id[16] | u16 kind | str
    /// account)`, the saved shares' items.
    pub const SH_LIST: u16 = 0xA2;
    /// File manager: `share | id[16]` → nothing.
    pub const SH_DELETE: u16 = 0xA3;

    /// Share server, after its handshake: `key[32]` (the identity the
    /// server proved) → `u32 n | n × (id[16] | u16 kind | str account)`.
    pub const SH_ITEMS: u16 = 0xA8;
    /// Share server: `key[32] | id[16]` → `bytes value`, a password.
    pub const SH_FILL: u16 = 0xA9;
    /// Share server: `key[32] | id[16]` → `pubkey[32]`.
    pub const SH_PUBKEY: u16 = 0xAA;
    /// Share server: `key[32] | id[16] | bytes msg` → `sig[64]`, the SSH
    /// user-auth signature (RFC 4252 §7) with the share's key.
    pub const SH_SIGN: u16 = 0xAB;
}

/// Courier ops (0x80..): only a courier's uid, for its own endpoint.
pub mod courier_op {
    /// After TLS is up: `u8 n | n × (u8 source | ref[16])` → `n × bytes
    /// value`, all or nothing. Source 1 = an item id, 2 = a credential
    /// handle (redeemed once).
    pub const FILL: u16 = 0x80;
    /// A `Set-Cookie` from the endpoint: `str name | bytes value | bytes
    /// attributes (the text after the first ';') | str request path` →
    /// nothing. Arca validates it (arcad::cookie); DENIED = ignored.
    pub const SET_COOKIE: u16 = 0x81;
    /// After TLS is up: `str request path [| ticket[16] | u8 method | str
    /// query]` → the `Cookie` header value for that request (empty: none).
    /// An app's courier sends no ticket and gets every matching cookie. A
    /// web courier (its label has a partition) must redeem the broker's
    /// ticket for exactly this request — its method, path and query; SameSite
    /// goes by the ticket. A web courier's SET_COOKIE carries the same
    /// ticket at its end.
    pub const COOKIES: u16 = 0x82;

    pub const SOURCE_ITEM: u8 = 1;
    pub const SOURCE_HANDLE: u8 = 2;
    /// Most slots one request fills.
    pub const MAX_SLOTS: usize = 8;
}

/// An endpoint on the wire: `u8 scheme (1 https) | str host | u16 port`.
pub fn read_endpoint(r: &mut arca_store::codec::Reader<'_>) -> Result<arca_store::Recipient, arca_store::Error> {
    use arca_store::model::caps;
    let scheme = r.u8()?;
    let host = r.str(caps::HOST)?;
    let port = r.u16()?;
    if scheme != arca_store::Scheme::Https as u8 {
        return Err(arca_store::Error::BadRecipient);
    }
    arca_store::Recipient::endpoint(arca_store::Scheme::Https, host, port, None)
}

pub fn write_endpoint(w: &mut arca_store::codec::Writer, host: &str, port: u16) {
    w.u8(arca_store::Scheme::Https as u8);
    w.str(host);
    w.u16(port);
}

/// Reply status. Every refusal about an item that isn't the caller's own
/// is `DENIED`, whether it exists or not.
pub mod status {
    pub const OK: u8 = 0;
    /// EACCES: not yours, no such item, or this op isn't for your kind of peer.
    pub const DENIED: u8 = 1;
    pub const LOCKED: u8 = 2;
    pub const NEEDS_VERIFICATION: u8 = 3;
    pub const MALFORMED: u8 = 4;
    /// The peer uid is not a launched app Arca will serve.
    pub const UNKNOWN_PEER: u8 = 5;
    pub const FULL: u8 = 6;
    pub const INTERNAL: u8 = 7;
    /// Your own app-held item, but the user hasn't granted it to you.
    pub const GRANT_REQUIRED: u8 = 8;
    pub const WRONG_ROOT: u8 = 9;
    pub const EXISTS: u8 = 10;
    /// A key item: it is used, never released.
    pub const NOT_RELEASABLE: u8 = 11;
    /// The op isn't supported for this kind or class (yet).
    pub const UNSUPPORTED: u8 = 12;
    /// Vestibulum only: this user has no store yet (V_CREATE it).
    pub const NO_STORE: u8 = 13;
}

/// Read one frame into locked memory. `Ok(None)` at a clean end of stream.
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<LockedBuf>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let n = u32::from_le_bytes(len) as usize;
    if !(2..=MAX_FRAME).contains(&n) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame length"));
    }
    let mut buf = LockedBuf::zeroed(n)?;
    r.read_exact(buf.as_mut())?;
    Ok(Some(buf))
}

/// Write a reply frame. The body may be secret (RELEASE); it goes straight
/// from locked memory to the socket.
pub fn write_reply(w: &mut impl Write, status: u8, body: &[u8]) -> io::Result<()> {
    let n = 1 + body.len();
    if n > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "reply too large"));
    }
    w.write_all(&(n as u32).to_le_bytes())?;
    w.write_all(&[status])?;
    w.write_all(body)?;
    w.flush()
}

/// Write a request frame (clients and tests).
pub fn write_request(w: &mut impl Write, op: u16, body: &[u8]) -> io::Result<()> {
    let n = 2 + body.len();
    if n > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "request too large"));
    }
    w.write_all(&(n as u32).to_le_bytes())?;
    w.write_all(&op.to_le_bytes())?;
    w.write_all(body)?;
    w.flush()
}

/// Read a reply frame (clients and tests): `(status, body)`.
pub fn read_reply(r: &mut impl Read) -> io::Result<(u8, LockedBuf)> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let n = u32::from_le_bytes(len) as usize;
    if !(1..=MAX_FRAME).contains(&n) {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame length"));
    }
    let mut st = [0u8; 1];
    r.read_exact(&mut st)?;
    let mut body = LockedBuf::zeroed(n - 1)?;
    r.read_exact(body.as_mut())?;
    Ok((st[0], body))
}
