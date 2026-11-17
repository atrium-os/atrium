//! Replies and forwards (mail.md §9.1, §9.2), from what the parse jail
//! checked: recipients, subject, threading headers and the quoted text.

use mail_schema::{Address, Summary};

use crate::write::{clean, Mailbox};

/// The start of a reply or a forward, for the compose window.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Start {
    pub to: Vec<Mailbox>,
    pub cc: Vec<Mailbox>,
    pub subject: String,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    /// The text under the caret: the quote, or the forwarded message.
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Reply,
    ReplyAll,
    Forward,
}

fn mb(a: &Address) -> Mailbox {
    Mailbox { name: clean(&a.name), addr: a.addr.trim().to_string() }
}

fn same(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// "Re: " once (any "Re:", "RE:", "Aw:" already there counts), "Fwd: " once.
pub fn subject(s: &str, kind: Kind) -> String {
    let s = clean(s);
    let lower = s.to_lowercase();
    let has = |ps: &[&str]| ps.iter().any(|p| lower.starts_with(p));
    match kind {
        Kind::Forward if has(&["fwd:", "fw:"]) => s,
        Kind::Forward => format!("Fwd: {s}"),
        _ if has(&["re:", "aw:", "sv:", "antw:"]) => s,
        _ => format!("Re: {s}"),
    }
}

/// `text` quoted for a reply: "On …, Name wrote:" and each line marked.
pub fn quote(text: &str, who: &str, when: &str) -> String {
    let mut out = format!("\n\nOn {when}, {who} wrote:\n");
    for l in text.trim_end().lines() {
        if l.starts_with('>') {
            out.push('>');
        } else {
            out.push_str("> ");
        }
        out.push_str(l.trim_end());
        out.push('\n');
    }
    out
}

/// Which of the person's addresses a reply comes from (E12): the first
/// of `mine` the message was sent to; else none (the account's default).
pub fn reply_from(o: &Summary, mine: &[String]) -> Option<usize> {
    mine.iter().position(|m| o.to.iter().chain(&o.cc).any(|a| same(&a.addr, m)))
}

/// A reply, reply-all or forward of `o`, whose text is `text` and which
/// came `when` (as the reader shows the date). `mine`: the person's own
/// addresses, never replied to.
pub fn start(o: &Summary, text: &str, when: &str, mine: &[String], kind: Kind) -> Start {
    let who = o.from.first().map(|a| if a.name.trim().is_empty() { a.addr.clone() } else { clean(&a.name) }).unwrap_or_default();
    let is_mine = |a: &str| mine.iter().any(|m| same(m, a));
    let mut refs = o.references.clone();
    if let Some(id) = &o.message_id {
        refs.push(id.clone());
    }
    if kind == Kind::Forward {
        let mut t = String::from("\n\n---------- Forwarded message ----------\n");
        let list = |l: &[Address]| l.iter().map(|a| if a.name.trim().is_empty() { a.addr.clone() } else { format!("{} <{}>", clean(&a.name), a.addr) }).collect::<Vec<_>>().join(", ");
        t.push_str(&format!("From: {}\nDate: {when}\nSubject: {}\nTo: {}\n", list(&o.from), clean(&o.subject), list(&o.to)));
        if !o.cc.is_empty() {
            t.push_str(&format!("Cc: {}\n", list(&o.cc)));
        }
        t.push('\n');
        t.push_str(text.trim_end());
        t.push('\n');
        return Start { subject: subject(&o.subject, kind), references: refs, text: t, ..Start::default() };
    }
    // To: Reply-To, else From; a reply to one's own sent message goes to its recipients.
    let sent_by_me = o.from.iter().all(|a| is_mine(&a.addr)) && !o.from.is_empty();
    let mut to: Vec<Mailbox> = if sent_by_me {
        o.to.iter().map(mb).collect()
    } else if !o.reply_to.is_empty() {
        o.reply_to.iter().map(mb).collect()
    } else {
        o.from.iter().map(mb).collect()
    };
    let mut cc: Vec<Mailbox> = Vec::new();
    if kind == Kind::ReplyAll {
        let more: Vec<&Address> = if sent_by_me { o.cc.iter().collect() } else { o.to.iter().chain(&o.cc).collect() };
        for a in more {
            cc.push(mb(a));
        }
    }
    let mut seen: Vec<String> = Vec::new();
    let mut keep = |m: &Mailbox| {
        let k = m.addr.to_ascii_lowercase();
        if k.is_empty() || is_mine(&k) || seen.contains(&k) {
            return false;
        }
        seen.push(k);
        true
    };
    to.retain(|m| keep(m));
    cc.retain(|m| keep(m));
    // Everyone else was me: reply to me (a note to self).
    if to.is_empty() && cc.is_empty() {
        to = o.from.iter().map(mb).collect();
    }
    Start {
        to,
        cc,
        subject: subject(&o.subject, kind),
        in_reply_to: o.message_id.clone(),
        references: refs,
        text: quote(text, &who, when),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(name: &str, addr: &str) -> Address {
        Address { name: name.into(), addr: addr.into(), name_looks_like_address: false }
    }

    fn msg() -> Summary {
        Summary {
            message_id: Some("<q4@acme.example>".into()),
            references: vec!["<q3@acme.example>".into()],
            subject: "Q4 numbers".into(),
            from: vec![a("Arun", "arun@acme.example")],
            to: vec![a("Priya", "priya@example.org"), a("Lena", "lena@acme.example")],
            cc: vec![a("", "PRIYA@example.org"), a("Sam", "sam@acme.example")],
            ..Default::default()
        }
    }

    #[test]
    fn replies() {
        let mine = ["priya@example.org".to_string()];
        let r = start(&msg(), "Numbers:\n> earlier\n10", "Mon, 23 Nov", &mine, Kind::Reply);
        assert_eq!(r.to, [Mailbox::new("Arun", "arun@acme.example")]);
        assert!(r.cc.is_empty());
        assert_eq!(r.subject, "Re: Q4 numbers");
        assert_eq!(r.in_reply_to.as_deref(), Some("<q4@acme.example>"));
        assert_eq!(r.references, ["<q3@acme.example>", "<q4@acme.example>"]);
        assert_eq!(r.text, "\n\nOn Mon, 23 Nov, Arun wrote:\n> Numbers:\n>> earlier\n> 10\n");
        let all = start(&msg(), "x", "d", &mine, Kind::ReplyAll);
        assert_eq!(all.cc.iter().map(|m| m.addr.as_str()).collect::<Vec<_>>(), ["lena@acme.example", "sam@acme.example"], "never me, once each");
        // Reply-To wins.
        let mut m = msg();
        m.reply_to = vec![a("", "billing@other.example")];
        assert_eq!(start(&m, "", "d", &mine, Kind::Reply).to[0].addr, "billing@other.example");
        // A reply to my own sent message goes to its recipients.
        let mut m = msg();
        m.from = vec![a("Priya", "priya@example.org")];
        m.to = vec![a("Arun", "arun@acme.example")];
        assert_eq!(start(&m, "", "d", &mine, Kind::Reply).to[0].addr, "arun@acme.example");
        assert_eq!(subject("RE: Q4", Kind::Reply), "RE: Q4");
        assert_eq!(subject("Fwd: Q4", Kind::Forward), "Fwd: Q4");
        assert_eq!(reply_from(&msg(), &["me@else.example".into(), "priya@example.org".into()]), Some(1));
    }

    #[test]
    fn forwards() {
        let f = start(&msg(), "Body", "Mon, 23 Nov", &[], Kind::Forward);
        assert!(f.to.is_empty() && f.in_reply_to.is_none());
        assert_eq!(f.subject, "Fwd: Q4 numbers");
        assert!(f.text.contains("From: Arun <arun@acme.example>\nDate: Mon, 23 Nov\nSubject: Q4 numbers\n"));
        assert!(f.text.ends_with("\nBody\n"));
    }
}
