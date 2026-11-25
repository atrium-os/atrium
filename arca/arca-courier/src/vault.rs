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

    /// Return only an endpoint-bound login handle to Mail. Serialize
    /// exchanges for the same account, including its background IDLE worker.
    pub fn refresh_mail(&self, ids: &[u8], net: &(dyn navigator_fetch::Connect + Sync),
        tls: &std::sync::Arc<rustls::ClientConfig>, policy: navigator_fetch::addr::Policy) -> Result<Vec<u8>, Refusal> {
        if ids.len() != 33 { return Err(Refusal::Malformed); }
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        let start = loop {
            let (st, body) = self.call(courier_op::MAIL_REFRESH_BEGIN, ids)?;
            if st == status::OK { break body; }
            if st != status::FULL || std::time::Instant::now() >= deadline { return Err(Refusal::Denied); }
            std::thread::sleep(Duration::from_millis(200));
        };
        let (nonce, grant) = start.as_ref().split_at_checked(16).ok_or(Refusal::Malformed)?;
        let exchange = (|| {
            let g = crate::oauth::parse_grant(grant).filter(|g| g.provider == "microsoft").ok_or(Refusal::Denied)?;
            let tokens = crate::oauth::refresh(&crate::oauth::MICROSOFT, g.client_id, g.refresh, net, tls, policy)
                .map_err(|e| match e { crate::oauth::RefreshError::Failed(r) => r, crate::oauth::RefreshError::SignInAgain => Refusal::SignInAgain })?;
            let refresh = tokens.refresh.as_ref().map_or(&[][..], |v| v.as_ref());
            let access = tokens.access.as_ref();
            let mut body = LockedBuf::zeroed(32 + refresh.len() + access.len()).map_err(|_| Refusal::Internal)?;
            let mut w = std::io::Cursor::new(body.as_mut());
            use std::io::Write;
            for part in [nonce, &(refresh.len() as u32).to_le_bytes(), refresh, &(access.len() as u32).to_le_bytes(),
                access, &tokens.expires_in.to_le_bytes()] {
                w.write_all(part).map_err(|_| Refusal::Internal)?;
            }
            match self.call(courier_op::MAIL_REFRESH_FINISH, body.as_ref())? {
                (status::OK, ticket) if ticket.len() == 16 => Ok(ticket.as_ref().to_vec()),
                _ => Err(Refusal::Denied),
            }
        })();
        if exchange.is_err() { let _ = self.call(courier_op::MAIL_REFRESH_CANCEL, nonce); }
        exchange
    }

    pub fn redeem_mail(&self, request: &[u8], net: &(dyn navigator_fetch::Connect + Sync),
        tls: &std::sync::Arc<rustls::ClientConfig>, policy: navigator_fetch::addr::Policy) -> Result<(), Refusal> {
        let ticket = request.get(..16).ok_or(Refusal::Malformed)?;
        let (code, form) = self.call(courier_op::MAIL_CODE, request)?;
        if code != status::OK { return Err(Refusal::Denied); }
        let tokens = crate::oauth::exchange_body(&crate::oauth::MICROSOFT, form, net, tls, policy)
            .map_err(|_| Refusal::Http)?;
        let refresh = tokens.refresh.ok_or(Refusal::Http)?;
        let mut grant = LockedBuf::zeroed(20 + refresh.len()).map_err(|_| Refusal::Internal)?;
        grant.as_mut()[..16].copy_from_slice(ticket);
        grant.as_mut()[16..20].copy_from_slice(&(refresh.len() as u32).to_le_bytes());
        grant.as_mut()[20..].copy_from_slice(refresh.as_ref());
        match self.call(courier_op::MAIL_GRANT, grant.as_ref())? {
            (status::OK, b) if b.is_empty() => Ok(()),
            _ => Err(Refusal::Denied),
        }
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

    fn cookies(&mut self, method: crate::schema::Method, path: &str, query: &str, ticket: Option<&[u8; 16]>, origin: Option<&str>) -> Result<Option<LockedBuf>, Refusal> {
        let mut w = Writer::new();
        w.str(path);
        // A ticket is redeemed for exactly this request: its method and
        // query go with it, and its Origin, which Arca checks against the
        // ticket's initiator.
        match (ticket, origin) {
            (Some(t), origin) => {
                w.raw(t);
                w.u8(method as u8);
                w.str(query);
                if let Some(o) = origin { w.str(o); }
            }
            (None, Some(_)) => return Err(Refusal::Denied),
            (None, None) => {}
        }
        match self.call(courier_op::COOKIES, &w.buf) {
            Ok((status::OK, b)) if !b.is_empty() => Ok(Some(b)),
            // A refused Origin refuses the request; without one, a refusal
            // is no cookies.
            Ok((status::DENIED, _)) | Err(_) if origin.is_some() => Err(Refusal::Denied),
            _ => Ok(None),
        }
    }
}

impl crate::mail::MailVault for Arcad {
    fn login(&mut self, item: &[u8; 16]) -> Result<(String, crate::mail::Secret), Refusal> {
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
        let kind = r.u8().map_err(|_| Refusal::Internal)?;
        r.end().map_err(|_| Refusal::Internal)?;
        match kind {
            courier_op::LOGIN_PASSWORD => Ok((account, crate::mail::Secret::Password(v))),
            courier_op::LOGIN_ACCESS => Ok((account, crate::mail::Secret::Access(v))),
            courier_op::LOGIN_OAUTH => Ok((account, crate::mail::Secret::OAuth(v))),
            _ => Err(Refusal::Internal),
        }
    }

    fn refreshed(&mut self, item: &[u8; 16], grant: &LockedBuf) -> Result<(), Refusal> {
        let mut w = Writer::new();
        w.u8(courier_op::SOURCE_ITEM);
        w.raw(item);
        w.bytes(grant.as_ref());
        let (st, _) = self.call(courier_op::REFRESHED, &w.buf)?;
        // The request held the token: wipe it.
        w.buf.iter_mut().for_each(|b| *b = 0);
        match st {
            status::OK => Ok(()),
            status::LOCKED => Err(Refusal::Locked),
            _ => Err(Refusal::Denied),
        }
    }

    fn anomaly(&mut self, code: u8, detail: &str) {
        let mut w = Writer::new();
        w.u8(code);
        w.str(&detail[..detail.len().min(courier_op::ANOMALY_DETAIL_MAX)]);
        let _ = self.call(courier_op::ANOMALY, &w.buf);
    }
}
