//! What Mail tells Praeco (mail.md §12, §12.1): quiet by default, and no
//! storm of pings after a gap. The rules are here, apart from the posting,
//! so they can be checked without a notification service:
//!
//! - only new mail in an inbox, unread, not held (screener, Later) counts;
//! - one by one only for mail from a person (not a list or automated) or in
//!   a conversation the person wrote in, and only if it arrived in the last
//!   [`FRESH_SECS`]; anything older is one silent "while you were away"
//!   summary, replacing the last one rather than stacking;
//! - more than [`BURST`] within [`BURST_SECS`] is one grouped post;
//! - a first sync (an inbox that was empty) never notifies;
//! - a message read elsewhere since is withdrawn;
//! - Later: once a day after [`REVIEW_HOUR`], "3 due today, 2 overdue".
//!
//! - Settings ([`crate::prefs`]): notifications off (only withdrawals),
//!   VIPs always one by one, and quiet hours: what arrives inside them is
//!   held back and said in one silent summary when they end (VIPs only get
//!   through if the person lets them).
//!
//! Not yet: quiet hours per persona (one Mail instance is one persona, so
//! the instance's quiet hours are the persona's), sounds per account, the
//! digest of everything else.

use std::collections::{HashMap, VecDeque};

use mail_store::{flag, AccountId, MsgId, Role};

use crate::Engine;

pub const FRESH_SECS: i64 = 600;
pub const BURST: usize = 2;
pub const BURST_SECS: i64 = 30;
/// The daily review's hour, local time.
pub const REVIEW_HOUR: i64 = 9;
const BODY_MAX: usize = 2048;
const SCAN_MAX: usize = 5000;

/// What to post (urgency, title, body), or take back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Post {
    /// One message: its whole subject, then who sent it and its new text.
    One { msg: MsgId, title: String, body: String },
    /// Several at once ("5 new messages"), naming the latest.
    Group { title: String, body: String },
    /// "While you were away: 23 new, 4 from people (Priya, Arun…)": silent,
    /// replaces the last summary.
    Summary { title: String, body: String },
    /// The Later review.
    Review { title: String, body: String },
    /// Take back the post made for this message (read elsewhere).
    Withdraw { msg: MsgId },
}

/// One arrival as the rules see it.
#[derive(Debug, Clone)]
struct Arrival {
    msg: MsgId,
    person: bool,
    from: String,
    subject: String,
    text: String,
}

/// The notifier's memory between looks.
#[derive(Debug, Default)]
pub struct Notifier {
    /// The newest message already considered.
    last: MsgId,
    started: bool,
    /// Inboxes that had mail (a first sync is the others).
    had_mail: HashMap<AccountId, bool>,
    /// Posts made one by one, by message, to withdraw when read.
    posted: Vec<MsgId>,
    /// When recent posts were made (for bursts).
    recent: VecDeque<i64>,
    /// The day (days since 1970) the review was posted.
    reviewed: Option<i64>,
    /// Arrivals held back during quiet hours, said when they end.
    quiet: Vec<Arrival>,
}

impl Notifier {
    pub fn new() -> Notifier {
        Notifier::default()
    }

    /// What to post now. The first look only notes where things are:
    /// what was there before Mail started never notifies.
    pub fn check(&mut self, e: &Engine, now: i64) -> Vec<Post> {
        self.check_local(e, now, 0)
    }

    /// [`Notifier::check`] with the local time's offset from UTC (seconds),
    /// for quiet hours and the review's day and hour.
    pub fn check_local(&mut self, e: &Engine, now: i64, offset: i64) -> Vec<Post> {
        let prefs = e.prefs();
        let minute = ((now + offset).rem_euclid(86_400) / 60) as u16;
        let quiet = prefs.is_quiet(minute);
        let mut out = self.look(e, now, offset, &prefs, quiet);
        if !prefs.notify {
            // Off: only take back what was said before.
            self.quiet.clear();
            out.retain(|p| matches!(p, Post::Withdraw { .. }));
            return out;
        }
        if !quiet && !self.quiet.is_empty() {
            let held = std::mem::take(&mut self.quiet);
            let people: Vec<&Arrival> = held.iter().filter(|a| a.person).collect();
            let mut body = format!("{} new", held.len());
            if !people.is_empty() {
                body += &format!(", {} from people ({})", people.len(), first_names(people.iter().map(|a| a.from.as_str())));
            }
            out.push(Post::Summary { title: "During quiet hours".into(), body });
        }
        out
    }

    fn look(&mut self, e: &Engine, now: i64, offset: i64, prefs: &crate::prefs::Prefs, quiet: bool) -> Vec<Post> {
        let mut out = Vec::new();
        let accounts: Vec<AccountId> = e.store.accounts().map(|a| a.into_iter().map(|x| x.0).collect()).unwrap_or_default();
        if !self.started {
            self.started = true;
            self.last = e.store.last_message().unwrap_or(0);
            for a in &accounts {
                self.had_mail.insert(*a, self.inbox_has_mail(e, *a));
            }
            self.review(e, now, offset, &mut out);
            return out;
        }
        // Read elsewhere since: take it back.
        self.posted.retain(|m| {
            let seen = e.store.message(*m).map(|x| x.locations.iter().any(|(f, u)| e.store.at(*f, *u).ok().flatten().is_some_and(|(_, fl)| fl & flag::SEEN != 0))).unwrap_or(true);
            if seen {
                out.push(Post::Withdraw { msg: *m });
            }
            !seen
        });
        let new = e.store.messages_after(self.last, SCAN_MAX).unwrap_or_default();
        if let Some((id, _)) = new.last() {
            self.last = *id;
        }
        let mut fresh = Vec::new();
        let mut stale = Vec::new();
        let mut bulk: Vec<AccountId> = Vec::new();
        for a in &accounts {
            let had = *self.had_mail.get(a).unwrap_or(&false);
            if !had {
                bulk.push(*a);
                self.had_mail.insert(*a, self.inbox_has_mail(e, *a));
            }
        }
        let me: HashMap<AccountId, Vec<String>> = accounts.iter().map(|a| (*a, e.identities(*a).unwrap_or_default().into_iter().map(|i| i.addr.to_ascii_lowercase()).collect())).collect();
        for (id, m) in new {
            if bulk.contains(&m.account) {
                continue;
            }
            let inbox = m.locations.iter().any(|(f, _)| e.store.folder(*f).is_ok_and(|x| x.role == Role::Inbox));
            if !inbox || !m.held.is_empty() {
                continue;
            }
            let seen = m.locations.iter().any(|(f, u)| e.store.at(*f, *u).ok().flatten().is_some_and(|(_, fl)| fl & flag::SEEN != 0));
            if seen {
                continue;
            }
            let s = &m.summary;
            let mine = me.get(&m.account).cloned().unwrap_or_default();
            if s.from.iter().any(|a| mine.contains(&a.addr.to_ascii_lowercase())) {
                continue;
            }
            let wrote_in = e.store.thread(m.thread).unwrap_or_default().iter().any(|x| x.msg != id && x.message.summary.from.iter().any(|a| mine.contains(&a.addr.to_ascii_lowercase())));
            let vip = s.from.iter().any(|a| prefs.is_vip(&a.addr));
            let person = vip || (!s.automated && !crate::senders::is_list(s));
            let from = s.from.first().map(|a| if a.name.is_empty() { a.addr.clone() } else { a.name.clone() }).unwrap_or_default();
            let a = Arrival { msg: id, person: person || wrote_in, from, subject: s.subject.clone(), text: new_text(s.text.as_deref().unwrap_or(&s.preview)) };
            if quiet && !(vip && prefs.vips_through) {
                if self.quiet.len() < SCAN_MAX {
                    self.quiet.push(a);
                }
                continue;
            }
            if now - m.date <= FRESH_SECS && m.date <= now + FRESH_SECS {
                fresh.push(a);
            } else {
                stale.push(a);
            }
        }
        // Lists and automated mail are counted, not announced.
        let pings: Vec<&Arrival> = fresh.iter().filter(|a| a.person).collect();
        while self.recent.front().is_some_and(|t| now - *t > BURST_SECS) {
            self.recent.pop_front();
        }
        if !pings.is_empty() && self.recent.len() + pings.len() > BURST {
            let names = first_names(pings.iter().map(|a| a.from.as_str()));
            let n = self.recent.len() + pings.len();
            out.push(Post::Group { title: format!("{n} new messages"), body: format!("From {names}") });
            self.recent.push_back(now);
        } else {
            for a in pings {
                let mut body = format!("From {}", a.from);
                if !a.text.is_empty() {
                    body += "\n";
                    body += &a.text;
                }
                out.push(Post::One { msg: a.msg, title: if a.subject.trim().is_empty() { "(no subject)".into() } else { a.subject.clone() }, body });
                self.posted.push(a.msg);
                self.recent.push_back(now);
            }
        }
        if !stale.is_empty() {
            let people: Vec<&Arrival> = stale.iter().filter(|a| a.person).collect();
            let mut body = format!("{} new", stale.len());
            if !people.is_empty() {
                body += &format!(", {} from people ({})", people.len(), first_names(people.iter().map(|a| a.from.as_str())));
            }
            out.push(Post::Summary { title: "While you were away".into(), body });
        }
        self.review(e, now, offset, &mut out);
        out
    }

    fn inbox_has_mail(&self, e: &Engine, a: AccountId) -> bool {
        e.store.folders(a).unwrap_or_default().iter().any(|(id, f)| f.role == Role::Inbox && e.store.count(*id).unwrap_or(0) > 0)
    }

    /// Once a day after the review hour: what Later has due. Days and the
    /// hour are local (`offset` seconds east of UTC).
    fn review(&mut self, e: &Engine, now: i64, offset: i64, out: &mut Vec<Post>) {
        let now = now + offset;
        let day = now.div_euclid(86_400);
        if self.reviewed == Some(day) || now - day * 86_400 < REVIEW_HOUR * 3600 {
            return;
        }
        self.reviewed = Some(day);
        let items: Vec<_> = e.later_items().unwrap_or_default().into_iter().map(|(id, mut l)| {
            l.due += offset;
            (id, l)
        }).collect();
        let end = (day + 1) * 86_400;
        let overdue = items.iter().filter(|(_, l)| l.due < day * 86_400 + REVIEW_HOUR * 3600 && !l.back).count() + items.iter().filter(|(_, l)| l.back).count();
        let today = items.iter().filter(|(_, l)| !l.back && l.due >= day * 86_400 + REVIEW_HOUR * 3600 && l.due < end).count();
        if overdue + today == 0 {
            return;
        }
        let mut parts = Vec::new();
        if today > 0 {
            parts.push(format!("{today} due today"));
        }
        if overdue > 0 {
            parts.push(format!("{overdue} overdue"));
        }
        out.push(Post::Review { title: "Later".into(), body: parts.join(", ") });
    }
}

/// The new part of a message's text: quoted lines and the signature
/// left out, at most 2 KiB.
fn new_text(t: &str) -> String {
    let mut out = String::new();
    for line in t.lines() {
        if line == "-- " {
            break;
        }
        let l = line.trim_end();
        if l.starts_with('>') || (l.starts_with("On ") && l.ends_with("wrote:")) {
            continue;
        }
        if out.len() + l.len() + 1 > BODY_MAX {
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(l);
    }
    out.trim().to_string()
}

fn first_names<'a>(names: impl Iterator<Item = &'a str>) -> String {
    let mut seen: Vec<&str> = Vec::new();
    for n in names {
        let first = n.split_whitespace().next().unwrap_or(n);
        if !seen.contains(&first) {
            seen.push(first);
        }
    }
    let more = seen.len() > 3;
    let mut s = seen.into_iter().take(3).collect::<Vec<_>>().join(", ");
    if more {
        s += "…";
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_and_names() {
        assert_eq!(new_text("Yes, Friday works.\n\nOn Mon, Arun wrote:\n> Lunch?\n-- \nLena"), "Yes, Friday works.");
        assert_eq!(first_names(["Priya Raman", "Arun", "Priya R", "Lena", "Sam"].into_iter()), "Priya, Arun, Lena…");
    }
}
