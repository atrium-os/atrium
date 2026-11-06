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

/// Courier ops (0x80..): only a courier's uid, for its own endpoint.
pub mod courier_op {
    /// After TLS is up: `u8 n | n × (u8 source | ref[16])` → `n × bytes
    /// value`, all or nothing. Source 1 = an item id, 2 = a credential
    /// handle (redeemed once).
    pub const FILL: u16 = 0x80;
    /// A `Set-Cookie` from the endpoint, as a record: `str name | bytes
    /// value | bytes attributes` → nothing. Cookie rules proper are A2.
    pub const SET_COOKIE: u16 = 0x81;

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
