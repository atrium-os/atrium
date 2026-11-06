//! The courier's Arca: arcad's courier ops, as the courier's own uid.
//! arcad reads the courier's label (user, persona, endpoint) from the
//! courier registry by that uid; nothing here names it.

use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

use arca_store::codec::{Reader, Writer};
use arca_store::secret::LockedBuf;
use arcad::proto::{self, courier_op, status};

use crate::schema::Refusal;
use crate::send::Vault;

pub struct Arcad {
    path: PathBuf,
}

impl Arcad {
    pub fn new(path: impl Into<PathBuf>) -> Arcad {
        Arcad { path: path.into() }
    }

    fn call(&self, op: u16, body: &[u8]) -> Result<(u8, LockedBuf), Refusal> {
        let mut s = UnixStream::connect(&self.path).map_err(|_| Refusal::Internal)?;
        s.set_read_timeout(Some(Duration::from_secs(5))).ok();
        proto::write_request(&mut s, op, body).map_err(|_| Refusal::Internal)?;
        proto::read_reply(&mut s).map_err(|_| Refusal::Internal)
    }
}

impl Vault for Arcad {
    fn fill(&mut self, refs: &[(u8, [u8; 16])]) -> Result<Vec<LockedBuf>, Refusal> {
        let mut w = Writer::new();
        w.u8(refs.len() as u8);
        for (src, r) in refs {
            w.u8(*src);
            w.raw(r);
        }
        let (st, b) = self.call(courier_op::FILL, &w.buf)?;
        match st {
            status::OK => {}
            status::LOCKED => return Err(Refusal::Locked),
            _ => return Err(Refusal::Denied),
        }
        let mut r = Reader::new(b.as_ref());
        let mut out = Vec::with_capacity(refs.len());
        for _ in refs {
            let v = r.bytes(arca_store::model::caps::VALUE).map_err(|_| Refusal::Internal)?;
            out.push(LockedBuf::from_slice(v).map_err(|_| Refusal::Internal)?);
        }
        r.end().map_err(|_| Refusal::Internal)?;
        Ok(out)
    }

    fn set_cookie(&mut self, name: &str, value: &[u8], attrs: &[u8]) {
        let mut w = Writer::new();
        w.str(name);
        w.bytes(value);
        w.bytes(attrs);
        let _ = self.call(courier_op::SET_COOKIE, &w.buf);
    }
}
