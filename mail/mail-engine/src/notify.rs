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
//! - mail that isn't announced (lists, automated) is said in one silent
//!   digest at the person's times (9:00 and 17:00 unless they pick), the
//!   ones still unread then;
//! - Later: once a day at the review time (9:00 unless the person picks),
//!   "3 due today, 2 overdue",
//!   naming every item a week overdue or pushed back three times, every
//!   day until it is answered; and, when the weekly look is on, once on
//!   Friday after [`WEEKLY_HOUR`]: what is due next week and who has been
//!   waited on for more than a week.
//!
//! - Settings ([`crate::prefs`]): notifications off (only withdrawals),
//!   VIPs always one by one, and quiet hours (and, if asked, whole
//!   weekends): what arrives inside them is held back and said in one
//!   silent summary when they end (VIPs, and replies in the person's
//!   conversations, only get through if the person lets them).
//!
//! One Mail instance is one persona, so the instance's quiet hours are the
//! persona's. Not yet: sounds per account (Praeco's post has no sound).

use std::collections::{HashMap, VecDeque};

use mail_store::{flag, AccountId, MsgId, Role};

use crate::Engine;

pub const FRESH_SECS: i64 = 600;
const POLL_GAP_SECS: i64 = 60;
pub const BURST: usize = 2;
pub const BURST_SECS: i64 = 30;
/// The daily review's hour unless the person picks another time.
pub const REVIEW_HOUR: i64 = 9;
/// The weekly look's hour on Friday, local time.
pub const WEEKLY_HOUR: i64 = 15;
/// Items named in a review, at most.
const NAMED_MAX: usize = 3;
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
    /// The weekly look (Friday afternoon).
    Week { title: String, body: String },
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
    /// The week (Monday's day since 1970) the weekly look was posted.
    looked: Option<i64>,
    /// Arrivals held back during quiet hours, said when they end.
    quiet: Vec<Arrival>,
    /// Fresh mail not announced (lists, automated), for the digest.
    other: Vec<Arrival>,
    /// Local wall-clock time of the last look, to see a digest time pass.
    looked_at: Option<i64>,
    /// A digest time passed while it was quiet: said when it ends.
    digest_due: bool,
    // Mail polls about every five seconds. Keep the UTC boundary across later sync
    // pages so a near-wake delivery never escapes through the fresh-message path.
    last_poll: Option<i64>,
    wake_cutoff: Option<i64>,
    summarized_through: Option<i64>,
    wake_meetings_waiting: bool,
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
        let local = now + offset;
        let minute = (local.rem_euclid(86_400) / 60) as u16;
        let weekday = (local.div_euclid(86_400) + 3).rem_euclid(7) as u8;
        let quiet = prefs.is_quiet_on(weekday, minute);
        if self.last_poll.replace(now).is_some_and(|last| now.saturating_sub(last) >= POLL_GAP_SECS) {
            self.wake_cutoff = Some(now);
        }
        let mut out = self.look(e, now, offset, &prefs, quiet);
        // A digest time passed since the last look.
        if let Some(last) = self.looked_at {
            let (d0, d1) = (last.div_euclid(86_400), local.div_euclid(86_400));
            let passed = (d0..=d1).any(|d| prefs.digest.iter().any(|t| {
                let at = d * 86_400 + *t as i64 * 60;
                at > last && at <= local
            }));
            self.digest_due |= passed;
        }
        self.looked_at = Some(local);
        if !prefs.notify {
            // Off: only take back what was said before.
            self.quiet.clear();
            self.other.clear();
            self.digest_due = false;
            self.wake_meetings_waiting = false;
            out.retain(|p| matches!(p, Post::Withdraw { .. }));
            return out;
        }
        if prefs.digest.is_empty() {
            self.other.clear();
        }
        if self.digest_due && !quiet {
            self.digest_due = false;
            // The ones still unread.
            let other: Vec<Arrival> = std::mem::take(&mut self.other).into_iter().filter(|a| {
                e.store.message(a.msg).is_ok_and(|m| !m.locations.iter().any(|(f, u)| e.store.at(*f, *u).ok().flatten().is_some_and(|(_, fl)| fl & flag::SEEN != 0)))
            }).collect();
            if !other.is_empty() {
                let n = other.len();
                let body = format!("{n} {}, from {}", if n == 1 { "message" } else { "messages" }, first_names(other.iter().map(|a| a.from.as_str())));
                out.push(Post::Summary { title: "Other mail".into(), body });
            }
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
        if self.wake_meetings_waiting && !quiet {
            self.wake_meetings_waiting = false;
            self.summarized_through = self.wake_cutoff;
            if let Some(Post::Summary { body, .. }) = out.iter_mut().rev().find(|p| matches!(p, Post::Summary { .. })) {
                body.push_str("\nMeeting invitations also arrived.");
            } else {
                out.push(Post::Summary {
                    title: "Meeting invitations".into(),
                    body: "Invitations arrived while you were away. Review them in Mail.".into(),
                });
            }
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
            self.review(e, now, offset, prefs.review, &mut out);
            if prefs.weekly {
                self.week(e, now, offset, &mut out);
            }
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
        let rules = e.rules();
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
            // A `quiet` rule (§11.6).
            if crate::rules::first(&rules, s).is_some_and(|r| r.then == crate::rules::Then::Quiet) {
                continue;
            }
            let mine = me.get(&m.account).cloned().unwrap_or_default();
            if s.from.iter().any(|a| mine.contains(&a.addr.to_ascii_lowercase())) {
                continue;
            }
            if let Some(invite) = scheduling_message(s) {
                if self.wake_cutoff.is_some_and(|cutoff| m.date <= cutoff) {
                    self.wake_meetings_waiting |= self.summarized_through.is_none_or(|last| m.date > last);
                    continue;
                }
                // The individual deliveries stay available for Review in Calendar.
                if invite.sequence > 0 || invite.method != mail_schema::InviteMethod::Request {
                    continue;
                }
            }
            let wrote_in = e.store.thread(m.thread).unwrap_or_default().iter().any(|x| x.msg != id && x.message.summary.from.iter().any(|a| mine.contains(&a.addr.to_ascii_lowercase())));
            let vip = s.from.iter().any(|a| prefs.is_vip(&a.addr));
            let person = vip || (!s.automated && !crate::senders::is_list(s));
            let from = s.from.first().map(|a| if a.name.is_empty() { a.addr.clone() } else { a.name.clone() }).unwrap_or_default();
            let a = Arrival { msg: id, person: person || wrote_in, from, subject: s.subject.clone(), text: new_text(s.text.as_deref().unwrap_or(&s.preview)) };
            if quiet && !(vip && prefs.vips_through) && !(wrote_in && prefs.replies_through) {
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
        // Lists and automated mail are counted, not announced: the digest.
        for a in fresh.iter().filter(|a| !a.person) {
            if self.other.len() < SCAN_MAX {
                self.other.push(a.clone());
            }
        }
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
        self.review(e, now, offset, prefs.review, &mut out);
        if prefs.weekly {
            self.week(e, now, offset, &mut out);
        }
        out
    }

    fn inbox_has_mail(&self, e: &Engine, a: AccountId) -> bool {
        e.store.folders(a).unwrap_or_default().iter().any(|(id, f)| f.role == Role::Inbox && e.store.count(*id).unwrap_or(0) > 0)
    }

    /// Once a day after the review hour: what Later has due. Days and the
    /// hour are local (`offset` seconds east of UTC).
    fn review(&mut self, e: &Engine, now: i64, offset: i64, at: u16, out: &mut Vec<Post>) {
        let at = at as i64 * 60;
        let now = now + offset;
        let day = now.div_euclid(86_400);
        if self.reviewed == Some(day) || now - day * 86_400 < at {
            return;
        }
        self.reviewed = Some(day);
        let items: Vec<_> = e.later_items().unwrap_or_default().into_iter().map(|(id, mut l)| {
            l.due += offset;
            (id, l)
        }).collect();
        let end = (day + 1) * 86_400;
        let overdue = items.iter().filter(|(_, l)| l.due < day * 86_400 + at && !l.back).count() + items.iter().filter(|(_, l)| l.back).count();
        let today = items.iter().filter(|(_, l)| !l.back && l.due >= day * 86_400 + at && l.due < end).count();
        let asking = e.later_asking(now - offset).unwrap_or_default();
        if overdue + today == 0 && asking.is_empty() {
            return;
        }
        let mut parts = Vec::new();
        if today > 0 {
            parts.push(format!("{today} due today"));
        }
        if overdue > 0 {
            parts.push(format!("{overdue} overdue"));
        }
        let mut body = parts.join(", ");
        let stale: Vec<_> = asking.iter().filter(|a| a.2 == crate::later::Ask::Stale).map(|a| &a.1).collect();
        let pushed: Vec<_> = asking.iter().filter(|a| a.2 == crate::later::Ask::Pushed).map(|a| &a.1).collect();
        if !stale.is_empty() {
            body += &format!("\nOver a week overdue: {}", subjects(e, &stale));
        }
        if !pushed.is_empty() {
            body += &format!("\nPushed back {} times or more: {}", crate::later::PUSHED_ASK, subjects(e, &pushed));
        }
        out.push(Post::Review { title: "Later".into(), body: body.trim_start().into() });
    }

    /// The weekly look (§11.4): once a week, on Friday after
    /// [`WEEKLY_HOUR`] local time, when it is on.
    fn week(&mut self, e: &Engine, now: i64, offset: i64, out: &mut Vec<Post>) {
        let local = now + offset;
        let day = local.div_euclid(86_400);
        let weekday = (day + 3).rem_euclid(7);
        let monday = day - weekday;
        if weekday != 4 || local - day * 86_400 < WEEKLY_HOUR * 3600 || self.looked == Some(monday) {
            return;
        }
        self.looked = Some(monday);
        let Ok((next, waiting)) = e.later_week(local, offset) else { return };
        if next.is_empty() && waiting.is_empty() {
            return;
        }
        let mut lines = Vec::new();
        if !next.is_empty() {
            lines.push(format!("{} due next week: {}", next.len(), subjects(e, &next.iter().map(|x| &x.1).collect::<Vec<_>>())));
        }
        if !waiting.is_empty() {
            lines.push(format!("Waiting over a week: {}", subjects(e, &waiting.iter().map(|x| &x.1).collect::<Vec<_>>())));
        }
        out.push(Post::Week { title: "Later this week".into(), body: lines.join("\n") });
    }
}

/// Direct scheduling mail only. Ordinary discussion and forwarded conversations
/// retain their usual Mail notification behavior. Classification grants no trust.
fn scheduling_message(s: &mail_schema::Summary) -> Option<&mail_schema::Invite> {
    use mail_schema::{InviteMethod, PartKind};
    let invite = s.invite.as_ref()?;
    let [from] = s.from.as_slice() else { return None };
    if s.encrypted || s.parts.iter().any(|p| p.kind == PartKind::Message)
        || s.parts.iter().filter(|p| p.kind == PartKind::Calendar).count() != 1
        || invite.uid.is_empty() { return None; }
    match invite.method {
        InviteMethod::Request | InviteMethod::Cancel if invite.organizer.eq_ignore_ascii_case(&from.addr) => Some(invite),
        InviteMethod::Reply => Some(invite),
        _ => None,
    }
}

/// "Numbers for Q3, Lunch…": the first items' subjects (or notes).
fn subjects(e: &Engine, items: &[&mail_store::later::Later]) -> String {
    let mut s: Vec<String> = items.iter().take(NAMED_MAX).map(|l| {
        let subj = e.store.message(l.msg).map(|m| m.summary.subject.trim().to_string()).unwrap_or_default();
        let t = if !l.note.is_empty() { l.note.clone() } else if subj.is_empty() { "(no subject)".into() } else { subj };
        if t.chars().count() > 40 { t.chars().take(39).collect::<String>() + "…" } else { t }
    }).collect();
    if items.len() > NAMED_MAX {
        s.push(format!("{} more", items.len() - NAMED_MAX));
    }
    s.join(", ")
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

/// The buttons on a one-message notification (tabula.md §5.5): (verb,
/// label). Group and summary posts have none.
pub const ACTIONS: [(&str, &str); 3] = [("archive", "Archive"), ("read", "Mark read"), ("reply", "Reply")];

/// The buttons for `msg`'s notification: the action id carries the
/// message ("archive.42"), so a press that starts Mail again, or comes
/// after a restart, still knows which message it was for.
pub fn actions_for(msg: MsgId) -> Vec<(String, &'static str)> {
    ACTIONS.iter().map(|(v, l)| (format!("{v}.{msg}"), *l)).collect()
}

/// A pressed action id back as (message, verb).
pub fn action_of(id: &str) -> Option<(MsgId, &str)> {
    let (v, m) = id.split_once('.')?;
    let m: MsgId = m.parse().ok()?;
    ACTIONS.iter().find(|(x, _)| *x == v).map(|(x, _)| (m, *x))
}

/// What a button press did.
#[derive(Debug)]
pub enum Acted {
    /// Archived or marked read: the op ids, for Undo in the window.
    Done { ops: Vec<u64>, label: &'static str },
    /// A reply to open in a compose window.
    Reply(Box<crate::compose::Compose>),
    /// Nothing to do: the message is gone, or the action isn't one of
    /// [`ACTIONS`].
    Nothing,
}

impl Engine {
    /// The person pressed `action` on the notification for `msg` (§12):
    /// Archive takes the message out of the inbox, Mark read reads it
    /// everywhere it is, Reply makes a reply. Like the same press in the
    /// window, it goes to the server after the undo window.
    pub fn notification_action(&self, msg: MsgId, action: &str) -> Result<Acted, crate::Error> {
        let Ok(m) = self.store.message(msg) else { return Ok(Acted::Nothing) };
        match action {
            "archive" => {
                let inbox = m.locations.iter().find(|(f, _)| self.store.folder(*f).is_ok_and(|x| x.role == Role::Inbox)).copied();
                let Some((f, uid)) = inbox else { return Ok(Acted::Nothing) };
                let ops = self.act(f, &[uid], crate::Action::Archive)?;
                Ok(if ops.is_empty() { Acted::Nothing } else { Acted::Done { ops, label: "Message archived" } })
            }
            "read" => {
                let mut ops = Vec::new();
                for (f, uid) in &m.locations {
                    ops.extend(self.act(*f, &[*uid], crate::Action::Read(true))?);
                }
                Ok(if ops.is_empty() { Acted::Nothing } else { Acted::Done { ops, label: "Marked read" } })
            }
            "reply" => Ok(Acted::Reply(Box::new(self.compose_reply(msg, mail_compose::reply::Kind::Reply)?))),
            _ => Ok(Acted::Nothing),
        }
    }
}
