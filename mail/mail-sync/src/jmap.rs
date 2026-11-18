//! JMAP sync and send (mail.md §6.1, RFC 8620/8621) into the same [`Sink`]
//! IMAP fills. The HTTP is the caller's: an Arca courier that fills in the
//! account's `Authorization` (bearer token or Basic, §5.2), so nothing here
//! holds a credential.
//!
//! The sink's model is IMAP's (a folder's messages by UID), so each JMAP
//! mailbox is a folder named by its path, and a message gets a local UID in
//! each mailbox it is in, allocated from the folder's `uidnext`; the JMAP
//! email id is its server id, which the sink uses to find it again.

use mail_proto::jmap::{self, Answers, Email, Mailbox, Update};

use crate::{Change, FolderState, Report, Role, RemoteFolder, Sink, What};

/// The caller's HTTP to the JMAP server (through the courier).
pub trait Http {
    /// GET `url`; the body of a 2xx answer, at most `max` bytes.
    fn get(&mut self, url: &str, max: usize) -> std::result::Result<Vec<u8>, String>;
    /// POST `body` as `content_type`; the body of a 2xx answer.
    fn post(&mut self, url: &str, content_type: &str, body: &[u8], max: usize) -> std::result::Result<Vec<u8>, String>;
    /// GET `url` as an event stream: `on_event` for each whole event while
    /// it returns `true`. `Ok` when the server ends the stream.
    fn events(&mut self, _url: &str, _on_event: &mut dyn FnMut(&[u8]) -> bool) -> std::result::Result<(), String> {
        Err("no push here".into())
    }
}

impl<T: Http + ?Sized> Http for Box<T> {
    fn get(&mut self, url: &str, max: usize) -> std::result::Result<Vec<u8>, String> {
        (**self).get(url, max)
    }
    fn post(&mut self, url: &str, content_type: &str, body: &[u8], max: usize) -> std::result::Result<Vec<u8>, String> {
        (**self).post(url, content_type, body, max)
    }
    fn events(&mut self, url: &str, on_event: &mut dyn FnMut(&[u8]) -> bool) -> std::result::Result<(), String> {
        (**self).events(url, on_event)
    }
}

/// What the engine does with a JMAP account, here or in the `sync`
/// worker (remote.rs).
pub trait Jmap {
    fn sync(&mut self, sink: &mut dyn Sink) -> Result<Report>;
    fn send(&mut self, message: &[u8], from: &str, rcpts: &[String], send_at: Option<i64>) -> Result<String>;
    fn can_hold(&self, secs: i64) -> bool;
    /// See [`Client::watch`].
    fn watch(&mut self, on: &mut dyn FnMut(bool) -> bool) -> Result<Pushed>;
    /// Search the server for `text` (§11.2); see [`Client::search_server`].
    fn search(&mut self, text: &str, sink: &mut dyn Sink, limit: usize) -> Result<usize>;
}

impl<H: Http> Jmap for Client<H> {
    fn sync(&mut self, sink: &mut dyn Sink) -> Result<Report> {
        Client::sync(self, sink)
    }
    fn send(&mut self, message: &[u8], from: &str, rcpts: &[String], send_at: Option<i64>) -> Result<String> {
        Client::send(self, message, from, rcpts, send_at)
    }
    fn can_hold(&self, secs: i64) -> bool {
        Client::can_hold(self, secs)
    }
    fn search(&mut self, text: &str, sink: &mut dyn Sink, limit: usize) -> Result<usize> {
        Client::search_server(self, text, sink, limit)
    }
    fn watch(&mut self, on: &mut dyn FnMut(bool) -> bool) -> Result<Pushed> {
        Client::watch(self, on)
    }
}

/// How a push watch ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Pushed {
    /// The server has no event source.
    NoPush,
    /// The caller said stop.
    Stopped,
    /// The server ended the stream (connect again).
    Ended,
}

/// Seconds between the server's pings, so a quiet stream is known alive.
pub const PING_SECS: u32 = 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<jmap::Error> for Error {
    fn from(e: jmap::Error) -> Self {
        Error(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Messages per request, and the most read of one mailbox on the first sync
/// (newest first; older ones come with search, §6.4).
pub const PAGE: u64 = 256;
pub const FIRST_SYNC_MAX: u64 = 5_000;
/// Whole messages downloaded for the text window in one pass.
pub const TEXTS_PER_PASS: usize = 200;
/// Largest message downloaded.
pub const MESSAGE_MAX: usize = 64 << 20;

/// The sink key holding the Email state.
const EMAIL_STATE: &str = "jmap:email";

pub struct Client<H: Http> {
    h: H,
    pub session: jmap::Session,
    mailboxes: Vec<Mailbox>,
}

fn role(r: Option<&str>) -> Role {
    match r {
        Some("inbox") => Role::Inbox,
        Some("sent") => Role::Sent,
        Some("drafts") => Role::Drafts,
        Some("archive") => Role::Archive,
        Some("junk") => Role::Junk,
        Some("trash") => Role::Trash,
        Some("all") => Role::All,
        Some("flagged") => Role::Flagged,
        _ => Role::Other,
    }
}

/// JMAP keywords as IMAP flags.
fn flags(keywords: &[String]) -> Vec<String> {
    keywords
        .iter()
        .map(|k| match k.as_str() {
            "$seen" => "\\Seen".to_string(),
            "$flagged" => "\\Flagged".into(),
            "$answered" => "\\Answered".into(),
            "$draft" => "\\Draft".into(),
            other => other.to_string(),
        })
        .collect()
}

fn keyword(flag: &str) -> String {
    match flag.to_ascii_lowercase().as_str() {
        "\\seen" => "$seen".into(),
        "\\flagged" => "$flagged".into(),
        "\\answered" => "$answered".into(),
        "\\draft" => "$draft".into(),
        other => other.to_string(),
    }
}

impl<H: Http> Client<H> {
    /// Read the session resource at `url` (`https://<host>/.well-known/jmap`).
    pub fn connect(mut h: H, url: &str) -> Result<Client<H>> {
        let body = h.get(url, 1 << 20).map_err(Error)?;
        let session = jmap::session(&body)?;
        Ok(Client { h, session, mailboxes: Vec::new() })
    }

    /// Listen on the session's event source (RFC 8620 §7.3) for changes to
    /// mail and mailboxes: `on(true)` when the account's states move (and
    /// once on connecting, as the stream's first states may be news),
    /// `on(false)` for any other event (a ping); return `false` to stop.
    pub fn watch(&mut self, on: &mut dyn FnMut(bool) -> bool) -> Result<Pushed> {
        let Some(t) = self.session.event_source_url.clone() else { return Ok(Pushed::NoPush) };
        let url = jmap::event_source_url(&t, "Email,Mailbox,EmailDelivery", "no", PING_SECS);
        let account = self.session.account.clone();
        let mut last: Option<Vec<(String, String)>> = None;
        let mut stopped = false;
        self.h
            .events(&url, &mut |raw| {
                let news = match jmap::sse_event(raw) {
                    Some((kind, data)) if kind == "state" => match jmap::state_change(&data, &account) {
                        Some(states) if last.as_ref() != Some(&states) => {
                            last = Some(states);
                            true
                        }
                        _ => false,
                    },
                    _ => false,
                };
                if !on(news) {
                    stopped = true;
                }
                !stopped
            })
            .map_err(Error)?;
        Ok(if stopped { Pushed::Stopped } else { Pushed::Ended })
    }

    fn call(&mut self, body: Vec<u8>) -> Result<Answers> {
        let url = self.session.api_url.clone();
        let r = self.h.post(&url, "application/json", &body, jmap::MAX_RESPONSE).map_err(Error)?;
        Ok(jmap::answers(&r)?)
    }

    /// A mailbox's folder name: its path from the top, `/` between.
    fn path(&self, id: &str) -> Option<String> {
        let mut names = Vec::new();
        let mut cur = self.mailboxes.iter().find(|m| m.id == id)?;
        for _ in 0..16 {
            names.push(cur.name.clone());
            match cur.parent.as_deref().and_then(|p| self.mailboxes.iter().find(|m| m.id == p)) {
                Some(p) => cur = p,
                None => break,
            }
        }
        names.reverse();
        Some(names.join("/"))
    }

    fn id_of(&self, folder: &str) -> Option<String> {
        self.mailboxes.iter().find(|m| self.path(&m.id).as_deref() == Some(folder)).map(|m| m.id.clone())
    }

    /// The folders, refreshed.
    pub fn folders(&mut self, sink: &mut dyn Sink) -> Result<Vec<RemoteFolder>> {
        let a = self.call(jmap::get_mailboxes(&self.session.account))?;
        let (list, _) = jmap::mailboxes(&a)?;
        self.mailboxes = list;
        let folders: Vec<RemoteFolder> = self.mailboxes.iter().filter_map(|m| Some(RemoteFolder { name: self.path(&m.id)?, role: role(m.role.as_deref()), selectable: true })).collect();
        sink.folders(&folders).map_err(Error)?;
        Ok(folders)
    }

    /// The person's changes first, then the folders, then what changed on
    /// the server (or, the first time, each mailbox's newest messages).
    pub fn sync(&mut self, sink: &mut dyn Sink) -> Result<Report> {
        let mut report = Report::default();
        if self.mailboxes.is_empty() {
            self.folders(sink)?;
        }
        self.push(sink)?;
        report.folders = self.folders(sink)?.len();
        match sink.server_state(EMAIL_STATE) {
            None => self.first(sink, &mut report)?,
            Some(state) => {
                let mut state = state;
                loop {
                    let a = self.call(jmap::email_changes(&self.session.account, &state, PAGE))?;
                    let c = match jmap::changes(&a) {
                        Ok(c) => c,
                        Err(e) if e.0.ends_with("cannotCalculateChanges") => {
                            // Too old: read everything again.
                            return self.first(sink, &mut report).map(|_| report);
                        }
                        Err(e) => return Err(e.into()),
                    };
                    for e in &c.changed {
                        self.place(sink, e, true, &mut report)?;
                    }
                    for id in &c.destroyed {
                        for (folder, uid) in sink.listed(id) {
                            sink.vanished(&folder, &[uid]).map_err(Error)?;
                            report.vanished += 1;
                        }
                    }
                    state = c.new_state;
                    sink.set_server_state(EMAIL_STATE, &state).map_err(Error)?;
                    if !c.more {
                        break;
                    }
                }
            }
        }
        self.texts(sink, &mut report)?;
        Ok(report)
    }

    /// Search the server for `text` (JMAP `Email/query`, §11.2), newest
    /// first, at most `limit`: messages not listed here are placed whole
    /// (or by their headers when large); listed ones without their text
    /// get it. How many the server found.
    pub fn search_server(&mut self, text: &str, sink: &mut dyn Sink, limit: usize) -> Result<usize> {
        if text.trim().is_empty() || limit == 0 {
            return Ok(0);
        }
        if self.mailboxes.is_empty() {
            self.folders(sink)?;
        }
        let a = self.call(jmap::query_text(&self.session.account, text, limit as u64))?;
        let (emails, _, _) = jmap::mailbox_page(&a)?;
        let mut report = Report::default();
        for e in emails.iter().take(limit) {
            let listed = sink.listed(&e.id);
            if listed.is_empty() {
                self.place(sink, e, true, &mut report)?;
                continue;
            }
            let lacking: Vec<(String, u32)> = listed.into_iter().filter(|(f, u)| !sink.has_text(f, *u)).collect();
            if lacking.is_empty() || e.size > crate::TEXT_MAX {
                continue;
            }
            let url = self.session.download(&e.blob);
            let bytes = self.h.get(&url, MESSAGE_MAX).map_err(Error)?;
            for (folder, uid) in lacking {
                sink.messages(&folder, vec![crate::Fetched { uid, flags: flags(&e.keywords), size: e.size.max(bytes.len() as u64), internal_date: e.received, server_id: Some(e.id.clone()), bytes: bytes.clone(), full: true }]).map_err(Error)?;
            }
        }
        Ok(emails.len().min(limit))
    }

    /// Stage 3 (§6.4): whole messages for the text window, newest first,
    /// the inbox and Sent before the rest, at most [`TEXTS_PER_PASS`] a pass.
    pub fn texts(&mut self, sink: &mut dyn Sink, report: &mut Report) -> Result<()> {
        let mut order: Vec<(u8, String)> = self.mailboxes.iter().filter_map(|m| Some((match m.role.as_deref() { Some("inbox") => 0, Some("sent") => 1, _ => 2 }, self.path(&m.id)?))).collect();
        order.sort();
        let mut budget = TEXTS_PER_PASS;
        for (_, folder) in order {
            if budget == 0 {
                break;
            }
            let wanted: Vec<(u32, String)> = sink.text_wanted(&folder, budget).into_iter().filter(|(_, size)| *size <= crate::TEXT_MAX).filter_map(|(uid, _)| Some((uid, sink.server_id_at(&folder, uid)?))).collect();
            if wanted.is_empty() {
                continue;
            }
            let ids: Vec<String> = wanted.iter().map(|(_, id)| id.clone()).collect();
            let a = self.call(jmap::get_emails(&self.session.account, &ids))?;
            let found = jmap::emails(&a)?;
            for (uid, id) in wanted {
                let Some(e) = found.iter().find(|e| e.id == id) else { continue };
                let url = self.session.download(&e.blob);
                let bytes = self.h.get(&url, MESSAGE_MAX).map_err(Error)?;
                sink.messages(&folder, vec![crate::Fetched { uid, flags: flags(&e.keywords), size: e.size.max(bytes.len() as u64), internal_date: e.received, server_id: Some(e.id.clone()), bytes, full: true }]).map_err(Error)?;
                report.texts += 1;
                budget -= 1;
            }
        }
        Ok(())
    }

    /// The first sync: the state now, then each mailbox's newest messages.
    fn first(&mut self, sink: &mut dyn Sink, report: &mut Report) -> Result<()> {
        let a = self.call(jmap::email_state(&self.session.account))?;
        let state = jmap::current_state(&a)?;
        let boxes: Vec<String> = self.mailboxes.iter().map(|m| m.id.clone()).collect();
        for mb in boxes {
            let mut position = 0;
            while position < FIRST_SYNC_MAX {
                let a = self.call(jmap::query_mailbox(&self.session.account, &mb, position, PAGE))?;
                let (emails, total, _) = jmap::mailbox_page(&a)?;
                if emails.is_empty() {
                    break;
                }
                position += emails.len() as u64;
                for e in &emails {
                    self.place(sink, e, false, report)?;
                }
                if position >= total {
                    break;
                }
            }
        }
        sink.set_server_state(EMAIL_STATE, &state).map_err(Error)
    }

    /// Put message `e` where it is now: out of folders it left, flags
    /// where it stayed, listed in folders it is new to. `whole`: download
    /// it whole (new mail) when it is small enough; else its header block
    /// lists it, and the text comes with [`Client::texts`] or on opening.
    fn place(&mut self, sink: &mut dyn Sink, e: &Email, whole: bool, report: &mut Report) -> Result<()> {
        let now: Vec<String> = e.mailboxes.iter().filter_map(|m| self.path(m)).collect();
        let was = sink.listed(&e.id);
        for (folder, uid) in &was {
            if !now.contains(folder) {
                sink.vanished(folder, &[*uid]).map_err(Error)?;
                report.vanished += 1;
            }
        }
        let fl = flags(&e.keywords);
        // Whole messages up to TEXT_MAX; larger ones list by their headers
        // and wait to be opened (§6.4).
        let full = (whole && e.size <= crate::TEXT_MAX) || e.header_block.is_empty();
        let mut bytes: Option<Vec<u8>> = (!full).then(|| e.header_block.clone());
        for folder in &now {
            if let Some((_, uid)) = was.iter().find(|(f, _)| f == folder) {
                sink.flags(folder, &[(*uid, fl.clone())]).map_err(Error)?;
                report.flag_changes += 1;
                continue;
            }
            if bytes.is_none() {
                let url = self.session.download(&e.blob);
                bytes = Some(self.h.get(&url, MESSAGE_MAX).map_err(Error)?);
            }
            let mut st: FolderState = sink.state(folder);
            if st.uidvalidity == 0 {
                st.uidvalidity = 1;
            }
            let uid = st.uidnext.max(1);
            let b = bytes.clone().unwrap_or_default();
            sink.messages(folder, vec![crate::Fetched { uid, flags: fl.clone(), size: e.size.max(b.len() as u64), internal_date: e.received, server_id: Some(e.id.clone()), bytes: b, full }]).map_err(Error)?;
            st.uidnext = uid + 1;
            st.synced_from = 1;
            st.exists += 1;
            sink.checkpoint(folder, &st).map_err(Error)?;
            report.headers += 1;
            report.texts += full as usize;
        }
        Ok(())
    }

    /// The person's changes, made on the server.
    fn push(&mut self, sink: &mut dyn Sink) -> Result<()> {
        let changes: Vec<Change> = sink.changes();
        if changes.is_empty() {
            return Ok(());
        }
        let mut calls: Vec<(u64, String, Update)> = Vec::new();
        for c in changes {
            let (Some(id), Some(from)) = (sink.server_id_at(&c.folder, c.uid), self.id_of(&c.folder)) else {
                sink.change_done(c.id, false).map_err(Error)?;
                continue;
            };
            let u = match &c.what {
                What::Flags { add, remove } => Update::Keywords { set: add.iter().map(|f| keyword(f)).collect(), unset: remove.iter().map(|f| keyword(f)).collect() },
                What::Move { to } => match self.id_of(to) {
                    Some(to) => Update::Move { from, to },
                    None => {
                        sink.change_done(c.id, false).map_err(Error)?;
                        continue;
                    }
                },
                What::Expunge => {
                    if sink.listed(&id).len() > 1 { Update::Remove { from } } else { Update::Destroy }
                }
            };
            calls.push((c.id, id, u));
        }
        if calls.is_empty() {
            return Ok(());
        }
        let list: Vec<(String, Update)> = calls.iter().map(|(_, id, u)| (id.clone(), u.clone())).collect();
        let a = self.call(jmap::set_emails(&self.session.account, &list))?;
        let failed = jmap::set_failures(&a)?;
        for (cid, id, _) in calls {
            sink.change_done(cid, !failed.contains(&id)).map_err(Error)?;
        }
        Ok(())
    }

    /// Send `message` (as written, with Bcc left out) from `from` to
    /// `rcpts`; the server keeps the copy in Sent. `send_at`: hold it until
    /// then, where the server holds sends. The new message's id.
    pub fn send(&mut self, message: &[u8], from: &str, rcpts: &[String], send_at: Option<i64>) -> Result<String> {
        if !self.session.can_submit {
            return Err(Error("this account can't send over JMAP".into()));
        }
        if self.session.max_size_upload > 0 && message.len() as u64 > self.session.max_size_upload {
            return Err(Error("the message is larger than the server takes".into()));
        }
        if self.mailboxes.is_empty() {
            let a = self.call(jmap::get_mailboxes(&self.session.account))?;
            self.mailboxes = jmap::mailboxes(&a)?.0;
        }
        let sent = self.mailboxes.iter().find(|m| m.role.as_deref() == Some("sent")).map(|m| m.id.clone()).ok_or(Error("the account has no Sent mailbox".into()))?;
        let a = self.call(jmap::get_identities(&self.session.account))?;
        let from_l = from.to_ascii_lowercase();
        let identity = jmap::identities(&a)?.into_iter().find(|i| i.email == from_l).ok_or_else(|| Error(format!("the server doesn't let this account send as {from}")))?;
        let url = self.session.upload();
        let r = self.h.post(&url, "message/rfc822", message, 64 << 10).map_err(Error)?;
        let blob = jmap::uploaded(&r)?;
        let hold = send_at.filter(|_| self.session.max_delayed_send > 0).map(jmap::utc_string);
        let a = self.call(jmap::submit(&self.session.account, &blob, &sent, &identity.id, from, rcpts, hold.as_deref()))?;
        Ok(jmap::submitted(&a)?)
    }

    /// Whether the server can hold a send until `t` (seconds from now).
    pub fn can_hold(&self, secs: i64) -> bool {
        secs > 0 && (secs as u64) <= self.session.max_delayed_send
    }
}
