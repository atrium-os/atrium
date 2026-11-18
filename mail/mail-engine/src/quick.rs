//! Quick filters (mail.md §11.7): chips above the list, combinable, run
//! on the local index so they are instant and work offline.

use std::collections::{HashMap, HashSet};

use mail_schema::PartKind;
use mail_store::{flag, AccountId, MsgId, ThreadId};

use crate::Engine;

pub const UNREAD: u16 = 1;
pub const FLAGGED: u16 = 2;
/// The person is in To (not only Cc), and it isn't list mail.
pub const TO_ME: u16 = 4;
/// From a person: not a list, not automated (or a VIP).
pub const FROM_PEOPLE: u16 = 8;
pub const VIPS: u16 = 16;
pub const ATTACHMENT: u16 = 32;
/// The conversation's last message is to the person and not from them.
pub const NEEDS_REPLY: u16 = 64;
/// Set aside with Later, and come back.
pub const BACK: u16 = 128;

/// The chips, in order.
pub const CHIPS: [(u16, &str); 8] = [
    (UNREAD, "Unread"),
    (FLAGGED, "Flagged"),
    (TO_ME, "To me"),
    (FROM_PEOPLE, "From people"),
    (VIPS, "VIPs"),
    (ATTACHMENT, "Has attachment"),
    (NEEDS_REPLY, "Needs reply"),
    (BACK, "Snoozed back"),
];

/// The search operators that ask for `bits` (§11.2, §11.7), so a set of
/// quick filters can be kept as a saved search.
pub fn query_of(bits: u16) -> String {
    const OPS: [(u16, &str); 8] = [
        (UNREAD, "is:unread"),
        (FLAGGED, "is:flagged"),
        (TO_ME, "is:to-me"),
        (FROM_PEOPLE, "is:from-people"),
        (VIPS, "is:vip"),
        (ATTACHMENT, "has:attachment"),
        (NEEDS_REPLY, "is:needs-reply"),
        (BACK, "is:back"),
    ];
    OPS.iter().filter(|(b, _)| bits & b != 0).map(|(_, o)| *o).collect::<Vec<_>>().join(" ")
}

/// Rows a filtered list looks through at most.
pub const SCAN_MAX: usize = 5000;

/// What the filters need, read once per filtering.
pub struct Quick<'a> {
    e: &'a Engine,
    filters: u16,
    prefs: crate::prefs::Prefs,
    mine: HashMap<AccountId, HashSet<String>>,
    back: HashSet<ThreadId>,
}

impl Engine {
    /// Ready to filter by `filters` (bits above).
    pub fn quick(&self, filters: u16) -> Quick<'_> {
        let back = if filters & BACK != 0 { self.later_items().unwrap_or_default().into_iter().filter(|(_, l)| l.back).map(|(_, l)| l.thread).collect() } else { HashSet::new() };
        Quick { e: self, filters, prefs: self.prefs(), mine: HashMap::new(), back }
    }
}

impl Quick<'_> {
    fn mine(&mut self, account: AccountId) -> &HashSet<String> {
        let e = self.e;
        self.mine.entry(account).or_insert_with(|| {
            let mut s: HashSet<String> = e.identities(account).unwrap_or_default().into_iter().map(|i| i.addr.to_ascii_lowercase()).collect();
            // The account's own name, when it is an address.
            if let Some((_, a)) = e.store.accounts().unwrap_or_default().into_iter().find(|(id, a)| *id == account && a.name.contains('@')) {
                s.insert(a.name.to_ascii_lowercase());
            }
            s
        })
    }

    /// Whether a row passes every filter: the row's message `msg` (a
    /// conversation's newest), `thread` for a conversation row, and the
    /// row's own unread and flagged state.
    pub fn passes(&mut self, msg: MsgId, thread: Option<ThreadId>, unread: bool, flagged: bool) -> bool {
        let f = self.filters;
        if f & UNREAD != 0 && !unread || f & FLAGGED != 0 && !flagged {
            return false;
        }
        if f & !(UNREAD | FLAGGED) == 0 {
            return true;
        }
        let Ok(m) = self.e.store.message(msg) else { return false };
        let s = &m.summary;
        let list = crate::senders::is_list(s);
        let vip = s.from.iter().any(|a| self.prefs.is_vip(&a.addr));
        if f & VIPS != 0 && !vip || f & FROM_PEOPLE != 0 && !(vip || !s.automated && !list) {
            return false;
        }
        if f & BACK != 0 && !self.back.contains(&m.thread) {
            return false;
        }
        let mine = self.mine(m.account).clone();
        let to_me = |s: &mail_schema::Summary| !crate::senders::is_list(s) && s.to.iter().any(|a| mine.contains(&a.addr.to_ascii_lowercase()));
        if f & TO_ME != 0 && !to_me(s) {
            return false;
        }
        let in_thread = || self.e.store.thread(m.thread).unwrap_or_default();
        if f & ATTACHMENT != 0 {
            let has = |s: &mail_schema::Summary| s.parts.iter().any(|p| p.kind == PartKind::Attachment);
            let any = match thread {
                Some(_) => in_thread().iter().any(|x| has(&x.message.summary)),
                None => has(s),
            };
            if !any {
                return false;
            }
        }
        if f & NEEDS_REPLY != 0 {
            let all = in_thread();
            let Some(last) = all.iter().max_by_key(|x| (x.message.date, x.msg)) else { return false };
            let ls = &last.message.summary;
            let from_me = ls.from.iter().any(|a| mine.contains(&a.addr.to_ascii_lowercase()));
            if from_me || ls.automated || !to_me(ls) || last.flags & flag::ANSWERED != 0 {
                return false;
            }
        }
        true
    }
}
