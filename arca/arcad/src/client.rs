//! A client for Vestibulum's side of Arca: derive the user's root key from
//! the passcode it just verified, and unlock (or, at first login, create)
//! the user's store; lock it at sign-out (arca.md §4.2, §6.1).

use std::io;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use arca_store::codec::{Reader, Writer};
use arca_store::{KdfParams, RootKey};

use crate::proto::{self, op, status};

/// What happened at unlock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unlocked {
    Existing,
    /// First login: a new store was made.
    Created,
}

#[derive(Debug)]
pub enum ClientError {
    Io(io::Error),
    /// arcad answered with this status.
    Refused(u8),
    Derive,
}

impl From<io::Error> for ClientError {
    fn from(e: io::Error) -> Self {
        ClientError::Io(e)
    }
}

pub struct VestibulumLink {
    pub socket: PathBuf,
}

impl VestibulumLink {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        VestibulumLink { socket: socket.into() }
    }

    fn call(&self, opc: u16, body: &[u8]) -> Result<(u8, arca_store::secret::LockedBuf), ClientError> {
        let mut s = UnixStream::connect(&self.socket)?;
        s.set_read_timeout(Some(Duration::from_secs(10)))?;
        proto::write_request(&mut s, opc, body)?;
        Ok(proto::read_reply(&mut s)?)
    }

    fn user_body(user: &str) -> Vec<u8> {
        let mut w = Writer::new();
        w.str(user);
        w.buf
    }

    /// Derive the root key for `user` from `passcode` (which the caller has
    /// just verified) and unlock their store, creating it at first login.
    pub fn unlock(&self, user: &str, passcode: &[u8]) -> Result<Unlocked, ClientError> {
        let (st, b) = self.call(op::V_KDF, &Self::user_body(user))?;
        if st != status::OK {
            return Err(ClientError::Refused(st));
        }
        let mut r = Reader::new(b.as_ref());
        let parsed = (|| {
            let salt = r.array::<{ arca_store::root::SALT_LEN }>()?;
            let k = KdfParams { m_kib: r.u32()?, t_cost: r.u32()?, p_cost: r.u32()? };
            r.end()?;
            Ok::<_, arca_store::Error>((salt, k))
        })();
        let (salt, k) = parsed.map_err(|_| ClientError::Refused(status::MALFORMED))?;
        let root = RootKey::derive(passcode, &salt, k).map_err(|_| ClientError::Derive)?;
        let body = |root: &RootKey| -> Result<arca_store::secret::LockedBuf, ClientError> {
            let ub = Self::user_body(user);
            let mut b = arca_store::secret::LockedBuf::zeroed(ub.len() + 32)?;
            b.as_mut()[..ub.len()].copy_from_slice(&ub);
            b.as_mut()[ub.len()..].copy_from_slice(root.as_bytes());
            Ok(b)
        };
        let req = body(&root)?;
        match self.call(op::V_UNLOCK, req.as_ref())?.0 {
            status::OK => Ok(Unlocked::Existing),
            status::NO_STORE => match self.call(op::V_CREATE, req.as_ref())?.0 {
                status::OK => Ok(Unlocked::Created),
                st => Err(ClientError::Refused(st)),
            },
            st => Err(ClientError::Refused(st)),
        }
    }

    /// Lock `user`'s store: while-unlocked items become unreadable.
    pub fn lock(&self, user: &str) -> Result<(), ClientError> {
        match self.call(op::V_LOCK, &Self::user_body(user))?.0 {
            status::OK => Ok(()),
            st => Err(ClientError::Refused(st)),
        }
    }

    /// 0 none, 1 sealed, 2 locked, 3 unlocked.
    pub fn state(&self, user: &str) -> Result<u8, ClientError> {
        let (st, b) = self.call(op::V_STATE, &Self::user_body(user))?;
        if st != status::OK || b.len() != 1 {
            return Err(ClientError::Refused(st));
        }
        Ok(b.as_ref()[0])
    }
}
