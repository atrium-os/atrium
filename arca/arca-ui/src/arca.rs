//! arca-ui's side of arcad's trusted-UI ops (arcad::proto::op::U_*). arcad
//! knows arca-ui by its uid (ARCA_UI_UID), so these work for no one else.
//!
//! Every field op names the field's parent: who the user is filling *for*.
//! arca-ui resolves it itself from the launch registry, using the uid
//! limend read off the parent's connection; never from anything the parent
//! says.

use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use arca_store::codec::{Reader, Writer};
use arca_store::secret::LockedBuf;
use arcad::proto::{self, op, status};

use crate::field::Account;

/// The app a field fills for, as the registry has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parent {
    pub user: String,
    pub publisher: [u8; 32],
    pub app_id: String,
}

impl Parent {
    /// Look `uid` up in the launch registry at `path`. `None` for an
    /// unregistered uid, and for an unsigned app unless `allow_unsigned`
    /// (dev only, as for arcad).
    pub fn resolve(path: &str, uid: u32, allow_unsigned: bool) -> Option<Parent> {
        let reg = portcullis_peer::AppRegistry::load(path).ok()?;
        let (user, app_id) = reg.resolve(uid)?;
        let publisher = match reg.publisher(uid)? {
            Some(p) if p != arcad::label::UNSIGNED_PUBLISHER => p,
            _ if allow_unsigned => arcad::label::UNSIGNED_PUBLISHER,
            _ => return None,
        };
        Some(Parent { user: user.into(), publisher, app_id: app_id.into() })
    }

    fn write(&self, w: &mut Writer) {
        w.str(&self.user);
        w.raw(&self.publisher);
        w.str(&self.app_id);
    }
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
    /// arcad's status byte.
    Status(u8),
    Malformed,
}

impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Error::Io(e)
    }
}

impl Error {
    pub fn locked(&self) -> bool {
        matches!(self, Error::Status(status::LOCKED))
    }
}

pub struct Arcad {
    path: PathBuf,
}

impl Arcad {
    pub fn new(path: impl Into<PathBuf>) -> Arcad {
        Arcad { path: path.into() }
    }

    /// One request per connection, as arcad's other clients do.
    fn call(&self, opc: u16, body: &[u8]) -> Result<LockedBuf, Error> {
        let mut s = UnixStream::connect(&self.path)?;
        s.set_read_timeout(Some(Duration::from_secs(5)))?;
        proto::write_request(&mut s, opc, body)?;
        let (st, b) = proto::read_reply(&mut s)?;
        if st != status::OK {
            return Err(Error::Status(st));
        }
        Ok(b)
    }

    fn handle(b: &LockedBuf) -> Result<[u8; 16], Error> {
        b.as_ref().try_into().map_err(|_| Error::Malformed)
    }

    /// The parent's saved passwords: ids and account names, no values.
    pub fn accounts(&self, p: &Parent) -> Result<Vec<Account>, Error> {
        let mut w = Writer::new();
        p.write(&mut w);
        let b = self.call(op::U_ACCOUNTS, &w.buf)?;
        let mut r = Reader::new(b.as_ref());
        let mut parse = || -> Result<Vec<Account>, arca_store::Error> {
            let n = r.u32()? as usize;
            let mut out = Vec::with_capacity(n.min(1024));
            for _ in 0..n {
                let id = r.array::<16>()?;
                let name = std::str::from_utf8(r.bytes(arca_store::model::caps::ACCOUNT)?)
                    .map_err(|_| arca_store::Error::Corrupt("account"))?
                    .to_string();
                out.push(Account { id, name });
            }
            r.end()?;
            Ok(out)
        };
        parse().map_err(|_| Error::Malformed)
    }

    /// A handle to the item the user picked.
    pub fn handle_for(&self, p: &Parent, item: &[u8; 16]) -> Result<[u8; 16], Error> {
        let mut w = Writer::new();
        p.write(&mut w);
        w.raw(item);
        Self::handle(&self.call(op::U_HANDLE, &w.buf)?)
    }

    /// A strong password made inside arcad, held pending.
    pub fn generate(&self, p: &Parent) -> Result<[u8; 16], Error> {
        let mut w = Writer::new();
        p.write(&mut w);
        Self::handle(&self.call(op::U_GENERATE, &w.buf)?)
    }

    /// Hold what the user typed. The request is built in locked memory and
    /// written straight to the socket: the value is never in a `Vec`.
    pub fn hold(&self, p: &Parent, value: &[u8]) -> Result<[u8; 16], Error> {
        let mut w = Writer::new();
        p.write(&mut w);
        let head = w.buf;
        let n = 2 + head.len() + 4 + value.len();
        let mut frame = LockedBuf::zeroed(4 + n)?;
        {
            let f = frame.as_mut();
            f[..4].copy_from_slice(&(n as u32).to_le_bytes());
            f[4..6].copy_from_slice(&op::U_HOLD.to_le_bytes());
            let mut at = 6;
            f[at..at + head.len()].copy_from_slice(&head);
            at += head.len();
            f[at..at + 4].copy_from_slice(&(value.len() as u32).to_le_bytes());
            at += 4;
            f[at..].copy_from_slice(value);
        }
        let mut s = UnixStream::connect(&self.path)?;
        s.set_read_timeout(Some(Duration::from_secs(5)))?;
        s.write_all(frame.as_ref())?;
        drop(frame);
        let (st, b) = proto::read_reply(&mut s)?;
        if st != status::OK {
            return Err(Error::Status(st));
        }
        Self::handle(&b)
    }

    pub fn drop_handle(&self, h: &[u8; 16]) -> Result<(), Error> {
        self.call(op::U_DROP, h).map(|_| ())
    }
}
