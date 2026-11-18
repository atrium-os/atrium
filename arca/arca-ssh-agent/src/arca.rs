//! The agent's calls to arcad, as the `ssh-agent` service (its uid is in
//! `ARCA_SERVICES`). arcad answers only that uid.

use std::io;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use arca_store::codec::{Reader, Writer};
use arcad::proto::{self, ssh_op, status};

use crate::wire::Key;

pub struct Arca {
    pub socket: PathBuf,
}

/// A key as the person sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
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
        s.set_read_timeout(Some(Duration::from_secs(10)))?;
        s.set_write_timeout(Some(Duration::from_secs(10)))?;
        proto::write_request(&mut s, op, body)?;
        let (st, b) = proto::read_reply(&mut s)?;
        if st != status::OK {
            return Err(Error::Refused(st));
        }
        Ok(b.as_ref().to_vec())
    }

    pub fn mint(&self, user: &str, persona: &str, comment: &str) -> Result<Key, Error> {
        let mut w = Writer::new();
        owner(&mut w, user, persona);
        w.str(comment);
        let b = self.call(ssh_op::SA_MINT, &w.buf)?;
        let mut r = Reader::new(&b);
        let k = Key { id: r.array()?, pk: r.array()?, comment: comment.into() };
        r.end()?;
        Ok(k)
    }

    pub fn list(&self, user: &str, persona: &str) -> Result<Vec<Listed>, Error> {
        let mut w = Writer::new();
        owner(&mut w, user, persona);
        let b = self.call(ssh_op::SA_LIST, &w.buf)?;
        let mut r = Reader::new(&b);
        let n = r.u32()?;
        let mut v = Vec::new();
        for _ in 0..n {
            let key = Key { id: r.array()?, pk: r.array()?, comment: r.str(ssh_op::COMMENT_MAX)?.into() };
            let grants = arcad::ssh::grants(r.str(arca_store::model::caps::SCOPE)?.as_bytes()).into_iter().map(String::from).collect();
            v.push(Listed { key, grants });
        }
        r.end()?;
        Ok(v)
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
            v.push(Key { id: r.array()?, pk: r.array()?, comment: r.str(ssh_op::COMMENT_MAX)?.into() });
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
