//! Microsoft IMAP and submission share a rotating grant. Only the provider
//! courier may refresh it. Each mail courier receives a single login handle.
use super::*;
use crate::oauth::MICROSOFT;
use std::time::{Duration, Instant};

pub(super) const LINK: &[u8] = b"mail-grant-ref ";
const GRANT: &[u8] = b"oauth1 microsoft ";

pub(super) fn link(id: ItemId) -> Vec<u8> {
    [LINK, &id.0].concat()
}

pub(super) const OWNER: &[u8] = b"mail-refresh-owner";

pub(super) struct Lease {
    owner: CourierLabel,
    incoming: ItemId,
    destination: Recipient,
    account: String,
    expires: Instant,
}

/// A reauthorization is valid only for this signed persona’s linked Outlook pair.
pub(super) fn renewal_owner(store: &Store, persona: &str, address: &str, ids: &[u8; 32]) -> Result<(ItemId, u64), Error> {
    let id = ItemId(ids[..16].try_into().unwrap());
    let imap = Recipient::endpoint(Scheme::Imaps, MICROSOFT.imap.0, MICROSOFT.imap.1, None)?;
    let smtp = Recipient::endpoint(Scheme::Submission, MICROSOFT.smtp.0, MICROSOFT.smtp.1, None)?;
    let im = store.meta(&id, &Access { persona, recipient: &imap })?;
    let (sm, alias) = store.release(&ItemId(ids[16..].try_into().unwrap()), &Access { persona, recipient: &smtp }, None)?;
    if im.kind != Kind::OAuthToken || im.scope != OWNER || im.account != address || sm.kind != Kind::OAuthToken
        || sm.account != address || alias.as_ref() != link(id) { return Err(Error::Denied); }
    Ok((id, im.version))
}

impl State {
    pub(super) fn refresh_mail(&mut self, c: &CourierLabel, op: u16, body: &[u8]) -> Reply {
        let provider = tri!(Recipient::endpoint(Scheme::Https, MICROSOFT.token_host, 443, None));
        if c.endpoint != provider || !self.mail_sign_ins.mail_publisher.is_some_and(|p| app_persona(&p, "org.atrium.mail") == c.persona) {
            return reply(status::DENIED);
        }
        let Some(store) = self.stores.get_mut(&c.user) else { return reply(status::LOCKED) };
        let imap = tri!(Recipient::endpoint(Scheme::Imaps, MICROSOFT.imap.0, MICROSOFT.imap.1, None));
        let smtp = tri!(Recipient::endpoint(Scheme::Submission, MICROSOFT.smtp.0, MICROSOFT.smtp.1, None));
        let incoming_access = Access { persona: &c.persona, recipient: &imap };
        let outgoing_access = Access { persona: &c.persona, recipient: &smtp };
        let now = Instant::now();
        self.mail_leases.retain(|_, l| l.expires > now);
        let mut r = Reader::new(body);
        if op == courier_op::MAIL_REFRESH_BEGIN {
            let (Ok(i), Ok(o), Ok(direction)) = (r.array::<16>(), r.array::<16>(), r.u8()) else { return malformed() };
            if r.end().is_err() || direction > 1 { return malformed(); }
            let (i, o) = (ItemId(i), ItemId(o));
            if self.mail_leases.values().any(|l| l.incoming == i && l.owner == *c) { return reply(status::FULL); }
            if self.mail_leases.len() >= 128 { return reply(status::FULL); }
            let (im, grant) = tri!(store.release(&i, &incoming_access, None));
            let (sm, outgoing) = tri!(store.release(&o, &outgoing_access, None));
            if im.kind != Kind::OAuthToken || sm.kind != Kind::OAuthToken || im.account != sm.account || !grant.as_ref().starts_with(GRANT) {
                return reply(status::DENIED);
            }
            let reference = link(i);
            if outgoing.as_ref() != reference {
                // Older callback accounts have two identical grant records.
                // Link them before contacting the provider, while both match.
                if outgoing.as_ref() != grant.as_ref() { return reply(status::DENIED); }
                tri!(store.update(&o, &outgoing_access, &reference, None));
            }
            if im.scope != OWNER {
                tri!(store.set_scope(&i, &incoming_access, OWNER));
            }
            let nonce = ItemId::random().0;
            let mut out = tri!(LockedBuf::zeroed(16 + grant.len()).map_err(Error::Io));
            out.as_mut()[..16].copy_from_slice(&nonce);
            out.as_mut()[16..].copy_from_slice(grant.as_ref());
            self.mail_leases.insert(nonce, Lease { owner: c.clone(), incoming: i, account: im.account,
                destination: if direction == 0 { imap } else { smtp }, expires: now + Duration::from_secs(120) });
            return Reply { status: status::OK, body: Body::Secret(out) };
        }
        let Ok(nonce) = r.array::<16>() else { return malformed() };
        if !self.mail_leases.get(&nonce).is_some_and(|l| l.owner == *c) { return reply(status::DENIED); }
        let lease = self.mail_leases.remove(&nonce).unwrap();
        if op == courier_op::MAIL_REFRESH_CANCEL { return if r.end().is_ok() { ok(vec![]) } else { malformed() }; }
        let (Ok(refresh), Ok(access), Ok(lifetime)) = (r.bytes(crate::oauth::MAX_TOKEN), r.bytes(crate::oauth::MAX_TOKEN), r.u64()) else { return malformed() };
        if r.end().is_err() || access.is_empty() || !access.iter().chain(refresh).all(u8::is_ascii_graphic) { return malformed(); }
        if !refresh.is_empty() {
            let (_, old) = tri!(store.release(&lease.incoming, &incoming_access, None));
            let Some(last) = old.as_ref().iter().rposition(|&b| b == b' ') else { return malformed() };
            let mut grant = tri!(LockedBuf::zeroed(last + 1 + refresh.len()).map_err(Error::Io));
            grant.as_mut()[..=last].copy_from_slice(&old.as_ref()[..=last]);
            grant.as_mut()[last + 1..].copy_from_slice(refresh);
            tri!(store.update(&lease.incoming, &incoming_access, grant.as_ref(), None));
        }
        let account = lease.account.as_bytes();
        let mut login = tri!(LockedBuf::zeroed(9 + account.len() + access.len()).map_err(Error::Io));
        let (a, rest) = login.as_mut().split_at_mut(4 + account.len());
        a[..4].copy_from_slice(&(account.len() as u32).to_le_bytes());
        a[4..].copy_from_slice(account);
        rest[..4].copy_from_slice(&(access.len() as u32).to_le_bytes());
        rest[4..4 + access.len()].copy_from_slice(access);
        rest[4 + access.len()] = courier_op::LOGIN_ACCESS;
        let entry = crate::handles::Entry { user: c.user.clone(), persona: c.persona.clone(), recipient: lease.destination,
            source: crate::handles::Source::Pending(login), expires: now + Duration::from_secs(lifetime.min(60)),
            generated: false, hold: crate::handles::HOLD_MAIL_LOGIN, printer: None };
        self.handles.issue(entry).map_or_else(|| reply(status::FULL), |h| ok(h.to_vec()))
    }
}

impl Lease {
    pub(super) fn uses(&self, user: &str, id: ItemId) -> bool { self.owner.user == user && self.incoming == id }
    pub(super) fn belongs_to(&self, user: &str) -> bool { self.owner.user == user }
}
