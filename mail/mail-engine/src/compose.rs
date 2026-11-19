//! What a compose window starts with (mail.md §9.1, §9.2): a new message,
//! a reply, reply-all or forward, or a `mailto:` link; From is one of the
//! account's identities (a reply comes from the address it was sent to,
//! E12) and its signature is added.

use mail_compose::mailto;
use mail_compose::reply::{self, Kind};
use mail_compose::{Draft, Mailbox};
use mail_store::{AccountId, Identity, MsgId};

use crate::{Engine, Error};

/// The signature separator (a line of "-- ", RFC 3676 §4.3).
pub const SIG_SEP: &str = "-- \n";

/// A compose window's start.
#[derive(Debug, Clone, PartialEq)]
pub struct Compose {
    pub account: AccountId,
    pub draft: Draft,
    /// The original's converted document, shown under a reply or forward
    /// (in a doc-viewer, as the reader shows it).
    pub original: Option<mail_schema::Document>,
    /// Which identity From is.
    pub identity: usize,
    /// Notes the window shows above the message (from a `mailto:` link).
    pub notes: Vec<String>,
}

fn sig_block(i: &Identity) -> String {
    let s = i.signature.trim_end();
    if s.is_empty() {
        String::new()
    } else {
        format!("\n\n{SIG_SEP}{s}\n")
    }
}

impl Engine {
    /// `account`'s From addresses, the default first. With none set, the
    /// account's own address.
    pub fn identities(&self, account: AccountId) -> Result<Vec<Identity>, Error> {
        let ids = self.store.identities(account)?;
        if !ids.is_empty() {
            return Ok(ids);
        }
        let a = self.store.accounts()?.into_iter().find(|(id, _)| *id == account).ok_or(Error::NotFound)?.1;
        Ok(vec![Identity { name: String::new(), addr: a.name, signature: String::new() }])
    }

    fn from(&self, account: AccountId, identity: usize) -> Result<(usize, Identity), Error> {
        let ids = self.identities(account)?;
        let i = identity.min(ids.len() - 1);
        Ok((i, ids[i].clone()))
    }

    /// A new message from identity `identity` (0: the default).
    pub fn compose_new(&self, account: AccountId, identity: usize) -> Result<Compose, Error> {
        let (i, id) = self.from(account, identity)?;
        let draft = Draft { from: Mailbox::new(&id.name, &id.addr), text: sig_block(&id), ..Draft::default() };
        Ok(Compose { account, draft, identity: i, notes: Vec::new(), original: None })
    }

    /// A reply, reply-all or forward of message `msg`.
    pub fn compose_reply(&self, msg: MsgId, kind: Kind) -> Result<Compose, Error> {
        let m = self.store.message(msg).map_err(|_| Error::NotFound)?;
        let account = m.account;
        let mut ids = self.identities(account)?;
        // Sent to a private address (§13.3): the reply comes from it.
        if let Some(p) = self.private_to(account, &m.summary) {
            if !ids.iter().any(|i| i.addr.eq_ignore_ascii_case(&p)) {
                let base = ids.first().cloned().unwrap_or_default();
                ids.push(mail_store::Identity { name: base.name, addr: p, signature: base.signature });
            }
        }
        let mine: Vec<String> = ids.iter().map(|i| i.addr.clone()).collect();
        let identity = reply::reply_from(&m.summary, &mine).unwrap_or(0);
        let r = self.open(msg)?;
        let mut text = r.text.clone().or(m.summary.text.clone()).unwrap_or_default().replace("\r\n", "\n");
        // An HTML-only original: its text, for the plain part's quote.
        if text.trim().is_empty() {
            if let Some(d) = &r.document {
                text = mail_compose::quote::text_of(&d.html);
            }
        }
        let when = m.summary.date.map(mail_compose::write::date).unwrap_or_default();
        let s = reply::start(&m.summary, &text, &when, &mine, kind);
        let id = &ids[identity];
        // An HTML original is quoted with its formatting: kept apart from
        // the text, under it in each part.
        let (text, quote) = match &r.document {
            Some(doc) => {
                let intro: Vec<&str> = s.text.trim_start().lines().take_while(|l| !l.is_empty() && !l.starts_with('>')).collect();
                let html = mail_compose::quote::html(&intro.join("\n"), &doc.html, kind != Kind::Forward);
                (sig_block(id), Some(mail_compose::quote::Quote { text: s.text, html }))
            }
            None => (format!("{}{}", sig_block(id), s.text), None),
        };
        let draft = Draft {
            from: Mailbox::new(&id.name, &id.addr),
            to: s.to,
            cc: s.cc,
            subject: s.subject,
            text,
            in_reply_to: s.in_reply_to,
            references: s.references,
            quote,
            ..Draft::default()
        };
        let mut c = Compose { account, draft, identity, notes: Vec::new(), original: r.document };
        // Replying to a first contact or a look-alike: the reader's note (§9.1).
        if kind != Kind::Forward {
            if let Some(n) = self.sender_note(msg)? {
                c.notes.push(n.line().0);
            }
        }
        // A forward carries the original's files (§9.1).
        if kind == Kind::Forward {
            for a in &r.attachments {
                match self.attachment(msg, &a.which) {
                    Ok(got) => c.draft.attachments.push(mail_compose::Attachment { name: a.name.clone(), mime: a.content_type.clone(), bytes: got.bytes }),
                    Err(_) => c.notes.push(format!("“{}” couldn't be read, so it isn't attached.", a.name)),
                }
            }
        }
        Ok(c)
    }

    /// A message filled from a `mailto:` link (shown in full; never sent
    /// without the person pressing Send).
    pub fn compose_mailto(&self, account: AccountId, link: &str) -> Result<Compose, Error> {
        let f = mailto::parse(link).map_err(Error::Compose)?;
        let mut c = self.compose_new(account, 0)?;
        let mbs = |l: Vec<String>| l.into_iter().map(|a| Mailbox::new("", &a)).collect();
        c.draft.to = mbs(f.to);
        c.draft.cc = mbs(f.cc);
        c.draft.bcc = mbs(f.bcc);
        c.draft.subject = f.subject;
        c.draft.text = format!("{}{}", f.body, c.draft.text);
        c.draft.in_reply_to = f.in_reply_to;
        c.notes = f.notes;
        Ok(c)
    }

    /// Change From to identity `to`; the old identity's signature, if the
    /// text still has it, becomes the new one's.
    pub fn switch_identity(&self, c: &mut Compose, to: usize) -> Result<(), Error> {
        let ids = self.identities(c.account)?;
        let Some(new) = ids.get(to) else { return Err(Error::NotFound) };
        let old = sig_block(&ids[c.identity.min(ids.len() - 1)]);
        let new_sig = sig_block(new);
        if !old.is_empty() && c.draft.text.contains(&old) {
            c.draft.text = c.draft.text.replacen(&old, &new_sig, 1);
        } else if old.is_empty() && !new_sig.is_empty() && !c.draft.text.contains(SIG_SEP) {
            // No signature yet: under the person's text, above any quote.
            let at = c.draft.text.find("\n\nOn ").or_else(|| c.draft.text.find("\n\n---------- Forwarded")).unwrap_or(c.draft.text.len());
            c.draft.text.insert_str(at, &new_sig);
        }
        c.draft.from = Mailbox::new(&new.name, &new.addr);
        c.identity = to;
        Ok(())
    }
}

/// The recipients of `to` outside the domain of `from` (§9.1, for a Work
/// persona): an address at that domain or under it (`eu.example.com` for
/// `example.com`) is inside; case aside. Each once, in order.
pub fn outside_domain<'a>(from: &str, to: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let dom = |a: &str| a.trim().rsplit_once('@').map(|(_, d)| d.trim_end_matches('.').to_ascii_lowercase());
    let Some(home) = dom(from).filter(|d| !d.is_empty()) else { return Vec::new() };
    let mut out: Vec<String> = Vec::new();
    for a in to {
        let Some(d) = dom(a) else { continue };
        let inside = d == home || d.ends_with(&format!(".{home}"));
        let a = a.trim().to_string();
        if !inside && !out.iter().any(|x| x.eq_ignore_ascii_case(&a)) {
            out.push(a);
        }
    }
    out
}

#[cfg(test)]
mod outside_tests {
    use super::outside_domain;

    #[test]
    fn outside_the_company() {
        let to = ["priya@example.com", "Lena@EU.Example.com", "ada@other.org", "ADA@other.org", "sam@notexample.com", "nobody"];
        assert_eq!(outside_domain("arun@example.com", to), ["ada@other.org", "sam@notexample.com"]);
        assert!(outside_domain("not an address", to).is_empty());
    }
}
