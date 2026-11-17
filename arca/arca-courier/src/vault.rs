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

/// Longer than arcad's own wait on sigillumd (11 minutes).
const VERIFY_WAIT: Duration = Duration::from_secs(12 * 60);

pub struct Arcad {
    path: PathBuf,
}

impl Arcad {
    pub fn new(path: impl Into<PathBuf>) -> Arcad {
        Arcad { path: path.into() }
    }

    fn call(&self, op: u16, body: &[u8]) -> Result<(u8, LockedBuf), Refusal> {
        let mut s = UnixStream::connect(&self.path).map_err(|_| Refusal::Internal)?;
        // A FILL of a verify-each-use item waits while the user answers
        // Sigillum's sheet (arcad bounds that wait); everything else is quick.
        let wait = if op == courier_op::FILL { VERIFY_WAIT } else { Duration::from_secs(5) };
        s.set_read_timeout(Some(wait)).ok();
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
            status::NEEDS_VERIFICATION => return Err(Refusal::NotVerified),
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

    fn set_cookie(&mut self, name: &str, value: &[u8], attrs: &[u8], path: &str, ticket: Option<&[u8; 16]>) {
        let mut w = Writer::new();
        w.str(name);
        w.bytes(value);
        w.bytes(attrs);
        w.str(path);
        if let Some(t) = ticket {
            w.raw(t);
        }
        let _ = self.call(courier_op::SET_COOKIE, &w.buf);
    }

    fn cookies(&mut self, method: crate::schema::Method, path: &str, query: &str, ticket: Option<&[u8; 16]>) -> Option<LockedBuf> {
        let mut w = Writer::new();
        w.str(path);
        // A ticket is redeemed for exactly this request: its method and
        // query go with it.
        if let Some(t) = ticket {
            w.raw(t);
            w.u8(method as u8);
            w.str(query);
        }
        match self.call(courier_op::COOKIES, &w.buf) {
            Ok((status::OK, b)) if !b.is_empty() => Some(b),
            _ => None,
        }
    }
}

impl crate::mail::MailVault for Arcad {
    fn login(&mut self, item: &[u8; 16]) -> Result<(String, LockedBuf), Refusal> {
        let mut w = Writer::new();
        w.u8(courier_op::SOURCE_ITEM);
        w.raw(item);
        let (st, b) = self.call(courier_op::LOGIN, &w.buf)?;
        match st {
            status::OK => {}
            status::LOCKED => return Err(Refusal::Locked),
            status::NEEDS_VERIFICATION => return Err(Refusal::NotVerified),
            _ => return Err(Refusal::Denied),
        }
        let mut r = Reader::new(b.as_ref());
        let account = r.str(arca_store::model::caps::ACCOUNT).map_err(|_| Refusal::Internal)?.to_string();
        let v = r.bytes(arca_store::model::caps::VALUE).map_err(|_| Refusal::Internal)?;
        let v = LockedBuf::from_slice(v).map_err(|_| Refusal::Internal)?;
        r.end().map_err(|_| Refusal::Internal)?;
        Ok((account, v))
    }

    fn anomaly(&mut self, code: u8, detail: &str) {
        let mut w = Writer::new();
        w.u8(code);
        w.str(&detail[..detail.len().min(courier_op::ANOMALY_DETAIL_MAX)]);
        let _ = self.call(courier_op::ANOMALY, &w.buf);
    }
}
