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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SendState {
    /// Waiting for `due`; Undo still works.
    Waiting,
    /// Being handed to the server now.
    Sending,
    /// Not sent, and won't be tried again without the person: the server
    /// refused it, or Mail stopped while sending (it may have gone).
    Stopped { why: String },
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
        if o.state == SendState::Sending {
            return Ok(None);
        }
        t.remove(id)?;
        Ok(Some(o))
    }

    /// Claim a waiting send that is due by `now`, so Undo can't take it
    /// any more. False when it isn't waiting (taken back, or claimed).
    pub fn claim_send(&mut self, id: u64, now: i64) -> Result<bool> {
        let mut t = self.tx.open_table(OUTBOX)?;
        let Some(mut o) = t.get(id)?.map(|v| dec::<Outgoing>(v.value())).transpose()? else { return Ok(false) };
        if o.state != SendState::Waiting || o.due > now {
            return Ok(false);
        }
        o.state = SendState::Sending;
        t.insert(id, enc(&o).as_slice())?;
        Ok(true)
    }

    /// The server took send `id`: it leaves the outbox.
    pub fn send_done(&mut self, id: u64) -> Result<Option<Outgoing>> {
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
            o.state = SendState::Waiting;
        } else {
            o.state = SendState::Stopped { why: why.to_string() };
        }
        t.insert(id, enc(&o).as_slice())?;
        Ok(())
    }

    /// The server can't hold claimed send `id`: it waits here until `until`.
    pub fn hold_here(&mut self, id: u64, until: i64) -> Result<()> {
        let mut t = self.tx.open_table(OUTBOX)?;
        let Some(mut o) = t.get(id)?.map(|v| dec::<Outgoing>(v.value())).transpose()? else { return Err(Error::NotFound) };
        o.hold_on_server = false;
        o.due = until;
        o.state = SendState::Waiting;
        t.insert(id, enc(&o).as_slice())?;
        Ok(())
    }

    /// Send a waiting send now (the person said so: "Send now", or yes to
    /// sends that came due while the machine was off).
    pub fn send_now(&mut self, id: u64, now: i64) -> Result<bool> {
        let mut t = self.tx.open_table(OUTBOX)?;
        let Some(mut o) = t.get(id)?.map(|v| dec::<Outgoing>(v.value())).transpose()? else { return Ok(false) };
        if o.state != SendState::Waiting {
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
        o.state = SendState::Waiting;
        o.due = now;
        o.later = None;
        t.insert(id, enc(&o).as_slice())?;
        Ok(true)
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
            if o.state == SendState::Sending {
                hit.push((k.value(), o));
            }
        }
        for (k, mut o) in hit.iter().cloned() {
            o.state = SendState::Stopped { why: "Mail stopped while sending this; it may have gone".into() };
            t.insert(k, enc(&o).as_slice())?;
        }
        Ok(hit.len())
    }
}

impl Store {
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
        Ok(self.outbox()?.into_iter().filter(|(_, o)| o.state == SendState::Waiting && o.due <= now).collect())
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
