//! Mail keeps only message IDs here. The verified source and calendar part
//! are re-read from the ordinary inbox before delivery.
use super::*;
use mail_schema::{InviteMethod, SenderCheck};

pub(super) const PENDING: TableDefinition<u64, u64> = TableDefinition::new("calendar_updates");

pub(super) const REVISIONS: TableDefinition<(u32, &str, u64), u32> =
    TableDefinition::new("calendar_revisions");

fn candidate(m: &Message) -> bool {
    m.full.is_some()
        && matches!(m.summary.sender_check, SenderCheck::Verified { .. })
        && m.summary.invite.as_ref().is_some_and(|i| {
            i.sequence > 0
                && !i.repeats
                && matches!(i.method, InviteMethod::Request | InviteMethod::Cancel)
        })
}

impl Writer<'_> {
    pub(super) fn queue_calendar(&mut self, id: MsgId, m: &Message) -> Result<()> {
        if candidate(m) {
            let i = m.summary.invite.as_ref().unwrap();
            self.tx
                .open_table(REVISIONS)?
                .insert((m.account, i.uid.as_str(), id), i.sequence)?;
            self.mark_calendar_pending(id)?;
        }
        Ok(())
    }

    fn mark_calendar_pending(&mut self, id: MsgId) -> Result<()> {
        let generation = Store::next_id(self.tx, "calendar-generation")?;
        self.tx.open_table(PENDING)?.insert(id, generation)?;
        Ok(())
    }

    // Updates can reach Mail before the recipient saves the original. A
    // completed handoff replays later revisions without asking what they saved.
    pub fn replay_calendar_revisions(&mut self, msg: MsgId) -> Result<()> {
        let Some(m) = self.get_msg(msg)? else {
            return Ok(());
        };
        let Some(i) = &m.summary.invite else {
            return Ok(());
        };
        let revisions = self.tx.open_table(REVISIONS)?;
        let key = i.uid.as_str();
        for item in revisions.range((m.account, key, 0)..=(m.account, key, u64::MAX))? {
            let (k, v) = item?;
            if v.value() > i.sequence {
                self.mark_calendar_pending(k.value().2)?;
            }
        }
        Ok(())
    }

    pub fn finish_calendar_delivery(&mut self, id: MsgId, generation: u64) -> Result<()> {
        let mut pending = self.tx.open_table(PENDING)?;
        if pending.get(id)?.is_some_and(|g| g.value() == generation) {
            pending.remove(id)?;
        }
        Ok(())
    }
}

impl Store {
    /// A bounded upgrade pass also handles inboxes created before delivery existed.
    pub fn index_calendar_page(&self) -> Result<bool> {
        const CURSOR: &str = "calendar-backfill";
        let tx = self.db.begin_read()?;
        let meta = tx.open_table(META)?;
        let cursor = meta.get(CURSOR)?.map_or(0, |n| n.value());
        if cursor == u64::MAX {
            return Ok(false);
        }
        let page = self.messages_after(cursor, 64)?;
        let last = if page.len() == 64 {
            page.last().unwrap().0
        } else {
            u64::MAX
        };
        self.write(|w| {
            for (id, _) in &page {
                if let Some(m) = w.get_msg(*id)? {
                    w.queue_calendar(*id, &m)?;
                }
            }
            w.tx.open_table(META)?.insert(CURSOR, last)?;
            Ok(())
        })?;
        Ok(last != u64::MAX)
    }

    pub fn pending_calendar(&self, after: MsgId) -> Result<Vec<(MsgId, u64)>> {
        let tx = self.db.begin_read()?;
        let pending = tx.open_table(PENDING)?;
        pending
            .range((std::ops::Bound::Excluded(after), std::ops::Bound::Unbounded))?
            .take(16)
            .map(|row| {
                let (id, generation) = row?;
                Ok((id.value(), generation.value()))
            })
            .collect()
    }
}
