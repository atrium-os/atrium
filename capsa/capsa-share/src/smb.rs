//! An SMB 3.1.1 client, the little a file share needs, and the share it
//! serves through FUSE (network-shares.md §5.3, S3).
//!
//! The floor is the spec's: dialect 3.1.1 only, with the SHA-512
//! pre-authentication integrity hash, so nobody in the middle can change
//! the negotiation; AES-GCM encryption of every message after the login
//! (AES-256 preferred), and a server that won't encrypt is refused before
//! any credential is used; NTLMv2 inside SPNEGO, no guest, no anonymous.
//! The login's final reply must be signed with the key both sides derived
//! from the password: a server that didn't know it can't produce that.
//!
//! One TCP connection, one request at a time. Everything the server sends
//! is bounded before use: frame sizes, offsets, listing entries and names.

use std::collections::HashMap;
use std::io::{self, Read, Write};

use aes_gcm::aead::AeadInPlace;
use aes_gcm::{Aes128Gcm, Aes256Gcm, KeyInit, Nonce, Tag};
use capsa_fat::Error as FsError;
use capsa_fuse::{Attr, Backend};
use cmac::Cmac;
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256, Sha512};
use zeroize::Zeroizing;

use crate::ntlm;
use crate::sftp::valid_name;

type FsResult<T> = std::result::Result<T, FsError>;

/// The largest frame the server may send (a read of MAX_IO and headers).
pub const MAX_FRAME: usize = MAX_IO + 64 * 1024;
/// The most one read or write moves.
pub const MAX_IO: usize = 1024 * 1024;
/// The most entries one folder may list.
pub const MAX_DIR: usize = 100_000;
/// Open files kept for reads and writes.
const HANDLES: usize = 8;

// Commands.
const NEGOTIATE: u16 = 0;
const SESSION_SETUP: u16 = 1;
const LOGOFF: u16 = 2;
const TREE_CONNECT: u16 = 3;
const CREATE: u16 = 5;
const CLOSE: u16 = 6;
const READ: u16 = 8;
const WRITE: u16 = 9;
const ECHO: u16 = 13;
const QUERY_DIRECTORY: u16 = 14;
const QUERY_INFO: u16 = 16;
const SET_INFO: u16 = 17;

// Header flags.
const SERVER_TO_REDIR: u32 = 0x1;
const ASYNC: u32 = 0x2;
const SIGNED: u32 = 0x8;

// Status codes.
pub const STATUS_SUCCESS: u32 = 0;
pub const STATUS_PENDING: u32 = 0x0000_0103;
pub const STATUS_NO_MORE_FILES: u32 = 0x8000_0006;
pub const STATUS_MORE_PROCESSING_REQUIRED: u32 = 0xC000_0016;
pub const STATUS_END_OF_FILE: u32 = 0xC000_0011;
pub const STATUS_ACCESS_DENIED: u32 = 0xC000_0022;
pub const STATUS_OBJECT_NAME_NOT_FOUND: u32 = 0xC000_0034;
pub const STATUS_OBJECT_NAME_COLLISION: u32 = 0xC000_0035;
pub const STATUS_OBJECT_PATH_NOT_FOUND: u32 = 0xC000_003A;
pub const STATUS_DELETE_PENDING: u32 = 0xC000_0056;
pub const STATUS_LOGON_FAILURE: u32 = 0xC000_006D;
pub const STATUS_DISK_FULL: u32 = 0xC000_007F;
pub const STATUS_FILE_IS_A_DIRECTORY: u32 = 0xC000_00BA;
pub const STATUS_BAD_NETWORK_NAME: u32 = 0xC000_00CC;
pub const STATUS_DIRECTORY_NOT_EMPTY: u32 = 0xC000_0101;
pub const STATUS_NOT_A_DIRECTORY: u32 = 0xC000_0103;
pub const STATUS_OBJECT_NAME_INVALID: u32 = 0xC000_0033;

// Access, share, disposition, options, attributes.
const FILE_READ_DATA: u32 = 0x1;
const FILE_WRITE_DATA: u32 = 0x2;
const FILE_READ_ATTRIBUTES: u32 = 0x80;
const FILE_WRITE_ATTRIBUTES: u32 = 0x100;
const DELETE: u32 = 0x1_0000;
const SYNCHRONIZE: u32 = 0x10_0000;
const SHARE_ALL: u32 = 0x7;
const FILE_OPEN: u32 = 1;
const FILE_CREATE: u32 = 2;
const DIRECTORY_FILE: u32 = 0x1;
const NON_DIRECTORY_FILE: u32 = 0x40;
const OPEN_REPARSE_POINT: u32 = 0x20_0000;
const ATTR_READONLY: u32 = 0x1;
const ATTR_DIRECTORY: u32 = 0x10;
const ATTR_REPARSE_POINT: u32 = 0x400;

// Ciphers (negotiate context 2).
pub const AES_128_GCM: u16 = 2;
pub const AES_256_GCM: u16 = 4;

/// FILETIME of the Unix epoch.
const EPOCH_FT: i64 = 116_444_736_000_000_000;

pub fn unix_of(ft: u64) -> i64 {
    (ft as i64 - EPOCH_FT).div_euclid(10_000_000)
}

pub fn filetime_of(unix: i64) -> u64 {
    (unix.saturating_mul(10_000_000).saturating_add(EPOCH_FT)).max(0) as u64
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// The server broke the protocol.
    Protocol(&'static str),
    /// The server doesn't meet the floor: why, in words for the user.
    Refused(String),
    /// The user name or password was not accepted.
    Auth,
    /// An NT status for a request.
    Status(u32),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Protocol(w) => write!(f, "protocol error: {w}"),
            Error::Refused(w) => write!(f, "refused: {w}"),
            Error::Auth => write!(f, "the server didn't accept the user name and password"),
            Error::Status(s) => write!(f, "status {s:#010x}"),
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<ntlm::Error> for Error {
    fn from(e: ntlm::Error) -> Self {
        match e {
            ntlm::Error::Malformed(w) => Error::Protocol(w),
            ntlm::Error::Refused(w) => Error::Refused(w.into()),
        }
    }
}

type R<T> = Result<T, Error>;

fn u16_at(b: &[u8], at: usize) -> R<u16> {
    b.get(at..at + 2).map(|s| u16::from_le_bytes([s[0], s[1]])).ok_or(Error::Protocol("short reply"))
}
fn u32_at(b: &[u8], at: usize) -> R<u32> {
    b.get(at..at + 4).map(|s| u32::from_le_bytes(s.try_into().unwrap())).ok_or(Error::Protocol("short reply"))
}
fn u64_at(b: &[u8], at: usize) -> R<u64> {
    b.get(at..at + 8).map(|s| u64::from_le_bytes(s.try_into().unwrap())).ok_or(Error::Protocol("short reply"))
}
/// `len` bytes at `off`, both from the server.
fn span(b: &[u8], off: usize, len: usize) -> R<&[u8]> {
    off.checked_add(len).and_then(|e| b.get(off..e)).ok_or(Error::Protocol("a field points outside the reply"))
}

pub fn utf16(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

fn from_utf16(b: &[u8]) -> Option<String> {
    if b.len() % 2 != 0 {
        return None;
    }
    String::from_utf16(&b.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect::<Vec<_>>()).ok()
}

/// SP800-108 in counter mode with HMAC-SHA256, as SMB 3 derives keys.
pub fn kdf(ki: &[u8], label: &[u8], context: &[u8], bits: u32) -> Zeroizing<Vec<u8>> {
    let mut h = <Hmac<Sha256> as Mac>::new_from_slice(ki).unwrap();
    h.update(&1u32.to_be_bytes());
    h.update(label);
    h.update(&[0]);
    h.update(context);
    h.update(&bits.to_be_bytes());
    let out = h.finalize().into_bytes();
    Zeroizing::new(out[..(bits / 8) as usize].to_vec())
}

enum Cipher {
    G128(Box<Aes128Gcm>),
    G256(Box<Aes256Gcm>),
}

impl Cipher {
    fn new(id: u16, key: &[u8]) -> Cipher {
        if id == AES_256_GCM {
            Cipher::G256(Box::new(Aes256Gcm::new_from_slice(key).unwrap()))
        } else {
            Cipher::G128(Box::new(Aes128Gcm::new_from_slice(key).unwrap()))
        }
    }
    fn seal(&self, nonce: &[u8; 12], aad: &[u8], buf: &mut [u8]) -> [u8; 16] {
        let n = &Nonce::from(*nonce);
        let t = match self {
            Cipher::G128(c) => c.encrypt_in_place_detached(n, aad, buf),
            Cipher::G256(c) => c.encrypt_in_place_detached(n, aad, buf),
        };
        t.expect("GCM seals any length we send").into()
    }
    fn open(&self, nonce: &[u8; 12], aad: &[u8], buf: &mut [u8], tag: &[u8; 16]) -> bool {
        let n = &Nonce::from(*nonce);
        let t = &Tag::from(*tag);
        match self {
            Cipher::G128(c) => c.decrypt_in_place_detached(n, aad, buf, t).is_ok(),
            Cipher::G256(c) => c.decrypt_in_place_detached(n, aad, buf, t).is_ok(),
        }
    }
}

struct Keys {
    enc: Cipher,
    dec: Cipher,
}

/// What the server said in the negotiation: the identity the share is
/// pinned to (a hint, §5.3) and the cipher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Negotiated {
    pub guid: [u8; 16],
    pub cipher: u16,
}

/// The server's GUID as text, as Windows writes GUIDs.
pub fn guid_text(g: &[u8; 16]) -> String {
    format!(
        "{:08x}-{:04x}-{:04x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        u32::from_le_bytes(g[0..4].try_into().unwrap()),
        u16::from_le_bytes([g[4], g[5]]),
        u16::from_le_bytes([g[6], g[7]]),
        g[8], g[9], g[10], g[11], g[12], g[13], g[14], g[15]
    )
}

/// What a share item is pinned to: the digest of the server's GUID.
pub fn guid_key(g: &[u8; 16]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"atrium smb server guid\0");
    h.update(g);
    h.finalize().into()
}

pub fn cipher_name(c: u16) -> &'static str {
    match c {
        AES_256_GCM => "AES-256-GCM",
        AES_128_GCM => "AES-128-GCM",
        _ => "none",
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FileId([u8; 16]);

/// What CREATE and directory listings say of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Info {
    pub dir: bool,
    pub size: u64,
    pub mtime: i64,
    pub ctime: i64,
    pub atime: i64,
    pub read_only: bool,
    pub reparse: bool,
}

fn info_of(attrs: u32, size: u64, atime: u64, mtime: u64, change: u64) -> Info {
    Info {
        dir: attrs & ATTR_DIRECTORY != 0,
        size: if attrs & ATTR_DIRECTORY != 0 { 0 } else { size },
        mtime: unix_of(mtime),
        ctime: unix_of(change),
        atime: unix_of(atime),
        read_only: attrs & ATTR_READONLY != 0,
        reparse: attrs & ATTR_REPARSE_POINT != 0,
    }
}

/// One SMB 3.1.1 connection, logged in to one share.
pub struct Conn<S> {
    s: S,
    msg_id: u64,
    credits: u32,
    session_id: u64,
    tree_id: u32,
    keys: Option<Keys>,
    preauth: [u8; 64],
    pub negotiated: Negotiated,
    max_read: usize,
    max_write: usize,
    max_trans: usize,
    nonce_ctr: u64,
    nonce_salt: [u8; 4],
}

/// A reply: its header fields and the whole message (header included,
/// since offsets count from it).
struct Reply {
    status: u32,
    flags: u32,
    session_id: u64,
    tree_id: u32,
    msg: Vec<u8>,
}

impl Reply {
    fn body(&self) -> &[u8] {
        &self.msg[64..]
    }
}

fn header(cmd: u16, charge: u16, credits: u16, msg_id: u64, tree_id: u32, session_id: u64) -> [u8; 64] {
    let mut h = [0u8; 64];
    h[0..4].copy_from_slice(b"\xfeSMB");
    h[4..6].copy_from_slice(&64u16.to_le_bytes());
    h[6..8].copy_from_slice(&charge.to_le_bytes());
    h[12..14].copy_from_slice(&cmd.to_le_bytes());
    h[14..16].copy_from_slice(&credits.to_le_bytes());
    h[24..32].copy_from_slice(&msg_id.to_le_bytes());
    h[36..40].copy_from_slice(&tree_id.to_le_bytes());
    h[40..48].copy_from_slice(&session_id.to_le_bytes());
    h
}

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::getrandom(&mut b).expect("the system's random source");
    b
}

/// Parse a plain SMB2 reply's header and check it answers `cmd`/`msg_id`.
fn parse_reply(msg: Vec<u8>, cmd: u16, msg_id: u64) -> R<(Reply, u16)> {
    if msg.len() < 64 || &msg[0..4] != b"\xfeSMB" || u16_at(&msg, 4)? != 64 {
        return Err(Error::Protocol("not an SMB2 reply"));
    }
    let flags = u32_at(&msg, 16)?;
    if flags & SERVER_TO_REDIR == 0 {
        return Err(Error::Protocol("a request where a reply belongs"));
    }
    if u16_at(&msg, 12)? != cmd || u64_at(&msg, 24)? != msg_id {
        return Err(Error::Protocol("a reply to another request"));
    }
    if u32_at(&msg, 20)? != 0 {
        return Err(Error::Protocol("compounded reply"));
    }
    let granted = u16_at(&msg, 14)?;
    let tree_id = if flags & ASYNC != 0 { 0 } else { u32_at(&msg, 36)? };
    Ok((Reply { status: u32_at(&msg, 8)?, flags, session_id: u64_at(&msg, 40)?, tree_id, msg }, granted))
}

fn signature(key: &[u8], msg: &[u8]) -> [u8; 16] {
    let mut m = <Cmac<aes::Aes128> as Mac>::new_from_slice(key).unwrap();
    m.update(&msg[..48]);
    m.update(&[0u8; 16]);
    m.update(&msg[64..]);
    m.finalize().into_bytes().into()
}

impl<S: Read + Write> Conn<S> {
    fn send_frame(&mut self, msg: &[u8]) -> R<()> {
        let mut f = Vec::with_capacity(4 + msg.len());
        f.extend_from_slice(&(msg.len() as u32).to_be_bytes());
        f.extend_from_slice(msg);
        self.s.write_all(&f)?;
        Ok(())
    }

    fn recv_frame(&mut self) -> R<Vec<u8>> {
        let mut l = [0u8; 4];
        self.s.read_exact(&mut l)?;
        let n = u32::from_be_bytes(l) as usize;
        if l[0] != 0 || n < 64 || n > MAX_FRAME {
            return Err(Error::Protocol("frame length out of range"));
        }
        let mut b = vec![0u8; n];
        self.s.read_exact(&mut b)?;
        Ok(b)
    }

    /// Wrap `msg` in a transform header, sealed with the client's key.
    fn seal(&mut self, msg: &[u8]) -> Vec<u8> {
        let keys = self.keys.as_ref().unwrap();
        self.nonce_ctr += 1;
        let mut nonce = [0u8; 12];
        nonce[..8].copy_from_slice(&self.nonce_ctr.to_le_bytes());
        nonce[8..].copy_from_slice(&self.nonce_salt);
        let mut t = vec![0u8; 52 + msg.len()];
        t[0..4].copy_from_slice(b"\xfdSMB");
        t[20..32].copy_from_slice(&nonce);
        t[36..40].copy_from_slice(&(msg.len() as u32).to_le_bytes());
        t[42..44].copy_from_slice(&1u16.to_le_bytes());
        t[44..52].copy_from_slice(&self.session_id.to_le_bytes());
        let (head, body) = t.split_at_mut(52);
        body.copy_from_slice(msg);
        let tag = keys.enc.seal(&nonce, &head[20..52], body);
        head[4..20].copy_from_slice(&tag);
        t
    }

    /// Open a sealed frame; anything else after the login is refused.
    fn open_sealed(&self, mut f: Vec<u8>) -> R<Vec<u8>> {
        let keys = self.keys.as_ref().unwrap();
        if f.len() < 52 + 64 || &f[0..4] != b"\xfdSMB" {
            return Err(Error::Protocol("an unencrypted reply after the login"));
        }
        if u16_at(&f, 42)? != 1 || u64_at(&f, 44)? != self.session_id || u32_at(&f, 36)? as usize != f.len() - 52 {
            return Err(Error::Protocol("a sealed reply for another session"));
        }
        let nonce: [u8; 12] = f[20..32].try_into().unwrap();
        let tag: [u8; 16] = f[4..20].try_into().unwrap();
        let aad = f[20..52].to_vec();
        if !keys.dec.open(&nonce, &aad, &mut f[52..], &tag) {
            return Err(Error::Protocol("a reply that doesn't decrypt"));
        }
        f.drain(..52);
        Ok(f)
    }

    /// One request and its final reply.
    fn call(&mut self, cmd: u16, body: &[u8], payload: usize) -> R<Reply> {
        let charge = payload.saturating_sub(1) / 65536 + 1;
        let charge = charge as u32;
        if cmd != NEGOTIATE && self.credits < charge {
            return Err(Error::Protocol("the server granted no credits"));
        }
        let want = if self.credits < 64 { 128 } else { 1 };
        let id = self.msg_id;
        let mut msg = Vec::with_capacity(64 + body.len());
        msg.extend_from_slice(&header(cmd, if cmd == NEGOTIATE { 0 } else { charge as u16 }, want, id, self.tree_id, self.session_id));
        msg.extend_from_slice(body);
        self.msg_id += charge as u64;
        self.credits = self.credits.saturating_sub(charge);
        if cmd == NEGOTIATE || cmd == SESSION_SETUP {
            self.hash(&msg);
        }
        let out = if self.keys.is_some() { self.seal(&msg) } else { msg };
        self.send_frame(&out)?;
        loop {
            let f = self.recv_frame()?;
            let m = if self.keys.is_some() { self.open_sealed(f)? } else { f };
            let (r, granted) = parse_reply(m, cmd, id)?;
            self.credits = self.credits.saturating_add(granted as u32).min(8192);
            if r.status == STATUS_PENDING && r.flags & ASYNC != 0 {
                continue;
            }
            return Ok(r);
        }
    }

    fn hash(&mut self, msg: &[u8]) {
        let mut h = Sha512::new();
        h.update(self.preauth);
        h.update(msg);
        self.preauth = h.finalize().into();
    }

    /// Connect: negotiate 3.1.1 with encryption, or refuse.
    pub fn negotiate(s: S) -> R<Conn<S>> {
        let mut c = Conn {
            s,
            msg_id: 0,
            credits: 1,
            session_id: 0,
            tree_id: 0,
            keys: None,
            preauth: [0; 64],
            negotiated: Negotiated { guid: [0; 16], cipher: 0 },
            max_read: 65536,
            max_write: 65536,
            max_trans: 65536,
            nonce_ctr: 0,
            nonce_salt: random(),
        };
        let mut b = Vec::with_capacity(128);
        b.extend_from_slice(&36u16.to_le_bytes());
        b.extend_from_slice(&1u16.to_le_bytes()); // one dialect
        b.extend_from_slice(&1u16.to_le_bytes()); // signing enabled
        b.extend_from_slice(&[0, 0]);
        b.extend_from_slice(&0x44u32.to_le_bytes()); // large MTU, encryption
        b.extend_from_slice(&random::<16>());
        let ctx_off_at = b.len();
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&[0, 0]);
        b.extend_from_slice(&0x0311u16.to_le_bytes());
        while (64 + b.len()) % 8 != 0 {
            b.push(0);
        }
        let off = (64 + b.len()) as u32;
        b[ctx_off_at..ctx_off_at + 4].copy_from_slice(&off.to_le_bytes());
        // Pre-authentication integrity: SHA-512 with a salt.
        let mut pre = Vec::new();
        pre.extend_from_slice(&1u16.to_le_bytes());
        pre.extend_from_slice(&32u16.to_le_bytes());
        pre.extend_from_slice(&1u16.to_le_bytes());
        pre.extend_from_slice(&random::<32>());
        // Encryption: AES-256-GCM, then AES-128-GCM.
        let mut enc = Vec::new();
        enc.extend_from_slice(&2u16.to_le_bytes());
        enc.extend_from_slice(&AES_256_GCM.to_le_bytes());
        enc.extend_from_slice(&AES_128_GCM.to_le_bytes());
        for (i, (t, d)) in [(1u16, pre), (2u16, enc)].into_iter().enumerate() {
            if i > 0 {
                while (64 + b.len()) % 8 != 0 {
                    b.push(0);
                }
            }
            b.extend_from_slice(&t.to_le_bytes());
            b.extend_from_slice(&(d.len() as u16).to_le_bytes());
            b.extend_from_slice(&[0; 4]);
            b.extend_from_slice(&d);
        }
        let r = c.call(NEGOTIATE, &b, 0)?;
        if r.status != STATUS_SUCCESS {
            return Err(Error::Refused(format!("the server doesn't speak SMB 3.1.1 (status {:#010x})", r.status)));
        }
        let m = &r.msg;
        let body = r.body();
        if u16_at(body, 0)? != 65 {
            return Err(Error::Protocol("negotiate reply"));
        }
        let dialect = u16_at(body, 4)?;
        if dialect != 0x0311 {
            return Err(Error::Refused(format!("the server offers only SMB {}.{}", dialect >> 8, (dialect >> 4) & 0xf)));
        }
        let guid: [u8; 16] = span(body, 8, 16)?.try_into().unwrap();
        c.max_trans = (u32_at(body, 28)? as usize).clamp(65536, MAX_IO);
        c.max_read = (u32_at(body, 32)? as usize).clamp(65536, MAX_IO);
        c.max_write = (u32_at(body, 36)? as usize).clamp(65536, MAX_IO);
        let n_ctx = u16_at(body, 6)? as usize;
        let mut at = u32_at(body, 60)? as usize;
        let (mut sha512, mut cipher) = (false, 0u16);
        for i in 0..n_ctx.min(16) {
            if i > 0 {
                at = at.next_multiple_of(8);
            }
            let t = u16_at(m, at)?;
            let len = u16_at(m, at + 2)? as usize;
            let d = span(m, at + 8, len)?;
            match t {
                1 => sha512 = u16_at(d, 0)? == 1 && u16_at(d, 4)? == 1,
                2 => {
                    if u16_at(d, 0)? == 1 {
                        cipher = u16_at(d, 2)?;
                    }
                }
                _ => {}
            }
            at += 8 + len;
        }
        if !sha512 {
            return Err(Error::Refused("no SHA-512 pre-authentication integrity".into()));
        }
        if cipher != AES_128_GCM && cipher != AES_256_GCM {
            return Err(Error::Refused("the server won't encrypt (no AES-GCM)".into()));
        }
        c.hash(&r.msg);
        c.negotiated = Negotiated { guid, cipher };
        Ok(c)
    }

    fn session_setup(&mut self, token: &[u8]) -> R<Reply> {
        let mut b = Vec::with_capacity(24 + token.len());
        b.extend_from_slice(&25u16.to_le_bytes());
        b.push(0);
        b.push(1); // signing enabled
        b.extend_from_slice(&[0; 8]); // capabilities, channel
        b.extend_from_slice(&88u16.to_le_bytes());
        b.extend_from_slice(&(token.len() as u16).to_le_bytes());
        b.extend_from_slice(&[0; 8]); // previous session
        b.extend_from_slice(token);
        self.call(SESSION_SETUP, &b, 0)
    }

    fn security_buffer(r: &Reply) -> R<Vec<u8>> {
        let body = r.body();
        let off = u16_at(body, 4)? as usize;
        let len = u16_at(body, 6)? as usize;
        Ok(span(&r.msg, off, len)?.to_vec())
    }

    /// Log in with NTLMv2. `user` may be `DOMAIN\user`. The password is
    /// used for this one exchange and not kept.
    pub fn login(&mut self, user: &str, password: &[u8]) -> R<()> {
        let (domain, user) = match user.split_once('\\') {
            Some((d, u)) => (d, u),
            None => ("", user),
        };
        let r = self.session_setup(&ntlm::spnego_init(&ntlm::negotiate()))?;
        if r.status != STATUS_MORE_PROCESSING_REQUIRED {
            return Err(Error::Protocol("no NTLM challenge"));
        }
        self.session_id = r.session_id;
        let token = Self::security_buffer(&r)?;
        self.hash(&r.msg);
        let ch = ntlm::parse_challenge(&ntlm::spnego_token(&token)?)?;
        let now = filetime_of(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64));
        let a = ntlm::authenticate(&ch, user, domain, password, random(), now);
        let r = self.session_setup(&ntlm::spnego_resp(&a.message))?;
        match r.status {
            STATUS_SUCCESS => {}
            STATUS_LOGON_FAILURE | STATUS_ACCESS_DENIED => return Err(Error::Auth),
            s => return Err(Error::Status(s)),
        }
        if r.session_id != self.session_id {
            return Err(Error::Protocol("the login answered for another session"));
        }
        let flags = u16_at(r.body(), 2)?;
        if flags & 0x3 != 0 {
            return Err(Error::Refused("the server logged us in as a guest".into()));
        }
        // Keys from the password-derived key and everything negotiated.
        let ki = &a.session_key[..];
        let ctx = self.preauth;
        let bits = if self.negotiated.cipher == AES_256_GCM { 256 } else { 128 };
        let signing = kdf(ki, b"SMBSigningKey\0", &ctx, 128);
        // The final reply is signed: the server knew the key.
        if r.flags & SIGNED == 0 || r.msg[48..64] != signature(&signing, &r.msg) {
            return Err(Error::Refused("the server's login reply isn't signed with the session key".into()));
        }
        let enc = kdf(ki, b"SMBC2SCipherKey\0", &ctx, bits);
        let dec = kdf(ki, b"SMBS2CCipherKey\0", &ctx, bits);
        self.keys = Some(Keys { enc: Cipher::new(self.negotiated.cipher, &enc), dec: Cipher::new(self.negotiated.cipher, &dec) });
        Ok(())
    }

    /// Connect to `\\host\share`.
    pub fn tree_connect(&mut self, unc: &str) -> R<()> {
        let p = utf16(unc);
        let mut b = Vec::with_capacity(8 + p.len());
        b.extend_from_slice(&9u16.to_le_bytes());
        b.extend_from_slice(&[0, 0]);
        b.extend_from_slice(&72u16.to_le_bytes());
        b.extend_from_slice(&(p.len() as u16).to_le_bytes());
        b.extend_from_slice(&p);
        let r = self.call(TREE_CONNECT, &b, 0)?;
        if r.status != STATUS_SUCCESS {
            return Err(Error::Status(r.status));
        }
        if *r.body().get(2).ok_or(Error::Protocol("tree reply"))? != 1 {
            return Err(Error::Refused("that share is not a disk".into()));
        }
        self.tree_id = r.tree_id;
        Ok(())
    }

    pub fn create(&mut self, path: &str, access: u32, disposition: u32, options: u32) -> R<(FileId, Info)> {
        let name = utf16(path);
        let mut b = Vec::with_capacity(56 + name.len().max(1));
        b.extend_from_slice(&57u16.to_le_bytes());
        b.push(0);
        b.push(0); // no oplock
        b.extend_from_slice(&2u32.to_le_bytes()); // impersonation
        b.extend_from_slice(&[0; 16]);
        b.extend_from_slice(&access.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes()); // attributes
        b.extend_from_slice(&SHARE_ALL.to_le_bytes());
        b.extend_from_slice(&disposition.to_le_bytes());
        b.extend_from_slice(&options.to_le_bytes());
        b.extend_from_slice(&120u16.to_le_bytes());
        b.extend_from_slice(&(name.len() as u16).to_le_bytes());
        b.extend_from_slice(&[0; 8]);
        b.extend_from_slice(&name);
        if name.is_empty() {
            b.push(0);
        }
        let r = self.call(CREATE, &b, 0)?;
        if r.status != STATUS_SUCCESS {
            return Err(Error::Status(r.status));
        }
        let d = r.body();
        if u16_at(d, 0)? != 89 {
            return Err(Error::Protocol("create reply"));
        }
        let info = info_of(u32_at(d, 56)?, u64_at(d, 48)?, u64_at(d, 16)?, u64_at(d, 24)?, u64_at(d, 32)?);
        Ok((FileId(span(d, 64, 16)?.try_into().unwrap()), info))
    }

    pub fn close(&mut self, f: FileId) -> R<()> {
        let mut b = Vec::with_capacity(24);
        b.extend_from_slice(&24u16.to_le_bytes());
        b.extend_from_slice(&[0; 6]);
        b.extend_from_slice(&f.0);
        let r = self.call(CLOSE, &b, 0)?;
        match r.status {
            STATUS_SUCCESS => Ok(()),
            s => Err(Error::Status(s)),
        }
    }

    /// Up to `len` bytes at `off`; empty at the end.
    pub fn read(&mut self, f: FileId, off: u64, len: usize) -> R<Vec<u8>> {
        let len = len.min(self.max_read);
        let mut b = Vec::with_capacity(49);
        b.extend_from_slice(&49u16.to_le_bytes());
        b.push(0x50);
        b.push(0);
        b.extend_from_slice(&(len as u32).to_le_bytes());
        b.extend_from_slice(&off.to_le_bytes());
        b.extend_from_slice(&f.0);
        b.extend_from_slice(&[0; 16]); // minimum, channel, remaining, channel info
        b.push(0);
        let r = self.call(READ, &b, len)?;
        match r.status {
            STATUS_SUCCESS => {}
            STATUS_END_OF_FILE => return Ok(Vec::new()),
            s => return Err(Error::Status(s)),
        }
        let d = r.body();
        let off = *d.get(2).ok_or(Error::Protocol("read reply"))? as usize;
        let n = u32_at(d, 4)? as usize;
        if n > len {
            return Err(Error::Protocol("a read returned more than asked"));
        }
        Ok(span(&r.msg, off, n)?.to_vec())
    }

    pub fn write(&mut self, f: FileId, off: u64, data: &[u8]) -> R<usize> {
        let data = &data[..data.len().min(self.max_write)];
        let mut b = Vec::with_capacity(48 + data.len());
        b.extend_from_slice(&49u16.to_le_bytes());
        b.extend_from_slice(&112u16.to_le_bytes());
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        b.extend_from_slice(&off.to_le_bytes());
        b.extend_from_slice(&f.0);
        b.extend_from_slice(&[0; 16]); // channel, remaining, channel info, flags
        b.extend_from_slice(data);
        let r = self.call(WRITE, &b, data.len())?;
        match r.status {
            STATUS_SUCCESS => {}
            s => return Err(Error::Status(s)),
        }
        let n = u32_at(r.body(), 4)? as usize;
        if n > data.len() {
            return Err(Error::Protocol("a write took more than sent"));
        }
        Ok(n)
    }

    /// Every entry of an open folder (FileDirectoryInformation).
    pub fn list(&mut self, f: FileId) -> R<Vec<(String, Info)>> {
        let star = utf16("*");
        let mut out = Vec::new();
        let mut first = true;
        loop {
            let mut b = Vec::with_capacity(34);
            b.extend_from_slice(&33u16.to_le_bytes());
            b.push(1); // FileDirectoryInformation
            b.push(if first { 1 } else { 0 }); // restart
            b.extend_from_slice(&0u32.to_le_bytes());
            b.extend_from_slice(&f.0);
            b.extend_from_slice(&96u16.to_le_bytes());
            b.extend_from_slice(&(star.len() as u16).to_le_bytes());
            b.extend_from_slice(&(self.max_trans as u32).to_le_bytes());
            b.extend_from_slice(&star);
            first = false;
            let r = self.call(QUERY_DIRECTORY, &b, self.max_trans)?;
            match r.status {
                STATUS_SUCCESS => {}
                STATUS_NO_MORE_FILES => return Ok(out),
                s => return Err(Error::Status(s)),
            }
            let d = r.body();
            let buf = span(&r.msg, u16_at(d, 2)? as usize, u32_at(d, 4)? as usize)?;
            parse_listing(buf, &mut out)?;
            if out.len() > MAX_DIR {
                return Err(Error::Protocol("a folder with too many entries"));
            }
        }
    }

    fn set_info(&mut self, f: FileId, class: u8, data: &[u8]) -> R<()> {
        let mut b = Vec::with_capacity(32 + data.len());
        b.extend_from_slice(&33u16.to_le_bytes());
        b.push(1); // file
        b.push(class);
        b.extend_from_slice(&(data.len() as u32).to_le_bytes());
        b.extend_from_slice(&96u16.to_le_bytes());
        b.extend_from_slice(&[0; 6]);
        b.extend_from_slice(&f.0);
        b.extend_from_slice(data);
        let r = self.call(SET_INFO, &b, 0)?;
        match r.status {
            STATUS_SUCCESS => Ok(()),
            s => Err(Error::Status(s)),
        }
    }

    pub fn set_size(&mut self, f: FileId, len: u64) -> R<()> {
        self.set_info(f, 20, &len.to_le_bytes())
    }

    pub fn set_mtime(&mut self, f: FileId, unix: i64) -> R<()> {
        let mut d = [0u8; 40];
        d[16..24].copy_from_slice(&filetime_of(unix).to_le_bytes());
        self.set_info(f, 4, &d)
    }

    pub fn delete(&mut self, f: FileId) -> R<()> {
        self.set_info(f, 13, &[1])
    }

    /// Rename to `path` (from the share's root), replacing a file there.
    pub fn rename(&mut self, f: FileId, path: &str) -> R<()> {
        let n = utf16(path);
        let mut d = Vec::with_capacity(20 + n.len());
        d.push(1);
        d.extend_from_slice(&[0; 15]);
        d.extend_from_slice(&(n.len() as u32).to_le_bytes());
        d.extend_from_slice(&n);
        self.set_info(f, 10, &d)
    }

    /// (bytes per unit, total units, units free to the caller).
    pub fn fs_size(&mut self, f: FileId) -> R<(u64, u64, u64)> {
        let mut b = Vec::with_capacity(41);
        b.extend_from_slice(&41u16.to_le_bytes());
        b.push(2); // file system
        b.push(7); // FileFsFullSizeInformation
        b.extend_from_slice(&64u32.to_le_bytes());
        b.extend_from_slice(&[0; 12]);
        b.extend_from_slice(&[0; 4]);
        b.extend_from_slice(&f.0);
        b.push(0);
        let r = self.call(QUERY_INFO, &b, 0)?;
        if r.status != STATUS_SUCCESS {
            return Err(Error::Status(r.status));
        }
        let d = r.body();
        let buf = span(&r.msg, u16_at(d, 2)? as usize, u32_at(d, 4)? as usize)?;
        let unit = (u32_at(buf, 24)? as u64).saturating_mul(u32_at(buf, 28)? as u64).max(1);
        Ok((unit, u64_at(buf, 0)?, u64_at(buf, 8)?))
    }

    pub fn echo(&mut self) -> R<()> {
        let r = self.call(ECHO, &[4, 0, 0, 0], 0)?;
        match r.status {
            STATUS_SUCCESS => Ok(()),
            s => Err(Error::Status(s)),
        }
    }

    pub fn logoff(&mut self) {
        let _ = self.call(LOGOFF, &[4, 0, 0, 0], 0);
    }
}

/// FileDirectoryInformation entries into `out`; names that aren't one
/// valid component are skipped.
fn parse_listing(buf: &[u8], out: &mut Vec<(String, Info)>) -> R<()> {
    let mut at = 0usize;
    loop {
        let e = buf.get(at..).ok_or(Error::Protocol("listing entry"))?;
        let next = u32_at(e, 0)? as usize;
        let nlen = u32_at(e, 60)? as usize;
        let name = span(e, 64, nlen)?;
        if let Some(n) = from_utf16(name) {
            let info = info_of(u32_at(e, 56)?, u64_at(e, 40)?, u64_at(e, 16)?, u64_at(e, 24)?, u64_at(e, 32)?);
            out.push((n, info));
        }
        if next == 0 {
            return Ok(());
        }
        if next < 64 + nlen || out.len() > MAX_DIR {
            return Err(Error::Protocol("listing entries overlap"));
        }
        at += next;
    }
}

/// A name this share shows: one component, nothing Windows can't name.
pub fn smb_name(n: &str) -> bool {
    valid_name(n) && !n.contains(['\\', ':', '*', '?', '"', '<', '>', '|']) && !n.chars().any(|c| c.is_control())
}

// ---- the share, through FUSE ------------------------------------------------

#[derive(Debug, Clone)]
pub struct Node {
    id: u64,
    name: String,
    info: Info,
}

pub struct SmbFs<S: Read + Write> {
    c: Conn<S>,
    /// id -> path from the share's root, '\'-separated; None once removed.
    /// 0 unused, 1 the root.
    paths: Vec<Option<String>>,
    ids: HashMap<String, u64>,
    writable: bool,
    /// Open files: (id, handle, opened for writing), oldest first.
    open: Vec<(u64, FileId, bool)>,
    /// The root folder, held open (statfs, and so the server keeps the
    /// session).
    root: FileId,
    /// The connection failed: requests fail at once until `backoff` says
    /// to try `reconnect` (S4).
    dead: bool,
    reconnect: Option<crate::Reconnect<Conn<S>>>,
    backoff: crate::dialer::Backoff,
    exit_on_loss: bool,
}

fn map(e: &Error) -> FsError {
    match e {
        Error::Status(STATUS_OBJECT_NAME_NOT_FOUND | STATUS_OBJECT_PATH_NOT_FOUND | STATUS_DELETE_PENDING) => FsError::NotFound,
        Error::Status(STATUS_OBJECT_NAME_COLLISION) => FsError::Exists,
        Error::Status(STATUS_DIRECTORY_NOT_EMPTY) => FsError::NotEmpty,
        Error::Status(STATUS_FILE_IS_A_DIRECTORY) => FsError::IsDir,
        Error::Status(STATUS_NOT_A_DIRECTORY) => FsError::NotDir,
        Error::Status(STATUS_DISK_FULL) => FsError::NoSpace,
        Error::Status(STATUS_OBJECT_NAME_INVALID) => FsError::InvalidName,
        Error::Status(STATUS_ACCESS_DENIED) => FsError::Io("permission denied".into()),
        other => FsError::Io(other.to_string()),
    }
}

impl<S: Read + Write> SmbFs<S> {
    /// Serve `root` (a folder in the share, '/'-separated) of a connection
    /// that is logged in and connected to its share.
    pub fn new(mut c: Conn<S>, root: &str, writable: bool) -> R<Self> {
        let root: Vec<&str> = root.split('/').filter(|s| !s.is_empty()).collect();
        if root.iter().any(|s| !smb_name(s)) {
            return Err(Error::Refused("the share's folder has a name SMB can't use".into()));
        }
        let root = root.join("\\");
        let (fid, info) = c.create(&root, FILE_READ_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE, FILE_OPEN, DIRECTORY_FILE)?;
        if !info.dir {
            return Err(Error::Refused("the share's root is not a folder".into()));
        }
        let mut ids = HashMap::new();
        ids.insert(root.clone(), 1);
        Ok(SmbFs { c, paths: vec![None, Some(root)], ids, writable, open: Vec::new(), root: fid, dead: false, reconnect: None, backoff: Default::default(), exit_on_loss: false })
    }

    /// A lost connection ends the process (status 4) rather than leave a
    /// share that fails every request.
    pub fn exit_on_loss(mut self) -> Self {
        self.exit_on_loss = true;
        self
    }

    fn path(&self, id: u64) -> FsResult<&str> {
        self.paths.get(id as usize).and_then(|p| p.as_deref()).ok_or(FsError::NotFound)
    }

    fn child(&self, d: u64, name: &str) -> FsResult<String> {
        if !smb_name(name) {
            return Err(FsError::InvalidName);
        }
        let p = self.path(d)?;
        Ok(if p.is_empty() { name.to_string() } else { format!("{p}\\{name}") })
    }

    fn intern(&mut self, p: String) -> u64 {
        if let Some(&id) = self.ids.get(&p) {
            return id;
        }
        let id = self.paths.len() as u64;
        self.paths.push(Some(p.clone()));
        self.ids.insert(p, id);
        id
    }

    /// Connect again after a loss, through `f` (a session logged in and
    /// connected to the share), instead of ending.
    pub fn reconnect_with(mut self, f: crate::Reconnect<Conn<S>>) -> Self {
        self.reconnect = Some(f);
        self
    }

    fn live(&mut self) -> FsResult<()> {
        if !self.dead {
            return Ok(());
        }
        if let Some(f) = self.reconnect.as_mut() {
            if self.backoff.ready() {
                let root = self.paths[1].clone().unwrap_or_default();
                let r = f().and_then(|mut c| {
                    let (fid, _) = c.create(&root, FILE_READ_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE, FILE_OPEN, DIRECTORY_FILE).map_err(|e| e.to_string())?;
                    Ok((c, fid))
                });
                match r {
                    Ok((c, fid)) => {
                        eprintln!("atrium-share-smb: reconnected");
                        self.c = c;
                        self.root = fid;
                        // Handles were the old session's.
                        self.open.clear();
                        self.dead = false;
                        self.backoff.ok();
                        return Ok(());
                    }
                    Err(e) => {
                        eprintln!("atrium-share-smb: reconnect: {e}");
                        self.backoff.failed();
                    }
                }
            }
            return Err(FsError::Io("the server can't be reached: trying again soon".into()));
        }
        Err(FsError::Io("the connection to the server is gone".into()))
    }

    fn chk<T>(&mut self, r: R<T>) -> FsResult<T> {
        if let Err(e @ (Error::Io(_) | Error::Protocol(_) | Error::Status(0xC000_035C | 0xC000_0203))) = &r {
            if !self.dead && self.reconnect.is_some() {
                eprintln!("atrium-share-smb: {e}: reconnecting on the next request");
            }
            self.dead = true;
            if self.exit_on_loss && self.reconnect.is_none() {
                eprintln!("atrium-share-smb: {e}: ending the share");
                std::process::exit(4);
            }
        }
        r.map_err(|e| map(&e))
    }

    /// An open handle for `id`, for writing if `w`.
    fn handle(&mut self, id: u64, w: bool) -> FsResult<FileId> {
        if let Some(i) = self.open.iter().position(|h| h.0 == id && (h.2 || !w)) {
            let h = self.open.remove(i);
            self.open.push(h);
            return Ok(h.1);
        }
        let p = self.path(id)?.to_string();
        let access = FILE_READ_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE | if w { FILE_WRITE_DATA | FILE_WRITE_ATTRIBUTES } else { 0 };
        let r = self.c.create(&p, access, FILE_OPEN, NON_DIRECTORY_FILE | OPEN_REPARSE_POINT);
        let (f, _) = self.chk(r)?;
        self.keep(id, f, w)?;
        Ok(f)
    }

    fn keep(&mut self, id: u64, f: FileId, w: bool) -> FsResult<()> {
        if self.open.len() >= HANDLES {
            let (_, old, _) = self.open.remove(0);
            let r = self.c.close(old);
            self.chk(r)?;
        }
        self.open.push((id, f, w));
        Ok(())
    }

    /// Close what is open of `id` (all, if None).
    fn close_open(&mut self, id: Option<u64>) -> FsResult<()> {
        let (gone, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.open).into_iter().partition(|h| id.is_none_or(|i| h.0 == i));
        self.open = kept;
        for (_, f, _) in gone {
            let r = self.c.close(f);
            self.chk(r)?;
        }
        Ok(())
    }

    fn stat(&mut self, p: &str) -> FsResult<Info> {
        let r = self.c.create(p, FILE_READ_ATTRIBUTES | SYNCHRONIZE, FILE_OPEN, OPEN_REPARSE_POINT);
        let (f, info) = self.chk(r)?;
        let r = self.c.close(f);
        self.chk(r)?;
        // A link on the server is not followed, and not shown (§4.2).
        if info.reparse {
            return Err(FsError::NotFound);
        }
        Ok(info)
    }

    fn forget(&mut self, id: u64) {
        self.open.retain(|h| h.0 != id);
        if let Some(Some(p)) = self.paths.get(id as usize).cloned() {
            self.ids.remove(&p);
            let prefix = format!("{p}\\");
            let under: Vec<(String, u64)> = self.ids.iter().filter(|(k, _)| k.starts_with(&prefix)).map(|(k, v)| (k.clone(), *v)).collect();
            for (k, v) in under {
                self.ids.remove(&k);
                self.paths[v as usize] = None;
            }
            self.paths[id as usize] = None;
        }
    }

    /// Delete `id` (a folder if `dir`).
    fn remove(&mut self, id: u64, dir: bool) -> FsResult<()> {
        self.live()?;
        self.close_open(Some(id))?;
        let p = self.path(id)?.to_string();
        let r = self.c.create(&p, DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE, FILE_OPEN, OPEN_REPARSE_POINT | if dir { DIRECTORY_FILE } else { NON_DIRECTORY_FILE });
        let (f, _) = self.chk(r)?;
        let r = self.c.delete(f);
        let c = self.c.close(f);
        self.chk(r)?;
        self.chk(c)?;
        self.forget(id);
        Ok(())
    }
}

impl<S: Read + Write> Backend for SmbFs<S> {
    type Dir = u64;
    type Node = Node;
    type Key = u64;

    fn root(&self) -> u64 {
        1
    }
    fn key(n: &Node) -> u64 {
        n.id
    }
    fn name(n: &Node) -> &str {
        &n.name
    }
    fn attr(n: &Node) -> Attr {
        let i = n.info;
        Attr { size: i.size, dir: i.dir, read_only: i.read_only, mtime: i.mtime, ctime: i.ctime, atime: i.atime }
    }
    fn as_dir(&self, n: &Node) -> FsResult<u64> {
        if n.info.dir {
            Ok(n.id)
        } else {
            Err(FsError::NotDir)
        }
    }
    fn list(&mut self, d: u64) -> FsResult<Vec<Node>> {
        self.live()?;
        let p = self.path(d)?.to_string();
        let r = self.c.create(&p, FILE_READ_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE, FILE_OPEN, DIRECTORY_FILE);
        let (f, _) = self.chk(r)?;
        let l = self.c.list(f);
        let c = self.c.close(f);
        let l = self.chk(l)?;
        self.chk(c)?;
        // One name never means two files: case-insensitive servers.
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::with_capacity(l.len());
        for (name, info) in l {
            if !smb_name(&name) || info.reparse || !seen.insert(name.to_lowercase()) {
                continue;
            }
            let cp = if p.is_empty() { name.clone() } else { format!("{p}\\{name}") };
            let id = self.intern(cp);
            out.push(Node { id, name, info });
        }
        Ok(out)
    }
    fn lookup(&mut self, d: u64, name: &str) -> FsResult<Node> {
        self.live()?;
        let cp = self.child(d, name)?;
        let info = self.stat(&cp)?;
        let id = self.intern(cp);
        Ok(Node { id, name: name.to_string(), info })
    }
    fn refresh(&mut self, k: u64) -> FsResult<Node> {
        self.live()?;
        let p = self.path(k)?.to_string();
        let info = self.stat(&p)?;
        let name = p.rsplit('\\').next().unwrap_or("").to_string();
        Ok(Node { id: k, name, info })
    }
    fn read(&mut self, n: &Node, off: u64, buf: &mut [u8]) -> FsResult<usize> {
        self.live()?;
        if n.info.dir {
            return Err(FsError::IsDir);
        }
        let f = self.handle(n.id, false)?;
        let mut got = 0;
        while got < buf.len() {
            let r = self.c.read(f, off + got as u64, buf.len() - got);
            let d = self.chk(r)?;
            if d.is_empty() {
                break;
            }
            buf[got..got + d.len()].copy_from_slice(&d);
            got += d.len();
        }
        Ok(got)
    }
    fn statfs(&mut self) -> FsResult<(u32, u64, u64)> {
        self.live()?;
        let root = self.root;
        let r = self.c.fs_size(root);
        let (unit, total, free) = self.chk(r)?;
        // In 4 KiB blocks, whatever the server's unit.
        let bytes = |n: u64| n.saturating_mul(unit) / 4096;
        Ok((4096, bytes(total), bytes(free)))
    }
    fn writable(&self) -> bool {
        self.writable
    }
    fn create(&mut self, d: u64, name: &str, dir: bool, now: i64) -> FsResult<Node> {
        self.live()?;
        let cp = self.child(d, name)?;
        let access = if dir { FILE_READ_ATTRIBUTES | SYNCHRONIZE } else { FILE_READ_DATA | FILE_WRITE_DATA | FILE_READ_ATTRIBUTES | FILE_WRITE_ATTRIBUTES | SYNCHRONIZE };
        let r = self.c.create(&cp, access, FILE_CREATE, if dir { DIRECTORY_FILE } else { NON_DIRECTORY_FILE });
        let (f, mut info) = self.chk(r)?;
        let id = self.intern(cp);
        if dir {
            let r = self.c.close(f);
            self.chk(r)?;
        } else {
            self.keep(id, f, true)?;
        }
        if info.mtime <= 0 {
            info.mtime = now;
        }
        Ok(Node { id, name: name.to_string(), info })
    }
    fn write(&mut self, n: &mut Node, off: u64, data: &[u8], now: i64) -> FsResult<usize> {
        self.live()?;
        if n.info.dir {
            return Err(FsError::IsDir);
        }
        let f = self.handle(n.id, true)?;
        let mut put = 0;
        while put < data.len() {
            let r = self.c.write(f, off + put as u64, &data[put..]);
            let k = self.chk(r)?;
            if k == 0 {
                return Err(FsError::NoSpace);
            }
            put += k;
        }
        n.info.size = n.info.size.max(off + put as u64);
        n.info.mtime = now;
        Ok(put)
    }
    fn truncate(&mut self, n: &mut Node, len: u64, now: i64) -> FsResult<()> {
        self.live()?;
        let f = self.handle(n.id, true)?;
        let r = self.c.set_size(f, len);
        self.chk(r)?;
        n.info.size = len;
        n.info.mtime = now;
        Ok(())
    }
    fn set_mtime(&mut self, n: &mut Node, t: i64) -> FsResult<()> {
        self.live()?;
        if n.info.dir {
            // Folders' times are the server's.
            return Ok(());
        }
        let f = self.handle(n.id, true)?;
        let r = self.c.set_mtime(f, t);
        self.chk(r)?;
        n.info.mtime = t;
        Ok(())
    }
    fn unlink(&mut self, n: &Node) -> FsResult<()> {
        self.remove(n.id, false)
    }
    fn rmdir(&mut self, n: &Node) -> FsResult<()> {
        self.remove(n.id, true)
    }
    fn rename(&mut self, n: &Node, to: u64, name: &str, _now: i64) -> FsResult<Node> {
        self.live()?;
        let from = self.path(n.id)?.to_string();
        let dest = self.child(to, name)?;
        // Nothing of it or what it replaces may stay open.
        self.close_open(None)?;
        let r = self.c.create(&from, DELETE | FILE_READ_ATTRIBUTES | SYNCHRONIZE, FILE_OPEN, OPEN_REPARSE_POINT);
        let (f, _) = self.chk(r)?;
        let r = self.c.rename(f, &dest);
        let c = self.c.close(f);
        self.chk(r)?;
        self.chk(c)?;
        if let Some(&old) = self.ids.get(&dest) {
            if old != n.id {
                self.forget(old);
            }
        }
        self.ids.remove(&from);
        self.ids.insert(dest.clone(), n.id);
        self.paths[n.id as usize] = Some(dest.clone());
        let prefix = format!("{from}\\");
        let under: Vec<(String, u64)> = self.ids.iter().filter(|(k, _)| k.starts_with(&prefix)).map(|(k, v)| (k.clone(), *v)).collect();
        for (k, v) in under {
            let nk = format!("{dest}\\{}", &k[prefix.len()..]);
            self.ids.remove(&k);
            self.ids.insert(nk.clone(), v);
            self.paths[v as usize] = Some(nk);
        }
        Ok(Node { id: n.id, name: name.to_string(), info: n.info })
    }
    fn sync(&mut self) -> FsResult<()> {
        if self.dead {
            return Ok(());
        }
        // Writes are on the server already; closing settles size and time.
        let w: Vec<u64> = self.open.iter().filter(|h| h.2).map(|h| h.0).collect();
        for id in w {
            self.close_open(Some(id))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_and_names() {
        assert_eq!(unix_of(EPOCH_FT as u64), 0);
        assert_eq!(unix_of(filetime_of(1_789_000_000)), 1_789_000_000);
        assert_eq!(filetime_of(i64::MIN), 0);
        assert!(smb_name("report é.txt"));
        for bad in ["a\\b", "a:b", "x*", "q?", "<", "|", "a/b", "..", "", "tab\there"] {
            assert!(!smb_name(bad), "{bad:?}");
        }
        let g = [0x78, 0x56, 0x34, 0x12, 0x34, 0x12, 0x78, 0x56, 1, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(guid_text(&g), "12345678-1234-5678-0102-030405060708");
        assert_ne!(guid_key(&g), guid_key(&[0; 16]));
    }

    fn entry(name: &str, attrs: u32, size: u64, next: u32) -> Vec<u8> {
        let n = utf16(name);
        let mut e = vec![0u8; 64];
        e[0..4].copy_from_slice(&next.to_le_bytes());
        e[24..32].copy_from_slice(&filetime_of(60).to_le_bytes());
        e[40..48].copy_from_slice(&size.to_le_bytes());
        e[56..60].copy_from_slice(&attrs.to_le_bytes());
        e[60..64].copy_from_slice(&(n.len() as u32).to_le_bytes());
        e.extend_from_slice(&n);
        while next != 0 && e.len() < next as usize {
            e.push(0);
        }
        e
    }

    #[test]
    fn listings_are_bounded() {
        let mut b = entry("a.txt", 0x20, 5, 80);
        b.extend(entry("Docs", ATTR_DIRECTORY, 4096, 0));
        let mut out = Vec::new();
        parse_listing(&b, &mut out).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!((out[0].0.as_str(), out[0].1.size, out[0].1.mtime), ("a.txt", 5, 60));
        assert!(out[1].1.dir && out[1].1.size == 0);
        // A name running past the buffer, an entry pointing into itself,
        // a next offset past the end.
        let mut long = entry("abc", 0, 0, 0);
        long[60] = 200;
        assert!(parse_listing(&long, &mut Vec::new()).is_err());
        let mut lap = entry("abc", 0, 0, 72);
        lap[0] = 8;
        assert!(parse_listing(&lap, &mut Vec::new()).is_err());
        let past = entry("abc", 0, 0, 4000);
        assert!(parse_listing(&past[..72], &mut Vec::new()).is_err());
        assert!(parse_listing(&[0; 10], &mut Vec::new()).is_err());
    }

    #[test]
    fn replies_must_answer_the_request() {
        let mut m = header(CREATE, 1, 1, 7, 0, 0).to_vec();
        m[16] = SERVER_TO_REDIR as u8;
        m.extend_from_slice(&[0; 8]);
        assert!(parse_reply(m.clone(), CREATE, 7).is_ok());
        assert!(parse_reply(m.clone(), CREATE, 8).is_err(), "another message id");
        assert!(parse_reply(m.clone(), READ, 7).is_err(), "another command");
        let mut req = m.clone();
        req[16] = 0;
        assert!(parse_reply(req, CREATE, 7).is_err(), "a request");
        let mut comp = m.clone();
        comp[20] = 64;
        assert!(parse_reply(comp, CREATE, 7).is_err(), "compounded");
        assert!(parse_reply(m[..40].to_vec(), CREATE, 7).is_err());
    }

    /// Sealing and opening with the two directions' keys, and a tampered
    /// frame refused.
    #[test]
    fn transform_round_trip() {
        let ki = [7u8; 16];
        let ctx = [9u8; 64];
        let c2s = kdf(&ki, b"SMBC2SCipherKey\0", &ctx, 256);
        let s2c = kdf(&ki, b"SMBS2CCipherKey\0", &ctx, 256);
        assert_eq!(c2s.len(), 32);
        assert_ne!(c2s, s2c);
        let mk = |enc: &[u8], dec: &[u8]| Conn {
            s: std::io::Cursor::new(Vec::new()),
            msg_id: 0,
            credits: 1,
            session_id: 42,
            tree_id: 0,
            keys: Some(Keys { enc: Cipher::new(AES_256_GCM, enc), dec: Cipher::new(AES_256_GCM, dec) }),
            preauth: [0; 64],
            negotiated: Negotiated { guid: [0; 16], cipher: AES_256_GCM },
            max_read: 65536,
            max_write: 65536,
            max_trans: 65536,
            nonce_ctr: 0,
            nonce_salt: [1, 2, 3, 4],
        };
        let mut client = mk(&c2s, &s2c);
        let server = mk(&s2c, &c2s);
        let msg = [header(ECHO, 1, 1, 3, 0, 42).to_vec(), vec![4, 0, 0, 0]].concat();
        let sealed = client.seal(&msg);
        assert_ne!(&sealed[52..], &msg[..]);
        assert_eq!(server.open_sealed(sealed.clone()).unwrap(), msg);
        let mut bad = sealed.clone();
        bad[60] ^= 1;
        assert!(server.open_sealed(bad).is_err());
        let mut other = sealed;
        other[44] = 43;
        assert!(server.open_sealed(other).is_err(), "another session");
        assert!(server.open_sealed(msg.clone()).is_err(), "unencrypted");
    }
}
