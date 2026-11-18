//! Senders and lists (mail.md §11.3): one-press unsubscribe, blocking a
//! list, the newsletter lane and the screener. All local rules over what a
//! parse jail checked; nothing is fetched to decide, nothing bounces.

use std::time::{Duration, Instant};

use mail_schema::Summary;
use mail_store::senders::{contact, lane, list, why};
use mail_store::{AccountId, FolderId, MsgId, OpKind, Role};
use nuntius::{Client, Event, Limits, Open};

use crate::{now, Engine, Error};

/// How a list says to unsubscribe, best first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unsubscribe {
    /// RFC 8058: a POST to this `https:` URL does it, no page to visit.
    OneClick(String),
    /// A `mailto:` link: a prefilled message the person sends.
    Mail(String),
    /// Only a web page: opened in the Navigator (through the link bar).
    Web(String),
}

/// A message's list, as the reader shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct ListInfo {
    pub account: AccountId,
    /// The `List-Id` (its `<id>` part), else the sender's address.
    pub key: String,
    /// "The Weekly", from the `List-Id` phrase or the sender's name.
    pub name: String,
    pub how: Option<Unsubscribe>,
    /// [`list`] state.
    pub state: u8,
}

/// What came of a one-press unsubscribe.
pub const UNSUBSCRIBE_TIME: Duration = Duration::from_secs(30);

/// The key a list is known by: the `<...>` of `List-Id`, lower case.
pub fn list_key(s: &Summary) -> Option<String> {
    let id = s.list_id.as_deref()?;
    let inner = match (id.rfind('<'), id.rfind('>')) {
        (Some(a), Some(b)) if b > a + 1 => &id[a + 1..b],
        _ => id,
    };
    let k = inner.trim().to_ascii_lowercase();
    (!k.is_empty()).then_some(k)
}

/// Whether a message is list or bulk mail (a newsletter, if from someone
/// the person hasn't written to).
pub fn is_list(s: &Summary) -> bool {
    s.list_id.is_some() || !s.list_unsubscribe.is_empty()
}

fn list_name(s: &Summary) -> String {
    if let Some(id) = &s.list_id {
        let phrase = id.split('<').next().unwrap_or("").trim().trim_matches('"').trim();
        if !phrase.is_empty() {
            return phrase.to_string();
        }
    }
    s.from.first().map(|a| if a.name.is_empty() { a.addr.clone() } else { a.name.clone() }).unwrap_or_default()
}

/// The best way to unsubscribe from what `List-Unsubscribe` offers.
pub fn unsubscribe(s: &Summary) -> Option<Unsubscribe> {
    let https = s.list_unsubscribe.iter().find(|u| u.get(..8).is_some_and(|p| p.eq_ignore_ascii_case("https://")));
    let mailto = s.list_unsubscribe.iter().find(|u| u.get(..7).is_some_and(|p| p.eq_ignore_ascii_case("mailto:")));
    match (https, mailto, s.one_click_unsubscribe) {
        (Some(u), _, true) => Some(Unsubscribe::OneClick(u.clone())),
        (_, Some(m), _) => Some(Unsubscribe::Mail(m.clone())),
        (Some(u), None, false) => Some(Unsubscribe::Web(u.clone())),
        _ => None,
    }
}

/// The RFC 8058 POST, through Nuntius (no cookies, no `Referer`). Done when
/// the server answers 2xx.
pub fn post(client: &mut Client, url: &str) -> Result<(), String> {
    let mut o = Open::post(1, url, "application/x-www-form-urlencoded", b"List-Unsubscribe=One-Click".to_vec());
    o.limits = Limits { max_bytes: 64 << 10, connect_ms: 10_000, idle_ms: 15_000, total_ms: 30_000 };
    client.open(o).map_err(|e| format!("{e:?}"))?;
    let deadline = Instant::now() + UNSUBSCRIBE_TIME;
    let mut status = None;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            let _ = client.cancel(1);
            return Err("the list's server didn't answer".into());
        }
        match client.recv_timeout(left.min(Duration::from_secs(1))) {
            Ok(Some(Event::Head { status: s, .. })) => status = Some(s),
            Ok(Some(Event::Data { bytes, .. })) => {
                let _ = client.budget(bytes.len() as u64);
            }
            Ok(Some(Event::Done { .. })) => {
                return match status {
                    Some(s) if (200..300).contains(&s) => Ok(()),
                    Some(s) => Err(format!("the list's server said {s}")),
                    None => Err("the list's server said nothing".into()),
                };
            }
            Ok(Some(Event::Error { code, .. })) => return Err(format!("{code:?}")),
            Ok(_) => {}
            Err(e) => return Err(format!("{e:?}")),
        }
    }
}

/// The screener's setting: when it was turned on (mail dated before is
/// let through).
const SCREENER: &str = "screener";

impl Engine {
    /// The list `msg` came from, if it is list mail.
    pub fn list_of(&self, msg: MsgId) -> Result<Option<ListInfo>, Error> {
        let m = self.store.message(msg).map_err(|_| Error::NotFound)?;
        if !is_list(&m.summary) {
            return Ok(None);
        }
        let key = match list_key(&m.summary) {
            Some(k) => k,
            None => match m.summary.from.first() {
                Some(a) => a.addr.to_ascii_lowercase(),
                None => return Ok(None),
            },
        };
        let state = self.store.list_state(m.account, &key)?;
        Ok(Some(ListInfo { account: m.account, name: list_name(&m.summary), how: unsubscribe(&m.summary), key, state }))
    }

    /// Remember a list as unsubscribed, blocked, or neither.
    pub fn set_list(&self, account: AccountId, key: &str, state: u8) -> Result<(), Error> {
        Ok(self.store.write(|w| w.set_list_state(account, key, state))?)
    }

    pub fn screener(&self, account: AccountId) -> Result<bool, Error> {
        Ok(self.store.setting(account, SCREENER)?.is_some())
    }

    /// Turn the screener on (from now: mail already here is let through)
    /// or off (what it held goes back to the inbox).
    pub fn set_screener(&self, account: AccountId, on: bool) -> Result<(), Error> {
        if !on {
            let held = self.store.held(account)?;
            self.store.write(|w| {
                for (msg, .., _) in held.iter().filter(|h| h.3 == why::SCREENER) {
                    w.release(*msg)?;
                }
                w.set_setting(account, SCREENER, None)
            })?;
            return Ok(());
        }
        if self.screener(account)? {
            return Ok(());
        }
        Ok(self.store.write(|w| w.set_setting(account, SCREENER, Some(now().max(0) as u64)))?)
    }

    /// Mail from a sender let through: what is held from them goes to the
    /// inbox, and later mail goes there too.
    pub fn accept_sender(&self, account: AccountId, addr: &str) -> Result<(), Error> {
        let held = self.held_from(account, addr)?;
        Ok(self.store.write(|w| {
            w.mark_contact(account, addr, contact::ACCEPTED, contact::REFUSED)?;
            for msg in &held {
                w.release(*msg)?;
            }
            Ok(())
        })?)
    }

    /// Refused: the sender's mail stays out of the inbox, under *Screened
    /// out*. Nothing is sent to them.
    pub fn refuse_sender(&self, account: AccountId, addr: &str) -> Result<(), Error> {
        Ok(self.store.write(|w| w.mark_contact(account, addr, contact::REFUSED, contact::ACCEPTED))?)
    }

    fn held_from(&self, account: AccountId, addr: &str) -> Result<Vec<MsgId>, Error> {
        let mut out = Vec::new();
        for (msg, .., w) in self.store.held(account)? {
            if w != why::SCREENER {
                continue;
            }
            if self.store.message(msg).ok().and_then(|m| m.summary.from.first().map(|a| a.addr.eq_ignore_ascii_case(addr))).unwrap_or(false) {
                out.push(msg);
            }
        }
        Ok(out)
    }

    /// The screener's two views: (*New senders*, *Screened out*), newest
    /// first.
    pub fn screened(&self, account: AccountId) -> Result<(Vec<MsgId>, Vec<MsgId>), Error> {
        let (mut new, mut out) = (Vec::new(), Vec::new());
        for (msg, .., w) in self.store.held(account)? {
            if w != why::SCREENER {
                continue;
            }
            let Ok(m) = self.store.message(msg) else { continue };
            let from = m.summary.from.first().map(|a| a.addr.clone()).unwrap_or_default();
            if self.store.contact(account, &from)? & contact::REFUSED != 0 {
                out.push(msg);
            } else {
                new.push(msg);
            }
        }
        Ok((new, out))
    }

    /// The newsletter lane (§11.3): list mail from senders the person
    /// hasn't written to, every folder, newest first.
    pub fn newsletters(&self, account: AccountId, offset: usize, limit: usize) -> Result<Vec<MsgId>, Error> {
        let mut out = Vec::new();
        let mut at = offset;
        while out.len() < limit {
            let page = self.store.lane(account, lane::NEWSLETTERS, at, 200)?;
            if page.is_empty() {
                break;
            }
            at += page.len();
            for msg in page {
                let Ok(m) = self.store.message(msg) else { continue };
                if m.locations.is_empty() {
                    continue;
                }
                let from = m.summary.from.first().map(|a| a.addr.clone()).unwrap_or_default();
                if self.store.contact(account, &from)? & contact::WROTE == 0 {
                    out.push(msg);
                    if out.len() == limit {
                        break;
                    }
                }
            }
        }
        Ok(out)
    }

    /// New mail stored in `folder`: the rules of §11.3 (who the person
    /// writes to, the newsletter lane, blocked lists, the screener).
    pub(crate) fn arrived(&self, account: AccountId, folder: FolderId, new: &[(u32, MsgId)]) -> Result<(), Error> {
        if new.is_empty() {
            return Ok(());
        }
        let f = self.store.folder(folder)?;
        for &(uid, msg) in new {
            self.later_arrived(account, folder, f.role, uid, msg)?;
        }
        let screener = self.store.setting(account, SCREENER)?;
        let trash = self.store.folders(account)?.into_iter().find(|(_, x)| x.role == Role::Trash && !x.hidden).map(|(id, _)| id);
        let mut block = Vec::new();
        let mut hold = Vec::new();
        let mut wrote = Vec::new();
        let mut people = Vec::new();
        let mut lane_in = Vec::new();
        let mut received: Vec<String> = Vec::new();
        let mut private_in = Vec::new();
        for &(uid, msg) in new {
            let Ok(m) = self.store.message(msg) else { continue };
            let s = &m.summary;
            if f.role == Role::Sent {
                wrote.extend(s.to.iter().chain(&s.cc).chain(&s.bcc).map(|a| a.addr.clone()));
                people.push((s.message_id.clone(), s.to.iter().chain(&s.cc).chain(&s.bcc).map(|a| (a.name.clone(), a.addr.clone())).collect::<Vec<_>>(), m.date));
                continue;
            }
            if matches!(f.role, Role::Drafts | Role::Trash | Role::Junk) {
                continue;
            }
            if is_list(s) {
                lane_in.push((msg, m.date));
            }
            if f.role != Role::Inbox {
                continue;
            }
            private_in.push((uid, s.clone(), m.date));
            if let Some(k) = list_key(s).or_else(|| is_list(s).then(|| s.from.first().map(|a| a.addr.to_ascii_lowercase())).flatten()) {
                if self.store.list_state(account, &k)? == list::BLOCKED {
                    block.push(uid);
                    continue;
                }
            }
            let Some(from) = s.from.first() else { continue };
            let known = self.store.contact(account, &from.addr)? & (contact::WROTE | contact::ACCEPTED | contact::KNOWN) != 0
                || received.iter().any(|a: &String| a.eq_ignore_ascii_case(&from.addr));
            // Held: someone new, after the screener was turned on.
            if !known && screener.is_some_and(|since| m.date >= since as i64) {
                hold.push(uid);
            } else if !known {
                received.push(from.addr.clone());
            }
        }
        // Mail to a private address turned off goes to Trash (§13.3).
        for uid in self.private_arrived(account, &private_in)? {
            if !block.contains(&uid) {
                block.push(uid);
            }
            hold.retain(|u| *u != uid);
        }
        self.store.write(|w| {
            for a in &wrote {
                w.mark_contact(account, a, contact::WROTE, 0)?;
            }
            for (id, to, at) in &people {
                let to: Vec<(&str, &str)> = to.iter().map(|(n, a)| (n.as_str(), a.as_str())).collect();
                w.wrote_to(id.as_deref(), &to, *at)?;
            }
            for a in &received {
                w.mark_contact(account, a, contact::KNOWN, 0)?;
            }
            for (msg, date) in &lane_in {
                w.set_lane(account, lane::NEWSLETTERS, *msg, *date, true)?;
            }
            for uid in &hold {
                w.hold(folder, *uid, why::SCREENER)?;
            }
            if let Some(to) = trash {
                for uid in &block {
                    match w.queue(folder, *uid, OpKind::Move { to }, now()) {
                        Ok(_) | Err(mail_store::Error::NotFound) => {}
                        Err(e) => return Err(e),
                    }
                }
            }
            Ok(())
        })?;
        Ok(())
    }
}
