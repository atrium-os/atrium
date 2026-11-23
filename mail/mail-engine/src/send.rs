//! Sending (mail.md §9.3, §9.4): a draft is written into a message and put
//! in the outbox; it waits out the undo window (or the time chosen with
//! Send later) and is then handed to the server through a courier
//! submission session. Afterwards a copy goes to the Sent folder, unless the
//! provider files one itself.
//!
//! The engine never opens a connection: the caller passes one (the courier
//! session for this account), so a send happens only where the caller's
//! policy allows (a press, or the person's own schedule).

use std::io::Read;

use mail_compose::{Draft, Stamp};
use mail_store::{AccountId, Outgoing, Role, SendState};
use mail_sync::submit::{self, SendError};

use crate::{now, Engine, Error};

/// The undo window for sends, by default (§9.3; a setting, 5–30 s).
pub const UNDO_SEND_SECS: i64 = 10;
/// The range the undo setting takes.
pub const UNDO_SEND_RANGE: std::ops::RangeInclusive<i64> = 5..=30;
/// A scheduled send held on the server needs at least this much time left;
/// closer than that it is simply sent.
pub const HOLD_MIN_SECS: i64 = 120;
/// A scheduled send whose time passed this long ago while the machine was
/// off is asked about, not sent (§9.3).
pub const OVERDUE_GRACE: i64 = 15 * 60;

/// When a message leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct When {
    /// Seconds to wait for Undo (clamped to [`UNDO_SEND_RANGE`]).
    pub undo_secs: i64,
    /// Send later: the time (seconds since 1970).
    pub later: Option<i64>,
    /// Remind if nobody replies within this many seconds of it leaving: a
    /// *Waiting* item once the sent copy is in Sent (§9, §11.4).
    pub follow_up: Option<i64>,
}

impl Default for When {
    fn default() -> Self {
        When { undo_secs: UNDO_SEND_SECS, later: None, follow_up: None }
    }
}

/// What handing a send to the server did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Delivery {
    /// It left; `copy` is the message to file in Sent (`None`: the
    /// provider files it itself, or it was held on the server). `rejected`:
    /// recipients the server refused, with its reply.
    Sent { copy: Option<Vec<u8>>, rejected: Vec<(String, String)>, held: bool },
    /// The server can't hold it until `until`: the engine keeps it and sends
    /// it then.
    HeldHere { until: i64 },
    /// A failed attempt, or an uncertain submission held for review.
    Failed { why: String, transient: bool },
    /// Not due, or taken back, or already being sent: nothing done.
    NotDue,
}

pub(crate) fn random16() -> [u8; 16] {
    let mut r = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut r);
    }
    if r == [0; 16] {
        // No /dev/urandom: the time and the process still make it unique.
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        r[..8].copy_from_slice(&(t as u64).to_le_bytes());
        r[8..12].copy_from_slice(&std::process::id().to_le_bytes());
    }
    r
}

impl Engine {
    /// Write `draft` and put it in the outbox of `account`; the send's id
    /// (for [`Engine::undo_send`]). The message is written now (its Date is
    /// when Send was pressed, or the scheduled time).
    pub fn send(&self, account: AccountId, draft: &Draft, when: When) -> Result<u64, Error> {
        self.send_protected(account, draft, when, crate::smime::Protect::default())
    }

    /// [`Engine::send`], signed and/or encrypted with S/MIME (§10.1).
    pub fn send_protected(&self, account: AccountId, draft: &Draft, when: When, protect: crate::smime::Protect) -> Result<u64, Error> {
        let t = now();
        let undo = when.undo_secs.clamp(*UNDO_SEND_RANGE.start(), *UNDO_SEND_RANGE.end());
        let later = when.later.filter(|l| *l > t + undo);
        let random = random16();
        let mut w = mail_compose::write(draft, &Stamp { date: later.unwrap_or(t), random }).map_err(|e| Error::Compose(e.to_string()))?;
        w.bytes = self.protect(&w, draft, protect, t, &random)?;
        let message = self.store.put_blob(&w.bytes)?;
        let draft_blob = self.store.put_blob(&postcard::to_stdvec(draft).map_err(|e| Error::Compose(e.to_string()))?)?;
        let o = Outgoing {
            account,
            message,
            draft: draft_blob,
            message_id: w.message_id,
            from: w.from,
            rcpts: w.rcpts,
            due: t + undo,
            later,
            hold_on_server: true,
            state: SendState::Waiting,
            tries: 0,
            last_error: None,
        };
        let follow_up = when.follow_up.map(|s| later.unwrap_or(t) + s);
        Ok(self.store.write(|wr| {
            if let Some(due) = follow_up {
                wr.set_follow_up(&o.message_id, account, due)?;
            }
            wr.queue_send(&o)
        })?)
    }

    /// Undo a send not yet handed over: its draft, to open in compose
    /// again. `None` when it has already gone (or is going).
    pub fn undo_send(&self, id: u64) -> Result<Option<Draft>, Error> {
        let Some(o) = self.store.write(|w| w.take_back(id))? else { return Ok(None) };
        self.draft_of(&o).map(Some)
    }

    /// Add a sent message's recipients to the address book (§14.2), with
    /// the names the draft gave them.
    fn note_wrote(&self, w: &mut mail_store::Writer<'_>, o: &Outgoing) -> mail_store::Result<()> {
        let draft = self.draft_of(o).unwrap_or_default();
        let named: Vec<(&str, &str)> = draft.to.iter().chain(&draft.cc).chain(&draft.bcc).map(|m| (m.name.as_str(), m.addr.as_str())).collect();
        let to: Vec<(&str, &str)> = o
            .rcpts
            .iter()
            .map(|r| named.iter().find(|(_, a)| a.eq_ignore_ascii_case(r)).copied().unwrap_or(("", r.as_str())))
            .collect();
        w.wrote_to(Some(&o.message_id), &to, o.later.unwrap_or_else(now))
    }

    /// Who `typed` could mean, from the address book (§14.2): at most `n`,
    /// most likely first.
    pub fn complete(&self, typed: &str, n: usize) -> Result<Vec<mail_store::people::Person>, Error> {
        Ok(self.store.complete(typed, now(), n)?)
    }

    /// Fill an empty address book from the Sent folders already synced
    /// (a store from before the address book). The people added.
    pub fn people_from_sent(&self) -> Result<u64, Error> {
        if self.store.people_count()? > 0 {
            return Ok(0);
        }
        for (account, _) in self.store.accounts()? {
            let Some((sent, _)) = self.store.folders(account)?.into_iter().find(|(_, f)| f.role == Role::Sent && !f.hidden) else { continue };
            let mut offset = 0;
            loop {
                let rows = self.store.list(sent, offset, 500)?;
                if rows.is_empty() {
                    break;
                }
                offset += rows.len();
                self.store.write(|w| {
                    for r in &rows {
                        if let Ok(m) = self.store.message(r.msg) {
                            let s = &m.summary;
                            let to: Vec<(&str, &str)> = s.to.iter().chain(&s.cc).chain(&s.bcc).map(|a| (a.name.as_str(), a.addr.as_str())).collect();
                            w.wrote_to(s.message_id.as_deref(), &to, m.date)?;
                        }
                    }
                    Ok(())
                })?;
            }
        }
        Ok(self.store.people_count()?)
    }

    /// The draft a send was written from.
    pub fn draft_of(&self, o: &Outgoing) -> Result<Draft, Error> {
        let b = self.store.blob(&o.draft)?;
        postcard::from_bytes(&b).map_err(|_| Error::Store(mail_store::Error::Corrupt("draft")))
    }

    /// Hand send `id` to the server over `conn` (a courier submission
    /// session for its account, already signed in), if it is due.
    /// `files_sent`: the provider files sent mail itself (Gmail).
    pub fn deliver(&self, id: u64, conn: impl FnOnce() -> std::io::Result<std::os::unix::net::UnixStream>, files_sent: bool) -> Result<Delivery, Error> {
        let t = now();
        if !self.store.write(|w| w.claim_send(id, t))? {
            return Ok(Delivery::NotDue);
        }
        let Some((_, o)) = self.store.outbox()?.into_iter().find(|(k, _)| *k == id) else { return Ok(Delivery::NotDue) };
        let hold_for = match o.later {
            Some(l) if o.hold_on_server && l - t >= HOLD_MIN_SECS => Some((l - t) as u64),
            _ => None,
        };
        let failed = |why: String, transient: bool| -> Result<Delivery, Error> {
            self.store.write(|w| w.send_failed(id, &why, transient, now()))?;
            Ok(Delivery::Failed { why, transient })
        };
        let bytes = match self.store.blob(&o.message) {
            Ok(b) => b,
            Err(e) => return failed(format!("the written message is missing: {e}"), false),
        };
        let s = match conn() {
            Ok(s) => s,
            Err(e) => return failed(format!("couldn't reach the server: {e}"), true),
        };
        match crate::accounts::submit(s, &submit::Outgoing { from: &o.from, rcpts: &o.rcpts, bytes: &bytes, hold_for }) {
            Ok(sent) => {
                self.store.write(|w| {
                    w.send_done(id)?;
                    self.note_wrote(w, &o)
                })?;
                let held = hold_for.is_some();
                Ok(Delivery::Sent { copy: (!files_sent).then_some(bytes), rejected: sent.rejected, held })
            }
            Err(SendError::HoldNotOffered) => {
                let until = o.later.unwrap_or(t);
                self.store.write(|w| w.hold_here(id, until))?;
                Ok(Delivery::HeldHere { until })
            }
            Err(SendError::Failed { why, transient }) => failed(why, transient),
        }
    }

    /// [`Engine::deliver`] over JMAP (§6.1): the server files the copy in
    /// Sent itself, and holds it until later when it can.
    pub fn deliver_jmap(&self, id: u64, client: &mut dyn mail_sync::jmap::Jmap) -> Result<Delivery, Error> {
        use mail_sync::jmap::SendError;

        let t = now();
        if !self.store.write(|w| w.claim_jmap_send(id, t))? {
            return Ok(Delivery::NotDue);
        }
        let Some((_, o)) = self.store.outbox()?.into_iter().find(|(k, _)| *k == id) else { return Ok(Delivery::NotDue) };
        let failed = |why: String, transient: bool| -> Result<Delivery, Error> {
            self.store.write(|w| w.send_failed(id, &why, transient, now()))?;
            Ok(Delivery::Failed { why, transient })
        };
        let send_at = match o.later {
            Some(l) if o.hold_on_server && l - t >= HOLD_MIN_SECS => {
                if !client.can_hold(l - t) {
                    self.store.write(|w| w.hold_here(id, l))?;
                    return Ok(Delivery::HeldHere { until: l });
                }
                Some(l)
            }
            _ => None,
        };
        let bytes = match self.store.blob(&o.message) {
            Ok(b) => b,
            Err(e) => return failed(format!("the written message is missing: {e}"), false),
        };
        match client.send(&bytes, &o.from, &o.rcpts, send_at) {
            Ok(_) => {
                self.store.write(|w| {
                    w.send_done(id)?;
                    self.note_wrote(w, &o)
                })?;
                Ok(Delivery::Sent { copy: None, rejected: Vec::new(), held: send_at.is_some() })
            }
            Err(SendError::BeforeSubmission { why, transient }) => failed(why, transient),
            Err(SendError::Refused(why)) => failed(why, false),
            Err(SendError::Uncertain(detail)) => {
                let why = format!("JMAP submission could not be confirmed and may have gone through: {detail}");
                self.store.write(|w| w.send_uncertain(id, &why))?;
                Ok(Delivery::Failed { why, transient: false })
            }
        }
    }

    /// The name of `account`'s Sent folder, if it has one.
    pub fn sent_folder(&self, account: AccountId) -> Result<Option<String>, Error> {
        Ok(self.store.folders(account)?.into_iter().find(|(_, f)| f.role == Role::Sent && !f.hidden).map(|(_, f)| f.name))
    }

    /// File a sent message's `copy` in the account's Sent folder (seen), on
    /// `imap`. False when the account has no Sent folder.
    pub fn file_sent(&self, account: AccountId, imap: &mut dyn mail_sync::remote::Imap, copy: &[u8]) -> Result<bool, Error> {
        let Some(name) = self.sent_folder(account)? else { return Ok(false) };
        imap.append(&name, &["\\Seen"], copy).map_err(|e| Error::Sync(e.to_string()))?;
        Ok(true)
    }

    /// Sends to hand over now: due, and not among the overdue ones Mail
    /// asks about first.
    pub fn due_sends(&self) -> Result<Vec<(u64, Outgoing)>, Error> {
        let t = now();
        let ask: Vec<u64> = self.store.sends_overdue(t, OVERDUE_GRACE)?.into_iter().map(|(k, _)| k).collect();
        Ok(self.store.sends_due(t)?.into_iter().filter(|(k, _)| !ask.contains(k)).collect())
    }

    /// Send a waiting send now ("Send now", or yes to overdue sends).
    pub fn send_now(&self, id: u64) -> Result<bool, Error> {
        Ok(self.store.write(|w| w.send_now(id, now()))?)
    }

    /// Sends the person scheduled that came due while the machine was off;
    /// Mail asks "Send now?" before [`Engine::deliver`] sends them.
    pub fn overdue_sends(&self) -> Result<Vec<(u64, Outgoing)>, Error> {
        Ok(self.store.sends_overdue(now(), OVERDUE_GRACE)?)
    }

    /// At start: sends Mail was in the middle of when it stopped wait for
    /// the person (they may have gone). How many.
    pub fn recover_sends(&self) -> Result<usize, Error> {
        Ok(self.store.write(|w| w.stop_interrupted_sends())?)
    }
}
