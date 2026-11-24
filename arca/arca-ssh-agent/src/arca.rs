//! The agent's calls to arcad, as the `ssh-agent` service (its uid is in
//! `ARCA_SERVICES`). arcad answers only that uid.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use arca_store::codec::{Reader, Writer};
use arca_store::Kind;
use arcad::proto::{self, ssh_op, status};

use crate::wire::Key;

pub struct Arca {
    pub socket: PathBuf,
}

/// A key as the person sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// An HTTPS token (its comment is `host username`, pk empty), not a key.
    pub token: bool,
    pub key: Key,
    pub grants: Vec<String>,
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// arcad's status.
    Refused(u8),
    Malformed,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "Arca: {e}"),
            Error::Refused(status::LOCKED) => write!(f, "Arca is locked"),
            Error::Refused(status::DENIED) => write!(f, "no such key"),
            Error::Refused(status::NEEDS_VERIFICATION) => write!(f, "security key verification required"),
            Error::Refused(s) => write!(f, "Arca refused ({s})"),
            Error::Malformed => write!(f, "Arca's answer was malformed"),
        }
    }
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl From<arca_store::Error> for Error {
    fn from(_: arca_store::Error) -> Self {
        Error::Malformed
    }
}

fn owner(w: &mut Writer, user: &str, persona: &str) {
    w.str(user);
    w.str(crate::arca_persona(persona));
}

impl Arca {
    fn call(&self, op: u16, body: &[u8]) -> Result<Vec<u8>, Error> {
        let mut s = UnixStream::connect(&self.socket)?;
        // Signing may be waiting on an enrolled key on Sigillum's sheet.
        let read_seconds = if op == ssh_op::SA_SIGN { 11 * 60 + 5 } else { 10 };
        s.set_read_timeout(Some(Duration::from_secs(read_seconds)))?;
        s.set_write_timeout(Some(Duration::from_secs(10)))?;
        proto::write_request(&mut s, op, body)?;
        let (st, b) = proto::read_reply(&mut s)?;
        if st != status::OK {
            return Err(Error::Refused(st));
        }
        Ok(b.as_ref().to_vec())
    }

    /// A new key: P-256 (`p256`) or Ed25519, verify-each-use (`touch`) or
    /// while-unlocked.
    pub fn mint(&self, user: &str, persona: &str, comment: &str, touch: bool, p256: bool) -> Result<Key, Error> {
        let mut w = Writer::new();
        owner(&mut w, user, persona);
        w.str(comment);
        w.u8(if touch { arca_store::Class::VerifyEachUse } else { arca_store::Class::WhileUnlocked } as u8);
        w.u16(if p256 { Kind::P256 } else { Kind::Ed25519 } as u16);
        let b = self.call(ssh_op::SA_MINT, &w.buf)?;
        let mut r = Reader::new(&b);
        let id = r.array()?;
        let pk = r.raw(if p256 { 65 } else { 32 })?.to_vec();
        r.end()?;
        Ok(Key { id, p256, pk, comment: comment.into() })
    }

    pub fn list(&self, user: &str, persona: &str) -> Result<Vec<Listed>, Error> {
        let mut w = Writer::new();
        owner(&mut w, user, persona);
        let b = self.call(ssh_op::SA_LIST, &w.buf)?;
        let mut r = Reader::new(&b);
        let n = r.u32()?;
        let mut v = Vec::new();
        for _ in 0..n {
            let id = r.array()?;
            let kind = r.u16()?;
            let token = kind == Kind::ApiKey as u16;
            let p256 = kind == Kind::P256 as u16;
            let key = Key { id, p256, pk: r.bytes(65)?.to_vec(), comment: r.str(arca_store::model::caps::ACCOUNT)?.into() };
            let grants = arcad::ssh::grants(r.str(arca_store::model::caps::SCOPE)?.as_bytes()).into_iter().map(String::from).collect();
            v.push(Listed { token, key, grants });
        }
        r.end()?;
        Ok(v)
    }

    /// Store an HTTPS token for `host` (replacing one for the same host and
    /// username, its grants kept); its id.
    pub fn token_add(&self, user: &str, persona: &str, host: &str, username: &str, token: &[u8]) -> Result<[u8; 16], Error> {
        let mut w = Writer::new();
        owner(&mut w, user, persona);
        w.str(host);
        w.str(username);
        w.bytes(token);
        let b = self.call(ssh_op::SA_TOKEN_ADD, &w.buf)?;
        b.as_slice().try_into().map_err(|_| Error::Malformed)
    }

    /// The token for `host` granted to workspace `ws`: username and token.
    pub fn token_get(&self, user: &str, persona: &str, ws: &str, host: &str) -> Result<(String, Vec<u8>), Error> {
        let mut w = Writer::new();
        owner(&mut w, user, persona);
        w.str(ws);
        w.str(host);
        let b = self.call(ssh_op::SA_TOKEN_GET, &w.buf)?;
        let mut r = Reader::new(&b);
        let u = r.str(arca_store::model::caps::ACCOUNT)?.to_string();
        let t = r.bytes(ssh_op::TOKEN_MAX)?.to_vec();
        r.end()?;
        Ok((u, t))
    }

    pub fn grant(&self, user: &str, persona: &str, id: &[u8; 16], ws: &str, on: bool) -> Result<(), Error> {
        let mut w = Writer::new();
        owner(&mut w, user, persona);
        w.raw(id);
        w.str(ws);
        self.call(if on { ssh_op::SA_GRANT } else { ssh_op::SA_REVOKE }, &w.buf).map(|_| ())
    }

    pub fn delete(&self, user: &str, persona: &str, id: &[u8; 16]) -> Result<(), Error> {
        let mut w = Writer::new();
        owner(&mut w, user, persona);
        w.raw(id);
        self.call(ssh_op::SA_DELETE, &w.buf).map(|_| ())
    }

    /// Take workspace `ws` out of every key's grants (it was deleted).
    pub fn forget(&self, user: &str, persona: &str, ws: &str) -> Result<u32, Error> {
        let mut w = Writer::new();
        owner(&mut w, user, persona);
        w.str(ws);
        let b = self.call(ssh_op::SA_FORGET, &w.buf)?;
        Ok(u32::from_le_bytes(b.as_slice().try_into().map_err(|_| Error::Malformed)?))
    }

    /// The keys granted to workspace `ws` of `user`, in its persona.
    pub fn keys(&self, user: &str, persona: &str, ws: &str) -> Result<Vec<Key>, Error> {
        let mut w = Writer::new();
        owner(&mut w, user, persona);
        w.str(ws);
        let b = self.call(ssh_op::SA_KEYS, &w.buf)?;
        let mut r = Reader::new(&b);
        let n = r.u32()?;
        let mut v = Vec::new();
        for _ in 0..n {
            let id = r.array()?;
            let p256 = r.u16()? == Kind::P256 as u16;
            let pk = r.bytes(65)?.to_vec();
            if pk.len() != if p256 { 65 } else { 32 } {
                return Err(Error::Malformed);
            }
            v.push(Key { id, p256, pk, comment: r.str(ssh_op::COMMENT_MAX)?.into() });
        }
        r.end()?;
        Ok(v)
    }

    pub fn sign(&self, user: &str, persona: &str, ws: &str, id: &[u8; 16], msg: &[u8]) -> Result<[u8; 64], Error> {
        let mut w = Writer::new();
        owner(&mut w, user, persona);
        w.str(ws);
        w.raw(id);
        w.bytes(msg);
        let b = self.call(ssh_op::SA_SIGN, &w.buf)?;
        b.as_slice().try_into().map_err(|_| Error::Malformed)
    }
}
