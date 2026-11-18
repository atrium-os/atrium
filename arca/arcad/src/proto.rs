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
    /// The Wi-Fi key service only (`WIFI_SERVICE`, curia.md §4):
    /// `handle[16]` → `bytes value`, once. Redeems what the user typed into
    /// a Wi-Fi passphrase field (held with `HOLD_FOR_WIFI`) and nothing
    /// else: not a stored item, not another field's value.
    pub const TAKE_WIFI: u16 = 0x0A;
    /// Keep what the user typed into (or picked on) this app's credential
    /// field as a password for one or more of this app's endpoints (mail.md
    /// §5.1: one password for the IMAP and the SMTP server), the app never
    /// holding it: `handle[16] | u8 class | str account | u8 n (1..=4) | n ×
    /// endpoint` → `n × id[16]`. The handle is redeemed once, for this app.
    pub const KEEP_FOR: u16 = 0x0B;
    /// Most endpoints one `KEEP_FOR` binds.
    pub const KEEP_FOR_MAX: usize = 4;
    /// Delete one of this app's items bound to an endpoint (what
    /// `KEEP_FOR` or a courier's `GRANTED` made; mail.md §5.1: removing an
    /// account removes its password): `endpoint | id[16]` → nothing.
    pub const DELETE_FOR: u16 = 0x0C;
    /// The service name in `ARCA_SERVICES` that may `TAKE_WIFI`.
    pub const WIFI_SERVICE: &str = "aura-keys";
    /// A trailing byte on `U_HOLD`: the field is a Wi-Fi passphrase field.
    pub const HOLD_FOR_WIFI: u8 = 1;
    /// A trailing byte on `U_HOLD`: the field is a stick recovery code's
    /// (tessera-encryption.md §7.4); only `VK_ADOPT` redeems it.
    pub const HOLD_FOR_RECOVERY: u8 = 2;

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
    /// `parent | bytes value [| u8 HOLD_FOR_WIFI | HOLD_FOR_RECOVERY]` →
    /// `handle[16]`: hold what the user typed. With `HOLD_FOR_WIFI`, only
    /// the Wi-Fi key service can redeem it (`TAKE_WIFI`); with
    /// `HOLD_FOR_RECOVERY`, only `VK_ADOPT`. No courier, fill or save
    /// takes either.
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
    /// `str partition` → `u32 cookies`. "Sign out here" (navigator-ui.md
    /// §6.2): the session cookies (no expiry) of exactly that partition
    /// go; cookies with an expiry stay, and so does the jar's data key.
    /// Needs the store unlocked.
    pub const W_SIGN_OUT: u16 = 0x72;
    /// `str site` → `u32 n | n × (u8 kind | str account)`: the accounts
    /// Arca holds for a site, for the presence menu (navigator-ui.md §6.2).
    /// Kind [`ACCOUNT_PASSKEY`]: passkeys whose RP ID is the site or under
    /// it; [`ACCOUNT_PASSWORD`]: passwords in the web persona for an https
    /// endpoint at the site or under it. Names only, never a value; at
    /// most [`ACCOUNTS_MAX`]. Needs the store unlocked.
    pub const W_ACCOUNTS: u16 = 0x73;
    /// `endpoint | str partition | str path` → the `document.cookie`
    /// string (`n=v; n2=v2`, as a `Cookie` header orders them) of the
    /// cookies a request to that host and path in that partition would
    /// carry, leaving out every `HttpOnly` one (arca.md §8.1, decision 4:
    /// script-readable cookies are the page's own by the web's contract).
    /// For a cookied view's converter. Needs the store unlocked.
    pub const W_SCRIPT_COOKIES: u16 = 0x74;
    /// `endpoint | str partition | str path | str line` → empty: keep a
    /// cookie a page's script wrote (`document.cookie = line`, at the
    /// document `path`) in the partition, by the rules a `Set-Cookie` has,
    /// except that it can neither be `HttpOnly` nor replace one. Only the
    /// writes of a transition the reader took come here (the accept or
    /// continue press of a consent wall, legacy-web §5.4.1e), never
    /// exploration's. Needs the store unlocked.
    pub const W_SCRIPT_SET_COOKIE: u16 = 0x75;
    pub const ACCOUNT_PASSKEY: u8 = 1;
    pub const ACCOUNT_PASSWORD: u8 = 2;
    pub const ACCOUNTS_MAX: usize = 16;
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

/// Tessera volume keys (0xB0..), tessera-encryption.md §4.3: only the
/// services in [`crate::volume::SERVICES`]. The key never appears in a reply.
/// `owner` is `str user | str persona`; user [`crate::server::MACHINE_USER`]
/// with persona [`crate::server::MACHINE_PERSONA`] is the machine store.
pub mod volume_op {
    /// `owner | uuid[16] | u64 group | str label` → `key_id[16]`. arcad
    /// makes the key, stores it (class after-first-unlock) and loads it
    /// into the kernel. Needs the owner's store unlocked.
    pub const VK_MINT: u16 = 0xB0;
    /// `owner | key_id[16] | uuid[16] | u64 group` → nothing. Loads the key
    /// with its high-water mark if the owner's store is unlocked and holds
    /// that key for exactly that scope; `DENIED` otherwise, `LOCKED` if
    /// the store is locked. Already loaded: `OK`.
    pub const VK_LOAD: u16 = 0xB1;
    /// `owner | key_id[16]` → `u64 high_water`: the kernel's highest
    /// committed generation under the key, raised into the item. Works
    /// while the screen is locked.
    pub const VK_RECORD: u16 = 0xB2;
    /// `owner | key_id[16]` → nothing: crypto-erase (§9). The kernel drops
    /// the key and the item is deleted. Needs Sigillum's `VERIFY_USER`
    /// (`NEEDS_VERIFICATION` first).
    pub const VK_FORGET: u16 = 0xB3;
    /// `str user | key_id[16] | uuid[16] | u64 group` → the persona's name,
    /// a NUL, and the label the stick was minted with (UTF-8): a
    /// stick was plugged in (tessera-encryption.md §7.2). Looks for the key
    /// in every persona of the user's store and loads it as `VK_LOAD` does;
    /// the reply names the persona it belongs to. `DENIED`: not this
    /// person's (the stick shows Locked), `LOCKED` if the store is.
    pub const VK_FIND: u16 = 0xB4;
    /// `owner | key_id[16] | uuid[16]` → `slot[256] | code`: a recovery
    /// code for the owner's key (tessera-encryption.md §7.4), and the slot
    /// (type 3) that opens it, for the kernel to write at format. The code
    /// is ASCII, `XXXXX-` × 6, shown to the person once; arcad keeps
    /// neither.
    pub const VK_RECOVERY: u16 = 0xB5;
    /// `owner | key_id[16] | uuid[16] | slot[256] | u8 keep | str code
    /// [| handle[16] when code is empty]` → the stick's name from its slot
    /// (empty if it has none): a stick from
    /// elsewhere, opened with its recovery code. With a handle, the code is
    /// what the person typed into Arca's field (`U_HOLD` with
    /// `HOLD_FOR_RECOVERY`, as the same user); `DENIED` if it isn't one.
    /// arcad unwraps the key from the slot; with `keep` it stores it in the
    /// owner's persona (high-water 0) as if minted here. Then it loads it.
    /// `DENIED`: the code isn't this slot's; `MALFORMED`: not a code.
    pub const VK_ADOPT: u16 = 0xB6;
    /// `owner | key_id[16] | uuid[16]` → nothing: "This stick is older
    /// than the last time it was used here. Open read-only?" (§8). Needs
    /// Sigillum's `VERIFY_USER` (`NEEDS_VERIFICATION` first). arcad reloads
    /// the key `TKEY_LOAD_OLDER_RO`: it opens the copy below its high-water
    /// mark, and the kernel refuses every writable mount with it. The
    /// item's mark is unchanged.
    pub const VK_OLDER: u16 = 0xB7;
    /// `owner | key_id[16] | uuid[16]` → `u64` generation: "Use this copy
    /// as current" for an older copy opened read-only (§8), and unmounted
    /// since. Needs Sigillum's `VERIFY_USER`. arcad lowers the item's
    /// high-water mark to the generation the kernel saw mounted (`TKEY_GEN`,
    /// never a number from the caller) and reloads the key as usual, so
    /// the copy mounts writable. `DENIED` if no older copy was opened.
    pub const VK_CURRENT: u16 = 0xB8;
    pub const SLOT_LEN: usize = crate::recovery::SLOT_LEN;
    pub const CODE_MAX: usize = 64;
    /// Longest label.
    pub const LABEL_MAX: usize = 128;
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

    /// A mail courier's login (mail.md §5.2), after TLS is up and only for
    /// a courier whose label is a mail scheme: `u8 source (1 item) | ref[16]`
    /// → `str account | bytes value | u8 kind` ([`LOGIN_PASSWORD`] or
    /// [`LOGIN_OAUTH`], whose value is an OAuth grant record). Password and
    /// OAuth token items only.
    pub const LOGIN: u16 = 0x83;
    pub const LOGIN_PASSWORD: u8 = 1;
    pub const LOGIN_OAUTH: u8 = 2;
    /// A mail courier hands back the grant the provider rotated (mail.md
    /// §5.3), for the item it just logged in with, and only an OAuth one:
    /// `u8 source (1 item) | ref[16] | bytes grant` → nothing.
    pub const REFRESHED: u16 = 0x85;
    /// A courier reports an exact anomaly (arca.md §10) before it ends its
    /// client: `u8 code | str detail` → nothing. Logged, never a value.
    pub const ANOMALY: u16 = 0x84;

    /// [`ANOMALY`] codes.
    pub mod anomaly {
        /// A mail courier's client sent a refused command after login
        /// (`LOGIN`, `AUTHENTICATE`, `STARTTLS`, `AUTH`, ...): the detail is
        /// the command's name.
        pub const REFUSED_COMMAND: u8 = 1;
        /// The client broke the protocol in a way the relay's parser
        /// refuses (a line or literal over its limits).
        pub const MALFORMED: u8 = 2;
    }
    /// Longest [`ANOMALY`] detail.
    pub const ANOMALY_DETAIL_MAX: usize = 32;

    pub const SOURCE_ITEM: u8 = 1;
    pub const SOURCE_HANDLE: u8 = 2;
    /// Most slots one request fills.
    pub const MAX_SLOTS: usize = 8;
}

/// An endpoint on the wire, https or a mail scheme: `u8 scheme | str host |
/// u16 port`. For storing and listing an app's items and for asking for a
/// courier; the web broker's ops take https only ([`read_endpoint`]).
pub fn read_endpoint_any(r: &mut arca_store::codec::Reader<'_>) -> Result<arca_store::Recipient, arca_store::Error> {
    use arca_store::model::caps;
    let scheme = arca_store::Scheme::from_u8(r.u8()?).ok_or(arca_store::Error::BadRecipient)?;
    let host = r.str(caps::HOST)?;
    let port = r.u16()?;
    if scheme != arca_store::Scheme::Https && !scheme.is_mail() {
        return Err(arca_store::Error::BadRecipient);
    }
    arca_store::Recipient::endpoint(scheme, host, port, None)
}

pub fn write_endpoint_scheme(w: &mut arca_store::codec::Writer, scheme: arca_store::Scheme, host: &str, port: u16) {
    w.u8(scheme as u8);
    w.str(host);
    w.u16(port);
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
