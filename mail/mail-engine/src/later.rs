//! *Later* (mail.md §11.4): one action for flag, snooze and pin. A
//! conversation set aside leaves the inbox and comes back at its time;
//! a reply closes a *Reply* item, someone's answer closes a *Waiting* one,
//! and new mail to the person brings it back at once. Times are UTC until
//! Atrium's time zones reach apps.

use mail_store::later::{kind, Later};
use mail_store::senders::why;
use mail_store::{flag, AccountId, MsgId, OpKind, Role, ThreadId};

use crate::{now, Engine, Error};

/// When an item comes back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    /// 18:00 today (tomorrow's when that has passed).
    Evening,
    /// 09:00 tomorrow.
    Tomorrow,
    /// Saturday 09:00.
    Weekend,
    /// Monday 09:00.
    NextWeek,
    /// 30 days out, when it asks again.
    Someday,
}

impl When {
    pub const ALL: [When; 5] = [When::Evening, When::Tomorrow, When::Weekend, When::NextWeek, When::Someday];

    pub fn label(self) -> &'static str {
        match self {
            When::Evening => "This evening",
            When::Tomorrow => "Tomorrow morning",
            When::Weekend => "This weekend",
            When::NextWeek => "Next week",
            When::Someday => "Someday",
        }
    }

    /// The time it names, from `now`.
    pub fn at(self, now: i64) -> i64 {
        let day = now.div_euclid(86_400) * 86_400;
        // 1970-01-01 was a Thursday: 0 = Monday.
        let weekday = (now.div_euclid(86_400) + 3).rem_euclid(7);
        match self {
            When::Evening if now < day + 18 * 3600 => day + 18 * 3600,
            When::Evening => day + 86_400 + 18 * 3600,
            When::Tomorrow => day + 86_400 + 9 * 3600,
            When::Weekend => {
                let ahead = (5 - weekday).rem_euclid(7);
                let ahead = if ahead == 0 && now >= day + 9 * 3600 { 7 } else { ahead };
                day + ahead * 86_400 + 9 * 3600
            }
            When::NextWeek => day + (7 - weekday) * 86_400 + 9 * 3600,
            When::Someday => day + 30 * 86_400 + 9 * 3600,
        }
    }
}

pub fn kind_label(k: u8) -> &'static str {
    match k {
        kind::REPLY => "Reply",
        kind::READ => "Read",
        kind::DO => "Do",
        _ => "Waiting",
    }
}

impl Engine {
    /// What an item on `msg` is for unless the person says: *Read* for
    /// lists and automated mail, else *Reply*.
    pub fn later_kind(&self, msg: MsgId) -> u8 {
        match self.store.message(msg) {
            Ok(m) if m.summary.automated || crate::senders::is_list(&m.summary) => kind::READ,
            _ => kind::REPLY,
        }
    }

    /// The open item on `msg`'s conversation.
    pub fn later_of(&self, msg: MsgId) -> Result<Option<(u64, Later)>, Error> {
        let m = self.store.message(msg).map_err(|_| Error::NotFound)?;
        Ok(self.store.later_of(m.thread)?)
    }

    /// Set `msg`'s conversation aside until `due` (again, if it was: that
    /// counts as pushed back). It leaves the inbox; the server sees
    /// `\Flagged` on it.
    pub fn set_later(&self, msg: MsgId, k: u8, due: i64, note: &str) -> Result<u64, Error> {
        let m = self.store.message(msg).map_err(|_| Error::NotFound)?;
        let t = m.thread;
        let item = match self.store.later_of(t)? {
            Some((id, mut l)) => {
                l.pushed += 1;
                l.due = due;
                l.kind = k;
                if !note.is_empty() {
                    l.note = note.into();
                }
                l.back = false;
                (Some(id), l)
            }
            None => (None, Later { account: m.account, thread: t, msg, kind: k, due, note: note.into(), set_at: now(), pushed: 0, back: false }),
        };
        let inbox = self.inboxes(m.account)?;
        let msgs = self.store.thread(t).unwrap_or_default();
        let flag_at = msgs.last().and_then(|x| x.message.locations.first().copied());
        let id = self.store.write(|w| {
            if let Some((f, u)) = flag_at {
                if self.store.at(f, u)?.is_some_and(|(_, fl)| fl & flag::FLAGGED == 0) {
                    match w.queue(f, u, OpKind::Flags { add: flag::FLAGGED, remove: 0 }, now()) {
                        Ok(_) | Err(mail_store::Error::NotFound) => {}
                        Err(e) => return Err(e),
                    }
                }
            }
            for x in &msgs {
                for &(f, u) in x.message.locations.iter().filter(|(f, _)| inbox.contains(f)) {
                    w.hold(f, u, why::LATER)?;
                }
            }
            w.put_later(item.0, &item.1)
        })?;
        Ok(id)
    }

    /// *Done* (or *Drop*): the item closes, its conversation is back where
    /// it was, and the flag comes off.
    pub fn later_done(&self, id: u64) -> Result<(), Error> {
        let Some(l) = self.store.write(|w| w.close_later(id))? else { return Ok(()) };
        self.release_later(l.account, l.thread)?;
        let msgs = self.store.thread(l.thread).unwrap_or_default();
        self.store.write(|w| {
            for x in &msgs {
                for &(f, u) in &x.message.locations {
                    if self.store.at(f, u)?.is_some_and(|(_, fl)| fl & flag::FLAGGED != 0) {
                        match w.queue(f, u, OpKind::Flags { add: 0, remove: flag::FLAGGED }, now()) {
                            Ok(_) | Err(mail_store::Error::NotFound) => {}
                            Err(e) => return Err(e),
                        }
                    }
                }
            }
            Ok(())
        })?;
        Ok(())
    }

    /// Items whose time has come go back to the inbox; how many.
    pub fn later_due(&self, at: i64) -> Result<usize, Error> {
        let mut n = 0;
        for (a, _) in self.store.accounts()? {
            for (id, mut l) in self.store.later_items(a)? {
                if l.back || l.due > at {
                    continue;
                }
                self.release_later(a, l.thread)?;
                l.back = true;
                self.store.write(|w| w.put_later(Some(id), &l))?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// Every open item, soonest first.
    pub fn later_items(&self) -> Result<Vec<(u64, Later)>, Error> {
        let mut out = Vec::new();
        for (a, _) in self.store.accounts()? {
            out.extend(self.store.later_items(a)?);
        }
        out.sort_by_key(|(id, l)| (l.due, *id));
        Ok(out)
    }

    fn inboxes(&self, account: AccountId) -> Result<Vec<mail_store::FolderId>, Error> {
        Ok(self.store.folders(account)?.into_iter().filter(|(_, f)| f.role == Role::Inbox).map(|(id, _)| id).collect())
    }

    fn release_later(&self, account: AccountId, t: ThreadId) -> Result<(), Error> {
        let held = self.store.held(account)?;
        let mine: Vec<MsgId> = held.iter().filter(|h| h.3 == why::LATER).map(|h| h.0).filter(|m| self.store.message(*m).is_ok_and(|x| x.thread == t)).collect();
        self.store.write(|w| {
            for m in &mine {
                w.release(*m)?;
            }
            Ok(())
        })?;
        Ok(())
    }

    /// New mail in a set-aside conversation (§11.4): a reply the person
    /// sent closes a *Reply* item; an answer from someone else closes a
    /// *Waiting* one; mail to the person from a person brings it back at
    /// once; anything else waits for the date with it.
    pub(crate) fn later_arrived(&self, account: AccountId, folder: mail_store::FolderId, role: Role, uid: u32, msg: MsgId) -> Result<(), Error> {
        let Ok(m) = self.store.message(msg) else { return Ok(()) };
        // The sent copy of a message sent with "remind me": a Waiting item.
        if role == Role::Sent {
            if let Some(mid) = m.summary.message_id.as_deref() {
                if let Some((_, due)) = self.store.write(|w| w.take_follow_up(mid))? {
                    if self.store.later_of(m.thread)?.is_none() {
                        self.set_later(msg, kind::WAITING, due, "")?;
                    }
                    return Ok(());
                }
            }
        }
        let Some((id, mut l)) = self.store.later_of(m.thread)? else { return Ok(()) };
        let me: Vec<String> = self.identities(account)?.into_iter().map(|i| i.addr.to_ascii_lowercase()).collect();
        let from_me = m.summary.from.iter().any(|a| me.contains(&a.addr.to_ascii_lowercase()));
        if role == Role::Sent || from_me {
            if l.kind == kind::REPLY {
                return self.later_done(id);
            }
            return Ok(());
        }
        if role != Role::Inbox {
            return Ok(());
        }
        if l.kind == kind::WAITING {
            return self.later_done(id);
        }
        if l.back {
            return Ok(());
        }
        let to_me = m.summary.to.iter().chain(&m.summary.cc).any(|a| me.contains(&a.addr.to_ascii_lowercase()));
        if to_me && !m.summary.automated && !crate::senders::is_list(&m.summary) {
            self.release_later(account, l.thread)?;
            l.back = true;
            self.store.write(|w| w.put_later(Some(id), &l))?;
        } else {
            self.store.write(|w| w.hold(folder, uid, why::LATER))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times() {
        let mon = 1_795_425_660; // Mon 2026-11-23 09:21 UTC
        let day = 1_795_392_000;
        assert_eq!(When::Evening.at(mon), day + 18 * 3600);
        assert_eq!(When::Evening.at(day + 19 * 3600), day + 86_400 + 18 * 3600);
        assert_eq!(When::Tomorrow.at(mon), day + 86_400 + 9 * 3600);
        assert_eq!(When::Weekend.at(mon), day + 5 * 86_400 + 9 * 3600);
        assert_eq!(When::NextWeek.at(mon), day + 7 * 86_400 + 9 * 3600);
        assert_eq!(When::Someday.at(mon), day + 30 * 86_400 + 9 * 3600);
    }
}
