//! The outbox (mail.md §9.3): messages the person sent that haven't left
//! yet. Each waits until `due` (the undo window, or the time chosen with
//! Send later), is then claimed by the sender, and leaves the outbox when
//! the server took it. Until it is claimed, Undo takes it back to compose.
//!
//! The store keeps the written message and the draft it came from as blobs;
//! it doesn't know what's in either.

use redb::{ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::{dec, enc, AccountId, Error, Hash, Result, Store, Writer};

pub(crate) const OUTBOX: TableDefinition<u64, &[u8]> = TableDefinition::new("outbox");
/// The server identity and envelope saved before dispatch.
pub(crate) const JMAP_SUBMISSIONS: TableDefinition<u64, &[u8]> = TableDefinition::new("jmap_submissions");

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SendState {
    /// Waiting for `due`; Undo still works.
    Waiting,
    /// Being handed to the server now.
    Sending,
    /// Not sent, and won't be tried again without the person: the server
    /// refused it, or Mail stopped while sending (it may have gone).
    Stopped { why: String },
    /// A JMAP attempt ended before it could submit. It is safe to retry.
    JmapRetry,
    /// A claimed JMAP send may reach the server before Mail records its reply.
    JmapSending,
    /// Submission may have succeeded. Neither sync nor Send again resends it.
    Uncertain { why: String },
}

impl SendState {
    fn waiting(&self) -> bool {
        matches!(self, Self::Waiting | Self::JmapRetry)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outgoing {
    pub account: AccountId,
    /// The message as written (RFC 5322 bytes).
    pub message: Hash,
    /// The draft it was written from, for Undo and for a failed send.
    pub draft: Hash,
    pub message_id: String,
    pub from: String,
    pub rcpts: Vec<String>,
    /// Not handed over before this time (seconds).
    pub due: i64,
    /// The time the person chose (Send later), not just the undo window.
    pub later: Option<i64>,
    /// Try to have the server hold a scheduled send (FUTURERELEASE); false
    /// once it said it can't, so the engine holds it.
    pub hold_on_server: bool,
    pub state: SendState,
    /// Failed tries so far (a transient failure is tried again later).
    pub tries: u32,
    pub last_error: Option<String>,
}

/// Seconds before the next try after `tries` transient failures: 1, 2, 4 …
/// minutes, at most an hour.
pub fn retry_after(tries: u32) -> i64 {
    60 * (1i64 << tries.min(6)).min(60)
}

impl Writer<'_> {
    /// Put `o` in the outbox; its id.
    pub fn queue_send(&mut self, o: &Outgoing) -> Result<u64> {
        let id = Store::next_id(self.tx, "send")?;
        self.tx.open_table(OUTBOX)?.insert(id, enc(o).as_slice())?;
        Ok(id)
    }

    /// Undo a send that hasn't been claimed: it leaves the outbox and the
    /// caller gets it back (its draft opens again). A stopped send can be
    /// taken back too. `None` when it is being sent, or has gone.
    pub fn take_back(&mut self, id: u64) -> Result<Option<Outgoing>> {
        let mut t = self.tx.open_table(OUTBOX)?;
        let Some(o) = t.get(id)?.map(|v| dec::<Outgoing>(v.value())).transpose()? else { return Ok(None) };
        if matches!(o.state, SendState::Sending | SendState::JmapSending) {
            return Ok(None);
        }
        t.remove(id)?;
        self.tx.open_table(JMAP_SUBMISSIONS)?.remove(id)?;
        Ok(Some(o))
    }

    /// Claim a waiting send that is due by `now`, so Undo can't take it
    /// any more. False when it isn't waiting (taken back, or claimed).
    pub fn claim_send(&mut self, id: u64, now: i64) -> Result<bool> {
        self.claim(id, now, false)
    }

    pub fn claim_jmap_send(&mut self, id: u64, now: i64) -> Result<bool> {
        self.claim(id, now, true)
    }

    fn claim(&mut self, id: u64, now: i64, jmap: bool) -> Result<bool> {
        let mut t = self.tx.open_table(OUTBOX)?;
        let Some(mut o) = t.get(id)?.map(|v| dec::<Outgoing>(v.value())).transpose()? else { return Ok(false) };
        if !o.state.waiting() || o.due > now {
            return Ok(false);
        }
        // Older builds put uncertain submissions back in the waiting queue.
        // A preflight-only failure from this build is marked JmapRetry.
        let legacy_retry = jmap && o.state == SendState::Waiting && o.tries != 0;
        o.state = if legacy_retry {
            uncertain("An earlier JMAP send may have gone through. Check with the recipient before sending again.")
        } else if jmap {
            SendState::JmapSending
        } else {
            SendState::Sending
        };
        t.insert(id, enc(&o).as_slice())?;
        Ok(!legacy_retry)
    }

    /// The server took send `id`: it leaves the outbox.
    pub fn send_done(&mut self, id: u64) -> Result<Option<Outgoing>> {
        self.tx.open_table(JMAP_SUBMISSIONS)?.remove(id)?;
        Ok(self.tx.open_table(OUTBOX)?.remove(id)?.map(|v| dec::<Outgoing>(v.value())).transpose()?)
    }

    /// Send `id` failed. `transient`: try again at `now` + [`retry_after`];
    /// otherwise it stops until the person decides.
    pub fn send_failed(&mut self, id: u64, why: &str, transient: bool, now: i64) -> Result<()> {
        let mut t = self.tx.open_table(OUTBOX)?;
        let Some(mut o) = t.get(id)?.map(|v| dec::<Outgoing>(v.value())).transpose()? else { return Err(Error::NotFound) };
        o.last_error = Some(why.to_string());
        if transient {
            o.due = now + retry_after(o.tries);
            o.tries += 1;
            o.state = if o.state == SendState::JmapSending { SendState::JmapRetry } else { SendState::Waiting };
        } else {
            o.state = SendState::Stopped { why: why.to_string() };
        }
        t.insert(id, enc(&o).as_slice())?;
        self.tx.open_table(JMAP_SUBMISSIONS)?.remove(id)?;
        Ok(())
    }

    pub fn save_jmap_submission(&mut self, id: u64, evidence: &[u8]) -> Result<bool> {
        let outbox = self.tx.open_table(OUTBOX)?;
        let row = outbox.get(id)?.map(|value| dec::<Outgoing>(value.value())).transpose()?;
        if !row.is_some_and(|row| row.state == SendState::JmapSending) {
            return Ok(false);
        }
        let mut saved = self.tx.open_table(JMAP_SUBMISSIONS)?;
        if saved.get(id)?.is_some() {
            return Ok(false);
        }
        saved.insert(id, evidence)?;
        Ok(true)
    }

    /// Apply a read-only finding only while the original held row and its
    /// evidence still exist. Return an accepted send for address bookkeeping.
    pub fn resolve_jmap_submission(&mut self, id: u64, evidence: &[u8], canceled: bool) -> Result<Option<Outgoing>> {
        {
            let outbox = self.tx.open_table(OUTBOX)?;
            let row = outbox.get(id)?.map(|value| dec::<Outgoing>(value.value())).transpose()?;
            if !row.is_some_and(|row| matches!(row.state, SendState::Uncertain { .. })) {
                return Ok(None);
            }
            let saved = self.tx.open_table(JMAP_SUBMISSIONS)?;
            if !saved.get(id)?.is_some_and(|value| value.value() == evidence) {
                return Ok(None);
            }
        }
        if canceled {
            self.send_failed(id, "The JMAP server canceled this submission.", false, 0)?;
            return Ok(None);
        }
        self.send_done(id)
    }

    pub fn send_uncertain(&mut self, id: u64, why: &str) -> Result<()> {
        let mut t = self.tx.open_table(OUTBOX)?;
        let Some(mut o) = t.get(id)?.map(|v| dec::<Outgoing>(v.value())).transpose()? else { return Err(Error::NotFound) };
        o.last_error = Some(why.to_string());
        o.state = uncertain(why);
        t.insert(id, enc(&o).as_slice())?;
        Ok(())
    }

    /// The server can't hold claimed send `id`: it waits here until `until`.
    pub fn hold_here(&mut self, id: u64, until: i64) -> Result<()> {
        let mut t = self.tx.open_table(OUTBOX)?;
        let Some(mut o) = t.get(id)?.map(|v| dec::<Outgoing>(v.value())).transpose()? else { return Err(Error::NotFound) };
        o.hold_on_server = false;
        o.due = until;
        o.state = if o.state == SendState::JmapSending { SendState::JmapRetry } else { SendState::Waiting };
        t.insert(id, enc(&o).as_slice())?;
        Ok(())
    }

    /// Send a waiting send now (the person said so: "Send now", or yes to
    /// sends that came due while the machine was off).
    pub fn send_now(&mut self, id: u64, now: i64) -> Result<bool> {
        let mut t = self.tx.open_table(OUTBOX)?;
        let Some(mut o) = t.get(id)?.map(|v| dec::<Outgoing>(v.value())).transpose()? else { return Ok(false) };
        if !o.state.waiting() {
            return Ok(false);
        }
        o.due = now;
        o.later = None;
        t.insert(id, enc(&o).as_slice())?;
        Ok(true)
    }

    /// Try a stopped send again now (the person said so).
    pub fn send_again(&mut self, id: u64, now: i64) -> Result<bool> {
        let mut t = self.tx.open_table(OUTBOX)?;
        let Some(mut o) = t.get(id)?.map(|v| dec::<Outgoing>(v.value())).transpose()? else { return Ok(false) };
        if !matches!(o.state, SendState::Stopped { .. }) {
            return Ok(false);
        }
        o.state = if self.jmap_account(o.account)? { SendState::JmapRetry } else { SendState::Waiting };
        o.due = now;
        o.later = None;
        t.insert(id, enc(&o).as_slice())?;
        Ok(true)
    }

    fn jmap_account(&self, account: AccountId) -> Result<bool> {
        use crate::config::{AccountConfig, Protocol, CONFIG};

        let table = self.tx.open_table(CONFIG)?;
        let config = table.get(account)?.map(|value| dec::<AccountConfig>(value.value())).transpose()?;
        Ok(config.is_some_and(|config| matches!(config.protocol, Protocol::Jmap { .. })))
    }

    /// At start: a send claimed when Mail stopped may or may not have left,
    /// so it stops for the person to decide (sending twice is worse than
    /// asking). Returns how many.
    pub fn stop_interrupted_sends(&mut self) -> Result<usize> {
        let mut t = self.tx.open_table(OUTBOX)?;
        let mut hit = Vec::new();
        for e in t.iter()? {
            let (k, v) = e?;
            let o: Outgoing = dec(v.value())?;
            if matches!(o.state, SendState::Sending | SendState::JmapSending) {
                hit.push((k.value(), o));
            }
        }
        for (k, mut o) in hit.iter().cloned() {
            let why = "Mail stopped while sending this; it may have gone";
            o.state = if o.state == SendState::JmapSending || self.jmap_account(o.account)? {
                uncertain(why)
            } else {
                SendState::Stopped { why: why.into() }
            };
            t.insert(k, enc(&o).as_slice())?;
        }
        Ok(hit.len())
    }
}

fn uncertain(why: &str) -> SendState {
    SendState::Uncertain { why: why.into() }
}

impl Store {
    pub fn jmap_submission(&self, id: u64) -> Result<Option<Vec<u8>>> {
        let tx = self.db.begin_read()?;
        let saved = tx.open_table(JMAP_SUBMISSIONS)?;
        Ok(saved.get(id)?.map(|value| value.value().to_vec()))
    }

    /// Every send in the outbox, oldest first.
    pub fn outbox(&self) -> Result<Vec<(u64, Outgoing)>> {
        let tx = self.db.begin_read()?;
        let mut out = Vec::new();
        for e in tx.open_table(OUTBOX)?.iter()? {
            let (k, v) = e?;
            out.push((k.value(), dec(v.value())?));
        }
        Ok(out)
    }

    /// Waiting sends due by `now` (overdue ones included: see
    /// [`Store::sends_overdue`]).
    pub fn sends_due(&self, now: i64) -> Result<Vec<(u64, Outgoing)>> {
        Ok(self.outbox()?.into_iter().filter(|(_, o)| o.state.waiting() && o.due <= now).collect())
    }

    /// Sends the person scheduled whose time passed more than `grace`
    /// seconds ago, unsent (the machine was off): Mail asks before sending
    /// them (§9.3).
    pub fn sends_overdue(&self, now: i64, grace: i64) -> Result<Vec<(u64, Outgoing)>> {
        Ok(self
            .sends_due(now)?
            .into_iter()
            .filter(|(_, o)| o.later.is_some_and(|t| t + grace < now) && o.tries == 0)
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(due: i64, later: Option<i64>) -> Outgoing {
        Outgoing {
            account: 1,
            message: [1; 32],
            draft: [2; 32],
            message_id: "<a@x.example>".into(),
            from: "me@x.example".into(),
            rcpts: vec!["you@y.example".into()],
            due,
            later,
            hold_on_server: true,
            state: SendState::Waiting,
            tries: 0,
            last_error: None,
        }
    }

    #[test]
    fn outbox() {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(dir.path()).unwrap();
        let a = s.write(|w| w.queue_send(&o(110, None))).unwrap();
        let b = s.write(|w| w.queue_send(&o(200, Some(200)))).unwrap();
        // Undo inside the window.
        assert!(s.write(|w| w.take_back(a)).unwrap().is_some());
        assert!(s.write(|w| w.take_back(a)).unwrap().is_none());
        assert!(s.sends_due(150).unwrap().is_empty());
        // Due: claimed, then Undo can't reach it.
        assert_eq!(s.sends_due(250).unwrap().len(), 1);
        assert!(!s.write(|w| w.claim_send(b, 150)).unwrap(), "not due yet");
        assert!(s.write(|w| w.claim_send(b, 250)).unwrap());
        assert!(!s.write(|w| w.claim_send(b, 250)).unwrap(), "once");
        assert!(s.write(|w| w.take_back(b)).unwrap().is_none());
        // A transient failure waits a minute, then two.
        s.write(|w| w.send_failed(b, "421 busy", true, 250)).unwrap();
        assert!(s.sends_due(300).unwrap().is_empty());
        assert_eq!(s.sends_due(310).unwrap()[0].1.tries, 1);
        s.write(|w| w.claim_send(b, 310).map(|_| ())).unwrap();
        s.write(|w| w.send_failed(b, "421 busy", true, 310)).unwrap();
        assert_eq!(s.sends_due(430).unwrap().len(), 1);
        // Mail stopped while sending: stops for the person.
        s.write(|w| w.claim_send(b, 430).map(|_| ())).unwrap();
        assert_eq!(s.write(|w| w.stop_interrupted_sends()).unwrap(), 1);
        assert!(s.sends_due(1000).unwrap().is_empty());
        assert!(matches!(s.outbox().unwrap()[0].1.state, SendState::Stopped { .. }));
        assert!(s.write(|w| w.send_again(b, 1000)).unwrap());
        assert!(s.write(|w| w.claim_send(b, 1000)).unwrap());
        assert!(s.write(|w| w.send_done(b)).unwrap().is_some());
        assert!(s.outbox().unwrap().is_empty());
        // Scheduled while the machine was off: asked about.
        s.write(|w| w.queue_send(&o(500, Some(500)))).unwrap();
        s.write(|w| w.queue_send(&o(500, None))).unwrap();
        assert_eq!(s.sends_overdue(5000, 300).unwrap().len(), 1);
        assert!(s.sends_overdue(600, 300).unwrap().is_empty());
        let late = s.sends_overdue(5000, 300).unwrap()[0].0;
        assert!(s.write(|w| w.send_now(late, 5000)).unwrap());
        assert!(s.sends_overdue(5000, 300).unwrap().is_empty());
        // No server hold: waits here until its time.
        assert!(s.write(|w| w.claim_send(late, 5000)).unwrap());
        s.write(|w| w.hold_here(late, 9000)).unwrap();
        assert!(s.sends_due(8999).unwrap().iter().all(|(k, _)| *k != late));
        assert!(!s.outbox().unwrap().iter().find(|(k, _)| *k == late).unwrap().1.hold_on_server);
        assert_eq!(retry_after(20), 3600);
    }
}
