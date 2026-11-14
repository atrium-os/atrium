//! The share server's side of Arca (network-shares.md §5.1, arcad's
//! `share_op`). Arca knows which share this process serves from its uid;
//! each request also names the host key digest the server just proved, and
//! Arca answers only if it is the one pinned for this share.
//!
//! The wire is arcad's (arcad::proto): a request is `u32 LE length | u16 LE
//! op | body`, a reply `u32 LE length | u8 status | body`; strings and byte
//! strings are u32-LE-length prefixed. Kept here, a few lines, so the share
//! server doesn't link arcad.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use zeroize::Zeroizing;

pub const SH_ITEMS: u16 = 0xA8;
pub const SH_FILL: u16 = 0xA9;
pub const SH_PUBKEY: u16 = 0xAA;
pub const SH_SIGN: u16 = 0xAB;

/// arca_store's `Kind`s a share holds.
pub const KIND_PASSWORD: u16 = 1;
pub const KIND_ED25519: u16 = 0x100;

const MAX_FRAME: usize = 64 * 1024;

#[derive(Debug)]
pub enum ArcaError {
    Io(io::Error),
    /// arcad's status: 1 denied, 2 locked, 5 unknown peer, ...
    Refused(u8),
    Malformed,
}

impl std::fmt::Display for ArcaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArcaError::Io(e) => write!(f, "{e}"),
            ArcaError::Refused(2) => write!(f, "the session is locked"),
            ArcaError::Refused(1) => write!(f, "refused (not this share's item, or not its pinned server)"),
            ArcaError::Refused(5) => write!(f, "this process is not a registered share server"),
            ArcaError::Refused(s) => write!(f, "refused, status {s}"),
            ArcaError::Malformed => write!(f, "malformed reply"),
        }
    }
}

impl From<io::Error> for ArcaError {
    fn from(e: io::Error) -> Self {
        ArcaError::Io(e)
    }
}

pub struct Item {
    pub id: [u8; 16],
    pub kind: u16,
    pub account: String,
}

/// One share's view of Arca, for the identity `key` the server proved.
pub struct Arca {
    s: UnixStream,
    key: [u8; 32],
}

impl Arca {
    pub fn connect(path: &str, key: [u8; 32]) -> Result<Arca, ArcaError> {
        Arca::from_stream(UnixStream::connect(path)?, key)
    }

    /// Over a connection made elsewhere: the dialer's, once this process
    /// is in capability mode (S4).
    pub fn from_stream(s: UnixStream, key: [u8; 32]) -> Result<Arca, ArcaError> {
        s.set_read_timeout(Some(Duration::from_secs(10)))?;
        Ok(Arca { s, key })
    }

    /// A request; the reply body is in memory that is wiped when dropped.
    fn call(&mut self, op: u16, rest: &[u8]) -> Result<Zeroizing<Vec<u8>>, ArcaError> {
        let mut f = Zeroizing::new(Vec::with_capacity(4 + 2 + 32 + rest.len()));
        f.extend_from_slice(&((2 + 32 + rest.len()) as u32).to_le_bytes());
        f.extend_from_slice(&op.to_le_bytes());
        f.extend_from_slice(&self.key);
        f.extend_from_slice(rest);
        self.s.write_all(&f)?;
        let mut len = [0u8; 4];
        self.s.read_exact(&mut len)?;
        let n = u32::from_le_bytes(len) as usize;
        if !(1..=MAX_FRAME).contains(&n) {
            return Err(ArcaError::Malformed);
        }
        let mut st = [0u8; 1];
        self.s.read_exact(&mut st)?;
        let mut body = Zeroizing::new(vec![0u8; n - 1]);
        self.s.read_exact(&mut body)?;
        if st[0] != 0 {
            return Err(ArcaError::Refused(st[0]));
        }
        Ok(body)
    }

    /// This share's items. Arca still locked waits a little: a share
    /// brought back at unlock (S4) may ask a moment before Arca has its
    /// keys again.
    pub fn items(&mut self) -> Result<Vec<Item>, ArcaError> {
        let until = std::time::Instant::now() + Duration::from_secs(10);
        let b = loop {
            match self.call(SH_ITEMS, &[]) {
                Err(ArcaError::Refused(2)) if std::time::Instant::now() < until => std::thread::sleep(Duration::from_millis(500)),
                r => break r?,
            }
        };
        let mut r = Cur(&b);
        let n = r.u32()?;
        let mut out = Vec::new();
        for _ in 0..n {
            let id: [u8; 16] = r.take(16)?.try_into().unwrap();
            let kind = u16::from_le_bytes(r.take(2)?.try_into().unwrap());
            let len = r.u32()? as usize;
            let account = String::from_utf8(r.take(len)?.to_vec()).map_err(|_| ArcaError::Malformed)?;
            out.push(Item { id, kind, account });
        }
        Ok(out)
    }

    /// A password, for one login.
    pub fn fill(&mut self, id: &[u8; 16]) -> Result<Zeroizing<Vec<u8>>, ArcaError> {
        let b = self.call(SH_FILL, id)?;
        let mut r = Cur(&b);
        let n = r.u32()? as usize;
        Ok(Zeroizing::new(r.take(n)?.to_vec()))
    }

    pub fn pubkey(&mut self, id: &[u8; 16]) -> Result<[u8; 32], ArcaError> {
        let b = self.call(SH_PUBKEY, id)?;
        b.as_slice().try_into().map_err(|_| ArcaError::Malformed)
    }

    pub fn sign(&mut self, id: &[u8; 16], msg: &[u8]) -> Result<[u8; 64], ArcaError> {
        let mut rest = Vec::with_capacity(20 + msg.len());
        rest.extend_from_slice(id);
        rest.extend_from_slice(&(msg.len() as u32).to_le_bytes());
        rest.extend_from_slice(msg);
        let b = self.call(SH_SIGN, &rest)?;
        b.as_slice().try_into().map_err(|_| ArcaError::Malformed)
    }
}

struct Cur<'a>(&'a [u8]);

impl<'a> Cur<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ArcaError> {
        if n > self.0.len() {
            return Err(ArcaError::Malformed);
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u32(&mut self) -> Result<u32, ArcaError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
}
